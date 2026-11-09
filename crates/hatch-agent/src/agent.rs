//! The turn loop.
//!
//! ```text
//! user input ─► persist ─► maybe compact (enqueue, or extractive sync
//!   past the hard limit) ─► loop (≤ max_steps):
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

use crate::assemble::{assemble, message_tokens, Layers};
use crate::bridge::{is_meta_call, ToolBridge};
use crate::compaction;
pub use crate::config::{AgentConfig, RunOptions};
use crate::progress::{clip, Progress, ProgressKind};
use crate::prompt;
use crate::reanchor::CallRef;
use crate::recall::recall;
use crate::report::{add_usage, StopReason, TurnReport};
use crate::session::SessionStore;
use crate::subagent::{self, ParentCtx, PendingHandoff};
use crate::unanswered;
use crate::worker;
use crate::{bridge, bridge_call};
use hatch_core::time::{fmt_local, to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result, Trust};
use hatch_llm::{ChatMessage, ChatRequest, Router, StreamSink, ToolSpec};
use hatch_memory::{MemoryStore, SynthesisDoc};
use hatch_policy::Gatekeeper;
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, Registry, ToolContext};

pub struct Agent<'a> {
    pub db: &'a Db,
    pub router: &'a Router,
    pub tools: &'a Registry,
    pub executors: &'a ExecutorRegistry,
    pub gate: &'a Gatekeeper,
    pub config: AgentConfig,
}

impl<'a> Agent<'a> {
    fn layers(
        &self,
        query: &str,
        summary: Option<String>,
        now: &Timestamp,
        handoffs: &[PendingHandoff],
        synthesis: Option<&SynthesisDoc>,
    ) -> Result<Layers> {
        let mem = MemoryStore::with_ranking(
            self.db,
            hatch_memory::RankingConfig::from_days(self.config.salience_half_life_days),
        );
        // Memory is best-effort context: a DB error must never fail a turn.
        let profile_claims = mem.with_tags(&["preference", "profile", "style"], now).unwrap_or_default();
        let profile: Vec<String> = profile_claims.iter().take(12).map(|c| format!("[{}] {}", c.id, c.text)).collect();
        let mut recalled = recall(&mem, query, &profile_claims, now);
        recalled.extend(self.config.memory_context.iter().cloned());
        let mut spec = prompt::behavior(&self.config.user_name);
        if let Some(note) = &self.config.system_note {
            spec.push_str("\n\n");
            spec.push_str(note);
        }
        Ok(Layers {
            spec,
            // P3: the nightly synthesis, pre-rendered with its heading.
            // Trust is system content, not untrusted — see `Layers`.
            synthesis: synthesis.map(|d| {
                format!(
                    "## 当前理解（每夜综合，生成于 {}，来源 {} 条记忆）\n{}",
                    to_rfc3339(&d.generated_at),
                    d.source_claims,
                    d.content
                )
            }),
            now_line: format!("{}，时区 {}", fmt_local(now, &self.config.tz), self.config.tz.name()),
            profile,
            recalled,
            skills: self.config.skills.clone(),
            background: self.config.background.clone(),
            handoffs: handoffs.iter().map(|h| h.handoff.clone()).collect(),
            handoffs_tainted: handoffs.iter().any(|h| h.tainted),
            summary,
            // D2: set by the turn loop on anchor steps, after `layers()`.
            plan_anchor: None,
        })
    }

