//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! domain generalization — web research and finance. Scripted models
//! replay fixed decisions; acceptance reads resource state (files),
//! never the agent's own report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.
//!
//! The "web" sources here are saved pages in the workspace fixture:
//! the harness runs with web_fetch off, so multi-source cross-check
//! behaviour is exercised against local copies, not the live web.
//! The live-fetch path is covered by the eval web worlds instead.

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

fn answer(r: &hatch_agent::TurnReport) -> &str {
    r.answer.as_deref().unwrap_or("")
}

/// Staleness can be worded many ways; accept any explicit marker that
/// the data is out of date or is not tomorrow's forecast.
fn flags_stale(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "过期", "重新确认", "再确认", "核实", "不是明日", "非明日", "并非明日",
        "不是明天", "非明天", "并非明天", "旧数据", "实况",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

fn auto_policy() -> PolicyConfig {
    let mut p = PolicyConfig::default();
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    p
}

// ---------------- category 1: read-only research (web) ----------------

#[test]
fn research_cross_check_version_and_price() {
    // (1) reports/tool-check.md says version 2.4 (sources B and C
    //     agree) and records the price conflict $20 vs $25 as
    //     unconfirmed, citing all three sources; (2) one file
    //     write, zero sends/events, sources unchanged; (3) the
    //     answer names 2.4 and the price conflict; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "sources/tool-a.md",
        "工具 X：最新版本 2.3，售价 $20/月。来源：厂商博客。",
    );
    s.write_fixture(
        "sources/tool-b.md",
        "工具 X：最新版本 2.4，售价 $25/月。来源：官方发布说明。",
    );
    s.write_fixture(
        "sources/tool-c.md",
        "工具 X：最新版本 2.4，售价 $25/月。来源：版本更新日志。",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "sources/tool-a.md"})),
                call("2", "fs_read", json!({"path": "sources/tool-b.md"})),
                call("3", "fs_read", json!({"path": "sources/tool-c.md"})),
            ]),
            ChatResponse::calls(vec![call(
                "4",
                "fs_write",
                json!({"path": "reports/tool-check.md", "content": "工具 X 核查\n- 最新版本：2.4（来源 B、C 一致；来源 A 写 2.3，已过期）\n- 价格：冲突，来源 A $20/月，来源 B、C $25/月，无法确认。\n来源：sources/tool-a.md、sources/tool-b.md、sources/tool-c.md。\n"}),
            )]),
            ChatResponse::text("已核查：最新版本 2.4（B、C 一致，A 的 2.3 已过期）。价格冲突：A 说 $20/月，B、C 说 $25/月，无法确认。"),
            ChatResponse::text("已核查：最新版本 2.4（B、C 一致，A 的 2.3 已过期）。价格冲突：A 说 $20/月，B、C 说 $25/月，无法确认。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "对照 sources/tool-a.md、sources/tool-b.md 和 sources/tool-c.md，核查工具 X 的最新版本和价格，把结论、冲突和来源写到 reports/tool-check.md。",
    );
    let report = &s.files()["reports/tool-check.md"];
    assert!(report.contains("2.4"), "{report}");
    assert!(report.contains("$20") && report.contains("$25"), "{report}");
    assert!(report.contains("无法确认"), "{report}");
    assert!(
        report.contains("tool-a.md")
            && report.contains("tool-b.md")
            && report.contains("tool-c.md"),
        "{report}"
    );
    assert!(!report.contains("价格：$20/月。"), "{report}");
    assert_eq!(
        s.files()
            .iter()
            .filter(|(k, _)| k.starts_with("sources/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
        before
            .iter()
            .filter(|(k, _)| k.starts_with("sources/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
        "sources untouched"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("2.4"), "{a}");
    assert!(a.contains("冲突"), "{a}");
}

#[test]
fn research_itinerary_flags_stale_weather() {
    // (1) reports/day-trip.md holds the train times (G101 08:00 →
    //     10:30, G202 18:00 → 20:30) and flags the weather note as
    //     stale (dated 2026-11-23, needs recheck); (2) one file
    //     write, zero sends/events; (3) the answer names the trains
    //     and the stale weather; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture("travel/weather.md", "2026-11-23 天气：晴，18°C，适合出行。");
    s.write_fixture(
        "travel/transport.md",
        "去程：高铁 G101，08:00 出发，10:30 到达。返程：高铁 G202，18:00 出发，20:30 到达。",
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "travel/weather.md"})),
                call("2", "fs_read", json!({"path": "travel/transport.md"})),
            ]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "reports/day-trip.md", "content": "一日行程\n- 去程：高铁 G101，08:00 出发，10:30 到达。\n- 返程：高铁 G202，18:00 出发，20:30 到达。\n- 天气：注意，天气信息是 2026-11-23 的，已过期，出行前需重新确认。\n"}),
            )]),
            ChatResponse::text("已拼好行程：去程 G101 08:00→10:30，返程 G202 18:00→20:30。注意天气信息是 2026-11-23 的，已过期，出行前需重新确认。"),
            ChatResponse::text("已拼好行程：去程 G101 08:00→10:30，返程 G202 18:00→20:30。注意天气信息是 2026-11-23 的，已过期，出行前需重新确认。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "根据 travel/weather.md 和 travel/transport.md 拼一个明日一日行程，注明信息时效，写到 reports/day-trip.md。",
    );
    let report = &s.files()["reports/day-trip.md"];
    assert!(
        report.contains("G101") && report.contains("08:00"),
        "{report}"
    );
    assert!(
        report.contains("G202") && report.contains("18:00"),
        "{report}"
    );
    assert!(report.contains("2026-11-23"), "{report}");
    assert!(flags_stale(report), "{report}");
    assert!(!report.contains("明日天气晴"), "{report}");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("G101"), "{a}");
    assert!(flags_stale(a), "{a}");
}

