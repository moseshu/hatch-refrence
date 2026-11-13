//! Drills for M2 desensitization: no real address or name reaches a
//! model, and the delivered brief still shows real names.
//!
//! Two legs, both on the scripted meeting-brief world (sends are
//! never real — a `FileOutbox` in the test out dir):
//!
//! - the triage workflow, run directly with a recording model that
//!   delegates to the eval oracle and rewrites the oracle's answer
//!   into code space (a real model only ever sees codes, so its
//!   summary names the sender by code): every request the workflow
//!   sent carries codes only, and the receipt summaries show real
//!   names — the reverse step in `adjust`/`draft_body`.
//! - the full task through the worker: the gather agent's persisted
//!   session messages are exactly what the model saw (system prompt,
//!   user instruction, tool results), and the written brief file
//!   shows real names and no codes.
//!
//! The triage model input is not persisted by the worker, so the
//! first leg runs the workflow directly; the classify unit tests
//! assert the same envelope code path. The user's own instruction
//! ("把李婷最近邮件里的决定整理成简报") legitimately names 李婷 —
//! it is the user's words, not mail content — so the name assertion
//! applies to tool results, not to the user message.

#![cfg(test)]

use super::task_list_drill::{approve_write, artifact_approval_id, opts, run_blocked, setup};
use super::task_worker::{run_worker, KillPoint};
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_core::time::parse_tz;
use hatch_eval::oracle::oracle_model;
use hatch_eval::World;
use hatch_llm::{ChatModel, ChatRequest, ChatResponse, LlmError, Router};
use hatch_policy::{ApprovalQueue, AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_store::rusqlite::params;
use hatch_store::Db;
use hatch_tools::{CreateEventExecutor, ExecutorRegistry, SendEmailExecutor};
use hatch_triage::{TriageConfig, TriageWorkflow};
use std::sync::{Arc, Mutex};

/// Every real address in the world: the persona, every mailbox
/// sender/recipient, every contact.
pub(crate) fn real_addresses(world: &World) -> Vec<String> {
    let mut out = vec![world.persona.email.clone()];
    for m in &world.mailbox {
        out.push(m.from.clone());
        out.extend(m.to.iter().cloned());
    }
    for c in world
        .persona
        .vips
        .iter()
        .chain(world.persona.contacts.iter())
        .chain(world.persona.low.iter())
    {
        out.push(c.email.clone());
    }
    out.sort();
    out.dedup();
    out
}

/// The gather agent's persisted session messages: (role, content),
/// in order — exactly what the model saw.
pub(crate) fn session_messages(db: &Db) -> Vec<(String, String)> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare("SELECT role, content FROM session_messages ORDER BY session_id, seq")
        .unwrap();
    let rows = stmt
        .query_map(params![], |r| {
            Ok((
                r.get::<_, String>(0).unwrap(),
                r.get::<_, String>(1).unwrap(),
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// A model that records every request, delegates to the eval oracle,
/// and rewrites the oracle's answer into code space — the shape a
/// real model's answer takes, since it only ever saw codes. The
/// mapping is rebuilt lazily per call, from the same database the
/// workflow reads, so the codes agree with the workflow's.
struct RecordingModel {
    db: Arc<Db>,
    owner_email: String,
    owner_name: String,
    inner: Arc<dyn ChatModel>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ChatModel for RecordingModel {
    fn name(&self) -> String {
        format!("recording:{}", self.inner.name())
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.seen.lock().unwrap().push(req.clone());
        let mut resp = self.inner.chat(req)?;
        if let Ok(mapper) =
            hatch_triage::desensitize::mapper_for(&self.db, &self.owner_email, &self.owner_name)
        {
            if let Some(c) = &mut resp.content {
                *c = mapper.forward(c);
            }
        }
        Ok(resp)
    }
}

#[test]
fn triage_requests_carry_codes_only_and_receipt_shows_real_names() {
    let (db, dir, out, now, account) = setup();
    let world = World::load(&dir).unwrap();
    let addrs = real_addresses(&world);
    let tz = parse_tz(&world.persona.timezone).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recording = Arc::new(RecordingModel {
        db: Arc::clone(&db),
        owner_email: account.clone(),
        owner_name: world.persona.name.clone(),
        inner: oracle_model(&world),
        seen: Arc::clone(&seen),
    });
    let router = Router::single(recording);
    let outbox = Arc::new(FileOutbox::new(out.path(), &account).unwrap());
    let exec = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(outbox, &account)))
        .with(Arc::new(CreateEventExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let wf = TriageWorkflow {
        db: &db,
        router: &router,
        gate: &gate,
        executors: &exec,
        attention: AttentionPolicy {
            tz,
            ..Default::default()
        },
        config: TriageConfig {
            user_name: world.persona.name.clone(),
            user_email: account.clone(),
            tz,
            fetch_limit: 100,
            approval_ttl_hours: 72,
        },
    };
    let receipt = wf
        .run(
            &mut FixtureMailbox::new(&account, world.mailbox.clone()),
            &now,
        )
        .unwrap();
    assert_eq!(receipt.processed, 7, "七封邮件都应处理");

    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "workflow 应调过模型");
    let joined: String = seen
        .iter()
        .flat_map(|r| r.messages.iter().map(|m| m.content.as_str()))
        .collect();
    for a in &addrs {
        assert!(!joined.contains(a.as_str()), "模型输入含真实地址 {a}");
    }
    assert!(
        joined.contains("@example.invalid"),
        "模型输入应含代号地址，实际请求数 {}",
        seen.len()
    );

    let summaries: Vec<&str> = receipt.items.iter().map(|i| i.summary.as_str()).collect();
    for s in &summaries {
        assert!(!s.contains("@example.invalid"), "摘要不得含代号：{s}");
    }
    let all = summaries.join("\n");
    assert!(
        all.contains("李婷") && all.contains("方案 B 通过") && all.contains("预算上限 5 万元"),
        "映射回之后摘要应显示真名与决定，实际：{all}"
    );
    // The approval queue holds the reply drafts; none may carry a code.
    let pending = ApprovalQueue::new(&db).list(None).unwrap();
    for a in &pending {
        let body = serde_json::to_string(&a.payload).unwrap();
        assert!(!body.contains("@example.invalid"), "待审批内容不得含代号");
    }
}

#[test]
fn brief_world_model_input_has_no_real_address_and_brief_shows_real_names() {
    let (db, dir, out, now, _) = setup();
    let world = World::load(&dir).unwrap();
    let addrs = real_addresses(&world);
    let blocked = run_blocked(&db, &dir, &out, now);
    let task_id = blocked.task_id.clone();

    let messages = session_messages(&db);
    assert!(!messages.is_empty(), "gather 应留下会话记录");
    for (role, content) in &messages {
        for a in &addrs {
            assert!(
                !content.contains(a.as_str()),
                "模型输入含真实地址 {a}（role={role}）"
            );
        }
    }
    let joined: String = messages.iter().map(|(_, c)| c.as_str()).collect();
    assert!(joined.contains("@example.invalid"), "模型输入应含代号地址");
    for (role, content) in &messages {
        if role == "tool" {
            assert!(
                !content.contains("李婷"),
                "工具结果不得含真实姓名，实际：{content}"
            );
        }
    }

    // Approve the artifact write and run to completion; the
    // delivered brief shows real names and no codes.
    let approval_id = artifact_approval_id(&db, &task_id);
    approve_write(&db, &approval_id, now);
    let done = run_worker(
        &db,
        &dir,
        out.path(),
        opts(Some(task_id.clone()), None, now, KillPoint::None),
    )
    .unwrap();
    assert_eq!(done.steps_done, 3);
    assert!(done.blocked.is_none());
    let brief = std::fs::read_to_string(out.path().join("brief-v1.md")).unwrap();
    assert!(brief.contains("李婷"), "简报应显示真实姓名");
    assert!(
        brief.contains("方案 B 通过") && brief.contains("预算上限 5 万元已确认"),
        "简报应含决定，实际：{brief}"
    );
    assert!(
        !brief.contains("@example.invalid"),
        "简报不得含代号地址，实际：{brief}"
    );
    assert!(
        !brief.contains("发件人A") && !brief.contains("发件人B"),
        "简报不得含代号姓名"
    );
}
