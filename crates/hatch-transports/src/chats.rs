//! Per-chat state under `$HATCH_HOME/chats/<chat-id>/`.
//!
//! Two files per chat:
//!
//! * `DISCRETION.md` — the sharing rules ([`DiscretionRules`](crate::discretion::DiscretionRules)).
//!   Missing means "no extra restrictions" (secrets are still blocked).
//! * `session.json` — `{"session_id": "ses_…"}`: the chat's own
//!   session/transcript. The mapping is resolved through
//!   [`ChatStore::resolve_session`], which creates the session on first
//!   use with the title `transport:<transport>:<chat-id>` and reuses it
//!   afterwards. A message from chat A therefore lands in A's session
//!   only — never in B's context.
//!
//! Drafts a worker prepared for the main agent wait in
//! `drafts/<draft-id>.json` (see [`Relay`](crate::relay::Relay)).

use crate::discretion::{extract_mentioned_rules, DiscretionRules};
use crate::message::{check_chat_id, valid_chat_id};
use hatch_agent::SessionStore;
use hatch_core::{Error, Result};
use hatch_store::Db;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct SessionFile {
    session_id: String,
}

/// Per-chat files under `$HATCH_HOME/chats`.
#[derive(Clone, Debug)]
pub struct ChatStore {
    root: PathBuf,
}

