//! The `create_event` executor: a different event overlapping the
//! slot refuses the create instead of double-booking, and a
//! re-execution of the same approved action is idempotent.
//!
//! These tests live here (not in `executors.rs`) to keep that file
//! under 400 lines: they exercise the executor only through the
//! public API (`execute_approved` + `ExecutorRegistry`).

use hatch_connectors::{CalendarRepo, Event};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, NewApproval};
use hatch_store::Db;
use hatch_tools::{
    execute_approved, CreateEventExecutor, EventPayload, ExecContext, ExecOutcome, ExecutorRegistry,
};
use std::sync::Arc;

fn setup() -> (Db, ExecutorRegistry) {
    let db = Db::open_in_memory().unwrap();
    let reg = ExecutorRegistry::new().with(Arc::new(CreateEventExecutor));
    (db, reg)
}

fn event_approval(key: &str, p: &EventPayload) -> NewApproval {
    NewApproval {
        idempotency_key: key.into(),
        kind: "create_event".into(),
        class: ActionClass::WriteCalendar,
        title: format!("新建日程：{}", p.title),
        preview: p.preview(&chrono_tz::Asia::Shanghai),
        payload: serde_json::to_value(p).unwrap(),
        reasons: vec![],
        origin: "test".into(),
        ttl: chrono::Duration::hours(72),
    }
}

/// A different event overlapping the slot refuses the create
/// instead of double-booking, and nothing is written.
#[test]
fn create_event_refuses_a_conflicting_slot() {
    let (db, reg) = setup();
    let now = hatch_core::time::now();
    let start = now + chrono::Duration::hours(1);
    let end = start + chrono::Duration::hours(1);
    CalendarRepo::new(&db)
        .upsert(&Event {
            uid: "other".into(),
            title: "已有会".into(),
            starts_at: start + chrono::Duration::minutes(30),
            ends_at: end + chrono::Duration::minutes(30),
            location: String::new(),
            source: "test".into(),
            attendees: vec![],
        })
        .unwrap();
    let p = EventPayload {
        uid: "e2".into(),
        title: "评审".into(),
        starts_at: start,
        ends_at: end,
        location: String::new(),
        attendees: vec![],
        calendar: "local".into(),
    };
    let q = ApprovalQueue::new(&db);
    let id = q
        .submit(&event_approval("ev-k1", &p), &now)
        .unwrap()
        .id()
        .to_string();
    q.decide(&id, true, "", &now).unwrap();
    let outcome = execute_approved(&db, &reg, &id, &now).unwrap();
    let msg = match outcome {
        ExecOutcome::Failed(m) => m,
        other => panic!("冲突时段应执行失败，实际 {other:?}"),
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
        q.get(&id).unwrap().status,
        ApprovalStatus::Failed,
        "冲突的审批记为失败（定论），不是结果不明"
    );
    let events = CalendarRepo::new(&db).between(&start, &end).unwrap();
    assert_eq!(events.len(), 1, "冲突时不得写入新事件");
    assert_eq!(events[0].uid, "other", "原有事件不得被覆盖");
}

/// The same uid re-executed is this event itself, not a
/// conflict — and a back-to-back event is not one either.
#[test]
fn create_event_same_uid_and_back_to_back_are_not_conflicts() {
    let (db, reg) = setup();
    let now = hatch_core::time::now();
    let start = now + chrono::Duration::hours(1);
    let end = start + chrono::Duration::hours(1);
    let ctx = ExecContext {
        db: &db,
        now,
        approval_id: "t",
        origin: "test",
        cancel: None,
    };
    let exec = reg.get("create_event").unwrap();
    let p = EventPayload {
        uid: "e3".into(),
        title: "评审".into(),
        starts_at: start,
        ends_at: end,
        location: String::new(),
        attendees: vec![],
        calendar: "local".into(),
    };
    exec.execute(&serde_json::to_value(&p).unwrap(), &ctx)
        .unwrap();
    // Same uid again: the upsert is idempotent, still one event.
    exec.execute(&serde_json::to_value(&p).unwrap(), &ctx)
        .unwrap();
    let back_to_back = EventPayload {
        uid: "e4".into(),
        title: "下一场".into(),
        starts_at: end,
        ends_at: end + chrono::Duration::hours(1),
        location: String::new(),
        attendees: vec![],
        calendar: "local".into(),
    };
    exec.execute(&serde_json::to_value(&back_to_back).unwrap(), &ctx)
        .unwrap();
    let events = CalendarRepo::new(&db)
        .between(&start, &(end + chrono::Duration::hours(1)))
        .unwrap();
    assert_eq!(events.len(), 2, "同一事件一次 + 紧邻事件一次");
    assert_eq!(events[0].uid, "e3");
    assert_eq!(events[1].uid, "e4");
}
