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
use crate::verification::{self, VerificationMode, VerificationStore};
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
/// output after mandatory independent readback, before it is marked done.
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
    /// done. `None` uses the mandatory independent evidence checks; a callback
    /// adds workflow criteria and cannot override failed readback.
    pub verify: Option<StepVerifier<'a>>,
    /// Output transform, applied to a step's settled tool output
    /// before it is verified and recorded (M2): a caller whose tools
    /// answer in code space (desensitized mail) maps the output back
    /// to real names here, so the receipt check, the plan row and the
    /// report all carry the user-facing form. `None` = identity.
    pub output_map: Option<&'a dyn Fn(&str) -> String>,
    /// The user's own name, for tools that render mail for the model
    /// (the desensitize mapping needs it to hide the name as well as
    /// the address). Empty when the caller does not know it.
    pub user_name: String,
    /// Task lease fence for verification writes: when the plan runs
    /// under a task worker's lease, verification commits require that
    /// exact lease to still hold. `None` for direct user runs.
    pub lease_fence: Option<crate::verification::LeaseFence>,
    /// Isolated semantic grader for the contract's NL items (L3),
    /// supplied by a caller that owns a model router. `None` records
    /// no L3 checks (batch-1 behaviour); with it, the whole NL group
    /// is graded once over the shared evidence bundle, in Observe.
    pub grade: Option<crate::acceptance::NlGrader<'a>>,
    /// Which acceptance layers gate completion (default: none —
    /// L2/L3 Observe). Flipping a layer to Enforce is a per-class
    /// decision backed by evaluation data, never a code default.
    pub acceptance_enforce: crate::acceptance::AcceptanceEnforce,
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

    deps.db.transaction(|c| {
        crate::verification_store::guard(c, plan_id, plan.version as i64, deps.lease_fence.as_ref())?;
        store.set_running(plan_id, true, now)
    })?;
    let plan_mode = VerificationStore::with_fence(deps.db, deps.lease_fence.clone())
        .records(plan_id)?
        .into_iter()
        .find(|(_, v, _)| *v == plan.version as i64)
        .map(|(_, _, r)| r.mode)
        .map(Ok)
        .unwrap_or_else(VerificationMode::configured)?;
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
        let verification_store = VerificationStore::with_fence(deps.db, deps.lease_fence.clone());
        let frozen = verification::contract(&plan, step)?;
        let mut previous = verification_store.get(plan_id, plan.version as i64, &step.id)?;
        if let Some(record) = previous.as_ref() {
            if record.contract != frozen {
                return Err(hatch_core::Error::Invalid(
                    "验收规则或目标变化，需要新计划版本".into(),
                ));
            }
            if matches!(record.phase.as_str(), "executed" | "verified") {
                let checked = crate::completion::finish(deps, &plan, step, idx, now)?;
                let done = checked.status == StepStatus::Done;
                outcomes.push(checked);
                if !done {
                    outcome = RunOutcome::StoppedOnFailure;
                    break;
                }
                continue;
            }
            if record.phase == "dispatching" {
                // A crash between dispatch and receipt may have applied
                // an effect. For a read-only step nothing can have been
                // applied, so the stale dispatch record is cleared and
                // the step re-dispatches below. Anything else (including
                // a tool this registry does not know) stays conservative:
                // never dispatch again just because verification is missing.
                let read_only = deps
                    .registry
                    .get(&step.tool)
                    .map(|t| {
                        matches!(
                            t.class(),
                            hatch_policy::ActionClass::ReadPersonal
                                | hatch_policy::ActionClass::FetchWeb
                                | hatch_policy::ActionClass::BrowseLocal
                        )
                    })
                    .unwrap_or(false);
                if read_only {
                    verification_store.rejected(plan_id, plan.version as i64, &step.id)?;
                    previous = None;
                } else {
                    let reason = "执行在保存回执前中断，外部状态不明；请人工核对，不会自动重做";
                    store.record_step_classified(
                        plan_id,
                        idx,
                        StepStatus::Failed,
                        reason,
                        None,
                        Some(crate::FailureKind::NeedsHuman),
                        now,
                    )?;
                    outcomes.push(StepOutcome {
                        ambiguous: true,
                        ..StepOutcome::failed(
                            step.id.clone(),
                            reason.into(),
                            crate::FailureKind::NeedsHuman,
                        )
                    });
                    outcome = RunOutcome::StoppedOnFailure;
                    break;
                }
            }
        }
        if previous.is_none() {
            if !verification_store.begin(
                plan_id,
                plan.version as i64,
                &step.id,
                &frozen,
                plan_mode,
                now,
            )? {
                return Err(hatch_core::Error::Invalid(
                    "另一个执行器已领取该步骤".into(),
                ));
            }
            if step.status == StepStatus::AwaitingApproval {
                verification_store.waiting(
                    plan_id,
                    plan.version as i64,
                    &step.id,
                    step.approval_id.as_deref().unwrap_or(""),
                    now,
                )?;
            }
        }
        let persisted_approval = previous
            .as_ref()
            .filter(|r| r.phase == "waiting")
            .map(|r| r.approval_id.as_str());
        let approval_id = persisted_approval.or(step.approval_id.as_deref());
        let status = if persisted_approval.is_some() {
            StepStatus::AwaitingApproval
        } else {
            step.status
        };
        match status {
            StepStatus::Done => {
                // Unchanged completed steps survive a local replan. Carry the
                // original immutable payload/receipt forward, then read it anew.
                let inherited =
                    verification_store
                        .records(plan_id)?
                        .into_iter()
                        .rev()
                        .find(|(id, v, r)| {
                            id == &step.id
                                && *v < plan.version as i64
                                && r.contract == frozen
                                && matches!(r.phase.as_str(), "executed" | "verified")
                        });
                if let Some((_, _, r)) = inherited {
                    verification_store.executed(&crate::verification::Executed {
                        owner: plan_id,
                        version: plan.version as i64,
                        step: &step.id,
                        output: &r.output,
                        receipt: &r.receipt,
                        approval: &r.approval_id,
                        now,
                    })?;
                } else {
                    // Legacy rows have no proof; they are checked, never relabelled verified.
                    verification_store.executed(&crate::verification::Executed {
                        owner: plan_id,
                        version: plan.version as i64,
                        step: &step.id,
                        output: &step.output,
                        receipt: "",
                        approval: step.approval_id.as_deref().unwrap_or(""),
                        now,
                    })?;
                }
                let checked = crate::completion::finish(deps, &plan, step, idx, now)?;
                let done = checked.status == StepStatus::Done;
                outcomes.push(checked);
                if !done {
                    outcome = RunOutcome::StoppedOnFailure;
                    break;
                }
                continue;
            }
            StepStatus::AwaitingApproval => {
                match resolve_awaiting(deps.db, approval_id)? {
                    Resolve::Done { result, receipt } => {
                        verification_store.executed(&crate::verification::Executed {
                            owner: plan_id,
                            version: plan.version as i64,
                            step: &step.id,
                            output: &result,
                            receipt: &receipt,
                            approval: approval_id.unwrap_or(""),
                            now,
                        })?;
                        let checked = crate::completion::finish(deps, &plan, step, idx, now)?;
                        let done = checked.status == StepStatus::Done;
                        outcomes.push(checked);
                        if !done {
                            outcome = RunOutcome::StoppedOnFailure;
                            break;
                        }
                        continue;
                    }
                    Resolve::StillWaiting => {
                        outcome = RunOutcome::PausedAwaitingApproval;
                        outcomes.push(StepOutcome::plain(
                            step.id.clone(),
                            StepStatus::AwaitingApproval,
                            "等待审批".into(),
                            approval_id.map(str::to_string),
                        ));
                        break;
                    }
                    Resolve::Unknown(detail) => {
                        store.record_step_classified(
                            plan_id,
                            idx,
                            StepStatus::Failed,
                            &detail,
                            None,
                            Some(crate::FailureKind::NeedsHuman),
                            now,
                        )?;
                        outcomes.push(StepOutcome {
                            ambiguous: true,
                            ..StepOutcome::failed(
                                step.id.clone(),
                                detail,
                                crate::FailureKind::NeedsHuman,
                            )
                        });
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                    Resolve::Rejected(detail) => {
                        // Task 6: a rejected / expired / invalidated
                        // approval is needs-human, never a silent retry
                        // — the classifier reads it off the detail.
                        let kind = crate::failure::classify_failure(&detail, false);
                        store.record_step_classified(
                            plan_id,
                            idx,
                            StepStatus::Failed,
                            &detail,
                            None,
                            Some(kind),
                            now,
                        )?;
                        outcomes.push(StepOutcome::failed(step.id.clone(), detail, kind));
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
                // The origin scopes the approval's idempotency key to
                // the plan version (task 5): a resume at the same
                // version reuses the queued approval, but an edit that
                // bumps the version — and invalidates the old approval
                // — mints a fresh key instead of re-dispatching into
                // the invalidated approval's dead end (which
                // `settle_dispatched` would fail as "已随计划版本失效").
                let ctx = ToolContext {
                    turn_id: String::new(),
                    db: deps.db,
                    now: *now,
                    tz: deps.tz,
                    user_email: deps.user_email.clone(),
                    user_name: deps.user_name.clone(),
                    origin: format!("plan:{plan_id}:v{}", plan.version),
                    tainted: false,
                    call_id: call.id.clone(),
                    cancel: CancelToken::new(),
                    intent_review: None,
                };
                let result = dispatch_call(deps.registry, deps.executors, deps.gate, &call, &ctx);
                match settle_dispatched(deps.db, &result)? {
                    Settle::Done { output, receipt } => {
                        verification_store.executed(&crate::verification::Executed {
                            owner: plan_id,
                            version: plan.version as i64,
                            step: &step.id,
                            output: &output,
                            receipt: receipt.as_deref().unwrap_or(""),
                            approval: result.approval_id.as_deref().unwrap_or(""),
                            now,
                        })?;
                        let checked = crate::completion::finish(deps, &plan, step, idx, now)?;
                        let done = checked.status == StepStatus::Done;
                        outcomes.push(checked);
                        if !done {
                            outcome = RunOutcome::StoppedOnFailure;
                            break;
                        }
                    }
                    Settle::Queued { approval_id } => {
                        verification_store.waiting(
                            plan_id,
                            plan.version as i64,
                            &step.id,
                            &approval_id,
                            now,
                        )?;
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
                    Settle::Unknown { approval_id } => {
                        verification_store.waiting(
                            plan_id,
                            plan.version as i64,
                            &step.id,
                            &approval_id,
                            now,
                        )?;
                        let reason = "外部执行结果不明，需要人工对账；不会自动重复操作";
                        store.record_step_classified(
                            plan_id,
                            idx,
                            StepStatus::Failed,
                            reason,
                            Some(&approval_id),
                            Some(crate::FailureKind::NeedsHuman),
                            now,
                        )?;
                        outcomes.push(StepOutcome {
                            ambiguous: true,
                            ..StepOutcome::failed(
                                step.id.clone(),
                                reason.into(),
                                crate::FailureKind::NeedsHuman,
                            )
                        });
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                    Settle::Failed(error) => {
                        verification_store.rejected(plan_id, plan.version as i64, &step.id)?;
                        // Task 6: classify the tool error so the
                        // caller routes it (transient retries,
                        // permanent fails, needs-human blocks) instead
                        // of treating every failure the same.
                        let kind = crate::failure::classify_failure(&error, false);
                        store.record_step_classified(
                            plan_id,
                            idx,
                            StepStatus::Failed,
                            &error,
                            None,
                            Some(kind),
                            now,
                        )?;
                        outcomes.push(StepOutcome::failed(step.id.clone(), error, kind));
                        outcome = RunOutcome::StoppedOnFailure;
                        break;
                    }
                }
            }
        }
    }
    if outcome == RunOutcome::Completed {
        if let Some(failed) = crate::completion::finish_task(deps, &plan, now)? {
            outcomes.push(failed);
            outcome = RunOutcome::StoppedOnFailure;
        }
    }
    if outcome == RunOutcome::Completed {
        deps.db.transaction(|c| {
            crate::verification_store::guard(c, plan_id, plan.version as i64, deps.lease_fence.as_ref())?;
            let current = store.get(plan_id)?.unwrap();
            if current.steps.len() != plan.steps.len() || current.goal != plan.goal {
                return Err(hatch_core::Error::Invalid(
                    "完成前计划目标或步骤已变化".into(),
                ));
            }
            store.mark_completed(plan_id, now)
        })?;
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
    Done {
        result: String,
        receipt: String,
    },
    StillWaiting,
    Rejected(String),
    Unknown(String),
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
        // The action was attempted but its outcome is unknown: the
        // step waits for a human to reconcile it against the
        // supplier instead of failing as if it had not happened.
        ApprovalStatus::NeedsReview => Ok(Resolve::Unknown(format!(
            "审批 {id} 的外部执行结果不明，需要人工对账"
        ))),
        ApprovalStatus::Rejected => Ok(Resolve::Rejected(format!("审批 {id} 已被拒绝"))),
        ApprovalStatus::Expired => Ok(Resolve::Rejected(format!("审批 {id} 已过期"))),
        ApprovalStatus::Invalidated => Ok(Resolve::Rejected(format!("审批 {id} 已随计划版本失效"))),
        // A failed approval carries its executor's error in the
        // note: the step failure names the reason (e.g. a time-slot
        // conflict), so the failure classifier can route it to a
        // human instead of failing the task on a bare "执行失败".
        ApprovalStatus::Failed => Ok(Resolve::Rejected(format!(
            "审批 {id} 执行失败{}",
            if approval.note.is_empty() {
                String::new()
            } else {
                format!("：{}", approval.note)
            }
        ))),
    }
}

enum Settle {
    /// `receipt` is the ledger key when an approval executed the step.
    Done {
        output: String,
        receipt: Option<String>,
    },
    Queued {
        approval_id: String,
    },
    Failed(String),
    Unknown {
        approval_id: String,
    },
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
            // Attempted, outcome unknown: the step waits for human
            // reconciliation — it did not fail, and nothing retries it.
            ApprovalStatus::NeedsReview => Settle::Unknown {
                approval_id: id.clone(),
            },
            ApprovalStatus::Rejected => Settle::Failed(format!("审批 {id} 已被拒绝")),
            ApprovalStatus::Expired => Settle::Failed(format!("审批 {id} 已过期")),
            ApprovalStatus::Invalidated => Settle::Failed(format!("审批 {id} 已随计划版本失效")),
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
