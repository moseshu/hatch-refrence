//! Per-client bearer tokens for the MCP HTTP server.
//!
//! One shared bearer token (`mcp-http` in the vault / `HATCH_MCP_TOKEN`)
//! used to be the only credential: leak it and every client must
//! re-key. This registry gives each client its own named token with its
//! own scopes, revocable on its own.
//!
//! Storage: `<home>/mcp-clients.json`, written atomically (temp file +
//! rename) with mode `0600`. Only the SHA-256 hash of a token is stored;
//! the plaintext is shown exactly once, when the token is created, and
//! never appears in the file, in logs, or in audit output. The server
//! reloads the file on every request, so `revoke` takes effect on the
//! very next request with no restart.
//!
//! A token is `hatch_mcp_` + 32 random bytes (hex). Scopes are `read`
//! and/or `write` (see [`crate::scope`]); `write` implies `read`.

use crate::scope::Scope;
use hatch_core::ids::{digest, random_id};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Registry file name under the hatch home directory.
pub const REGISTRY_FILE: &str = "mcp-clients.json";

/// The registry file for a home directory: `<home>/mcp-clients.json`.
pub fn registry_path(home: &Path) -> PathBuf {
    home.join(REGISTRY_FILE)
}

/// One registered client: its identity, scopes, and the hash of its
/// token. The plaintext token is never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientToken {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// SHA-256 hex of the full token string (`hatch_mcp_...`).
    pub token_hash: String,
    pub created_at: String,
    pub revoked: bool,
    pub revoked_at: Option<String>,
}

/// The whole registry file (versioned so the format can evolve).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientRegistry {
    pub version: u32,
    #[serde(default)]
    pub clients: Vec<ClientToken>,
}

/// What authentication establishes about a caller: its name (for the
/// approval origin `mcp:http:<name>`) and its scopes.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    pub name: String,
    pub scopes: Vec<Scope>,
}

/// A client name is valid when it is non-empty, at most 64 characters,
/// and contains only ASCII letters, digits, `_`, or `-` (same rule as
/// vault token names; the name ends up in approval origins, so it must
/// stay out of trouble there).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        // Fallback: time + pid + counter (same shape as the session-id
        // fallback in http_proto.rs). 32 bytes are still produced, just
        // from weaker entropy.
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
    format!("hatch_mcp_{hex}")
}

/// Constant-time string comparison (no early exit on the first
/// differing byte), same shape as `bearer_ok` in http_proto.rs.
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

impl ClientRegistry {
    /// Load the registry. A missing file is an empty registry; a
    /// present but unreadable/corrupt file is an error (fail closed:
    /// the server refuses to guess at credentials).
    pub fn load(path: &Path) -> Result<ClientRegistry, String> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("token 注册表 {} 损坏：{e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClientRegistry {
                version: 1,
                clients: Vec::new(),
            }),
            Err(e) => Err(format!("读取 token 注册表 {} 失败：{e}", path.display())),
        }
    }

    /// Load the registry only when the file exists (`None` = no
    /// per-client auth configured at all). A corrupt file is an error.
    pub fn load_if_present(path: &Path) -> Result<Option<ClientRegistry>, String> {
        if !path.exists() {
            return Ok(None);
        }
        Self::load(path).map(Some)
    }

    /// Write the registry atomically (temp file in the same directory,
    /// then rename) with mode `0600` (unix).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化 token 注册表失败：{e}"))?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| format!("写 token 注册表临时文件 {} 失败：{e}", tmp.display()))?;
            f.write_all(text.as_bytes())
                .map_err(|e| format!("写 token 注册表临时文件失败：{e}"))?;
            f.sync_all().ok();
        }
        tighten_file(&tmp);
        fs::rename(&tmp, path)
            .map_err(|e| format!("保存 token 注册表 {} 失败：{e}", path.display()))?;
        tighten_file(path);
        Ok(())
    }

    /// Number of clients that can still authenticate.
    pub fn active_count(&self) -> usize {
        self.clients.iter().filter(|c| !c.revoked).count()
    }

    /// Create a client and return `(record, plaintext_token)`. The
    /// plaintext is returned only here — callers must show it once and
    /// never store it. A duplicate name is an error.
    pub fn create(
        &mut self,
        name: &str,
        scopes: Vec<Scope>,
    ) -> Result<(ClientToken, String), String> {
        if !is_valid_name(name) {
            return Err("客户端名字无效：非空、最多 64 字符、只含字母/数字/_/-".to_string());
        }
        if self.clients.iter().any(|c| c.name == name) {
            return Err(format!(
                "客户端 `{name}` 已存在（先 revoke 再重建，或换个名字）"
            ));
        }
        let mut scopes = scopes;
        scopes.sort_by_key(|s| s.as_str());
        scopes.dedup();
        if scopes.is_empty() {
            scopes.push(Scope::Read);
        }
        let token = random_token();
        let record = ClientToken {
            id: random_id("mcp_client"),
            name: name.to_string(),
            scopes,
            token_hash: digest(&[&token]),
            created_at: now_rfc3339(),
            revoked: false,
            revoked_at: None,
        };
        self.clients.push(record.clone());
        Ok((record, token))
    }

    /// Revoke a client by name or id. The record is kept (marked
    /// revoked) so the audit trail survives; it can no longer
    /// authenticate. Already-revoked is an error (not silent success).
    pub fn revoke(&mut self, name_or_id: &str) -> Result<&ClientToken, String> {
        let c = self
            .clients
            .iter_mut()
            .find(|c| c.name == name_or_id || c.id == name_or_id)
            .ok_or_else(|| format!("没有客户端 `{name_or_id}`"))?;
        if c.revoked {
            return Err(format!("客户端 `{}` 已经被撤销", c.name));
        }
        c.revoked = true;
        c.revoked_at = Some(now_rfc3339());
        Ok(c)
    }

    /// Authenticate a presented bearer token: hash it, compare against
    /// every non-revoked record in constant time. Returns the client's
    /// identity, or `None` when no record matches.
    pub fn authenticate(&self, token: &str) -> Option<ClientIdentity> {
        let hash = digest(&[token]);
        self.clients
            .iter()
            .filter(|c| !c.revoked)
            .find(|c| ct_eq(&c.token_hash, &hash))
            .map(|c| ClientIdentity {
                name: c.name.clone(),
                scopes: c.scopes.clone(),
            })
    }
}

