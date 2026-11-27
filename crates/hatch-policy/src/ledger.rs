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

use hatch_core::secrets::{self, SecretKind};
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

    /// Claim the key for one attempt. The read-then-write runs in a
    /// single `BEGIN IMMEDIATE` transaction: two concurrent attempts
    /// for the same key serialize, so exactly one sees `Fresh` and
    /// the other sees `InProgress` (never a duplicate execution, and
    /// never a unique-constraint error surfacing as a failure).
    pub fn begin(&self, key: &str, kind: &str, now: &Timestamp) -> Result<LedgerState> {
        self.db.transaction(|c| {
            let row: Option<(String, String)> = c
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
                    c.execute(
                        "UPDATE action_ledger SET status = 'in_progress', started_at = ?2, finished_at = NULL WHERE idempotency_key = ?1",
                        params![key, to_rfc3339(now)],
                    )
                    .db()?;
                    Ok(LedgerState::Fresh)
                }
                None => {
                    c.execute(
                        "INSERT INTO action_ledger(idempotency_key, kind, status, result, started_at) VALUES(?1, ?2, 'in_progress', '', ?3)",
                        params![key, kind, to_rfc3339(now)],
                    )
                    .db()?;
                    Ok(LedgerState::Fresh)
                }
            }
        })
    }

    pub fn complete(&self, key: &str, result: &str, now: &Timestamp) -> Result<()> {
        self.finish(key, "done", result, now)
    }

    /// Record a definite failure (nothing was delivered); a later `begin` may retry.
    pub fn fail(&self, key: &str, error: &str, now: &Timestamp) -> Result<()> {
        self.finish(key, "failed", error, now)
    }

    fn finish(&self, key: &str, status: &str, result: &str, now: &Timestamp) -> Result<()> {
        let conn = self.db.conn();
        let stored_result = if status == "done" {
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM action_ledger WHERE idempotency_key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .db()?;
            redact_receipt(kind.as_deref().unwrap_or_default(), result)
        } else {
            secrets::redact(result)
        };
        conn.execute(
            "UPDATE action_ledger SET status = ?2, result = ?3, finished_at = ?4 WHERE idempotency_key = ?1",
            params![key, status, stored_result, to_rfc3339(now)],
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

/// Successful executor receipts carry lookup keys for resuming. Preserve
/// numeric IDs in the executor's reference field while redacting the rest
/// of the receipt. Other secret kinds are still redacted in every field.
fn redact_receipt(kind: &str, receipt: &str) -> String {
    let Some((prefix, reference, suffix)) = receipt_reference(kind, receipt) else {
        return secrets::redact(receipt);
    };
    if secrets::kinds(reference)
        .iter()
        .any(|kind| !matches!(kind, SecretKind::BankCard | SecretKind::NationalId))
    {
        return secrets::redact(receipt);
    }
    format!(
        "{}{reference}{}",
        secrets::redact(prefix),
        secrets::redact(suffix)
    )
}

fn receipt_reference<'a>(kind: &str, receipt: &'a str) -> Option<(&'a str, &'a str, &'a str)> {
    let (prefix_len, reference) = match kind {
        "send_email" => {
            let (id, _) = receipt.strip_prefix("sent ")?.split_once(" to ")?;
            if let Some(inner) = id.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
                let (local, domain) = inner.split_once('@')?;
                if !is_identifier(local, b"._+-") || !is_identifier(domain, b".-") {
                    return None;
                }
            } else if !is_identifier(id, b"_-") {
                return None;
            }
            ("sent ".len(), id)
        }
        "create_event" | "create_google_event" => {
            let (id, _) = receipt.strip_prefix("event ")?.split_once(" created (")?;
            if !is_identifier(id, b"_-") {
                return None;
            }
            ("event ".len(), id)
        }
        "create_job" => {
            let heading = "定时任务已创建：";
            let (before, after) = receipt.strip_prefix(heading)?.rsplit_once('（')?;
            let (id, _) = after.split_once('，')?;
            if !id.strip_prefix("job_").is_some_and(|s| {
                s.len() == 16 && s.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return None;
            }
            (heading.len() + before.len() + '（'.len_utf8(), id)
        }
        _ => return None,
    };
    let (prefix, rest) = receipt.split_at(prefix_len);
    let (_, suffix) = rest.split_at(reference.len());
    Some((prefix, reference, suffix))
}

fn is_identifier(value: &str, punctuation: &[u8]) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || punctuation.contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_identifiers_survive_receipt_redaction() {
        let db = Db::open_in_memory().unwrap();
        let ledger = ActionLedger::new(&db);
        let now = hatch_core::time::now();
        for (key, kind, receipt, expected) in [
            (
                "mail", "send_email",
                "sent 4111111111111111 to user@corp.example",
                "sent 4111111111111111 to user@corp.example",
            ),
            (
                "smtp", "send_email",
                "sent <a4111111111111111b@corp.example> to user@corp.example",
                "sent <a4111111111111111b@corp.example> to user@corp.example",
            ),
            (
                "calendar", "create_google_event",
                "event 4111111111111111 created (card4111111111111111)",
                "event 4111111111111111 created (card[REDACTED:bank_card])",
            ),
            (
                "job", "create_job",
                "定时任务已创建：周报（job_4111111111111111，每周一）",
                "定时任务已创建：周报（job_4111111111111111，每周一）",
            ),
            (
                "credential", "send_email",
                "sent sk-abcdefghijklmnopqrstuv to user@corp.example",
                "sent [REDACTED:api_key] to user@corp.example",
            ),
            (
                "output", "exec_command",
                "sent 4111111111111111 to user@corp.example",
                "sent [REDACTED:bank_card] to user@corp.example",
            ),
        ] {
            assert_eq!(ledger.begin(key, kind, &now).unwrap(), LedgerState::Fresh);
            ledger.complete(key, receipt, &now).unwrap();
            assert_eq!(ledger.result(key).unwrap().as_deref(), Some(expected));
            assert_eq!(
                ledger.begin(key, kind, &now).unwrap(),
                LedgerState::AlreadyDone(expected.into())
            );
        }
    }

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
