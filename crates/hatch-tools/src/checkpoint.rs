//! File checkpoints: a snapshot of every workspace file right before hatch
//! changes it, so any write can be rewound.

use crate::workspace::Workspace;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Largest single file we snapshot (and therefore are willing to modify).
pub const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;
/// Total snapshot storage; the oldest checkpoints are pruned beyond this.
pub const MAX_TOTAL_BYTES: i64 = 200 * 1024 * 1024;
/// At most this many checkpoints are kept.
pub const MAX_CHECKPOINTS: i64 = 2000;

pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub root: String,
    pub path: String,
    pub existed: bool,
    pub hash: String,
    pub size: i64,
    pub origin: String,
    pub approval_id: String,
    pub created_at: Timestamp,
}

pub struct CheckpointStore<'a> {
    db: &'a Db,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Checkpoint> {
    let ts: String = r.get(8)?;
    Ok(Checkpoint {
        id: r.get(0)?,
        root: r.get(1)?,
        path: r.get(2)?,
        existed: r.get::<_, i64>(3)? != 0,
        hash: r.get(4)?,
        size: r.get(5)?,
        origin: r.get(6)?,
        approval_id: r.get(7)?,
        created_at: parse_rfc3339(&ts).unwrap_or_default(),
    })
}

const COLS: &str = "id, root, path, existed, hash, size, origin, approval_id, created_at";

impl<'a> CheckpointStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        CheckpointStore { db }
    }

    /// Snapshot `rel` (which may not exist yet) before it is modified.
    pub fn snapshot(&self, ws: &Workspace, rel: &str, origin: &str, approval_id: &str, now: &Timestamp) -> Result<String> {
        let current = ws.read_bytes(rel, MAX_SNAPSHOT_BYTES).map_err(|e| match e {
            Error::Invalid(m) if m.contains("larger than") => {
                Error::Denied(format!("{rel} 超过 {} MiB，无法建立检查点，拒绝修改", MAX_SNAPSHOT_BYTES / 1024 / 1024))
            }
            other => other,
        })?;
        let id = ids::random_id("cp");
        let (existed, hash, size) = match &current {
            Some(b) => (1, content_hash(b), b.len() as i64),
            None => (0, String::new(), 0),
        };
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO checkpoints({COLS}, content) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
                params![id, ws.root().display().to_string(), rel, existed, hash, size, origin, approval_id, to_rfc3339(now), current],
            )
            .db()?;
        self.prune()?;
        Ok(id)
    }

    fn prune(&self) -> Result<()> {
        let c = self.db.conn();
        c.execute(
            "DELETE FROM checkpoints WHERE id IN (SELECT id FROM checkpoints ORDER BY created_at DESC, rowid DESC LIMIT -1 OFFSET ?1)",
            params![MAX_CHECKPOINTS],
        )
        .db()?;
        loop {
            let total: i64 = c.query_row("SELECT COALESCE(SUM(size), 0) FROM checkpoints", [], |r| r.get(0)).db()?;
            if total <= MAX_TOTAL_BYTES {
                return Ok(());
            }
            c.execute("DELETE FROM checkpoints WHERE id = (SELECT id FROM checkpoints ORDER BY created_at, rowid LIMIT 1)", []).db()?;
        }
    }

    pub fn list(&self, limit: usize) -> Result<Vec<Checkpoint>> {
        let c = self.db.conn();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM checkpoints ORDER BY created_at DESC, rowid DESC LIMIT ?1")).db()?;
        let rows = st.query_map(params![limit as i64], row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn get(&self, id: &str) -> Result<(Checkpoint, Option<Vec<u8>>)> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS}, content FROM checkpoints WHERE id = ?1"), params![id], |r| Ok((row(r)?, r.get(9)?)))
            .map_err(|_| Error::NotFound(format!("checkpoint {id}")))
    }

    /// Restore the file to its state at checkpoint `id`. The current state is
    /// snapshotted first, so a rewind can itself be rewound.
    pub fn rewind(&self, id: &str, now: &Timestamp) -> Result<String> {
        let (cp, content) = self.get(id)?;
        let ws = Workspace::new(Path::new(&cp.root))?;
        let undo = self.snapshot(&ws, &cp.path, "rewind", id, now)?;
        let what = if cp.existed {
            let bytes = content.ok_or_else(|| Error::Storage(format!("checkpoint {id} has no content")))?;
            if content_hash(&bytes) != cp.hash {
                return Err(Error::Storage(format!("checkpoint {id} is corrupted (hash mismatch)")));
            }
            ws.write_atomic(&cp.path, &bytes)?;
            format!("{} 已恢复到 {} 时的内容", cp.path, to_rfc3339(&cp.created_at))
        } else {
            ws.remove_file(&cp.path)?;
            format!("{} 在检查点时不存在，已删除", cp.path)
        };
        self.db.audit("user", "checkpoint_rewind", &format!("{id} {} (undo: {undo})", cp.path))?;
        Ok(format!("{what}（撤销此次回退可用检查点 {undo}）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_rewind_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::new(d.path()).unwrap();
        let now = hatch_core::time::now();
        let store = CheckpointStore::new(&db);
        ws.write_atomic("a.txt", b"v1").unwrap();
        let cp1 = store.snapshot(&ws, "a.txt", "chat:s", "ap1", &now).unwrap();
        ws.write_atomic("a.txt", b"v2").unwrap();
        let cp_new = store.snapshot(&ws, "new.txt", "chat:s", "ap2", &now).unwrap();
        ws.write_atomic("new.txt", b"fresh").unwrap();

        assert!(store.rewind(&cp1, &now).unwrap().contains("已恢复"));
        assert_eq!(std::fs::read(d.path().join("a.txt")).unwrap(), b"v1");
        store.rewind(&cp_new, &now).unwrap();
        assert!(!d.path().join("new.txt").exists());
        // Rewinds are checkpointed too: 2 originals + 2 undo snapshots.
        assert_eq!(store.list(10).unwrap().len(), 4);
        let undo = store.list(10).unwrap().into_iter().find(|c| c.origin == "rewind" && c.path == "a.txt").unwrap();
        store.rewind(&undo.id, &now).unwrap();
        assert_eq!(std::fs::read(d.path().join("a.txt")).unwrap(), b"v2");
        assert!(store.rewind("cp_missing", &now).is_err());
    }
}
