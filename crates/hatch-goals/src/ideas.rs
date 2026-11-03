//! Ideas corpus: one SQLite table (`ideas`), following the existing
//! store conventions (claims, briefs, worker_outputs are all SQLite).
//!
//! Fields: id, text, source, status (`new` / `acted` / `dismissed`),
//! confidence (0..1), created_at. Ideas a worker records are suggestions
//! with low confidence — never user decisions — and carry their source
//! (`worker:<kind>`). A dismissed idea NEVER resurfaces: [`IdeaStore::list`]
//! excludes dismissed rows by construction, and there is no flag to
//! include them.

use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use serde::Serialize;

/// Confidence recorded for worker-generated suggestions (low on purpose).
pub const WORKER_SUGGESTION_CONFIDENCE: f64 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeaStatus {
    New,
    Acted,
    Dismissed,
}

impl IdeaStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IdeaStatus::New => "new",
            IdeaStatus::Acted => "acted",
            IdeaStatus::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<IdeaStatus> {
        match s {
            "new" => Some(IdeaStatus::New),
            "acted" => Some(IdeaStatus::Acted),
            "dismissed" => Some(IdeaStatus::Dismissed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Idea {
    pub id: String,
    pub text: String,
    pub source: String,
    pub status: IdeaStatus,
    pub confidence: f64,
    pub created_at: Timestamp,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Idea> {
    Ok(Idea {
        id: r.get(0)?,
        text: r.get(1)?,
        source: r.get(2)?,
        status: IdeaStatus::parse(&r.get::<_, String>(3)?).unwrap_or(IdeaStatus::New),
        confidence: r.get(4)?,
        created_at: parse_rfc3339(&r.get::<_, String>(5)?).unwrap_or_default(),
    })
}

const COLS: &str = "id, text, source, status, confidence, created_at";

pub struct IdeaStore<'a> {
    db: &'a Db,
}

impl<'a> IdeaStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        IdeaStore { db }
    }

    pub fn add(&self, text: &str, source: &str, confidence: f64, now: &Timestamp) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Invalid("idea text must not be empty".into()));
        }
        if !(0.0..=1.0).contains(&confidence) {
            return Err(Error::Invalid(format!("confidence {confidence} is outside 0..=1")));
        }
        let id = ids::random_id("idea");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO ideas({COLS}) VALUES(?1,?2,?3,?4,?5,?6)"),
                params![id, text, source, IdeaStatus::New.as_str(), confidence, to_rfc3339(now)],
            )
            .db()?;
        Ok(id)
    }

    /// Record a worker-generated suggestion: low confidence, source
    /// recorded, status `new`. It is a suggestion, never a decision.
    pub fn record_suggestion(&self, text: &str, worker_kind: &str, now: &Timestamp) -> Result<String> {
        self.add(text, &format!("worker:{worker_kind}"), WORKER_SUGGESTION_CONFIDENCE, now)
    }

    /// Live ideas only: dismissed ideas never resurface here.
    pub fn list(&self, limit: usize) -> Result<Vec<Idea>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {COLS} FROM ideas WHERE status != 'dismissed' ORDER BY created_at DESC, rowid DESC LIMIT ?1"
            ))
            .db()?;
        let rows = st.query_map(params![limit as i64], row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn get(&self, id: &str) -> Result<Option<Idea>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM ideas WHERE id = ?1"), params![id], row)
            .optional()
            .db()
    }

    fn set_status(&self, id: &str, status: IdeaStatus) -> Result<()> {
        let n = self
            .db
            .conn()
            .execute("UPDATE ideas SET status = ?2 WHERE id = ?1", params![id, status.as_str()])
            .db()?;
        if n == 0 {
            return Err(Error::Invalid(format!("no such idea {id}")));
        }
        Ok(())
    }

    pub fn dismiss(&self, id: &str) -> Result<()> {
        self.set_status(id, IdeaStatus::Dismissed)
    }

    pub fn mark_acted(&self, id: &str) -> Result<()> {
        self.set_status(id, IdeaStatus::Acted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    #[test]
    fn dismissed_ideas_never_resurface() {
        let db = Db::open_in_memory().unwrap();
        let store = IdeaStore::new(&db);
        let a = store.add("学 Rust 宏", "user", 1.0, &now()).unwrap();
        let b = store.add("每天跑步", "user", 1.0, &now()).unwrap();
        let c = store.record_suggestion("从简报里看到一个点子", "daily_brief", &now()).unwrap();
        assert_eq!(store.get(&c).unwrap().unwrap().confidence, WORKER_SUGGESTION_CONFIDENCE);
        assert!(store.get(&c).unwrap().unwrap().source.starts_with("worker:"));
        store.dismiss(&b).unwrap();
        let live: Vec<String> = store.list(10).unwrap().into_iter().map(|i| i.id).collect();
        assert!(live.contains(&a) && live.contains(&c));
        assert!(!live.contains(&b), "dismissed idea resurfaced in list()");
        // Dismissing twice, or acting on a dismissed idea, stays dismissed in listings.
        store.dismiss(&b).unwrap();
        assert!(!store.list(10).unwrap().into_iter().any(|i| i.id == b));
        assert!(store.dismiss("idea_missing").is_err());
        assert!(store.add("", "user", 1.0, &now()).is_err());
        assert!(store.add("x", "user", 1.5, &now()).is_err());
        store.mark_acted(&a).unwrap();
        assert_eq!(store.get(&a).unwrap().unwrap().status, IdeaStatus::Acted);
    }
}
