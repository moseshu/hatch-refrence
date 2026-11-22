//! Cancelling a background sub-agent from inside a turn.
//!
//! The model-visible `cancel_subagent` tool is the counterpart of
//! `delegate`: when the user says "cancel that sub-agent", the model
//! calls it with the run id and the child is killed from this
//! process — no daemon needed. It goes through the same registry
//! path as the daemon API cancel: the monitor thread kills the child
//! and the run finishes as cancelled, with a handoff for the next turn.

use super::store::RunStore;
use super::{Delegation, Handled};
use crate::agent::Agent;
use hatch_core::Trust;
use hatch_llm::{ToolCall, ToolSpec};
use serde_json::{json, Value};

/// Tool name for cancelling a background sub-agent.
pub const CANCEL_SUBAGENT: &str = "cancel_subagent";

pub fn is_cancel_call(name: &str) -> bool {
    name == CANCEL_SUBAGENT
}

/// The `cancel_subagent` tool definition, offered only where a
/// background child can exist: the main agent of an async turn. A
/// child process clears the registry (so its delegation is
/// synchronous) and an in-process child's delegation runs
/// synchronously, so neither ever has a background run to cancel.
pub fn tool_spec(d: &Delegation) -> Option<ToolSpec> {
    if d.depth != 0 || !d.is_async() {
        return None;
    }
    Some(ToolSpec {
        name: CANCEL_SUBAGENT.into(),
        description: "Cancel a sub-agent running in the background (by run id): the child process is terminated, the run is recorded as cancelled, and the cancellation reaches you as a handoff next turn. Only sub-agents delegated in this conversation can be cancelled; finished ones report their current status.".into(),
        parameters: json!({"type": "object", "properties": {"run_id": {"type": "string", "description": "run id of the sub-agent to cancel (the id returned by delegate, e.g. sa_xxxx)"}},
                           "required": ["run_id"]}),
    })
}

/// Cancel a background sub-agent of this turn's parent, by run id.
///
/// No gatekeeper, no approval queue — like starting a sub-agent,
/// stopping your own is a local control action (nothing is sent,
/// written or spent), and making it wait for approval would defeat
/// the point of cancelling. Only runs this parent delegated can be
/// cancelled from here: a run owned by another session (or another
/// process) is refused, not killed.
pub fn handle_cancel_call(parent: &Agent, call: &ToolCall, parent_ref: &str) -> Handled {
    let run_id = call
        .arguments
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if run_id.is_empty() {
        return Handled {
            content: "ERROR: missing run_id (the sub-agent to cancel)".into(),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![],
        };
    }
    let d = parent
        .config
        .delegation
        .as_ref()
        .expect("cancel call without delegation config");
    let store = RunStore::new(parent.db);
    let rec = match store.get(run_id) {
        Ok(Some(r)) => r,
        _ => {
            return Handled {
                content: format!("No sub-agent run {run_id}."),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![],
            }
        }
    };
    if rec.parent != parent_ref {
        return Handled {
            content: format!("Sub-agent {run_id} does not belong to this conversation; it cannot be cancelled from here."),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![],
        };
    }
    if rec.status.is_terminal() {
        return Handled {
            content: format!(
                "Sub-agent {run_id} has already finished (status {}).",
                rec.status.as_str()
            ),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![run_id.to_string()],
        };
    }
    match d.registry.as_ref() {
        Some(reg) if reg.cancel(run_id) => {
            let _ = parent.db.audit("subagent", "cancelled", &format!("{run_id}: 对话中取消"));
            Handled {
                content: format!(
                    "Cancelled sub-agent {run_id} (type {}): the child process will be terminated, the run recorded as cancelled, and its cancellation handoff arrives next turn.",
                    rec.agent_type
                ),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            }
        }
        _ => Handled {
            content: format!(
                "Sub-agent {run_id} is not running in this process and cannot be cancelled from here (it may belong to the daemon; cancel it with `hatch agents cancel {run_id}`, or wait a turn and see whether it has finished)."
            ),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![],
        },
    }
}
