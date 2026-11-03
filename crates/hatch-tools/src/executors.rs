//! Executors run approved actions — and only approved actions.
//!
//! [`execute_approved`] is the one function that performs external effects. It
//! checks the approval status, verifies the payload hash (the user approved
//! *this* content), and goes through the idempotency ledger so a re-run, a
//! double click or a crash never sends the same mail twice.

use chrono::{DateTime, Utc};
use hatch_connectors::{CalendarRepo, Event, MailSender, OutgoingMail};
use hatch_core::time::{fmt_local, Timestamp};
use hatch_core::{CancelToken, Error, Result};
use hatch_policy::{ActionLedger, ApprovalQueue, ApprovalStatus, LedgerState};
use hatch_store::Db;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ExecContext<'a> {
    pub db: &'a Db,
    pub now: Timestamp,
    pub approval_id: &'a str,
    /// Where the action came from (`chat:<session>`, `triage:<run>`).
    pub origin: &'a str,
    /// Long-running executors (commands, fetches) stop when this fires.
    pub cancel: Option<&'a CancelToken>,
}

pub trait ActionExecutor: Send + Sync {
    fn kind(&self) -> &'static str;
    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String>;
}

#[derive(Default, Clone)]
pub struct ExecutorRegistry {
    map: HashMap<String, Arc<dyn ActionExecutor>>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, e: Arc<dyn ActionExecutor>) -> Self {
        self.map.insert(e.kind().to_string(), e);
        self
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn ActionExecutor>> {
        self.map.get(kind).cloned()
    }
}

// ------------------------------------------------------------------ email

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EmailPayload {
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
}

impl EmailPayload {
    pub fn preview(&self) -> String {
        let mut s = format!("收件人: {}\n主题: {}\n", self.to.join(", "), self.subject);
        if let Some(r) = &self.in_reply_to {
            s.push_str(&format!("回复: {r}\n"));
        }
        s.push_str("----------------------------------------\n");
        s.push_str(&self.body);
        s
    }
}

pub struct SendEmailExecutor {
    sender: Arc<dyn MailSender>,
    from: String,
}

impl SendEmailExecutor {
    pub fn new(sender: Arc<dyn MailSender>, from: &str) -> Self {
        SendEmailExecutor { sender, from: from.to_string() }
    }
}

impl ActionExecutor for SendEmailExecutor {
    fn kind(&self) -> &'static str {
        "send_email"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let p: EmailPayload = serde_json::from_value(payload.clone())?;
        if p.to.is_empty() {
            return Err(Error::Invalid("email has no recipients".into()));
        }
        if hatch_core::secrets::contains_secret(&format!("{} {}", p.subject, p.body)) {
            return Err(Error::SecretRejected("email body contains secret material".into()));
        }
        let id = self.sender.send(&OutgoingMail {
            from: self.from.clone(),
            to: p.to.clone(),
            subject: p.subject,
            body: p.body,
            in_reply_to: p.in_reply_to,
            references: p.references,
        })?;
        Ok(format!("sent {id} to {}", p.to.join(", ")))
    }
}

// --------------------------------------------------------------- calendar

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventPayload {
    pub uid: String,
    pub title: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[serde(default)]
    pub location: String,
}

impl EventPayload {
    pub fn preview(&self, tz: &chrono_tz::Tz) -> String {
        format!(
            "新建日程: {}\n时间: {} — {}\n地点: {}",
            self.title,
            fmt_local(&self.starts_at, tz),
            fmt_local(&self.ends_at, tz),
            if self.location.is_empty() { "(未指定)" } else { &self.location }
        )
    }
}

pub struct CreateEventExecutor;

impl ActionExecutor for CreateEventExecutor {
    fn kind(&self) -> &'static str {
        "create_event"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: EventPayload = serde_json::from_value(payload.clone())?;
        CalendarRepo::new(ctx.db).upsert(&Event {
            uid: p.uid.clone(),
            title: p.title.clone(),
            starts_at: p.starts_at,
            ends_at: p.ends_at,
            location: p.location,
            source: "hatch".into(),
        })?;
        Ok(format!("event {} created ({})", p.uid, p.title))
    }
}

// ------------------------------------------------------------- execution

#[derive(Clone, Debug, PartialEq)]
pub enum ExecOutcome {
    Executed(String),
    /// Ledger shows it already happened; nothing was repeated.
    AlreadyDone(String),
    /// A previous attempt crashed mid-flight; refusing to guess.
    Ambiguous(String),
    Failed(String),
}

/// Execute one approved action through the idempotency ledger.
pub fn execute_approved(db: &Db, reg: &ExecutorRegistry, approval_id: &str, now: &Timestamp) -> Result<ExecOutcome> {
    execute_approved_with(db, reg, approval_id, now, None)
}

