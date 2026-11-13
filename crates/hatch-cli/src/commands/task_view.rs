//! The task list / detail projection (task 5): one read path for the
//! CLI (`task list` / `task show`) and the daemon API (`GET /v1/tasks`
//! ...). The ledger is the only source; the view never keeps its own
//! state, so the two clients cannot drift apart. A task appears
//! exactly once: the list is a `SELECT` over `tasks`, and the detail
//! is keyed by task id — a duplicate row is a bug, not a display
//! choice.

use hatch_core::{Error, Result};
use hatch_plan::PlanStore;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{ExecStatus, Task, TaskApprovals, TaskStatus, TaskStore, UnitStore};
use serde::Serialize;

/// One row of `task list` — and one element of `GET /v1/tasks`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub plan_version: u64,
    pub steps_done: usize,
    pub steps_total: usize,
    pub pending_approvals: usize,
    pub lease_owner: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One step of the plan card — `task show` and `GET /v1/tasks/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskStepView {
    pub step_id: String,
    pub title: String,
    pub tool: String,
    pub expected: String,
    pub status: String,
    pub attempts: u32,
    pub approval_id: String,
    pub receipt_id: String,
    pub output: String,
}

/// One bound approval — `task show` and `GET /v1/tasks/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskApprovalView {
    pub id: String,
    pub step_id: String,
    pub plan_version: u64,
    pub status: String,
}

/// The full detail — `task show` and `GET /v1/tasks/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskDetail {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    /// Mail source (M2, schema v15): `fixture` / `gmail` / ''.
    pub source: String,
    pub plan_id: String,
    pub plan_version: u64,
    pub plan_status: String,
    pub goal: String,
    pub blocked_kind: String,
    pub error: String,
    pub retries: u32,
    pub max_retries: u32,
    pub max_model_calls: u64,
    pub model_calls: u64,
    pub lease_owner: String,
    pub lease_generation: u64,
    pub lease_expires_at: String,
    pub steps_done: usize,
    pub steps_total: usize,
    pub current_step: String,
    pub steps: Vec<TaskStepView>,
    pub approvals: Vec<TaskApprovalView>,
    pub created_at: String,
    pub updated_at: String,
}

/// Summarize one task: step progress + pending approvals, read
/// straight from the ledger (steps, approvals). No second state.
pub fn summarize(db: &Db, task: &Task) -> Result<TaskSummary> {
    let steps = UnitStore::new(db).list_steps(&task.id)?;
    let done = steps
        .iter()
        .filter(|s| s.status == ExecStatus::Done)
        .count();
    Ok(TaskSummary {
        id: task.id.clone(),
        kind: task.kind.clone(),
        title: task.title.clone(),
        status: task.status.as_str().to_string(),
        plan_version: task.plan_version,
        steps_done: done,
        steps_total: steps.len(),
        pending_approvals: pending_approvals(db, &task.id)?,
        lease_owner: task.lease_owner.clone(),
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
    })
}

/// List summaries, newest first — the same projection the API serves.
pub fn list_summaries(
    db: &Db,
    status: Option<TaskStatus>,
    limit: usize,
) -> Result<Vec<TaskSummary>> {
    let store = TaskStore::new(db);
    let mut out = Vec::new();
    for task in store.list(status, limit)? {
        out.push(summarize(db, &task)?);
    }
    Ok(out)
}

/// How many of the task's bound approvals are still pending.
fn pending_approvals(db: &Db, task_id: &str) -> Result<usize> {
    let mut count = 0;
    for a in TaskApprovals::new(db).for_task(task_id)? {
        if approval_status(db, &a.approval_id) == Some(ApprovalStatus::Pending) {
            count += 1;
        }
    }
    Ok(count)
}

/// The approval's status; `None` when the queue no longer knows it
/// (orphan — the view shows "unknown" instead of failing).
pub(crate) fn approval_status(db: &Db, approval_id: &str) -> Option<ApprovalStatus> {
    ApprovalQueue::new(db)
        .get(approval_id)
        .ok()
        .map(|a| a.status)
}

