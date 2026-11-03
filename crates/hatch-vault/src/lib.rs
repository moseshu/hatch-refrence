//! `hatch-vault` — local credential vault for hatch.
//!
//! Two layers:
//!
//! * **File store** ([`Vault`]): per-connector token records in
//!   `<home>/vault/tokens.json` (directory `0700`, file `0600`). Kept for
//!   the CLI and for `hatch doctor` permission checks. [`Vault::get`]
//!   reads a value into the caller's memory and is the **legacy** path —
//!   agent code must use the helper client instead.
//! * **Helper process** ([`VaultClient`], `client.rs`; request handlers in
//!   `helper.rs`; NDJSON loop in the `hatch-vault-helper` binary): a separate
//!   process that holds token material in its own memory. The parent holds only a
//!   surrogate (the token *name*) and asks the helper to inject a token
//!   into a child process (`inject`), perform an HTTP call with it
//!   (`request`), or — only for the spawn instant of a long-lived stdio
//!   child — reveal it (`reveal`). See `docs/SECURITY.md` for the threat
//!   model: the helper protects tokens at rest and from casual reads, it
//!   is not a sandbox — a compromised agent process can still ask the
//!   helper to *use* a token.
//!
//! Token values are never logged or printed anywhere in this crate.

pub mod client;
pub mod helper;
pub mod locate;
pub mod oauth;
pub mod record;
pub(crate) mod spawn;

pub use client::{helper_path, helper_pid, helper_status, stop_helper, HelperStatus, VaultClient};
pub(crate) use spawn::spawn_capture;
pub use record::{TokenRecord, TokenSummary};

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use hatch_core::{Error, Result};

/// Directory mode for the vault directory (unix).
const DIR_MODE: u32 = 0o700;
/// File mode for `tokens.json` and its write temp file (unix).
const FILE_MODE: u32 = 0o600;
/// Maximum length of a token name, in characters.
const MAX_NAME_LEN: usize = 64;

/// A handle to an opened vault directory.
#[derive(Debug, Clone)]
pub struct Vault {
    /// The vault directory (`<home>/vault`).
    pub dir: PathBuf,
    /// The token store file (`<home>/vault/tokens.json`).
    pub path: PathBuf,
}

/// The vault directory for a given home directory: `<home>/vault`.
pub fn vault_dir(home: &Path) -> PathBuf {
    home.join("vault")
}

/// The token store file for a given home directory:
/// `<home>/vault/tokens.json`.
pub fn vault_path(home: &Path) -> PathBuf {
    vault_dir(home).join("tokens.json")
}

/// Open (creating if needed) the vault under `home`.
///
/// Creates the vault directory with mode `0700` and `tokens.json` with mode
/// `0600` (initial content `{}`) when missing. If either already exists,
/// permissions are tightened to `0700`/`0600` on a best-effort basis (unix
/// only; on non-unix platforms this step is a no-op).
pub fn open(home: &Path) -> Result<Vault> {
    let dir = vault_dir(home);
    let path = vault_path(home);

    create_dir_0700(&dir)?;
    tighten_dir(&dir);

    if !path.exists() {
        create_empty_file_0600(&path)?;
    }
    tighten_file(&path);

    Ok(Vault { dir, path })
}

