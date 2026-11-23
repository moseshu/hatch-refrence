//! Turn-end semantic grading (ReAct path) after rubric unification:
//! the rubric comes from the frozen acceptance contract (agent-bound
//! items, else the bound plan's), evidence is the executor's observed
//! projection when it has one, and L3 gates only when the layer is
//! enforced — in Observe the verdict is a shadow verdict.
//!
//! All models are scripted; zero real calls. The env switches are
//! process-wide, so every test here serializes on one lock.

mod common;

use common::{call, model, Harness};
use hatch_agent::{AgentConfig, StopReason};
use hatch_llm::{ChatResponse, Role};
use hatch_plan::acceptance::AcceptanceItem;
use hatch_plan::plan::{PlanStep, RiskLevel, StepStatus};
use hatch_plan::PlanStore;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::verification::{EffectContract, EvidenceCheck, Layer, Verdict};
use hatch_tools::{ActionExecutor, ExecContext, ExecutorRegistry};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A `fetch_url` executor double: executes canned, verifies Pass,
/// and (optionally) projects observed fields.
struct StubFetch {
    observe_fields: bool,
}

impl ActionExecutor for StubFetch {
    fn kind(&self) -> &'static str {
        "fetch_url"
    }
    fn execute(&self, _p: &Value, _c: &ExecContext) -> hatch_core::Result<String> {
        Ok("URL: https://example.com/page\nHTTP 200\n\n页面正文：会议在 3 号会议室。".into())
    }
    fn contract(&self) -> EffectContract {
        EffectContract::ExternalEffect
    }
    fn verify(
        &self,
        _p: &Value,
        _r: &str,
        _c: &ExecContext,
    ) -> hatch_core::Result<EvidenceCheck> {
        Ok(EvidenceCheck {
            verdict: Verdict::Pass,
            ..EvidenceCheck::unknown("stub-fetch", "ok")
        })
    }
    fn observe(
        &self,
        _p: &Value,
        _r: &str,
        _c: &ExecContext,
    ) -> hatch_core::Result<BTreeMap<String, Value>> {
        if self.observe_fields {
            Ok(BTreeMap::from([
                ("fetch.url".into(), json!("https://example.com/page")),
                ("fetch.status".into(), json!(200)),
            ]))
        } else {
            Ok(Default::default())
        }
    }
}

fn harness(observe_fields: bool) -> Harness {
    let mut h = Harness::new();
    h.tools = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: None,
            web_fetch: true,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    h.exec = ExecutorRegistry::new().with(Arc::new(StubFetch { observe_fields }));
    h
}

/// Scripted model: fetch once, then answer; grader requests
/// (json_mode) get `verdict` for every rubric id, citing the first
/// evidence id on a pass.
fn scripted(verdict: &'static str) -> (hatch_llm::Router, common::Seen) {
    model(move |req| {
        if req.json_mode {
            let payload: Value = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .and_then(|m| serde_json::from_str(&m.text_content()).ok())
                .unwrap_or(Value::Null);
            let ev_id = payload["evidence"][0]["id"].as_str().unwrap_or("").to_string();
            let items: Vec<Value> = payload["rubric"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            json!({
                                "id": r["id"],
                                "verdict": verdict,
                                "evidence": if verdict == "pass" { vec![ev_id.clone()] } else { vec![] },
                                "reason": "scripted verdict",
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            return ChatResponse::text(json!({"items": items}).to_string());
        }
        if req.messages.iter().any(|m| m.role == Role::Tool) {
            ChatResponse::text("页面说会议在 3 号会议室。")
        } else {
            ChatResponse::calls(vec![call(
                "c1",
                "web_fetch",
                json!({"url": "https://example.com/page"}),
            )])
        }
    })
}

/// The grader request payload (goal/rubric/evidence/candidate).
fn grader_payload(seen: &common::Seen) -> Value {
    let reqs = seen.lock().unwrap();
    let req = reqs
        .iter()
        .find(|r| r.json_mode)
        .expect("应有一次 grader 调用");
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .and_then(|m| serde_json::from_str(&m.text_content()).ok())
        .expect("grader payload 应是 JSON")
}

fn l3_report(report: &hatch_agent::TurnReport) -> &hatch_plan::verification::VerificationReport {
    report
        .verification
        .iter()
        .find(|r| r.checks.iter().any(|c| c.layer == Layer::L3))
        .expect("应有 L3 语义验收报告")
}

fn nl(id: &str, expected: &str) -> AcceptanceItem {
    AcceptanceItem::Nl {
        id: id.into(),
        expected: expected.into(),
    }
}

#[test]
fn contract_rubric_and_observed_evidence_observe_does_not_block() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("HATCH_SEMANTIC_VERIFICATION", "1");
    std::env::remove_var("HATCH_ACCEPTANCE_ENFORCE");
    let h = harness(true);
    let (router, seen) = scripted("fail");
    let config = AgentConfig {
        completeness_check: false,
        acceptance: vec![nl("nl-1", "回答引用了页面来源")],
        ..Default::default()
    };
    let (report, _s) = h.run(router, config, "打开页面看看会议在哪");
    std::env::remove_var("HATCH_SEMANTIC_VERIFICATION");

    assert_eq!(report.stop, StopReason::Answered, "Observe 的 L3 Fail 不得拦回合");
    let r = l3_report(&report);
    assert_eq!(r.checks[0].rule, "cross-step-v1:nl-1");
    assert!(!r.checks[0].enforced);
    assert_eq!(r.shadow(), Verdict::Fail);
    assert_eq!(r.verdict, Verdict::NotApplicable, "门裁决只看 enforced 检查");

    let payload = grader_payload(&seen);
    assert_eq!(payload["rubric"][0]["id"], "nl-1");
    assert_eq!(payload["rubric"][0]["expected"], "回答引用了页面来源");
    assert_eq!(payload["evidence"][0]["source"], "fetch_url:observed");
    assert_eq!(payload["evidence"][0]["value"]["fetch.status"], 200);
}

#[test]
fn enforced_l3_fail_blocks_the_turn() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("HATCH_SEMANTIC_VERIFICATION", "1");
    std::env::set_var("HATCH_ACCEPTANCE_ENFORCE", "l3");
    let h = harness(true);
    let (router, _seen) = scripted("fail");
    let config = AgentConfig {
        completeness_check: false,
        acceptance: vec![nl("nl-1", "回答引用了页面来源")],
        ..Default::default()
    };
    let (report, _s) = h.run(router, config, "打开页面看看会议在哪");
    std::env::remove_var("HATCH_SEMANTIC_VERIFICATION");
    std::env::remove_var("HATCH_ACCEPTANCE_ENFORCE");

    assert!(
        matches!(report.stop, StopReason::VerificationBlocked(_)),
        "Enforce 的 L3 Fail 应拦回合，实际 {:?}",
        report.stop
    );
    let r = l3_report(&report);
    assert!(r.checks[0].enforced);
    assert_eq!(r.verdict, Verdict::Fail);
}

#[test]
fn payload_fallback_evidence_and_goal_rubric_without_contract() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("HATCH_SEMANTIC_VERIFICATION", "1");
    std::env::remove_var("HATCH_ACCEPTANCE_ENFORCE");
    let h = harness(false); // executor has no observed projection
    let (router, seen) = scripted("pass");
    let config = AgentConfig {
        completeness_check: false,
        ..Default::default()
    };
    let (report, _s) = h.run(router, config, "打开页面看看会议在哪");
    std::env::remove_var("HATCH_SEMANTIC_VERIFICATION");

    assert_eq!(report.stop, StopReason::Answered);
    let r = l3_report(&report);
    assert_eq!(r.checks[0].rule, "cross-step-v1:user-goal");
    assert_eq!(r.checks[0].verdict, Verdict::Pass);

    let payload = grader_payload(&seen);
    assert_eq!(payload["rubric"][0]["id"], "user-goal");
    assert!(
        payload["evidence"][0]["source"]
            .as_str()
            .unwrap()
            .contains("approved payload"),
        "无观测投影时证据应诚实标注为批准载荷"
    );
    assert_eq!(
        payload["evidence"][0]["value"]["url"], "https://example.com/page",
        "回退证据是批准载荷本身"
    );
}

