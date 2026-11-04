//! Pipe transport end to end: inbox → agent turn → outbox.
//!
//! Uses a scripted model (no real LLM, no network). The real-Agent
//! tests run the turn loop for real; the stub-runner tests drive the
//! relay paths (widget fallback, artifact delivery, discretion,
//! worker relay) directly.

use hatch_agent::{Agent, AgentConfig};
use hatch_artifacts::store::{ArtifactKind, ArtifactStore};
use hatch_core::Result;
use hatch_llm::{ChatResponse, Router};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::{ExecutorRegistry, Registry};
use hatch_transports::capability::CapabilityManifest;
use hatch_transports::{
    labels_for_tool_calls, ChatStore, ContentKind, Draft, InboundMessage, MainDecision, PipeDirs, PipeTransport, PollReport, Relay,
    RelayOutcome, Transport, TurnOutcome, TurnRunner,
};
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    db: Arc<Db>,
    artifacts_root: PathBuf,
    pipe: Arc<PipeTransport>,
    chats: ChatStore,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let artifacts_root = home.join("artifacts");
        let chats = ChatStore::new(&home);
        let pipe = Arc::new(PipeTransport::new(
            PipeDirs::default_under(&home),
            chats.clone(),
            artifacts_root.clone(),
            db.clone(),
        ));
        Env { _dir: dir, home, db, artifacts_root, pipe, chats }
    }

    fn relay(&self) -> Relay {
        Relay::new(self.pipe.clone(), self.chats.clone(), self.artifacts_root.clone(), Some(CapabilityManifest::local_default()))
    }

    fn inbox(&self, name: &str, body: &str) {
        let dirs = PipeDirs::default_under(&self.home);
        std::fs::create_dir_all(&dirs.inbox).unwrap();
        std::fs::write(dirs.inbox.join(name), body).unwrap();
    }

    fn outbox_texts(&self) -> Vec<serde_json::Value> {
        let dirs = PipeDirs::default_under(&self.home);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dirs.outbox)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
            .collect()
    }

    fn poll(&self, runner: &dyn TurnRunner) -> PollReport {
        let relay = self.relay();
        self.pipe
            .poll_once(runner, &|msg, outcome| relay.deliver_main(msg, outcome))
            .unwrap()
    }
}

/// A TurnRunner backed by a real Agent with a scripted model.
struct AgentRunner<'a> {
    agent: Agent<'a>,
}

impl TurnRunner for AgentRunner<'_> {
    fn run_turn(&self, _msg: &InboundMessage, session: &str, input: &str) -> Result<TurnOutcome> {
        let report = self.agent.run_turn(session, input, &hatch_core::time::now())?;
        Ok(TurnOutcome {
            text: report.answer.clone().unwrap_or_else(|| report.receipt()),
            labels: labels_for_tool_calls(&report.tool_calls),
            pending_approvals: report.approvals.clone(),
            ..Default::default()
        })
    }
}

fn agent_runner<'a>(
    db: &'a Db,
    router: &'a Router,
    tools: &'a Registry,
    exec: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
) -> AgentRunner<'a> {
    AgentRunner {
        agent: Agent {
            db,
            router,
            tools,
            executors: exec,
            gate,
            config: AgentConfig { origin: Some("transport:pipe".into()), ..AgentConfig::default() },
        },
    }
}

/// A TurnRunner that returns canned outcomes in order.
struct StubRunner {
    outcomes: Mutex<VecDeque<TurnOutcome>>,
}

impl StubRunner {
    fn one(outcome: TurnOutcome) -> Self {
        StubRunner { outcomes: Mutex::new(VecDeque::from([outcome])) }
    }
}

