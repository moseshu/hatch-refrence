//! The turn loop.
//!
//! ```text
//! user input ─► persist ─► maybe compact ─► loop (≤ max_steps):
//!     cancelled? ─► stop (third terminal state, nothing rolled back)
//!     recall memory ─► assemble layers ─► model (cancellable)
//!       ├─ text answer ─► persist ─► done
//!       └─ tool calls ─► delegate / delegate_parallel ─► sub-agents (threads)
//!                      └ anything else ─► gate (queued approvals are reported
//!                                         back, never awaited)
//!                      ─► persist results with trust labels ─► next step
//! ```
//!
//! Moses <moseshu25@gmail.com>

use crate::assemble::{assemble, Layers};
use crate::compaction;
use crate::progress::{clip, Observer, Progress, ProgressKind};
use crate::prompt;
use crate::report::{add_usage, StopReason, TurnReport};
use crate::session::SessionStore;
use crate::subagent::{self, Delegation, ParentCtx};
use chrono_tz::Tz;
use hatch_core::time::{fmt_local, Timestamp};
use hatch_core::{CancelToken, Result, Trust};
use hatch_llm::{ChatMessage, ChatRequest, Router, StreamSink, ToolSpec, Workload};
use hatch_memory::MemoryStore;
use hatch_policy::Gatekeeper;
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, Registry, ToolContext};

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub user_name: String,
    pub user_email: String,
    pub tz: Tz,
    pub max_steps: usize,
    /// Total context budget in (estimated) tokens.
    pub context_budget: usize,
    /// Compact when live history exceeds this many tokens.
    pub compaction_threshold: usize,
    pub keep_tail_turns: usize,
    pub max_tokens: Option<u32>,
    /// Model workload for this agent's calls.
    pub workload: Workload,
    /// Extra system instructions appended to the behaviour spec (sub-agent role).
    pub system_note: Option<String>,
    /// Recorded on approvals; default `chat:<session>`.
    pub origin: Option<String>,
    /// Start in the tainted state (a sub-agent inherits its parent's).
    pub start_tainted: bool,
    /// Sub-agent settings; `None` = no `delegate` tools.
    pub delegation: Option<Delegation>,
    /// 0 for the main agent, 1 for its sub-agents, …
    pub depth: usize,
    /// `main` or the sub-agent run id (progress events).
    pub agent_id: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            user_name: "用户".into(),
            user_email: String::new(),
            tz: chrono_tz::Asia::Shanghai,
            max_steps: 24,
            context_budget: 24_000,
            compaction_threshold: 12_000,
            keep_tail_turns: 3,
            max_tokens: None,
            workload: Workload::Main,
            system_note: None,
            origin: None,
            start_tainted: false,
            delegation: None,
            depth: 0,
            agent_id: "main".into(),
        }
    }
}

/// Per-turn knobs: streaming, cancellation and progress.
#[derive(Default)]
pub struct RunOptions<'o> {
    pub on_delta: Option<&'o mut StreamSink<'o>>,
    pub cancel: CancelToken,
    pub observer: Option<&'o Observer<'o>>,
}

pub struct Agent<'a> {
    pub db: &'a Db,
    pub router: &'a Router,
    pub tools: &'a Registry,
    pub executors: &'a ExecutorRegistry,
    pub gate: &'a Gatekeeper,
    pub config: AgentConfig,
}

impl<'a> Agent<'a> {
    fn layers(&self, query: &str, summary: Option<String>, now: &Timestamp) -> Result<Layers> {
        let mem = MemoryStore::new(self.db);
        let profile_claims = mem.with_tags(&["preference", "profile", "style"], now)?;
        let profile: Vec<String> = profile_claims.iter().take(12).map(|c| format!("[{}] {}", c.id, c.text)).collect();
        let recalled = mem
            .search(query, 5, now)?
            .into_iter()
            .filter(|(c, _)| !profile_claims.iter().any(|p| p.id == c.id))
            .map(|(c, _)| {
                let tag = if c.trust.is_untrusted() { "（来源不可信）" } else { "" };
                format!("[{}] {}{tag}", c.id, c.text)
            })
            .collect();
        let mut spec = prompt::behavior(&self.config.user_name);
        if let Some(note) = &self.config.system_note {
            spec.push_str("\n\n");
            spec.push_str(note);
        }
        Ok(Layers {
            spec,
            now_line: format!("{}，时区 {}", fmt_local(now, &self.config.tz), self.config.tz.name()),
            profile,
            recalled,
            summary,
        })
    }

    fn maybe_compact(&self, sessions: &SessionStore, session: &str, now: &Timestamp) -> Result<bool> {
        let (summary, history) = sessions.active(session)?;
        let Some(p) = compaction::plan(&history, self.config.compaction_threshold, self.config.keep_tail_turns) else {
            return Ok(false);
        };
        let (text, _) = compaction::summarize(self.router, summary.as_deref(), &history[..p.count]);
        sessions.compact(session, p.upto_seq, &text, now)?;
        Ok(true)
    }

