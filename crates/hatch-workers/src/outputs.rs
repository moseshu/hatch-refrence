//! Worker output waiting to be surfaced, and its delivery.
//!
//! Every worker result is stored in `worker_outputs`. `hatch chat` picks up
//! the unread ones as a short "background updates" layer (see
//! [`handoff_lines`]) and marks them read. Separately, the job's delivery
//! setting decides whether a notification is raised.

use hatch_core::time::{fmt_local, parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::tokens::estimate_tokens;
use hatch_core::{ids, Result};
use hatch_policy::{AttentionPolicy, Notifier, Priority};
use hatch_scheduler::{Delivery, Job};
use hatch_store::{params, rusqlite, Db, DbResultExt};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct WorkerOutput {
    pub id: String,
    pub kind: String,
    pub created_at: Timestamp,
    pub title: String,
    pub content: String,
    pub read: bool,
}

pub struct WorkerOutputs<'a> {
    db: &'a Db,
}

impl<'a> WorkerOutputs<'a> {
    pub fn new(db: &'a Db) -> Self {
        WorkerOutputs { db }
    }

    pub fn add(&self, kind: &str, title: &str, content: &str, now: &Timestamp) -> Result<String> {
        let id = ids::random_id("wout");
        self.db
            .conn()
            .execute(
                "INSERT INTO worker_outputs(id, kind, created_at, title, content) VALUES(?1,?2,?3,?4,?5)",
                params![id, kind, to_rfc3339(now), title, hatch_core::secrets::redact(content)],
            )
            .db()?;
        Ok(id)
    }

    /// Unread outputs, oldest first.
    pub fn unread(&self) -> Result<Vec<WorkerOutput>> {
        self.query("WHERE read_at IS NULL ORDER BY created_at, rowid", 1000)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<WorkerOutput>> {
        self.query("ORDER BY created_at DESC, rowid DESC", limit)
    }

    fn query(&self, tail: &str, limit: usize) -> Result<Vec<WorkerOutput>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!("SELECT id, kind, created_at, title, content, read_at FROM worker_outputs {tail} LIMIT ?1"))
            .db()?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok(WorkerOutput {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    created_at: parse_rfc3339(&r.get::<_, String>(2)?).unwrap_or_default(),
                    title: r.get(3)?,
                    content: r.get(4)?,
                    read: r.get::<_, Option<String>>(5)?.is_some(),
                })
            })
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn mark_read(&self, ids: &[String], now: &Timestamp) -> Result<()> {
        let c = self.db.conn();
        for id in ids {
            c.execute("UPDATE worker_outputs SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL", params![id, to_rfc3339(now)]).db()?;
        }
        Ok(())
    }
}

/// Render unread outputs for the chat context within `budget` estimated tokens.
/// Newest first; anything that does not fit is named but not included.
/// Returns the lines and the ids that were surfaced (to mark read).
pub fn handoff_lines(outputs: &[WorkerOutput], tz: &chrono_tz::Tz, budget: usize) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    let mut used = 0;
    for o in outputs.iter().rev() {
        let head = format!("[{}] {}（{}）", o.kind, o.title, fmt_local(&o.created_at, tz));
        let body: String = o.content.chars().take(1500).collect();
        let full = format!("{head}\n{body}");
        let cost = estimate_tokens(&full);
        if used + cost <= budget {
            used += cost;
            lines.push(full);
        } else {
            used += estimate_tokens(&head);
            lines.push(format!("{head}（内容较长，未放入上下文；可用 `hatch brief show` 查看）"));
        }
        ids.push(o.id.clone());
    }
    (lines, ids)
}

/// Raise a notification according to the job's delivery setting.
pub fn deliver(db: &Db, attention: &AttentionPolicy, job: &Job, text: &str, ref_id: &str, now: &Timestamp) -> Result<Option<String>> {
    let priority = match job.delivery {
        Delivery::Silent => return Ok(None),
        Delivery::Notify => Priority::High,
        Delivery::Digest => Priority::Normal,
    };
    let d = Notifier::new(db, attention.clone()).notify(priority, text, ref_id, now)?;
    Ok(Some(d.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_outputs_are_surfaced_newest_first_within_budget() {
        let db = Db::open_in_memory().unwrap();
        let w = WorkerOutputs::new(&db);
        let now = hatch_core::time::now();
        w.add("brief", "每日简报 2026-11-02", "旧内容", &now).unwrap();
        w.add("brief", "每日简报 2026-11-03", &"很长的内容".repeat(400), &now).unwrap();
        let unread = w.unread().unwrap();
        let (lines, ids) = handoff_lines(&unread, &chrono_tz::Asia::Shanghai, 300);
        assert_eq!(ids.len(), 2);
        assert!(lines[0].contains("11-03") && lines[0].contains("未放入上下文"));
        assert!(lines[1].contains("旧内容"));
        w.mark_read(&ids, &now).unwrap();
        assert!(w.unread().unwrap().is_empty());
    }
}
