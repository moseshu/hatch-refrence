//! A new app joins through the tool contract — and through nothing else.
//!
//! A mock notes app (one read tool, one write tool) registers in a
//! fresh [`Registry`] and behaves like every built-in tool: the read
//! runs straight through the dispatcher, the write can only run after
//! approval (dispatch queues it, a direct `call` refuses, approval +
//! `execute_approved` runs it exactly once), and a narrowed registry
//! can only lose tools, never gain them. If a future app cannot meet
//! this contract, it cannot join.

use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_llm::{ToolCall, ToolSpec};
use hatch_policy::{
    ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, NewApproval, PolicyConfig,
};
use hatch_store::Db;
use hatch_tools::{
    dispatch_call, execute_approved, ActionExecutor, ExecContext, ExecutorRegistry, PlannedAction,
    Registry, Tool, ToolContext, ToolOutput,
};
use serde_json::{json, Value};
use std::sync::Arc;

struct MockNotesRead;

impl Tool for MockNotesRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mock_notes_read".into(),
            description: "Read a note from the mock notes app.".into(),
            parameters: json!({"type":"object","properties":{"id":{"type":"string"}}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: format!(
                "note {}: 记得周五前确认预算",
                args["id"].as_str().unwrap_or("?")
            ),
            trust: Trust::System,
            widget: None,
        })
    }
}

struct MockNotesWrite;

// The mock notes app stores notes as files, so its write is a file
// write — Draft by default, which means it must be approved before
// anything is written.
impl Tool for MockNotesWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mock_notes_write".into(),
            description: "Write a note in the mock notes app.".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteFile
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            summary: format!("在 mock 笔记里写入「{}」", args["text"].as_str().unwrap_or("")),
            ..Default::default()
        })
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        Ok(Some(NewApproval {
            idempotency_key: format!("mock-notes-write:{}", ctx.call_id),
            kind: "mock_notes_write".into(),
            class: self.class(),
            title: "写入 mock 笔记".into(),
            preview: args["text"].as_str().unwrap_or("").into(),
            payload: args.clone(),
            reasons: vec!["写入文件需要你确认".into()],
            origin: ctx.origin.clone(),
            ttl: chrono::Duration::minutes(30),
        }))
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("mock_notes_write runs only through the approval queue".into()))
    }
}

/// The mock app's side effect: append the approved text to a file.
/// Only the executor may do this — never the tool's `call`.
struct MockNotesWriteExecutor {
    path: std::path::PathBuf,
}

impl ActionExecutor for MockNotesWriteExecutor {
    fn kind(&self) -> &'static str {
        "mock_notes_write"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let text = payload["text"].as_str().unwrap_or("");
        let mut out = std::fs::read_to_string(&self.path).unwrap_or_default();
        out.push_str(text);
        out.push('\n');
        std::fs::write(&self.path, out)?;
        Ok(format!("wrote {text}"))
    }
}

fn ctx<'a>(db: &'a Db, call_id: &str) -> ToolContext<'a> {
    ToolContext {
        db,
        now: hatch_core::time::now(),
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@example.com".into(),
        user_name: String::new(),
        origin: "chat:test".into(),
        tainted: false,
        call_id: call_id.into(),
        cancel: CancelToken::new(),
    }
}

#[test]
fn a_new_app_joins_through_the_tool_contract() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let notes = dir.path().join("notes.txt");
    let reg = Registry::new()
        .with(Arc::new(MockNotesRead))
        .with(Arc::new(MockNotesWrite));
    let exec = ExecutorRegistry::new().with(Arc::new(MockNotesWriteExecutor { path: notes.clone() }));
    let gate = Gatekeeper::new(PolicyConfig::default());

    // The tools are visible to the model, with their specs.
    assert!(reg.names().contains(&"mock_notes_read".to_string()));
    assert!(reg.names().contains(&"mock_notes_write".to_string()));
    assert_eq!(reg.get("mock_notes_read").unwrap().spec().name, "mock_notes_read");

    // A narrowed registry can only lose tools.
    let narrow = reg.filtered(&["mock_notes_read"]);
    assert_eq!(narrow.names(), vec!["mock_notes_read".to_string()]);
    assert!(narrow.get("mock_notes_write").is_none());

    // The read runs straight through the dispatcher.
    let read = dispatch_call(
        &reg,
        &exec,
        &gate,
        &ToolCall { id: "c1".into(), name: "mock_notes_read".into(), arguments: json!({"id": "7"}) },
        &ctx(&db, "c1"),
    );
    assert!(read.content.contains("记得周五前确认预算"), "{}", read.content);
    assert!(read.approval_id.is_none(), "a read needs no approval");

    // The write queues an approval and has no side effect yet.
    let write_call = ToolCall {
        id: "c2".into(),
        name: "mock_notes_write".into(),
        arguments: json!({"text": "预算已确认"}),
    };
    let write = dispatch_call(&reg, &exec, &gate, &write_call, &ctx(&db, "c2"));
    assert!(write.content.starts_with("已提交审批"), "{}", write.content);
    let approval_id = write.approval_id.clone().expect("the write must queue an approval");
    assert!(!notes.exists(), "nothing is written before approval");
    // Dispatching the same call again reuses the queued approval.
    let again = dispatch_call(&reg, &exec, &gate, &write_call, &ctx(&db, "c2"));
    assert_eq!(again.approval_id.as_deref(), Some(approval_id.as_str()));

    // A direct call bypassing the dispatcher refuses.
    let tool = reg.get("mock_notes_write").unwrap();
    assert!(tool.call(&write_call.arguments, &ctx(&db, "c2")).is_err());

    // Approve, run once, and a second run refuses.
    let q = ApprovalQueue::new(&db);
    let now = hatch_core::time::now();
    q.decide(&approval_id, true, "ok", &now).unwrap();
    execute_approved(&db, &exec, &approval_id, &now).unwrap();
    assert_eq!(q.get(&approval_id).unwrap().status, ApprovalStatus::Executed);
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "预算已确认\n");
    assert!(execute_approved(&db, &exec, &approval_id, &now).is_err());
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "预算已确认\n", "no second write");
}
