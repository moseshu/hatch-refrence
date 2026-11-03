//! Tiny fake MCP server for tests: reads newline-delimited JSON-RPC on
//! stdin, answers on stdout. Notifications (no `id`) get no reply.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

fn reply(id: &Value, result: Value) {
    println!("{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn error(id: &Value, code: i64, message: &str) {
    println!(
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
    );
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": "Parse error"}})
                );
                continue;
            }
        };
        // Notification: no `id`, no reply.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => reply(
                &id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0.1"},
                }),
            ),
            "ping" => reply(&id, json!({})),
            "tools/list" => reply(
                &id,
                json!({"tools": [
                    {"name": "echo", "description": "echoes back arguments",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "add", "description": "adds a and b",
                     "inputSchema": {"type": "object", "properties": {
                        "a": {"type": "number"}, "b": {"type": "number"}},
                        "required": ["a", "b"]}},
                ]}),
            ),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match name {
                    "echo" => reply(
                        &id,
                        json!({"content": [{"type": "text", "text": serde_json::to_string(&args).unwrap()}]}),
                    ),
                    "add" => {
                        let a = params["arguments"]["a"].as_f64().unwrap_or(0.0);
                        let b = params["arguments"]["b"].as_f64().unwrap_or(0.0);
                        let sum = a + b;
                        let text = if sum.fract() == 0.0 {
                            format!("{}", sum as i64)
                        } else {
                            format!("{sum}")
                        };
                        reply(&id, json!({"content": [{"type": "text", "text": text}]}))
                    }
                    _ => error(&id, -32602, &format!("unknown tool {name}")),
                }
            }
            "resources/list" => reply(&id, json!({"resources": []})),
            "resources/read" => error(&id, -32602, "no such resource"),
            "prompts/list" => reply(&id, json!({"prompts": []})),
            _ => error(&id, -32601, &format!("Method not found: {method}")),
        }
        let _ = out.flush();
    }
}
