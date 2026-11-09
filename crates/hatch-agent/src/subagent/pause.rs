//! Pausing and resuming a background sub-agent from inside a turn (D5).
//!
//! The model-visible `pause_subagent` / `resume_subagent` tools are the
//! counterparts of `cancel_subagent`, with the opposite trade: cancel
//! kills the child and its half-done work is lost (only the progress
//! already reported and the handoff survive); pause freezes the child
//! in place and resume continues the same run from exactly where it
//! stopped.
//!
//! Mechanism: process-level stop/continue (`SIGSTOP` / `SIGCONT` on
//! the child pid), not cooperative suspension. The child is a separate
//! OS process holding all of its state — the turn loop, an in-flight
//! model or tool call, its own database in the scratch dir, the files
//! it touched — so freezing it loses nothing and resumes nothing twice.
//! No child-side code and no protocol change: the monitor thread keeps
//! waiting on the socket (still open), and the run's timeout clock does
//! not run while paused. A repeated pause/resume is an idempotent
//! no-op, not an error.
//!
//! The three boundary semantics, stated once here so no caller has to
//! guess:
//!
//! * **Paused while waiting for approval**: the child queues approvals
//!   into its own database and never blocks on a human mid-run — they
//!   are replayed to the parent only when the run ends. A pause freezes
//!   the wait wherever it is; the approval stays pending in the child's
//!   database and the run continues with the same queue state on
//!   resume. No approval is auto-approved, auto-denied, or lost.
//! * **Paused mid-tool-call**: the call completes normally on resume
//!   and its result returns to the turn loop; nothing is re-executed.
//!   Two caveats: a shell command the child already spawned keeps
//!   running while paused (the child is not put in its own process
//!   group — the same granularity as today's kill), and a tool call
//!   with its own wall-clock timeout may fail on resume if its timeout
//!   elapsed while frozen — that surfaces as an ordinary tool error
//!   the agent may retry.
//! * **Parent dies while paused**: a stopped child cannot watch its
//!   socket, so unlike a running child it cannot notice the parent is
//!   gone and exit on its own. A paused run therefore survives only
//!   while its owner process lives: on the next reconciliation a
//!   paused run whose owner is gone is interrupted (its stopped child
//!   is killed, its scratch dir removed) with a handoff saying it died
//!   while paused, because resuming needs the owner's monitor thread
//!   and child handle, which no new process has. See
//!   [`super::registry::reconcile_runs`].
//!
//! The D7 watchdog cannot mistake a paused run for a lost one: it
//! reconciles the task ledger, and sub-agent runs are not ledger rows
//! (wiring them is the D7 follow-up, out of scope). The only
//! convergence path that sees a paused run is `reconcile_runs`, which
//! leaves a live pause alone. When runs are wired to the ledger later,
//! a paused run must map to `TaskStatus::Blocked` (reconcile never
//! moves it), never `Waiting` (the system would retry it).

use super::registry::ControlOutcome;
use super::store::RunStore;
use super::{Delegation, Handled};
use crate::agent::Agent;
use hatch_core::Trust;
use hatch_llm::{ToolCall, ToolSpec};
use serde_json::{json, Value};

/// Tool name for pausing a background sub-agent.
pub const PAUSE_SUBAGENT: &str = "pause_subagent";
/// Tool name for resuming a paused background sub-agent.
pub const RESUME_SUBAGENT: &str = "resume_subagent";

pub fn is_pause_call(name: &str) -> bool {
    name == PAUSE_SUBAGENT
}

pub fn is_resume_call(name: &str) -> bool {
    name == RESUME_SUBAGENT
}

/// The `pause_subagent` / `resume_subagent` tool definitions, offered
/// only where a background child can exist: the main agent of an async
/// turn. Same gate as `cancel_subagent`.
pub fn tool_specs(d: &Delegation) -> Vec<ToolSpec> {
    if d.depth != 0 || !d.is_async() {
        return vec![];
    }
    vec![
        ToolSpec {
            name: PAUSE_SUBAGENT.into(),
            description: "暂停一个正在后台运行的子代理（按 run id）：子进程被冻结在原地，做到一半的工作不丢，稍后用 resume_subagent 从同一处继续。暂停期间不计入运行超时。只能暂停本次对话委派的子代理；已经结束的会直接告诉你当前状态。".into(),
            parameters: json!({"type": "object", "properties": {"run_id": {"type": "string", "description": "要暂停的子代理 run id（delegate 调用返回的 id，如 sa_xxxx）"}},
                               "required": ["run_id"]}),
        },
        ToolSpec {
            name: RESUME_SUBAGENT.into(),
            description: "恢复一个已暂停的子代理（按 run id）：同一个 run 从暂停处继续执行，不是重头再来。只能恢复本次对话暂停的子代理；已经结束的会直接告诉你当前状态。".into(),
            parameters: json!({"type": "object", "properties": {"run_id": {"type": "string", "description": "要恢复的子代理 run id（pause_subagent 暂停过的 id）"}},
                               "required": ["run_id"]}),
        },
    ]
}

fn run_id_of(call: &ToolCall) -> &str {
    call.arguments.get("run_id").and_then(Value::as_str).map(str::trim).unwrap_or("")
}

fn missing_run_id(verb: &str) -> Handled {
    Handled {
        content: format!("ERROR: 缺少 run_id（要{verb}的子代理 id）"),
        trust: Trust::System,
        approvals: vec![],
        runs: vec![],
    }
}

fn no_such_run(run_id: &str) -> Handled {
    Handled { content: format!("没有子代理运行 {run_id}。"), trust: Trust::System, approvals: vec![], runs: vec![] }
}

