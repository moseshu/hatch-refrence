//! The `create_google_event` executor: the event goes to the real
//! Google Calendar (primary) instead of the local store, the Google
//! event id is the receipt, and a slot that filled up after the
//! preview refuses the create instead of double-booking.
//!
//! These tests live here (not in `executors.rs`) to keep that file
//! under 400 lines: they exercise the executor only through the
//! public API (`execute_approved` + `ExecutorRegistry`), with a
//! canned writer standing in for the helper — never a real calendar.

use hatch_connectors::{GoogleCalendarWriter, GoogleEvent, NewGoogleEvent};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
use hatch_store::Db;
use hatch_tools::{
    execute_approved, CreateGoogleEventExecutor, EventPayload, ExecOutcome, ExecutorRegistry,
};
use std::sync::{Arc, Mutex};

/// Canned Google writer: canned list/get answers, records inserts.
struct FakeGoogleWriter {
    list: Vec<GoogleEvent>,
    get: Option<GoogleEvent>,
    inserts: Mutex<Vec<NewGoogleEvent>>,
}

impl FakeGoogleWriter {
    fn new(list: Vec<GoogleEvent>, get: Option<GoogleEvent>) -> Self {
        FakeGoogleWriter {
            list,
            get,
            inserts: Mutex::new(vec![]),
        }
    }
}

impl GoogleCalendarWriter for FakeGoogleWriter {
    fn insert(&self, event: &NewGoogleEvent) -> Result<GoogleEvent> {
        self.inserts.lock().unwrap().push(event.clone());
        Ok(GoogleEvent {
            id: "google-ev-1".into(),
            title: event.title.clone(),
            start: event.start,
            end: event.end,
            location: event.location.clone(),
            attendees: event.attendees.clone(),
        })
    }

    fn get(&self, _event_id: &str) -> Result<GoogleEvent> {
        self.get
            .clone()
            .ok_or_else(|| hatch_core::Error::Connector("fake: no get answer".into()))
    }

    fn list(&self, _start: Timestamp, _end: Timestamp) -> Result<Vec<GoogleEvent>> {
        Ok(self.list.clone())
    }
}

fn setup(
    list: Vec<GoogleEvent>,
    get: Option<GoogleEvent>,
) -> (Db, ExecutorRegistry, Arc<FakeGoogleWriter>) {
    let db = Db::open_in_memory().unwrap();
    let writer = Arc::new(FakeGoogleWriter::new(list, get));
    let reg =
        ExecutorRegistry::new().with(Arc::new(CreateGoogleEventExecutor::new(writer.clone())));
    (db, reg, writer)
}

fn google_payload() -> EventPayload {
    let now = hatch_core::time::now();
    EventPayload {
        uid: "evt-test".into(),
        title: "评审".into(),
        starts_at: now,
        ends_at: now + chrono::Duration::hours(1),
        location: "3 号会议室".into(),
        attendees: vec!["li.ting@corp.example".into()],
        calendar: "google".into(),
    }
}

fn event_approval(key: &str, p: &EventPayload) -> NewApproval {
    NewApproval {
        idempotency_key: key.into(),
        kind: "create_google_event".into(),
        class: ActionClass::WriteCalendar,
        title: format!("新建日程：{}", p.title),
        preview: p.preview(&chrono_tz::Asia::Shanghai),
        payload: serde_json::to_value(p).unwrap(),
        reasons: vec![],
        origin: "test".into(),
        ttl: chrono::Duration::hours(72),
    }
}

/// Submit + approve + execute one Google create approval.
fn approve_and_execute(
    db: &Db,
    reg: &ExecutorRegistry,
    key: &str,
    p: &EventPayload,
) -> ExecOutcome {
    let now = hatch_core::time::now();
    let q = ApprovalQueue::new(db);
    let id = q
        .submit(&event_approval(key, p), &now)
        .unwrap()
        .id()
        .to_string();
    q.decide(&id, true, "", &now).unwrap();
    execute_approved(db, reg, &id, &now).unwrap()
}

