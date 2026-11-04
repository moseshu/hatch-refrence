//! Versioned artifact store under `$HATCH_HOME/artifacts/<id>/v<n>/`
//! (`meta.json` per artifact, `manifest.json` + content per version;
//! a later build never touches an old one). Sharing is an egress,
//! recorded only by the `share_artifact` executor through the
//! approval queue; delivery is a separate step whose message must
//! contain the artifact's usable path, and only then flips
//! `delivered` (the flag is the record, not the file's existence).
//! Kinds: `document` (markdown/text) and `html` (sanitised on build).
//! Content is secret-scanned before writing (a hit names only the
//! secret *kinds*, never the values), and every build first passes
//! the quota check ([`crate::quota`]) — a refused build writes
//! nothing. Shares are bearer tokens ([`crate::share`]).

use crate::quota::Quota;
use crate::sanitize::sanitize_html;
use hatch_core::time::{now, to_rfc3339};
use hatch_core::{ids, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Artifact kinds. Only these two exist; anything else is out of scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Document,
    Html,
}

impl ArtifactKind {
    pub fn parse(s: &str) -> Result<ArtifactKind> {
        match s {
            "document" => Ok(ArtifactKind::Document),
            "html" => Ok(ArtifactKind::Html),
            other => Err(Error::Invalid(format!(
                "unknown artifact kind `{other}` (document | html); other kinds are out of scope"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Document => "document",
            ArtifactKind::Html => "html",
        }
    }

    pub fn filename(&self) -> &'static str {
        match self {
            ArtifactKind::Document => "content.md",
            ArtifactKind::Html => "content.html",
        }
    }
}

/// One version's manifest, stored next to its content file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    pub id: String,
    pub title: String,
    pub kind: ArtifactKind,
    pub version: u32,
    pub created_at: String,
    /// Where the content came from (`cli`, `tool:create_artifact`, or a
    /// workspace-relative source path).
    pub source: String,
    /// SHA-256 hex of the stored content bytes.
    pub checksum: String,
    pub filename: String,
}

/// Per-artifact state, stored in `meta.json` next to the versions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub id: String,
    pub title: String,
    pub kind: ArtifactKind,
    pub created_at: String,
    pub versions: Vec<u32>,
    pub shared: bool,
    pub shared_approval_id: Option<String>,
    pub delivered: bool,
    pub delivered_at: Option<String>,
}

impl ArtifactMeta {
    pub fn latest_version(&self) -> u32 {
        self.versions.iter().max().copied().unwrap_or(0)
    }

    /// Usable path (link) to a version's content file.
    pub fn content_path(&self, root: &Path, version: u32) -> PathBuf {
        root.join(&self.id).join(format!("v{version}")).join(self.kind.filename())
    }
}

fn checksum_hex(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What a build produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltArtifact {
    pub meta: ArtifactMeta,
    pub manifest: VersionManifest,
    /// Absolute path of the written content file.
    pub path: PathBuf,
}

/// Filesystem store for versioned artifacts.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    quota: Quota,
}

impl ArtifactStore {
    pub fn new(root: &Path) -> ArtifactStore {
        ArtifactStore { root: root.to_path_buf(), quota: Quota::default() }
    }

    /// Override the quota (config `[artifacts]` + env; default is
    /// [`Quota::default`]).
    pub fn with_quota(mut self, quota: Quota) -> ArtifactStore {
        self.quota = quota;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("meta.json")
    }

    fn read_meta(&self, id: &str) -> Result<ArtifactMeta> {
        let p = self.meta_path(id);
        let raw = std::fs::read_to_string(&p).map_err(|e| Error::NotFound(format!("artifact {id} 不存在（{}：{e}）", p.display())))?;
        serde_json::from_str(&raw).map_err(|e| Error::Invalid(format!("artifact {id} 的 meta.json 已损坏：{e}")))
    }

    fn write_meta(&self, meta: &ArtifactMeta) -> Result<()> {
        let dir = self.root.join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(self.meta_path(&meta.id), serde_json::to_string_pretty(meta)?)?;
        Ok(())
    }