impl TurnRunner for StubRunner {
    fn run_turn(&self, _msg: &InboundMessage, _session: &str, _input: &str) -> Result<TurnOutcome> {
        Ok(self.outcomes.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn plain_tools(artifacts_root: &Path) -> (Registry, ExecutorRegistry, Gatekeeper) {
    let tools = hatch_artifacts::tools::add_artifact_tools(default_registry(), artifacts_root);
    let exec = ExecutorRegistry::new()
        .with(Arc::new(
            hatch_artifacts::executor::CreateArtifactExecutor::new(artifacts_root.to_path_buf()),
        ))
        .with(Arc::new(hatch_artifacts::executor::ShareArtifactExecutor {
            root: artifacts_root.to_path_buf(),
        }))
        .with(Arc::new(
            hatch_artifacts::executor::DeliverArtifactExecutor {
                root: artifacts_root.to_path_buf(),
            },
        ));
    (tools, exec, Gatekeeper::new(PolicyConfig::default()))
}

#[test]
fn pipe_round_trip_text_with_real_agent_turn() {
    let env = Env::new();
    let (tools, exec, gate) = plain_tools(&env.artifacts_root);
    let router = Router::single(Arc::new(hatch_llm::scripted::ScriptedModel::ok(
        "t",
        vec![ChatResponse::text("你好，我是 hatch。")],
    )));
    let runner = agent_runner(&env.db, &router, &tools, &exec, &gate);
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "你好"}"#);
    let report = env.poll(&runner);
    assert_eq!(report, PollReport { processed: 1, sent: 1, blocked: 0, failed: 0, errors: vec![] });
    let out = env.outbox_texts();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["chat"], "family");
    assert_eq!(out[0]["status"], "sent");
    assert!(out[0]["text"].as_str().unwrap().contains("你好，我是 hatch。"), "{}", out[0]);
    // Inbox file moved to processed/; a send.log line exists.
    let dirs = PipeDirs::default_under(&env.home);
    assert!(dirs.processed.join("01.json").is_file());
    assert!(dirs.root.join("send.log").is_file());
}

#[test]
fn two_chats_get_separate_transcripts() {
    let env = Env::new();
    let (tools, exec, gate) = plain_tools(&env.artifacts_root);
    let router = Router::single(Arc::new(hatch_llm::scripted::ScriptedModel::ok(
        "t",
        vec![ChatResponse::text("答 A"), ChatResponse::text("答 B")],
    )));
    let runner = agent_runner(&env.db, &router, &tools, &exec, &gate);
    env.inbox("01.json", r#"{"chat": "chat-a", "sender": "alice", "text": "A 的问题"}"#);
    env.inbox("02.json", r#"{"chat": "chat-b", "sender": "bob", "text": "B 的问题"}"#);
    let report = env.poll(&runner);
    assert_eq!(report.sent, 2);
    let sessions = hatch_agent::SessionStore::new(&env.db);
    let a = env.chats.resolve_session(&env.db, "chat-a", "pipe").unwrap();
    let b = env.chats.resolve_session(&env.db, "chat-b", "pipe").unwrap();
    assert_ne!(a, b);
    let (_, a_msgs) = sessions.active(&a).unwrap();
    let (_, b_msgs) = sessions.active(&b).unwrap();
    assert!(a_msgs.iter().any(|m| m.message.content.contains("A 的问题")));
    assert!(!a_msgs.iter().any(|m| m.message.content.contains("B 的问题")), "chat A must not see B");
    assert!(b_msgs.iter().any(|m| m.message.content.contains("B 的问题")));
    assert!(!b_msgs.iter().any(|m| m.message.content.contains("A 的问题")), "chat B must not see A");
}

#[test]
fn discretion_blocks_a_mail_labelled_reply() {
    let env = Env::new();
    env.chats.write_discretion("family", "forbid: mail\n").unwrap();
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "邮件里说什么？"}"#);
    let runner = StubRunner::one(TurnOutcome {
        text: "邮件说预算批了。".into(),
        labels: vec![ContentKind::Mail],
        ..Default::default()
    });
    let report = env.poll(&runner);
    assert_eq!(report, PollReport { processed: 1, sent: 0, blocked: 1, failed: 0, errors: vec![] });
    let out = env.outbox_texts();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["status"], "blocked");
    let text = out[0]["text"].as_str().unwrap();
    assert!(text.contains("拦截"), "{text}");
    assert!(!text.contains("预算批了"), "the blocked draft must not leak into the notice: {text}");
}

#[test]
fn discretion_redacts_mail_headers_and_sends_the_rest() {
    let env = Env::new();
    env.chats.write_discretion("family", "forbid: mail\n").unwrap();
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "转告我"}"#);
    let runner = StubRunner::one(TurnOutcome {
        text: "老板让我转告你：\n发件人：boss@corp.com\n主题：Q3 预算\n会议改到周四。".into(),
        ..Default::default()
    });
    let report = env.poll(&runner);
    assert_eq!(report.sent, 1);
    let out = env.outbox_texts();
    assert_eq!(out[0]["status"], "sent");
    assert_eq!(out[0]["redacted"], true);
    let text = out[0]["text"].as_str().unwrap();
    assert!(text.contains("会议改到周四"), "{text}");
    assert!(!text.contains("boss@corp.com"), "{text}");
}