fn foreign_run(run_id: &str, verb: &str) -> Handled {
    Handled {
        content: format!("子代理 {run_id} 不属于本次对话，无法在这里{verb}。"),
        trust: Trust::System,
        approvals: vec![],
        runs: vec![],
    }
}

fn terminal_run(run_id: &str, status: &str, verb: &str) -> Handled {
    Handled {
        content: format!("子代理 {run_id} 已经结束，当前状态 {status}，无法{verb}。"),
        trust: Trust::System,
        approvals: vec![],
        runs: vec![run_id.to_string()],
    }
}

fn not_in_this_process(run_id: &str, verb: &str, cli: &str) -> Handled {
    Handled {
        content: format!(
            "子代理 {run_id} 不在本进程运行，无法从这里{verb}（它可能属于 daemon；用 `hatch agents {cli} {run_id}` {verb}，或等下一轮看它是否已结束）。"
        ),
        trust: Trust::System,
        approvals: vec![],
        runs: vec![],
    }
}

/// Handle a `pause_subagent` / `resume_subagent` call, if that is
/// what this is — shaped for the turn loop's dispatch chain: it
/// returns the (content, trust, approvals) triple and records the run
/// in `runs`, or `None` for any other tool (or when no delegation
/// config exists), so the caller chains it in one branch.
pub fn handle_pause_or_resume(
    parent: &Agent,
    call: &ToolCall,
    parent_ref: &str,
    runs: &mut Vec<String>,
) -> Option<(String, Trust, Vec<String>)> {
    parent.config.delegation.as_ref()?;
    let h = if is_pause_call(&call.name) {
        handle_pause_call(parent, call, parent_ref)
    } else if is_resume_call(&call.name) {
        handle_resume_call(parent, call, parent_ref)
    } else {
        return None;
    };
    runs.extend(h.runs);
    Some((h.content, h.trust, h.approvals))
}

/// Pause a background sub-agent of this turn's parent, by run id.
///
/// No gatekeeper, no approval queue — like cancelling, pausing your
/// own child is a local control action (nothing is sent, written or
/// spent), and making it wait for approval would defeat the point.
/// Only runs this parent delegated can be paused from here.
pub fn handle_pause_call(parent: &Agent, call: &ToolCall, parent_ref: &str) -> Handled {
    let run_id = run_id_of(call);
    if run_id.is_empty() {
        return missing_run_id("暂停");
    }
    let d = parent
        .config
        .delegation
        .as_ref()
        .expect("pause call without delegation config");
    let store = RunStore::new(parent.db);
    let rec = match store.get(run_id) {
        Ok(Some(r)) => r,
        _ => return no_such_run(run_id),
    };
    if rec.parent != parent_ref {
        return foreign_run(run_id, "暂停");
    }
    if rec.status.is_terminal() {
        return terminal_run(run_id, rec.status.as_str(), "暂停");
    }
    match d.registry.as_ref() {
        Some(reg) => match reg.pause(run_id) {
            ControlOutcome::Done => {
                let _ = parent.db.audit("subagent", "paused", &format!("{run_id}: 对话中暂停"));
                Handled {
                    content: format!(
                        "已暂停子代理 {run_id}（类型 {}）：子进程被冻结在原地，做到一半的工作保留，暂停期间不计入运行超时；用 resume_subagent 从同一处继续，或用 cancel_subagent 取消它。",
                        rec.agent_type
                    ),
                    trust: Trust::System,
                    approvals: vec![],
                    runs: vec![run_id.to_string()],
                }
            }
            ControlOutcome::Already => Handled {
                content: format!("子代理 {run_id} 已经处于暂停状态，无需重复暂停。"),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            },
            ControlOutcome::Terminal(status) => terminal_run(run_id, status.as_str(), "暂停"),
            ControlOutcome::NotOwned => not_in_this_process(run_id, "暂停", "pause"),
        },
        None => not_in_this_process(run_id, "暂停", "pause"),
    }
}

/// Resume a paused background sub-agent of this turn's parent, by run
/// id. The same run continues from where the pause froze it.
pub fn handle_resume_call(parent: &Agent, call: &ToolCall, parent_ref: &str) -> Handled {
    let run_id = run_id_of(call);
    if run_id.is_empty() {
        return missing_run_id("恢复");
    }
    let d = parent
        .config
        .delegation
        .as_ref()
        .expect("resume call without delegation config");
    let store = RunStore::new(parent.db);
    let rec = match store.get(run_id) {
        Ok(Some(r)) => r,
        _ => return no_such_run(run_id),
    };
    if rec.parent != parent_ref {
        return foreign_run(run_id, "恢复");
    }
    if rec.status.is_terminal() {
        return terminal_run(run_id, rec.status.as_str(), "恢复");
    }
    match d.registry.as_ref() {
        Some(reg) => match reg.resume(run_id) {
            ControlOutcome::Done => {
                let _ = parent.db.audit("subagent", "resumed", &format!("{run_id}: 对话中恢复"));
                Handled {
                    content: format!(
                        "已恢复子代理 {run_id}（类型 {}）：同一个 run 从暂停处继续执行，不是重头再来；完成后会以交接形式在下一轮告诉你。",
                        rec.agent_type
                    ),
                    trust: Trust::System,
                    approvals: vec![],
                    runs: vec![run_id.to_string()],
                }
            }
            ControlOutcome::Already => Handled {
                content: format!("子代理 {run_id} 正在运行，无需恢复。"),
                trust: Trust::System,
                approvals: vec![],
                runs: vec![run_id.to_string()],
            },
            ControlOutcome::Terminal(status) => terminal_run(run_id, status.as_str(), "恢复"),
            ControlOutcome::NotOwned => not_in_this_process(run_id, "恢复", "resume"),
        },
        None => not_in_this_process(run_id, "恢复", "resume"),
    }
}
