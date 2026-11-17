//! Drills for the invite's calendar leg (acceptance chain): the
//! `create-event` plan step writes the event into the local calendar
//! under its own approval, exactly once, and a slot that filled up
//! after the preview refuses the create instead of double-booking.
//!
//! The runs use the meeting-brief fixture world with a custom
//! `RunInput` (`with_calendar: false`, `create_event: true`): mail
//! from its fixture mailbox, send to a `FileOutbox`, calendar local
//! — never a real account, never a real model call.

#![cfg(test)]

use super::task_invite::InviteSpec;
use super::task_worker::{run_worker_core, KillPoint, RunInput, WorkerOpts, WorkerReport};
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_connectors::{CalendarRepo, Event};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_eval::World;
use hatch_plan::{latest_plan_approval, PlanApproveExecutor, PlanStatus, PlanStore};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{BlockedKind, TaskStatus, TaskStore, UnitStore};
use hatch_tools::{execute_approved, CreateEventExecutor, ExecOutcome, ExecutorRegistry, SendEmailExecutor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

fn setup() -> (Arc<Db>, World, PathBuf, TempDir, Timestamp) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let dir = world_dir();
    let world = World::load(&dir).unwrap();
    let now = parse_rfc3339(&world.persona.now).unwrap();
    (db, world, dir, TempDir::new().unwrap(), now)
}

fn opts(task_id: Option<String>, now: Timestamp) -> WorkerOpts {
    WorkerOpts {
        task_id,
        kind: Some("meeting-invite".into()),
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill: KillPoint::None,
        router: None,
        app: None,
        max_model_calls: None,
    }
}

/// One invite run with the calendar leg on, targeting the free 13:00
/// slot; mail from the fixture mailbox, send via `FileOutbox`.
fn run_invite_with_calendar(
    db: &Arc<Db>,
    world: &World,
    dir: &Path,
    out: &TempDir,
    now: Timestamp,
    task_id: Option<String>,
) -> WorkerReport {
    let tz: chrono_tz::Tz = world.persona.timezone.parse().unwrap();
    let spec = InviteSpec::from_args(
        "项目评审会",
        "2026-11-04 13:00",
        "3 号会议室",
        "李婷",
        "li.ting@corp.example",
        "方案 B 通过；预算上限 5 万元",
        &world.persona.name,
        &now,
        &tz,
    )
    .unwrap();
    let mut source = FixtureMailbox::new(&world.persona.email, world.mailbox.clone());
    let sender = Arc::new(FileOutbox::new(out.path(), &world.persona.email).unwrap());
    let input = RunInput {
        title: None,
        prompt: None,
        invite_spec: Some(spec),
        invite_sender: Some(sender),
        with_calendar: false,
        create_event: true,
        calendar: "local".into(),
        source: "fixture",
    };
    run_worker_core(
        db,
        world,
        Some(dir),
        out.path(),
        opts(task_id, now),
        &mut source,
        input,
    )
    .unwrap()
}

/// Approve the task's plan (the `hatch plan approve` path, in-process).
fn approve_plan(db: &Db, plan_id: &str, now: Timestamp) {
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("计划应有待审批记录");
    let q = ApprovalQueue::new(db);
    let status = q
        .decide(&entry.id, true, "drill approve plan", &now)
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("drill")));
    let outcome = execute_approved(db, &exec, &entry.id, &now).unwrap();
    assert!(
        matches!(outcome, ExecOutcome::Executed(_)),
        "计划审批执行应成功，实际 {outcome:?}"
    );
}

/// Decide + execute one step's approval through the given registry.
/// Returns the approval id.
fn approve_step(
    db: &Db,
    task_id: &str,
    step_id: &str,
    exec: ExecutorRegistry,
    now: Timestamp,
) -> (String, ExecOutcome) {
    let units = UnitStore::new(db);
    let step = units.get_step(task_id, step_id).unwrap().unwrap();
    let id = step.approval_id.clone();
    assert!(!id.is_empty(), "{step_id} 步骤应已记录审批 id");
    let q = ApprovalQueue::new(db);
    let status = q.decide(&id, true, "drill approve step", &now).unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let outcome = execute_approved(db, &exec, &id, &now).unwrap();
    (id, outcome)
}

fn calendar_events(db: &Db) -> Vec<Event> {
    CalendarRepo::new(db).all().unwrap()
}

/// Events this leg created (stable `evt_…` ids) — as opposed to the
/// world's seeded ICS events (`cal-…`).
fn created_events(db: &Db) -> Vec<Event> {
    calendar_events(db)
        .into_iter()
        .filter(|e| e.uid.starts_with("evt_"))
        .collect()
}

