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
        let mut st = self
            .db
            .conn()
            .prepare("SELECT seq, role, content, meta FROM session_messages WHERE session_id = ?1 AND compacted = 0 AND role != 'summary' ORDER BY seq")
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
}
