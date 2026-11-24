//! `ask_user` end to end with a scripted model: the question is
//! issued, an answer recorded through the shared answer helper is
//! backfilled as the tool result, and the turn continues. With no
//! answer, the call returns unanswered at the timeout instead of
//! hanging.

#[path = "common/mod.rs"]
mod common;

use common::call;
use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_llm::scripted::{FnModel, ScriptedModel};
use hatch_llm::{ChatResponse, Router};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::ask_user::{answer_question, Choice, ASK_KIND};
use hatch_tools::builtin::default_registry;
use hatch_tools::intent::{intent_digest, IntentGateMode};
use hatch_tools::{AskUserExecutor, CreateEventExecutor, ExecutorRegistry};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn ask_call(id: &str, timeout_secs: u64) -> hatch_llm::ToolCall {
    call(
        id,
        "use_tool",
        json!({"name": "ask_user", "arguments": {
            "question": "用哪种方案？",
            "options": [
                {"label": "方案甲", "description": "快"},
                {"label": "方案乙", "description": "稳"}
            ],
            "timeout_secs": timeout_secs
        }}),
    )
}

fn run_turn(db: &Arc<Db>, router: Router, input: &str) -> (hatch_agent::TurnReport, String) {
    let tools = default_registry();
    let exec = ExecutorRegistry::new().with(Arc::new(AskUserExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let agent = Agent {
        db,
        router: &router,
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config: AgentConfig::default(),
    };
    let now = hatch_core::time::now();
    let s = SessionStore::new(db).create("t", &now).unwrap();
    (agent.run_turn(&s, input, &now).unwrap(), s)
}

fn tool_result(db: &Db, session: &str, call_id: &str) -> String {
    SessionStore::new(db)
        .transcript(session)
        .unwrap()
        .into_iter()
        .find(|r| r.tool_call_id.as_deref() == Some(call_id))
        .map(|r| r.content.clone())
        .unwrap_or_default()
}

#[test]
fn answer_is_backfilled_and_the_turn_continues() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    // Answer from another thread once the question is pending, the
    // way the API/CLI answer entry points would.
    let answer_db = db.clone();
    let answerer = std::thread::spawn(move || {
        let q = ApprovalQueue::new(&answer_db);
        for _ in 0..200 {
            if let Ok(rows) = q.list(Some(ApprovalStatus::Pending)) {
                if let Some(a) = rows.iter().find(|a| a.kind == ASK_KIND) {
                    let now = hatch_core::time::now();
                    answer_question(&answer_db, &a.id, Some(Choice::Index(1)), None, &now).unwrap();
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("question never appeared");
    });

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![ask_call("1", 30)]),
            ChatResponse::text("按方案乙来。"),
            // Turn-end completeness check intercepts the first finish.
            ChatResponse::text("按方案乙来。"),
        ],
    ));
    let (r, s) = run_turn(&db, Router::single(model), "帮我选个方案");
    answerer.join().unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["ask_user"]);
    let result = tool_result(&db, &s, "1");
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["answered"], true);
    assert_eq!(v["label"], "方案乙");
    assert_eq!(v["choice"], 1);
    // The answered question is settled to Executed, not left Approved:
    // the turn-end verification counts Approved rows as pending and
    // would replace the turn's answer with a "still waiting" message.
    let rows = ApprovalQueue::new(&db).list(None).unwrap();
    assert_eq!(rows[0].status, ApprovalStatus::Executed);
    let answer = r.answer.as_deref().unwrap_or_default();
    assert!(answer.contains("方案乙"), "{answer}");
    assert!(!answer.contains("等待审批"), "{answer}");
}

#[test]
fn no_answer_times_out_instead_of_hanging() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![ask_call("1", 1)]),
            ChatResponse::text("你没回，我先按方案甲。"),
            ChatResponse::text("你没回，我先按方案甲。"),
        ],
    ));
    let (r, s) = run_turn(&db, Router::single(model), "帮我选个方案");
    assert_eq!(r.stop, StopReason::Answered);
    let result = tool_result(&db, &s, "1");
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["answered"], false);
    assert_eq!(v["reason"], "timeout");
    let rows = ApprovalQueue::new(&db).list(None).unwrap();
    assert_eq!(rows[0].status, ApprovalStatus::Expired);
}

#[test]
fn ask_user_is_hidden_but_findable() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let tools = default_registry();
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let agent = Agent {
        db: &db,
        router: &Router::single(Arc::new(ScriptedModel::ok("m", vec![]))),
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config: AgentConfig::default(),
    };
    let names: Vec<String> = agent.tool_specs().iter().map(|s| s.name.clone()).collect();
    assert!(!names.contains(&"ask_user".to_string()), "{names:?}");

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "find_tool",
                json!({"query": "ask the user a question with options"}),
            )]),
            ChatResponse::text("找到了。"),
        ],
    ));
    let (r, s) = run_turn(&db, Router::single(model), "问问用户选哪个");
    assert_eq!(r.stop, StopReason::Answered);
    let found = tool_result(&db, &s, "1");
    assert!(found.contains("ask_user"), "{found}");
}

