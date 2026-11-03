//! Helpers that put data where the workers read it — used by tests and the
//! scheduler evaluation world. Real data arrives through triage, calendar
//! import and the approval queue.

use chrono::Duration;
use hatch_connectors::{CalendarRepo, Event, MailRepo, ParsedMail};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval, Priority};
use hatch_triage::repo::TriageRepo;
use hatch_triage::{Category, TriageResult};

pub struct SeedMail<'a> {
    pub message_id: &'a str,
    pub from_addr: &'a str,
    pub from_name: &'a str,
    pub subject: &'a str,
    pub category: Category,
    pub priority: Priority,
    pub summary: &'a str,
    pub needs_reply: bool,
    pub suspicious: bool,
}

/// Store a mail and its triage result; returns the mail id.
pub fn mail(db: &hatch_store::Db, account: &str, m: &SeedMail, now: &Timestamp) -> Result<String> {
    let parsed = ParsedMail {
        uid: None,
        message_id: m.message_id.into(),
        from_addr: m.from_addr.into(),
        from_name: m.from_name.into(),
        to: vec![account.into()],
        subject: m.subject.into(),
        sent_at: Some(*now),
        body: m.summary.into(),
    };
    let (id, _) = MailRepo::new(db).store(account, &parsed, now)?;
    let r = TriageResult {
        mail_id: id.clone(),
        category: m.category,
        priority: m.priority,
        summary: m.summary.into(),
        needs_reply: m.needs_reply,
        reply_hint: String::new(),
        events: vec![],
        suspicious: m.suspicious,
        notes: vec![],
        model: "seed".into(),
    };
    TriageRepo::new(db).save(&r, now)?;
    Ok(id)
}

pub fn event(db: &hatch_store::Db, uid: &str, title: &str, start: Timestamp, minutes: i64, location: &str) -> Result<()> {
    CalendarRepo::new(db).upsert(&Event {
        uid: uid.into(),
        title: title.into(),
        starts_at: start,
        ends_at: start + Duration::minutes(minutes),
        location: location.into(),
        source: "seed".into(),
    })
}

/// A pending approval that does nothing if approved (`noop` has no executor).
pub fn approval(db: &hatch_store::Db, key: &str, title: &str, now: &Timestamp) -> Result<String> {
    let a = NewApproval {
        idempotency_key: key.into(),
        kind: "noop".into(),
        class: ActionClass::SendMessage,
        title: title.into(),
        preview: title.into(),
        payload: serde_json::json!({ "seed": key }),
        reasons: vec!["seed".into()],
        origin: "seed".into(),
        ttl: Duration::hours(72),
    };
    Ok(ApprovalQueue::new(db).submit(&a, now)?.id().to_string())
}
