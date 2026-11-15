//! Bound-subagent reconciliation (P1, piece 4): a chat-delegated
//! subagent run bound to a ledger subtask row reports its result
//! through the exactly-once `report_result` path, so `run_join` /
//! `consume` pick it up. Split out of `task_delegate` so neither
//! file grows past the line-count rule.

use super::task_delegate::{build_child_result, extract_mail_ids, truncate};
use hatch_agent::subagent::{RunRecord, RunStatus, RunStore};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_store::Db;
use hatch_tasks::{ChildResult, DelegationStore, EvidenceRef};

pub(crate) fn reconcile_turn_subagents(db: &Db, run_ids: &[String], now: &Timestamp) {
    let runs = RunStore::new(db);
    let delegations = DelegationStore::new(db);
    for id in run_ids {
        let Ok(Some(run)) = runs.get(id) else {
            continue;
        };
        if run.subtask_id.is_empty() {
            continue;
        }
        let Ok(Some(d)) = delegations.get_by_child(&run.subtask_id) else {
            continue;
        };
        let _ = report_finished_bound_runs(db, &d.parent_task, now);
    }
}

/// Reconcile bound subagent runs (P1, piece 4): for each open
/// delegation of this parent, look up the subagent run bound to the
/// child task (`subagent_runs.subtask_id`, schema v21). When the run
/// is terminal, its result goes into the delegation row through the
/// exactly-once `report_result` path, so `run_join` / `consume` can
/// pick it up. Cancelled runs are skipped — cancel propagation owns
/// them. Best-effort: a report failure never fails the caller (the
/// next sweep retries); a run that is still open is left alone.
pub(crate) fn report_finished_bound_runs(
    db: &Db,
    parent_task: &str,
    now: &Timestamp,
) -> Result<()> {
    let delegations = DelegationStore::new(db);
    let runs = RunStore::new(db);
    for d in delegations.list_for_parent(parent_task)? {
        if d.status.is_terminal() {
            continue;
        }
        let Some(run) = runs.run_by_subtask(&d.child_task)? else {
            continue;
        };
        let failure_kind = match run.status {
            RunStatus::Completed => String::new(),
            RunStatus::Failed | RunStatus::Rejected | RunStatus::Interrupted => {
                "permanent".to_string()
            }
            // Spawned / Running / Paused: not a result yet.
            // Cancelled: cancel propagation owns it.
            _ => continue,
        };
        let result = build_bound_result(db, &d.child_task, &run)?;
        let _ = delegations.report_result(&d.child_task, &result, &failure_kind, now);
    }
    Ok(())
}

/// Build a bound run's structured result: the child's durable
/// records first (a worker-spawned child has steps and context),
/// then the run's own answer — for a chat-delegated child the
/// answer IS the result (there is no child task row). Mail ids
/// cited in the answer become evidence references, as in the
/// worker path.
fn build_bound_result(db: &Db, child_task: &str, run: &RunRecord) -> Result<ChildResult> {
    let mut result = build_child_result(db, child_task)?;
    let answer = run.answer.trim();
    if !answer.is_empty() {
        result.summary = truncate(answer, 200);
        result.conclusions.push(truncate(answer, 2000));
        for id in extract_mail_ids(answer) {
            let evidence = EvidenceRef::new("mail", &id, "子任务回答引用");
            if !result.evidence.iter().any(|e| e.kind == "mail" && e.id == id) {
                result.evidence.push(evidence);
            }
        }
    }
    Ok(result)
}
