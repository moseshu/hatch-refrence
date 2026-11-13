//! `hatch task replan` (task 6): locally replan a task's plan when a
//! resource version changed.
//!
//! The command carries the plan version the user saw. A stale version
//! is rejected before anything moves. A successful replan resets only
//! the steps named in `--steps`, commits a new plan version (the old
//! approvals die with the old version), and leaves the other steps —
//! including their receipts — exactly as they were. The new version
//! needs a fresh approval round before it runs again. The remaining
//! budget is never touched: the replanned branches inherit it.

use anyhow::{anyhow, Context, Result};
use hatch_core::time::Timestamp;
use hatch_plan::{replan_local, ReplanStep};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskStore, UnitStore};

use super::task_version::{check_expected_version, finalize_command};

/// Replan a task's plan: reset only the named steps, bump the plan
/// version, invalidate the old approvals, and record the diff.
pub(crate) fn replan_task(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    step_ids: &[String],
    reason: &str,
    user_id: &str,
    now: &Timestamp,
) -> Result<String> {
    if step_ids.is_empty() {
        return Err(anyhow!(
            "没有要重做的步骤（至少给一个 --steps 里的步骤 id）"
        ));
    }
    if reason.trim().is_empty() {
        return Err(anyhow!("重规划需要 --reason（说明哪个资源版本变了）"));
    }
    let task = TaskStore::new(db)
        .get(task_id)?
        .ok_or_else(|| anyhow!("task {task_id} 不存在"))?;
    if task.plan_id.is_empty() {
        return Err(anyhow!("task {task_id} 没有计划可重规划"));
    }
    let summary = format!("重规划：{reason}");
    check_expected_version(db, &task, expected_version, &summary, user_id, now)?;
    let steps: Vec<ReplanStep> = step_ids
        .iter()
        .map(|id| ReplanStep {
            step_id: id.clone(),
            edit: None,
        })
        .collect();
    // Bumps the plan version, resets the named steps, clears the
    // approved snapshot and submits a fresh plan approval.
    let diff = replan_local(db, &task.plan_id, expected_version, &steps, reason, now)
        .context("重规划失败")?;
    let new_version = diff.to_version;
    // The task mirror must agree with the plan: reset the same steps
    // there (the old receipt stays on the row for audit; the rerun
    // overwrites it). Steps that never ran have no row yet — no-op.
    for step_id in diff.changed_ids() {
        UnitStore::new(db).reset_step_for_replan(task_id, step_id, now)?;
    }
    // Bring the task row to the same version (the plan already moved;
    // a concurrent bump here only desyncs the mirror, so report it
    // in the event instead of pretending the replan did not apply).
    if let Err(e) = TaskStore::new(db).bump_plan_version(task_id, expected_version, now) {
        EventLog::new(db).append_control_event(
            task_id,
            "plan_changed",
            "",
            &format!(
                "计划已重规划：计划 v{expected_version} → v{new_version}；任务版本未同步：{e}"
            ),
            now,
        )?;
    }
    let invalidated = finalize_command(
        db,
        task_id,
        expected_version,
        new_version,
        &summary,
        user_id,
        "计划已重规划",
        now,
    )?;
    EventLog::new(db).append_control_event(task_id, "plan_changed", "", &diff.describe(), now)?;
    Ok(format!(
        "已重规划：计划 v{expected_version} → v{new_version}，{} 个步骤重做（{}），{} 个步骤保持，{invalidated} 个旧审批已失效；新版本需重新批准后才能继续执行",
        diff.reset_count(),
        diff.changed_ids().join("、"),
        diff.unchanged_ids().len()
    ))
}
