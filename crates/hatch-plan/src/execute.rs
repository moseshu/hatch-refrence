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

use crate::plan::{PlanStatus, RiskLevel, StepOutcome, StepStatus};
use crate::store::PlanStore;
use crate::validate::{check_step, detect_deviation, validate_step};
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Result};
use hatch_llm::ToolCall;
use hatch_policy::{ActionLedger, ApprovalQueue, ApprovalStatus, Gatekeeper};
use hatch_snapshots::SnapshotStore;
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, Registry, ToolContext, Workspace};
use std::path::PathBuf;

/// What the per-step receipt verifier says about one executed step
/// (M1, task 4): a step is `Done` only when its delivery is checked
/// against its `expected`, not merely because the tool did not error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepVerify {
    /// The delivery matches `expected` (optionally with a receipt id).
    Pass(Option<String>),
    /// The delivery demonstrably did not happen or does not match.
    Fail(String),
    /// The verifier cannot tell whether the side effect happened — the
    /// step fails, but its external state is unknown: the caller must
    /// show it, never silently retry it.
    Ambiguous(String),
}

/// Everything a run needs besides the plan itself.
/// Per-step receipt verifier (M1): called with the step and its tool
/// output right after a step executes, before it is marked done.
pub type StepVerifier<'a> = &'a dyn Fn(&crate::plan::PlanStep, &str) -> StepVerify;

pub struct RunDeps<'a> {
    pub db: &'a Db,
    pub registry: &'a Registry,
    pub executors: &'a ExecutorRegistry,
    pub gate: &'a Gatekeeper,
    pub tz: Tz,
    pub user_email: String,
    /// Workspace root the step `files` resolve against (`None` = file
    /// tools off, no snapshots). Same root the tool executors use.
    pub workspace_root: Option<PathBuf>,
    /// Snapshot root (`$HATCH_HOME/snapshots`; `None` = no snapshots).
    pub snapshot_root: Option<PathBuf>,
    /// Per-step receipt verifier (M1): called with the step and its
    /// tool output right after a step executes, before it is marked
    /// done. `None` = the pre-M1 rule (a step is done when its tool
    /// did not error).
    pub verify: Option<StepVerifier<'a>>,
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
    /// Snapshot ids taken before high-risk steps this run (D11).
    pub snapshots: Vec<String>,
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
            snapshots: vec![],
        });
    }

    // M1 (task 4): execution-time registry validation, before any
    // step runs and before the plan is marked executing. A plan whose
    // steps name a tool the registry no longer has — or touch files
    // with no workspace configured — fails here as a whole: no step
    // executes, no approval is queued, the plan stays `approved`.
    // (Creation time already validates, but a tool can be removed or a
    // step tampered with between approval and run.)
    let ws = match &deps.workspace_root {
        Some(root) => Some(Workspace::new(root)?),
        None => None,
    };
    for step in &plan.steps {
        validate_step(step, deps.registry, ws.as_ref())?;
    }

    store.set_running(plan_id, true, now)?;
    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut snapshots: Vec<String> = Vec::new();
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
                snapshots,
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
                outcomes.push(StepOutcome::plain(
                    step.id.clone(),
                    StepStatus::Done,
                    step.output.clone(),
                    None,
                ));
                continue;
            }
            StepStatus::AwaitingApproval => {
                match resolve_awaiting(deps.db, step.approval_id.as_deref())? {
                    Resolve::Done { result, receipt } => {
                        store.record_step(plan_id, idx, StepStatus::Done, &result, None, now)?;
                        outcomes.push(StepOutcome {
                            receipt: Some(receipt),
                            ..StepOutcome::plain(
                                step.id.clone(),
                                StepStatus::Done,
                                result,
                                None,
                            )
                        });
                        continue;
                    }
                    Resolve::StillWaiting => {
                        outcome = RunOutcome::PausedAwaitingApproval;
                        outcomes.push(StepOutcome::plain(
                            step.id.clone(),
                            StepStatus::AwaitingApproval,
                            "等待审批".into(),
                            step.approval_id.clone(),
                        ));
                        break;
                    }
                    Resolve::Rejected(detail) => {
                        store.record_step(plan_id, idx, StepStatus::Failed, &detail, None, now)?;
                        outcomes.push(StepOutcome::plain(
                            step.id.clone(),
                            StepStatus::Failed,
                            detail,
                            None,
                        ));
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                }
            }
            StepStatus::Pending | StepStatus::Failed => {
                // D11: snapshot the step's files before a high-risk step
                // runs. Best-effort — a skipped snapshot never fails the step.
                if let Some(id) = snapshot_step_files(deps, plan_id, step, now) {
                    snapshots.push(id);
                }
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
                    Settle::Done { output, receipt } => {
                        // M1 (task 4): a step is done only when its
                        // delivery is verified against `expected`, not
                        // merely because the tool did not error. A
                        // rejected delivery fails the step and stops the
                        // run; an ambiguous one fails it too, and the
                        // outcome carries the flag so the caller shows
                        // the unknown external state instead of
                        // retrying it silently.
                        let verified = match &deps.verify {
                            Some(v) => v(step, &output),
                            None => StepVerify::Pass(receipt.clone()),
                        };
                        match verified {
                            StepVerify::Pass(v_receipt) => {
                                // The verifier's receipt wins; without one
                                // the settle receipt (the ledger key of an
                                // executed approval) is the delivery proof.
                                let receipt = v_receipt.or(receipt);
                                store.record_step(
                                    plan_id,
                                    idx,
                                    StepStatus::Done,
                                    &output,
                                    None,
                                    now,
                                )?;
                                outcomes.push(StepOutcome {
                                    receipt,
                                    ..StepOutcome::plain(
                                        step.id.clone(),
                                        StepStatus::Done,
                                        output,
                                        None,
                                    )
                                });
                            }
                            StepVerify::Fail(reason) => {
                                store.record_step(
                                    plan_id,
                                    idx,
                                    StepStatus::Failed,
                                    &reason,
                                    None,
                                    now,
                                )?;
                                outcomes.push(StepOutcome::plain(
                                    step.id.clone(),
                                    StepStatus::Failed,
                                    reason,
                                    None,
                                ));
                                outcome = RunOutcome::StoppedOnFailure;
                                break;
                            }
                            StepVerify::Ambiguous(reason) => {
                                store.record_step(
                                    plan_id,
                                    idx,
                                    StepStatus::Failed,
                                    &reason,
                                    None,
                                    now,
                                )?;
                                outcomes.push(StepOutcome {
                                    ambiguous: true,
                                    ..StepOutcome::plain(
                                        step.id.clone(),
                                        StepStatus::Failed,
                                        reason,
                                        None,
                                    )
                                });
                                outcome = RunOutcome::StoppedOnFailure;
                                break;
                            }
                        }
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
                        outcomes.push(StepOutcome::plain(
                            step.id.clone(),
                            StepStatus::AwaitingApproval,
                            "已提交审批，等待用户确认".into(),
                            Some(approval_id),
                        ));
                        outcome = RunOutcome::PausedAwaitingApproval;
                        break;
                    }
                    Settle::Failed(error) => {
                        store.record_step(plan_id, idx, StepStatus::Failed, &error, None, now)?;
                        outcomes.push(StepOutcome::plain(
                            step.id.clone(),
                            StepStatus::Failed,
                            error,
                            None,
                        ));
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
        snapshots,
    })
}

