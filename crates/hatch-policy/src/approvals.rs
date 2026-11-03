//! Asynchronous approval queue.
//!
//! Approvals are events, not blocking prompts: an action that needs consent is
//! written here and the workflow/agent carries on. The user decides later
//! (`hatch approvals approve <id>`), and an executor runs the *stored* payload —
//! exactly what the user saw. Editing means rejecting and creating a new
//! approval. Expired approvals never execute.

use crate::action::ActionClass;
use chrono::Duration;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Executed,
    Failed,
}

impl ApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Executed => "executed",
            ApprovalStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Result<ApprovalStatus> {
        [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Expired,
            ApprovalStatus::Executed,
            ApprovalStatus::Failed,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
        .ok_or_else(|| Error::Invalid(format!("unknown approval status {s}")))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Approval {
    pub id: String,
    pub idempotency_key: String,
    /// Executor kind, e.g. `send_email`, `create_event`.
    pub kind: String,
    pub class: ActionClass,
    pub title: String,
    /// Human-readable rendering of exactly what will happen.
    pub preview: String,
    pub payload: Value,
    pub payload_hash: String,
    pub reasons: Vec<String>,
    pub status: ApprovalStatus,
    /// Who asked: `triage:<run>`, `chat:<session>`.
    pub origin: String,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub decided_at: Option<Timestamp>,
    pub note: String,
}

impl Approval {
    /// The stored payload still matches the hash recorded at submission.
    pub fn payload_intact(&self) -> bool {
        hash_payload(&self.payload) == self.payload_hash
    }
}

#[derive(Clone, Debug)]
pub struct NewApproval {
    pub idempotency_key: String,
    pub kind: String,
    pub class: ActionClass,
    pub title: String,
    pub preview: String,
    pub payload: Value,
    pub reasons: Vec<String>,
    pub origin: String,
    pub ttl: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Created(String),
    /// Same idempotency key already queued (re-run of a workflow).
    Existing(String, ApprovalStatus),
}

impl SubmitOutcome {
    pub fn id(&self) -> &str {
        match self {
            SubmitOutcome::Created(id) | SubmitOutcome::Existing(id, _) => id,
        }
    }
}

pub fn hash_payload(v: &Value) -> String {
    ids::digest(&[&v.to_string()])
}

const COLS: &str = "id, idempotency_key, kind, action_class, title, preview, payload, payload_hash, reasons, status, origin, created_at, expires_at, decided_at, note";

fn row_to_approval(r: &rusqlite::Row) -> rusqlite::Result<Approval> {
    let class: String = r.get(3)?;
    let payload: String = r.get(6)?;
    let reasons: String = r.get(8)?;
    let status: String = r.get(9)?;
    let created: String = r.get(11)?;
    let expires: String = r.get(12)?;
    let decided: Option<String> = r.get(13)?;
    Ok(Approval {
        id: r.get(0)?,
        idempotency_key: r.get(1)?,
        kind: r.get(2)?,
        class: ActionClass::parse(&class).unwrap_or(ActionClass::Payment),
        title: r.get(4)?,
        preview: r.get(5)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        payload_hash: r.get(7)?,
        reasons: serde_json::from_str(&reasons).unwrap_or_default(),
        status: ApprovalStatus::parse(&status).unwrap_or(ApprovalStatus::Failed),
        origin: r.get(10)?,
        created_at: parse_rfc3339(&created).unwrap_or_default(),
        expires_at: parse_rfc3339(&expires).unwrap_or_default(),
        decided_at: decided.and_then(|d| parse_rfc3339(&d).ok()),
        note: r.get(14)?,
    })
}

pub struct ApprovalQueue<'a> {
    db: &'a Db,
}

impl<'a> ApprovalQueue<'a> {
    pub fn new(db: &'a Db) -> Self {
        ApprovalQueue { db }
    }

    pub fn submit(&self, a: &NewApproval, now: &Timestamp) -> Result<SubmitOutcome> {
        if let Some(existing) = self.by_key(&a.idempotency_key)? {
            return Ok(SubmitOutcome::Existing(existing.id, existing.status));
        }
        let preview = hatch_core::secrets::redact(&a.preview);
        if preview != a.preview {
            return Err(Error::SecretRejected("approval content contains secret material; refusing to queue it".into()));
        }
        let id = ids::stable_id("apr", &[&a.idempotency_key]);
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO approvals({COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending',?10,?11,?12,NULL,'')"),
                params![
                    id,
                    a.idempotency_key,
                    a.kind,
                    a.class.as_str(),
                    a.title,
                    a.preview,
                    a.payload.to_string(),
                    hash_payload(&a.payload),
                    serde_json::to_string(&a.reasons)?,
                    a.origin,
                    to_rfc3339(now),
                    to_rfc3339(&(*now + a.ttl)),
                ],
            )
            .db()?;
        self.db.audit(&a.origin, "approval_requested", &format!("{id} kind={} title={}", a.kind, a.title))?;
        Ok(SubmitOutcome::Created(id))
    }

    pub fn get(&self, id: &str) -> Result<Approval> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM approvals WHERE id = ?1"), params![id], row_to_approval)
            .optional()
            .db()?
            .ok_or_else(|| Error::NotFound(format!("approval {id}")))
    }

    pub fn by_key(&self, key: &str) -> Result<Option<Approval>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM approvals WHERE idempotency_key = ?1"), params![key], row_to_approval)
            .optional()
            .db()
    }

    /// Approvals with the given status (or all), newest first.
    pub fn list(&self, status: Option<ApprovalStatus>) -> Result<Vec<Approval>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(&format!(
                "SELECT {COLS} FROM approvals WHERE (?1 IS NULL OR status = ?1) ORDER BY created_at DESC, id"
            ))
            .db()?;
        let rows = st.query_map(params![status.map(|s| s.as_str())], row_to_approval).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    fn set_status(&self, id: &str, s: ApprovalStatus, note: &str, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE approvals SET status = ?2, note = ?3, decided_at = COALESCE(decided_at, ?4) WHERE id = ?1",
                params![id, s.as_str(), hatch_core::secrets::redact(note), to_rfc3339(now)],
            )
            .db()?;
        self.db.audit("approvals", s.as_str(), id)
    }

    /// Record the user's decision. Expired approvals become `Expired` regardless of the decision.
    pub fn decide(&self, id: &str, approve: bool, note: &str, now: &Timestamp) -> Result<ApprovalStatus> {
        let a = self.get(id)?;
        if a.status != ApprovalStatus::Pending {
            return Err(Error::Invalid(format!("approval {id} is already {}", a.status.as_str())));
        }
        let status = if a.expires_at <= *now {
            ApprovalStatus::Expired
        } else if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.set_status(id, status, note, now)?;
        Ok(status)
    }

    pub fn mark_executed(&self, id: &str, result: &str, now: &Timestamp) -> Result<()> {
        self.set_status(id, ApprovalStatus::Executed, result, now)
    }

    pub fn mark_failed(&self, id: &str, error: &str, now: &Timestamp) -> Result<()> {
        self.set_status(id, ApprovalStatus::Failed, error, now)
    }

    /// Expire every pending approval past its deadline. Returns how many.
    pub fn expire_due(&self, now: &Timestamp) -> Result<usize> {
        let due: Vec<String> = self
            .list(Some(ApprovalStatus::Pending))?
            .into_iter()
            .filter(|a| a.expires_at <= *now)
            .map(|a| a.id)
            .collect();
        for id in &due {
            self.set_status(id, ApprovalStatus::Expired, "deadline passed", now)?;
        }
        Ok(due.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new(key: &str) -> NewApproval {
        NewApproval {
            idempotency_key: key.into(),
            kind: "send_email".into(),
            class: ActionClass::SendMessage,
            title: "回复王总".into(),
            preview: "To: boss@corp.com\n收到，明天交。".into(),
            payload: json!({"to": ["boss@corp.com"], "body": "收到，明天交。"}),
            reasons: vec!["default".into()],
            origin: "test".into(),
            ttl: Duration::hours(1),
        }
    }

    #[test]
    fn submit_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let q = ApprovalQueue::new(&db);
        let now = hatch_core::time::now();
        let a = q.submit(&new("k1"), &now).unwrap();
        assert!(matches!(a, SubmitOutcome::Created(_)));
        let b = q.submit(&new("k1"), &now).unwrap();
        assert_eq!(b, SubmitOutcome::Existing(a.id().into(), ApprovalStatus::Pending));
        assert_eq!(q.list(Some(ApprovalStatus::Pending)).unwrap().len(), 1);
        assert!(q.get(a.id()).unwrap().payload_intact());
    }

    #[test]
    fn decisions_and_expiry() {
        let db = Db::open_in_memory().unwrap();
        let q = ApprovalQueue::new(&db);
        let now = hatch_core::time::now();
        let a = q.submit(&new("a"), &now).unwrap();
        assert_eq!(q.decide(a.id(), true, "ok", &now).unwrap(), ApprovalStatus::Approved);
        assert!(q.decide(a.id(), true, "again", &now).is_err());
        let b = q.submit(&new("b"), &now).unwrap();
        let later = now + Duration::hours(2);
        // Approving after the deadline does NOT approve.
        assert_eq!(q.decide(b.id(), true, "late", &later).unwrap(), ApprovalStatus::Expired);
        let c = q.submit(&new("c"), &now).unwrap();
        assert_eq!(q.expire_due(&later).unwrap(), 1);
        assert_eq!(q.get(c.id()).unwrap().status, ApprovalStatus::Expired);
        q.mark_executed(a.id(), "sent", &now).unwrap();
        assert_eq!(q.get(a.id()).unwrap().status, ApprovalStatus::Executed);
        assert_eq!(q.list(None).unwrap().len(), 3);
    }

    #[test]
    fn refuses_secret_content() {
        let db = Db::open_in_memory().unwrap();
        let q = ApprovalQueue::new(&db);
        let mut a = new("s");
        a.preview = "你的验证码是 889911".into();
        assert!(q.submit(&a, &hatch_core::time::now()).is_err());
    }
}
