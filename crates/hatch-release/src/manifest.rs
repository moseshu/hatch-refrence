//! Versioned releases (task 9).
//!
//! A release pins one versioned set of the three things that
//! change a run's behaviour without a code change:
//!
//! * prompts — the hash of the compiled-in templates
//!   (`behavior_en`, `triage_classify_zh`, `triage_draft_zh`,
//!   `brief_zh`). A prompt edit changes the binary, which changes
//!   the hash, which is a new release — never a silent drift.
//! * skills — name -> version, from the skill registry. Skills
//!   carry their own `version` in frontmatter.
//! * the execution policy version — `hatch-policy`'s
//!   `POLICY_VERSION`, bumped when a gate rule changes.
//!
//! Exactly one release is active. A rollback is an activation of
//! the previous release: the ledger (tasks, receipts, approvals)
//! is untouched, so what was recorded under the rolled-back
//! release stays queryable.

use hatch_core::ids;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use rusqlite::Row;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Hash a set of named texts (prompt name -> template text).
/// Names are sorted first, so the hash is stable regardless of
/// the order the caller lists them in.
pub fn prompt_sha256(parts: &[(&str, &str)]) -> String {
    let mut sorted: Vec<(&str, &str)> = parts.to_vec();
    sorted.sort_by_key(|(name, _)| *name);
    let mut h = Sha256::new();
    for (name, text) in sorted {
        h.update(name.as_bytes());
        h.update([0u8]);
        h.update(text.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

/// One pinned release.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    /// sha256 of the compiled-in prompt templates.
    pub prompt_hash: String,
    /// Skill name -> version, as JSON.
    pub skill_versions_json: String,
    /// `hatch-policy` POLICY_VERSION at release time.
    pub policy_version: String,
    pub note: String,
    pub active: bool,
    pub created_at: String,
}

impl Release {
    /// Skill versions as a sorted map.
    pub fn skill_versions(&self) -> BTreeMap<String, String> {
        serde_json::from_str(&self.skill_versions_json).unwrap_or_default()
    }
}

const COLS: &str = "id, prompt_hash, skill_versions_json, policy_version, note, active, created_at";

fn row(r: &Row) -> rusqlite::Result<Release> {
    Ok(Release {
        id: r.get(0)?,
        prompt_hash: r.get(1)?,
        skill_versions_json: r.get(2)?,
        policy_version: r.get(3)?,
        note: r.get(4)?,
        active: r.get::<_, i64>(5)? != 0,
        created_at: r.get(6)?,
    })
}

pub struct ReleaseStore<'a> {
    db: &'a Db,
}

impl<'a> ReleaseStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        ReleaseStore { db }
    }

    /// Create a release (inactive). The caller passes the
    /// already-computed prompt hash, the skill versions it scanned,
    /// and the policy version.
    pub fn create(
        &self,
        prompt_hash: &str,
        skill_versions: &BTreeMap<String, String>,
        policy_version: &str,
        note: &str,
        now: &Timestamp,
    ) -> Result<Release> {
        let id = ids::random_id("rel");
        let ts = to_rfc3339(now);
        let skills = serde_json::to_string(skill_versions)
            .map_err(|e| Error::Storage(format!("序列化技能版本失败：{e}")))?;
        let c = self.db.conn();
        c.execute(
            &format!("INSERT INTO releases({COLS}) VALUES(?1,?2,?3,?4,?5,0,?6)"),
            params![id, prompt_hash, skills, policy_version, note, ts],
        )
        .db()?;
        Ok(Release {
            id,
            prompt_hash: prompt_hash.into(),
            skill_versions_json: skills,
            policy_version: policy_version.into(),
            note: note.into(),
            active: false,
            created_at: ts,
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Release>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                &format!("SELECT {COLS} FROM releases WHERE id = ?1"),
                params![id],
                row,
            )
            .optional()
            .db()?;
        Ok(row)
    }

    /// All releases, newest first.
    pub fn list(&self) -> Result<Vec<Release>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {COLS} FROM releases ORDER BY created_at DESC, id DESC"
            ))
            .db()?;
        let rows = st.query_map([], row).db()?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.db()?);
        }
        Ok(out)
    }

    /// The active release, if any.
    pub fn active(&self) -> Result<Option<Release>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                &format!("SELECT {COLS} FROM releases WHERE active != 0 ORDER BY created_at DESC, id DESC LIMIT 1"),
                [],
                row,
            )
            .optional()
            .db()?;
        Ok(row)
    }

    /// Activate one release; exactly one row stays active. The
    /// switch is one transaction: a crash cannot leave zero or
    /// two active releases.
    pub fn activate(&self, id: &str) -> Result<Release> {
        let target = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("release {id} 不存在")))?;
        self.db.transaction(|c| {
            c.execute("UPDATE releases SET active = 0", []).db()?;
            c.execute("UPDATE releases SET active = 1 WHERE id = ?1", params![id])
                .db()?;
            Ok(())
        })?;
        let mut target = target;
        target.active = true;
        Ok(target)
    }

    /// The most recent release that is not the active one — what a
    /// rollback activates. `None` when there is nothing to roll
    /// back to.
    pub fn previous(&self) -> Result<Option<Release>> {
        let active = self.active()?;
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {COLS} FROM releases ORDER BY created_at DESC, id DESC"
            ))
            .db()?;
        let rows = st.query_map([], row).db()?;
        for r in rows {
            let rel = r.db()?;
            if active.as_ref().map(|a| a.id.as_str()) != Some(rel.id.as_str()) {
                return Ok(Some(rel));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    fn skills() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("brief".to_string(), "1".to_string()),
            ("triage".to_string(), "2".to_string()),
        ])
    }

    #[test]
    fn prompt_hash_is_order_stable() {
        let a = prompt_sha256(&[("b", "two"), ("a", "one")]);
        let b = prompt_sha256(&[("a", "one"), ("b", "two")]);
        assert_eq!(a, b);
        let c = prompt_sha256(&[("a", "one"), ("b", "three")]);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn exactly_one_active_and_rollback_target() {
        let db = Db::open_in_memory().unwrap();
        let store = ReleaseStore::new(&db);
        let r1 = store
            .create("hash1", &skills(), "1", "first", &now())
            .unwrap();
        let r2 = store
            .create("hash2", &skills(), "1", "second", &now())
            .unwrap();
        assert!(store.active().unwrap().is_none());
        store.activate(&r1.id).unwrap();
        assert_eq!(store.active().unwrap().unwrap().id, r1.id);
        store.activate(&r2.id).unwrap();
        assert_eq!(store.active().unwrap().unwrap().id, r2.id);
        // Rollback target is the previous release.
        assert_eq!(store.previous().unwrap().unwrap().id, r1.id);
        store.activate(&r1.id).unwrap();
        assert_eq!(store.active().unwrap().unwrap().id, r1.id);
        // Activating an unknown release fails.
        assert!(store.activate("rel_missing").is_err());
    }
}