    /// Build a version. `id == None` starts a new artifact at v1;
    /// `Some(id)` adds a version to an existing artifact. The content is
    /// secret-scanned first (rejected, kinds only, values never logged)
    /// and HTML is sanitised. Old versions are never overwritten.
    pub fn build(&self, id: Option<&str>, title: &str, kind: ArtifactKind, content: &str, source: &str) -> Result<BuiltArtifact> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::Invalid("artifact title must not be empty".into()));
        }
        let kinds = hatch_core::secrets::kinds(&format!("{title}\n{content}"));
        if !kinds.is_empty() {
            let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
            return Err(Error::SecretRejected(format!(
                "artifact 内容疑似包含秘密（{}），已拒绝写入；请移除后再试",
                names.join(", ")
            )));
        }
        let content = match kind {
            ArtifactKind::Html => sanitize_html(content),
            ArtifactKind::Document => content.to_string(),
        };
        let id = match id {
            Some(i) => {
                if !valid_id(i) {
                    return Err(Error::Invalid(format!("invalid artifact id `{i}` (letters, digits, `-`, `_` only)")));
                }
                i.to_string()
            }
            None => ids::random_id("art"),
        };
        let (mut meta, is_new) = match self.read_meta(&id) {
            Ok(m) => {
                if m.kind != kind {
                    return Err(Error::Invalid(format!(
                        "artifact {id} 已是 {}，不能再建 {} 版本",
                        m.kind.as_str(),
                        kind.as_str()
                    )));
                }
                (m, false)
            }
            Err(Error::NotFound(_)) => (
                ArtifactMeta {
                    id: id.clone(),
                    title: title.to_string(),
                    kind,
                    created_at: to_rfc3339(&now()),
                    versions: vec![],
                    shared: false,
                    shared_approval_id: None,
                    delivered: false,
                    delivered_at: None,
                },
                true,
            ),
            Err(e) => return Err(e),
        };
        // Quota first: a refused build writes nothing (no half version).
        crate::quota::check(&self.quota, &crate::quota::usage(&self.root)?, is_new, content.len() as u64)?;
        // A retitle on a new version is a new title for the whole artifact.
        meta.title = title.to_string();
        let version = meta.latest_version() + 1;
        let dir = self.root.join(&id).join(format!("v{version}"));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(kind.filename());
        std::fs::write(&path, content.as_bytes())?;
        let manifest = VersionManifest {
            id: id.clone(),
            title: title.to_string(),
            kind,
            version,
            created_at: to_rfc3339(&now()),
            source: source.to_string(),
            checksum: checksum_hex(content.as_bytes()),
            filename: kind.filename().to_string(),
        };
        std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
        meta.versions.push(version);
        self.write_meta(&meta)?;
        Ok(BuiltArtifact { meta, manifest, path })
    }

    /// All artifacts, newest first by creation time.
    pub fn list(&self) -> Result<Vec<ArtifactMeta>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            if let Ok(m) = self.read_meta(&e.file_name().to_string_lossy()) {
                out.push(m);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<ArtifactMeta> {
        if !valid_id(id) {
            return Err(Error::Invalid(format!("invalid artifact id `{id}`")));
        }
        self.read_meta(id)
    }

    /// Manifest + content of one version (`version == None` = latest).
    pub fn read_version(&self, id: &str, version: Option<u32>) -> Result<(VersionManifest, String)> {
        let meta = self.get(id)?;
        let v = version.unwrap_or_else(|| meta.latest_version());
        if !meta.versions.contains(&v) {
            return Err(Error::NotFound(format!("artifact {id} 没有 v{v} 版本")));
        }
        let dir = self.root.join(id).join(format!("v{v}"));
        let raw = std::fs::read_to_string(dir.join("manifest.json"))
            .map_err(|e| Error::NotFound(format!("artifact {id} v{v} 的 manifest 缺失：{e}")))?;
        let manifest: VersionManifest = serde_json::from_str(&raw)?;
        let content = std::fs::read_to_string(dir.join(manifest.filename.as_str()))
            .map_err(|e| Error::NotFound(format!("artifact {id} v{v} 的内容缺失：{e}")))?;
        if checksum_hex(content.as_bytes()) != manifest.checksum {
            return Err(Error::Invalid(format!("artifact {id} v{v} 的内容与 manifest 校验和不符")));
        }
        Ok((manifest, content))
    }

    /// Record that sharing was approved (only the `share_artifact`
    /// executor calls this, after the approval queue ran). Sharing is
    /// per artifact, not per version.
    pub fn mark_shared(&self, id: &str, approval_id: &str) -> Result<ArtifactMeta> {
        let mut meta = self.get(id)?;
        meta.shared = true;
        meta.shared_approval_id = Some(approval_id.to_string());
        self.write_meta(&meta)?;
        Ok(meta)
    }

    /// Set the shared flag directly (crate-internal: the share module
    /// flips it off when the last share of an artifact is revoked).
    pub(crate) fn set_shared(&self, id: &str, shared: bool) -> Result<ArtifactMeta> {
        let mut meta = self.get(id)?;
        meta.shared = shared;
        self.write_meta(&meta)?;
        Ok(meta)
    }

    /// Deliver the artifact: compose the announcement message and only
    /// then flip `delivered`. Requires the artifact to be shared first —
    /// an unshared artifact has no approved egress. The returned message
    /// always contains the usable path to the latest version.
    pub fn deliver(&self, id: &str) -> Result<String> {
        let mut meta = self.get(id)?;
        if !meta.shared {
            return Err(Error::Denied(format!(
                "artifact {id} 尚未分享：先 `hatch artifact share {id}` 并批准，再交付"
            )));
        }
        let v = meta.latest_version();
        let path = meta.content_path(&self.root, v);
        if !path.is_file() {
            return Err(Error::NotFound(format!("artifact {id} v{v} 的内容文件缺失")));
        }
        let message = format!(
            "已交付：{}（{}）v{}\n文件：{}\n（已分享，审批 {}）",
            meta.title,
            meta.kind.as_str(),
            v,
            path.display(),
            meta.shared_approval_id.as_deref().unwrap_or("—")
        );
        meta.delivered = true;
        meta.delivered_at = Some(to_rfc3339(&now()));
        self.write_meta(&meta)?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ArtifactStore) {
        let d = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(&d.path().join("artifacts"));
        (d, s)
    }

    #[test]
    fn versioning_keeps_v1_after_v2_and_checksums() {
        let (_d, s) = store();
        let b1 = s.build(None, "周报", ArtifactKind::Document, "# 第一周\n", "cli").unwrap();
        assert_eq!(b1.manifest.version, 1);
        assert_eq!(b1.manifest.checksum, checksum_hex("# 第一周\n".as_bytes()));
        let id = b1.meta.id.clone();
        let b2 = s.build(Some(&id), "周报", ArtifactKind::Document, "# 第二周\n", "cli").unwrap();
        assert_eq!(b2.manifest.version, 2);
        let (m1, c1) = s.read_version(&id, Some(1)).unwrap();
        assert_eq!(c1, "# 第一周\n");
        assert_eq!(m1.checksum, checksum_hex("# 第一周\n".as_bytes()));
        let (m2, c2) = s.read_version(&id, None).unwrap();
        assert_eq!(m2.version, 2);
        assert_eq!(c2, "# 第二周\n");
        let meta = s.get(&id).unwrap();
        assert_eq!(meta.versions, vec![1, 2]);
        assert!(!meta.shared && !meta.delivered, "write-only is not shared or delivered");
    }

    #[test]
    fn secret_content_is_rejected_without_echoing_the_value() {
        let (_d, s) = store();
        let err = s.build(None, "笔记", ArtifactKind::Document, "我的密码：hunter22x", "cli").unwrap_err();
        assert!(matches!(err, Error::SecretRejected(_)), "{err}");
        assert!(!err.to_string().contains("hunter22x"), "secret value must not leak: {err}");
        assert!(s.list().unwrap().is_empty(), "rejected build writes nothing");
    }

    #[test]
    fn html_kind_is_sanitised_at_build() {
        let (_d, s) = store();
        let b = s.build(None, "卡片", ArtifactKind::Html, "<p>好</p><script>坏</script>", "cli").unwrap();
        let (_, content) = s.read_version(&b.meta.id, None).unwrap();
        assert!(content.contains("好") && !content.contains("script"));
    }

    #[test]
    fn deliver_requires_share_first() {
        let (_d, s) = store();
        let b = s.build(None, "周报", ArtifactKind::Document, "内容", "cli").unwrap();
        assert!(s.deliver(&b.meta.id).is_err(), "unshared artifact must not deliver");
        assert!(!s.get(&b.meta.id).unwrap().delivered);
        s.mark_shared(&b.meta.id, "appr_1").unwrap();
        let msg = s.deliver(&b.meta.id).unwrap();
        assert!(msg.contains(&b.path.display().to_string()), "delivery message must carry the link: {msg}");
        assert!(s.get(&b.meta.id).unwrap().delivered);
    }

    #[test]
    fn unknown_kind_and_bad_ids_rejected() {
        assert!(ArtifactKind::parse("pdf").is_err());
        assert!(ArtifactKind::parse("document").is_ok());
        let (_d, s) = store();
        assert!(s.build(Some("../evil"), "t", ArtifactKind::Document, "x", "cli").is_err());
        assert!(s.get("../evil").is_err());
    }
}
