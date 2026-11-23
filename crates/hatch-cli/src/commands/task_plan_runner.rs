//! The generic Plan-and-Execute runner (P1, §6.1): a task whose plan
//! came from a submitted draft runs the existing plan executor
//! (`hatch-plan::run_plan`), with the same skeleton as the invite
//! runner — plan-approval gate first, then execute, then mirror the
//! outcomes into the task's steps.
//!
//! Differences from the invite runner, by design: the plan is not
//! built here (it arrived with the submission); the registry is the
//! default set plus file/web tools scoped to the task's output dir
//! (a plan step may read or write there); the executors are the
//! generic file + calendar (local and Google, as in the invite
//! runner) + mail ones (the mail sender is resolved
//! by the worker exactly as for the invite kind: a real-account
//! sender when one is injected, else a `FileOutbox` in the output
//! dir — so a `propose_email` step sends, and its sent-mail readback
//! feeds both the L1 receipt check and cross-step acceptance); and
//! there is no
//! plan-specific receipt verifier — a step is done when its tool
//! did not error, and the task-6 failure routing still applies on
//! a failed run. Desensitization at the model boundary is the
//! submit/chat layer's job; this runner records tool outputs as
//! they come back, in the user's own database.

use super::task_invite::route_stopped_on_failure;
use super::task_plan_mirror::{fail_task, mirror_done_steps, mirror_plan};
use super::task_worker::{is_cancelled, is_paused, RunnerOutcome};
use hatch_connectors::calendar::google::GoogleCalendar;
use hatch_connectors::MailSender;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result};
use hatch_eval::World;
use hatch_llm::Router;
use hatch_plan::acceptance::{AcceptanceItem, NlEvidence, NlItemVerdict};
use hatch_plan::{
    latest_plan_approval, pending_plan_approval, run_plan, PlanApproveExecutor, PlanStatus,
    RunDeps, RunOutcome, StepStatus,
};
use hatch_policy::{ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{BlockedKind, EventLog, Lease, StepSpec, TaskApprovals, TaskStore, UnitStore};
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{
    execute_approved, CreateEventExecutor, CreateGoogleEventExecutor, ExecutorRegistry,
    SendEmailExecutor, Workspace, WriteFileExecutor,
};
use std::path::Path;
use std::sync::Arc;

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_plan_task(
    db: &Arc<Db>,
    world: &World,
    out_dir: &Path,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    router: Option<&Router>,
    sender: Arc<dyn MailSender>,
) -> Result<RunnerOutcome> {
    let db_ref: &Db = db;
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);
    // The task's plan: it arrived with the submission. A plan task
    // without one is misfiled, not transient — fail it visibly.
    let task = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    if task.plan_id.is_empty() {
        let reason = "plan 任务没有计划，无法执行（提交时须附带计划草案）".to_string();
        fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
        return Err(Error::Invalid(reason));
    }
    let plan = match hatch_plan::PlanStore::new(db_ref).get(&task.plan_id)? {
        Some(p) => p,
        None => {
            let reason = format!("任务的计划 {} 找不到了", task.plan_id);
            fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
            return Err(Error::NotFound(reason));
        }
    };
    let steps: Vec<StepSpec> = plan
        .steps
        .iter()
        .map(|s| StepSpec {
            step_id: s.id.clone(),
            title: s.title.clone(),
            tool: s.tool.clone(),
            arguments: s.arguments.clone(),
            expected: s.expected.clone(),
        })
        .collect();
    units.ensure_steps(task_id, lease, &steps, now)?;
    let mut outcome = RunnerOutcome::default();

    if is_cancelled(&store, task_id)? {
        outcome.cancelled = true;
        return Ok(outcome);
    }
    if is_paused(&store, task_id)? {
        outcome.paused = true;
        return Ok(outcome);
    }

    match plan.status {
        PlanStatus::PendingApproval | PlanStatus::NeedsReapproval => {
            // The plan waits on its `plan_approve` entry — the same
            // gate as the invite: a decided-but-unexecuted approval
            // is executed here, then the run starts over.
            match pending_plan_approval(db_ref, &plan.id)? {
                Some(entry) => {
                    TaskApprovals::new(db_ref).link(
                        task_id,
                        &entry.id,
                        task.plan_version,
                        "plan",
                        now,
                    )?;
                    outcome.blocked = Some(format!(
                        "等待计划审批 {}：计划 {} 已提交，批准后重新运行任务继续",
                        entry.id, plan.id
                    ));
                    outcome.blocked_step = Some("plan".into());
                    return Ok(outcome);
                }
                None => match latest_plan_approval(db_ref, &plan)? {
                    Some(entry) if entry.status == ApprovalStatus::Approved => {
                        let exec = ExecutorRegistry::new()
                            .with(Arc::new(PlanApproveExecutor::new("worker")));
                        execute_approved(db_ref, &exec, &entry.id, now)?;
                        return run_plan_task(
                            db, world, out_dir, task_id, lease, now, tz, router, sender,
                        );
                    }
                    Some(entry) => {
                        let reason = format!(
                            "计划 {} 的审批已{}，无法执行",
                            plan.id,
                            entry.status.as_str()
                        );
                        fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
                        return Err(Error::Invalid(reason));
                    }
                    None => {
                        let reason = format!("计划 {} 没有审批记录，无法执行", plan.id);
                        fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
                        return Err(Error::Invalid(reason));
                    }
                },
            }
        }
        PlanStatus::Approved | PlanStatus::Executing | PlanStatus::Blocked => {
            // Runnable (a pause leaves the plan Approved/Executing).
        }
        PlanStatus::Completed => {
            // The plan finished but the task did not (a crash between
            // plan completion and task finish): mirror its outcomes.
            mirror_plan(db_ref, &plan, &units, task_id, lease, now)?;
            return Ok(outcome);
        }
        _ => {
            let reason = format!("计划 {} 处于 {}，无法执行", plan.id, plan.status.as_str());
            fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
            return Err(Error::Invalid(reason));
        }
    }

    // Execute (or resume) the approved plan.
    std::fs::create_dir_all(out_dir)
        .map_err(|e| Error::Invalid(format!("创建输出目录失败：{e}")))?;
    let ws = Workspace::new(out_dir)?;
    let registry = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(ws),
            web_fetch: true,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    let executors = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(CreateEventExecutor))
        .with(Arc::new(CreateGoogleEventExecutor::new(Arc::new(
            GoogleCalendar::new(hatch_connectors::gws_cli::cli_path()),
        ))))
        .with(Arc::new(SendEmailExecutor::new(
            sender,
            &world.persona.email,
        )));
    let gate = Gatekeeper::new(PolicyConfig::default());
    // L3 semantic grader (Observe): the isolated grader over the
    // shared evidence bundle, only when a real router is present
    // (`--live`). Scripted runs record no L3 checks, as before.
    let cancel = CancelToken::new();
    let grade_usage = std::cell::Cell::new((0u64, 0u64, 0u64));
    let grader = router.map(|r| {
        let cancel = &cancel;
        let grade_usage = &grade_usage;
        move |goal: &str,
              items: &[AcceptanceItem],
              evidence: &[NlEvidence],
              candidate: &str|
              -> Result<Vec<NlItemVerdict>> {
            let rubric: Vec<hatch_agent::independent_grader::RubricItem> = items
                .iter()
                .filter_map(|i| match i {
                    AcceptanceItem::Nl { id, expected } => {
                        Some(hatch_agent::independent_grader::RubricItem {
                            id: id.clone(),
                            expected: expected.clone(),
                        })
                    }
                    AcceptanceItem::Predicate { .. } => None,
                })
                .collect();
            let ev: Vec<hatch_agent::independent_grader::GraderEvidence> = evidence
                .iter()
                .map(|e| hatch_agent::independent_grader::GraderEvidence {
                    id: e.id.clone(),
                    source: e.source.clone(),
                    value: e.value.clone(),
                })
                .collect();
            let (grade, usage) =
                hatch_agent::independent_grader::grade(r, goal, &rubric, &ev, candidate, cancel)?;
            let (calls, prompt, completion) = grade_usage.get();
            grade_usage.set((
                calls + 1,
                prompt + usage.prompt_tokens,
                completion + usage.completion_tokens,
            ));
            Ok(grade
                .items
                .into_iter()
                .map(|g| NlItemVerdict {
                    id: g.id,
                    verdict: g.verdict,
                    evidence: g.evidence,
                    reason: g.reason,
                })
                .collect())
        }
    });
    let deps = RunDeps {
        db: db_ref,
        registry: &registry,
        executors: &executors,
        gate: &gate,
        tz: *tz,
        user_email: world.persona.email.clone(),
        workspace_root: Some(out_dir.to_path_buf()),
        snapshot_root: None,
        verify: None,
        output_map: None,
        user_name: world.persona.name.clone(),
        lease_fence: Some(hatch_plan::verification::LeaseFence {
            owner: lease.owner.clone(),
            generation: lease.generation as i64,
        }),
        grade: grader.as_ref().map(|g| {
            g as &dyn Fn(&str, &[AcceptanceItem], &[NlEvidence], &str) -> Result<Vec<NlItemVerdict>>
        }),
        acceptance_enforce: hatch_plan::AcceptanceEnforce::configured()?,
    };
    let _ = log.append_event(task_id, lease, "plan_started", "", "计划开始执行", now);
    let report = run_plan(&deps, &plan.id, now)?;
    let (grade_calls, grade_prompt, grade_completion) = grade_usage.get();
    outcome.model_calls += grade_calls;
    outcome.prompt_tokens += grade_prompt;
    outcome.completion_tokens += grade_completion;
    match report.outcome {
        RunOutcome::Completed => {
            mirror_done_steps(&report, &units, task_id, lease, now)?;
            Ok(outcome)
        }
        RunOutcome::PausedAwaitingApproval => {
            mirror_done_steps(&report, &units, task_id, lease, now)?;
            let waiting = report
                .steps
                .iter()
                .find(|s| s.status == StepStatus::AwaitingApproval)
                .ok_or_else(|| {
                    Error::Invalid("计划暂停等待审批，但报告里没有等待中的步骤".into())
                })?;
            let approval_id = waiting.approval_id.clone().ok_or_else(|| {
                Error::Invalid(format!(
                    "步骤 {} 等待审批，但没有记录 approval id",
                    waiting.step_id
                ))
            })?;
            let task = store
                .get(task_id)?
                .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
            units.set_step_approval(task_id, lease, &waiting.step_id, &approval_id, now)?;
            TaskApprovals::new(db_ref).link(
                task_id,
                &approval_id,
                task.plan_version,
                &waiting.step_id,
                now,
            )?;
            outcome.blocked = Some(format!(
                "等待审批 {approval_id}：{} 需人工批准，批准后重新运行任务继续",
                waiting.step_id
            ));
            outcome.blocked_kind = Some(BlockedKind::Approval);
            outcome.blocked_step = Some(waiting.step_id.clone());
            Ok(outcome)
        }
        RunOutcome::StoppedOnFailure => {
            route_stopped_on_failure(db_ref, task_id, lease, &report, tz, now)
        }
        RunOutcome::StoppedOnDeviation => {
            let reason = format!(
                "计划 {} 已偏离批准快照：{}",
                plan.id,
                report.deviation.as_deref().unwrap_or("步骤与批准时不一致")
            );
            fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
            Err(Error::Invalid(reason))
        }
    }
}
