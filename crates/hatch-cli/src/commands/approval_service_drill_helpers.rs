//! Shared fixtures for the P0 approval-unification drills
//! (`approval_service_drill`): an in-memory database + temp-home
//! app, approval/task factories, and the test-double senders. No
//! real account, no real mail.

#![cfg(test)]

use crate::app::App;
use chrono::Duration as ChronoDuration;
use hatch_connectors::{MailSender, OutgoingMail};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval, SubmitOutcome};
use hatch_store::{params, Db};
use hatch_tasks::{TaskApprovals, TaskStore};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tempfile::TempDir;

pub(crate) fn setup() -> (Arc<Db>, Arc<App>, TempDir, Timestamp) {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let home = TempDir::new().unwrap();
    let app = Arc::new(App::open_with_db(Some(home.path()), Arc::clone(&db)).unwrap());
    let now = parse_rfc3339("2026-11-03T09:00:00+08:00").unwrap();
    (db, app, home, now)
}

/// A `send_email` approval with a test payload. The recipient is a
/// reserved `.invalid` address, so even a stub that leaked would
/// have nowhere to send to.
pub(crate) fn email_approval(db: &Db, key: &str, now: &Timestamp) -> String {
    let q = ApprovalQueue::new(db);
    match q
        .submit(
            &NewApproval {
                idempotency_key: key.into(),
                kind: "send_email".into(),
                class: ActionClass::SendMessage,
                title: "drill mail".into(),
                preview: "drill mail".into(),
                payload: json!({"to": ["test@example.invalid"], "subject": "drill", "body": "drill"}),
                reasons: vec![],
                origin: "drill".into(),
                ttl: ChronoDuration::hours(72),
            },
            now,
        )
        .unwrap()
    {
        SubmitOutcome::Created(id) => id,
        SubmitOutcome::Existing(id, _) => panic!("approval key {key} already used ({id})"),
    }
}

pub(crate) fn make_task(db: &Db, kind: &str, source: &str, now: &Timestamp) -> String {
    let store = TaskStore::new(db);
    let task = store
        .create(kind, "drill task", "drill", "", 3, None, now)
        .unwrap();
    store.set_source(&task.id, source, now).unwrap();
    task.id
}

pub(crate) fn link(db: &Db, task_id: &str, approval_id: &str, now: &Timestamp) {
    let version = TaskStore::new(db)
        .get(task_id)
        .unwrap()
        .unwrap()
        .plan_version;
    TaskApprovals::new(db)
        .link(task_id, approval_id, version, "step-1", now)
        .unwrap();
}

pub(crate) fn ledger_status(db: &Db, key: &str) -> String {
    db.conn()
        .query_row(
            "SELECT status FROM action_ledger WHERE idempotency_key = ?1",
            params![key],
            |r| r.get(0),
        )
        .unwrap()
}

/// How many mails landed in the app's outbox. One `.eml` per send;
/// `outbox.jsonl` is the sender's index, not a mail, so it is not
/// counted.
pub(crate) fn outbox_files(app: &App) -> usize {
    std::fs::read_dir(app.outbox_dir())
        .map(|r| {
            r.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "eml"))
                .count()
        })
        .unwrap_or(0)
}

/// Counts sends, always succeeds.
pub(crate) struct CountingSender {
    pub(crate) sent: Arc<AtomicUsize>,
}

impl MailSender for CountingSender {
    fn send(&self, _mail: &OutgoingMail) -> Result<String> {
        let n = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("stub:{n}"))
    }
}

/// Counts sends, then refuses outright: a definite failure.
pub(crate) struct RejectingSender {
    pub(crate) sent: Arc<AtomicUsize>,
}

impl MailSender for RejectingSender {
    fn send(&self, _mail: &OutgoingMail) -> Result<String> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        Err(Error::Invalid("supplier refused: 550 rejected".into()))
    }
}

/// Counts sends, then reports the outcome as unknown: the mail may
/// already have gone out and the receipt was lost.
pub(crate) struct UnknownSender {
    pub(crate) sent: Arc<AtomicUsize>,
}

impl MailSender for UnknownSender {
    fn send(&self, _mail: &OutgoingMail) -> Result<String> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        Err(Error::UnknownOutcome("receipt lost after submit".into()))
    }
}

/// Blocks inside `send` until the test opens the gate, so the
/// concurrent-approve drill can stage the second attempt exactly
/// while the first is in flight.
pub(crate) struct BlockingSender {
    pub(crate) sent: Arc<AtomicUsize>,
    pub(crate) started: Arc<(Mutex<bool>, Condvar)>,
    pub(crate) gate: Arc<(Mutex<bool>, Condvar)>,
}

impl MailSender for BlockingSender {
    fn send(&self, _mail: &OutgoingMail) -> Result<String> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        {
            let (lock, cvar) = &*self.started;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        let (lock, cvar) = &*self.gate;
        let guard = lock.lock().unwrap();
        let _ = cvar
            .wait_timeout_while(guard, Duration::from_secs(10), |open| !*open)
            .unwrap();
        Ok("stub:blocked".into())
    }
}
