//! Session transcripts.
//!
//! Each message carries its trust label so the agent can tell, after a restart,
//! whether the live context contains untrusted content. Compaction marks old
//! rows as compacted and appends a `summary` row; history is "latest summary +
//! all non-compacted messages".

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result, Trust};
use hatch_llm::{ChatMessage, Role, ToolCall};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredMessage {
    pub seq: i64,
    pub message: ChatMessage,
    pub trust: Trust,
}

#[derive(Serialize, Deserialize, Default)]
struct Meta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(default)]
    trust: Option<Trust>,
}

pub struct SessionStore<'a> {
    db: &'a Db,
}

impl<'a> SessionStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        SessionStore { db }
    }

    pub fn create(&self, title: &str, now: &Timestamp) -> Result<String> {
        let id = ids::random_id("ses");
        self.db
            .conn()
            .execute("INSERT INTO sessions(id, created_at, title) VALUES(?1, ?2, ?3)", params![id, to_rfc3339(now), title])
            .db()?;
        Ok(id)
    }

    pub fn exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .db
            .conn()
            .query_row("SELECT 1 FROM sessions WHERE id = ?1", params![id], |_| Ok(()))
            .optional()
            .db()?
            .is_some())
    }

    /// Most recent session id, if any.
    pub fn latest(&self) -> Result<Option<String>> {
        self.db
            .conn()
            .query_row("SELECT id FROM sessions ORDER BY created_at DESC, rowid DESC LIMIT 1", [], |r| r.get(0))
            .optional()
            .db()
    }

    fn next_seq(&self, session: &str) -> Result<i64> {
        self.db
            .conn()
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM session_messages WHERE session_id = ?1", params![session], |r| r.get(0))
            .db()
    }

    fn insert(&self, session: &str, role: &str, content: &str, meta: &Meta, now: &Timestamp) -> Result<i64> {
        if !self.exists(session)? {
            return Err(Error::NotFound(format!("session {session}")));
        }
        let seq = self.next_seq(session)?;
        self.db
            .conn()
            .execute(
                "INSERT INTO session_messages(session_id, seq, role, content, meta, compacted, created_at) VALUES(?1,?2,?3,?4,?5,0,?6)",
                params![session, seq, role, content, serde_json::to_string(meta)?, to_rfc3339(now)],
            )
            .db()?;
        Ok(seq)
    }

    pub fn append(&self, session: &str, m: &ChatMessage, trust: Trust, now: &Timestamp) -> Result<i64> {
        let meta = Meta { tool_calls: m.tool_calls.clone(), tool_call_id: m.tool_call_id.clone(), trust: Some(trust) };
        self.insert(session, m.role.as_str(), &m.content, &meta, now)
    }

    /// Latest summary (if any) and every non-compacted message, in order.
    pub fn active(&self, session: &str) -> Result<(Option<String>, Vec<StoredMessage>)> {
        let summary: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT content FROM session_messages WHERE session_id = ?1 AND role = 'summary' ORDER BY seq DESC LIMIT 1",
                params![session],
                |r| r.get(0),
            )
            .optional()
            .db()?;
        let conn = self.db.conn();
        let mut st = conn.prepare("SELECT seq, role, content, meta FROM session_messages WHERE session_id = ?1 AND compacted = 0 AND role != 'summary' ORDER BY seq")
            .db()?;
        let rows = st
            .query_map(params![session], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
            .db()?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, role, content, meta) = row.db()?;
            let meta: Meta = serde_json::from_str(&meta).unwrap_or_default();
            let role = Role::parse(&role).unwrap_or(Role::User);
            out.push(StoredMessage {
                seq,
                trust: meta.trust.unwrap_or(if role == Role::Tool { Trust::Untrusted } else { Trust::System }),
                message: ChatMessage { role, content, tool_calls: meta.tool_calls, tool_call_id: meta.tool_call_id },
            });
        }
        Ok((summary, out))
    }

    /// Mark every message with `seq <= upto` compacted and store the summary.
    pub fn compact(&self, session: &str, upto: i64, summary: &str, now: &Timestamp) -> Result<()> {
        self.db.transaction(|c| {
            c.execute(
                "UPDATE session_messages SET compacted = 1 WHERE session_id = ?1 AND seq <= ?2 AND role != 'summary'",
                params![session, upto],
            )
            .db()?;
            Ok(())
        })?;
        self.insert(session, "summary", summary, &Meta::default(), now)?;
        Ok(())
    }
}

/// One session in a listing.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub messages: i64,
}

/// One stored row, for display.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptRow {
    pub seq: i64,
    pub role: String,
    pub content: String,
    pub compacted: bool,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindReport {
    pub removed: usize,
    /// Rows hidden by a removed summary that are live again.
    pub uncompacted: usize,
}

