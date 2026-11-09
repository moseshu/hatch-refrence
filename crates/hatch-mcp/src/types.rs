//! MCP server configuration and the tool/resource/prompt descriptors
//! returned by [`Client::list_tools`], [`Client::list_resources`] and
//! [`Client::list_prompts`].
//!
//! `env` and `token_ref` hold **names only** — never secret values. The caller
//! resolves the names to values and passes the resolved map into
//! [`Client::connect`](crate::Client::connect).

use serde::{Deserialize, Serialize};

/// How to reach one MCP server: `stdio` (spawn a child process) or `http`
/// (POST JSON-RPC 2.0 to `url`, accepting a plain JSON reply or an SSE
/// stream). `env` and `token_ref` hold **names only** — never secret values.
/// The caller resolves the names to values and passes the resolved maps
/// into [`Client::connect`](crate::Client::connect) /
/// [`HttpClient::connect`](crate::HttpClient::connect).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Logical name used in tool names (`mcp_<name>_<tool>`) and lookups.
    pub name: String,
    /// `stdio` (default) or `http`.
    pub transport: String,
    /// Command to spawn (stdio only; resolved by the caller; no shell).
    pub command: String,
    pub args: Vec<String>,
    /// Names of environment variables the server needs. Stdio: passed to
    /// the child process. Http: sent as extra request headers under these
    /// names (the caller resolves names to values).
    pub env: Vec<String>,
    /// HTTP endpoint (http transport only), e.g. `http://127.0.0.1:8080/mcp`.
    pub url: String,
    /// Names of environment variables sent as extra HTTP headers (http
    /// transport only). `Authorization` is set from `token_ref` when
    /// present and must not be listed here.
    pub headers: Vec<String>,
    /// Name of a token in the vault (`hatch-vault`) used as this server's
    /// auth token, if any. Stdio: passed to the child as `MCP_TOKEN`.
    /// Http: sent as `Authorization: Bearer <token>`. The caller resolves
    /// it from the vault; the token value is never stored here.
    pub token_ref: Option<String>,
    /// OAuth login for a remote HTTP server (PKCE + static client id;
    /// DCR is a later step). When set, the client logs in via
    /// `hatch mcp login <name>` and refreshes tokens automatically;
    /// `token_ref` is ignored for this server.
    pub oauth: Option<crate::oauth::OAuthClientConfig>,
    /// Per-read timeout in seconds for JSON-RPC replies.
    pub timeout_secs: u64,
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        McpServerConfig {
            name: String::new(),
            transport: "stdio".into(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: String::new(),
            headers: Vec::new(),
            token_ref: None,
            oauth: None,
            timeout_secs: 30,
            enabled: false,
        }
    }
}

/// One tool offered by an MCP server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The tool's JSON input schema (an object schema; `{}` when absent).
    pub input_schema: serde_json::Value,
}

/// One resource offered by an MCP server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

/// One prompt template offered by an MCP server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
}