/// Best-effort file snapshot before a high-risk step (D11). Only the
/// step's declared `files` are snapshotted, resolved through the same
/// [`Workspace`] guard the file tools use — a path that escapes the
/// workspace refuses the whole snapshot, it never snapshots a file
/// outside. Any failure (no workspace configured, no snapshot root,
/// unresolvable path, unreadable file, unwritable snapshot root) skips
/// the snapshot and returns `None`; the step still runs.
fn snapshot_step_files(
    deps: &RunDeps,
    plan_id: &str,
    step: &crate::plan::PlanStep,
    now: &Timestamp,
) -> Option<String> {
    if step.risk != RiskLevel::High || step.files.is_empty() {
        return None;
    }
    let ws_root = deps.workspace_root.as_ref()?;
    let snap_root = deps.snapshot_root.as_ref()?;
    let ws = Workspace::new(ws_root).ok()?;
    let mut paths = Vec::with_capacity(step.files.len());
    for f in &step.files {
        paths.push(ws.resolve(f).ok()?);
    }
    let store = SnapshotStore::new(snap_root);
    let manifest = store
        .create(&paths, &format!("plan:{plan_id} step:{}", step.id), now)
        .ok()?;
    Some(manifest.id)
}

enum Resolve {
    /// The approval executed; `receipt` is its ledger key.
    Done { result: String, receipt: String },
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
            Ok(Resolve::Done {
                result,
                receipt: approval.idempotency_key.clone(),
            })
        }
        ApprovalStatus::Pending | ApprovalStatus::Approved => Ok(Resolve::StillWaiting),
        ApprovalStatus::Rejected => Ok(Resolve::Rejected(format!("审批 {id} 已被拒绝"))),
        ApprovalStatus::Expired => Ok(Resolve::Rejected(format!("审批 {id} 已过期"))),
        ApprovalStatus::Invalidated => Ok(Resolve::Rejected(format!(
            "审批 {id} 已随计划版本失效"
        ))),
        ApprovalStatus::Failed => Ok(Resolve::Rejected(format!("审批 {id} 执行失败"))),
    }
}

enum Settle {
    /// `receipt` is the ledger key when an approval executed the step.
    Done { output: String, receipt: Option<String> },
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
                Settle::Done {
                    output: out,
                    receipt: Some(approval.idempotency_key.clone()),
                }
            }
            ApprovalStatus::Pending | ApprovalStatus::Approved => Settle::Queued {
                approval_id: id.clone(),
            },
            ApprovalStatus::Rejected => Settle::Failed(format!("审批 {id} 已被拒绝")),
            ApprovalStatus::Expired => Settle::Failed(format!("审批 {id} 已过期")),
            ApprovalStatus::Invalidated => {
                Settle::Failed(format!("审批 {id} 已随计划版本失效"))
            }
            ApprovalStatus::Failed => Settle::Failed(format!("审批 {id} 执行失败")),
        });
    }
    if let Some(error) = result.content.strip_prefix("ERROR: ") {
        return Ok(Settle::Failed(error.to_string()));
    }
    Ok(Settle::Done {
        output: result.content.clone(),
        receipt: None,
    })
}
