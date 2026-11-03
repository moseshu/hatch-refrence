//! Adapter: expose connector ops as hatch [`Tool`]s and run approved
//! connector calls through an [`ActionExecutor`].
//!
//! Safety shape, matching the MCP adapter:
//!
//! * [`ConnectorTool::call`] never runs anything. It only exists so the
//!   tool shows up in the model-facing registry; the real work goes
//!   through [`ConnectorTool::to_approval`], which defers to the approval
//!   queue. Read-only ops and side-effecting ops both go through the
//!   queue; the gate decides whether a particular call may run
//!   (side-effecting ops use class [`ActionClass::ExecCommand`]).
//! * [`ConnectorCallExecutor`] runs only payloads the user already
//!   approved. When the manifest names a `token_ref`, the call is spawned
//!   through the vault helper (`inject`): the token is set as an env var
//!   of the connector child only and the agent process never sees it.
//!   Without a `token_ref` the runner spawns the child directly.
//! * Everything a connector returns is **untrusted content**: the
//!   connector is a third-party process and its output may contain
//!   prompt-injection text. This executor returns the raw JSON string;
//!   `connector_call` is in `UNTRUSTED_RESULT_KINDS`, so dispatch marks
//!   results `Trust::Untrusted` before they reach the model.

use chrono::Duration;
use hatch_connectors::framework::{self, ConnectorManifest};
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, NewApproval};
use crate::executors::ExecContext;
use crate::{ActionExecutor, PlannedAction, Tool, ToolContext, ToolOutput};
use hatch_vault::VaultClient;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One connector op, wrapped for the hatch tool registry.
///
/// `spec().name` is `connector_<connector>_<op>`.
pub struct ConnectorTool {
    pub connector: String,
    pub op: String,
    pub description: String,
    pub parameters: Value,
    pub read_only: bool,
}

impl ConnectorTool {
    pub fn new(manifest: &ConnectorManifest, op: &str) -> Option<Self> {
        let def = manifest.op(op)?;
        Some(ConnectorTool {
            connector: manifest.name.clone(),
            op: def.name.clone(),
            description: def.description.clone(),
            parameters: def.args_schema.clone(),
            read_only: def.read_only,
        })
    }

    /// The registry name for a connector op.
    pub fn tool_name(connector: &str, op: &str) -> String {
        format!("connector_{connector}_{op}")
    }
}

impl Tool for ConnectorTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::tool_name(&self.connector, &self.op),
            description: format!("[{}] {}", self.connector, self.description),
            parameters: self.parameters.clone(),
        }
    }

    fn class(&self) -> ActionClass {
        if self.read_only {
            ActionClass::FetchWeb
        } else {
            ActionClass::ExecCommand
        }
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            class: Some(self.class()),
            summary: format!("connector {} op {} with {}", self.connector, self.op, args),
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid(format!(
            "connector tool {} must go through approval (kind `connector_call`), not a direct call",
            self.name()
        )))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let payload =
            json!({"connector": self.connector, "op": self.op, "arguments": args});
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id(
                "connector",
                &[&self.connector, &self.op, &payload.to_string()],
            ),
            kind: "connector_call".into(),
            class: self.class(),
            title: format!("连接器 {}：{}", self.connector, self.op),
            preview: format!(
                "连接器 {}\n操作 {}\n参数：{}",
                self.connector, self.op, args
            ),
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

/// Build one registry-ready connector tool.
pub fn connector_tool(manifest: &ConnectorManifest, op: &str) -> Option<Arc<dyn Tool>> {
    ConnectorTool::new(manifest, op).map(|t| Arc::new(t) as Arc<dyn Tool>)
}

/// All tools for all manifests found in `dir` (bad manifests are skipped
/// with a warning on stderr, never fatal).
pub fn connector_tools(dir: &Path) -> Vec<Arc<dyn Tool>> {
    let (manifests, warnings) = framework::load_dir(dir);
    for w in warnings {
        eprintln!("（连接器清单警告：{w}）");
    }
    let mut out = Vec::new();
    for m in &manifests {
        for op in &m.ops {
            if let Some(t) = connector_tool(m, &op.name) {
                out.push(t);
            }
        }
    }
    out
}

/// Runs approved `connector_call` payloads against installed connectors.
///
/// Payload shape (written by [`ConnectorTool::to_approval`]):
/// `{"connector": "<name>", "op": "<name>", "arguments": {...}}`.
/// Returns the op result as a JSON string.
pub struct ConnectorCallExecutor {
    pub home: PathBuf,
    pub dir: PathBuf,
    vault: Mutex<Option<Arc<VaultClient>>>,
}

impl ConnectorCallExecutor {
    pub fn new(home: &Path) -> Self {
        let dir = framework::default_dir(home);
        ConnectorCallExecutor { home: home.to_path_buf(), dir, vault: Mutex::new(None) }
    }

    pub fn with_dir(home: &Path, dir: &Path) -> Self {
        ConnectorCallExecutor { home: home.to_path_buf(), dir: dir.to_path_buf(), vault: Mutex::new(None) }
    }

    /// The vault helper client, started lazily on first token use.
    fn vault(&self) -> Result<Arc<VaultClient>> {
        let mut slot = self
            .vault
            .lock()
            .map_err(|_| Error::Invalid("connector vault lock poisoned".into()))?;
        if let Some(v) = slot.as_ref() {
            return Ok(v.clone());
        }
        let client = Arc::new(VaultClient::start(&self.home)?);
        *slot = Some(client.clone());
        Ok(client)
    }
}

impl ActionExecutor for ConnectorCallExecutor {
    fn kind(&self) -> &'static str {
        "connector_call"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let connector = payload
            .get("connector")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("connector_call payload has no connector".into()))?;
        let op = payload
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("connector_call payload has no op".into()))?;
        let args = payload.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let manifest = framework::find(&self.dir, connector)?;
        let (env, _missing) = framework::resolve_env(&manifest);
        let result = match &manifest.token_ref {
            Some(token_name) => {
                // Spawn through the vault helper: the token is injected
                // into the child's env only; this process never sees it.
                let vault = self.vault()?;
                let line = framework::request_line(&manifest, op, &args)?;
                let out = vault.inject(
                    token_name,
                    &manifest.token_env,
                    &manifest.command,
                    &manifest.args,
                    &line,
                    manifest.timeout().as_secs(),
                )?;
                // The child must have answered with one protocol line.
                framework::parse_stdout(&out.stdout, &out.stderr)?
            }
            None => framework::call(&manifest, op, &args, &env)?,
        };
        // NOTE: the returned string is untrusted connector output.
        // Callers must treat it as Trust::Untrusted; this executor does
        // not launder it.
        serde_json::to_string(&result).map_err(Error::from)
    }
}

