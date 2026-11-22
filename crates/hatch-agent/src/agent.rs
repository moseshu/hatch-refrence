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

use crate::assemble::{assemble, Layers};
use crate::bridge::{is_meta_call, ToolBridge};
pub use crate::config::{AgentConfig, RunOptions, StepEvent};
use crate::progress::{clip, Progress, ProgressKind};
use crate::prompt;
use crate::reanchor::CallRef;
use crate::recall::recall;
use crate::report::{add_usage, StopReason, TurnReport};
use crate::session::SessionStore;
use crate::subagent::{self, ParentCtx, PendingHandoff};
use crate::system_prompt::stable_prefix;
use crate::unanswered;
use crate::{bridge, bridge_call};
use hatch_core::time::{fmt_local, to_rfc3339, Timestamp};
use hatch_core::tokens::estimate_tokens;
use hatch_core::{CancelToken, Result, Trust};
use hatch_llm::{ChatMessage, ChatRequest, Router, StreamSink, ToolSpec};
use hatch_memory::{MemoryStore, SynthesisDoc};
use hatch_policy::{ActionClass, Gatekeeper};
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, ImageAttachment, Registry, ToolContext};

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
        let profile_claims = match mem.with_tags(&["preference", "profile", "style"], now) {
            Ok(claims) => claims,
            Err(e) => {
                let _ = self.db.audit("agent", "profile_claims_failed", &e.to_string());
                Vec::new()
            }
        };
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
            // A stale document (a claim was corrected after it was
            // generated) says so in its heading, so the model does not
            // treat superseded wording as current.
            synthesis: synthesis.map(|d| {
                let caveat = if d.stale.is_some() {
                    "; note: a memory was corrected after generation; this document may be out of date"
                } else {
                    ""
                };
                format!(
                    "## Current understanding (nightly synthesis, generated {}, from {} memories{})\n{}",
                    to_rfc3339(&d.generated_at),
                    d.source_claims,
                    caveat,
                    d.content
                )
            }),
            now_line: format!("{}, timezone {}", fmt_local(now, &self.config.tz), self.config.tz.name()),
            profile,
            recalled,
            skills: self.config.skills.clone(),
            background: self.config.background.clone(),
            handoffs: handoffs.iter().map(|h| h.handoff.clone()).collect(),
            handoffs_tainted: handoffs.iter().any(|h| h.tainted),
            summary,
            // D2: set by the turn loop on anchor steps, after `layers()`.
            plan_anchor: None,
            // Set by the turn loop in the final stretch, after `layers()`.
            step_note: None,
        })
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
        if let Some(spec) = crate::todo::tool_spec(&self.config) {
            specs.push(spec);
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
        let note = format!("(This turn was interrupted at step {}: {reason}. {}.)", report.steps, report.done_summary());
        sessions.append(session, &ChatMessage::assistant(&note), Trust::System, now)?;
        unanswered::annotate(self.db, sessions, session, &report.approvals, now);
        emit(ProgressKind::Stopped, format!("已取消：{reason}"));
        Ok(report)
    }

    /// The turn loop with streaming, cancellation and progress reporting.
    pub fn run(&self, session: &str, input: &str, now: &Timestamp, opts: RunOptions<'_>) -> Result<TurnReport> {
        let RunOptions { mut on_delta, cancel, observer, mut on_step_event } = opts;
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
        // P3: nightly synthesis — read once per turn, before the step
        // loop, so the stable prefix cannot change mid-turn. A read
        // failure means "no synthesis", never a failed turn.
        let synthesis = hatch_memory::load_synthesis(self.db).ok().flatten();
        // D2: bind this turn to a plan when configured (unbound turns run as before).
        let mut turn_plan = self.config.plan_id.as_deref().and_then(|id| crate::turn_plan::TurnPlan::bind(self.db, id, self.config.reanchor_every));
        // §3.4: measure the stable prefix before deciding on compaction.
        // The probe layers reuse the turn's recall; handoffs are volatile
        // tail, so they are left out of the measurement, and the plan
        // anchor is the step-0 one (the decision is made for this turn's
        // first step). A layers failure here fails the turn, as it would
        // in the step loop below.
        let (summary0, _) = sessions.active(session)?;
        let mut probe = self.layers(input, summary0, now, &[], synthesis.as_ref())?;
        if let Some(p) = &turn_plan {
            probe.plan_anchor = p.anchor_for_step(0);
        }
        // 4 = the system-message overhead `assemble` charges to the spec.
        let prefix_tokens = 4 + estimate_tokens(&stable_prefix(&probe));
        let budget = self.config.effective_budget();
        let (compacted, requested) = self.maybe_compact(&sessions, session, now, prefix_tokens)?;
        report.compacted = compacted;
        report.compaction_requested = requested;
        // Async delegation: reconcile runs a previous process left open,
        // then collect this agent's undelivered handoffs once for the
        // whole turn — collecting per step would deliver them on step 1
        // and hide them from later steps, since delivery is exactly-once.
        let parent_ref = subagent::registry::parent_ref(depth, &me, session);
        // P1 D6: converge tasks a previous process left in `running`; best-effort, never fails a turn.
        let _ = hatch_tasks::TaskStore::new(self.db).reconcile(now, hatch_tasks::DEFAULT_STALE_AFTER_SECS);
        let mut handoffs: Vec<PendingHandoff> = Vec::new();
        if let Some(d) = self.config.delegation.as_ref().filter(|d| d.is_async()) {
            subagent::reconcile_runs(self.db, d.registry.as_deref(), &parent_ref, now);
            handoffs = subagent::collect_handoffs(self.db, &parent_ref);
        }
        let specs = self.tool_specs();
        let origin = self.config.origin.clone().unwrap_or_else(|| format!("chat:{session}"));
        // A skill allowlist narrows what the model may call for the rest of
        // this turn, including tools loaded mid-turn via `load_skill`.
        let mut skill_allowlist = self.config.skill_tools.clone();
        let mut unverified_direct_effects = Vec::new();
        // Exact-duplicate circuit breaker: (tool, canonical args) ->
        // (first step, first result was an error). A repeat of a call
        // that already succeeded is intercepted, not re-dispatched.
        let mut seen_calls: std::collections::HashMap<(String, String), (usize, bool)> =
            std::collections::HashMap::new();
        // Streak detection: consecutive read-only steps and
        // same-source error repeats become step notes (and, at the
        // hard threshold, a forced text-only wrap-up).
        let mut streaks = crate::streak::StreakTracker::new();
        // Last step note written to the audit log (notes repeat across
        // steps in the final stretch; audit each distinct one once).
        let mut audited_step_note: Option<String> = None;
        // Turn-end completeness check: side-effecting actions actually
        // dispatched this turn, and whether the once-per-turn coverage
        // self-check already fired (see crate::completeness).
        let mut executed_effects: Vec<String> = Vec::new();
        let mut completeness_checked = false;
        for step in 0..self.config.max_steps {
            report.steps = step + 1;
            if cancel.is_cancelled() {
                return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit);
            }
            // Parent steering (child loops only): messages the
            // delegating agent queued mid-run land here, merged into
            // one user message before this step's context is built.
            // They are instructions from the parent — context, not
            // authorization; approval gates are unaffected.
            if let Some(queue) = &self.config.steer_queue {
                let drained: Vec<String> = {
                    let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                    q.drain(..).collect()
                };
                if !drained.is_empty() {
                    let _ = self.db.audit(
                        &format!("chat:{session}"),
                        "steer_applied",
                        &format!("{} 条补充指令", drained.len()),
                    );
                    let merged = subagent::steer::merged_steer_message(&drained);
                    sessions.append(session, &ChatMessage::user(&merged), Trust::User, now)?;
                }
            }
            emit(ProgressKind::Step, format!("第 {} 步", step + 1));
            let (summary, history) = sessions.active(session)?;
            let mut layers = self.layers(input, summary, now, &handoffs, synthesis.as_ref())?;
            if let Some(p) = &turn_plan {
                layers.plan_anchor = p.anchor_for_step(step);
            }
            // Budget ladder: the model sees its step budget only in the
            // final stretch — a soft convergence nudge, then a hard
            // text-only final step so a turn can never end in silence
            // just because the step budget ran out mid-exploration.
            let remaining = self.config.max_steps - step;
            let last_step = remaining == 1;
            // A tripped same-source error breaker ends exploration
            // the same way the budget's last step does: tools off,
            // text-only, but with the block-report instruction.
            let wrapup = streaks.force_wrapup();
            let tools_off = last_step || wrapup;
            if wrapup {
                layers.step_note = Some(crate::streak::WRAPUP_NOTE.into());
            } else if last_step {
                layers.step_note = Some(
                    "## Step budget\nThis is the last step of this turn; tools are disabled. Give the final answer directly from the evidence you have; if evidence is insufficient, say plainly what is missing — do not fabricate."
                        .into(),
                );
            } else if remaining <= 5 {
                layers.step_note = Some(format!(
                    "## Step budget reminder\nThis turn allows at most {} steps; {} remain (including this one). Stop broad exploration and organize the evidence you have to prepare an answer; when the core facts are clear, do not burn steps chasing complete certainty.",
                    self.config.max_steps, remaining
                ));
            } else if let Some(note) = streaks.take_note() {
                layers.step_note = Some(note);
            }
            // Step notes steer the model but never reach the stored
            // transcript; audit each distinct one so a trace can show
            // exactly which reminder fired on which step.
            if layers.step_note != audited_step_note {
                if let Some(note) = &layers.step_note {
                    let _ = self.db.audit(&format!("chat:{session}"), "step_note", note);
                }
                audited_step_note = layers.step_note.clone();
            }
            let assembled = assemble(&layers, &history, budget);
            report.layers = assembled.layers;
            let mut tainted = assembled.tainted || self.config.start_tainted || !self.config.background.is_empty() || layers.handoffs_tainted;
            report.tainted |= tainted;
            let mut req = ChatRequest::new(assembled.messages);
            req.tools = if tools_off { Vec::new() } else { specs.clone() };
            req.max_tokens = self.config.max_tokens;
            let w = self.config.workload;
            // Buffer candidate text until evidence checks finish. Progress events
            // still stream; completion text is emitted only after the guard.
            let result = self.router.chat_with(w, &req, Some(&cancel));
            let resp = match result {
                Ok(r) => r,
                Err(e) if e.is_cancelled() => return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit),
                Err(e) if e.is_budget() => {
                    report.stop = StopReason::Budget(e.message);
                    emit(ProgressKind::Stopped, "调用预算用尽".into());
                    unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
                    return Ok(report);
                }
                // Empty answer (Backend kind, after the router's own
                // retry): no text and no tool calls is a no-progress
                // step, not a fatal error — seen from thinking models
                // on tools-free final steps. Mid-turn it earns a nudge
                // and another step; on the last step, one retry with an
                // explicit instruction, then an honest fallback line.
                Err(e) if e.is_backend() && !tools_off => {
                    report.model_calls += 1;
                    sessions.append(
                        session,
                        &ChatMessage::user(
                            "(System note: your previous reply was empty. Continue: call a tool, or give the final answer directly.)",
                        ),
                        Trust::System,
                        now,
                    )?;
                    continue;
                }
                Err(e) if e.is_backend() => {
                    report.model_calls += 1;
                    let mut retry_req = req.clone();
                    retry_req
                        .messages
                        .push(ChatMessage::user("Output the final answer text directly; do not leave it empty."));
                    match self.router.chat_with(w, &retry_req, Some(&cancel)) {
                        // Usage/call counting for this retry happens in
                        // the common path after the match.
                        Ok(r2) if r2.content.as_deref().is_some_and(|c| !c.trim().is_empty()) => r2,
                        Ok(_) => hatch_llm::ChatResponse::text(
                            "（本轮未能生成回答：模型在最后一步返回了空文本。）",
                        ),
                        Err(e2) if e2.is_cancelled() => {
                            return self.finish_cancelled(&sessions, session, report, &cancel, now, &emit)
                        }
                        Err(e2) if e2.is_budget() => {
                            report.stop = StopReason::Budget(e2.message);
                            emit(ProgressKind::Stopped, "调用预算用尽".into());
                            unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
                            return Ok(report);
                        }
                        Err(_) => hatch_llm::ChatResponse::text(
                            "（本轮未能生成回答：模型在最后一步返回了空文本。）",
                        ),
                    }
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
            if let Some(sink) = on_delta.as_mut() {
                for call in &resp.tool_calls {
                    sink(hatch_llm::StreamEvent::ToolCallStarted { name: call.name.clone() });
                }
            }
            if resp.tool_calls.is_empty() {
                // Turn-end completeness self-check (once per turn): a
                // side-effecting turn may not end on the model's first
                // say-so. Show it the user's request next to what it
                // actually executed and let it remediate. Needs at
                // least 3 steps of headroom (check response + one
                // remediation step + final answer) and must not fight
                // a streak-forced wrap-up. The premature candidate is
                // discarded, exactly like the empty-answer nudge.
                // Main-agent turns only: coverage is judged against
                // the user's request, which only the top-level turn
                // owns; a sub-agent's brief is an intermediate
                // instruction whose result the parent reviews, and
                // re-presenting it risks redo loops on tight budgets.
                if self.config.completeness_check
                    && self.config.depth == 0
                    && !completeness_checked
                    && !executed_effects.is_empty()
                    && remaining >= 3
                    && !wrapup
                {
                    completeness_checked = true;
                    let note = crate::completeness::completeness_note(input, &executed_effects);
                    let _ = self.db.audit(&format!("chat:{session}"), "completeness_check", &note);
                    sessions.append(session, &ChatMessage::user(&note), Trust::System, now)?;
                    continue;
                }
                let candidate = resp.content.unwrap_or_default();
                let decision = crate::verification::check(
                    self,
                    &crate::verification::CheckInput {
                        session,
                        goal: input,
                        candidate: &candidate,
                        direct_effects: &unverified_direct_effects,
                        now,
                        cancel: &cancel,
                    },
                    &mut report,
                )?;
                let text = decision.text.unwrap_or(candidate);
                if let Some(sink) = on_delta.as_mut() {
                    sink(hatch_llm::StreamEvent::TextDelta(text.clone()));
                    sink(hatch_llm::StreamEvent::Done);
                }
                sessions.append(session, &ChatMessage::assistant(&text), Trust::System, now)?;
                // M0: the durable-unit hook sees the answer too, so a
                // recorded turn ends with exactly one terminal event.
                if let Some(hook) = on_step_event.as_mut() {
                    hook(StepEvent::Answer { step, text: text.clone() });
                }
                emit(ProgressKind::Answer, clip(&text, 80));
                report.answer = Some(text.clone());
                report.stop = if decision.blocked { StopReason::VerificationBlocked(text.clone()) } else { StopReason::Answered };
                unanswered::annotate(self.db, &sessions, session, &report.approvals, now);
                return Ok(report);
            }
            // M0: the decision batch is recorded BEFORE any call is
            // dispatched, so a worker killed mid-step resumes from the
            // recorded batch instead of re-asking the model.
            if let Some(hook) = on_step_event.as_mut() {
                hook(StepEvent::Decision { step, calls: resp.tool_calls.clone() });
            }
            sessions.append(
                session,
                &ChatMessage::assistant_tools(resp.content.clone().unwrap_or_default(), resp.tool_calls.clone()),
                Trust::System,
                now,
            )?;
            let mut observed: Vec<(String, serde_json::Value, String, bool)> = Vec::new();
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
                // Exact-duplicate circuit breaker: an identical call that
                // already succeeded this turn is intercepted, not re-run.
                // A repeat after an error stays allowed (transient retry).
                let dup_key = (call.name.clone(), canonical_args(&call.arguments));
                let dup_of = seen_calls
                    .get(&dup_key)
                    .filter(|(_, was_error)| !was_error)
                    .map(|(first_step, _)| *first_step);
                let ctx = ToolContext {
                    db: self.db,
                    now: *now,
                    tz: self.config.tz,
                    user_email: self.config.user_email.clone(),
                    user_name: self.config.user_name.clone(),
                    origin: origin.clone(),
                    tainted,
                    call_id: call.id.clone(),
                    cancel: cancel.clone(),
                };
                let effect_intent = if dup_of.is_some() {
                    None
                } else {
                    crate::verification::intent(self,session,call,now)?
                };
                let mut reported_name = call.name.clone();
                let (content, trust, approvals, image): (String, Trust, Vec<String>, Option<ImageAttachment>) = if let Some(first_step) = dup_of {
                    (
                        format!(
                            "This call exactly duplicates step {} (same tool, same arguments); intercepted and not executed. Change the search terms or arguments, or answer directly from what you already have.",
                            first_step + 1
                        ),
                        Trust::System,
                        Vec::new(),
                        None,
                    )
                } else if blocked_by_skill {
                    (bridge_call::skill_refusal(&skill_allowlist, &call.name), Trust::System, Vec::new(), None)
                } else if crate::todo::is_todo_call(&call.name) && self.config.todo_tools {
                    let out = crate::todo::handle_todo_call(call);
                    let _ = self.db.audit(&format!("chat:{session}"), "todo_write", &out.audit_detail);
                    if let Some(detail) = out.progress_detail {
                        emit(ProgressKind::Todo, detail);
                    }
                    (out.content, Trust::System, Vec::new(), None)
                } else if subagent::is_cancel_call(&call.name) && self.config.delegation.is_some() {
                    let h = subagent::handle_cancel_call(self, call, &parent_ref);
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals, None)
                } else if subagent::is_steer_call(&call.name) && self.config.delegation.is_some() {
                    let h = subagent::handle_steer_call(self, call, &parent_ref);
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals, None)
                } else if let Some(t) = subagent::handle_pause_or_resume(self, call, &parent_ref, &mut report.subagents) {
                    (t.0, t.1, t.2, None)
                } else if subagent::is_delegate_call(&call.name) && self.config.delegation.is_some() {
                    let pc = ParentCtx { session, tainted, cancel: &cancel, now, observer };
                    let h = if self.config.delegation.as_ref().is_some_and(|d| d.is_async()) {
                        subagent::handle_call_async(self, call, &pc)
                    } else {
                        subagent::handle_call(self, call, &pc)
                    };
                    report.subagents.extend(h.runs);
                    (h.content, h.trust, h.approvals, None)
                } else if is_meta_call(&call.name) {
                    let (c, t, a, reported, image) = bridge_call::bridge_call(self, call, &skill_allowlist, &ctx);
                    reported_name = reported;
                    (c, t, a, image)
                } else {
                    let out = dispatch_call(self.tools, self.executors, self.gate, call, &ctx);
                    (out.content, out.trust, out.approval_id.into_iter().collect(), out.image)
                };
                crate::verification::settle_intent(self,session,effect_intent.as_deref(),&content,!approvals.is_empty() || blocked_by_skill,now)?;
                if dup_of.is_none() {
                    seen_calls.entry(dup_key).or_insert((
                        step,
                        hatch_tools::verification::result_reports_error(&content),
                    ));
                }
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
                if !blocked_by_skill && approvals.is_empty() && !content.starts_with("ERROR:") && self.tools.get(&reported_name)
                    .is_some_and(|t| hatch_tools::verification::requires_evidence(t.class())) {
                    unverified_direct_effects.push(format!("{} 缺少独立验收证据", reported_name));
                }
                report.tool_calls.push(reported_name.clone());
                let read_only = self.tools.get(&reported_name).is_some_and(|t| {
                    matches!(
                        t.class(),
                        ActionClass::ReadPersonal | ActionClass::FetchWeb | ActionClass::BrowseLocal
                    )
                });
                observed.push((reported_name.clone(), call.arguments.clone(), content.clone(), read_only));
                // Record dispatched side-effecting actions for the
                // turn-end completeness check. Intercepted duplicates
                // and skill-blocked calls never ran, so they don't
                // count as coverage.
                if dup_of.is_none()
                    && !blocked_by_skill
                    && executed_effects.len() < crate::completeness::MAX_RECORDED_EFFECTS
                    && self.tools.get(&reported_name).is_some_and(|t| {
                        crate::completeness::is_side_effecting(t.class())
                    })
                {
                    executed_effects.push(crate::completeness::effect_summary(
                        &reported_name,
                        &call.arguments.to_string(),
                        hatch_tools::verification::result_reports_error(&content),
                    ));
                }
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
                let shot = image.map(|i| (i.data, i.media_type));
                let result_msg = ChatMessage::tool(&call.id, &content).with_optional_image(shot);
                sessions.append(session, &result_msg, trust, now)?;
                // M0: the observation is recorded right after it lands
                // in the session, so request and observation stay paired
                // even if the worker dies on the very next call.
                if let Some(hook) = on_step_event.as_mut() {
                    hook(StepEvent::Result {
                        step,
                        call_id: call.id.clone(),
                        tool: reported_name.clone(),
                        content: content.clone(),
                        trust,
                    });
                }
            }
            let observed_refs: Vec<crate::streak::ObservedCall> = observed
                .iter()
                .map(|(n, a, c, r)| crate::streak::ObservedCall {
                    name: n,
                    arguments: a,
                    content: c,
                    read_only: *r,
                })
                .collect();
            streaks.observe_step(&observed_refs);
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

/// Canonical form of tool arguments for exact-duplicate detection:
/// object keys sorted recursively, so key order never hides a repeat.
fn canonical_args(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical_args(&map[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(items) => {
            format!("[{}]", items.iter().map(canonical_args).collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}
