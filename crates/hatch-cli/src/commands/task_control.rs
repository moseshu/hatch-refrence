//! Control plane for durable tasks (M1, §10.2): versioned mid-task
//! commands and cancel. The worker never watches these; the CLI
//! applies them to the ledger directly, and the worker picks the
//! effect up on its next run (it reads the task row before starting
//! each step).

use super::task_version::{
    bump_version, check_expected_version, finalize_command, invalidate_older,
};
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, TaskStore, UnitStore};

/// Apply a mid-task command (M1 §10.2): the command carries the
/// plan version the user saw. A version that already moved on is
/// rejected as expired (and recorded as rejected). A successful
/// command bumps the task's — and its plan's — version and
/// invalidates every approval bound to an older version, so an
/// invalidated approval can never trigger an action again.
///
/// The command's content (`summary`) is recorded, not applied:
/// changing what the steps do (a new recipient, a different meeting)
/// needs intent parsing + step editing, which is M5/M6 work. What
/// this does — and what the plan requires — is the version move and
/// the invalidation that makes old approvals safe.
pub fn apply_command(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    summary: &str,
    user_id: &str,
    now: &Timestamp,
) -> Result<String> {
    let store = TaskStore::new(db);
    let task = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    check_expected_version(db, &task, expected_version, summary, user_id, now)?;
    let new_version = bump_version(
        db,
        task_id,
        expected_version,
        summary,
        user_id,
        "指令已应用",
        now,
    )?;
    let invalidated = finalize_command(
        db,
        task_id,
        expected_version,
        new_version,
        summary,
        user_id,
        "指令已应用",
        now,
    )?;
    Ok(format!(
        "指令已应用：task {task_id} 的计划 v{expected_version} → v{new_version}，{invalidated} 个旧审批已失效"
    ))
}

/// Cancel a task (M1): the task stops at the next step boundary (the
/// worker checks before each step) and its pending/approved
/// approvals are invalidated, so a late approval cannot trigger an
/// action after the cancel.
pub fn cancel_task(db: &Db, task_id: &str, note: &str, now: &Timestamp) -> Result<String> {
    let store = TaskStore::new(db);
    let task = store.cancel(task_id, note, now)?;
    // `plan_version + 1` as the bound: a cancel invalidates every
    // approval the task ever bound, not just ones older than the
    // current version.
    let invalidated = invalidate_older(db, task_id, task.plan_version + 1, now)?;
    EventLog::new(db).append_control_event(
        task_id,
        "task_cancelled",
        "",
        &format!("任务已取消：{note}（{invalidated} 个审批已失效）"),
        now,
    )?;
    Ok(format!(
        "task {} 已取消（{} 个审批已失效）",
        task.id, invalidated
    ))
}

/// Pause a task (task 5): the task stops claiming new steps at the
/// next step boundary and stays readable as blocked/paused until the
/// user resumes it with `task run --task` (which unblocks and
/// continues from the breakpoint). Unlike `cancel_task` this is not
/// terminal, and it invalidates no approvals — a hold, not a change
/// of intent. A blocked task (e.g. waiting on approval) cannot be
/// paused — it claims no steps anyway — and neither can a terminal
/// one; both are refused by `TaskStore::pause`.
pub fn pause_task(db: &Db, task_id: &str, note: &str, now: &Timestamp) -> Result<String> {
    let store = TaskStore::new(db);
    let task = store.pause(task_id, note, now)?;
    let done = UnitStore::new(db)
        .list_steps(task_id)?
        .iter()
        .filter(|s| s.status == ExecStatus::Done)
        .count();
    EventLog::new(db).append_control_event(
        task_id,
        "task_paused",
        "",
        &format!("任务已暂停：{note}（{done} 个步骤已完成；用 task run --task {task_id} 续跑）"),
        now,
    )?;
    Ok(format!(
        "task {} 已暂停（{} 个步骤已完成；用 task run --task {} 续跑）",
        task.id, done, task.id
    ))
}
