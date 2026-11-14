//! [`SnapshotStore`]: file snapshots on disk, next to the artifacts.
//!
//! One directory per snapshot, `<root>/<id>/manifest.json` plus
//! `<root>/<id>/files/<n>` (one stored copy per listed path, numbered by
//! position). A path that did not exist at snapshot time is recorded as
//! absent; restoring it removes the file if one was created since.
//!
//! Restoring is explicit and never automatic: [`SnapshotStore::restore`]
//! first snapshots the *current* state of the same paths, so a restore
//! can itself be undone by restoring the pre-restore snapshot. Restore is
//! not atomic across files — if one entry fails part-way, the earlier
//! entries are already restored and the error names the entry; the
//! pre-restore snapshot is the undo.

use crate::valid_id;
use hatch_core::ids;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn io_err(e: std::io::Error, what: &str) -> Error {
    Error::Storage(format!("{what}：{e}"))
}

/// One snapshotted path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    /// Absolute path as given at snapshot time.
    pub path: String,
    /// Stored copy under `files/`, by position (`None` when the path did
    /// not exist at snapshot time).
    pub stored: Option<String>,
    /// Whether the path existed at snapshot time.
    pub exists: bool,
    /// sha256 of the stored bytes (`None` when absent).
    pub sha256: Option<String>,
    /// Size of the stored bytes (0 when absent).
    pub size: u64,
}

/// A snapshot's manifest, stored next to its content files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub id: String,
    pub created_at: String,
    /// Where the snapshot came from: a task, a plan step
    /// (`plan:<id> step:<step id>`), a checkpoint, or a pre-restore.
    pub source: String,
    pub files: Vec<SnapshotFile>,
    /// Schema version the snapshot was written under (task 9).
    /// `None` on manifests written before versioning: they are
    /// plain file copies and restore like any older checkpoint.
    #[serde(default)]
    pub schema_version: Option<i64>,
    /// Execution policy version the snapshot was written under
    /// (task 9). `None` on pre-versioning manifests. A policy
    /// mismatch never blocks a restore — the checkpoint restores
    /// and runs under the current policy — but it is reported by
    /// the pre-release check, so a behaviour change is visible.
    #[serde(default)]
    pub policy_version: Option<String>,
}

/// What one [`SnapshotStore::restore`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    pub snapshot_id: String,
    /// Snapshot of the pre-restore state (same file list). Restoring it
    /// undoes this restore.
    pub pre_restore_id: String,
    /// Paths whose content was written back.
    pub restored: Vec<String>,
    /// Paths that were absent at snapshot time and removed by the restore.
    pub removed: Vec<String>,
}

