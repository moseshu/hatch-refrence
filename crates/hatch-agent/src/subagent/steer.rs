//! Steering a background sub-agent from inside a turn.
//!
//! The model-visible `steer_subagent` tool is the counterpart of
//! `delegate`: while a background child runs, the parent can send it
//! a supplementary instruction (a correction, an extra constraint, a
//! changed requirement) instead of cancelling and restarting it. The
//! message is queued through the registry into the child's monitor
//! session, written to the child socket, and injected into the
//! child's context at its next step boundary — queued, never
//! preemptive, and never a way around the child's approval gates
//! (it is context, not authorization).

use super::registry::SteerOutcome;
use super::store::RunStore;
use super::{Delegation, Handled};
use crate::agent::Agent;
use hatch_core::Trust;
use hatch_llm::{ToolCall, ToolSpec};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Pending steering messages for one child agent loop. The child's
/// socket reader thread pushes; the turn loop drains at each step
/// boundary (see `AgentConfig::steer_queue`). Only child processes
/// ever carry one; ordinary turns leave it `None`.
pub type SteerQueue = Arc<Mutex<VecDeque<String>>>;

/// A fresh, empty steering queue for a child run.
pub fn new_steer_queue() -> SteerQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Merge drained steering messages into the single user message the
/// child loop injects: one message, however many arrived, so the
/// transcript never carries consecutive user turns.
pub fn merged_steer_message(texts: &[String]) -> String {
    if texts.len() == 1 {
        format!("[Steering instruction from the parent agent] {}", texts[0])
    } else {
        let items: Vec<String> = texts.iter().map(|t| format!("- {t}")).collect();
        format!("[Steering instructions from the parent agent]\n{}", items.join("\n"))
    }
}

/// Tool name for steering a background sub-agent.
pub const STEER_SUBAGENT: &str = "steer_subagent";

/// Longest steering message accepted, in characters. Generous enough
/// for a detailed correction, small enough to bound what can pile up
/// in the socket buffer while a child is paused.
pub const MAX_STEER_CHARS: usize = 2000;

pub fn is_steer_call(name: &str) -> bool {
    name == STEER_SUBAGENT
}

/// The `steer_subagent` tool definition, offered only where a
/// background child can exist: the main agent of an async turn (same
/// gate as `cancel_subagent` — a synchronous parent is blocked
/// inside its delegate call and has no moment to steer).
pub fn tool_spec(d: &Delegation) -> Option<ToolSpec> {
    if d.depth != 0 || !d.is_async() {
        return None;
    }
    Some(ToolSpec {
        name: STEER_SUBAGENT.into(),
        description: "Send a steering instruction to a sub-agent running in the background (by run id): correct course, add constraints, or change requirements without cancelling and restarting. The message is queued; the sub-agent reads it before its next step and adjusts — it does not interrupt the current step. Only sub-agents delegated in this conversation can be steered; finished ones report their current status.".into(),
        parameters: json!({"type": "object", "properties": {
            "run_id": {"type": "string", "description": "run id of the sub-agent to steer (the id returned by delegate, e.g. sa_xxxx)"},
            "message": {"type": "string", "description": "the full steering instruction (self-contained: the sub-agent cannot see this conversation; state what to change and by what standard)"}
        }, "required": ["run_id", "message"]}),
    })
}

/// Queue a steering message for a background sub-agent of this
/// turn's parent, by run id.
///
/// No gatekeeper, no approval queue — like cancelling your own
/// sub-agent, steering it is a local control action (nothing is
/// sent externally, written, or spent), and making it wait for
/// approval would defeat the point of a timely correction. Only
/// runs this parent delegated can be steered from here: a run owned
/// by another session (or another process) is refused.
pub fn handle_steer_call(parent: &Agent, call: &ToolCall, parent_ref: &str) -> Handled {
    let refused = |content: String, runs: Vec<String>| Handled {
        content,
        trust: Trust::System,
        approvals: vec![],
        runs,
    };
    let run_id = call
        .arguments
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if run_id.is_empty() {
        return refused("ERROR: missing run_id (the sub-agent run to steer)".into(), vec![]);
    }
    let message = call
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if message.is_empty() {
        return refused("ERROR: missing message (the steering instruction must not be empty)".into(), vec![]);
    }
    let chars = message.chars().count();
    if chars > MAX_STEER_CHARS {
        return refused(
            format!("ERROR: steering instruction too long ({chars} chars, limit {MAX_STEER_CHARS}); shorten it and resend."),
            vec![],
        );
    }
    let d = parent
        .config
        .delegation
        .as_ref()
        .expect("steer call without delegation config");
    let store = RunStore::new(parent.db);
    let rec = match store.get(run_id) {
        Ok(Some(r)) => r,
        _ => return refused(format!("No sub-agent run {run_id}."), vec![]),
    };
    if rec.parent != parent_ref {
        return refused(format!("Sub-agent {run_id} does not belong to this conversation; it cannot be steered from here."), vec![]);
    }
    if rec.status.is_terminal() {
        return refused(
            format!("Sub-agent {run_id} has already finished (status {}); the steering instruction has nowhere to go.", rec.status.as_str()),
            vec![run_id.to_string()],
        );
    }
    let outcome = d.registry.as_ref().map(|reg| reg.steer(run_id, message.to_string()));
    match outcome {
        Some(SteerOutcome::Queued) => {
            let preview: String = message.chars().take(100).collect();
            let _ = parent.db.audit("subagent", "steer_sent", &format!("{run_id}: {chars} chars: {preview}"));
            Handled {
                content: format!(
                    "Steering instruction queued for sub-agent {run_id} (type {}); it will read it before its next step and adjust. Its conclusion still reaches you as a handoff next turn.",
                    rec.agent_type
                ),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            }
        }
        Some(SteerOutcome::Full) => refused(
            format!(
                "Sub-agent {run_id}'s steering queue is full ({} pending); wait until it advances a few steps, then send again.",
                super::registry::STEER_QUEUE_CAP
            ),
            vec![run_id.to_string()],
        ),
        _ => refused(
            format!(
                "Sub-agent {run_id} is not running in this process and cannot be steered from here (it may belong to the daemon; or wait a turn and see whether it has finished)."
            ),
            vec![run_id.to_string()],
        ),
    }
}
