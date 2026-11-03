//! MCP client over HTTP: POST JSON-RPC 2.0 to the server URL (the MCP
//! streamable-HTTP pattern). The server may answer with a single JSON
//! object (`application/json`) or with an SSE stream (`text/event-stream`);
//! both are accepted, and `data:` events that are not the reply to our
//! request (server notifications, comments) are skipped. There is no
//! WebSocket transport: the workspace `tungstenite` dep is handshake-only,
//! and POST + SSE covers the pattern testable without a real provider.
//!
//! Auth: `resolved_headers` maps header *names* (from
//! [`McpServerConfig::headers`](crate::McpServerConfig::headers), resolved
//! by the caller from the environment) to values; when `token` is `Some`
//! it is sent as `Authorization: Bearer <token>`. Header values are never
//! logged and never stored in the config — the config holds names only.
//! HTTPS uses ureq's default rustls roots (same as the rest of the
//! workspace); plain `http://` loopback URLs are what the tests use.
//! No reconnect is attempted: an HTTP endpoint is stateless per request,
//! so a failed request is reported and the caller retries.

use crate::client::{check_rpc_response, field, items};
use crate::types::{McpServerConfig, PromptInfo, ResourceInfo, ToolInfo};
use hatch_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// An MCP server reached over HTTP.
pub struct HttpClient {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    token: Option<String>,
    timeout: Duration,
    agent: ureq::Agent,
    next_id: u64,
}

impl HttpClient {
    /// POST `initialize` to `cfg.url` and check the reply carries a
    /// `protocolVersion`. `resolved_headers` maps the header *names* in
    /// `cfg.headers` to their values (resolved by the caller). When
    /// `token` is `Some`, it is sent as a Bearer token and never logged.
    pub fn connect(
        cfg: &McpServerConfig,
        resolved_headers: &HashMap<String, String>,
        token: Option<&str>,
    ) -> Result<Self> {
        if cfg.url.trim().is_empty() {
            return Err(Error::Config(format!(
                "mcp server {}: http transport needs a `url`",
                cfg.name
            )));
        }
        let mut headers = Vec::new();
        for name in &cfg.headers {
            match resolved_headers.get(name) {
                Some(v) => headers.push((name.clone(), v.clone())),
                None => {
                    return Err(Error::Config(format!(
                        "mcp server {}: header {name} is not set in the environment",
                        cfg.name
                    )))
                }
            }
        }
        let secs = if cfg.timeout_secs == 0 {
            DEFAULT_TIMEOUT_SECS
        } else {
            cfg.timeout_secs
        };
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(secs))
            .build();
        let mut client = HttpClient {
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            headers,
            token: token.map(str::to_string),
            timeout: Duration::from_secs(secs),
            agent,
            next_id: 1,
        };
        let init = client.request(
            "initialize",
            Some(json!({
                "protocolVersion": crate::client::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "hatch", "version": "0.1.0"},
            })),
        )?;
        if init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(Error::Invalid(format!(
                "mcp server {}: initialize reply has no protocolVersion",
                client.name
            )));
        }
        Ok(client)
    }

    pub fn ping(&mut self) -> Result<()> {
        self.request("ping", None)?;
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let r = self.request("tools/list", Some(json!({})))?;
        Ok(items(&r, "tools")
            .iter()
            .map(|t| ToolInfo {
                name: field(t, "name"),
                description: field(t, "description"),
                input_schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
            })
            .collect())
    }

    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        self.request("tools/call", Some(json!({"name": name, "arguments": args})))
    }

    pub fn list_resources(&mut self) -> Result<Vec<ResourceInfo>> {
        let r = self.request("resources/list", Some(json!({})))?;
        Ok(items(&r, "resources")
            .iter()
            .map(|t| ResourceInfo {
                uri: field(t, "uri"),
                name: field(t, "name"),
                description: field(t, "description"),
                mime_type: t
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
            .collect())
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.request("resources/read", Some(json!({"uri": uri})))
    }

    pub fn list_prompts(&mut self) -> Result<Vec<PromptInfo>> {
        let r = self.request("prompts/list", Some(json!({})))?;
        Ok(items(&r, "prompts")
            .iter()
            .map(|t| PromptInfo {
                name: field(t, "name"),
                description: field(t, "description"),
            })
            .collect())
    }

    /// POST one JSON-RPC request and wait for its reply (up to `timeout`).
    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            req["params"] = p;
        }
        let body = req.to_string();
        let mut call = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            call = call.set(k, v);
        }
        if let Some(t) = &self.token {
            // The value is only ever placed on the wire, never logged.
            call = call.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = match call.send_string(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => {
                // The error body is not included: it is server output and
                // could contain anything; the status code is enough.
                return Err(Error::Connector(format!(
                    "mcp server {}: HTTP {code} from {} for `{method}`",
                    self.name, self.url
                )));
            }
            Err(ureq::Error::Transport(e)) => {
                return Err(Error::Connector(format!(
                    "mcp server {}: HTTP transport error for `{method}` after {}s: {e}",
                    self.name,
                    self.timeout.as_secs()
                )));
            }
        };
        let is_sse = resp.content_type().contains("text/event-stream");
        if is_sse {
            self.read_sse(resp.into_reader(), id, method)
        } else {
            let text = resp.into_string().map_err(|e| {
                Error::Connector(format!(
                    "mcp server {}: could not read `{method}` reply: {e}",
                    self.name
                ))
            })?;
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                Error::Invalid(format!(
                    "mcp server {}: malformed JSON-RPC reply to `{method}`: {e}",
                    self.name
                ))
            })?;
            check_rpc_response(&value, id, method, &self.name)
        }
    }

    /// Read an SSE stream until the event whose JSON-RPC `id` matches ours
    /// arrives. Comments, `event:` lines and events for other ids (server
    /// notifications) are skipped; a stream that ends without our reply is
    /// an error, not a silent empty result.
    fn read_sse(&self, reader: impl std::io::Read, id: u64, method: &str) -> Result<Value> {
        let mut data = String::new();
        for line in std::io::BufReader::new(reader).lines() {
            let line = line.map_err(|e| {
                Error::Connector(format!(
                    "mcp server {}: SSE read failed for `{method}`: {e}",
                    self.name
                ))
            })?;
            if line.starts_with(':') {
                continue; // SSE comment (e.g. `: ping`)
            }
            if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim_start());
                continue;
            }
            if line.trim().is_empty() {
                // Blank line: end of one event.
                if data.is_empty() {
                    continue;
                }
                let event = std::mem::take(&mut data);
                if let Some(v) = self.try_sse_event(&event, id, method)? {
                    return Ok(v);
                }
            }
            // `event:` lines and anything else are ignored.
        }
        // A final event without a trailing blank line still counts.
        if !data.is_empty() {
            if let Some(v) = self.try_sse_event(&data, id, method)? {
                return Ok(v);
            }
        }
        Err(Error::Connector(format!(
            "mcp server {}: SSE stream ended without a reply to `{method}`",
            self.name
        )))
    }

    /// Parse one SSE `data:` payload. `Ok(None)` = not our reply (a server
    /// notification or an event for another request); keep reading.
    fn try_sse_event(&self, data: &str, id: u64, method: &str) -> Result<Option<Value>> {
        let value: Value = serde_json::from_str(data).map_err(|e| {
            Error::Invalid(format!(
                "mcp server {}: malformed SSE JSON-RPC event for `{method}`: {e}",
                self.name
            ))
        })?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Ok(None);
        }
        check_rpc_response(&value, id, method, &self.name).map(Some)
    }
}
