//! Share tokens for artifacts: `hatch_art_` + 32 random bytes,
//! minted when a `share_artifact` approval executes, served at
//! `GET /v1/shared/<token>`, revocable at any time. Only the SHA-256
//! hash is stored (registry `<artifacts root>/shares.json`, atomic
//! write, `0600`); the plaintext is shown exactly once and never
//! appears in the file, in logs, or in audit output. A missing file
//! is an empty registry; a corrupt one fails closed. Every operation
//! reloads the file, so a revoke takes effect on the next request.

use crate::store::{ArtifactStore, VersionManifest};
use hatch_core::ids::{digest, random_id};
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Share-token prefix (distinct from MCP's `hatch_mcp_`).
pub const TOKEN_PREFIX: &str = "hatch_art_";

/// Registry file name under the artifacts root.
pub const REGISTRY_FILE: &str = "shares.json";

/// One share: which artifact it opens, and the hash of its token.
/// The plaintext token is never stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecord {
    pub id: String,
    pub artifact_id: String,
    /// SHA-256 hex of the full token string (`hatch_art_...`).
    pub token_hash: String,
    pub created_at: String,
    pub revoked: bool,
    pub revoked_at: Option<String>,
    /// The approval that minted this share, if any.
    pub approval_id: Option<String>,
}

impl ShareRecord {
    /// Public view: everything except the token hash.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "artifact_id": self.artifact_id,
            "created_at": self.created_at,
            "revoked": self.revoked,
            "revoked_at": self.revoked_at,
            "approval_id": self.approval_id,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    shares: Vec<ShareRecord>,
}

fn now_rfc3339() -> String {
    hatch_core::time::to_rfc3339(&hatch_core::time::now())
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        // Fallback: time + pid hashed (same shape as the MCP token
        // fallback); 32 bytes from weaker entropy.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!("{nanos}:{}", std::process::id());
        let h = digest(&[&seed]);
        let hex = h.as_bytes();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = hex[i % hex.len()] ^ ((nanos >> (i % 8)) as u8);
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{TOKEN_PREFIX}{hex}")
}

/// Constant-time string comparison (no early exit on the first
/// differing byte), same shape as `bearer_ok` in the MCP server.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The share registry for one artifacts root.
pub struct ShareRegistry {
    path: PathBuf,
}

