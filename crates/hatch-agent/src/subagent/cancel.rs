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
        description: "取消一个正在后台运行的子代理（按 run id）：子进程被终止，运行状态记为 cancelled，取消的结论以交接形式在下一轮告诉你。只能取消本次对话委派的子代理；已经结束的会直接告诉你当前状态。".into(),
        parameters: json!({"type": "object", "properties": {"run_id": {"type": "string", "description": "要取消的子代理 run id（delegate 调用返回的 id，如 sa_xxxx）"}},
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
            content: "ERROR: 缺少 run_id（要取消的子代理 id）".into(),
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
                content: format!("没有子代理运行 {run_id}。"),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![],
            }
        }
    };
    if rec.parent != parent_ref {
        return Handled {
            content: format!("子代理 {run_id} 不属于本次对话，无法在这里取消。"),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![],
        };
    }
    if rec.status.is_terminal() {
        return Handled {
            content: format!(
                "子代理 {run_id} 已经结束，当前状态 {}。",
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
                    "已取消子代理 {run_id}（类型 {}）：子进程将被终止，状态记为 cancelled，下一轮会收到它的取消交接。",
                    rec.agent_type
                ),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            }
        }
        _ => Handled {
            content: format!(
                "子代理 {run_id} 不在本进程运行，无法从这里取消（它可能属于 daemon；用 `hatch agents cancel {run_id}` 取消，或等下一轮看它是否已结束）。"
            ),
            trust: Trust::System,
            approvals: vec![],
            runs: vec![],
        },
    }
}
