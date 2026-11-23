//! Task suite, categories 1-2 (spec: task-suite-spec-2026-11-23.md):
//! read-only research & convergence, cross-tool execution &
//! acceptance. Scripted models replay fixed decisions; acceptance
//! reads resource state (calendar, outbox, files, approvals),
//! never the agent's own report.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
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

fn auto_policy() -> PolicyConfig {
    let mut p = PolicyConfig::default();
    p.set(ActionClass::WriteCalendar, Autonomy::ActNotify);
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    p
}

// ---------------- category 1: read-only research ----------------

#[test]
fn readonly_converges_on_final_deadline() {
    let s = Suite::new();
    s.write_fixture("notes/plan-v1.md", "项目截止日期：2026-12-01（初版）");
    s.write_fixture("notes/plan-v2.md", "项目截止日期：2026-12-15（修订版）");
    s.write_fixture(
        "notes/plan-final.md",
        "最终确认：项目截止日期为 2026-12-20，此为最终版，前两版作废。",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_list", json!({"path": "notes"}))]),
            ChatResponse::calls(vec![
                call("2", "fs_read", json!({"path": "notes/plan-v1.md"})),
                call("3", "fs_read", json!({"path": "notes/plan-v2.md"})),
                call("4", "fs_read", json!({"path": "notes/plan-final.md"})),
            ]),
            ChatResponse::text("最终截止日期是 2026-12-20（plan-final.md 明确标注最终版，12-01 与 12-15 均为已作废的旧版）。"),
        ],
    ));
    let (r, _sess) = s.run(&Router::single(model), AgentConfig::default(), "从笔记里找出最终确定的截止日期");
    // Convergence: correct answer, sources explained.
    assert!(r.answer.as_deref().unwrap_or("").contains("2026-12-20"), "{:?}", r.answer);
    // Zero writes anywhere.
    assert_eq!(s.files(), before);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
    assert_eq!(s.approvals_with_status("pending"), 0);
    // Stopped within budget (3 model steps).
    assert!(r.tool_calls.len() <= 4, "{:?}", r.tool_calls);
}

#[test]
fn readonly_reports_unreadable_source() {
    let s = Suite::new();
    s.write_fixture(
        "notes/index.md",
        "截止日期见 schedule.md；另见 archive/old-plan.md（已归档）。",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "notes/index.md"}))]),
            ChatResponse::calls(vec![
                call("2", "fs_read", json!({"path": "schedule.md"})),
                call("3", "fs_read", json!({"path": "archive/old-plan.md"})),
            ]),
            ChatResponse::text("未能确认截止日期：index.md 引用的 schedule.md 与 archive/old-plan.md 都不存在，证据缺失，需要你补上来源。"),
        ],
    ));
    let (r, _sess) = s.run(&Router::single(model), AgentConfig::default(), "从笔记里找出最终确定的截止日期");
    // The gap is stated, not papered over with a guess.
    assert!(r.answer.as_deref().unwrap_or("").contains("未能确认") || r.answer.as_deref().unwrap_or("").contains("缺失"), "{:?}", r.answer);
    assert_eq!(s.files(), before);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
}

// ---------------- category 2: cross-tool execution ----------------

#[test]
fn cross_tool_event_and_checklist_agree() {
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "notes/project.md",
        "评审会时间：2026-11-24 15:00-16:00，地点 3 号会议室，标题：方案评审会。",
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "notes/project.md"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "propose_event",
                json!({"title": "方案评审会", "start": "2026-11-24 15:00", "end": "2026-11-24 16:00", "location": "3 号会议室"}),
            )]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "handover.md", "content": "交接清单\n- 方案评审会：2026-11-24 15:00-16:00，3 号会议室\n- 准备评审材料\n"}),
            )]),
            ChatResponse::text("已建日历事件并写好交接清单。"),
            // Completeness check intercepts the first finish after
            // side-effecting calls; the model re-answers.
            ChatResponse::text("已建日历事件并写好交接清单。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "从项目笔记提取评审时间，创建日历事件，再写一份交接清单",
    );
    assert!(r.answer.as_deref().unwrap_or("").contains("已建"), "{:?}", r.answer);
    // Independent readback: exactly one event, correct fields.
    let events = s.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].title, "方案评审会");
    // Checklist exists and agrees with the calendar.
    let files = s.files();
    let checklist = files.get("handover.md").expect("handover.md written");
    assert!(checklist.contains("2026-11-24 15:00"), "{checklist}");
    assert!(checklist.contains("3 号会议室"), "{checklist}");
    // No mail side effects.
    assert_eq!(s.sent_count(), 0);
}

#[test]
fn cross_tool_duplicate_proposal_creates_one_event() {
    let s = Suite::with_policy(auto_policy());
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            // The model proposes the same event twice in one batch
            // (a classic duplicate-side-effect shape).
            ChatResponse::calls(vec![
                call(
                    "1",
                    "propose_event",
                    json!({"title": "方案评审会", "start": "2026-11-24 15:00", "end": "2026-11-24 16:00"}),
                ),
                call(
                    "2",
                    "propose_event",
                    json!({"title": "方案评审会", "start": "2026-11-24 15:00", "end": "2026-11-24 16:00"}),
                ),
            ]),
            ChatResponse::text("已建日历事件。"),
            ChatResponse::text("已建日历事件。"),
        ],
    ));
    let (_r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "明天下午三点建一个方案评审会，一小时",
    );
    // Duplicate proposals must not duplicate the side effect.
    assert_eq!(s.events().len(), 1, "{:?}", s.events());
}