    /// Decide what to do about a history over the compaction threshold.
    /// Returns `(compacted_now, compaction_requested)`:
    /// * over the hard limit (2× threshold) the turn compacts synchronously
    ///   with the extractive fallback — no model call, so the turn can
    ///   always continue;
    /// * otherwise a one-shot background job is enqueued (deduped per
    ///   session) and this turn continues with the full history. The
    ///   summary lands for the next turn.
    ///
    /// A bookkeeping failure while enqueueing never fails the turn:
    /// compaction is an optimisation, and the hard limit above still
    /// protects the context.
    fn maybe_compact(
        &self,
        sessions: &SessionStore,
        session: &str,
        now: &Timestamp,
    ) -> Result<(bool, bool)> {
        let (summary, history) = sessions.active(session)?;
        let Some(p) = compaction::plan(
            &history,
            self.config.compaction_threshold,
            self.config.keep_tail_turns,
        ) else {
            return Ok((false, false));
        };
        let total: usize = history.iter().map(|m| message_tokens(&m.message)).sum();
        if total > self.config.compaction_threshold * 2 {
            let text = compaction::extractive(summary.as_deref(), &history[..p.count]);
            sessions.compact(session, p.upto_seq, &text, now)?;
            return Ok((true, false));
        }
        let requested =
            worker::request_compaction(self.db, session, p.upto_seq, now, &self.config.tz)
                .unwrap_or(false);
        Ok((false, requested))
    }

