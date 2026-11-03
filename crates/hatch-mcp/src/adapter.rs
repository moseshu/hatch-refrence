//! Adapter: expose an MCP server's tools as hatch [`Tool`]s and run approved
//! MCP calls through an [`ActionExecutor`].
//!
//! Safety shape, matching the rest of the workspace:
//!
//! * [`McpTool::call`] never runs anything. It only exists so the tool shows
//!   up in the model-facing registry; the real work goes through
//!   [`McpTool::to_approval`], which defers to the approval queue.
//! * [`McpCallExecutor`] runs only payloads the user already approved.
//! * Everything an MCP server returns is **untrusted content**: the server is
//!   a third-party process and its output may contain prompt-injection text.
//!   This executor returns the raw JSON string; the dispatch layer (not this
//!   crate) is responsible for marking MCP results `Trust::Untrusted` before
//!   they reach the model or any other tool.

use crate::transport::AnyClient;
use crate::ToolInfo;
use chrono::Duration;
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, NewApproval};
use hatch_tools::executors::ExecContext;
use hatch_tools::{ActionExecutor, PlannedAction, Tool, ToolContext, ToolOutput};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One MCP tool, wrapped for the hatch tool registry.
///
/// `spec().name` is `mcp_<server>_<tool>`; the description is the server's
/// description prefixed with the server name.
pub struct McpTool {
    pub server: String,
    pub info: ToolInfo,
    pub client: Arc<Mutex<AnyClient>>,
}

impl McpTool {
    pub fn new(server: &str, info: ToolInfo, client: Arc<Mutex<AnyClient>>) -> Self {
        McpTool {
            server: server.to_string(),
            info,
            client,
        }
    }

    /// The registry name for a server tool.
    pub fn tool_name(server: &str, tool: &str) -> String {
        format!("mcp_{server}_{tool}")
    }
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::tool_name(&self.server, &self.info.name),
            description: format!("[{}] {}", self.server, self.info.description),
            parameters: self.info.input_schema.clone(),
        }
    }

    /// Safe default: an MCP call is treated as a fetch. The approval path
    /// (not this class) is what decides whether a particular call may run.
    fn class(&self) -> ActionClass {
        ActionClass::FetchWeb
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            summary: format!("MCP {} tool {} with {}", self.server, self.info.name, args),
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid(format!(
            "mcp tool {} must go through approval (kind `mcp_call`), not a direct call",
            self.name()
        )))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let payload = json!({"server": self.server, "tool": self.info.name, "arguments": args});
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id(
                "mcp",
                &[&self.server, &self.info.name, &payload.to_string()],
            ),
            kind: "mcp_call".into(),
            class: ActionClass::FetchWeb,
            title: format!("MCP {}: {}", self.server, self.info.name),
            preview: format!(
                "MCP 服务器 {}\n工具 {}\n参数：{}",
                self.server, self.info.name, args
            ),
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

/// Build one registry-ready MCP tool.
pub fn mcp_tool(server: &str, info: ToolInfo, client: Arc<Mutex<AnyClient>>) -> Arc<dyn Tool> {
    Arc::new(McpTool::new(server, info, client))
}

/// Runs approved `mcp_call` payloads against connected MCP servers.
///
/// Payload shape (written by [`McpTool::to_approval`]):
/// `{"server": "<name>", "tool": "<name>", "arguments": {...}}`.
/// Returns the tool result as a JSON string.
pub struct McpCallExecutor {
    pub clients: Arc<Mutex<HashMap<String, Arc<Mutex<AnyClient>>>>>,
}

impl McpCallExecutor {
    pub fn new() -> Self {
        McpCallExecutor {
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_clients(clients: Arc<Mutex<HashMap<String, Arc<Mutex<AnyClient>>>>>) -> Self {
        McpCallExecutor { clients }
    }

    /// Register (or replace) the live client for one server name.
    pub fn register(&self, server: &str, client: Arc<Mutex<AnyClient>>) {
        if let Ok(mut map) = self.clients.lock() {
            map.insert(server.to_string(), client);
        }
    }
}

impl Default for McpCallExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionExecutor for McpCallExecutor {
    fn kind(&self) -> &'static str {
        "mcp_call"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let server = payload
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("mcp_call payload has no server".into()))?;
        let tool = payload
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("mcp_call payload has no tool".into()))?;
        let args = payload
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let client = {
            let map = self
                .clients
                .lock()
                .map_err(|_| Error::Invalid("mcp client map lock poisoned".into()))?;
            map.get(server).cloned().ok_or_else(|| {
                Error::Invalid(format!("no connected MCP server named `{server}`"))
            })?
        };
        let mut client = client
            .lock()
            .map_err(|_| Error::Invalid(format!("mcp client `{server}` lock poisoned")))?;
        let result = client.call_tool(tool, args)?;
        // NOTE: the returned string is untrusted server output. Callers must
        // treat it as Trust::Untrusted; this executor does not launder it.
        serde_json::to_string(&result).map_err(Error::from)
    }
}
