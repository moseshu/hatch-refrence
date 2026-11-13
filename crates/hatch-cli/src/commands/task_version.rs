//! Shared §10.2 version machinery (task 5): the version gate, the
//! conditional bump, the invalidate + record + event tail, and the
//! rejection record. One implementation serves `apply_command` and
//! `edit_step` — a second version mechanism would let the two drift
//! apart, which is exactly what the §10.2 invalidation is for.

use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_plan::PlanStore;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{EventLog, TaskApprovals, TaskCommands, TaskStore};

/// The version gate shared by `apply_command` and `edit_step`
/// (task 5): the plan row (when the task runs one) and the task row
/// must both still be at the version the user saw. A mismatch is
/// recorded as a rejected command and returned as an error — nothing
/// moves.
pub(crate) fn check_expected_version(
    db: &Db,
    task: &hatch_tasks::Task,
    expected_version: u64,
    summary: &str,
    user_id: &str,
    now: &Timestamp,
) -> Result<()> {
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
                &task.id,
                expected_version,
                task.plan_version,
                summary,
                user_id,
                &format!(
                    "计划 {} 的版本已是 v{}，不是期望的 v{}（版本已推进，指令过期）",
                    plan.id, plan.version, expected_version
                ),
                now,
            )
            .map(|_| ());
        }
    }
    // The common stale case, checked before the bump so the rejection
    // carries a clean message (the bump's error would re-wrap it).
    if task.plan_version != expected_version {
        return reject_command(
            db,
            &task.id,
            expected_version,
            task.plan_version,
            summary,
            user_id,
            &format!(
                "task {} 的计划版本已是 v{}，不是期望的 v{}（版本已推进，指令过期）",
                task.id, task.plan_version, expected_version
            ),
            now,
        )
        .map(|_| ());
    }
    Ok(())
}

/// Bump the task's — and its plan's — version (first half of the
/// §10.2 command flow, shared by `apply_command` and `edit_step`).
/// The bump is conditional on the expected version, so a concurrent
/// bump fails here before any content changes. Returns the new
/// version. A plan-bump failure is non-fatal and logged in an event
/// (the task version already moved; pretending otherwise would lie).
/// `verb` is the leading word of the event text ("指令已应用" /
/// "步骤已编辑").
#[allow(clippy::too_many_arguments)]
pub(crate) fn bump_version(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    summary: &str,
    user_id: &str,
    verb: &str,
    now: &Timestamp,
) -> Result<u64> {
    let store = TaskStore::new(db);
    let current = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    let bumped = match store.bump_plan_version(task_id, expected_version, now) {
        Ok(t) => t,
        Err(e) => {
            // The version was checked above, so this is a concurrent
            // bump between the check and the update (or the task
            // turned terminal): record it as rejected with the
            // underlying reason, without doubling the error prefix.
            let raw = e.to_string();
            let reason = raw.strip_prefix("invalid input: ").unwrap_or(&raw);
            // The record is the point: `reject_command` writes the
            // rejection and returns the same error; re-raise it here.
            let _ = reject_command(
                db,
                task_id,
                expected_version,
                current.plan_version,
                summary,
                user_id,
                reason,
                now,
            );
            return Err(Error::Invalid(reason.to_string()));
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
                &format!("{verb}：任务计划 v{new_version}（{summary}）；计划版本未同步：{e}"),
                now,
            )?;
        }
    }
    Ok(new_version)
}

/// Invalidate approvals bound to older versions, record the command,
/// and log the event (second half of the §10.2 command flow, shared
/// by `apply_command` and `edit_step`). Called only after the
/// command's content change landed, so a recorded "applied" command
/// always means the change is in the ledger. Returns how many
/// approvals were invalidated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_command(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    new_version: u64,
    summary: &str,
    user_id: &str,
    verb: &str,
    now: &Timestamp,
) -> Result<usize> {
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
            "{verb}：计划 v{expected_version} → v{new_version}（{summary}）；{invalidated} 个旧审批已失效"
        ),
        now,
    )?;
    Ok(invalidated)
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
pub(crate) fn invalidate_older(
    db: &Db,
    task_id: &str,
    current: u64,
    now: &Timestamp,
) -> Result<usize> {
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
