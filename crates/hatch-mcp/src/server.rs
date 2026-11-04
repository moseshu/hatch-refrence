//! MCP server (stdio): expose the hatch tool registry over JSON-RPC 2.0
//! (`initialize`, `tools/list`, `tools/call`, `resources/list`,
//! `resources/read`, `prompts/list`; protocol `2024-11-05`).
//!
//! What is exposed, and what is not:
//!
//! * Exposed: whatever [`Registry`] the caller builds — the CLI builds it
//!   from the built-in tools + file/shell/web tools + connector tools
//!   (`connector_<name>_<op>`) + browser tools when enabled. The
//!   `approval_status` tool in that set is the polling mechanism: a
//!   queued call's approval id can be checked with it.
//! * NOT exposed: `mcp_<server>_<tool>` client tools (they need a live
//!   client connection and would nest approvals inside approvals) and the
//!   skill-management tools (`list_skills` / `load_skill`, turn-scoped).
//!
//! Enforcement: every `tools/call` goes through the same
//! [`dispatch_call`](hatch_tools::dispatch_call) path as a local chat
//! turn — plan, gatekeeper, approval queue, executor. The server context
//! is marked **tainted** (the MCP caller is untrusted input), so a grant
//! can only auto-execute a call when it explicitly allows tainted
//! context, and never for commands. A call that needs approval does NOT
//! execute: the reply is a normal result with
//! `structuredContent.status == "approval_required"`,
//! `structuredContent.approval_id` and the gate's `reasons`; the user
//! approves with `hatch approvals approve <id>` and the caller polls
//! `approval_status`. Read-only calls execute directly. Tool outputs keep
//! their trust label in `structuredContent.trust`; anything from outside
//! hatch is `untrusted`. A widget-producing tool (e.g. `show_options`)
//! also puts its card JSON in `structuredContent.widget`, next to the
//! token: a conforming client renders the card only when the embed token
//! is in the message text, otherwise the widget is inert and only the
//! text in `content` shows.
//!
//! Resources are real, not faked: `hatch://people` (the people table)
//! and `hatch://memory` (current memory claims), each capped at 100
//! items. `prompts/list` honestly returns an empty list.
//!
//! Transport notes: newline-delimited JSON on stdin/stdout. Each
//! `tools/call` runs on a worker thread with a per-call timeout; on
//! timeout the reply is JSON-RPC error `-32000` and the worker is left
//! running detached (a timed-out call may still finish — check
//! `approval_status`). The loop ends cleanly on stdin EOF or when the
//! caller sets the shutdown flag (the CLI sets it from Ctrl-C / SIGTERM
//! via `ctrlc`). The same [`Server`] also backs the streamable-HTTP
//! transport in [`crate::http_server`], built with origin `mcp:http`
//! and protocol `2025-03-26` (see [`Server::with_origin`]): approvals
//! are isolated per transport — an HTTP caller can only poll approvals
//! it queued itself, never a stdio caller's, and vice versa.

