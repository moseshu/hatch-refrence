//! Connector manifest: the declarative description of one connector.
//!
//! A manifest is a TOML file, one per connector, stored under the
//! connectors directory (`$HATCH_HOME/connectors/<name>.toml`, or
//! `$HATCH_CONNECTORS_DIR` when set). Example:
//!
//! ```toml
//! name = "notes"
//! command = "hatch-notes-connector"
//! args = []
//! timeout_secs = 30
//! connect_guidance = "Set HATCH_NOTES_DIR to a notes directory."
//!
//! [[ops]]
//! name = "list_notes"
//! description = "List note names."
//! read_only = true
//!
//! [[ops]]
//! name = "write_note"
//! description = "Write a note (needs approval)."
//! read_only = false
//! ```
//!
//! `command` is resolved by the caller (absolute path, or a name looked up
//! on `PATH`); no shell is involved. `token_ref` names a vault token; the
//! value is never written in the manifest. `token_env` is the env-var name
//! the vault helper injects the token into for the child (default
//! `CONNECTOR_TOKEN`).

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One operation a connector offers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OpDef {
    /// Op name, as used in the runner protocol (`op` field) and in tool
    /// names (`connector_<connector>_<op>`).
    pub name: String,
    /// Shown to the model and the user.
    pub description: String,
    /// Read-only ops may run after the gate; side-effecting ops always go
    /// through the approval queue.
    pub read_only: bool,
    /// JSON Schema for `args` (an object schema; `{}` when absent).
    pub args_schema: Value,
}

impl Default for OpDef {
    fn default() -> Self {
        OpDef {
            name: String::new(),
            description: String::new(),
            read_only: true,
            args_schema: Value::Object(Default::default()),
        }
    }
}

/// The manifest for one connector.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorManifest {
    /// Logical name: file stem, tool-name segment, lookup key.
    pub name: String,
    /// Command to spawn (absolute path or `PATH` lookup; no shell).
    pub command: String,
    /// Fixed arguments passed to the command on every call.
    pub args: Vec<String>,
    /// Names of environment variables the connector needs (values are
    /// resolved by the caller from the process env; never written here).
    pub env: Vec<String>,
    /// Name of a vault token injected into the child as `token_env`.
    pub token_ref: Option<String>,
    /// Env-var name the token is injected under. Default `CONNECTOR_TOKEN`.
    pub token_env: String,
    /// Per-call timeout in seconds (0 = default 30).
    pub timeout_secs: u64,
    /// Ops the connector offers. Must be non-empty.
    pub ops: Vec<OpDef>,
    /// Human guidance shown when the connector is unconnected or its
    /// status check fails: how to connect, which env var / vault token is
    /// missing. Never promises results.
    pub connect_guidance: String,
}

impl Default for ConnectorManifest {
    fn default() -> Self {
        ConnectorManifest {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            token_ref: None,
            token_env: "CONNECTOR_TOKEN".to_string(),
            timeout_secs: 30,
            ops: Vec::new(),
            connect_guidance: String::new(),
        }
    }
}

impl ConnectorManifest {
    /// Parse TOML text into a manifest and validate it.
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: ConnectorManifest =
            toml::from_str(text).map_err(|e| Error::Config(format!("invalid connector manifest: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate a manifest that was built or deserialized another way.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::Config("connector manifest needs a `name`".into()));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::Config(format!(
                "connector name `{}` must be [A-Za-z0-9_-]",
                self.name
            )));
        }
        if self.command.trim().is_empty() {
            return Err(Error::Config(format!(
                "connector `{}` needs a `command`",
                self.name
            )));
        }
        if self.ops.is_empty() {
            return Err(Error::Config(format!(
                "connector `{}` declares no ops",
                self.name
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for op in &self.ops {
            if op.name.is_empty() {
                return Err(Error::Config(format!(
                    "connector `{}` has an op with no name",
                    self.name
                )));
            }
            if !seen.insert(op.name.clone()) {
                return Err(Error::Config(format!(
                    "connector `{}` declares op `{}` twice",
                    self.name, op.name
                )));
            }
        }
        if let Some(t) = &self.token_ref {
            if !hatch_vault::is_valid_name(t) {
                return Err(Error::Config(format!(
                    "connector `{}`: invalid token_ref `{t}`",
                    self.name
                )));
            }
        }
        if self.token_env.trim().is_empty() {
            return Err(Error::Config(format!(
                "connector `{}`: `token_env` must not be empty",
                self.name
            )));
        }
        Ok(())
    }

    /// Find one op by name.
    pub fn op(&self, name: &str) -> Option<&OpDef> {
        self.ops.iter().find(|o| o.name == name)
    }

    /// Effective per-call timeout.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(if self.timeout_secs == 0 { 30 } else { self.timeout_secs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_toml() -> &'static str {
        r#"
        name = "notes"
        command = "hatch-notes-connector"
        timeout_secs = 10
        connect_guidance = "Set HATCH_NOTES_DIR."

        [[ops]]
        name = "list_notes"
        description = "List notes."
        read_only = true

        [[ops]]
        name = "write_note"
        description = "Write a note."
        read_only = false
        "#
    }

    #[test]
    fn parse_valid_manifest() {
        let m = ConnectorManifest::parse(manifest_toml()).unwrap();
        assert_eq!(m.name, "notes");
        assert_eq!(m.ops.len(), 2);
        assert!(m.op("list_notes").unwrap().read_only);
        assert!(!m.op("write_note").unwrap().read_only);
        assert_eq!(m.token_env, "CONNECTOR_TOKEN");
        assert_eq!(m.timeout().as_secs(), 10);
    }

    #[test]
    fn parse_rejects_empty_ops() {
        let err = ConnectorManifest::parse("name = \"x\"\ncommand = \"y\"\n").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn parse_rejects_duplicate_op() {
        let text = "name = \"x\"\ncommand = \"y\"\n[[ops]]\nname = \"a\"\n[[ops]]\nname = \"a\"\n";
        assert!(ConnectorManifest::parse(text).is_err());
    }

    #[test]
    fn parse_rejects_bad_name() {
        let text = "name = \"has space\"\ncommand = \"y\"\n[[ops]]\nname = \"a\"\n";
        assert!(ConnectorManifest::parse(text).is_err());
    }

    #[test]
    fn parse_rejects_bad_token_ref() {
        let text = "name = \"x\"\ncommand = \"y\"\ntoken_ref = \"has space\"\n[[ops]]\nname = \"a\"\n";
        assert!(ConnectorManifest::parse(text).is_err());
    }

    #[test]
    fn default_timeout_is_30s() {
        let mut m = ConnectorManifest::parse(manifest_toml()).unwrap();
        m.timeout_secs = 0;
        assert_eq!(m.timeout().as_secs(), 30);
    }
}
