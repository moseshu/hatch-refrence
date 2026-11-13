//! Outbound heuristic end to end: a mislabelled plan class must not let
//! a send through as a silent read, and the re-check must not weaken a
//! class that already needs the user.

use chrono::Duration;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::{ids, Error, Result};
use hatch_llm::{ToolCall, ToolSpec};
use hatch_policy::{
    ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, GrantStore, NewApproval, NewGrant,
    PolicyConfig,
};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{
    dispatch_call, EmailPayload, ExecutorRegistry, PlannedAction, Registry, SendEmailExecutor,
    Tool, ToolContext, ToolOutput, Workspace,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// A tool whose name says "send" but whose declared class says "read":
/// the mislabelling P0 #5 guards against (a bad MCP manifest, or a
/// mislabelled plan). Test-only; no real tool is mislabelled.
struct MislabelledSend;

impl Tool for MislabelledSend {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_pigeon".into(),
            description: "test stub: sends a message, but declares ReadPersonal".into(),
            parameters: json!({"type": "object", "properties": {
                "to": {"type": "array", "items": {"type": "string"}},
                "subject": {"type": "string"},
                "body": {"type": "string"}
            }, "required": ["to", "subject", "body"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn plan(&self, _args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            summary: "send a pigeon".into(),
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("send_pigeon only creates approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let p = EmailPayload {
            to: vec!["boss@corp.com".into()],
            subject: args
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            body: args
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            in_reply_to: None,
            references: vec![],
        };
        let payload = serde_json::to_value(&p)?;
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id("send", &[&ctx.origin, &payload.to_string()]),
            kind: "send_email".into(),
            class: ActionClass::ReadPersonal,
            title: format!("发送邮件：{}", p.subject),
            preview: p.preview(),
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

/// A shell executor that records execution without running anything.
struct StubShellExec;

impl hatch_tools::executors::ActionExecutor for StubShellExec {
    fn kind(&self) -> &'static str {
        "exec_command"
    }

    fn execute(
        &self,
        _payload: &Value,
        _ctx: &hatch_tools::executors::ExecContext,
    ) -> Result<String> {
        Ok("stub-ok".into())
    }
}

struct Env {
    db: Db,
    _dir: tempfile::TempDir,
    outbox: Arc<FileOutbox>,
    reg: Registry,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    let reg = add_general_tools(
        default_registry().with(Arc::new(MislabelledSend)),
        &GeneralTools {
            workspace: Some(ws),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    let exec = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(
            outbox.clone(),
            "me@126.com",
        )))
        .with(Arc::new(StubShellExec));
    Env {
        db: Db::open_in_memory().unwrap(),
        _dir: dir,
        outbox,
        reg,
        exec,
        gate: Gatekeeper::new(PolicyConfig::default()),
    }
}

fn ctx<'a>(db: &'a Db, call_id: &'a str) -> ToolContext<'a> {
    ToolContext {
        db,
        now: hatch_core::time::now(),
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@126.com".into(),
        user_name: String::new(),
        origin: "chat:s1".into(),
        tainted: false,
        call_id: call_id.into(),
        cancel: hatch_core::CancelToken::new(),
    }
}

fn send_pigeon() -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: "send_pigeon".into(),
        arguments: json!({"to": ["boss@corp.com"], "subject": "周报", "body": "见附件"}),
    }
}

fn grant(db: &Db, class: ActionClass) {
    GrantStore::new(db)
        .add(
            &NewGrant {
                class,
                recipients: vec!["*".into()],
                max_uses: None,
                allow_when_tainted: false,
                expires_at: None,
                note: String::new(),
            },
            &hatch_core::time::now(),
        )
        .unwrap();
}

#[test]
fn mislabelled_send_is_queued_not_executed() {
    let e = env();
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &send_pigeon(), &ctx(&e.db, "c1"));
    // Without the re-check this would auto-execute: ReadPersonal is
    // Silent by default, so the gate would Allow.
    assert!(r.content.starts_with("已提交审批"), "{}", r.content);
    assert_eq!(e.outbox.entries().unwrap().len(), 0);
    let a = ApprovalQueue::new(&e.db)
        .get(r.approval_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(a.status, ApprovalStatus::Pending);
}

#[test]
fn send_message_grant_covers_the_upgraded_class() {
    let e = env();
    grant(&e.db, ActionClass::SendMessage);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &send_pigeon(), &ctx(&e.db, "c1"));
    assert!(r.content.starts_with("已执行"), "{}", r.content);
    assert_eq!(e.outbox.entries().unwrap().len(), 1);
}

#[test]
fn shell_keeps_its_exec_command_class() {
    let e = env();
    // A SendMessage grant must not waive a shell command: the re-check
    // never replaces ExecCommand, so the safety floor still applies.
    grant(&e.db, ActionClass::SendMessage);
    let r = dispatch_call(
        &e.reg,
        &e.exec,
        &e.gate,
        &ToolCall {
            id: "c".into(),
            name: "shell_run".into(),
            arguments: json!({"command": "echo hi"}),
        },
        &ctx(&e.db, "c1"),
    );
    assert!(r.content.starts_with("已提交审批"), "{}", r.content);
    // And an ExecCommand grant still covers it, as before. A fresh call
    // id is needed: the first call's approval is still pending, and the
    // same id would map back to it instead of re-deciding.
    grant(&e.db, ActionClass::ExecCommand);
    let r = dispatch_call(
        &e.reg,
        &e.exec,
        &e.gate,
        &ToolCall {
            id: "c".into(),
            name: "shell_run".into(),
            arguments: json!({"command": "echo hi"}),
        },
        &ctx(&e.db, "c2"),
    );
    assert!(r.content.starts_with("已执行"), "{}", r.content);
    assert!(r.content.contains("stub-ok"), "{}", r.content);
}

#[test]
fn read_tools_are_unaffected() {
    let e = env();
    let r = dispatch_call(
        &e.reg,
        &e.exec,
        &e.gate,
        &ToolCall {
            id: "c".into(),
            name: "memory_search".into(),
            arguments: json!({"query": "x"}),
        },
        &ctx(&e.db, "c1"),
    );
    assert_eq!(r.content, "没有找到相关记忆。");
}
