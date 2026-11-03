//! MCP server (stdio) tests: drive [`Server::handle_line`] directly with
//! JSON-RPC lines — no subprocess, no network, in-memory DB, temp
//! workspace. The subprocess end-to-end lives in
//! `crates/hatch-cli/tests/mcp_server_e2e.rs`.

use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::now;
use hatch_mcp::Server;
use hatch_policy::{ApprovalQueue, ApprovalStatus, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{
    execute_approved, CreateEventExecutor, ExecutorRegistry, SendEmailExecutor, Workspace,
    WriteFileExecutor,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

struct Env {
    server: Server,
    db: Arc<Db>,
    exec: ExecutorRegistry,
    dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws = Workspace::new(dir.path()).unwrap();
    let reg = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(ws),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 5,
        },
    );
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    let exec = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(SendEmailExecutor::new(outbox, "me@126.com")))
        .with(Arc::new(CreateEventExecutor));
    let server = Server::new(
        reg,
        exec.clone(),
        PolicyConfig::default(),
        Arc::clone(&db),
        chrono_tz::Asia::Shanghai,
        "me@126.com".into(),
        Duration::from_secs(10),
    );
    Env {
        server,
        db,
        exec,
        dir,
    }
}

/// Send one JSON-RPC request line, parse the reply.
fn rpc(server: &Server, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let reply = server
        .handle_line(&line)
        .expect("server must reply to requests");
    serde_json::from_str(&reply).unwrap()
}

fn call_tool(server: &Server, id: u64, name: &str, args: Value) -> Value {
    rpc(
        server,
        id,
        "tools/call",
        json!({"name": name, "arguments": args}),
    )
}

#[test]
fn initialize_handshake() {
    let e = env();
    let r = rpc(
        &e.server,
        1,
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(r["result"]["serverInfo"]["name"], "hatch");
    assert!(r["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn tools_list_matches_registry() {
    let e = env();
    let r = rpc(&e.server, 1, "tools/list", json!({}));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "memory_search",
        "memory_add",
        "fs_write",
        "fs_read",
        "approval_status",
        "propose_email",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    // No MCP-client tools and no skill-management tools are exposed.
    assert!(!names.iter().any(|n| n.starts_with("mcp_")), "{names:?}");
    assert!(
        !names.contains(&"list_skills") && !names.contains(&"load_skill"),
        "{names:?}"
    );
    assert_eq!(names.len(), e.server.tool_names().len());
}

#[test]
fn read_only_call_succeeds() {
    let e = env();
    let r = call_tool(
        &e.server,
        1,
        "memory_search",
        json!({"query": "zzz-no-such-memory"}),
    );
    assert_eq!(r["result"]["structuredContent"]["status"], "ok");
    assert_eq!(r["result"]["structuredContent"]["trust"], "system");
    assert_eq!(r["result"]["content"][0]["text"], "没有找到相关记忆。");
    assert_eq!(r["result"]["isError"], false);
}

#[test]
fn write_call_queues_approval_and_does_not_execute() {
    let e = env();
    let r = call_tool(
        &e.server,
        1,
        "fs_write",
        json!({"path": "note.txt", "content": "hello"}),
    );
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["status"], "approval_required", "{r}");
    let id = sc["approval_id"].as_str().expect("approval id present");
    assert!(!sc["reasons"].as_array().unwrap().is_empty(), "{r}");
    assert_eq!(r["result"]["isError"], false);
    assert!(
        !e.dir.path().join("note.txt").exists(),
        "queued write must not touch the file"
    );
    let a = ApprovalQueue::new(&e.db).get(id).unwrap();
    assert_eq!(a.status, ApprovalStatus::Pending);
    assert_eq!(a.origin, hatch_mcp::MCP_SERVER_ORIGIN);
}

#[test]
fn approval_then_poll_completes() {
    let e = env();
    let r = call_tool(
        &e.server,
        1,
        "fs_write",
        json!({"path": "note.txt", "content": "hello"}),
    );
    let id = r["result"]["structuredContent"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Poll before approval: pending.
    let r = call_tool(&e.server, 2, "approval_status", json!({"id": id}));
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pending"),
        "{r}"
    );
    // The user approves (out of band, e.g. `hatch approvals approve`) and
    // the approved action runs; the file appears.
    let now = now();
    ApprovalQueue::new(&e.db)
        .decide(&id, true, "test", &now)
        .unwrap();
    let out = execute_approved(&e.db, &e.exec, &id, &now).unwrap();
    assert!(
        matches!(out, hatch_tools::ExecOutcome::Executed(_)),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(e.dir.path().join("note.txt")).unwrap(),
        "hello"
    );
    // Poll again: executed, with the result note.
    let r = call_tool(&e.server, 3, "approval_status", json!({"id": id}));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("executed"), "{text}");
}

#[test]
fn unknown_tool_is_a_json_rpc_error() {
    let e = env();
    let r = call_tool(&e.server, 1, "nope", json!({}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
}

#[test]
fn malformed_json_and_unknown_method_are_handled() {
    let e = env();
    let reply = e.server.handle_line("{not json").unwrap();
    let r: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(r["error"]["code"], -32700);
    assert!(r["id"].is_null());
    let r = rpc(&e.server, 1, "resources/subscribe", json!({}));
    assert_eq!(r["error"]["code"], -32601, "{r}");
    // A notification (no id) gets no reply at all.
    assert!(e
        .server
        .handle_line(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .is_none());
}

#[test]
fn resources_list_and_read_are_real() {
    let e = env();
    let r = rpc(&e.server, 1, "resources/list", json!({}));
    let uris: Vec<&str> = r["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, vec!["hatch://people", "hatch://memory"]);
    let r = rpc(
        &e.server,
        2,
        "resources/read",
        json!({"uri": "hatch://people"}),
    );
    let text = r["result"]["contents"][0]["text"].as_str().unwrap();
    let items: Vec<Value> = serde_json::from_str(text).unwrap();
    assert!(items.is_empty(), "no people in a fresh db");
    let r = rpc(
        &e.server,
        3,
        "resources/read",
        json!({"uri": "hatch://nope"}),
    );
    assert_eq!(r["error"]["code"], -32602, "{r}");
}

#[test]
fn prompts_list_is_honestly_empty() {
    let e = env();
    let r = rpc(&e.server, 1, "prompts/list", json!({}));
    assert_eq!(r["result"]["prompts"], json!([]));
}
