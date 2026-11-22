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
        format!("【父代理补充指令】{}", texts[0])
    } else {
        let items: Vec<String> = texts.iter().map(|t| format!("- {t}")).collect();
        format!("【父代理补充指令】\n{}", items.join("\n"))
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
        description: "给一个正在后台运行的子代理（按 run id）补充一条指令：纠偏、追加约束或改要求，不必取消重来。消息排队送达，子代理在下一步开始前读到并据此调整；它不会打断子代理当前这一步。只能给本次对话委派的子代理发；已经结束的会直接告诉你当前状态。".into(),
        parameters: json!({"type": "object", "properties": {
            "run_id": {"type": "string", "description": "要补充指令的子代理 run id（delegate 调用返回的 id，如 sa_xxxx）"},
            "message": {"type": "string", "description": "补充指令全文（自包含：子代理看不到当前对话，写清改什么、按什么标准）"}
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
        return refused("ERROR: 缺少 run_id（要补充指令的子代理 id）".into(), vec![]);
    }
    let message = call
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if message.is_empty() {
        return refused("ERROR: 缺少 message（补充指令不能为空）".into(), vec![]);
    }
    let chars = message.chars().count();
    if chars > MAX_STEER_CHARS {
        return refused(
            format!("ERROR: 补充指令太长（{chars} 字，上限 {MAX_STEER_CHARS} 字），请精简后重发。"),
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
        _ => return refused(format!("没有子代理运行 {run_id}。"), vec![]),
    };
    if rec.parent != parent_ref {
        return refused(format!("子代理 {run_id} 不属于本次对话，无法在这里给它补充指令。"), vec![]);
    }
    if rec.status.is_terminal() {
        return refused(
            format!("子代理 {run_id} 已经结束，当前状态 {}，补充指令无处送达。", rec.status.as_str()),
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
                    "已给子代理 {run_id}（类型 {}）排队补充指令，它会在下一步开始前读到并据此调整。它的结论仍以交接形式在下一轮告诉你。",
                    rec.agent_type
                ),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            }
        }
        Some(SteerOutcome::Full) => refused(
            format!(
                "子代理 {run_id} 的补充指令队列已满（{} 条待处理），等它推进几步后再发。",
                super::registry::STEER_QUEUE_CAP
            ),
            vec![run_id.to_string()],
        ),
        _ => refused(
            format!(
                "子代理 {run_id} 不在本进程运行，无法从这里补充指令（它可能属于 daemon；或等下一轮看它是否已结束）。"
            ),
            vec![run_id.to_string()],
        ),
    }
}