#[cfg(unix)]
fn tighten_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten_file(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Scope;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hatch-mcp-test-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn create_authenticate_revoke_round_trip() {
        let path = tmp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let mut reg = ClientRegistry::load(&path).unwrap();
        assert_eq!(reg.active_count(), 0);
        let (rec, token) = reg.create("claude-desktop", vec![Scope::Read]).unwrap();
        assert!(token.starts_with("hatch_mcp_"));
        assert_eq!(rec.name, "claude-desktop");
        reg.save(&path).unwrap();
        // The file holds only the hash, never the plaintext.
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&token), "注册表文件绝不能含 token 明文");
        assert!(text.contains(&rec.token_hash));
        // Reload and authenticate.
        let reg2 = ClientRegistry::load(&path).unwrap();
        let id = reg2.authenticate(&token).expect("新 token 必须能认证");
        assert_eq!(id.name, "claude-desktop");
        assert_eq!(id.scopes, vec![Scope::Read]);
        assert!(reg2.authenticate("hatch_mcp_wrong").is_none());
        // Revoke: the very next authentication fails, no restart.
        let mut reg3 = reg2;
        reg3.revoke("claude-desktop").unwrap();
        assert!(reg3.authenticate(&token).is_none());
        assert_eq!(reg3.active_count(), 0);
        reg3.save(&path).unwrap();
        let reg4 = ClientRegistry::load(&path).unwrap();
        assert!(reg4.authenticate(&token).is_none());
        assert!(reg4.clients[0].revoked);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn create_rejects_bad_and_duplicate_names() {
        let mut reg = ClientRegistry::default();
        assert!(reg.create("", vec![Scope::Read]).is_err());
        assert!(reg.create("bad name!", vec![Scope::Read]).is_err());
        reg.create("a", vec![Scope::Read]).unwrap();
        assert!(reg.create("a", vec![Scope::Write]).is_err());
        assert!(reg.revoke("nope").is_err());
        reg.revoke("a").unwrap();
        assert!(reg.revoke("a").is_err()); // already revoked
    }

    #[test]
    fn corrupt_registry_is_an_error_not_empty() {
        let path = tmp_path("corrupt");
        fs::write(&path, "{not json").unwrap();
        assert!(ClientRegistry::load(&path).is_err());
        assert!(ClientRegistry::load_if_present(&path).is_err());
        let _ = fs::remove_file(&path);
        assert!(ClientRegistry::load_if_present(&path).unwrap().is_none());
    }

    #[test]
    fn write_scope_token_authenticates_with_write_scope() {
        let mut reg = ClientRegistry::default();
        let (_, token) = reg.create("w", vec![Scope::Write]).unwrap();
        let id = reg.authenticate(&token).unwrap();
        assert_eq!(id.scopes, vec![Scope::Write]);
    }
}