#[test]
fn widget_fallback_reaches_the_outbox_complete() {
    let env = Env::new();
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "选一个"}"#);
    let widget = json!({
        "type": "options", "title": "选一个",
        "options": [
            {"id": "a", "label": "方案 A", "reply": "选 A"},
            {"id": "b", "label": "方案 B", "reply": "选 B"}
        ]
    });
    let runner = StubRunner::one(TurnOutcome {
        text: "请选一个：".into(),
        widget: Some(widget),
        ..Default::default()
    });
    let report = env.poll(&runner);
    assert_eq!(report.sent, 1);
    let out = env.outbox_texts();
    let text = out[0]["text"].as_str().unwrap();
    // Every label and reply survives in the text fallback (the pipe
    // transport has no UI, so the fallback is the whole rendering).
    for needle in ["方案 A", "选 A", "方案 B", "选 B"] {
        assert!(text.contains(needle), "fallback lost `{needle}`: {text}");
    }
    assert!(!text.contains("embed_token"), "no card JSON goes to a UI-less transport: {text}");
}

#[test]
fn artifact_delivery_message_contains_the_absolute_path() {
    let env = Env::new();
    let store = ArtifactStore::new(&env.artifacts_root);
    let built = store.build(Some("art_week"), "周报", ArtifactKind::Document, "# 周报\n", "test").unwrap();
    store.mark_shared(&built.meta.id, "appr_1").unwrap();
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "把周报发给我"}"#);
    let runner = StubRunner::one(TurnOutcome {
        text: "周报在这里：".into(),
        artifact_id: Some("art_week".into()),
        ..Default::default()
    });
    let report = env.poll(&runner);
    assert_eq!(report.sent, 1);
    let out = env.outbox_texts();
    let text = out[0]["text"].as_str().unwrap();
    assert!(text.contains(&built.path.display().to_string()), "delivery message must carry the link: {text}");
    assert!(store.get("art_week").unwrap().delivered);
}

#[test]
fn image_is_a_reference_and_the_agent_says_it_cannot_see_it() {
    let env = Env::new();
    let (tools, exec, gate) = plain_tools(&env.artifacts_root);
    let model = Arc::new(hatch_llm::scripted::ScriptedModel::ok(
        "t",
        vec![ChatResponse::text("我看不到这张图片，只能看到文件名 photo.png 与大小。")],
    ));
    let router = Router::single(model.clone());
    let runner = agent_runner(&env.db, &router, &tools, &exec, &gate);
    // A fake PNG: distinctive bytes that must never reach the model.
    let image_bytes = b"FAKEPNG-BYTES-7f3a9-never-in-a-prompt";
    let dirs = PipeDirs::default_under(&env.home);
    std::fs::create_dir_all(&dirs.inbox).unwrap();
    std::fs::write(dirs.inbox.join("photo.png"), image_bytes).unwrap();
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "这张图片里有什么？", "image": "photo.png"}"#);
    let report = env.poll(&runner);
    assert_eq!(report.sent, 1);
    let out = env.outbox_texts();
    let text = out[0]["text"].as_str().unwrap();
    assert!(text.contains("我看不到这张图片"), "{text}");
    // The model saw the reference + the cannot-see note, never the bytes.
    let seen = model.requests();
    assert_eq!(seen.len(), 1);
    let prompt = serde_json::to_string(&seen[0].messages).unwrap();
    assert!(prompt.contains("photo.png"), "{prompt}");
    assert!(prompt.contains("无法直接查看图片内容"), "{prompt}");
    assert!(!prompt.contains("FAKEPNG-BYTES-7f3a9"), "image bytes must never enter the prompt");
    // The image file was moved to processed/ with its message.
    assert!(dirs.processed.join("photo.png").is_file());
}