/// The happy path: one Google write, the Google id is the receipt,
/// and the approval preview names the real calendar and the guest.
#[test]
fn create_google_event_writes_and_returns_the_google_id() {
    let p = google_payload();
    let matching = GoogleEvent {
        id: "google-ev-1".into(),
        title: p.title.clone(),
        start: p.starts_at,
        end: p.ends_at,
        location: p.location.clone(),
        attendees: p.attendees.clone(),
    };
    let (db, reg, writer) = setup(vec![], Some(matching));
    let outcome = approve_and_execute(&db, &reg, "g-k1", &p);
    let msg = match outcome {
        ExecOutcome::Executed(m) => m,
        other => panic!("Google 建日程应执行成功，实际 {other:?}"),
    };
    assert!(
        msg.starts_with("event google-ev-1 created (评审)"),
        "实际 {msg}"
    );
    let inserts = writer.inserts.lock().unwrap();
    assert_eq!(inserts.len(), 1);
    assert_eq!(
        inserts[0].attendees,
        vec!["li.ting@corp.example".to_string()]
    );
    // The Google preview names the real calendar and the guest.
    let preview = p.preview(&chrono_tz::Asia::Shanghai);
    assert!(preview.contains("Google 日历 primary"), "实际 {preview}");
    assert!(preview.contains("li.ting@corp.example"), "实际 {preview}");
    assert!(preview.contains("通知嘉宾"), "实际 {preview}");
}

/// A different event overlapping the slot refuses the create
/// instead of double-booking, and nothing is written.
#[test]
fn create_google_event_conflict_refuses() {
    let p = google_payload();
    let clash = GoogleEvent {
        id: "google-other".into(),
        title: "已有会".into(),
        start: p.starts_at + chrono::Duration::minutes(30),
        end: p.ends_at + chrono::Duration::minutes(30),
        location: String::new(),
        attendees: vec![],
    };
    let (db, reg, writer) = setup(vec![clash], None);
    let outcome = approve_and_execute(&db, &reg, "g-k2", &p);
    let msg = match outcome {
        ExecOutcome::Failed(m) => m,
        other => panic!("冲突时段应执行失败，实际 {other:?}"),
    };
    assert!(msg.contains("时段冲突"), "实际 {msg}");
    assert!(msg.contains("已有会"), "实际 {msg}");
    assert!(
        writer.inserts.lock().unwrap().is_empty(),
        "冲突时不得写 Google"
    );
}

/// The read-back disagrees with what was just created: a definite
/// failure that says to check, never to retry blindly.
#[test]
fn create_google_event_readback_mismatch_fails() {
    let p = google_payload();
    let other = GoogleEvent {
        id: "google-ev-2".into(),
        title: p.title.clone(),
        start: p.starts_at,
        end: p.ends_at,
        location: p.location.clone(),
        attendees: p.attendees.clone(),
    };
    let (db, reg, writer) = setup(vec![], Some(other));
    let outcome = approve_and_execute(&db, &reg, "g-k3", &p);
    let msg = match outcome {
        ExecOutcome::Failed(m) => m,
        other => panic!("读回不一致应执行失败，实际 {other:?}"),
    };
    assert!(msg.contains("不一致"), "实际 {msg}");
    assert_eq!(
        writer.inserts.lock().unwrap().len(),
        1,
        "insert 已发生，失败须说明核对"
    );
}

/// A local payload routed to the Google executor is refused, never
/// silently executed as a Google create.
#[test]
fn create_google_event_refuses_local_payload() {
    let mut p = google_payload();
    p.calendar = "local".into();
    let (db, reg, writer) = setup(vec![], None);
    let outcome = approve_and_execute(&db, &reg, "g-k4", &p);
    let msg = match outcome {
        ExecOutcome::Failed(m) => m,
        other => panic!("本地 payload 应执行失败，实际 {other:?}"),
    };
    assert!(msg.contains("只处理 Google"), "实际 {msg}");
    assert!(writer.inserts.lock().unwrap().is_empty());
}
