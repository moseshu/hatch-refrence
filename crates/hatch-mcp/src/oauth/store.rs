//! Per-server OAuth storage: token and config in **separate files**,
//! each mode `0600`, under `<home>/mcp-oauth/<server>/` (dir `0700`).
//!
//! * `config.json` — no secrets: server URL, client id, scopes, and the
//!   discovered authorization + token endpoints.
//! * `tokens.json` — secrets only: access token, refresh token (when the
//!   server issued one), expiry. No config mixed in.
//!
//! Writes are atomic (temp file in the same directory, then rename) and
//! the mode is tightened after every write; a missing file reads as
//! "nothing stored yet", a corrupt file is an error (never silently
//! wiped). Server names are validated like vault token names
//! (`[A-Za-z0-9_-]`, 1–64 chars) so the directory name cannot escape.

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// The static client registration for one MCP server (from
/// `[[mcp_servers]]` config; DCR is a later step, so the client id is
/// configured, not registered at runtime).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthClientConfig {
    /// Client id shown to the authorization server.
    pub client_id: String,
    /// Scopes to request, e.g. `["mcp:tools"]`.
    pub scopes: Vec<String>,
}

/// What `config.json` holds: the server identity plus the endpoints
/// discovery found. No secret material.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthConfig {
    /// The MCP server URL (the resource).
    pub server_url: String,
    /// Static client id.
    pub client_id: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Authorization endpoint (discovered).
    pub authorization_endpoint: String,
    /// Token endpoint (discovered).
    pub token_endpoint: String,
}

/// What `tokens.json` holds: secrets only.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token expires, if the server said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl OAuthTokens {
    /// Fresh when there is a token and (when an expiry is known) it is
    /// more than `margin_secs` in the future. A token with no known
    /// expiry is treated as fresh — the server will 401 if it is dead,
    /// and the client's 401 path refreshes then.
    pub fn is_fresh(&self, now_unix: i64, margin_secs: i64) -> bool {
        if self.access_token.is_empty() {
            return false;
        }
        match self.expires_at {
            Some(exp) => exp > now_unix + margin_secs,
            None => true,
        }
    }
}

/// The storage directory for one server: `<home>/mcp-oauth/<server>/`.
pub fn oauth_dir(home: &Path, server: &str) -> Result<PathBuf> {
    validate_server_name(server)?;
    Ok(home.join("mcp-oauth").join(server))
}

/// A server name is valid when it is non-empty, at most 64 characters,
/// and contains only ASCII letters, digits, `_`, or `-` (same rule as
/// vault token names).
pub fn validate_server_name(server: &str) -> Result<()> {
    let ok = !server.is_empty()
        && server.chars().count() <= 64
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid MCP server name for OAuth storage (must be 1-64 chars of [A-Za-z0-9_-]): {server:?}"
        )))
    }
}

/// One server's two files.
pub struct OAuthStore {
    dir: PathBuf,
}