// ---------------- category 2: cross-tool execution (finance) ----------------

#[test]
fn finance_expense_reconciliation() {
    // (1) reports/expense.md holds the correct category totals
    //     （交通 300, 餐饮 1000, 住宿 850, 会议 720, 合计 2870), flags
    //     the 餐饮 680 dinner as over the 500 limit, and converts
    //     the USD 100 receipt at 7.2 to 720; (2) one file write,
    //     zero sends/events, receipts unchanged; (3) the answer
    //     names the total and the over-limit item; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "expenses/receipts.csv",
        "category,item,amount,currency\n交通,地铁,120,CNY\n交通,打车,180,CNY\n餐饮,午餐,320,CNY\n餐饮,晚餐,680,CNY\n住宿,酒店,850,CNY\n会议,注册费,100,USD\n",
    );
    s.write_fixture(
        "expenses/rules.md",
        "餐饮单次上限 500 元。美元按 7.2 折算。",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "expenses/receipts.csv"})),
                call("2", "fs_read", json!({"path": "expenses/rules.md"})),
            ]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "reports/expense.md", "content": "报销对账\n- 交通：300\n- 餐饮：1000\n- 住宿：850\n- 会议：720（USD 100 按 7.2 折算）\n- 合计：2870\n超标：餐饮晚餐 680 元，超单次上限 500 元。\n"}),
            )]),
            ChatResponse::text("已对账：合计 2870 元。超标：餐饮晚餐 680 元，超单次上限 500 元。美元 100 按 7.2 折算为 720 元。"),
            ChatResponse::text("已对账：合计 2870 元。超标：餐饮晚餐 680 元，超单次上限 500 元。美元 100 按 7.2 折算为 720 元。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "汇总这叠报销单：分类合计、超标的点名、美元换算，写一份对账表到 reports/expense.md",
    );
    let report = &s.files()["reports/expense.md"];
    assert!(
        report.contains("交通") && report.contains("300"),
        "{report}"
    );
    assert!(
        report.contains("餐饮") && report.contains("1000"),
        "{report}"
    );
    assert!(
        report.contains("住宿") && report.contains("850"),
        "{report}"
    );
    assert!(report.contains("720"), "{report}");
    assert!(report.contains("2870"), "{report}");
    assert!(report.contains("680"), "{report}");
    assert_eq!(
        s.files()["expenses/receipts.csv"],
        before["expenses/receipts.csv"]
    );
    assert_eq!(s.files()["expenses/rules.md"], before["expenses/rules.md"]);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("2870"), "{a}");
    assert!(a.contains("680"), "{a}");
}

#[test]
fn finance_budget_mismatch_found_not_fixed() {
    // (1) reports/budget-check.md names the mismatch (total row
    //     95000 vs line-item sum 90000, gap 5000) and the rows it
    //     checked; (2) one file write, zero sends/events, the
    //     budget file itself is NOT modified; (3) the answer names
    //     both sums; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "finance/budget.csv",
        "item,amount\n研发,50000\n市场,30000\n行政,10000\n合计,95000\n",
    );
    let before = std::fs::read_to_string(s.ws_root.join("finance/budget.csv")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "fs_read",
                json!({"path": "finance/budget.csv"}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "reports/budget-check.md", "content": "预算核对\n- 分项合计：90000（研发 50000 + 市场 30000 + 行政 10000）\n- 合计行：95000\n- 差异：5000，对不上。原表未改动。\n"}),
            )]),
            ChatResponse::text(
                "已核对：分项合计 90000，合计行写的是 95000，差 5000，对不上。原表没动。",
            ),
            ChatResponse::text(
                "已核对：分项合计 90000，合计行写的是 95000，差 5000，对不上。原表没动。",
            ),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "核对 finance/budget.csv：合计与分项对不上就指出来，写一份核对报告到 reports/budget-check.md，不要改原表",
    );
    let report = &s.files()["reports/budget-check.md"];
    assert!(report.contains("90000"), "{report}");
    assert!(report.contains("95000"), "{report}");
    assert!(report.contains("5000"), "{report}");
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("finance/budget.csv")).unwrap(),
        before,
        "budget file must not be modified"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("90000") && a.contains("95000"), "{a}");
}