impl ShareRegistry {
    pub fn new(root: &Path) -> ShareRegistry {
        ShareRegistry {
            path: root.join(REGISTRY_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<RegistryFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                Error::Storage(format!("分享注册表 {} 已损坏：{e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile {
                version: 1,
                shares: Vec::new(),
            }),
            Err(e) => Err(Error::Storage(format!(
                "读取分享注册表 {} 失败：{e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, file: &RegistryFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            #[cfg(unix)]
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            f.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Mint a share token for an artifact. Returns the record plus
    /// the plaintext token (shown once by the caller, never stored).
    pub fn mint(
        &self,
        artifact_id: &str,
        approval_id: Option<&str>,
    ) -> Result<(ShareRecord, String)> {
        let mut file = self.load()?;
        let token = random_token();
        let rec = ShareRecord {
            id: random_id("shr"),
            artifact_id: artifact_id.to_string(),
            token_hash: digest(&[&token]),
            created_at: now_rfc3339(),
            revoked: false,
            revoked_at: None,
            approval_id: approval_id.map(|s| s.to_string()),
        };
        file.shares.push(rec.clone());
        self.save(&file)?;
        Ok((rec, token))
    }

    /// Mint, unless this approval already minted an active share
    /// (a re-run must not mint a second token it can never show
    /// again). The second element is `None` on a dedup hit.
    pub fn mint_or_existing(
        &self,
        artifact_id: &str,
        approval_id: Option<&str>,
    ) -> Result<(ShareRecord, Option<String>)> {
        if let Some(approval_id) = approval_id {
            let file = self.load()?;
            if let Some(rec) = file.shares.iter().find(|r| {
                !r.revoked
                    && r.artifact_id == artifact_id
                    && r.approval_id.as_deref() == Some(approval_id)
            }) {
                return Ok((rec.clone(), None));
            }
        }
        let (rec, token) = self.mint(artifact_id, approval_id)?;
        Ok((rec, Some(token)))
    }

    /// Resolve a token to its active share, if any. Unknown and
    /// revoked tokens both resolve to `None` (the caller 404s either way).
    pub fn resolve(&self, token: &str) -> Result<Option<ShareRecord>> {
        let file = self.load()?;
        let hash = digest(&[token]);
        Ok(file
            .shares
            .iter()
            .find(|r| !r.revoked && ct_eq(&r.token_hash, &hash))
            .cloned())
    }

    /// Revoke one share by id (`None` when there is no such share).
    pub fn revoke(&self, share_id: &str) -> Result<Option<ShareRecord>> {
        let mut file = self.load()?;
        let mut out = None;
        for r in file.shares.iter_mut() {
            if r.id == share_id && !r.revoked {
                r.revoked = true;
                r.revoked_at = Some(now_rfc3339());
                out = Some(r.clone());
            }
        }
        if out.is_some() {
            self.save(&file)?;
        }
        Ok(out)
    }

    /// Revoke every active share of one artifact. Returns the
    /// revoked records.
    pub fn revoke_for_artifact(&self, artifact_id: &str) -> Result<Vec<ShareRecord>> {
        let mut file = self.load()?;
        let mut out = Vec::new();
        for r in file.shares.iter_mut() {
            if r.artifact_id == artifact_id && !r.revoked {
                r.revoked = true;
                r.revoked_at = Some(now_rfc3339());
                out.push(r.clone());
            }
        }
        if !out.is_empty() {
            self.save(&file)?;
        }
        Ok(out)
    }

    /// All shares, oldest first.
    pub fn list(&self) -> Result<Vec<ShareRecord>> {
        Ok(self.load()?.shares)
    }

    /// Shares of one artifact, oldest first.
    pub fn list_for(&self, artifact_id: &str) -> Result<Vec<ShareRecord>> {
        Ok(self
            .load()?
            .shares
            .into_iter()
            .filter(|r| r.artifact_id == artifact_id)
            .collect())
    }

    /// Active shares of one artifact.
    pub fn active_for(&self, artifact_id: &str) -> Result<Vec<ShareRecord>> {
        Ok(self
            .list_for(artifact_id)?
            .into_iter()
            .filter(|r| !r.revoked)
            .collect())
    }
}

/// Resolve a share token to the artifact's latest version. Unknown
/// tokens, revoked tokens and missing artifacts all answer the same
/// `NotFound` — a share link must not leak whether the artifact exists.
pub fn resolve_share(
    store: &ArtifactStore,
    token: &str,
) -> Result<(ShareRecord, VersionManifest, String)> {
    let rec = ShareRegistry::new(store.root())
        .resolve(token)?
        .ok_or_else(|| Error::NotFound("分享不存在或已撤销".into()))?;
    let (manifest, content) = store
        .read_version(&rec.artifact_id, None)
        .map_err(|_| Error::NotFound("分享不存在或已撤销".into()))?;
    Ok((rec, manifest, content))
}

/// Revoke one share by id; flip the artifact's `shared` flag off
/// when it has no active shares left. `None` when no such share.
pub fn revoke_share(store: &ArtifactStore, share_id: &str) -> Result<Option<ShareRecord>> {
    let reg = ShareRegistry::new(store.root());
    let rec = match reg.revoke(share_id)? {
        Some(r) => r,
        None => return Ok(None),
    };
    if reg.active_for(&rec.artifact_id)?.is_empty() {
        store.set_shared(&rec.artifact_id, false)?;
    }
    Ok(Some(rec))
}

/// Revoke every active share of one artifact and flip its `shared`
/// flag off when none remain. Returns the revoked count; the
/// artifact itself is untouched.
pub fn revoke_artifact_shares(store: &ArtifactStore, artifact_id: &str) -> Result<usize> {
    let _meta = store.get(artifact_id)?;
    let revoked = ShareRegistry::new(store.root())
        .revoke_for_artifact(artifact_id)?
        .len();
    if ShareRegistry::new(store.root())
        .active_for(artifact_id)?
        .is_empty()
    {
        store.set_shared(artifact_id, false)?;
    }
    Ok(revoked)
}

/// Delete an artifact: revoke all its shares first, then remove its
/// directory. Returns how many shares were revoked.
pub fn delete_artifact(store: &ArtifactStore, id: &str) -> Result<usize> {
    let _meta = store.get(id)?;
    let revoked = ShareRegistry::new(store.root())
        .revoke_for_artifact(id)?
        .len();
    let dir = store.root().join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(revoked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ArtifactKind;

    /// G11: a leaked share link stops working the moment it is revoked —
    /// the revoked token resolves exactly like an unknown one (NotFound),
    /// so revocation leaks nothing about whether the artifact exists.
    #[test]
    fn revoked_share_resolves_like_an_unknown_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(&dir.path().join("artifacts"));
        let built = store
            .build(None, "周报", ArtifactKind::Document, "内容", "cli")
            .unwrap();
        let reg = ShareRegistry::new(store.root());
        let (rec, token) = reg.mint(&built.meta.id, None).unwrap();
        // The live link resolves …
        let (got, _, _) = resolve_share(&store, &token).unwrap();
        assert_eq!(got.id, rec.id);
        // … an unknown token does not …
        assert!(resolve_share(&store, "shr_nope").is_err());
        // … and after revocation the real token stops resolving too.
        let revoked = revoke_share(&store, &rec.id).unwrap().unwrap();
        assert!(revoked.revoked);
        let err = resolve_share(&store, &token).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        assert!(!store.get(&built.meta.id).unwrap().shared, "no active shares left flips shared off");
    }
}
