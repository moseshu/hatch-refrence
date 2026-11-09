//! Run an approved plan, one step at a time.
//!
//! Execution goes through the same [`dispatch_call`] gate path as the chat
//! agent — a plan never bypasses the gate, and a step whose action needs
//! consent is queued in the approval queue exactly like a chat tool call.
//!
//! Before each step the runner checks the step against the approved
//! snapshot (`tool` + `arguments` + `files`). Any deviation — a changed
//! tool, changed arguments, changed files, an extra step or a skipped step —
//! stops the run immediately and flags the plan `needs_reapproval` with the
//! deviation recorded. Nothing not in the approved plan ever executes.
//!
//! A run that stops mid-way (failed step, or a step waiting on a gate
//! approval) leaves the plan `executing` with per-step results recorded, so
//! `hatch plan run` resumes from the first unfinished step instead of
//! re-running completed ones.

use crate::plan::{PlanStatus, StepOutcome, StepStatus};
use crate::store::PlanStore;
use crate::validate::{check_step, detect_deviation};
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Result};
use hatch_llm::ToolCall;
use hatch_policy::{ActionLedger, ApprovalQueue, ApprovalStatus, Gatekeeper};
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, Registry, ToolContext};

/// Everything a run needs besides the plan itself.
pub struct RunDeps<'a> {
    pub db: &'a Db,
    pub registry: &'a Registry,
    pub executors: &'a ExecutorRegistry,
    pub gate: &'a Gatekeeper,
    pub tz: Tz,
    pub user_email: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    /// A step was queued for a gate approval; run again after deciding it.
    PausedAwaitingApproval,
    /// A step failed (tool error / gate denial); plan stays `executing`.
    StoppedOnFailure,
    /// The plan no longer matches the approved snapshot.
    StoppedOnDeviation,
}

#[derive(Clone, Debug)]
pub struct RunReport {
    pub plan_id: String,
    pub outcome: RunOutcome,
    pub steps: Vec<StepOutcome>,
    pub deviation: Option<String>,
}

/// Run (or resume) an approved plan. Only `approved` and `executing` plans
/// may run; anything else is an error.
pub fn run_plan(deps: &RunDeps, plan_id: &str, now: &Timestamp) -> Result<RunReport> {
    let store = PlanStore::new(deps.db);
    let plan = store
        .get(plan_id)?
        .ok_or_else(|| hatch_core::Error::NotFound(format!("plan {plan_id} 不存在")))?;
    if !matches!(plan.status, PlanStatus::Approved | PlanStatus::Executing) {
        return Err(hatch_core::Error::Invalid(format!(
            "plan {} 处于 {}，不能执行（先批准）",
            plan_id,
            plan.status.as_str()
        )));
    }
    let approved = store.approved_steps(&plan)?;
    if approved.is_empty() {
        return Err(hatch_core::Error::Invalid(format!(
            "plan {plan_id} 没有批准快照，无法执行"
        )));
    }
    // Whole-plan deviation check up front (extra / missing steps).
    if let Some(d) = detect_deviation(&approved, &plan.steps) {
        let desc = d.describe();
        store.flag_deviation(plan_id, &desc, now)?;
        return Ok(RunReport {
            plan_id: plan_id.into(),
            outcome: RunOutcome::StoppedOnDeviation,
            steps: vec![],
            deviation: Some(desc),
        });
    }

    store.set_running(plan_id, true, now)?;
    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut outcome = RunOutcome::Completed;
    for (idx, step) in plan.steps.iter().enumerate() {
        // Per-step check against the approved snapshot, right before the step.
        if let Some(d) = approved.get(idx).and_then(|a| check_step(a, step)) {
            let desc = d.describe();
            store.flag_deviation(plan_id, &desc, now)?;
            outcome = RunOutcome::StoppedOnDeviation;
            return Ok(RunReport {
                plan_id: plan_id.into(),
                outcome,
                steps: outcomes,
                deviation: Some(desc),
            });
        }
        // Execution has reached this step. Done steps are skipped below
        // without moving the pointer; a failed / paused / finished run
        // leaves it at the step it stopped on, and completion clears it.
        if step.status != StepStatus::Done {
            store.set_current_step(plan_id, Some(&step.id), now)?;
        }
        match step.status {
            StepStatus::Done => {
                outcomes.push(StepOutcome {
                    step_id: step.id.clone(),
                    status: StepStatus::Done,
                    output: step.output.clone(),
                    approval_id: None,
                });
                continue;
            }
            StepStatus::AwaitingApproval => {
                match resolve_awaiting(deps.db, step.approval_id.as_deref())? {
                    Resolve::Done(result) => {
                        store.record_step(plan_id, idx, StepStatus::Done, &result, None, now)?;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::Done,
                            output: result,
                            approval_id: None,
                        });
                        continue;
                    }
                    Resolve::StillWaiting => {
                        outcome = RunOutcome::PausedAwaitingApproval;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::AwaitingApproval,
                            output: "等待审批".into(),
                            approval_id: step.approval_id.clone(),
                        });
                        break;
                    }
                    Resolve::Rejected(detail) => {
                        store.record_step(plan_id, idx, StepStatus::Failed, &detail, None, now)?;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::Failed,
                            output: detail,
                            approval_id: None,
                        });
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                }
            }
            StepStatus::Pending | StepStatus::Failed => {
                let call = ToolCall {
                    id: format!("plan:{plan_id}:{}", step.id),
                    name: step.tool.clone(),
                    arguments: step.arguments.clone(),
                };
                let ctx = ToolContext {
                    db: deps.db,
                    now: *now,
                    tz: deps.tz,
                    user_email: deps.user_email.clone(),
                    origin: format!("plan:{plan_id}"),
                    tainted: false,
                    call_id: call.id.clone(),
                    cancel: CancelToken::new(),
                };
                let result = dispatch_call(deps.registry, deps.executors, deps.gate, &call, &ctx);
                match settle_dispatched(deps.db, &result)? {
                    Settle::Done(output) => {
                        store.record_step(plan_id, idx, StepStatus::Done, &output, None, now)?;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::Done,
                            output,
                            approval_id: None,
                        });
                    }
                    Settle::Queued { approval_id } => {
                        store.record_step(
                            plan_id,
                            idx,
                            StepStatus::AwaitingApproval,
                            "已提交审批，等待用户确认",
                            Some(&approval_id),
                            now,
                        )?;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::AwaitingApproval,
                            output: "已提交审批，等待用户确认".into(),
                            approval_id: Some(approval_id),
                        });
                        outcome = RunOutcome::PausedAwaitingApproval;
                        break;
                    }
                    Settle::Failed(error) => {
                        store.record_step(plan_id, idx, StepStatus::Failed, &error, None, now)?;
                        outcomes.push(StepOutcome {
                            step_id: step.id.clone(),
                            status: StepStatus::Failed,
                            output: error,
                            approval_id: None,
                        });
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                }
            }
        }
    }
    if outcome == RunOutcome::Completed {
        store.mark_completed(plan_id, now)?;
    }
    Ok(RunReport {
        plan_id: plan_id.into(),
        outcome,
        steps: outcomes,
        deviation: None,
    })
}

