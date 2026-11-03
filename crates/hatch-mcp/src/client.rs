//! Stdio JSON-RPC 2.0 client for one MCP server.
//!
//! The server is spawned as a child process with piped stdin/stdout/stderr
//! and spoken to with newline-delimited JSON. Reads run on a background
//! thread and are collected with a timeout, so a hung server fails instead
//! of blocking the agent forever. A token (when the caller supplies one) is
//! passed to the child only as the `MCP_TOKEN` env var: never logged, never
//! stored in [`McpServerConfig`](crate::McpServerConfig) — the config holds
//! the token's *name* (`token_ref`) only.

use crate::types::{McpServerConfig, PromptInfo, ResourceInfo, ToolInfo};
use hatch_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Protocol version this client negotiates.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A connected MCP server process.
pub struct Client {
    name: String,
    child: Child,
    stdin: ChildStdin,
    /// Lines read from the server's stdout by the reader thread.
    rx: mpsc::Receiver<String>,
    next_id: u64,
    timeout: Duration,
}

/// Validate one JSON-RPC 2.0 response object: it must carry the request
/// `id`, must not carry an `error`, and its `result` is returned.
/// Shared by the stdio and HTTP clients.
pub(crate) fn check_rpc_response(resp: &Value, id: u64, method: &str, name: &str) -> Result<Value> {
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("?");
        return Err(Error::Connector(format!(
            "mcp server {name}: `{method}` failed ({code}): {msg}"
        )));
    }
    if resp.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(Error::Invalid(format!(
            "mcp server {name}: reply id mismatch for `{method}`"
        )));
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// A string field of a JSON object, `""` when absent.
pub(crate) fn field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// An array field of a JSON object, empty when absent.
pub(crate) fn items(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

impl Client {
    /// Spawn the server, run `initialize`, then send
    /// `notifications/initialized`. `resolved_env` maps the env-var *names*
    /// in `cfg.env` to their values (resolved by the caller). When `token`
    /// is `Some`, it is set as the child's `MCP_TOKEN` env var and never
    /// logged. One reconnect attempt is made (re-spawn + re-initialize
    /// once) on spawn/IO failure or initialize failure.
    pub fn connect(
        cfg: &McpServerConfig,
        resolved_env: &HashMap<String, String>,
        token: Option<&str>,
    ) -> Result<Self> {
        match Self::try_connect(cfg, resolved_env, token) {
            Ok(c) => Ok(c),
            Err(first) => Self::try_connect(cfg, resolved_env, token).map_err(|second| {
                Error::Connector(format!(
                    "mcp server {}: connect failed after one retry (first: {first}; retry: {second})",
                    cfg.name
                ))
            }),
        }
    }

    fn try_connect(
        cfg: &McpServerConfig,
        resolved_env: &HashMap<String, String>,
        token: Option<&str>,
    ) -> Result<Self> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(resolved_env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(t) = token {
            cmd.env("MCP_TOKEN", t);
        }
        let mut child = cmd.spawn().map_err(|e| {
            Error::Connector(format!(
                "mcp server {}: failed to spawn `{}`: {e}",
                cfg.name, cfg.command
            ))
        })?;
        let no = |what: &str| Error::Connector(format!("mcp server {}: no {what}", cfg.name));
        let stdin = child.stdin.take().ok_or_else(|| no("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| no("stdout"))?;
        // Drain stderr so a chatty server can never block on a full pipe.
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(err);
                let mut buf = String::new();
                while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                    buf.clear();
                }
            });
        }
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut line = String::new();
            while r.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        let secs = if cfg.timeout_secs == 0 {
            DEFAULT_TIMEOUT_SECS
        } else {
            cfg.timeout_secs
        };
        let timeout = Duration::from_secs(secs);
        let mut client = Client {
            name: cfg.name.clone(),
            child,
            stdin,
            rx,
            next_id: 1,
            timeout,
        };
        let init = client.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
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
        client.notify("notifications/initialized", None)?;
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

    /// Send one request and wait for its reply (up to `timeout`).
    fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            req["params"] = p;
        }
        self.write_line(&req.to_string())?;
        let line = self.recv_line(method)?;
        let resp: Value = serde_json::from_str(&line).map_err(|e| {
            Error::Invalid(format!(
                "mcp server {}: malformed JSON-RPC reply to `{method}`: {e}",
                self.name
            ))
        })?;
        check_rpc_response(&resp, id, method, &self.name)
    }

    /// Read one reply line, timing out per the configured timeout.
    fn recv_line(&self, method: &str) -> Result<String> {
        match self.rx.recv_timeout(self.timeout) {
            Ok(l) => Ok(l),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::Connector(format!(
                "mcp server {}: timed out after {}s waiting for `{method}` reply",
                self.name,
                self.timeout.as_secs()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Connector(format!(
                "mcp server {}: stdout closed (EOF) while waiting for `{method}`",
                self.name
            ))),
        }
    }

    /// Send a notification (no `id`, no reply expected).
    fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let mut req = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            req["params"] = p;
        }
        self.write_line(&req.to_string())
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Best effort: the child must not outlive the client.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mcp_tool;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Locate the fake server binary. `CARGO_BIN_EXE_*` is only set for
    /// integration tests, and plain `cargo test` builds only the bin's test
    /// harness, so build the plain binary on demand when it is missing.
    fn fake_server_path() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_fake-mcp-server") {
            return p.into();
        }
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let plain = manifest.join("../../target/debug/fake-mcp-server");
        if plain.is_file() {
            return plain;
        }
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = std::process::Command::new(&cargo)
            .args(["build", "-p", "hatch-mcp", "--bin", "fake-mcp-server"])
            .current_dir(manifest.parent().unwrap().parent().unwrap())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            ok && plain.is_file(),
            "could not build fake-mcp-server test binary"
        );
        plain
    }

    fn fake_cfg() -> McpServerConfig {
        McpServerConfig {
            name: "fake".into(),
            command: fake_server_path().to_string_lossy().into_owned(),
            args: vec![],
            env: vec![],
            token_ref: None,
            timeout_secs: 10,
            enabled: true,
            ..Default::default()
        }
    }

    fn connect_fake() -> Client {
        Client::connect(&fake_cfg(), &HashMap::new(), None).expect("connect to fake server")
    }

    #[test]
    fn connect_and_ping() {
        let mut c = connect_fake();
        c.ping().unwrap();
    }

    #[test]
    fn list_tools_returns_echo_and_add() {
        let mut c = connect_fake();
        let tools = c.list_tools().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["echo", "add"]);
        assert!(tools[0].description.contains("echoes"));
    }

    #[test]
    fn call_echo_round_trips() {
        let mut c = connect_fake();
        let args = json!({"hello": "world"});
        let r = c.call_tool("echo", args.clone()).unwrap();
        let text = r["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, serde_json::to_string(&args).unwrap());
    }

    #[test]
    fn call_add_returns_sum() {
        let mut c = connect_fake();
        let r = c.call_tool("add", json!({"a": 1, "b": 2})).unwrap();
        assert_eq!(r["content"][0]["text"].as_str().unwrap(), "3");
    }

    #[test]
    fn unknown_tool_returns_error() {
        let mut c = connect_fake();
        assert!(c.call_tool("nope", json!({})).is_err());
    }

    #[test]
    fn list_resources_and_prompts() {
        let mut c = connect_fake();
        assert!(c.list_resources().unwrap().is_empty());
        assert!(c.list_prompts().unwrap().is_empty());
        assert!(c.read_resource("fake://x").is_err());
    }

    #[test]
    fn connect_to_missing_command_fails() {
        let cfg = McpServerConfig {
            command: "/nonexistent/definitely-not-a-command".into(),
            ..fake_cfg()
        };
        let r = Client::connect(&cfg, &HashMap::new(), None);
        assert!(r.is_err(), "spawn of a missing command must fail honestly");
    }

    #[test]
    fn mcp_tool_spec_name() {
        let mut c = connect_fake();
        let info = c
            .list_tools()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "echo")
            .unwrap();
        let tool = mcp_tool(
            "fake",
            info,
            Arc::new(Mutex::new(crate::AnyClient::Stdio(c))),
        );
        assert_eq!(tool.spec().name, "mcp_fake_echo");
        assert_eq!(tool.name(), "mcp_fake_echo");
    }
}
