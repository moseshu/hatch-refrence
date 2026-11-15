//! Shared plan-runner helpers (P1): mirroring a plan's step
//! outcomes into the task's mirror steps, and failing a task with
//! a note. Moved out of `task_invite.rs` (which owned them for the
//! invite) so the generic Plan-and-Execute runner and the ReAct
//! runner share the same code — one mirror, one failure path.

use super::task_delegate::report_child_if_delegated;
use super::task_worker::notify_task;
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_plan::{Plan, RunReport, StepStatus};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Priority};
use hatch_store::Db;
use hatch_tasks::{ExecStatus, Lease, TaskStore, UnitStore};

/// Mirror a finished plan's step outcomes into the task's steps
/// (idempotent: a step already Done in the task is never re-completed,
/// so a resume cannot duplicate its `step_done` event). The receipt
/// is recovered from the approval queue: the ledger key of the step's
/// executed approval (the plan row itself stores no receipt).
pub(crate) fn mirror_plan(
    db: &Db,
    plan: &Plan,
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
) -> Result<()> {
    for step in &plan.steps {
        if step.status != StepStatus::Done {
            continue;
        }
        let receipt = step.approval_id.as_deref().and_then(|id| {
            ApprovalQueue::new(db)
                .get(id)
                .ok()
                .filter(|a| a.status == ApprovalStatus::Executed)
                .map(|a| a.idempotency_key)
        });
        mirror_step(
            units,
            task_id,
            lease,
            &step.id,
            &step.output,
            receipt.as_deref(),
            now,
        )?;
    }
    Ok(())
}

/// Mirror the steps a paused run already completed.
pub(crate) fn mirror_done_steps(
    report: &RunReport,
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
) -> Result<()> {
    for step in &report.steps {
        if step.status != StepStatus::Done {
            continue;
        }
        mirror_step(
            units,
            task_id,
            lease,
            &step.step_id,
            &step.output,
            step.receipt.as_deref(),
            now,
        )?;
    }
    Ok(())
}

fn mirror_step(
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    step_id: &str,
    output: &str,
    receipt: Option<&str>,
    now: &Timestamp,
) -> Result<()> {
    let Some(step) = units.get_step(task_id, step_id)? else {
        return Ok(());
    };
    if step.status == ExecStatus::Done {
        return Ok(());
    }
    units.start_step(task_id, lease, step_id, now)?;
    units.complete_step(task_id, lease, step_id, ExecStatus::Done, output, now)?;
    if let Some(receipt) = receipt {
        let _ = units.set_step_receipt(task_id, lease, step_id, receipt, now);
    }
    Ok(())
}

/// Fail the task with a note (a control-plane transition the runner
/// owns when the plan cannot proceed).
pub(crate) fn fail_task(
    db: &Db,
    store: &TaskStore,
    task_id: &str,
    lease: &Lease,
    reason: &str,
    tz: &chrono_tz::Tz,
    now: &Timestamp,
) -> Result<()> {
    let _ = store.fail(task_id, Some(lease), reason, now);
    let title = store
        .get(task_id)
        .ok()
        .flatten()
        .map(|t| t.title)
        .unwrap_or_else(|| task_id.to_string());
    notify_task(
        db,
        task_id,
        "failed",
        Priority::High,
        &format!("任务《{title}》失败：{reason}"),
        &format!("task:{task_id}:failed:{reason}"),
        *tz,
        now,
    );
    // A failed child reports its result (with the failure kind) and
    // wakes its parent (task 7).
    let _ = report_child_if_delegated(db, task_id, now);
    Ok(())
}
