//! Stored briefs (`hatch brief show`).

use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct StoredBrief {
    pub id: String,
    pub local_date: String,
    pub created_at: Timestamp,
    pub content: String,
    pub facts: serde_json::Value,
    pub model: String,
}

pub struct BriefStore<'a> {
    db: &'a Db,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<StoredBrief> {
    Ok(StoredBrief {
        id: r.get(0)?,
        local_date: r.get(1)?,
        created_at: parse_rfc3339(&r.get::<_, String>(2)?).unwrap_or_default(),
        content: r.get(3)?,
        facts: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        model: r.get(5)?,
    })
}

const COLS: &str = "id, local_date, created_at, content, facts, model";

impl<'a> BriefStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        BriefStore { db }
    }

    pub fn save(&self, local_date: &str, content: &str, facts: &serde_json::Value, model: &str, now: &Timestamp) -> Result<String> {
        let id = ids::random_id("brief");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO briefs({COLS}) VALUES(?1,?2,?3,?4,?5,?6)"),
                params![id, local_date, to_rfc3339(now), hatch_core::secrets::redact(content), facts.to_string(), model],
            )
            .db()?;
        Ok(id)
    }

    /// Latest brief, optionally for one local date (`YYYY-MM-DD`).
    pub fn latest(&self, local_date: Option<&str>) -> Result<Option<StoredBrief>> {
        self.db
            .conn()
            .query_row(
                &format!("SELECT {COLS} FROM briefs WHERE (?1 IS NULL OR local_date = ?1) ORDER BY created_at DESC, rowid DESC LIMIT 1"),
                params![local_date],
                row,
            )
            .optional()
            .db()
    }

    pub fn list(&self, limit: usize) -> Result<Vec<StoredBrief>> {
        let c = self.db.conn();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM briefs ORDER BY created_at DESC, rowid DESC LIMIT ?1")).db()?;
        let rows = st.query_map(params![limit as i64], row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn count_for(&self, local_date: &str) -> Result<i64> {
        self.db.conn().query_row("SELECT COUNT(*) FROM briefs WHERE local_date = ?1", params![local_date], |r| r.get(0)).db()
    }
}