fn outbox_mails(out: &TempDir) -> Vec<String> {
    std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".eml"))
        .collect()
}

/// The full chain with the calendar leg: plan → read-mail →
/// create-event → send. The event is created exactly once, its id is
/// the create-event step's output, and the task finishes 3/3.
#[test]
fn invite_create_event_runs_once_and_returns_its_id() {
    let (db, world, dir, out, now) = setup();
    let account = world.persona.email.clone();
    // Phase 1: the worker builds the plan and blocks on its approval.
    let first = run_invite_with_calendar(&db, &world, &dir, &out, now, None);
    assert!(first.blocked.is_some(), "第一轮应在计划审批上暂停");
    assert_eq!(first.steps_done, 0, "计划未批前不执行任何步骤");
    let store = TaskStore::new(&db);
    let task = store.get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.kind, "meeting-invite");
    let plan = PlanStore::new(&db).get(&task.plan_id).unwrap().unwrap();
    assert_eq!(
        plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["read-mail", "create-event", "send-invite"],
        "日历版邀请计划是读邮件 → 建日程 → 发邀请"
    );
    assert_eq!(
        calendar_events(&db).len(),
        3,
        "批准前日历只有世界自带的 3 个事件"
    );
    assert!(created_events(&db).is_empty(), "批准前不得建任何日程");

    // Phase 2: approve the plan; the worker reads the mail and blocks
    // on the create-event approval — no event, no mail yet.
    approve_plan(&db, &task.plan_id, now);
    let second = run_invite_with_calendar(
        &db,
        &world,
        &dir,
        &out,
        now,
        Some(first.task_id.clone()),
    );
    assert_eq!(second.steps_done, 1, "只有 read-mail 已完成");
    assert!(second.blocked.is_some(), "第二轮应在建日程审批上暂停");
    assert!(
        created_events(&db).is_empty(),
        "批准建日程前不得建日程"
    );
    assert!(outbox_mails(&out).is_empty(), "批准发送前不得有邮件");

    // Phase 3: approve + execute the create-event approval. Exactly
    // one event, with the fixture meeting's title, start and place.
    let exec = ExecutorRegistry::new().with(Arc::new(CreateEventExecutor));
    let (event_approval, outcome) = approve_step(
        &db,
        &first.task_id,
        "create-event",
        exec,
        now,
    );
    let created = match outcome {
        ExecOutcome::Executed(r) => r,
        other => panic!("建日程审批执行应成功，实际 {other:?}"),
    };
    assert!(
        created.starts_with("event evt_") && created.contains("项目评审会"),
        "建日程结果须带事件 ID 与标题，实际 {created}"
    );
    let events = created_events(&db);
    assert_eq!(events.len(), 1, "恰好创建一个日历事件");
    let event = &events[0];
    assert!(event.uid.starts_with("evt_"), "事件 ID 是 evt_…");
    assert!(created.contains(&event.uid), "步骤输出须带事件 ID");
    assert_eq!(event.title, "项目评审会");
    assert_eq!(
        event.starts_at,
        parse_rfc3339("2026-11-04T05:00:00Z").unwrap(),
        "13:00（上海）即 05:00Z"
    );
    assert_eq!(event.location, "3 号会议室");

    // Executing the same approval again is refused, and a resume does
    // not create a second event: the step is Done with the event id
    // in its output and a ledger receipt.
    let exec = ExecutorRegistry::new().with(Arc::new(CreateEventExecutor));
    assert!(
        execute_approved(&db, &exec, &event_approval, &now).is_err(),
        "已执行的建日程审批不得再执行"
    );
    let third = run_invite_with_calendar(
        &db,
        &world,
        &dir,
        &out,
        now,
        Some(first.task_id.clone()),
    );
    assert_eq!(third.steps_done, 2, "read-mail + create-event 已完成");
    assert!(third.blocked.is_some(), "第三轮应在发送审批上暂停");
    assert_eq!(created_events(&db).len(), 1, "续跑不得重复建日程");
    let units = UnitStore::new(&db);
    let step = units
        .get_step(&first.task_id, "create-event")
        .unwrap()
        .unwrap();
    assert_eq!(step.approval_id, event_approval);
    assert!(
        step.output.contains(&event.uid),
        "create-event 步骤输出须带事件 ID，实际 {}",
        step.output
    );
    assert!(
        !step.receipt_id.is_empty(),
        "create-event 步骤应记录回执（ledger key）"
    );

    // Phase 4: approve + execute the send; the task finishes 3/3
    // with exactly one event and exactly one mail.
    let outbox = Arc::new(FileOutbox::new(out.path(), &account).unwrap());
    let exec = ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(outbox, &account)));
    let (send_id, outcome) =
        approve_step(&db, &first.task_id, "send-invite", exec, now);
    assert!(
        matches!(outcome, ExecOutcome::Executed(_)),
        "发送审批执行应成功，实际 {outcome:?}"
    );
    let report = run_invite_with_calendar(
        &db,
        &world,
        &dir,
        &out,
        now,
        Some(first.task_id.clone()),
    );
    assert_eq!(report.steps_done, 3);
    assert!(report.blocked.is_none());
    let task = store.get(&report.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(created_events(&db).len(), 1, "全程恰好一个日历事件");
    assert_eq!(outbox_mails(&out).len(), 1, "恰好一封模拟发送的邮件");
    let step = units
        .get_step(&report.task_id, "send-invite")
        .unwrap()
        .unwrap();
    assert_eq!(step.approval_id, send_id);
    assert!(!step.receipt_id.is_empty(), "send-invite 步骤应记录回执");
}

