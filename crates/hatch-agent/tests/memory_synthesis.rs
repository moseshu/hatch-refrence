//! P3: the nightly synthesis ("current understanding") is injected at the
//! start of every turn, in the stable prefix right after the behaviour
//! spec; no document means no section, and a read failure never fails
//! the turn.

use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_memory::{save_synthesis, MemoryStore, NewClaim, SynthesisDoc};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;
use std::sync::Arc;

fn agent(db: &Db, model: Arc<ScriptedModel>) -> Agent<'_> {
    let router = Box::leak(Box::new(Router::single(model)));
    let tools = Box::leak(Box::new(default_registry()));
    let exec = Box::leak(Box::new(ExecutorRegistry::new()));
    let gate = Box::leak(Box::new(Gatekeeper::new(PolicyConfig::default())));
    Agent { db, router, tools, executors: exec, gate, config: AgentConfig::default() }
}

fn system_of(model: &ScriptedModel, turn: usize) -> String {
    model.requests()[turn].messages[0].content.clone()
}

#[test]
fn synthesis_is_injected_after_spec_before_profile() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    let mem = MemoryStore::new(&db);
    mem.add(
        &NewClaim::stated("preference:reply", "回复邮件请用中文，语气简洁", "user:chat").with_tags(&["preference"]),
        &now,
    )
    .unwrap();
    // Untagged, so it is recalled (not profile) for the query below.
    mem.add(&NewClaim::stated("preference:meeting", "周三下午不安排会议", "user:chat"), &now).unwrap();
    save_synthesis(
        &db,
        &SynthesisDoc {
            content: "## 用户画像\n注重效率的工程师。\n\n## 长期偏好\n- 回复邮件用中文\n".into(),
            generated_at: now,
            source_claims: 2,
            stale: None,
        },
    )
    .unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "我之前说的回复邮件风格和周三下午的安排是什么", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let sys = system_of(&model, 0);
    assert!(sys.contains("当前理解（每夜综合"), "{sys}");
    assert!(sys.contains("注重效率的工程师"), "{sys}");
    assert!(sys.contains("来源 2 条记忆"), "{sys}");
    let pos = |x: &str| sys.find(x).unwrap();
    assert!(pos("当前理解") < pos("用户画像与偏好"), "synthesis comes before the profile section");
    assert!(pos("当前理解") < pos("与本轮相关的记忆"), "synthesis comes before recalled memory");
    // The behaviour spec itself names the marker; what must not appear is
    // an actual untrusted envelope around the synthesis.
    assert!(!sys.contains("<external_content source="), "synthesis is the user's own memory, not untrusted input");
}

#[test]
fn no_synthesis_injects_nothing() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    a.run_turn(&s, "你好", &now).unwrap();
    assert!(!system_of(&model, 0).contains("当前理解"));
}

#[test]
fn synthesis_read_failure_never_fails_the_turn() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    db.conn().execute("DROP TABLE meta", []).unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "你好", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("好的"));
}

#[test]
fn stale_synthesis_warns_the_model_it_may_be_outdated() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    let mem = MemoryStore::new(&db);
    let old = mem
        .add(
            &NewClaim::stated("preference:reply", "回复邮件请用中文，语气简洁", "user:chat").with_tags(&["preference"]),
            &now,
        )
        .unwrap()
        .id()
        .to_string();
    save_synthesis(
        &db,
        &SynthesisDoc {
            content: "## 用户画像\n注重效率的工程师。\n\n## 长期偏好\n- 回复邮件用中文\n".into(),
            generated_at: now,
            source_claims: 1,
            stale: None,
        },
    )
    .unwrap();
    // Correcting the claim marks the stored synthesis stale …
    mem.correct(&old, &NewClaim::stated("preference:reply", "回复邮件请用英文", "user:cli"), &now)
        .unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    a.run_turn(&s, "你好", &now).unwrap();
    // … and the turn's system prompt says so next to the document.
    let sys = system_of(&model, 0);
    assert!(sys.contains("当前理解（每夜综合"), "{sys}");
    assert!(sys.contains("可能已过时"), "{sys}");
    // The corrected claim itself is what recall returns, not the old text.
    let current = mem.current(&now).unwrap();
    assert!(current.iter().any(|c| c.text.contains("回复邮件请用英文")));
    assert!(!current.iter().any(|c| c.text.contains("回复邮件请用中文")));
}
