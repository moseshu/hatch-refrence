//! Idempotency ledger for external side effects.
//!
//! Before an executor sends a mail it calls [`ActionLedger::begin`] with the
//! action's idempotency key:
//!
//! * `Fresh`       — never attempted (or definitively failed before): go ahead.
//! * `AlreadyDone` — it happened; do not repeat, return the stored result.
//! * `InProgress`  — a previous attempt started but never recorded an outcome
//!   (crash mid-flight). We cannot know whether the mail went out, so we do
//!   **not** resend; the caller must surface this to the user.

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerState {
    Fresh,
    AlreadyDone(String),
    InProgress,
}

pub struct ActionLedger<'a> {
    db: &'a Db,
}

impl<'a> ActionLedger<'a> {
    pub fn new(db: &'a Db) -> Self {
        ActionLedger { db }
    }

    pub fn begin(&self, key: &str, kind: &str, now: &Timestamp) -> Result<LedgerState> {
        let row: Option<(String, String)> = self
            .db
            .conn()
            .query_row(
                "SELECT status, result FROM action_ledger WHERE idempotency_key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .db()?;
        match row {
            Some((s, result)) if s == "done" => Ok(LedgerState::AlreadyDone(result)),
            Some((s, _)) if s == "in_progress" => Ok(LedgerState::InProgress),
            Some(_) => {
                self.db
                    .conn()
                    .execute(
                        "UPDATE action_ledger SET status = 'in_progress', started_at = ?2, finished_at = NULL WHERE idempotency_key = ?1",
                        params![key, to_rfc3339(now)],
                    )
                    .db()?;
                Ok(LedgerState::Fresh)
            }
            None => {
                self.db
                    .conn()
                    .execute(
                        "INSERT INTO action_ledger(idempotency_key, kind, status, result, started_at) VALUES(?1, ?2, 'in_progress', '', ?3)",
                        params![key, kind, to_rfc3339(now)],
                    )
                    .db()?;
                Ok(LedgerState::Fresh)
            }
        }
    }

    pub fn complete(&self, key: &str, result: &str, now: &Timestamp) -> Result<()> {
        self.finish(key, "done", result, now)
    }

    /// Record a definite failure (nothing was delivered); a later `begin` may retry.
    pub fn fail(&self, key: &str, error: &str, now: &Timestamp) -> Result<()> {
        self.finish(key, "failed", error, now)
    }

    fn finish(&self, key: &str, status: &str, result: &str, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE action_ledger SET status = ?2, result = ?3, finished_at = ?4 WHERE idempotency_key = ?1",
                params![key, status, hatch_core::secrets::redact(result), to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    pub fn status(&self, key: &str) -> Result<Option<String>> {
        self.db
            .conn()
            .query_row("SELECT status FROM action_ledger WHERE idempotency_key = ?1", params![key], |r| r.get(0))
            .optional()
            .db()
    }

    /// Stored outcome of a finished action, if any.
    pub fn result(&self, key: &str) -> Result<Option<String>> {
        self.db
            .conn()
            .query_row("SELECT result FROM action_ledger WHERE idempotency_key = ?1", params![key], |r| r.get(0))
            .optional()
            .db()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_once_semantics() {
        let db = Db::open_in_memory().unwrap();
        let l = ActionLedger::new(&db);
        let now = hatch_core::time::now();
        assert_eq!(l.begin("k", "send_email", &now).unwrap(), LedgerState::Fresh);
        // Crash before complete(): a second attempt must not resend.
        assert_eq!(l.begin("k", "send_email", &now).unwrap(), LedgerState::InProgress);
        l.complete("k", "message-id <x@y>", &now).unwrap();
        assert_eq!(l.begin("k", "send_email", &now).unwrap(), LedgerState::AlreadyDone("message-id <x@y>".into()));
        // Definite failure allows retry.
        assert_eq!(l.begin("f", "send_email", &now).unwrap(), LedgerState::Fresh);
        l.fail("f", "SMTP 550", &now).unwrap();
        assert_eq!(l.begin("f", "send_email", &now).unwrap(), LedgerState::Fresh);
        assert_eq!(l.status("f").unwrap().as_deref(), Some("in_progress"));
    }
}
