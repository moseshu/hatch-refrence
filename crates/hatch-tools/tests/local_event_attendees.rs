//! Local calendar events persist their attendee roster (schema
//! v25), and the executor's readback evidence names it — before
//! this, a local create silently dropped attendees, so cross-step
//! checks against "all attendees" could never be decided from
//! local evidence (the Google twin always carried them).

use hatch_core::time::now;
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
use hatch_store::Db;
use hatch_tools::{
    execute_approved, CreateEventExecutor, EventPayload, ExecutorRegistry,
};
use std::sync::Arc;

fn local_payload() -> EventPayload {
    let t = now();
    EventPayload {
        uid: "evt-local-1".into(),
        title: "项目评审会".into(),
        starts_at: t,
        ends_at: t + chrono::Duration::hours(1),
        location: "3 号会议室".into(),
        attendees: vec![
            "li.lei@corp.example".into(),
            "han.mei@partner.example".into(),
        ],
        calendar: "local".into(),
    }
}

#[test]
fn local_event_observe_names_the_attendee_roster() {
    let db = Db::open_in_memory().unwrap();
    let reg = ExecutorRegistry::new().with(Arc::new(CreateEventExecutor));
    let p = local_payload();
    let q = ApprovalQueue::new(&db);
    let approval = NewApproval {
        turn_id: String::new(),
        idempotency_key: "local-attendees-1".into(),
        kind: "create_event".into(),
        class: ActionClass::WriteCalendar,
        title: format!("新建日程：{}", p.title),
        preview: p.preview(&chrono_tz::Asia::Shanghai),
        payload: serde_json::to_value(&p).unwrap(),
        reasons: vec![],
        origin: "test".into(),
        ttl: chrono::Duration::hours(72),
    };
    let t = now();
    let id = q.submit(&approval, &t).unwrap().id().to_string();
    q.decide(&id, true, "", &t).unwrap();
    let outcome = execute_approved(&db, &reg, &id, &t).unwrap();
    let receipt = match outcome {
        hatch_tools::ExecOutcome::Executed(r) => r,
        other => panic!("expected Executed, got {other:?}"),
    };

    // The stored event carries the roster…
    let stored = hatch_connectors::CalendarRepo::new(&db)
        .get(&p.uid)
        .unwrap()
        .unwrap();
    assert_eq!(stored.attendees, p.attendees);

    // …and the executor's observed fields name it (L2/L3 evidence).
    let exec = CreateEventExecutor;
    let ctx = hatch_tools::ExecContext {
        db: &db,
        now: t,
        approval_id: &id,
        origin: "test",
        cancel: None,
    };
    let fields = hatch_tools::ActionExecutor::observe(
        &exec,
        &serde_json::to_value(&p).unwrap(),
        &receipt,
        &ctx,
    )
    .unwrap();
    assert_eq!(
        fields.get("event.attendees"),
        Some(&serde_json::json!(["han.mei@partner.example", "li.lei@corp.example"])),
        "observed attendees must be the stored roster (address-normalized)"
    );
}