/// A token name is valid when it is non-empty, at most 64 characters long,
/// and contains only ASCII letters, digits, `_`, or `-`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Vault {
    /// Store `value` under `name`, overwriting any previous value.
    ///
    /// Rejects invalid names and empty values. The file is rewritten
    /// atomically (write to a temp file in the same directory, then rename)
    /// and the final file is left with mode `0600`.
    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        self.set_record(name, TokenRecord::new(value))
    }

    /// Store a full record under `name`, overwriting any previous record.
    pub fn set_record(&self, name: &str, record: TokenRecord) -> Result<()> {
        validate_name(name)?;
        if record.value.is_empty() {
            return Err(Error::Invalid("token value must not be empty".into()));
        }
        let mut map = self.read_records()?;
        map.insert(name.to_string(), record);
        self.write_records(&map)
    }

    /// Return the value stored under `name`, or `None` if there is none.
    ///
    /// Legacy path: reads the value into the caller's memory. Agent code
    /// should use [`VaultClient`] instead.
    pub fn get(&self, name: &str) -> Result<Option<String>> {
        Ok(self.get_record(name)?.map(|r| r.value))
    }

    /// Return the full record stored under `name`, or `None`.
    pub fn get_record(&self, name: &str) -> Result<Option<TokenRecord>> {
        validate_name(name)?;
        Ok(self.read_records()?.remove(name))
    }

    /// Delete the entry for `name`. Returns `true` if an entry existed.
    pub fn delete(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        let mut map = self.read_records()?;
        if map.remove(name).is_none() {
            return Ok(false);
        }
        self.write_records(&map)?;
        Ok(true)
    }

    /// Return all stored token names, sorted. Never returns values.
    pub fn list(&self) -> Result<Vec<String>> {
        Ok(self.read_records()?.into_keys().collect())
    }

    /// Return metadata for all stored tokens (names and state, never
    /// values).
    pub fn list_summaries(&self) -> Result<Vec<TokenSummary>> {
        let now = record::now_unix();
        Ok(self
            .read_records()?
            .iter()
            .map(|(n, r)| r.summary(n, now))
            .collect())
    }

    /// Check the vault's filesystem permissions.
    ///
    /// Returns one warning string per problem found, or an empty vec when
    /// everything is fine:
    /// * vault directory is not mode `0700`,
    /// * `tokens.json` is not mode `0600`,
    /// * `tokens.json` is missing.
    ///
    /// On non-unix platforms there is nothing to check, so this is empty.
    pub fn check_permissions(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut warnings = Vec::new();
            match fs::metadata(&self.dir) {
                Ok(meta) => {
                    if meta.permissions().mode() & 0o777 != DIR_MODE {
                        warnings.push(format!(
                            "vault directory {} is not mode 0700",
                            self.dir.display()
                        ));
                    }
                }
                Err(_) => {
                    warnings.push(format!("vault directory {} is missing", self.dir.display()));
                }
            }
            match fs::metadata(&self.path) {
                Ok(meta) => {
                    if meta.permissions().mode() & 0o777 != FILE_MODE {
                        warnings.push(format!(
                            "tokens file {} is not mode 0600",
                            self.path.display()
                        ));
                    }
                }
                Err(_) => {
                    warnings.push(format!("tokens file {} is missing", self.path.display()));
                }
            }
            warnings
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }

    /// Read and parse `tokens.json` into name -> record. Accepts both the
    /// new record format and the legacy name -> string format. A missing
    /// file is treated as empty. A corrupt file is an [`Error::Invalid`];
    /// the file is never silently wiped or repaired.
    fn read_records(&self) -> Result<BTreeMap<String, TokenRecord>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => return Err(Error::Io(err)),
        };
        record::parse_store(&text)
            .map_err(|m| Error::Invalid(format!("{m}: {}", self.path.display())))
    }

    /// Write the whole record map atomically: temp file in the same
    /// directory (mode `0600`), then rename over `tokens.json`.
    fn write_records(&self, map: &BTreeMap<String, TokenRecord>) -> Result<()> {
        let text = serde_json::to_string(map)?;
        let tmp_path = self.temp_path();
        {
            let mut file = create_file_0600(&tmp_path)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &self.path)?;
        tighten_file(&self.path);
        Ok(())
    }

    /// A temp path next to `tokens.json`, unique per process.
    fn temp_path(&self) -> PathBuf {
        self.dir
            .join(format!(".tokens.json.tmp.{}", std::process::id()))
    }
}

fn validate_name(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        return Err(Error::Invalid(format!(
            "invalid token name (must be 1-64 chars of [A-Za-z0-9_-]): {name:?}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn create_dir_0700(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(Error::Io)
}

#[cfg(not(unix))]
fn create_dir_0700(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(Error::Io)
}

#[cfg(unix)]
pub(crate) fn create_file_0600(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(Error::Io)
}

#[cfg(not(unix))]
pub(crate) fn create_file_0600(path: &Path) -> Result<fs::File> {
    fs::File::create(path).map_err(Error::Io)
}

fn create_empty_file_0600(path: &Path) -> Result<()> {
    let mut file = create_file_0600(path)?;
    file.write_all(b"{}")?;
    file.sync_all()?;
    Ok(())
}

/// Best-effort permission tightening; failures are ignored.
#[cfg(unix)]
fn tighten_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE));
}

/// Best-effort permission tightening; failures are ignored.
#[cfg(unix)]
pub(crate) fn tighten_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE));
}

#[cfg(not(unix))]
fn tighten_dir(_dir: &Path) {}

#[cfg(not(unix))]
pub(crate) fn tighten_file(_path: &Path) {}

#[cfg(test)]
mod tests;