impl ChatStore {
    pub fn new(home: &Path) -> Self {
        ChatStore { root: home.join("chats") }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The chat's directory, after validating the id (no traversal).
    pub fn chat_dir(&self, chat_id: &str) -> Result<PathBuf> {
        check_chat_id(chat_id)?;
        Ok(self.root.join(chat_id))
    }

    pub fn discretion_path(&self, chat_id: &str) -> Result<PathBuf> {
        Ok(self.chat_dir(chat_id)?.join("DISCRETION.md"))
    }

    /// Read the chat's rules. A missing file is the open default; a
    /// file that fails to parse is an error (fail closed — the caller
    /// must block outbound, not guess).
    pub fn read_discretion(&self, chat_id: &str) -> Result<DiscretionRules> {
        let path = self.discretion_path(chat_id)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => DiscretionRules::parse(&text).map_err(|e| {
                Error::Invalid(format!("{} 无法解析：{e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DiscretionRules::open()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Write the chat's rules, after parsing them (a bad file is never
    /// written). Returns the parsed rules.
    pub fn write_discretion(&self, chat_id: &str, text: &str) -> Result<DiscretionRules> {
        let rules = DiscretionRules::parse(text)?;
        let dir = self.chat_dir(chat_id)?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(self.discretion_path(chat_id)?, rules.render())?;
        Ok(rules)
    }

    /// Record sharing rules the user stated casually in a message
    /// (Muse §9.4: mentioned rules become the chat's long-term rules).
    /// Returns the kinds newly added (empty when nothing was mentioned
    /// or everything was already recorded). The file is only rewritten
    /// when something new was found.
    pub fn record_mentioned(&self, chat_id: &str, text: &str) -> Result<Vec<crate::message::ContentKind>> {
        let mentioned = extract_mentioned_rules(text);
        if mentioned.is_empty() {
            return Ok(Vec::new());
        }
        let mut rules = self.read_discretion(chat_id)?;
        let mut added = Vec::new();
        for kind in mentioned {
            if !rules.forbidden.contains(&kind) {
                rules.forbidden.push(kind);
                added.push(kind);
            }
        }
        if !added.is_empty() {
            let dir = self.chat_dir(chat_id)?;
            std::fs::create_dir_all(&dir)?;
            std::fs::write(self.discretion_path(chat_id)?, rules.render())?;
        }
        Ok(added)
    }

    /// Chat ids that have any state (a session file, a discretion file,
    /// or a drafts directory).
    pub fn list_chats(&self) -> Result<Vec<String>> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if valid_chat_id(&name) && e.path().is_dir() {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// The chat's session id, creating the session on first use. A
    /// mapping that points at a session that no longer exists is
    /// repaired by creating a fresh one.
    pub fn resolve_session(&self, db: &Db, chat_id: &str, transport: &str) -> Result<String> {
        let dir = self.chat_dir(chat_id)?;
        let file = dir.join("session.json");
        let sessions = SessionStore::new(db);
        if let Ok(raw) = std::fs::read_to_string(&file) {
            if let Ok(saved) = serde_json::from_str::<SessionFile>(&raw) {
                if sessions.exists(&saved.session_id)? {
                    return Ok(saved.session_id);
                }
            }
        }
        let now = hatch_core::time::now();
        let id = sessions.create(&format!("transport:{transport}:{chat_id}"), &now)?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, serde_json::to_string(&SessionFile { session_id: id.clone() })?)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discretion_round_trips_and_missing_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::new(dir.path());
        assert!(store.read_discretion("family").unwrap().forbidden.is_empty());
        let rules = store.write_discretion("family", "forbid: mail\nforbid: files\n").unwrap();
        assert_eq!(rules.forbidden.len(), 2);
        assert_eq!(store.read_discretion("family").unwrap(), rules);
        assert!(store.write_discretion("family", "forbid: nope\n").is_err());
        // The bad write left the previous file untouched.
        assert_eq!(store.read_discretion("family").unwrap(), rules);
        assert!(store.discretion_path("../evil").is_err());
        assert_eq!(store.list_chats().unwrap(), vec!["family"]);
    }

    #[test]
    fn resolve_session_is_stable_and_repairs_a_dead_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::new(dir.path());
        let db = Db::open_in_memory().unwrap();
        let a = store.resolve_session(&db, "family", "pipe").unwrap();
        let b = store.resolve_session(&db, "family", "pipe").unwrap();
        assert_eq!(a, b, "same chat resolves to the same session");
        let other = store.resolve_session(&db, "work", "pipe").unwrap();
        assert_ne!(a, other, "different chats get different sessions");
        // Corrupt the mapping: a fresh session is created, not a crash.
        std::fs::write(store.chat_dir("family").unwrap().join("session.json"), "{\"session_id\": \"ses_gone\"}").unwrap();
        let c = store.resolve_session(&db, "family", "pipe").unwrap();
        assert_ne!(c, a);
        assert!(SessionStore::new(&db).exists(&c).unwrap());
    }

    #[test]
    fn per_chat_sessions_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::new(dir.path());
        let db = Db::open_in_memory().unwrap();
        let now = hatch_core::time::now();
        let a = store.resolve_session(&db, "chat-a", "pipe").unwrap();
        let b = store.resolve_session(&db, "chat-b", "pipe").unwrap();
        let sessions = SessionStore::new(&db);
        sessions.append(&a, &hatch_llm::ChatMessage::user("A 的秘密计划"), hatch_core::Trust::User, &now).unwrap();
        let (_, a_msgs) = sessions.active(&a).unwrap();
        let (_, b_msgs) = sessions.active(&b).unwrap();
        assert_eq!(a_msgs.len(), 1);
        assert!(b_msgs.is_empty(), "chat B must never see chat A's messages");
        assert!(!b_msgs.iter().any(|m| m.message.content.contains("秘密计划")));
    }

    #[test]
    fn main_and_side_chats_share_memory_but_not_transcripts() {
        // Muse §9.4: the main chat and side chats share memory and
        // goals, but each has its own transcript. Memory lives in the
        // same database for every session; transcripts are per session.
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::new(dir.path());
        let db = Db::open_in_memory().unwrap();
        let now = hatch_core::time::now();
        let main = store.resolve_session(&db, "main", "pipe").unwrap();
        let side = store.resolve_session(&db, "family", "pipe").unwrap();
        // A claim recorded from the main chat …
        let mem = hatch_memory::MemoryStore::new(&db);
        mem.add(
            &hatch_memory::NewClaim::stated("preference:tea", "用户喜欢喝绿茶", "test:main").with_tags(&["preference"]),
            &now,
        )
        .unwrap();
        // … is retrievable from the side chat's side (same database),
        // while the transcripts stay separate.
        let found = hatch_memory::MemoryStore::new(&db).search("绿茶", 5, &now).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.text, "用户喜欢喝绿茶");
        let sessions = SessionStore::new(&db);
        sessions.append(&main, &hatch_llm::ChatMessage::user("主聊天的消息"), hatch_core::Trust::User, &now).unwrap();
        let (_, side_msgs) = sessions.active(&side).unwrap();
        assert!(side_msgs.is_empty(), "the side chat's transcript is its own");
    }

    #[test]
    fn mentioned_rules_are_recorded_as_long_term_rules() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::new(dir.path());
        let added = store.record_mentioned("family", "以后不要把邮件发到这个群").unwrap();
        assert_eq!(added, vec![crate::message::ContentKind::Mail]);
        assert!(store.read_discretion("family").unwrap().forbids(crate::message::ContentKind::Mail));
        // Saying it again adds nothing.
        assert!(store.record_mentioned("family", "不要把邮件发到这里").unwrap().is_empty());
        assert!(store.record_mentioned("family", "今天天气不错").unwrap().is_empty());
    }
}
