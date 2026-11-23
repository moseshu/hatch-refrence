//! Task suite, categories 5-6 (spec: task-suite-spec-2026-11-23.md):
//! failure / crash / recovery, long-context & collaboration.
//! Acceptance reads resource state and run records.

#[path = "common/mod.rs"]
mod common;
#[path = "common/suite.rs"]
mod suite;

use common::{async_config, env};
use hatch_agent::assemble::message_tokens;
use hatch_agent::{Agent, AgentConfig, ChildRegistry, RunOptions, RunStatus, RunStore, SessionStore};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::{MailSender, OutgoingMail, ParsedMail};
use hatch_core::{Error, Result, Trust};
use hatch_llm::scripted::{FnModel, ScriptedModel};
use hatch_llm::{ChatMessage, ChatRequest, ChatResponse, LlmError, Role, Router, ToolCall};
use hatch_policy::{ActionClass, PolicyConfig};
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::sync::Arc;
use suite::Suite;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn answer(r: &hatch_agent::TurnReport) -> &str {
    r.answer.as_deref().unwrap_or("")
}

/// Sends for real, then reports the outcome as unknown — the
/// documented sender contract for "did it go out?" ambiguity
/// (the SMTP sender maps I/O timeouts to UnknownOutcome).
struct TimeoutAfterWrite {
    inner: Arc<FileOutbox>,
}

impl MailSender for TimeoutAfterWrite {
    fn send(&self, mail: &OutgoingMail) -> Result<String> {
        let _id = self.inner.send(mail)?;
        Err(Error::UnknownOutcome("发送超时，无法确认是否送达".into()))
    }
    fn read_sent(&self, message_id: &str) -> Result<ParsedMail> {
        self.inner.read_sent(message_id)
    }
}

/// Fails without sending; delivery is unconfirmable.
struct AlwaysFails;

impl MailSender for AlwaysFails {
    fn send(&self, _mail: &OutgoingMail) -> Result<String> {
        Err(Error::Connector("发送服务不可用".into()))
    }
}

fn send_script() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["boss@corp.example"], "subject": "周报", "body": "本周进展。"}),
            )]),
            ChatResponse::text("已处理。"),
            ChatResponse::text("已处理。"),
        ],
    ))
}

// ---------------- category 5: failure & recovery ----------------

#[test]
fn recovery_timeout_after_write_goes_to_needs_review_not_resend() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Arc::new(FileOutbox::new(&dir.path().join("outbox"), "me@corp.example").unwrap());
    let sender = Arc::new(TimeoutAfterWrite { inner: outbox.clone() });
    let s = Suite::custom(dir, PolicyConfig::default(), sender, outbox);
    s.seed_person("boss@corp.example", "王总");
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);

    let (_r, sess) = s.run(
        &Router::single(send_script()),
        AgentConfig::default(),
        "把周报发给 boss@corp.example",
    );
    // The mail went out exactly once...
    assert_eq!(s.sent_count(), 1);
    // ...and the ambiguous outcome is parked for human review,
    // recorded neither as done nor as a definite failure.
    assert_eq!(s.approvals_with_status("needs_review"), 1);
    assert_eq!(s.approvals_with_status("executed"), 0);
    assert_eq!(s.approvals_with_status("failed"), 0);
    // The model is told the state is unconfirmed, not "failed".
    let result = s.tool_result(&sess, "1");
    assert!(
        result.contains("无法确认") || result.contains("核查") || result.contains("needs_review"),
        "{result}"
    );
}

#[test]
fn recovery_unconfirmable_send_is_not_claimed_sent() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Arc::new(FileOutbox::new(&dir.path().join("outbox"), "me@corp.example").unwrap());
    let s = Suite::custom(dir, PolicyConfig::default(), Arc::new(AlwaysFails), outbox);
    s.seed_person("boss@corp.example", "王总");
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);

    let (r, _sess) = s.run(
        &Router::single(send_script()),
        AgentConfig::default(),
        "把周报发给 boss@corp.example",
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
    // The final answer must not claim a send that never happened.
    assert!(!answer(&r).contains("已发送"), "{}", answer(&r));
}

// ---------------- category 6: long context & collaboration ----------------

#[test]
fn long_context_constraint_survives_compaction() {
    let s = Suite::new();
    s.seed_person("boss@corp.example", "王总");
    s.seed_grant(ActionClass::SendMessage, &["boss@corp.example"]);
    let sessions = SessionStore::new(&s.db);
    let now = hatch_core::time::now();
    let sess = sessions.create("suite", &now).unwrap();
    // The constraint lands early, then history grows past the hard
    // compaction limit (2x threshold at total_budget = 12_000).
    sessions
        .append(&sess, &ChatMessage::user("记住：以后发邮件只发摘要，不要发全文。"), Trust::User, &now)
        .unwrap();
    for _ in 0..60 {
        let (_, history) = sessions.active(&sess).unwrap();
        let total: usize = history.iter().map(|m| message_tokens(&m.message)).sum();
        if total > 24_000 {
            break;
        }
        sessions.append(&sess, &ChatMessage::user("用户长消息 ".repeat(500)), Trust::User, &now).unwrap();
        sessions.append(&sess, &ChatMessage::assistant("助手长回复 ".repeat(500)), Trust::System, &now).unwrap();
    }
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_email",
                json!({"to": ["boss@corp.example"], "subject": "季度报告摘要", "body": "摘要：营收增长百分之二十。"}),
            )]),
            ChatResponse::text("已按摘要发送。"),
            ChatResponse::text("已按摘要发送。"),
        ],
    ));
    let cfg = AgentConfig {
        total_budget: 12_000,
        ..Default::default()
    };
    let r = s.run_in(&Router::single(model.clone()), cfg, &sess, "把季度报告发给 boss@corp.example");
    assert!(r.compacted, "hard-limit compaction must have run: {r:?}");
    // The constraint is still visible to the model after compaction
    // (in the summary or re-anchored context).
    let reqs = model.requests();
    let last = reqs.last().expect("model was called");
    let ctx: String = last.messages.iter().map(|m| m.text_content()).collect::<Vec<_>>().join("\n");
    assert!(ctx.contains("只发摘要"), "constraint lost in compaction");
    assert_eq!(s.sent_count(), 1);
}

fn is_child(r: &ChatRequest) -> bool {
    r.messages[0].text_content().contains("Your role: sub-agent")
}

fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

#[test]
fn long_context_failed_subagent_is_not_counted_as_success() {
    let e = env();
    let now = hatch_core::time::now();
    let registry = Arc::new(ChildRegistry::new(e.db.clone()));
    let model = Arc::new(FnModel::new("m", |r: &ChatRequest| {
        if is_child(r) {
            // The sub-agent's model call fails outright.
            Err(LlmError::permanent("child model down"))
        } else if tool_results(r) == 0 {
            Ok(ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查一下记忆里有没有预算信息", "type": "explore"}),
            )]))
        } else {
            Ok(ChatResponse::text("子任务没能完成，我这边没有结论。"))
        }
    }));
    let router = Router::single(model);
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: async_config(registry),
    };
    let sess = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run(&sess, "帮我查预算", &now, RunOptions::default()).unwrap();
    assert_eq!(r.subagents.len(), 1);
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_ne!(run.status, RunStatus::Completed, "failed child must not read as success");
    assert!(!answer(&r).contains("结论："), "{}", answer(&r));
}
