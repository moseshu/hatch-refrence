//! The meeting-invite runner (M1, task 4): a plan-first task.
//!
//! The worker builds a three-step plan from the world — read the
//! calendar, read the invitee's mail, send the invite — submits it
//! for plan approval, and executes it with `hatch-plan::run_plan`.
//! The send step goes through the gate (`propose_email` → a
//! `send_email` approval); the worker blocks the task on each
//! approval and resumes from the breakpoint. Sends are simulated
//! only: the `FileOutbox` writes a mail file in the output dir,
//! never a real SMTP send. The invite body is assembled
//! deterministically from the world (no model-drafted body in M1).

use super::task_worker::is_cancelled;
use super::task_worker::is_paused;
use super::task_worker::RunnerOutcome;
use chrono::TimeZone;
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_connectors::{MailRepo, MailSource};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_eval::World;
use hatch_plan::{
    latest_plan_approval, pending_plan_approval, run_plan, Plan, PlanApproveExecutor, PlanStatus,
    PlanStep, RiskLevel, RunDeps, RunOutcome, StepStatus, StepVerify,
};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, Lease, StepSpec, TaskApprovals, TaskStore, UnitStore};
use hatch_tools::builtin::default_registry;
use hatch_tools::{execute_approved, CreateEventExecutor, ExecutorRegistry, SendEmailExecutor};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// Everything the plan and its receipt verifier need from the world.
struct InviteSpec {
    event_title: String,
    /// "2026-11-04 10:00" (world timezone).
    event_start: String,
    location: String,
    attendee_name: String,
    attendee_email: String,
    subject: String,
    body: String,
    /// "2026-11-04 09:30" — the calendar query's `from`.
    cal_from: String,
}

impl InviteSpec {
    /// `world_now` is the fixture clock: the calendar query's `from`
    /// ("tomorrow", relative to the world's own date) is anchored to
    /// it, not to the runtime clock.
    fn from_world(world: &World, world_now: &Timestamp, tz: &chrono_tz::Tz) -> Result<Self> {
        let brief = world
            .expectations
            .brief
            .as_ref()
            .ok_or_else(|| Error::Invalid("此世界没有会议简报期望，无法组装邀请".into()))?;
        let event_title = brief.event_title.clone();
        let attendee_email = brief.attendee_email.clone();
        let attendee_name = world
            .persona
            .contacts
            .iter()
            .find(|c| c.email == attendee_email)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "李婷".into());
        let start = parse_rfc3339(&brief.event_start)?;
        let start_local = tz.from_utc_datetime(&start.naive_utc());
        let event_start = start_local.format("%Y-%m-%d %H:%M").to_string();
        let location = event_location(&world.calendar_ics, &event_title).unwrap_or_default();
        let decisions = brief
            .decisions
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
            .join("；");
        let body = format!(
            "{attendee_name}你好：\n\n邀请你参加明天的{event_title}。\n时间：{event_start}\n地点：{location}\n议题：{decisions}。\n\n请准时参加。\n\n{}",
            world.persona.name
        );
        let subject = format!("邀请：{event_title}（{event_start}）");
        let from_local =
            tz.from_utc_datetime(&(*world_now + chrono::Duration::days(1)).naive_utc());
        Ok(InviteSpec {
            event_title,
            event_start,
            location,
            attendee_name,
            attendee_email,
            subject,
            body,
            cal_from: from_local.format("%Y-%m-%d %H:%M").to_string(),
        })
    }
}

/// Pull the world's fixture mailbox into the mail repo (cursor-based,
/// so a second call is a no-op). The triage workflow does the same
/// sync for the brief kind; the invite kind does it itself because it
/// has no triage step.
fn sync_mailbox(db: &Db, world: &World, now: &Timestamp) -> Result<()> {
    let account = world.persona.email.clone();
    let repo = MailRepo::new(db);
    let cursor = repo.cursor(&account)?;
    let mut source = FixtureMailbox::new(&account, world.mailbox.clone());
    let mails = source.fetch_new(cursor, 100)?;
    for m in &mails {
        repo.store(&account, m, now)?;
        if let Some(uid) = m.uid {
            repo.set_cursor(&account, uid)?;
        }
    }
    Ok(())
}

