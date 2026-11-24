//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! moses's third supplementary group — multimodal extraction and
//! paginated UI work. Scripted models replay fixed decisions;
//! acceptance reads resource state (files), never the agent's report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.
//!
//! The "scanned pages" and "screenshot" here are OCR-text fixtures
//! in the workspace, and the "admin backend" is paginated record
//! files: the suite has no live OCR service or browser session, so
//! the fixtures stand in for those inputs. What the tests do pin is
//! the behaviour that matters downstream — traceable extraction with
//! fuzzy fields flagged, and cross-page find → edit → save →
//! re-read verification.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
use serde_json::{json, Value};
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
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    p
}

// ---------------- category 1/2: multimodal extraction ----------------

#[test]
fn multimodal_inspection_table_with_traceability() {
    // (1) inspect/report.md holds a structured table with the
    //     readings from all three sources — page 1 （设备 A 72°C
    //     正常）, page 2 （设备 B 65°C 正常）, screenshot （设备 D
    //     80°C 偏高） — each row cites its page/position, and the
    //     fuzzy page-2 设备 C reading is marked 待确认, not guessed;
    //     (2) one file write, zero sends/events, sources unchanged;
    //     (3) the answer names the high reading and the 待确认
    //     field; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "inspect/page1.txt",
        "巡检扫描件 第 1 页：设备 A 温度 72°C，状态正常；设备 B 温度 65°C，状态正常。",
    );
    s.write_fixture(
        "inspect/page2.txt",
        "巡检扫描件 第 2 页：设备 C 温度 7?°C（字迹模糊），状态待确认。",
    );
    s.write_fixture(
        "inspect/screenshot.txt",
        "监控截图：设备 D 温度 80°C，状态偏高，需关注。",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "inspect/page1.txt"})),
                call("2", "fs_read", json!({"path": "inspect/page2.txt"})),
                call("3", "fs_read", json!({"path": "inspect/screenshot.txt"})),
            ]),
            ChatResponse::calls(vec![call(
                "4",
                "fs_write",
                json!({"path": "inspect/report.md", "content": "设备巡检结果\n| 设备 | 温度 | 状态 | 来源 |\n| A | 72°C | 正常 | 扫描件第 1 页 |\n| B | 65°C | 正常 | 扫描件第 1 页 |\n| C | 待确认 | 待确认 | 扫描件第 2 页（字迹模糊） |\n| D | 80°C | 偏高 | 监控截图 |\n"}),
            )]),
            ChatResponse::text(
                "已提取巡检结果：设备 D 80°C 偏高需关注。设备 C 温度字迹模糊，标为待确认，没猜。",
            ),
            ChatResponse::text(
                "已提取巡检结果：设备 D 80°C 偏高需关注。设备 C 温度字迹模糊，标为待确认，没猜。",
            ),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "从两页巡检扫描件和一张监控截图里提取结果，输出结构化表格，模糊的标待确认",
    );
    let report = &s.files()["inspect/report.md"];
    assert!(
        report.contains("72°C") && report.contains("第 1 页"),
        "{report}"
    );
    assert!(report.contains("65°C"), "{report}");
    assert!(
        report.contains("80°C") && report.contains("截图"),
        "{report}"
    );
    assert!(report.contains("待确认"), "{report}");
    assert!(report.contains("第 2 页"), "{report}");
    assert!(
        !report.contains("7?°C") || report.contains("待确认"),
        "{report}"
    );
    assert!(
        !report.contains("78°C") && !report.contains("79°C"),
        "{report}"
    );
    assert_eq!(s.files()["inspect/page1.txt"], before["inspect/page1.txt"]);
    assert_eq!(s.files()["inspect/page2.txt"], before["inspect/page2.txt"]);
    assert_eq!(
        s.files()["inspect/screenshot.txt"],
        before["inspect/screenshot.txt"]
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("80°C"), "{a}");
    assert!(a.contains("待确认"), "{a}");
}

// ---------------- category 2/5: paginated admin edit + re-read ----------------

#[test]
fn ui_admin_paged_edit_save_and_recheck() {
    // (1) admin/page2.json has R-008 status 已处理, the other 9
    //     records are byte-identical; (2) one file write, zero
    //     sends/events; (3) the answer names the record ID, the new
    //     status, and the affected count (1); (4) one turn — and
    //     the turn re-reads page 2 after saving instead of trusting
    //     the write receipt.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "admin/page1.json",
        r#"{"page": 1, "records": [
          {"id": "R-001", "status": "待处理"},
          {"id": "R-002", "status": "已处理"},
          {"id": "R-003", "status": "待处理"},
          {"id": "R-004", "status": "已处理"},
          {"id": "R-005", "status": "待处理"}
        ]}"#,
    );
    s.write_fixture(
        "admin/page2.json",
        r#"{"page": 2, "records": [
          {"id": "R-006", "status": "待处理"},
          {"id": "R-007", "status": "待处理"},
          {"id": "R-008", "status": "待处理"},
          {"id": "R-009", "status": "已处理"},
          {"id": "R-010", "status": "待处理"}
        ]}"#,
    );
    let page1_before = s.files()["admin/page1.json"].clone();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "fs_read",
                json!({"path": "admin/page1.json"}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_read",
                json!({"path": "admin/page2.json"}),
            )]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "admin/page2.json", "content": "{\"page\": 2, \"records\": [\n  {\"id\": \"R-006\", \"status\": \"待处理\"},\n  {\"id\": \"R-007\", \"status\": \"待处理\"},\n  {\"id\": \"R-008\", \"status\": \"已处理\"},\n  {\"id\": \"R-009\", \"status\": \"已处理\"},\n  {\"id\": \"R-010\", \"status\": \"待处理\"}\n]}"}),
            )]),
            // Save, then re-read to verify instead of trusting the receipt.
            ChatResponse::calls(vec![call(
                "4",
                "fs_read",
                json!({"path": "admin/page2.json"}),
            )]),
            ChatResponse::text("已修改并复核：R-008 状态改为已处理，受影响 1 条，其余记录未变。"),
            ChatResponse::text("已修改并复核：R-008 状态改为已处理，受影响 1 条，其余记录未变。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "在管理后台跨分页找到 R-008，把状态改成已处理，保存后刷新复核",
    );
    let page2: Value = serde_json::from_str(&s.files()["admin/page2.json"]).unwrap();
    let recs = page2["records"].as_array().unwrap();
    assert_eq!(recs.len(), 5);
    assert_eq!(recs[2]["id"], "R-008");
    assert_eq!(recs[2]["status"], "已处理");
    for (i, want) in [(0, "待处理"), (1, "待处理"), (3, "已处理"), (4, "待处理")] {
        assert_eq!(recs[i]["status"], want, "record {} untouched", i);
    }
    assert_eq!(s.files()["admin/page1.json"], page1_before);
    // The re-read after save actually happened.
    assert!(
        r.tool_calls.iter().filter(|t| *t == "fs_read").count() >= 3,
        "{:?}",
        r.tool_calls
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("R-008"), "{a}");
    assert!(a.contains("已处理"), "{a}");
    assert!(a.contains('1'), "{a}");
}