use chrono_tz::Tz;
use hatch_core::{CancelToken, Result};
use hatch_llm::ToolCall;
use hatch_memory::{MemoryStore, People};
use hatch_policy::{ApprovalQueue, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::{dispatch_call, DispatchResult, ExecutorRegistry, Registry, ToolContext};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Protocol version this server negotiates.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Protocol version the streamable-HTTP transport negotiates (MCP spec
/// 2025-03-26, the version that introduced streamable HTTP). The HTTP
/// server always answers `initialize` with this version; stdio keeps
/// negotiating `2024-11-05`.
pub const HTTP_PROTOCOL_VERSION: &str = "2025-03-26";

/// Origin recorded on every approval the server queues, and the origin
/// `approval_status` checks against — so an MCP caller can only poll its
/// own approvals, never another session's.
pub const ORIGIN: &str = "mcp:serve";

/// Origin for approvals queued through the streamable-HTTP transport
/// (same isolation rule as [`ORIGIN`]).
pub const HTTP_ORIGIN: &str = "mcp:http";

const MAX_RESOURCE_ITEMS: usize = 100;

fn result_reply(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error_reply(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// The MCP server: a registry plus everything dispatch needs.
pub struct Server {
    registry: Registry,
    executors: ExecutorRegistry,
    gate_config: PolicyConfig,
    db: Arc<Db>,
    tz: Tz,
    user_email: String,
    call_timeout: Duration,
    /// Origin stamped on approvals (`mcp:serve` / `mcp:http`).
    /// `approval_status` only shows same-origin approvals.
    origin: String,
    /// Protocol version answered in `initialize` (`2024-11-05` for
    /// stdio, `2025-03-26` for streamable HTTP).
    protocol_version: &'static str,
}

impl Server {
    pub fn new(
        registry: Registry,
        executors: ExecutorRegistry,
        gate_config: PolicyConfig,
        db: Arc<Db>,
        tz: Tz,
        user_email: String,
        call_timeout: Duration,
    ) -> Self {
        Server {
            registry,
            executors,
            gate_config,
            db,
            tz,
            user_email,
            call_timeout,
            origin: ORIGIN.to_string(),
            protocol_version: PROTOCOL_VERSION,
        }
    }

    /// Use a different approval origin (the HTTP transport uses
    /// [`HTTP_ORIGIN`]).
    pub fn with_origin(mut self, origin: &str) -> Self {
        self.origin = origin.to_string();
        self
    }

    /// Use a different protocol version in `initialize` replies (the
    /// HTTP transport uses [`HTTP_PROTOCOL_VERSION`]).
    pub fn with_protocol_version(mut self, version: &'static str) -> Self {
        self.protocol_version = version;
        self
    }

    /// Tool names the server exposes (the registry's names, in order).
    pub fn tool_names(&self) -> Vec<String> {
        self.registry.names()
    }

    /// Handle one input line (`None` = notification, no reply).
    /// Malformed JSON gets a `-32700` reply, never a crash.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Some(error_reply(&Value::Null, -32700, "Parse error")),
        };
        let Some(id) = req.get("id").cloned() else {
            return None; // Notification: no reply.
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => Some(result_reply(
                &id,
                json!({
                    "protocolVersion": self.protocol_version,
                    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                    "serverInfo": {"name": "hatch", "version": "0.1.0"},
                }),
            )),
            "ping" => Some(result_reply(&id, json!({}))),
            "tools/list" => {
                let tools: Vec<Value> = self
                    .registry
                    .specs()
                    .iter()
                    .map(|s| json!({"name": s.name, "description": s.description, "inputSchema": s.parameters}))
                    .collect();
                Some(result_reply(&id, json!({"tools": tools})))
            }
            "tools/call" => match self.call_tool(&id, &params) {
                Ok(result) => Some(result_reply(&id, result)),
                Err((code, message)) => Some(error_reply(&id, code, &message)),
            },
            "resources/list" => Some(result_reply(
                &id,
                json!({"resources": Self::resource_list()}),
            )),
            "resources/read" => match self.read_resource(&params) {
                Ok(result) => Some(result_reply(&id, result)),
                Err((code, message)) => Some(error_reply(&id, code, &message)),
            },
            "prompts/list" => Some(result_reply(&id, json!({"prompts": []}))),
            "" => Some(error_reply(&id, -32600, "Invalid Request: no method")),
            other => Some(error_reply(
                &id,
                -32601,
                &format!("Method not found: {other}"),
            )),
        }
    }

    fn resource_list() -> Vec<Value> {
        vec![
            json!({"uri": "hatch://people", "name": "people",
                   "description": "联系人（people 表，姓名/邮箱/重要程度）", "mimeType": "application/json"}),
            json!({"uri": "hatch://memory", "name": "memory",
                   "description": "当前长期记忆 claims（主题/内容/来源/信任标签）", "mimeType": "application/json"}),
        ]
    }

    fn read_resource(&self, params: &Value) -> std::result::Result<Value, (i64, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or((-32602, "resources/read needs a string `uri`".to_string()))?;
        let text = match uri {
            "hatch://people" => {
                let people = People::new(&self.db)
                    .list()
                    .map_err(|e| (-32603, e.to_string()))?;
                let items: Vec<Value> = people
                    .iter()
                    .take(MAX_RESOURCE_ITEMS)
                    .map(|p| {
                        json!({"email": p.email, "name": p.name, "aliases": p.aliases,
                               "importance": p.importance.as_str(), "notes": p.notes,
                               "interactions": p.interactions})
                    })
                    .collect();
                serde_json::to_string(&items).map_err(|e| (-32603, e.to_string()))?
            }
            "hatch://memory" => {
                let claims = MemoryStore::new(&self.db)
                    .current(&hatch_core::time::now())
                    .map_err(|e| (-32603, e.to_string()))?;
                let items: Vec<Value> = claims
                    .iter()
                    .take(MAX_RESOURCE_ITEMS)
                    .map(|c| {
                        json!({"id": c.id, "subject": c.subject, "text": c.text, "tags": c.tags,
                               "source": c.source, "trust": c.trust.as_str(), "confidence": c.confidence})
                    })
                    .collect();
                serde_json::to_string(&items).map_err(|e| (-32603, e.to_string()))?
            }
            other => return Err((-32602, format!("unknown resource {other}"))),
        };
        Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
    }

    /// One `tools/call` on a worker thread, cut off by `call_timeout`.
    fn call_tool(
        &self,
        req_id: &Value,
        params: &Value,
    ) -> std::result::Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call needs a string `name`".to_string()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let call = ToolCall {
            id: format!("mcp:{req_id}"),
            name: name.to_string(),
            arguments: args,
        };
        let (tx, rx) = mpsc::channel::<DispatchResult>();
        let reg = self.registry.clone();
        let exec = self.executors.clone();
        let gate = Gatekeeper::new(self.gate_config.clone());
        let db = Arc::clone(&self.db);
        let tz = self.tz;
        let user_email = self.user_email.clone();
        let origin = self.origin.clone();
        let call_id = call.id.clone();
        std::thread::spawn(move || {
            let ctx = ToolContext {
                db: &db,
                now: hatch_core::time::now(),
                tz,
                user_email,
                origin,
                // The MCP caller is untrusted input: taint the context so
                // grants can only auto-execute when they opt into tainted
                // context (never for commands).
                tainted: true,
                call_id,
                cancel: CancelToken::new(),
            };
            let r = dispatch_call(&reg, &exec, &gate, &call, &ctx);
            let _ = tx.send(r);
        });
        match rx.recv_timeout(self.call_timeout) {
            Ok(r) => self.tool_result(r),
            Err(mpsc::RecvTimeoutError::Timeout) => Err((
                -32000,
                format!(
                    "tool call timed out after {}s; it may still finish in the background — use approval_status to check whether an approval was queued",
                    self.call_timeout.as_secs()
                ),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err((-32603, "tool worker died before replying".into()))
            }
        }
    }

    /// Map a dispatch outcome onto an MCP `tools/call` result: errors
    /// become JSON-RPC errors, a queued approval a normal result with
    /// `status: "approval_required"` — never a silent execution.
    fn tool_result(&self, r: DispatchResult) -> std::result::Result<Value, (i64, String)> {
        if let Some(msg) = r.content.strip_prefix("ERROR: ") {
            let code = if msg.contains("unknown tool") {
                -32602
            } else {
                -32000
            };
            return Err((code, msg.to_string()));
        }
        let mut sc = json!({"status": "ok", "trust": r.trust.as_str()});
        if let Some(w) = &r.widget {
            // Widget payload (card JSON + embed token): a conforming
            // client renders it only when the token is in the message
            // text; otherwise it is inert and only `content` shows.
            sc["widget"] = w.clone();
        }
        if let Some(id) = &r.approval_id {
            sc["approval_id"] = json!(id);
            if r.content.starts_with("已提交审批") {
                sc["status"] = json!("approval_required");
                let reasons = ApprovalQueue::new(&self.db)
                    .get(id)
                    .map(|a| a.reasons)
                    .unwrap_or_default();
                sc["reasons"] = json!(reasons);
            } else if r.content.starts_with("已执行") {
                sc["status"] = json!("executed");
            } else if r.content.starts_with("相同动作此前已处理") {
                sc["status"] = json!("already_handled");
            } else if let Ok(a) = ApprovalQueue::new(&self.db).get(id) {
                sc["status"] = json!(a.status.as_str());
            }
        }
        Ok(json!({
            "content": [{"type": "text", "text": r.content}],
            "structuredContent": sc,
            "isError": false,
        }))
    }

    /// Serve JSON-RPC 2.0 over stdin/stdout until stdin EOF or `shutdown`.
    /// Replies are written from this thread only; stdout is the protocol.
    pub fn run_stdio(&self, shutdown: &AtomicBool) -> Result<()> {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: the client went away.
                    Ok(_) => {
                        if tx.send(line.trim_end().to_string()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(reply) = self.handle_line(&line) {
                        writeln!(out, "{reply}")?;
                        out.flush()?;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}
