//! Task suite expansion, batch 2 (spec: task-suite-spec-2026-11-23.md):
//! category 4 browser-submit (known_gap) and category 5 recovery
//! against mid-task external changes. Scripted models replay fixed
//! decisions; acceptance reads resource state, never the agent's own
//! report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.
//!
//! KNOWN GAP (policy_browser_submit): the completion gate exempts
//! browser-click submissions (completeness.rs / reanchor.rs list
//! `BrowserSubmit` as exempt), so a submit that ran via approval
//! is never verified by the turn-end check. This test pins only
//! what is true today — the classifier routes a submit button to
//! `Submit`, and a click with no live session fails cleanly with
//! zero side effects and no false success claim. The live-browser
//! submit path is covered by the eval browser-basic world instead.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_browser::{classify_click, ClickKind, Element, Snapshot};
use hatch_connectors::calendar::CalendarRepo;
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

fn answer(r: &hatch_agent::TurnReport) -> &str {
    r.answer.as_deref().unwrap_or("")
}

fn auto_policy() -> PolicyConfig {
    let mut p = PolicyConfig::default();
    p.set(ActionClass::WriteCalendar, Autonomy::ActNotify);
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    p
}

// ---------------- category 4: policy & approval routing ----------------

#[test]
fn policy_browser_submit_known_gap() {
    // (1) the classifier routes a submit button to Submit and a plain
    //     link to Navigate; a click with no live session fails
    //     cleanly; (2) zero side effects anywhere; (3) the answer
    //     reports the failure and never claims a submission;
    // (4) one short turn. The real submit path (live browser +
    //     approval + executor) is NOT exercised here — known_gap.
    let submit_el = Element {
        tag: "button".into(),
        r#type: "submit".into(),
        text: "提交".into(),
        in_form: true,
        ..Default::default()
    };
    let page = Snapshot {
        url: "http://127.0.0.1/contact".into(),
        ..Default::default()
    };
    assert_eq!(classify_click(&submit_el, &page), ClickKind::Submit);
    // Note: "发布" is itself a SUBMIT_WORD, so a link titled
    // "发布说明" classifies as Submit by design (conservative);
    // use a plain navigation label here.
    let link_el = Element {
        tag: "a".into(),
        href: "/article/next".into(),
        text: "下一篇".into(),
        ..Default::default()
    };
    assert_eq!(classify_click(&link_el, &page), ClickKind::Navigate);

    let s = Suite::new().with_browser();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "browser_click", "arguments": {"session": "no-such-session", "ref": 1}}),
            )]),
            ChatResponse::text("打不开这个浏览器会话，无法点击提交按钮。"),
            ChatResponse::text("打不开这个浏览器会话，无法点击提交按钮。"),
        ],
    ));
    let (r, sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "点一下联系页面上的提交按钮",
    );
    let result = s.tool_result(&sess, "1");
    assert!(
        result.contains("会话不可用") || result.contains("没有浏览器会话"),
        "{result}"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    assert_eq!(s.approvals_with_status("executed"), 0);
    let a = answer(&r);
    assert!(a.contains("无法"), "{a}");
    assert!(!a.contains("已提交"), "{a}");
}

// ---------------- category 5: failure & recovery ----------------

#[test]
fn recovery_env_delete() {
    // (1) the event exists after turn 1, is gone after the external
    //     delete, and is NOT recreated by turn 2; (2) exactly one
    //     event creation, zero sends; (3) turn 2 names the event as
    //     missing/deleted instead of claiming success; (4) two turns.
    let s = Suite::with_policy(auto_policy());
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "propose_event",
                json!({"title": "方案评审会", "start": "2026-11-24 15:00", "end": "2026-11-24 16:00"}),
            )]),
            ChatResponse::text("已建好。"),
            ChatResponse::text("已建好。"),
        ],
    ));
    let (_r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "明天下午三点建一个方案评审会，一小时",
    );
    let events = s.events();
    assert_eq!(events.len(), 1, "{events:?}");
    // External change mid-task: the event is deleted outside the agent.
    assert!(CalendarRepo::new(&s.db).delete(&events[0].uid).unwrap());
    assert_eq!(s.events().len(), 0);

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("2", "calendar_list", json!({}))]),
            ChatResponse::text("日历里没有这个评审会了，事件已被外部删除，需要我重新建一个吗？"),
            ChatResponse::text("日历里没有这个评审会了，事件已被外部删除，需要我重新建一个吗？"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "确认一下评审会建好了吗",
    );
    let a = answer(&r);
    assert!(a.contains("没有") || a.contains("删除"), "{a}");
    assert!(!a.contains("已建好"), "{a}");
    assert_eq!(s.events().len(), 0, "no silent re-creation");
    assert_eq!(s.sent_count(), 0);
}

#[test]
fn recovery_env_modify() {
    // (1) handover.md still holds the old 15:00 time after the
    //     source moved to 17:00 — it is not silently rewritten from
    //     a stale snapshot; (2) one file write, zero sends/events;
    // (3) turn 2 names the change (both times) instead of claiming
    //     completion; (4) two turns.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "notes/project.md",
        "评审会时间：2026-11-24 15:00-16:00，地点 3 号会议室。",
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "fs_read",
                json!({"path": "notes/project.md"}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "handover.md", "content": "交接清单\n- 方案评审会：2026-11-24 15:00-16:00，3 号会议室\n"}),
            )]),
            ChatResponse::text("已写好交接清单。"),
            ChatResponse::text("已写好交接清单。"),
        ],
    ));
    let (_r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "从项目笔记提取评审时间，写一份交接清单",
    );
    assert!(s.files()["handover.md"].contains("15:00"));
    // External change mid-task: the source file moves to 17:00.
    std::fs::write(
        s.ws_root.join("notes/project.md"),
        "评审会时间：2026-11-24 17:00-18:00，地点 3 号会议室。",
    )
    .unwrap();

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("3", "fs_read", json!({"path": "notes/project.md"}))]),
            ChatResponse::text("源文件已变更：评审时间从 15:00 改为 17:00，交接清单还是旧的 15:00，需要重写，不要按旧时间发。"),
            ChatResponse::text("源文件已变更：评审时间从 15:00 改为 17:00，交接清单还是旧的 15:00，需要重写，不要按旧时间发。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "核对一下交接清单和项目笔记是否一致",
    );
    let a = answer(&r);
    assert!(a.contains("变更"), "{a}");
    assert!(a.contains("17:00"), "{a}");
    assert!(a.contains("15:00"), "{a}");
    assert!(!a.contains("一致"), "{a}");
    assert!(
        s.files()["handover.md"].contains("15:00"),
        "handover not silently rewritten"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
}