/// L0 with `ask_user`: the model asks which day, the user answers,
/// and the next action is judged against the input *plus* the
/// answer — the answer is the user's own clarification, trusted
/// like the input, but scoped to the question it answers.
fn judging_router(seen_intents: Arc<Mutex<Vec<String>>>) -> Router {
    let step = Arc::new(AtomicUsize::new(0));
    let model = FnModel::new("judging-script", move |req| {
        let is_judge = req
            .messages
            .iter()
            .any(|m| m.text_content().contains("independent intent judge"));
        if is_judge {
            let payload = req
                .messages
                .iter()
                .find(|m| m.text_content().trim_start().starts_with('{'))
                .map(|m| m.text_content())
                .unwrap_or_default();
            let intent = serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("intent").and_then(|i| i.as_str()).map(str::to_string))
                .unwrap_or_default();
            seen_intents.lock().unwrap().push(intent);
            return Ok(ChatResponse::text(
                "{\"verdict\":\"allow\",\"reason\":\"scripted\",\"cited_intent\":\"\",\"cited_field\":\"\"}",
            ));
        }
        let n = step.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "ask_user", "arguments": {
                    "question": "改到周几？",
                    "options": [
                        {"label": "周五", "description": "本周五"},
                        {"label": "下周一", "description": "下周一"}
                    ],
                    "timeout_secs": 30
                }}),
            )])),
            1 => Ok(ChatResponse::calls(vec![call(
                "2",
                "propose_event",
                json!({
                    "title": "评审会",
                    "start": "2026-11-27 15:00"
                }),
            )])),
            _ => Ok(ChatResponse::text("已按你的回答把评审会改到周五 15:00。")),
        }
    });
    Router::single(Arc::new(model))
}

fn run_judged_turn(
    db: &Arc<Db>,
    router: Router,
    input: &str,
) -> (hatch_agent::TurnReport, Vec<String>) {
    let tools = default_registry();
    let exec = ExecutorRegistry::new()
        .with(Arc::new(AskUserExecutor))
        .with(Arc::new(CreateEventExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let config = AgentConfig {
        intent_gate: IntentGateMode::Observe,
        ..Default::default()
    };
    let agent = Agent {
        db,
        router: &router,
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config,
    };
    let now = hatch_core::time::now();
    let s = SessionStore::new(db).create("t", &now).unwrap();
    let report = agent.run_turn(&s, input, &now).unwrap();
    let digests: Vec<String> = db
        .conn()
        .prepare("SELECT intent_digest FROM intent_verdicts ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (report, digests)
}

fn answer_first_question(db: Arc<Db>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let q = ApprovalQueue::new(&db);
        for _ in 0..200 {
            if let Ok(rows) = q.list(Some(ApprovalStatus::Pending)) {
                if let Some(a) = rows.iter().find(|a| a.kind == ASK_KIND) {
                    let now = hatch_core::time::now();
                    answer_question(&db, &a.id, Some(Choice::Index(0)), Some("周五 15:00".into()), &now)
                        .unwrap();
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("question never appeared");
    })
}

#[test]
fn ask_user_answer_reaches_the_l0_judge_as_a_clarification() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    let answerer = answer_first_question(db.clone());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let input = "帮我把其中一个评审会改期";
    let (r, digests) = run_judged_turn(&db, judging_router(seen.clone()), input);
    answerer.join().unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["ask_user", "propose_event"]);
    assert_eq!(r.approvals.len(), 1, "propose_event queues one approval");
    let intents = seen.lock().unwrap();
    assert!(!intents.is_empty(), "the judge must have seen the event proposal");
    let judged = intents.last().unwrap();
    // The clarification is there, scoped to its question: the
    // question text and the answer, not a rewritten request.
    assert!(judged.starts_with(input), "base intent unchanged: {judged}");
    assert!(judged.contains("改到周几？"), "{judged}");
    assert!(judged.contains("周五 15:00"), "{judged}");
    // The cache key moves with the answer: the recorded digest is
    // not the digest of the bare input.
    assert!(!digests.is_empty(), "a verdict must be recorded");
    assert!(
        digests.iter().all(|d| d != &intent_digest(input)),
        "digest must change with the clarification: {digests:?}"
    );
}

#[test]
fn no_answer_leaves_the_l0_intent_unchanged() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    // Same turn shape, but the question times out unanswered: the
    // judge must see exactly the original input.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_in = seen.clone();
    let step = Arc::new(AtomicUsize::new(0));
    let model = FnModel::new("no-answer-script", move |req| {
        let is_judge = req
            .messages
            .iter()
            .any(|m| m.text_content().contains("independent intent judge"));
        if is_judge {
            let payload = req
                .messages
                .iter()
                .find(|m| m.text_content().trim_start().starts_with('{'))
                .map(|m| m.text_content())
                .unwrap_or_default();
            let intent = serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("intent").and_then(|i| i.as_str()).map(str::to_string))
                .unwrap_or_default();
            seen_in.lock().unwrap().push(intent);
            return Ok(ChatResponse::text(
                "{\"verdict\":\"allow\",\"reason\":\"scripted\",\"cited_intent\":\"\",\"cited_field\":\"\"}",
            ));
        }
        let n = step.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "ask_user", "arguments": {
                    "question": "改到周几？",
                    "options": ["周五", "下周一"],
                    "timeout_secs": 1
                }}),
            )])),
            1 => Ok(ChatResponse::calls(vec![call(
                "2",
                "propose_event",
                json!({
                    "title": "评审会",
                    "start": "2026-11-27 15:00"
                }),
            )])),
            _ => Ok(ChatResponse::text("你没回，我先按周五 15:00 排。")),
        }
    });
    let input = "帮我把其中一个评审会改期";
    let (r, digests) = run_judged_turn(&db, Router::single(Arc::new(model)), input);
    assert_eq!(r.stop, StopReason::Answered);
    let intents = seen.lock().unwrap();
    assert!(!intents.is_empty());
    assert_eq!(intents.last().unwrap(), input, "no clarification without an answer");
    assert!(
        digests.iter().all(|d| d == &intent_digest(input)),
        "digest unchanged without an answer: {digests:?}"
    );
}