/// Filesystem store for file snapshots.
#[derive(Clone, Debug)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: &Path) -> SnapshotStore {
        SnapshotStore { root: root.to_path_buf() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.dir(id).join("manifest.json")
    }

    fn check_id(&self, id: &str) -> Result<()> {
        if valid_id(id) {
            Ok(())
        } else {
            Err(Error::Invalid(format!("快照 id `{id}` 不合法")))
        }
    }

    fn load_manifest(&self, id: &str) -> Result<SnapshotManifest> {
        self.check_id(id)?;
        let p = self.manifest_path(id);
        let raw = fs::read_to_string(&p)
            .map_err(|e| Error::NotFound(format!("快照 {id} 不存在（{}：{e}）", p.display())))?;
        serde_json::from_str(&raw)
            .map_err(|e| Error::Invalid(format!("快照 {id} 的 manifest.json 已损坏：{e}")))
    }

    /// Snapshot the given absolute paths. A path that does not exist is
    /// recorded as absent (restoring removes it); a directory or a
    /// symlink is refused — this is a file snapshot, and a symlink is
    /// followed by no one (the workspace guard already refuses escapes
    /// for plan steps; the direct API refuses them here).
    pub fn create(&self, files: &[PathBuf], source: &str, now: &Timestamp) -> Result<SnapshotManifest> {
        let id = ids::random_id("snp");
        let dir = self.dir(&id);
        let files_dir = dir.join("files");
        if let Err(e) = self.create_snapshot(&id, &files_dir, files, source, now) {
            // Never leave a half-written snapshot behind: without a
            // manifest it would be invisible to list(), but its content
            // would still sit on disk.
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        self.load_manifest(&id)
    }

    fn create_snapshot(
        &self,
        id: &str,
        files_dir: &Path,
        files: &[PathBuf],
        source: &str,
        now: &Timestamp,
    ) -> Result<()> {
        fs::create_dir_all(files_dir).map_err(|e| io_err(e, "创建快照目录失败"))?;
        let mut recorded = Vec::with_capacity(files.len());
        for (n, path) in files.iter().enumerate() {
            let stored_name = n.to_string();
            let meta = match fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    recorded.push(SnapshotFile {
                        path: path.display().to_string(),
                        stored: None,
                        exists: false,
                        sha256: None,
                        size: 0,
                    });
                    continue;
                }
                Err(e) => return Err(io_err(e, &format!("读取 {} 失败", path.display()))),
            };
            if meta.file_type().is_symlink() {
                return Err(Error::Invalid(format!(
                    "拒绝快照符号链接 {}（先解析到真实路径再拍）",
                    path.display()
                )));
            }
            if meta.is_dir() {
                return Err(Error::Invalid(format!(
                    "快照只拍文件，{} 是目录",
                    path.display()
                )));
            }
            let bytes = fs::read(path).map_err(|e| io_err(e, &format!("读取 {} 失败", path.display())))?;
            let hash = hex::encode(Sha256::digest(&bytes));
            fs::write(files_dir.join(&stored_name), &bytes)
                .map_err(|e| io_err(e, "写入快照内容失败"))?;
            recorded.push(SnapshotFile {
                path: path.display().to_string(),
                stored: Some(stored_name),
                exists: true,
                sha256: Some(hash),
                size: bytes.len() as u64,
            });
        }
        let manifest = SnapshotManifest {
            id: id.to_string(),
            created_at: to_rfc3339(now),
            source: source.to_string(),
            files: recorded,
            // Task 9: stamp the versions the snapshot was written
            // under, so a later release can verify it restores.
            schema_version: Some(hatch_store::SCHEMA_VERSION),
            policy_version: Some(hatch_policy::POLICY_VERSION.to_string()),
        };
        let raw = serde_json::to_string_pretty(&manifest)
            .map_err(|e| Error::Storage(format!("序列化快照 manifest 失败：{e}")))?;
        fs::write(self.manifest_path(id), raw).map_err(|e| io_err(e, "写入快照 manifest 失败"))?;
        Ok(())
    }

    /// Load one snapshot's manifest. Unknown or malformed ids are
    /// rejected before touching the filesystem.
    pub fn get(&self, id: &str) -> Result<SnapshotManifest> {
        self.load_manifest(id)
    }

    /// All snapshots, newest first by creation time.
    pub fn list(&self) -> Result<Vec<SnapshotManifest>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(io_err(e, "列出快照失败")),
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !valid_id(&name) || !entry.path().is_dir() {
                continue;
            }
            if let Ok(m) = self.load_manifest(&name) {
                out.push(m);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    /// Restore a snapshot's files. First snapshots the current state of
    /// the same paths (returned as `pre_restore_id`), so this restore can
    /// itself be undone. A stored copy whose hash no longer matches the
    /// manifest aborts the restore — a corrupt snapshot must not
    /// overwrite good files. A target that is now a symlink or a
    /// directory is refused rather than followed or deleted.
    pub fn restore(&self, id: &str, now: &Timestamp) -> Result<RestoreReport> {
        let manifest = self.load_manifest(id)?;
        let paths: Vec<PathBuf> = manifest.files.iter().map(|f| PathBuf::from(&f.path)).collect();
        let pre = self.create(&paths, &format!("pre-restore of {id}"), now)?;
        let mut restored = Vec::new();
        let mut removed = Vec::new();
        for entry in &manifest.files {
            let target = Path::new(&entry.path);
            if entry.exists {
                let stored = entry.stored.as_deref().ok_or_else(|| {
                    Error::Invalid(format!("快照 {id} 的 manifest 缺少 {} 的存储名", entry.path))
                })?;
                let bytes = fs::read(self.dir(id).join("files").join(stored))
                    .map_err(|e| io_err(e, &format!("读取快照 {id} 的存储内容失败")))?;
                let hash = hex::encode(Sha256::digest(&bytes));
                if Some(hash.as_str()) != entry.sha256.as_deref() {
                    return Err(Error::Invalid(format!(
                        "快照 {id} 的存储内容已损坏（{} 的 hash 与 manifest 不符），已停止恢复",
                        entry.path
                    )));
                }
                self.check_restore_target(target)?;
                if let Some(parent) = target.parent() {
                    if !parent.as_os_str().is_empty() {
                        fs::create_dir_all(parent)
                            .map_err(|e| io_err(e, &format!("创建 {} 的父目录失败", target.display())))?;
                    }
                }
                fs::write(target, &bytes)
                    .map_err(|e| io_err(e, &format!("恢复 {} 失败", target.display())))?;
                restored.push(entry.path.clone());
            } else if target.exists() {
                self.check_restore_target(target)?;
                if target.is_dir() {
                    return Err(Error::Invalid(format!(
                        "恢复 {} 时目标已是目录，拒绝删除",
                        target.display()
                    )));
                }
                fs::remove_file(target).map_err(|e| io_err(e, &format!("恢复 {}（删除新建文件）失败", target.display())))?;
                removed.push(entry.path.clone());
            }
        }
        Ok(RestoreReport { snapshot_id: id.to_string(), pre_restore_id: pre.id, restored, removed })
    }

    /// A restore target must be a plain file (or absent). Symlinks are
    /// refused rather than followed; directories are refused rather than
    /// deleted.
    fn check_restore_target(&self, target: &Path) -> Result<()> {
        if let Ok(meta) = fs::symlink_metadata(target) {
            if meta.file_type().is_symlink() {
                return Err(Error::Invalid(format!(
                    "拒绝恢复到符号链接 {}（先解析到真实路径再恢复）",
                    target.display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;

    fn store(dir: &Path) -> SnapshotStore {
        SnapshotStore::new(&dir.join("snapshots"))
    }

    #[test]
    fn create_and_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "v1").unwrap();
        fs::write(&b, "v2").unwrap();
        let s = store(dir.path());
        let m = s.create(&[a.clone(), b.clone()], "test", &now()).unwrap();
        assert_eq!(m.files.len(), 2);
        assert!(m.files.iter().all(|f| f.exists));
        assert_eq!(m.files[0].size, 2);
        fs::write(&a, "v1-modified").unwrap();
        fs::remove_file(&b).unwrap();
        let report = s.restore(&m.id, &now()).unwrap();
        assert_eq!(report.restored, vec![a.display().to_string(), b.display().to_string()]);
        assert!(report.removed.is_empty());
        assert_eq!(fs::read_to_string(&a).unwrap(), "v1");
        assert_eq!(fs::read_to_string(&b).unwrap(), "v2");
        // The pre-restore snapshot holds the modified state.
        let pre = s.get(&report.pre_restore_id).unwrap();
        assert_eq!(pre.files[0].size, "v1-modified".len() as u64);
        assert!(!pre.files[1].exists);
    }

    #[test]
    fn absent_file_is_removed_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let s = store(dir.path());
        let m = s.create(std::slice::from_ref(&missing), "test", &now()).unwrap();
        assert!(!m.files[0].exists);
        fs::write(&missing, "created later").unwrap();
        let report = s.restore(&m.id, &now()).unwrap();
        assert_eq!(report.removed, vec![missing.display().to_string()]);
        assert!(!missing.exists());
    }

    #[test]
    fn unknown_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(matches!(s.get("snp_nope"), Err(Error::NotFound(_))));
        assert!(matches!(s.restore("snp_nope", &now()), Err(Error::NotFound(_))));
        assert!(matches!(s.get("../escape"), Err(Error::Invalid(_))));
        assert!(matches!(s.get(""), Err(Error::Invalid(_))));
    }

    #[test]
    fn list_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let f = dir.path().join("f.txt");
        fs::write(&f, "x").unwrap();
        // Explicit timestamps: two snapshots in the same second share a
        // created_at string, and the id tiebreak is random — so pin the
        // order with the clock instead of the wall clock.
        let earlier = hatch_core::time::parse_rfc3339("2026-01-01T00:00:00Z").unwrap();
        let later = hatch_core::time::parse_rfc3339("2026-01-02T00:00:00Z").unwrap();
        let first = s.create(std::slice::from_ref(&f), "one", &earlier).unwrap();
        let second = s.create(std::slice::from_ref(&f), "two", &later).unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);
    }

    #[test]
    fn directory_and_symlink_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(matches!(
            s.create(&[dir.path().to_path_buf()], "test", &now()),
            Err(Error::Invalid(_))
        ));
        let target = dir.path().join("real.txt");
        fs::write(&target, "real").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            s.create(&[link], "test", &now()),
            Err(Error::Invalid(_))
        ));
    }
}