    /// Tools offered to the model: the bridge's listed set (hot tools +
    /// skill tools + find_tool / use_tool) plus `delegate*` when allowed.
    /// A skill allowlist narrows both: only allowlisted tools (and always
    /// list_skills / load_skill) are offered, and delegation is offered only
    /// when the allowlist names a delegate tool.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let bridge = ToolBridge::from_specs(bridge::allowed_specs(self.tools, &self.config.skill_tools));
        let mut specs = bridge.listed().to_vec();
        let can_delegate = self.config.skill_tools.as_ref().is_none_or(|a| {
            a.iter()
                .any(|n| n == "delegate" || n == "delegate_parallel")
        });
        if can_delegate {
            if let Some(d) = self.config.delegation.as_ref().filter(|d| d.can_delegate()) {
                specs.extend(subagent::tool_specs(d));
            }
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
        unanswered::annotate(self.db, sessions, session, &report.approvals, now);
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
        let (compacted, requested) = self.maybe_compact(&sessions, session, now)?;
        report.compacted = compacted;
        report.compaction_requested = requested;
        // Async delegation: reconcile runs a previous process left open,
        // then collect this agent's undelivered handoffs once for the
        // whole turn — collecting per step would deliver them on step 1
        // and hide them from later steps, since delivery is exactly-once.
        let parent_ref = subagent::registry::parent_ref(depth, &me, session);
        let mut handoffs: Vec<PendingHandoff> = Vec::new();
        if let Some(d) = self.config.delegation.as_ref().filter(|d| d.is_async()) {
            subagent::reconcile_runs(self.db, d.registry.as_deref(), &parent_ref, now);
            handoffs = subagent::collect_handoffs(self.db, &parent_ref);
        }
        // P3: nightly synthesis — read once per turn, before the step
        // loop, so the stable prefix cannot change mid-turn. A read
        // failure means "no synthesis", never a failed turn.
        let synthesis = hatch_memory::load_synthesis(self.db).ok().flatten();
        let specs = self.tool_specs();
        let origin = self.config.origin.clone().unwrap_or_else(|| format!("chat:{session}"));
        // A skill allowlist narrows what the model may call for the rest of
        // this turn, including tools loaded mid-turn via `load_skill`.
        let mut skill_allowlist = self.config.skill_tools.clone();
        // D2: bind this turn to a plan when configured (unbound turns run as before).
        let mut turn_plan = self.config.plan_id.as_deref().and_then(|id| crate::turn_plan::TurnPlan::bind(self.db, id, self.config.reanchor_every));
        for step in 0..self.config.max_steps {
            report.steps = step + 1;
            if cancel.is_cancelled() {
                return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit);
            }
            emit(ProgressKind::Step, format!("第 {} 步", step + 1));
            let (summary, history) = sessions.active(session)?;
            let mut layers = self.layers(input, summary, now, &handoffs, synthesis.as_ref())?;
            if let Some(p) = &turn_plan {
                layers.plan_anchor = p.anchor_for_step(step);
            }
            let assembled = assemble(&layers, &history, self.config.context_budget);
            report.layers = assembled.layers;
            let mut tainted = assembled.tainted || self.config.start_tainted || !self.config.background.is_empty() || layers.handoffs_tainted;
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
                    unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
                    return Ok(report);
                }
                Err(e) => {
                    report.stop = StopReason::ModelError(e.to_string());
                    emit(ProgressKind::Stopped, format!("模型错误：{}", clip(&e.message, 80)));
                    unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
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
                unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
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
                // Enforce the skill allowlist even for tools the model calls
                // after a skill was loaded mid-turn (specs were fixed at
                // turn start, so the check has to happen here too). A
                // bridged call is reported under the tool it ran.
                let blocked_by_skill = bridge_call::allowlist_blocks(&skill_allowlist, &call.name);
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
                let mut reported_name = call.name.clone();
                let (content, trust, approvals) = if blocked_by_skill {
                    (bridge_call::skill_refusal(&skill_allowlist, &call.name), Trust::System, Vec::new())
                } else if subagent::is_cancel_call(&call.name) && self.config.delegation.is_some() {
                    let h = subagent::handle_cancel_call(self, call, &parent_ref);
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals)
                } else if subagent::is_delegate_call(&call.name) && self.config.delegation.is_some() {
                    let pc = ParentCtx { session, tainted, cancel: &cancel, now, observer };
                    let h = if self.config.delegation.as_ref().is_some_and(|d| d.is_async()) {
                        subagent::handle_call_async(self, call, &pc)
                    } else {
                        subagent::handle_call(self, call, &pc)
                    };
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals)
                } else if is_meta_call(&call.name) {
                    let (c, t, a, reported) = bridge_call::bridge_call(self, call, &skill_allowlist, &ctx);
                    reported_name = reported;
                    (c, t, a)
                } else {
                    let out = dispatch_call(self.tools, self.executors, self.gate, call, &ctx);
                    (out.content, out.trust, out.approval_id.into_iter().collect())
                };
                // A `load_skill` result starts with a machine-readable marker
                // (`SKILL_LOADED\t<name>\t<tool,tool>`); adopt its allowlist
                // for the rest of this turn (union with any earlier one).
                if call.name == "load_skill" {
                    if let Some(rest) = content.strip_prefix("SKILL_LOADED\t") {
                        let mut parts = rest.split('\t');
                        let name = parts.next().unwrap_or("");
                        let tools: Vec<String> = parts
                            .next()
                            .unwrap_or("")
                            .split(',')
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .collect();
                        if !tools.is_empty() {
                            let entry = skill_allowlist.get_or_insert_with(Vec::new);
                            for t in &tools {
                                if !entry.contains(t) {
                                    entry.push(t.clone());
                                }
                            }
                            emit(
                                ProgressKind::ToolCall,
                                format!("技能 {name} 已加载，工具限定为：{}", tools.join("、")),
                            );
                        }
                    }
                }
                if let Some(p) = &mut turn_plan {
                    if !blocked_by_skill {
                        p.note_call(CallRef::with_args(
                            &reported_name,
                            Some(call.arguments.clone()),
                            self.tools.get(&reported_name).map(|t| t.class()),
                        ));
                    }
                }
                report.tool_calls.push(reported_name);
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
            if let Some(p) = &turn_plan {
                let drift = p.after_step();
                report.plan = Some(p.report(drift.as_ref().map(|d| d.reason.as_str())));
                if let Some(d) = drift {
                    report.stop = crate::turn_plan::stop_on_drift(self.db, &sessions, session, &report.approvals, p.plan_id(), &d, now)?;
                    return Ok(report);
                }
            }
        }
        emit(ProgressKind::Stopped, format!("达到步数上限 {}", self.config.max_steps));
        unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
        Ok(report)
    }
}