enum Resolve {
    Done(String),
    StillWaiting,
    Rejected(String),
}

/// What happened to a step that was waiting on a gate approval.
fn resolve_awaiting(db: &Db, approval_id: Option<&str>) -> Result<Resolve> {
    let Some(id) = approval_id else {
        return Ok(Resolve::Rejected(
            "等待审批的步骤没有记录 approval id".into(),
        ));
    };
    let approval = ApprovalQueue::new(db).get(id)?;
    match approval.status {
        ApprovalStatus::Executed => {
            let result = ActionLedger::new(db)
                .result(&approval.idempotency_key)?
                .unwrap_or_default();
            Ok(Resolve::Done(result))
        }
        ApprovalStatus::Pending | ApprovalStatus::Approved => Ok(Resolve::StillWaiting),
        ApprovalStatus::Rejected => Ok(Resolve::Rejected(format!("审批 {id} 已被拒绝"))),
        ApprovalStatus::Expired => Ok(Resolve::Rejected(format!("审批 {id} 已过期"))),
        ApprovalStatus::Failed => Ok(Resolve::Rejected(format!("审批 {id} 执行失败"))),
    }
}

enum Settle {
    Done(String),
    Queued { approval_id: String },
    Failed(String),
}

/// Interpret a [`dispatch_call`] result. When an approval id is present the
/// approval queue is the source of truth (not the message prefix).
fn settle_dispatched(db: &Db, result: &hatch_tools::DispatchResult) -> Result<Settle> {
    if let Some(id) = &result.approval_id {
        let approval = ApprovalQueue::new(db).get(id)?;
        return Ok(match approval.status {
            ApprovalStatus::Executed => {
                let out = ActionLedger::new(db)
                    .result(&approval.idempotency_key)?
                    .unwrap_or_else(|| result.content.clone());
                Settle::Done(out)
            }
            ApprovalStatus::Pending | ApprovalStatus::Approved => Settle::Queued {
                approval_id: id.clone(),
            },
            ApprovalStatus::Rejected => Settle::Failed(format!("审批 {id} 已被拒绝")),
            ApprovalStatus::Expired => Settle::Failed(format!("审批 {id} 已过期")),
            ApprovalStatus::Failed => Settle::Failed(format!("审批 {id} 执行失败")),
        });
    }
    if let Some(error) = result.content.strip_prefix("ERROR: ") {
        return Ok(Settle::Failed(error.to_string()));
    }
    Ok(Settle::Done(result.content.clone()))
}