#[test]
fn bound_plan_supplies_the_rubric() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("HATCH_SEMANTIC_VERIFICATION", "1");
    std::env::remove_var("HATCH_ACCEPTANCE_ENFORCE");
    let h = harness(true);
    let store = PlanStore::new(&h.db);
    let plan = store
        .create(
            "test task",
            "",
            "",
            vec![PlanStep {
                id: "s1".into(),
                title: "读页面".into(),
                tool: "web_fetch".into(),
                arguments: json!({"url": "https://example.com/page"}),
                files: vec![],
                expected: "页面已读".into(),
                risk: RiskLevel::Low,
                status: StepStatus::Pending,
                output: String::new(),
                approval_id: None,
            }],
            true,
            &hatch_core::time::now(),
        )
        .unwrap();
    store
        .set_acceptance(&plan.id, &[nl("plan-nl", "回答注明会议地点")], &hatch_core::time::now())
        .unwrap();
    let (router, seen) = scripted("pass");
    let config = AgentConfig {
        completeness_check: false,
        plan_id: Some(plan.id.clone()),
        ..Default::default()
    };
    let (report, _s) = h.run(router, config, "打开页面看看会议在哪");
    std::env::remove_var("HATCH_SEMANTIC_VERIFICATION");

    assert_eq!(report.stop, StopReason::Answered);
    let r = l3_report(&report);
    assert_eq!(r.checks[0].rule, "cross-step-v1:plan-nl");
    let payload = grader_payload(&seen);
    assert_eq!(payload["rubric"][0]["expected"], "回答注明会议地点");
}

#[test]
fn contract_without_nl_items_skips_grading() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("HATCH_SEMANTIC_VERIFICATION", "1");
    std::env::remove_var("HATCH_ACCEPTANCE_ENFORCE");
    let h = harness(true);
    let (router, seen) = scripted("fail");
    let config = AgentConfig {
        completeness_check: false,
        acceptance: vec![AcceptanceItem::Predicate {
            id: "p1".into(),
            left: hatch_plan::acceptance::FieldRef {
                step: "s1".into(),
                field: "fetch.status".into(),
            },
            op: hatch_plan::acceptance::PredicateOp::Equals,
            right: Some(hatch_plan::acceptance::Operand::Literal { value: json!(200) }),
            normalize: None,
        }],
        ..Default::default()
    };
    let (report, _s) = h.run(router, config, "打开页面看看会议在哪");
    std::env::remove_var("HATCH_SEMANTIC_VERIFICATION");

    assert_eq!(report.stop, StopReason::Answered);
    assert!(
        !seen.lock().unwrap().iter().any(|r| r.json_mode),
        "契约无 NL 项时不得调用 grader"
    );
    assert!(
        report
            .verification
            .iter()
            .all(|r| r.checks.iter().all(|c| c.layer != Layer::L3)),
        "不应产生 L3 报告"
    );
}