/// A slot that filled up after the preview: approving the
/// create-event approval fails with the conflict reason, nothing is
/// created, no mail goes out, and the task blocks for a human with
/// the reason visible — it does not double-book and does not fail as
/// if the call were wrong.
#[test]
fn invite_create_event_conflict_blocks_for_a_human() {
    let (db, world, dir, out, now) = setup();
    // Reach the create-event approval.
    let first = run_invite_with_calendar(&db, &world, &dir, &out, now, None);
    let store = TaskStore::new(&db);
    let plan_id = store
        .get(&first.task_id)
        .unwrap()
        .unwrap()
        .plan_id;
    approve_plan(&db, &plan_id, now);
    let second = run_invite_with_calendar(
        &db,
        &world,
        &dir,
        &out,
        now,
        Some(first.task_id.clone()),
    );
    assert!(second.blocked.is_some(), "应在建日程审批上暂停");
    // The slot fills up after the preview: 13:30–14:30 overlaps the
    // 13:00–14:00 invite.
    CalendarRepo::new(&db)
        .upsert(&Event {
            uid: "other".into(),
            title: "已有会".into(),
            starts_at: parse_rfc3339("2026-11-04T05:30:00Z").unwrap(),
            ends_at: parse_rfc3339("2026-11-04T06:30:00Z").unwrap(),
            location: String::new(),
            source: "test".into(),
        })
        .unwrap();
    // Approving the create fails with the conflict reason.
    let exec = ExecutorRegistry::new().with(Arc::new(CreateEventExecutor));
    let (event_approval, outcome) = approve_step(&db, &first.task_id, "create-event", exec, now);
    let msg = match outcome {
        ExecOutcome::Failed(m) => m,
        other => panic!("冲突时段的建日程应执行失败，实际 {other:?}"),
    };
    assert!(
        msg.contains("时段冲突"),
        "失败原因须说明时段冲突，实际 {msg}"
    );
    assert!(
        msg.contains("已有会"),
        "失败原因须说出冲突的日程，实际 {msg}"
    );
    assert_eq!(
        ApprovalQueue::new(&db).get(&event_approval).unwrap().status,
        ApprovalStatus::Failed,
        "冲突的审批记为失败（定论），不是结果不明"
    );
    let events = calendar_events(&db);
    assert_eq!(
        events.len(),
        4,
        "冲突时不得建新事件（3 个自带 + 1 个已有会）"
    );
    assert!(created_events(&db).is_empty(), "冲突时不得建任何日程");
    assert!(
        events
            .iter()
            .any(|e| e.uid == "other" && e.title == "已有会"),
        "原有事件不得被覆盖"
    );
    assert!(outbox_mails(&out).is_empty(), "冲突时不得发任何邮件");
    // The worker run routes the failure to a human: the task blocks
    // with the conflict reason visible, the step carries needs_human.
    let third = run_invite_with_calendar(&db, &world, &dir, &out, now, Some(first.task_id.clone()));
    assert!(third.blocked.is_some(), "冲突后任务应在人工介入上暂停");
    let task = store.get(&first.task_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_kind, BlockedKind::MissingInfo);
    assert!(
        task.error.contains("时段冲突"),
        "任务阻塞原因须说明时段冲突，实际 {}",
        task.error
    );
    let units = UnitStore::new(&db);
    let step = units
        .get_step(&first.task_id, "create-event")
        .unwrap()
        .unwrap();
    assert_eq!(step.failure_kind, "needs_human", "时段冲突要人另选时间");
    assert!(
        created_events(&db).is_empty(),
        "阻塞后仍不得建新事件"
    );
}