    /// Tools offered to the model: the registry plus `delegate*` when allowed.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.tools.specs();
        if let Some(d) = self.config.delegation.as_ref().filter(|d| d.can_delegate()) {
            specs.extend(subagent::tool_specs(d));
        }
        specs
    }

    /// Run one user turn to completion (or until a stop condition).
    pub fn run_turn(&self, session: &str, input: &str, now: &Timestamp) -> Result<TurnReport> {
        self.run(session, input, now, RunOptions::default())
    }

    /// Like [`Agent::run_turn`], but model output is streamed to `on_delta`
    /// as it is generated (every step, including steps that end in tool calls).
    pub fn run_turn_streaming<'o>(
        &self,
        session: &str,
        input: &str,
        now: &Timestamp,
        on_delta: Option<&'o mut StreamSink<'o>>,
    ) -> Result<TurnReport> {
        self.run(session, input, now, RunOptions { on_delta, ..Default::default() })
    }

    fn finish_cancelled(
        &self,
        sessions: &SessionStore,
        session: &str,
        mut report: TurnReport,
        cancel: &CancelToken,
        now: &Timestamp,
        emit: &dyn Fn(ProgressKind, String),
    ) -> Result<TurnReport> {
        let reason = cancel.reason().unwrap_or_else(|| "cancelled".into());
        report.stop = StopReason::Cancelled(reason.clone());
        // Keep the transcript well-formed and honest about the interruption.
        let note = format!("（本轮在第 {} 步被中断：{reason}。{}。）", report.steps, report.done_summary());
        sessions.append(session, &ChatMessage::assistant(&note), Trust::System, now)?;
        emit(ProgressKind::Stopped, format!("已取消：{reason}"));
        Ok(report)
    }

    /// The turn loop with streaming, cancellation and progress reporting.
    pub fn run(&self, session: &str, input: &str, now: &Timestamp, opts: RunOptions<'_>) -> Result<TurnReport> {
        let RunOptions { mut on_delta, cancel, observer } = opts;
        let (depth, me) = (self.config.depth, self.config.agent_id.clone());
        let emit = |kind: ProgressKind, detail: String| {
            if let Some(o) = observer {
                o(&Progress { depth, agent: me.clone(), kind, detail });
            }
        };
        let sessions = SessionStore::new(self.db);
        sessions.append(session, &ChatMessage::user(input), Trust::User, now)?;
        let mut report = TurnReport::default();
        if cancel.is_cancelled() {
            return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit);
        }
        report.compacted = self.maybe_compact(&sessions, session, now)?;
        let specs = self.tool_specs();
        let origin = self.config.origin.clone().unwrap_or_else(|| format!("chat:{session}"));
        for step in 0..self.config.max_steps {
            report.steps = step + 1;
            if cancel.is_cancelled() {
                return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit);
            }
            emit(ProgressKind::Step, format!("第 {} 步", step + 1));
            let (summary, history) = sessions.active(session)?;
            let layers = self.layers(input, summary, now)?;
            let assembled = assemble(&layers, &history, self.config.context_budget);
            let mut tainted = assembled.tainted || self.config.start_tainted;
            report.tainted |= tainted;
            let mut req = ChatRequest::new(assembled.messages);
            req.tools = specs.clone();
            req.max_tokens = self.config.max_tokens;
            let w = self.config.workload;
            let result = match on_delta.as_mut() {
                Some(sink) => self.router.chat_stream_with(w, &req, &mut **sink, Some(&cancel)),
                None => self.router.chat_with(w, &req, Some(&cancel)),
            };
            let resp = match result {
                Ok(r) => r,
                Err(e) if e.is_cancelled() => return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit),
                Err(e) if e.is_budget() => {
                    report.stop = StopReason::Budget(e.message);
                    emit(ProgressKind::Stopped, "调用预算用尽".into());
                    return Ok(report);
                }
                Err(e) => {
                    report.stop = StopReason::ModelError(e.to_string());
                    emit(ProgressKind::Stopped, format!("模型错误：{}", clip(&e.message, 80)));
                    return Ok(report);
                }
            };
            report.model_calls += 1;
            add_usage(&mut report.usage, &resp.usage);
            if resp.tool_calls.is_empty() {
                let text = resp.content.unwrap_or_default();
                sessions.append(session, &ChatMessage::assistant(&text), Trust::System, now)?;
                emit(ProgressKind::Answer, clip(&text, 80));
                report.answer = Some(text);
                report.stop = StopReason::Answered;
                return Ok(report);
            }
            sessions.append(
                session,
                &ChatMessage::assistant_tools(resp.content.clone().unwrap_or_default(), resp.tool_calls.clone()),
                Trust::System,
                now,
            )?;
            for (i, call) in resp.tool_calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    for rest in &resp.tool_calls[i..] {
                        sessions.append(session, &ChatMessage::tool(&rest.id, "已取消，未执行。"), Trust::System, now)?;
                    }
                    return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit);
                }
                emit(ProgressKind::ToolCall, format!("{} {}", call.name, clip(&call.arguments.to_string(), 80)));
                let (content, trust, approvals) = if subagent::is_delegate_call(&call.name) && self.config.delegation.is_some() {
                    let pc = ParentCtx { session, tainted, cancel: &cancel, now, observer };
                    let h = subagent::handle_call(self, call, &pc);
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals)
                } else {
                    let ctx = ToolContext {
                        db: self.db,
                        now: *now,
                        tz: self.config.tz,
                        user_email: self.config.user_email.clone(),
                        origin: origin.clone(),
                        tainted,
                        call_id: call.id.clone(),
                        cancel: cancel.clone(),
                    };
                    let out = dispatch_call(self.tools, self.executors, self.gate, call, &ctx);
                    (out.content, out.trust, out.approval_id.into_iter().collect())
                };
                report.tool_calls.push(call.name.clone());
                for id in approvals {
                    if !report.approvals.contains(&id) {
                        report.approvals.push(id);
                    }
                }
                if trust == Trust::Untrusted {
                    tainted = true;
                    report.tainted = true;
                }
                emit(ProgressKind::ToolResult, format!("{} → {}", call.name, clip(&content, 60)));
                sessions.append(session, &ChatMessage::tool(&call.id, &content), trust, now)?;
            }
        }
        emit(ProgressKind::Stopped, format!("达到步数上限 {}", self.config.max_steps));
        Ok(report)
    }
}