/// [`execute_approved`] with a cancel token. Cancellation *before* the action
/// starts returns `Error::Cancelled` and leaves the approval approved (it can
/// be executed later); once started, the executor decides how to stop and
/// reports what happened.
pub fn execute_approved_with(
    db: &Db,
    reg: &ExecutorRegistry,
    approval_id: &str,
    now: &Timestamp,
    cancel: Option<&CancelToken>,
) -> Result<ExecOutcome> {
    if let Some(c) = cancel {
        c.check()?;
    }
    let q = ApprovalQueue::new(db);
    let a = q.get(approval_id)?;
    if a.status != ApprovalStatus::Approved {
        return Err(Error::Invalid(format!("approval {approval_id} is {}, not approved", a.status.as_str())));
    }
    if !a.payload_intact() {
        q.mark_failed(approval_id, "payload hash mismatch — content changed after review", now)?;
        return Ok(ExecOutcome::Failed("payload changed after review; not executed".into()));
    }
    let Some(exec) = reg.get(&a.kind) else {
        q.mark_failed(approval_id, "no executor configured", now)?;
        return Ok(ExecOutcome::Failed(format!("no executor for {}", a.kind)));
    };
    let ledger = ActionLedger::new(db);
    match ledger.begin(&a.idempotency_key, &a.kind, now)? {
        LedgerState::AlreadyDone(r) => {
            q.mark_executed(approval_id, &format!("already done earlier: {r}"), now)?;
            Ok(ExecOutcome::AlreadyDone(r))
        }
        LedgerState::InProgress => {
            let msg = "上一次执行在中途中断，无法确认是否已经生效；为避免重复发送，未重试，请人工核查";
            q.mark_failed(approval_id, msg, now)?;
            Ok(ExecOutcome::Ambiguous(msg.into()))
        }
        LedgerState::Fresh => match exec.execute(&a.payload, &ExecContext { db, now: *now, approval_id, origin: &a.origin, cancel }) {
            Ok(r) => {
                ledger.complete(&a.idempotency_key, &r, now)?;
                q.mark_executed(approval_id, &r, now)?;
                Ok(ExecOutcome::Executed(r))
            }
            Err(e) => {
                ledger.fail(&a.idempotency_key, &e.to_string(), now)?;
                q.mark_failed(approval_id, &e.to_string(), now)?;
                Ok(ExecOutcome::Failed(e.to_string()))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_connectors::mail::fixture::FileOutbox;
    use hatch_policy::{ActionClass, NewApproval};
    use serde_json::json;

    fn setup() -> (Db, tempfile::TempDir, Arc<FileOutbox>, ExecutorRegistry) {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ob = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
        let reg = ExecutorRegistry::new()
            .with(Arc::new(SendEmailExecutor::new(ob.clone(), "me@126.com")))
            .with(Arc::new(CreateEventExecutor));
        (db, dir, ob, reg)
    }

    fn approval(key: &str) -> NewApproval {
        let p = EmailPayload { to: vec!["a@b.com".into()], subject: "Re: x".into(), body: "好的".into(), in_reply_to: None, references: vec![] };
        NewApproval {
            idempotency_key: key.into(),
            kind: "send_email".into(),
            class: ActionClass::SendMessage,
            title: "reply".into(),
            preview: p.preview(),
            payload: serde_json::to_value(&p).unwrap(),
            reasons: vec![],
            origin: "test".into(),
            ttl: chrono::Duration::hours(1),
        }
    }

    #[test]
    fn executes_once_only() {
        let (db, _d, ob, reg) = setup();
        let q = ApprovalQueue::new(&db);
        let now = hatch_core::time::now();
        let id = q.submit(&approval("k"), &now).unwrap().id().to_string();
        assert!(execute_approved(&db, &reg, &id, &now).is_err(), "pending cannot execute");
        q.decide(&id, true, "", &now).unwrap();
        assert!(matches!(execute_approved(&db, &reg, &id, &now).unwrap(), ExecOutcome::Executed(_)));
        assert_eq!(ob.entries().unwrap().len(), 1);
        assert!(execute_approved(&db, &reg, &id, &now).is_err(), "executed cannot execute again");
        assert_eq!(ob.entries().unwrap().len(), 1);
    }

    #[test]
    fn crash_mid_flight_is_not_retried() {
        let (db, _d, ob, reg) = setup();
        let q = ApprovalQueue::new(&db);
        let now = hatch_core::time::now();
        let id = q.submit(&approval("k2"), &now).unwrap().id().to_string();
        q.decide(&id, true, "", &now).unwrap();
        // Simulate a crash after the ledger entry was opened.
        ActionLedger::new(&db).begin("k2", "send_email", &now).unwrap();
        assert!(matches!(execute_approved(&db, &reg, &id, &now).unwrap(), ExecOutcome::Ambiguous(_)));
        assert_eq!(ob.entries().unwrap().len(), 0);
        assert_eq!(q.get(&id).unwrap().status, ApprovalStatus::Failed);
    }

    #[test]
    fn tampered_payload_is_refused() {
        let (db, _d, ob, reg) = setup();
        let q = ApprovalQueue::new(&db);
        let now = hatch_core::time::now();
        let id = q.submit(&approval("k3"), &now).unwrap().id().to_string();
        q.decide(&id, true, "", &now).unwrap();
        db.conn()
            .execute("UPDATE approvals SET payload = ?1 WHERE id = ?2", hatch_store::params![json!({"to": ["x@evil.example"], "subject": "s", "body": "b"}).to_string(), id])
            .unwrap();
        assert!(matches!(execute_approved(&db, &reg, &id, &now).unwrap(), ExecOutcome::Failed(_)));
        assert_eq!(ob.entries().unwrap().len(), 0);
    }

    #[test]
    fn create_event_executor() {
        let (db, _d, _ob, reg) = setup();
        let now = hatch_core::time::now();
        let p = EventPayload { uid: "e1".into(), title: "评审".into(), starts_at: now, ends_at: now + chrono::Duration::hours(1), location: String::new() };
        let r = reg.get("create_event").unwrap().execute(&serde_json::to_value(&p).unwrap(), &ExecContext { db: &db, now, approval_id: "t", origin: "test", cancel: None }).unwrap();
        assert!(r.contains("e1"));
        assert_eq!(CalendarRepo::new(&db).between(&now, &(now + chrono::Duration::hours(2))).unwrap().len(), 1);
        assert!(p.preview(&chrono_tz::Asia::Shanghai).contains("(未指定)"));
    }
}
