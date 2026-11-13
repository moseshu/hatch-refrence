//! Step editing for durable tasks (task 5, §9 task 5 + §10.2): edit a
//! pending step's content through the same version mechanism as
//! `apply_command` — the command carries the expected version, a
//! stale version is rejected, a successful edit bumps the version
//! and invalidates old approvals.

use super::task_version::{bump_version, check_expected_version, finalize_command};
use super::task_view::approval_status;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_plan::{PlanStore, StepEdit as PlanStepEdit, StepStatus};
use hatch_policy::ApprovalStatus;
use hatch_store::Db;
use hatch_tasks::{ExecStatus, StepEdit, TaskStore, UnitStore};

/// Edit a pending step's content (task 5, §9 task 5 + §10.2): the
/// command carries the plan version the user saw; a stale version is
/// rejected, a successful edit bumps the version and invalidates old
/// approvals — the same mechanism as `apply_command`, so a concurrent
/// edit with the same expected version loses at the bump before it
/// can touch any step.
///
/// Order: version check → pre-check the step (pending, plan
/// editable) → bump → edit the plan step (when the task runs one)
/// → edit the mirror step → invalidate + record. The pre-checks make
/// a bumped version with no edit possible only on a storage error
/// mid-edit.
#[allow(clippy::too_many_arguments)]
pub fn edit_step(
    db: &Db,
    task_id: &str,
    step_id: &str,
    expected_version: u64,
    edit: &StepEdit,
    user_id: &str,
    now: &Timestamp,
) -> Result<String> {
    if edit.title.is_none()
        && edit.tool.is_none()
        && edit.arguments.is_none()
        && edit.expected.is_none()
    {
        return Err(Error::Invalid("没有要改的内容".into()));
    }
    let store = TaskStore::new(db);
    let task = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    if task.status.is_terminal() {
        return Err(Error::Invalid(format!(
            "task {} 已是终态 {}，不能编辑步骤",
            task.id,
            task.status.as_str()
        )));
    }
    let summary = describe_edit(step_id, edit);
    check_expected_version(db, &task, expected_version, &summary, user_id, now)?;
    // Pre-check the mirror: a pending step may be edited, and so may
    // a running step gated on a still-pending approval — its side
    // effect has not executed yet. Any other running step is actively
    // executing, and a done/failed step already ran; neither can be
    // rewritten.
    let units = UnitStore::new(db);
    let mirror = units
        .get_step(task_id, step_id)?
        .ok_or_else(|| Error::NotFound(format!("step {step_id} 不存在")))?;
    let editable = match mirror.status {
        ExecStatus::Pending => true,
        ExecStatus::Running => {
            !mirror.approval_id.is_empty()
                && approval_status(db, &mirror.approval_id) == Some(ApprovalStatus::Pending)
        }
        _ => false,
    };
    if !editable {
        return Err(Error::Invalid(format!(
            "步骤 {} 已是 {}，不能编辑（只能改未执行或待审批的步骤）",
            mirror.step_id,
            mirror.status.as_str()
        )));
    }
    // Pre-check the plan step when the task runs a plan: it must be
    // pending or awaiting approval (an awaiting-approval step is
    // dispatched but gated, like the mirror's running-but-gated
    // step), and the plan itself must be editable —
    // `PlanStore::edit_step` refuses executing/completed/terminal
    // plans.
    let has_plan = !task.plan_id.is_empty();
    if has_plan {
        let plan = PlanStore::new(db)
            .get(&task.plan_id)?
            .ok_or_else(|| Error::NotFound(format!("计划 {} 找不到了", task.plan_id)))?;
        let pstep = plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| Error::NotFound(format!("计划 {} 里没有步骤 {step_id}", plan.id)))?;
        if !matches!(
            pstep.status,
            StepStatus::Pending | StepStatus::AwaitingApproval
        ) {
            return Err(Error::Invalid(format!(
                "计划步骤 {} 已是 {}，不能编辑（只能改未执行或待审批的步骤）",
                pstep.id,
                pstep.status.as_str()
            )));
        }
    }
    let new_version = bump_version(
        db,
        task_id,
        expected_version,
        &summary,
        user_id,
        "步骤已编辑",
        now,
    )?;
    if has_plan {
        PlanStore::new(db).edit_step(
            &task.plan_id,
            step_id,
            PlanStepEdit {
                title: edit.title.clone(),
                tool: edit.tool.clone(),
                arguments: edit.arguments.clone(),
                expected: edit.expected.clone(),
                ..Default::default()
            },
            now,
        )?;
    }
    units.update_step(task_id, step_id, edit, now)?;
    let invalidated = finalize_command(
        db,
        task_id,
        expected_version,
        new_version,
        &summary,
        user_id,
        "步骤已编辑",
        now,
    )?;
    Ok(format!(
        "步骤 {step_id} 已编辑：task {task_id} 的计划 v{expected_version} → v{new_version}，{invalidated} 个旧审批已失效"
    ))
}

/// One-line description of a step edit for the command record.
fn describe_edit(step_id: &str, edit: &StepEdit) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if edit.title.is_some() {
        parts.push("标题");
    }
    if edit.tool.is_some() {
        parts.push("工具");
    }
    if edit.arguments.is_some() {
        parts.push("参数");
    }
    if edit.expected.is_some() {
        parts.push("预期");
    }
    format!("编辑步骤 {}：{}", step_id, parts.join("、"))
}
