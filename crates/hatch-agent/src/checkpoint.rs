//! Checkpoints: a session fork + a file snapshot, taken together.
//!
//! A checkpoint records where the conversation was (the session id and
//! the last transcript `seq` at the time) and what the files looked like
//! (a [`SnapshotStore`] snapshot id), plus a fork of the whole session —
//! the fork is the safety copy, so the pre-checkpoint conversation
//! survives even though restoring rewinds the original session.
//!
//! "回到检查点" is [`restore_checkpoint`]: rewind the session to the
//! recorded seq, then restore the files. Order matters: the rewind runs
//! first, so a rewind that refuses (its own guard — it never leaves a
//! tool call without its result) aborts the restore before any file is
//! touched. The restore itself first forks the session again (the
//! pre-restore fork) and snapshots the current files (the snapshot
//! store's pre-restore snapshot), so a restore can be undone on both
//! sides. A restore is not atomic across the two stores: if the file
//! restore fails after the rewind, the session is already rewound — the
//! pre-restore fork and the pre-restore snapshot are the undo.
//!
//! Checkpoint manifests live under the snapshot root,
//! `<root>/checkpoints/<id>.json` (same trust level as the snapshot
//! manifests: local state under `HATCH_HOME`, not validated beyond
//! shape — do not load checkpoint directories from untrusted sources).

use crate::session::{RewindReport, SessionStore};
use hatch_core::ids;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_snapshots::{valid_id, RestoreReport, SnapshotStore};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// One checkpoint: where the conversation was + what the files were.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    /// Last transcript seq at checkpoint time (0 = empty session).
    pub seq: i64,
    /// Fork of the whole session at checkpoint time (the safety copy).
    pub fork_id: String,
    pub snapshot_id: String,
    pub created_at: String,
    pub label: String,
}

/// What one [`restore_checkpoint`] did.
#[derive(Clone, Debug)]
pub struct CheckpointRestoreReport {
    pub checkpoint_id: String,
    /// Fork taken just before the rewind (undoes the session half).
    pub pre_restore_fork_id: String,
    pub rewind: RewindReport,
    /// The file half; its own `pre_restore_id` undoes the file half.
    pub restore: RestoreReport,
}

fn checkpoints_dir(snapshots: &SnapshotStore) -> PathBuf {
    snapshots.root().join("checkpoints")
}

fn manifest_path(snapshots: &SnapshotStore, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        return Err(Error::Invalid(format!("检查点 id `{id}` 不合法")));
    }
    Ok(checkpoints_dir(snapshots).join(format!("{id}.json")))
}

/// Take a checkpoint: fork the session, snapshot the files, record both.
/// The fork happens first; if the file snapshot then fails, the fork
/// stays behind as an ordinary forked session (a valid backup copy, not
/// corruption) and the checkpoint is not recorded.
pub fn create_checkpoint(
    db: &hatch_store::Db,
    snapshots: &SnapshotStore,
    session_id: &str,
    files: &[PathBuf],
    label: &str,
    now: &Timestamp,
) -> Result<Checkpoint> {
    let sessions = SessionStore::new(db);
    let fork_id = sessions.fork(session_id, now)?;
    let seq = sessions
        .transcript(session_id)?
        .last()
        .map_or(0, |r| r.seq);
    let snapshot = snapshots.create(files, &format!("checkpoint {label}"), now)?;
    let checkpoint = Checkpoint {
        id: ids::random_id("chk"),
        session_id: session_id.to_string(),
        seq,
        fork_id,
        snapshot_id: snapshot.id,
        created_at: to_rfc3339(now),
        label: label.to_string(),
    };
    let dir = checkpoints_dir(snapshots);
    fs::create_dir_all(&dir).map_err(|e| Error::Storage(format!("创建检查点目录失败：{e}")))?;
    let raw = serde_json::to_string_pretty(&checkpoint)
        .map_err(|e| Error::Storage(format!("序列化检查点失败：{e}")))?;
    fs::write(manifest_path(snapshots, &checkpoint.id)?, raw)
        .map_err(|e| Error::Storage(format!("写入检查点失败：{e}")))?;
    Ok(checkpoint)
}

/// Load one checkpoint's manifest.
pub fn get_checkpoint(snapshots: &SnapshotStore, id: &str) -> Result<Checkpoint> {
    let p = manifest_path(snapshots, id)?;
    let raw = fs::read_to_string(&p)
        .map_err(|e| Error::NotFound(format!("检查点 {id} 不存在（{}：{e}）", p.display())))?;
    serde_json::from_str(&raw).map_err(|e| Error::Invalid(format!("检查点 {id} 的 manifest 已损坏：{e}")))
}

/// All checkpoints, newest first by creation time.
pub fn list_checkpoints(snapshots: &SnapshotStore) -> Result<Vec<Checkpoint>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(checkpoints_dir(snapshots)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::Storage(format!("列出检查点失败：{e}"))),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some(id) = name.strip_suffix(".json") else { continue };
        if let Ok(c) = get_checkpoint(snapshots, id) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// Return to a checkpoint: rewind the session to the recorded seq, then
/// restore the files. Rewind first — its guard aborts the restore before
/// any file is touched. Before the rewind, the session is forked again
/// (the pre-restore fork); the file restore takes its own pre-restore
/// snapshot, so both halves can be undone.
pub fn restore_checkpoint(
    db: &hatch_store::Db,
    snapshots: &SnapshotStore,
    checkpoint_id: &str,
    now: &Timestamp,
) -> Result<CheckpointRestoreReport> {
    let checkpoint = get_checkpoint(snapshots, checkpoint_id)?;
    let sessions = SessionStore::new(db);
    let pre_restore_fork_id = sessions.fork(&checkpoint.session_id, now)?;
    let rewind = sessions.rewind(&checkpoint.session_id, checkpoint.seq)?;
    let restore = snapshots.restore(&checkpoint.snapshot_id, now)?;
    Ok(CheckpointRestoreReport {
        checkpoint_id: checkpoint_id.to_string(),
        pre_restore_fork_id,
        rewind,
        restore,
    })
}
