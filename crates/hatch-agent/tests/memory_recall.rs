//! Forced recall: every turn searches memory (BM25, k = 5) before the first
//! model call. No hits means no recall section; a memory error must never
//! fail the turn.

use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_core::Trust;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_memory::{MemoryStore, NewClaim};
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
fn relevant_claim_is_recalled_with_source_and_confidence() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    MemoryStore::new(&db)
        .add(&NewClaim::stated("preference:reply", "回复邮件请用中文，语气简洁", "user:chat"), &now)
        .unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")])); 
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "我之前说的回复邮件风格是什么", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    let sys = system_of(&model, 0);
    assert!(sys.contains("与本轮相关的记忆"), "{sys}");
    assert!(sys.contains("回复邮件请用中文，语气简洁"), "{sys}");
    assert!(sys.contains("来源 user:chat") && sys.contains("置信度 0.90"), "{sys}");
}

#[test]
fn no_memory_and_unrelated_query_inject_nothing() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的"), ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    a.run_turn(&s, "明天几点出门", &now).unwrap();
    assert!(!system_of(&model, 0).contains("与本轮相关的记忆"));
    MemoryStore::new(&db)
        .add(&NewClaim::stated("preference:reply", "喜欢简短的回复", "user:chat"), &now)
        .unwrap();
    let s2 = SessionStore::new(&db).create("t2", &now).unwrap();
    a.run_turn(&s2, "明天几点出门", &now).unwrap();
    assert!(!system_of(&model, 1).contains("与本轮相关的记忆"), "{}", system_of(&model, 1));
}

#[test]
fn memory_failure_never_fails_the_turn() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    db.conn().execute("DROP TABLE claims", []).unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "你好", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.answer.as_deref(), Some("好的"));
    assert!(!system_of(&model, 0).contains("与本轮相关的记忆"));
}

#[test]
fn untrusted_claim_is_flagged_in_recall() {
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    MemoryStore::new(&db)
        .add(
            &NewClaim::stated("person:boss@x.com", "老板要求周五前交预算", "mail:99")
                .with_trust(Trust::Untrusted, 0.9),
            &now,
        )
        .unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("好的")]));
    let a = agent(&db, model.clone());
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    a.run_turn(&s, "老板对预算有什么要求", &now).unwrap();
    let sys = system_of(&model, 0);
    assert!(sys.contains("老板要求周五前交预算"), "{sys}");
    assert!(sys.contains("来源不可信"), "{sys}");
}
