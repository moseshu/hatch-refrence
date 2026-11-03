//! One handle for either MCP transport. Callers that do not care how the
//! server is reached use [`AnyClient`]; the stdio [`Client`](crate::Client)
//! and HTTP [`HttpClient`](crate::HttpClient) keep their own `connect`
//! signatures underneath.

use crate::client::Client;
use crate::http::HttpClient;
use crate::types::{McpServerConfig, PromptInfo, ResourceInfo, ToolInfo};
use hatch_core::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;

/// An MCP client over stdio or HTTP, chosen by `cfg.transport`.
pub enum AnyClient {
    Stdio(Client),
    Http(HttpClient),
}

impl AnyClient {
    /// Connect using `cfg.transport` (`stdio` by default, `http` when set).
    /// `resolved_env` maps the env-var *names* in `cfg.env` to values
    /// (stdio child env); `resolved_headers` maps the header *names* in
    /// `cfg.headers` to values (HTTP request headers). `token`, when
    /// `Some`, becomes the stdio child's `MCP_TOKEN` or the HTTP
    /// `Authorization: Bearer` header, and is never logged.
    pub fn connect(
        cfg: &McpServerConfig,
        resolved_env: &HashMap<String, String>,
        resolved_headers: &HashMap<String, String>,
        token: Option<&str>,
    ) -> Result<Self> {
        match cfg.transport.as_str() {
            "http" => Ok(AnyClient::Http(HttpClient::connect(
                cfg,
                resolved_headers,
                token,
            )?)),
            "stdio" | "" => Ok(AnyClient::Stdio(Client::connect(cfg, resolved_env, token)?)),
            other => Err(Error::Config(format!(
                "mcp server {}: unknown transport `{other}` (stdio|http)",
                cfg.name
            ))),
        }
    }

    pub fn ping(&mut self) -> Result<()> {
        match self {
            AnyClient::Stdio(c) => c.ping(),
            AnyClient::Http(c) => c.ping(),
        }
    }

    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        match self {
            AnyClient::Stdio(c) => c.list_tools(),
            AnyClient::Http(c) => c.list_tools(),
        }
    }

    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        match self {
            AnyClient::Stdio(c) => c.call_tool(name, args),
            AnyClient::Http(c) => c.call_tool(name, args),
        }
    }

    pub fn list_resources(&mut self) -> Result<Vec<ResourceInfo>> {
        match self {
            AnyClient::Stdio(c) => c.list_resources(),
            AnyClient::Http(c) => c.list_resources(),
        }
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        match self {
            AnyClient::Stdio(c) => c.read_resource(uri),
            AnyClient::Http(c) => c.read_resource(uri),
        }
    }

    pub fn list_prompts(&mut self) -> Result<Vec<PromptInfo>> {
        match self {
            AnyClient::Stdio(c) => c.list_prompts(),
            AnyClient::Http(c) => c.list_prompts(),
        }
    }
}