/// The `LOCATION:` of the VEVENT whose `SUMMARY:` matches the title
/// (empty when the world gives none — the body then omits it).
fn event_location(ics: &str, title: &str) -> Option<String> {
    let mut in_match = false;
    let mut location = None;
    for line in ics.lines() {
        let line = line.trim_end();
        if line == "BEGIN:VEVENT" {
            in_match = false;
            location = None;
        } else if line == "END:VEVENT" {
            if in_match {
                return location;
            }
        } else if let Some(summary) = line.strip_prefix("SUMMARY:") {
            in_match = summary == title;
        } else if in_match {
            if let Some(loc) = line.strip_prefix("LOCATION:") {
                location = Some(loc.to_string());
            }
        }
    }
    None
}

/// The invite runner: the invite makes no model calls, so it never
/// stops on a budget and `--live` changes nothing for it.
/// `world_now` is the fixture clock (see `run_brief`).
/// The invite runner takes its inputs as separate parameters; the
/// eight are the worker's context, not a grab-bag (the worker owns the
/// lease and the clocks, the spec owns the world-derived values).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_invite(
    db: &Arc<Db>,
    world: &World,
    out_dir: &Path,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
    world_now: &Timestamp,
    tz: &chrono_tz::Tz,
) -> Result<RunnerOutcome> {
    let db_ref: &Db = db;
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);
    let spec = InviteSpec::from_world(world, world_now, tz)?;
    let steps: Vec<StepSpec> = plan_steps(&spec)
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
    // A user pause stops the run before the plan is touched
    // (task 5); the task is already Blocked, so no transition is
    // needed here.
    if is_paused(&store, task_id)? {
        outcome.paused = true;
        return Ok(outcome);
    }
    // The invite has no triage step, so nothing else syncs the
    // mailbox: pull the fixture mails into the mail repo first (the
    // same cursor-based sync the triage uses, so a resume re-syncs
    // nothing). `seed_world` only seeds the calendar.
    sync_mailbox(db_ref, world, now)?;

    // The task's plan: create + submit it on the first run, then
    // follow its status on every resume.
    let task = store
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    let plan_id = if task.plan_id.is_empty() {
        let plan_text = task.title.clone();
        let plan = hatch_plan::PlanStore::new(db_ref).create(
            &plan_text,
            "",
            "",
            plan_steps(&spec),
            false,
            now,
        )?;
        store.set_plan_id(task_id, &plan.id, now)?;
        let _ = log.append_event(
            task_id,
            lease,
            "plan_created",
            "",
            &format!("会议邀请计划 {} 已提交审批", plan.id),
            now,
        );
        plan.id
    } else {
        task.plan_id.clone()
    };

    // A missing plan row is not transient: no retry can bring it back,
    // so fail the task with the reason (like the other terminal plan
    // states below) instead of leaving it running to be retried forever.
    let plan = match hatch_plan::PlanStore::new(db_ref).get(&plan_id)? {
        Some(p) => p,
        None => {
            let reason = format!("任务的计划 {plan_id} 找不到了");
            fail_task(&store, task_id, lease, &reason, now)?;
            return Err(Error::NotFound(reason));
        }
    };
    match plan.status {
        PlanStatus::PendingApproval | PlanStatus::NeedsReapproval => {
            // The plan waits on its `plan_approve` entry. An entry
            // that was decided Approved but never executed (a crash
            // between decide and execute) is executed here — the
            // executor only flips the plan status, so re-running it
            // is the same action the human command would do.
            match pending_plan_approval(db_ref, &plan.id)? {
                Some(entry) => {
                    outcome.blocked = Some(format!(
                        "等待计划审批 {}：会议邀请计划 {} 已提交，批准后重新运行任务继续",
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
                        return run_invite(db, world, out_dir, task_id, lease, now, world_now, tz);
                    }
                    Some(entry) => {
                        let reason = format!(
                            "会议邀请计划 {} 的审批已{}，无法执行",
                            plan.id,
                            entry.status.as_str()
                        );
                        fail_task(&store, task_id, lease, &reason, now)?;
                        return Err(Error::Invalid(reason));
                    }
                    None => {
                        let reason = format!("会议邀请计划 {} 没有审批记录，无法执行", plan.id);
                        fail_task(&store, task_id, lease, &reason, now)?;
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
            let reason = format!(
                "会议邀请计划 {} 处于 {}，无法执行",
                plan.id,
                plan.status.as_str()
            );
            fail_task(&store, task_id, lease, &reason, now)?;
            return Err(Error::Invalid(reason));
        }
    }

    // Execute (or resume) the approved plan.
    std::fs::create_dir_all(out_dir)
        .map_err(|e| Error::Invalid(format!("创建输出目录失败：{e}")))?;
    let registry = default_registry();
    let outbox = Arc::new(FileOutbox::new(out_dir, &world.persona.email)?);
    let executors = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(
            outbox.clone(),
            &world.persona.email,
        )))
        .with(Arc::new(CreateEventExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    // The tools answer in code space (the model only ever saw codes):
    // map step outputs back to real names before they are verified
    // and recorded, so the receipt check, the plan row and the plan
    // card all show real names. The mapping is built from the same
    // database the tools read, so the codes agree.
    let mapper =
        hatch_triage::desensitize::mapper_for(db_ref, &world.persona.email, &world.persona.name)?;
    let map_output = |output: &str| mapper.reverse(output);
    let verify = |step: &PlanStep, output: &str| verify_invite_step(step, output, &spec);
    let deps = RunDeps {
        db: db_ref,
        registry: &registry,
        executors: &executors,
        gate: &gate,
        tz: *tz,
        user_email: world.persona.email.clone(),
        workspace_root: Some(out_dir.to_path_buf()),
        snapshot_root: None,
        verify: Some(&verify),
        output_map: Some(&map_output),
        user_name: world.persona.name.clone(),
    };
    let report = run_plan(&deps, &plan.id, now)?;
    match report.outcome {
        RunOutcome::Completed => {
            // The report carries the receipts (an executed approval's
            // ledger key); the plan rows do not, so mirror from the
            // report, not from a re-read plan.
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
            outcome.blocked_step = Some(waiting.step_id.clone());
            Ok(outcome)
        }
        RunOutcome::StoppedOnFailure => {
            let failed = report.steps.iter().find(|s| s.status == StepStatus::Failed);
            let reason = match failed {
                Some(f) => {
                    let _ = units.complete_step(
                        task_id,
                        lease,
                        &f.step_id,
                        ExecStatus::Failed,
                        &f.output,
                        now,
                    );
                    if f.ambiguous {
                        format!(
                            "步骤 {} 失败且外部状态不明（{}），请人工核查，不要自动重试",
                            f.step_id, f.output
                        )
                    } else {
                        format!("步骤 {} 失败：{}", f.step_id, f.output)
                    }
                }
                None => "计划执行失败（报告里没有失败步骤）".to_string(),
            };
            fail_task(&store, task_id, lease, &reason, now)?;
            Err(Error::Invalid(reason))
        }
        RunOutcome::StoppedOnDeviation => {
            let reason = format!(
                "计划 {} 已偏离批准快照：{}",
                plan.id,
                report.deviation.as_deref().unwrap_or("步骤与批准时不一致")
            );
            fail_task(&store, task_id, lease, &reason, now)?;
            Err(Error::Invalid(reason))
        }
    }
}

/// The three plan steps for the invite.
fn plan_steps(spec: &InviteSpec) -> Vec<PlanStep> {
    vec![
        PlanStep {
            id: "read-calendar".into(),
            title: "查明天会议".into(),
            tool: "calendar_list".into(),
            arguments: json!({"from": spec.cal_from, "days": 1}),
            files: vec![],
            expected: format!(
                "日程里有明天的{}（{}，{}）",
                spec.event_title, spec.event_start, spec.location
            ),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        },
        PlanStep {
            id: "read-mail".into(),
            title: "查李婷的邮件".into(),
            tool: "mail_search".into(),
            arguments: json!({"query": spec.attendee_name, "limit": 10}),
            files: vec![],
            expected: format!(
                "找到{}的工作邮箱 {} 的邮件（不是同名个人邮箱）",
                spec.attendee_name, spec.attendee_email
            ),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        },
        PlanStep {
            id: "send-invite".into(),
            title: "发会议邀请".into(),
            tool: "propose_email".into(),
            arguments: json!({
                "to": [spec.attendee_email],
                "subject": spec.subject,
                "body": spec.body,
            }),
            files: vec![],
            expected: format!(
                "邀请邮件已发送给 {}（模拟发送，见输出目录的 outbox）",
                spec.attendee_email
            ),
            risk: RiskLevel::Medium,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        },
    ]
}

/// Per-step receipt check: the delivery must match `expected`, not
/// just "the tool did not error".
fn verify_invite_step(step: &PlanStep, output: &str, spec: &InviteSpec) -> StepVerify {
    match step.id.as_str() {
        "read-calendar" => {
            if output.contains(&spec.event_title) && output.contains("10:00") {
                StepVerify::Pass(None)
            } else {
                StepVerify::Fail(format!(
                    "日程里没有找到{}（10:00），实际返回：{}",
                    spec.event_title,
                    truncate(output)
                ))
            }
        }
        "read-mail" => {
            if output.contains(&spec.attendee_email) {
                StepVerify::Pass(None)
            } else {
                StepVerify::Fail(format!(
                    "邮件搜索没有返回{}的邮件，实际返回：{}",
                    spec.attendee_email,
                    truncate(output)
                ))
            }
        }
        "send-invite" => {
            if output.starts_with("sent ") && output.contains(&spec.attendee_email) {
                StepVerify::Pass(None)
            } else {
                StepVerify::Fail(format!("发送结果不是一次成功的发送：{}", truncate(output)))
            }
        }
        _ => StepVerify::Pass(None),
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Mirror a finished plan's step outcomes into the task's steps
/// (idempotent: a step already Done in the task is never re-completed,
/// so a resume cannot duplicate its `step_done` event). The receipt
/// is recovered from the approval queue: the ledger key of the step's
/// executed approval (the plan row itself stores no receipt).
fn mirror_plan(
    db: &Db,
    plan: &Plan,
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
) -> Result<()> {
    for step in &plan.steps {
        if step.status != StepStatus::Done {
            continue;
        }
        let receipt = step.approval_id.as_deref().and_then(|id| {
            ApprovalQueue::new(db)
                .get(id)
                .ok()
                .filter(|a| a.status == ApprovalStatus::Executed)
                .map(|a| a.idempotency_key)
        });
        mirror_step(
            units,
            task_id,
            lease,
            &step.id,
            &step.output,
            receipt.as_deref(),
            now,
        )?;
    }
    Ok(())
}

/// Mirror the steps a paused run already completed.
fn mirror_done_steps(
    report: &hatch_plan::RunReport,
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
) -> Result<()> {
    for step in &report.steps {
        if step.status != StepStatus::Done {
            continue;
        }
        mirror_step(
            units,
            task_id,
            lease,
            &step.step_id,
            &step.output,
            step.receipt.as_deref(),
            now,
        )?;
    }
    Ok(())
}

fn mirror_step(
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    step_id: &str,
    output: &str,
    receipt: Option<&str>,
    now: &Timestamp,
) -> Result<()> {
    let Some(step) = units.get_step(task_id, step_id)? else {
        return Ok(());
    };
    if step.status == ExecStatus::Done {
        return Ok(());
    }
    units.start_step(task_id, lease, step_id, now)?;
    units.complete_step(task_id, lease, step_id, ExecStatus::Done, output, now)?;
    if let Some(receipt) = receipt {
        let _ = units.set_step_receipt(task_id, lease, step_id, receipt, now);
    }
    Ok(())
}

/// Fail the task with a note (a control-plane transition the runner
/// owns when the plan cannot proceed).
fn fail_task(
    store: &TaskStore,
    task_id: &str,
    lease: &Lease,
    reason: &str,
    now: &Timestamp,
) -> Result<()> {
    let _ = store.fail(task_id, Some(lease), reason, now);
    Ok(())
}
