//! §3.4 context engineering at the turn-loop level: the total-budget
//! trigger, the extractive hard floor, and the cooldown.
//!
//! Calibration (pinned, not guessed): the behaviour spec is ~625
//! estimated tokens, so with `total_budget = 1200` the history
//! threshold is θ ≈ 570, the hard floor 2θ ≈ 1140, and the cooldown
//! bar θ/2 ≈ 285. A 4000-char ASCII message costs ~1004 tokens
//! (`4 + 4000/4`); a 2000-char one ~504.

mod common;

use common::Harness;
use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_core::Trust;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatMessage, ChatResponse, Router};
use std::sync::Arc;

fn long(chars: usize) -> String {
    "x".repeat(chars)
}

fn run_turn(h: &Harness, router: &Router, config: AgentConfig, session: &str, input: &str) -> hatch_agent::TurnReport {
    let agent = Agent {
        db: &h.db,
        router,
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config,
    };
    let now = hatch_core::time::now();
    agent.run_turn(session, input, &now).unwrap()
}

fn seed(h: &Harness, session: &str, user_turns: &[String]) {
    let store = SessionStore::new(&h.db);
    let now = hatch_core::time::now();
    for u in user_turns {
        store.append(session, &ChatMessage::user(u), Trust::User, &now).unwrap();
        store.append(session, &ChatMessage::assistant("收到"), Trust::System, &now).unwrap();
    }
}

fn config() -> AgentConfig {
    AgentConfig { total_budget: 1200, keep_tail_turns: 1, ..Default::default() }
}

#[test]
fn sync_hard_floor_then_cooldown_then_release() {
    let h = Harness::new();
    let now = hatch_core::time::now();
    let s = SessionStore::new(&h.db).create("t", &now).unwrap();
    // Two seeded long turns (~1004 tokens each).
    seed(&h, &s, &[long(4000), long(4000)]);
    let model = Arc::new(ScriptedModel::ok(
        "c",
        vec![
            ChatResponse::text("好的"),
            ChatResponse::text("好的"),
            ChatResponse::text("好的"),
        ],
    ));
    let router = Router::single(model);

    // Turn 1: a third long turn. History ~3024 tokens > 2θ (~1140):
    // the turn compacts synchronously with the extractive fallback.
    let r1 = run_turn(&h, &router, config(), &s, &long(4000));
    assert!(r1.compacted, "history past 2θ must compact synchronously");
    assert!(!r1.compaction_requested);

    // Turn 2: a short question. The kept tail (~1004 tokens) is still
    // over θ (~570), so without the cooldown this turn would trigger
    // again — but only ~13 tokens arrived since the last summary, far
    // under θ/2 (~285), so nothing happens.
    let r2 = run_turn(&h, &router, config(), &s, "小问题");
    assert!(!r2.compacted, "cooldown must block the re-trigger");
    assert!(!r2.compaction_requested, "cooldown must block the re-trigger");

    // Turn 3: a long turn again. New history since the summary is now
    // ~1017 tokens > θ/2, so the cooldown releases and the total
    // (~2027) is past 2θ again: synchronous compaction.
    let r3 = run_turn(&h, &router, config(), &s, &long(4000));
    assert!(r3.compacted, "cooldown must release once new history passes θ/2");
}

#[test]
fn background_request_between_threshold_and_hard_floor() {
    let h = Harness::new();
    let now = hatch_core::time::now();
    let s = SessionStore::new(&h.db).create("t", &now).unwrap();
    // One seeded mid-size turn (~504 tokens).
    seed(&h, &s, &[long(2000)]);
    let model = Arc::new(ScriptedModel::ok("c", vec![ChatResponse::text("好的")]));
    let router = Router::single(model);

    // History ~1014 tokens: over θ (~570) but under 2θ (~1140), so the
    // turn enqueues a background job and continues with full history.
    let r = run_turn(&h, &router, config(), &s, &long(2000));
    assert!(!r.compacted);
    assert!(r.compaction_requested, "over θ but under 2θ must request a background job");
}

#[test]
fn short_history_triggers_nothing() {
    let h = Harness::new();
    let model = Arc::new(ScriptedModel::ok("c", vec![ChatResponse::text("你好")]));
    let router = Router::single(model);
    let (r, _) = h.run(router, config(), "你好");
    assert!(!r.compacted);
    assert!(!r.compaction_requested, "history under θ must not trigger");
}

#[test]
fn last_summary_seq_tracks_the_latest_summary_row() {
    // §3.4 cooldown support: messages past this seq are the new history.
    let db = hatch_store::Db::open_in_memory().unwrap();
    let s = SessionStore::new(&db);
    let now = hatch_core::time::now();
    let id = s.create("t", &now).unwrap();
    assert_eq!(s.last_summary_seq(&id).unwrap(), None);
    s.append(&id, &ChatMessage::user("one"), Trust::User, &now).unwrap(); // 1
    s.append(&id, &ChatMessage::assistant("ok one"), Trust::System, &now).unwrap(); // 2
    s.compact(&id, 2, "summary of one", &now).unwrap(); // 3 = summary
    assert_eq!(s.last_summary_seq(&id).unwrap(), Some(3));
    s.append(&id, &ChatMessage::user("two"), Trust::User, &now).unwrap(); // 4
    s.compact(&id, 4, "summary of two", &now).unwrap(); // 5 = summary
    assert_eq!(s.last_summary_seq(&id).unwrap(), Some(5));
}
