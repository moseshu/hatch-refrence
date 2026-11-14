//! Canary rollout for one release (task 9).
//!
//! A rollout moves a release through staged percentages —
//! typically 10, then 50, then 100. Each task is assigned to the
//! canary deterministically from its id, so the assignment is
//! sticky without storing anything per task: the same task id
//! always lands in the same bucket, and a re-run of the same task
//! does not flip it between the canary and the stable release.
//!
//! The kill switch stops *new* canary assignments. Tasks already
//! assigned to the canary keep running — killing a rollout must
//! never strand an in-flight task halfway; it only stops the
//! blast radius from growing. A killed rollout can be re-created
//! for the same release (the old row is replaced).

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use rusqlite::Row;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One canary rollout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rollout {
    pub release_id: String,
    /// Staged percentages, e.g. [10, 50, 100].
    pub stages: Vec<u8>,
    /// Index into `stages`: the current stage.
    pub stage_idx: usize,
    /// Kill switch: when set, no new task is assigned to the
    /// canary.
    pub killed: bool,
    pub created_at: String,
}

impl Rollout {
    /// The percentage of tasks in the canary right now (0 when
    /// killed).
    pub fn current_percent(&self) -> u8 {
        if self.killed {
            return 0;
        }
        self.stages.get(self.stage_idx).copied().unwrap_or(100)
    }
}

/// Deterministic canary assignment for one task id: the first 8
/// bytes of sha256(task id) as a bucket in [0, 100).
pub fn bucket(task_id: &str) -> u64 {
    let hash = Sha256::digest(task_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash[..8]);
    u64::from_be_bytes(bytes) % 100
}

/// Is this task in the canary under this rollout?
pub fn assign(task_id: &str, rollout: &Rollout) -> bool {
    bucket(task_id) < u64::from(rollout.current_percent())
}

const COLS: &str = "release_id, stages_json, stage_idx, killed, created_at";

fn row(r: &Row) -> rusqlite::Result<Rollout> {
    let stages_json: String = r.get(1)?;
    let stages: Vec<u8> = serde_json::from_str(&stages_json).unwrap_or(vec![100]);
    Ok(Rollout {
        release_id: r.get(0)?,
        stages,
        stage_idx: r.get::<_, i64>(2)? as usize,
        killed: r.get::<_, i64>(3)? != 0,
        created_at: r.get(4)?,
    })
}

pub struct RolloutStore<'a> {
    db: &'a Db,
}

impl<'a> RolloutStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        RolloutStore { db }
    }

    /// Create (or replace) the rollout for a release. Stages must
    /// be non-empty, each in 1..=100, strictly increasing — a
    /// rollout that goes backwards is a typo, not a policy.
    pub fn create(&self, release_id: &str, stages: &[u8], now: &Timestamp) -> Result<Rollout> {
        if stages.is_empty() {
            return Err(Error::Invalid("灰度阶段不能为空".into()));
        }
        if stages.iter().any(|p| *p == 0 || *p > 100) {
            return Err(Error::Invalid("灰度百分比须在 1..=100".into()));
        }
        if stages.windows(2).any(|w| w[0] >= w[1]) {
            return Err(Error::Invalid("灰度阶段须严格递增".into()));
        }
        let ts = to_rfc3339(now);
        let json = serde_json::to_string(stages)
            .map_err(|e| Error::Storage(format!("序列化灰度阶段失败：{e}")))?;
        let c = self.db.conn();
        c.execute(
            &format!(
                "INSERT INTO rollouts({COLS}) VALUES(?1,?2,0,0,?3) \
                 ON CONFLICT(release_id) DO UPDATE SET stages_json = excluded.stages_json, stage_idx = 0, killed = 0, created_at = excluded.created_at"
            ),
            params![release_id, json, ts],
        )
        .db()?;
        Ok(Rollout {
            release_id: release_id.into(),
            stages: stages.to_vec(),
            stage_idx: 0,
            killed: false,
            created_at: ts,
        })
    }

    pub fn get(&self, release_id: &str) -> Result<Option<Rollout>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                &format!("SELECT {COLS} FROM rollouts WHERE release_id = ?1"),
                params![release_id],
                row,
            )
            .optional()
            .db()?;
        Ok(row)
    }

    /// Move to the next stage. Fails at the last stage — a rollout
    /// is finished at 100%, it does not wrap around.
    pub fn advance(&self, release_id: &str) -> Result<Rollout> {
        let cur = self
            .get(release_id)?
            .ok_or_else(|| Error::NotFound(format!("release {release_id} 没有灰度")))?;
        if cur.stage_idx + 1 >= cur.stages.len() {
            return Err(Error::Invalid("已到最后一个灰度阶段".into()));
        }
        let c = self.db.conn();
        c.execute(
            "UPDATE rollouts SET stage_idx = stage_idx + 1 WHERE release_id = ?1",
            params![release_id],
        )
        .db()?;
        let mut next = cur;
        next.stage_idx += 1;
        Ok(next)
    }

    /// Kill switch: stop new canary assignments for this release.
    pub fn kill(&self, release_id: &str) -> Result<Rollout> {
        let cur = self
            .get(release_id)?
            .ok_or_else(|| Error::NotFound(format!("release {release_id} 没有灰度")))?;
        let c = self.db.conn();
        c.execute(
            "UPDATE rollouts SET killed = 1 WHERE release_id = ?1",
            params![release_id],
        )
        .db()?;
        let mut next = cur;
        next.killed = true;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    #[test]
    fn assignment_is_deterministic_and_sticky() {
        let r = Rollout {
            release_id: "rel_x".into(),
            stages: vec![10, 50, 100],
            stage_idx: 0,
            killed: false,
            created_at: String::new(),
        };
        // Same task id, same answer, every time.
        assert_eq!(assign("tsk_a", &r), assign("tsk_a", &r));
        // At 100% everything is in; killed, nothing is in.
        let mut full = r.clone();
        full.stage_idx = 2;
        assert!(assign("tsk_a", &full));
        let mut dead = r.clone();
        dead.killed = true;
        assert!(!assign("tsk_a", &dead));
        // At 10%, a spread of ids splits both ways.
        let ids: Vec<String> = (0..200).map(|i| format!("tsk_{i}")).collect();
        let inside = ids.iter().filter(|id| assign(id, &r)).count();
        assert!(inside > 0 && inside < 200, "10% split: {inside}/200");
    }

    #[test]
    fn stage_validation_and_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let store = RolloutStore::new(&db);
        assert!(store.create("rel_x", &[], &now()).is_err());
        assert!(store.create("rel_x", &[0], &now()).is_err());
        assert!(store.create("rel_x", &[50, 10], &now()).is_err());
        assert!(store.create("rel_x", &[10, 10], &now()).is_err());
        let r = store.create("rel_x", &[10, 50, 100], &now()).unwrap();
        assert_eq!(r.current_percent(), 10);
        let r = store.advance("rel_x").unwrap();
        assert_eq!(r.current_percent(), 50);
        let r = store.advance("rel_x").unwrap();
        assert_eq!(r.current_percent(), 100);
        assert!(store.advance("rel_x").is_err());
        let r = store.kill("rel_x").unwrap();
        assert_eq!(r.current_percent(), 0);
        assert!(!assign("tsk_a", &r));
        // Re-creating a rollout resets it.
        let r = store.create("rel_x", &[50, 100], &now()).unwrap();
        assert_eq!(r.current_percent(), 50);
        assert!(!r.killed);
    }
}
