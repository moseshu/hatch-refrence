//! Triage persistence: results, durable steps and run records.

use crate::model::{Category, TriageResult};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_policy::Priority;
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};

pub const WORKFLOW: &str = "mail_triage";

pub struct TriageRepo<'a> {
    db: &'a Db,
}

fn row_to_result(r: &rusqlite::Row) -> rusqlite::Result<TriageResult> {
    let cat: String = r.get(1)?;
    let pri: String = r.get(2)?;
    let events: String = r.get(6)?;
    let notes: String = r.get(8)?;
    Ok(TriageResult {
        mail_id: r.get(0)?,
        category: Category::parse(&cat).unwrap_or(Category::Unclassified),
        priority: Priority::parse(&pri).unwrap_or(Priority::Normal),
        summary: r.get(3)?,
        needs_reply: r.get::<_, i64>(4)? != 0,
        reply_hint: r.get(5)?,
        events: serde_json::from_str(&events).unwrap_or_default(),
        suspicious: r.get::<_, i64>(7)? != 0,
        notes: serde_json::from_str(&notes).unwrap_or_default(),
        model: r.get(9)?,
    })
}

const COLS: &str = "mail_id, category, priority, summary, needs_reply, reply_hint, events_json, suspicious, notes, model";

impl<'a> TriageRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        TriageRepo { db }
    }

    pub fn save(&self, r: &TriageResult, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO triage_results({COLS}, triaged_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(mail_id) DO UPDATE SET category=excluded.category, priority=excluded.priority,
                 summary=excluded.summary, needs_reply=excluded.needs_reply, reply_hint=excluded.reply_hint,
                 events_json=excluded.events_json, suspicious=excluded.suspicious, notes=excluded.notes,
                 model=excluded.model, triaged_at=excluded.triaged_at"),
                params![
                    r.mail_id,
                    r.category.as_str(),
                    r.priority.as_str(),
                    hatch_core::secrets::redact(&r.summary),
                    r.needs_reply as i64,
                    r.reply_hint,
                    serde_json::to_string(&r.events)?,
                    r.suspicious as i64,
                    serde_json::to_string(&r.notes)?,
                    r.model,
                    to_rfc3339(now)
                ],
            )
            .db()?;
        Ok(())
    }

    pub fn get(&self, mail_id: &str) -> Result<Option<TriageResult>> {
        self.db
            .conn()
            .query_row(
                &format!("SELECT {COLS} FROM triage_results WHERE mail_id = ?1"),
                params![mail_id],
                row_to_result,
            )
            .optional()
            .db()
    }

    pub fn all(&self) -> Result<Vec<TriageResult>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLS} FROM triage_results ORDER BY triaged_at"
            ))
            .db()?;
        let rows = st.query_map([], row_to_result).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }
}

/// Durable per-item step records.
pub struct Steps<'a> {
    db: &'a Db,
}

impl<'a> Steps<'a> {
    pub fn new(db: &'a Db) -> Self {
        Steps { db }
    }

    pub fn get(&self, item: &str, step: &str) -> Result<Option<(String, String)>> {
        self.db
            .conn()
            .query_row(
                "SELECT status, output FROM workflow_steps WHERE workflow = ?1 AND item = ?2 AND step = ?3",
                params![WORKFLOW, item, step],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .db()
    }

    pub fn is_done(&self, item: &str, step: &str) -> Result<bool> {
        Ok(matches!(self.get(item, step)?, Some((s, _)) if s == "done"))
    }

    pub fn set(
        &self,
        item: &str,
        step: &str,
        status: &str,
        output: &str,
        now: &Timestamp,
    ) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO workflow_steps(workflow, item, step, status, output, updated_at) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(workflow, item, step) DO UPDATE SET status=excluded.status, output=excluded.output, updated_at=excluded.updated_at",
                params![WORKFLOW, item, step, status, hatch_core::secrets::redact(output), to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    /// Stored mails whose triage has not reached the final step.
    pub fn unfinished_mails(&self) -> Result<Vec<String>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(
                "SELECT id FROM mail_messages m WHERE NOT EXISTS (
                    SELECT 1 FROM workflow_steps s WHERE s.workflow = ?1 AND s.item = m.id AND s.step = 'complete' AND s.status = 'done')
                 ORDER BY COALESCE(sent_at, fetched_at), id",
            )
            .db()?;
        let rows = st
            .query_map(params![WORKFLOW], |r| r.get::<_, String>(0))
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }
}

/// Run records.
pub fn start_run(db: &Db, run_id: &str, now: &Timestamp) -> Result<()> {
    db.conn()
        .execute(
            "INSERT INTO workflow_runs(run_id, workflow, started_at, status) VALUES(?1, ?2, ?3, 'running')",
            params![run_id, WORKFLOW, to_rfc3339(now)],
        )
        .db()?;
    Ok(())
}

pub fn finish_run(
    db: &Db,
    run_id: &str,
    status: &str,
    receipt_json: &str,
    now: &Timestamp,
) -> Result<()> {
    db.conn()
        .execute(
            "UPDATE workflow_runs SET status = ?2, receipt = ?3, finished_at = ?4 WHERE run_id = ?1",
            params![run_id, status, receipt_json, to_rfc3339(now)],
        )
        .db()?;
    Ok(())
}

/// Most recent run's receipt JSON.
pub fn last_receipt(db: &Db) -> Result<Option<String>> {
    db.conn()
        .query_row(
            "SELECT receipt FROM workflow_runs WHERE workflow = ?1 AND receipt != '' ORDER BY started_at DESC, rowid DESC LIMIT 1",
            params![WORKFLOW],
            |r| r.get(0),
        )
        .optional()
        .db()
}