/// The full detail for one task: steps with their acceptance, the
/// current step, bound approvals with their status, and the plan's
/// goal + status when the task runs a plan.
pub fn detail(db: &Db, task_id: &str) -> Result<TaskDetail> {
    let store = TaskStore::new(db);
    let task = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    let units = UnitStore::new(db);
    let steps = units.list_steps(task_id)?;
    let done = steps
        .iter()
        .filter(|s| s.status == ExecStatus::Done)
        .count();
    let current_step = steps
        .iter()
        .find(|s| s.status != ExecStatus::Done)
        .map(|s| s.step_id.clone())
        .unwrap_or_default();
    let mut plan_status = String::new();
    let mut goal = String::new();
    if !task.plan_id.is_empty() {
        match PlanStore::new(db).get(&task.plan_id)? {
            Some(plan) => {
                plan_status = plan.status.as_str().to_string();
                goal = plan.goal.clone();
            }
            None => plan_status = "unknown".to_string(),
        }
    }
    let approvals = TaskApprovals::new(db)
        .for_task(task_id)?
        .into_iter()
        .map(|a| TaskApprovalView {
            id: a.approval_id.clone(),
            step_id: a.step_id.clone(),
            plan_version: a.plan_version,
            status: approval_status(db, &a.approval_id)
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        })
        .collect();
    Ok(TaskDetail {
        id: task.id.clone(),
        kind: task.kind.clone(),
        title: task.title.clone(),
        status: task.status.as_str().to_string(),
        source: task.source.clone(),
        plan_id: task.plan_id.clone(),
        plan_version: task.plan_version,
        plan_status,
        goal,
        blocked_kind: task.blocked_kind.as_str().to_string(),
        error: task.error.clone(),
        retries: task.retries,
        max_retries: task.max_retries,
        max_model_calls: task.max_model_calls,
        model_calls: units.total_model_calls(task_id)?,
        lease_owner: task.lease_owner.clone(),
        lease_generation: task.lease_generation,
        lease_expires_at: task.lease_expires_at.clone(),
        steps_done: done,
        steps_total: steps.len(),
        current_step,
        steps: steps
            .into_iter()
            .map(|s| TaskStepView {
                step_id: s.step_id,
                title: s.title,
                tool: s.tool,
                expected: s.expected,
                status: s.status.as_str().to_string(),
                attempts: s.attempts,
                approval_id: s.approval_id,
                receipt_id: s.receipt_id,
                output: s.output,
            })
            .collect(),
        approvals,
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
    })
}

fn status_cn_str(s: &str) -> &'static str {
    match s {
        "queued" => "queued（排队）",
        "running" => "running（运行中）",
        "waiting" => "waiting（等待中）",
        "blocked" => "blocked（阻塞）",
        "done" => "done（完成）",
        "failed" => "failed（失败）",
        "cancelled" => "cancelled（已取消）",
        _ => "unknown（未知）",
    }
}

/// The done mark for a step, in the Claude Code plan-card shape the
/// user asked for (2026-11-10): a step list with a mark per step.
fn step_mark(status: &str) -> &'static str {
    match status {
        "done" => "✔",
        "running" => "▶",
        "failed" => "✗",
        _ => "○",
    }
}

/// One line per task for `task list`.
pub fn render_summary_line(s: &TaskSummary) -> String {
    let lease = if s.lease_owner.is_empty() {
        "-".to_string()
    } else {
        s.lease_owner.clone()
    };
    format!(
        "{}  {}  {}  {}  v{}  {}/{} 步  待审批 {}  lease={}",
        s.id,
        s.kind,
        s.title,
        s.status,
        s.plan_version,
        s.steps_done,
        s.steps_total,
        s.pending_approvals,
        lease
    )
}

/// The plan card for `task show`: goal, steps with acceptance +
/// current step + done marks + per-step receipt/evidence, plan
/// version, approvals with status, blocked reason.
pub fn render_detail(d: &TaskDetail) -> String {
    let mut out = String::new();
    out.push_str(&format!("task {}  {}\n", d.id, status_cn_str(&d.status)));
    out.push_str(&format!("kind: {}  title: {}\n", d.kind, d.title));
    if !d.source.is_empty() {
        out.push_str(&format!("来源：{}\n", d.source));
    }
    if d.plan_id.is_empty() {
        out.push_str(&format!(
            "plan: v{}  budget: {} 次模型调用（0 = 不限）\n",
            d.plan_version, d.max_model_calls
        ));
    } else {
        out.push_str(&format!(
            "plan: {} v{}（{}）  budget: {} 次模型调用（0 = 不限）\n",
            d.plan_id, d.plan_version, d.plan_status, d.max_model_calls
        ));
    }
    if !d.goal.is_empty() {
        out.push_str(&format!("目标：{}\n", d.goal));
    }
    out.push_str(&format!(
        "retries: {}/{}  error: {}\n",
        d.retries, d.max_retries, d.error
    ));
    if !d.lease_owner.is_empty() {
        out.push_str(&format!(
            "lease: {} (generation {}, expires {})\n",
            d.lease_owner, d.lease_generation, d.lease_expires_at
        ));
    }
    if !d.blocked_kind.is_empty() {
        out.push_str(&format!("blocked: {}\n", d.blocked_kind));
    }
    out.push_str(&format!("模型调用：{} 次\n", d.model_calls));
    out.push_str(&format!("步骤 {}/{}：\n", d.steps_done, d.steps_total));
    for step in &d.steps {
        let current = if step.step_id == d.current_step {
            "（当前）"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {} {} {}（尝试 {} 次）{}\n",
            step_mark(&step.status),
            step.step_id,
            step.title,
            step.attempts,
            current
        ));
        if !step.expected.is_empty() {
            out.push_str(&format!("    预期：{}\n", step.expected));
        }
        if !step.output.is_empty() {
            out.push_str(&format!("    输出：{}\n", step.output));
        }
        let extra = [
            (!step.approval_id.is_empty()).then(|| format!("approval={}", step.approval_id)),
            (!step.receipt_id.is_empty()).then(|| format!("receipt={}", step.receipt_id)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        if !extra.is_empty() {
            out.push_str(&format!("    {extra}\n"));
        }
    }
    if !d.approvals.is_empty() {
        out.push_str("审批：\n");
        for a in &d.approvals {
            out.push_str(&format!(
                "  {}（步骤 {}，v{}）{}\n",
                a.id, a.step_id, a.plan_version, a.status
            ));
        }
    }
    out
}