#[test]
fn mentioned_rule_is_recorded_then_enforced() {
    let env = Env::new();
    // Message 1 mentions the rule casually; the turn just acknowledges it.
    env.inbox("01.json", r#"{"chat": "family", "sender": "alice", "text": "以后不要把邮件发到这个群"}"#);
    let runner = StubRunner::one(TurnOutcome { text: "好的，已记下。".into(), ..Default::default() });
    assert_eq!(env.poll(&runner).sent, 1);
    assert!(env.chats.read_discretion("family").unwrap().forbids(ContentKind::Mail));
    // Message 2: a mail-labelled reply is now blocked by that rule.
    env.inbox("02.json", r#"{"chat": "family", "sender": "alice", "text": "邮件说什么？"}"#);
    let runner = StubRunner::one(TurnOutcome {
        text: "邮件说……".into(),
        labels: vec![ContentKind::Mail],
        ..Default::default()
    });
    let report = env.poll(&runner);
    assert_eq!(report.blocked, 1);
    let out = env.outbox_texts();
    assert_eq!(out.len(), 2);
    let statuses: Vec<&str> = out.iter().map(|o| o["status"].as_str().unwrap()).collect();
    assert!(statuses.contains(&"sent") && statuses.contains(&"blocked"), "{statuses:?}");
}

#[test]
fn worker_draft_needs_main_decision_and_missing_capability_is_honest() {
    let env = Env::new();
    let relay = env.relay();
    let draft = Draft {
        chat_id: "family".into(),
        text: "worker 准备的回复".into(),
        labels: vec![],
        widget: None,
        artifact_id: None,
        prepared_by: "sa_worker1".into(),
        facts: vec!["事实".into()],
        needs_capability: None,
    };
    // Submitting alone sends nothing.
    relay.submit_draft(&draft).unwrap();
    assert!(env.outbox_texts().is_empty());
    // With the main agent's decision it relays.
    let out = relay.deliver_draft(&draft, &MainDecision::approve("已核对")).unwrap();
    assert!(matches!(out, RelayOutcome::Sent { .. }), "{out:?}");
    let texts = env.outbox_texts();
    assert_eq!(texts.len(), 1);
    assert!(texts[0]["text"].as_str().unwrap().contains("worker 准备的回复"));
    // A draft needing computer.control (not on this device) is not
    // relayed: the reply is the honest missing-capability response.
    let draft = Draft { needs_capability: Some("computer.control".into()), ..draft };
    let out = relay.deliver_draft(&draft, &MainDecision::approve("好")).unwrap();
    let RelayOutcome::Blocked { reason } = out else {
        panic!("expected a block, got {out:?}")
    };
    assert!(reason.contains("不支持 computer.control"), "{reason}");
    assert!(reason.contains("辅助功能"), "{reason}");
    assert_eq!(env.outbox_texts().len(), 1, "the blocked draft sent nothing");
}

#[test]
fn malformed_inbox_file_goes_to_failed() {
    let env = Env::new();
    env.inbox("bad.json", r#"{"chat": "family", "text": "#);
    let runner = StubRunner::one(TurnOutcome::default());
    let report = env.poll(&runner);
    assert_eq!(report.failed, 1);
    assert!(!report.errors.is_empty());
    let dirs = PipeDirs::default_under(&env.home);
    assert!(dirs.failed.join("bad.json").is_file());
    assert!(env.outbox_texts().is_empty());
}
