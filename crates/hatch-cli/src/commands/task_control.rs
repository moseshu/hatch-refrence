//! Control plane for durable tasks (M1, §10.2): versioned mid-task
//! commands and cancel. The worker never watches these; the CLI
//! applies them to the ledger directly, and the worker picks the
//! effect up on its next run (it reads the task row before starting
//! each step).

use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_plan::PlanStore;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskApprovals, TaskCommands, TaskStore};

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
    // The plan must agree on the expected version before anything
    // moves: the task row is the source of truth, the plan row is
    // kept in sync by the same command.
    if !task.plan_id.is_empty() {
        let plan = PlanStore::new(db)
            .get(&task.plan_id)?
            .ok_or_else(|| Error::NotFound(format!("计划 {} 找不到了", task.plan_id)))?;
        if plan.version != expected_version {
            return reject_command(
                db,
                task_id,
                expected_version,
                task.plan_version,
                summary,
                user_id,
                &format!(
                    "计划 {} 的版本已是 v{}，不是期望的 v{}（版本已推进，指令过期）",
                    plan.id, plan.version, expected_version
                ),
                now,
            );
        }
    }
    // The common stale case, checked before the bump so the rejection
    // carries a clean message (the bump's error would re-wrap it).
    if task.plan_version != expected_version {
        return reject_command(
            db,
            task_id,
            expected_version,
            task.plan_version,
            summary,
            user_id,
            &format!(
                "task {task_id} 的计划版本已是 v{}，不是期望的 v{}（版本已推进，指令过期）",
                task.plan_version, expected_version
            ),
            now,
        );
    }
    let bumped = match store.bump_plan_version(task_id, expected_version, now) {
        Ok(t) => t,
        Err(e) => {
            // The version was checked above, so this is a concurrent
            // bump between the check and the update (or the task
            // turned terminal): record it as rejected with the
            // underlying reason, without doubling the error prefix.
            let raw = e.to_string();
            let reason = raw.strip_prefix("invalid input: ").unwrap_or(&raw);
            return reject_command(
                db,
                task_id,
                expected_version,
                task.plan_version,
                summary,
                user_id,
                reason,
                now,
            );
        }
    };
    let new_version = bumped.plan_version;
    if !bumped.plan_id.is_empty() {
        // The check above makes this fail only on a concurrent bump;
        // the task version already moved, so report it in the event
        // rather than pretending the command did not apply.
        if let Err(e) = PlanStore::new(db).bump_version(&bumped.plan_id, expected_version, now) {
            EventLog::new(db).append_control_event(
                task_id,
                "plan_changed",
                "",
                &format!("指令已应用：任务计划 v{new_version}（{summary}）；计划版本未同步：{e}"),
                now,
            )?;
        }
    }
    let invalidated = invalidate_older(db, task_id, new_version, now)?;
    TaskCommands::new(db).record(
        task_id,
        expected_version,
        new_version,
        user_id,
        summary,
        "applied",
        now,
    )?;
    EventLog::new(db).append_control_event(
        task_id,
        "plan_changed",
        "",
        &format!(
            "指令已应用：计划 v{expected_version} → v{new_version}（{summary}）；{invalidated} 个旧审批已失效"
        ),
        now,
    )?;
    Ok(format!(
        "指令已应用：task {task_id} 的计划 v{expected_version} → v{new_version}，{invalidated} 个旧审批已失效"
    ))
}

/// Record a rejected command and surface why it was rejected.
#[allow(clippy::too_many_arguments)]
fn reject_command(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    current_version: u64,
    summary: &str,
    user_id: &str,
    reason: &str,
    now: &Timestamp,
) -> Result<String> {
    TaskCommands::new(db).record(
        task_id,
        expected_version,
        current_version,
        user_id,
        summary,
        "rejected",
        now,
    )?;
    EventLog::new(db).append_control_event(
        task_id,
        "command_rejected",
        "",
        &format!("指令已拒绝（过期）：{reason}；指令内容：{summary}"),
        now,
    )?;
    Err(Error::Invalid(reason.to_string()))
}

/// Invalidate every approval of this task bound to a version older
/// than `current` that could still trigger an action (Pending or
/// Approved). Already-decided ones keep their outcome; they never
/// needed invalidation. Returns how many were invalidated.
pub fn invalidate_older(db: &Db, task_id: &str, current: u64, now: &Timestamp) -> Result<usize> {
    let queue = ApprovalQueue::new(db);
    let mut count = 0;
    for id in TaskApprovals::new(db).older_than(task_id, current)? {
        let Ok(approval) = queue.get(&id) else {
            continue;
        };
        if !matches!(
            approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Approved
        ) {
            continue;
        }
        if queue
            .invalidate(&id, &format!("计划已变更为 v{current}，此审批已失效"), now)
            .is_ok()
        {
            count += 1;
        }
    }
    Ok(count)
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