impl<'a> SessionStore<'a> {
    /// Sessions newest first. Sub-agent sessions are hidden unless asked for.
    pub fn list(&self, include_subagents: bool, limit: usize) -> Result<Vec<SessionInfo>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT s.id, s.title, s.created_at, (SELECT COUNT(*) FROM session_messages m WHERE m.session_id = s.id)
                 FROM sessions s WHERE (?1 OR s.title NOT LIKE 'subagent:%') ORDER BY s.created_at DESC, s.rowid DESC LIMIT ?2",
            )
            .db()?;
        let rows = st
            .query_map(params![include_subagents, limit as i64], |r| {
                Ok(SessionInfo { id: r.get(0)?, title: r.get(1)?, created_at: r.get(2)?, messages: r.get(3)? })
            })
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// One session's info, if it exists.
    pub fn info(&self, session: &str) -> Result<Option<SessionInfo>> {
        let conn = self.db.conn();
        conn.query_row(
            "SELECT s.id, s.title, s.created_at, (SELECT COUNT(*) FROM session_messages m WHERE m.session_id = s.id) FROM sessions s WHERE s.id = ?1",
            params![session],
            |r| Ok(SessionInfo { id: r.get(0)?, title: r.get(1)?, created_at: r.get(2)?, messages: r.get(3)? }),
        )
        .optional()
        .db()
    }

    /// Every stored row (including compacted ones and summaries), in order.
    pub fn transcript(&self, session: &str) -> Result<Vec<TranscriptRow>> {
        if !self.exists(session)? {
            return Err(Error::NotFound(format!("session {session}")));
        }
        let conn = self.db.conn();
        let mut st = conn
            .prepare("SELECT seq, role, content, meta, compacted FROM session_messages WHERE session_id = ?1 ORDER BY seq")
            .db()?;
        let rows = st
            .query_map(params![session], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?))
            })
            .db()?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, role, content, meta, compacted) = row.db()?;
            let meta: Meta = serde_json::from_str(&meta).unwrap_or_default();
            out.push(TranscriptRow { seq, role, content, compacted: compacted != 0, tool_calls: meta.tool_calls, tool_call_id: meta.tool_call_id });
        }
        Ok(out)
    }

    /// Copy a session's whole history into a new session. Approvals, files
    /// and sub-agent runs are not copied — only the conversation.
    pub fn fork(&self, session: &str, now: &Timestamp) -> Result<String> {
        if !self.exists(session)? {
            return Err(Error::NotFound(format!("session {session}")));
        }
        let title: String = self.db.conn().query_row("SELECT title FROM sessions WHERE id = ?1", params![session], |r| r.get(0)).db()?;
        let new_id = self.create(&format!("fork of {session}: {title}"), now)?;
        self.db.transaction(|c| {
            c.execute(
                "INSERT INTO session_messages(session_id, seq, role, content, meta, compacted, created_at)
                 SELECT ?1, seq, role, content, meta, compacted, created_at FROM session_messages WHERE session_id = ?2",
                params![new_id, session],
            )
            .db()?;
            Ok(())
        })?;
        Ok(new_id)
    }

    /// Delete a session and all its messages. Returns whether it existed.
    pub fn delete(&self, session: &str) -> Result<bool> {
        if !self.exists(session)? {
            return Ok(false);
        }
        let conn = self.db.conn();
        conn.execute(
            "DELETE FROM session_messages WHERE session_id = ?1",
            params![session],
        )
        .db()?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session])
            .db()?;
        Ok(true)
    }

    /// Truncate a session after `to_seq`. The cut must not leave a tool call
    /// without its result. Summaries removed by the cut un-hide the rows they
    /// covered. Files changed in the removed turns are *not* restored — use
    /// file checkpoints for that.
    pub fn rewind(&self, session: &str, to_seq: i64) -> Result<RewindReport> {
        let rows = self.transcript(session)?;
        let max = rows.last().map_or(0, |r| r.seq);
        if to_seq < 0 || to_seq > max {
            return Err(Error::Invalid(format!("seq 必须在 0..={max} 之间")));
        }
        let kept: Vec<&TranscriptRow> = rows.iter().filter(|r| r.seq <= to_seq && r.role != "summary").collect();
        if let Some((i, open)) = kept.iter().enumerate().rev().find(|(_, r)| r.role == "assistant" && !r.tool_calls.is_empty()) {
            let answered: Vec<&str> = kept[i + 1..].iter().filter_map(|r| r.tool_call_id.as_deref()).collect();
            if let Some(missing) = open.tool_calls.iter().find(|c| !answered.contains(&c.id.as_str())) {
                let last_answer = rows.iter().filter(|r| r.tool_call_id.is_some() && r.seq > open.seq).map(|r| r.seq).max();
                return Err(Error::Invalid(format!(
                    "在 seq {to_seq} 截断会留下没有结果的工具调用 {}（seq {} 发起）；请改用 --to {}{}",
                    missing.name,
                    open.seq,
                    open.seq - 1,
                    last_answer.map(|s| format!(" 或 --to {s}")).unwrap_or_default()
                )));
            }
        }
        self.db.transaction(|c| {
            let removed = c.execute("DELETE FROM session_messages WHERE session_id = ?1 AND seq > ?2", params![session, to_seq]).db()?;
            let last_summary: i64 = c
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM session_messages WHERE session_id = ?1 AND role = 'summary'",
                    params![session],
                    |r| r.get(0),
                )
                .db()?;
            let uncompacted = c
                .execute(
                    "UPDATE session_messages SET compacted = 0 WHERE session_id = ?1 AND seq > ?2 AND compacted = 1 AND role != 'summary'",
                    params![session, last_summary],
                )
                .db()?;
            Ok(RewindReport { removed, uncompacted })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_active_compact() {
        let db = Db::open_in_memory().unwrap();
        let s = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let id = s.create("t", &now).unwrap();
        s.append(&id, &ChatMessage::user("a"), Trust::User, &now).unwrap();
        let call = ToolCall { id: "c1".into(), name: "mail_search".into(), arguments: serde_json::json!({}) };
        s.append(&id, &ChatMessage::assistant_tools("", vec![call]), Trust::System, &now).unwrap();
        s.append(&id, &ChatMessage::tool("c1", "mail"), Trust::Untrusted, &now).unwrap();
        let seq = s.append(&id, &ChatMessage::user("b"), Trust::User, &now).unwrap();
        let (sum, msgs) = s.active(&id).unwrap();
        assert!(sum.is_none());
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].message.tool_calls[0].id, "c1");
        assert_eq!(msgs[2].trust, Trust::Untrusted);
        s.compact(&id, seq - 1, "earlier: user asked a", &now).unwrap();
        let (sum, msgs) = s.active(&id).unwrap();
        assert_eq!(sum.as_deref(), Some("earlier: user asked a"));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message.content, "b");
        assert_eq!(s.latest().unwrap().as_deref(), Some(id.as_str()));
        assert!(s.append("nope", &ChatMessage::user("x"), Trust::User, &now).is_err());
    }
    #[test]
    fn fork_and_rewind_keep_transcripts_well_formed() {
        let db = Db::open_in_memory().unwrap();
        let s = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let id = s.create("t", &now).unwrap();
        s.append(&id, &ChatMessage::user("one"), Trust::User, &now).unwrap(); // 1
        s.append(&id, &ChatMessage::assistant("ok one"), Trust::System, &now).unwrap(); // 2
        s.compact(&id, 2, "summary of one", &now).unwrap(); // 3 = summary
        s.append(&id, &ChatMessage::user("two"), Trust::User, &now).unwrap(); // 4
        let call = ToolCall { id: "c1".into(), name: "fs_read".into(), arguments: serde_json::json!({}) };
        s.append(&id, &ChatMessage::assistant_tools("", vec![call]), Trust::System, &now).unwrap(); // 5
        s.append(&id, &ChatMessage::tool("c1", "data"), Trust::Untrusted, &now).unwrap(); // 6
        s.append(&id, &ChatMessage::assistant("done two"), Trust::System, &now).unwrap(); // 7

        let f = s.fork(&id, &now).unwrap();
        assert_eq!(s.active(&f).unwrap(), s.active(&id).unwrap());
        assert_eq!(s.transcript(&f).unwrap().len(), 7);

        let e = s.rewind(&id, 5).unwrap_err().to_string();
        assert!(e.contains("fs_read") && e.contains("--to 4") && e.contains("--to 6"), "{e}");
        assert_eq!(s.rewind(&id, 6).unwrap(), RewindReport { removed: 1, uncompacted: 0 });
        // Cutting before the summary brings the compacted rows back.
        assert_eq!(s.rewind(&id, 2).unwrap(), RewindReport { removed: 4, uncompacted: 2 });
        let (sum, msgs) = s.active(&id).unwrap();
        assert!(sum.is_none());
        assert_eq!(msgs.iter().map(|m| m.message.content.as_str()).collect::<Vec<_>>(), vec!["one", "ok one"]);
        assert!(s.rewind(&id, 99).is_err());
        // The fork is untouched.
        assert_eq!(s.transcript(&f).unwrap().len(), 7);
        assert_eq!(s.list(false, 10).unwrap().len(), 2);
        s.create("subagent:sa_x", &now).unwrap();
        assert_eq!(s.list(false, 10).unwrap().len(), 2);
        assert_eq!(s.list(true, 10).unwrap().len(), 3);
    }

    #[test]
    fn delete_removes_session_and_messages() {
        let db = Db::open_in_memory().unwrap();
        let s = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let id = s.create("t", &now).unwrap();
        s.append(&id, &ChatMessage::user("a"), Trust::User, &now).unwrap();
        assert!(s.delete(&id).unwrap());
        assert!(!s.exists(&id).unwrap());
        assert_eq!(s.list(false, 10).unwrap().len(), 0);
        assert!(!s.delete(&id).unwrap(), "second delete reports absent");
    }
}