impl OAuthStore {
    pub fn new(home: &Path, server: &str) -> Result<Self> {
        Ok(OAuthStore { dir: oauth_dir(home, server)? })
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn tokens_path(&self) -> PathBuf {
        self.dir.join("tokens.json")
    }

    /// Load the config, or `None` when no login has discovered one yet.
    pub fn load_config(&self) -> Result<Option<OAuthConfig>> {
        load_json(&self.config_path())
    }

    /// Load the tokens, or `None` when the server has never been logged
    /// into (or was logged out).
    pub fn load_tokens(&self) -> Result<Option<OAuthTokens>> {
        load_json(&self.tokens_path())
    }

    pub fn save_config(&self, config: &OAuthConfig) -> Result<()> {
        write_json_0600(&self.dir, &self.config_path(), config)
    }

    pub fn save_tokens(&self, tokens: &OAuthTokens) -> Result<()> {
        write_json_0600(&self.dir, &self.tokens_path(), tokens)
    }

    /// Delete the stored tokens (logout). The config stays: discovery
    /// does not need to run again.
    pub fn clear_tokens(&self) -> Result<()> {
        match fs::remove_file(self.tokens_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::Invalid(format!("cannot parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Write JSON atomically (temp file + rename) with mode `0600`, creating
/// the directory (`0700`) when needed.
fn write_json_0600<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    create_dir_0700(dir)?;
    let text = serde_json::to_string_pretty(value)?;
    let tmp = dir.join(format!(".{}.tmp.{}", path.file_name().unwrap().to_string_lossy(), std::process::id()));
    {
        let mut file = create_file_0600(&tmp)?;
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    tighten_file(path);
    Ok(())
}

#[cfg(unix)]
fn create_dir_0700(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(Error::Io)
}

#[cfg(not(unix))]
fn create_dir_0700(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(Error::Io)
}

#[cfg(unix)]
fn create_file_0600(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(Error::Io)
}

#[cfg(not(unix))]
fn create_file_0600(path: &Path) -> Result<fs::File> {
    fs::File::create(path).map_err(Error::Io)
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

    fn tmp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn sample_config() -> OAuthConfig {
        OAuthConfig {
            server_url: "https://example.com/mcp".into(),
            client_id: "hatch".into(),
            scopes: vec!["mcp:tools".into()],
            authorization_endpoint: "https://example.com/oauth/authorize".into(),
            token_endpoint: "https://example.com/oauth/token".into(),
        }
    }

    fn sample_tokens() -> OAuthTokens {
        OAuthTokens {
            access_token: "access-123".into(),
            refresh_token: Some("refresh-456".into()),
            expires_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn config_and_tokens_live_in_separate_files() {
        let home = tmp_home();
        let store = OAuthStore::new(home.path(), "srv").unwrap();
        store.save_config(&sample_config()).unwrap();
        store.save_tokens(&sample_tokens()).unwrap();
        assert_ne!(store.config_path(), store.tokens_path());
        assert_eq!(store.load_config().unwrap().unwrap(), sample_config());
        assert_eq!(store.load_tokens().unwrap().unwrap(), sample_tokens());
        // The token file holds only token fields — no server URL, no
        // client id, no endpoints.
        let raw = fs::read_to_string(store.tokens_path()).unwrap();
        assert!(!raw.contains("example.com"), "token file must not hold config: {raw}");
        assert!(!raw.contains("hatch"), "token file must not hold the client id");
        let cfg_raw = fs::read_to_string(store.config_path()).unwrap();
        assert!(!cfg_raw.contains("access-123"), "config file must not hold tokens");
    }

    #[test]
    #[cfg(unix)]
    fn files_are_0600_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home();
        let store = OAuthStore::new(home.path(), "srv").unwrap();
        store.save_config(&sample_config()).unwrap();
        store.save_tokens(&sample_tokens()).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.dir), 0o700);
        assert_eq!(mode(&store.config_path()), 0o600);
        assert_eq!(mode(&store.tokens_path()), 0o600);
    }

    #[test]
    fn missing_files_read_as_none_and_corrupt_is_an_error() {
        let home = tmp_home();
        let store = OAuthStore::new(home.path(), "srv").unwrap();
        assert!(store.load_config().unwrap().is_none());
        assert!(store.load_tokens().unwrap().is_none());
        store.save_tokens(&sample_tokens()).unwrap();
        fs::write(store.tokens_path(), "{not json").unwrap();
        assert!(store.load_tokens().is_err(), "corrupt tokens must fail, not wipe");
        assert!(store.load_config().unwrap().is_none(), "config untouched by bad tokens");
    }

    #[test]
    fn bad_server_names_are_rejected() {
        let home = tmp_home();
        assert!(OAuthStore::new(home.path(), "../escape").is_err());
        assert!(OAuthStore::new(home.path(), "").is_err());
        assert!(OAuthStore::new(home.path(), "a/b").is_err());
        assert!(OAuthStore::new(home.path(), "ok-name_1").is_ok());
    }

    #[test]
    fn freshness_uses_the_margin() {
        let t = OAuthTokens { access_token: "x".into(), refresh_token: None, expires_at: Some(1000) };
        assert!(t.is_fresh(900, 60)); // 100s left > 60s margin
        assert!(!t.is_fresh(950, 60)); // 50s left < 60s margin
        assert!(!OAuthTokens::default().is_fresh(0, 60));
        let no_exp = OAuthTokens { access_token: "x".into(), refresh_token: None, expires_at: None };
        assert!(no_exp.is_fresh(9_999_999, 60), "no expiry known: server 401s if dead");
    }
}
