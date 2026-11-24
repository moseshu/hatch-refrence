//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! domain generalization — file/data work. Scripted models replay
//! fixed decisions; acceptance reads resource state (files, hashes,
//! directory trees), never the agent's own report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.

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

fn auto_policy() -> PolicyConfig {
    let mut p = PolicyConfig::default();
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    p
}

// ---------------- category 2: cross-tool execution (file/data) ----------------

#[test]
fn data_csv_sales_summary() {
    // (1) reports/sales-summary.md holds the correct per-region
    //     totals (华北 20000, 华东 23000, 华南 7000, 合计 50000) and
    //     names the anomalous -2000 refund row; (2) exactly one file
    //     write, zero sends/events, source CSV unchanged; (3) the
    //     answer names the totals and the anomaly; (4) one turn.
    // No mail involved: pure read-compute-write.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "data/sales.csv",
        "region,amount\n华北,12000\n华北,8000\n华东,15000\n华东,5000\n华南,9000\n华南,-2000\n华东,3000\n",
    );
    let before = std::fs::read_to_string(s.ws_root.join("data/sales.csv")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "data/sales.csv"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "reports/sales-summary.md", "content": "销售汇总\n- 华北：20000\n- 华东：23000\n- 华南：7000\n- 合计：50000\n异常行：华南 -2000，疑似退款冲账，需财务确认。\n"}),
            )]),
            ChatResponse::text("已汇总：华北 20000，华东 23000，华南 7000，合计 50000。异常行：华南 -2000 疑似退款冲账，需财务确认。"),
            ChatResponse::text("已汇总：华北 20000，华东 23000，华南 7000，合计 50000。异常行：华南 -2000 疑似退款冲账，需财务确认。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "汇总 data/sales.csv 各区销售额，找出异常行，写一份汇总报告",
    );
    let report = &s.files()["reports/sales-summary.md"];
    assert!(
        report.contains("华北") && report.contains("20000"),
        "{report}"
    );
    assert!(
        report.contains("华东") && report.contains("23000"),
        "{report}"
    );
    assert!(
        report.contains("华南") && report.contains("7000"),
        "{report}"
    );
    assert!(report.contains("50000"), "{report}");
    assert!(report.contains("-2000"), "{report}");
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("data/sales.csv")).unwrap(),
        before,
        "source CSV untouched"
    );
    assert_eq!(s.files().len(), 2, "one report, no extra files");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("50000"), "{a}");
    assert!(a.contains("-2000"), "{a}");
}

fn approval_id(result: &str) -> String {
    let start = result.find("apr_").expect(result);
    let rest = &result[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '，' || c == '。')
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

// ---------------- category 2 + 5: download organize, state-change ----------------

#[test]
fn data_download_organize_state_change() {
    // (1) after turn 1: photo.jpg → archive/images/, data.csv →
    //     archive/data/, report.pdf → archive/pdf/report-2.pdf (the
    //     pre-existing archive/pdf/report.pdf keeps its old content
    //     — same name is never overwritten); notes.txt stays in
    //     downloads/ as unclassifiable; (2) four shell commands,
    //     each approved and executed once, zero sends/events;
    //     (3) the verification answer names every moved file and
    //     the unclassifiable one; (4) three turns: organize, verify,
    //     idempotent re-run.
    // This is the suite's first state-change task: a re-run of the
    // same goal must produce zero extra archiving and leave file
    // contents intact (checked by tree + hashes).
    let s = Suite::new().with_shell();
    // Grant patterns are command prefixes; compound commands with
    // shell metacharacters (`&&`, `;`) never match a grant, so the
    // model issues one plain command per call.
    s.seed_grant(ActionClass::ExecCommand, &["mkdir *", "mv *"]);
    s.write_fixture("downloads/report.pdf", "NEW-REPORT");
    s.write_fixture("downloads/photo.jpg", "JPG-BYTES");
    s.write_fixture("downloads/data.csv", "a,b\n1,2\n");
    s.write_fixture("downloads/notes.txt", "UNCLASSIFIED-NOTE");
    s.write_fixture("archive/pdf/report.pdf", "OLD-REPORT");
    let old_report = std::fs::read_to_string(s.ws_root.join("archive/pdf/report.pdf")).unwrap();

    // Turn 1: organize. Reading the directory taints the turn, so
    // every shell_run queues for approval (ExecCommand is never
    // grant-waived from a tainted turn) — the test approves and
    // executes them below, exactly as a user would.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_list", json!({"path": "downloads"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "shell_run",
                json!({"command": "mkdir -p archive/pdf archive/images archive/data"}),
            )]),
            ChatResponse::calls(vec![call(
                "3",
                "shell_run",
                json!({"command": "mv downloads/photo.jpg archive/images/photo.jpg"}),
            )]),
            ChatResponse::calls(vec![call(
                "4",
                "shell_run",
                json!({"command": "mv downloads/data.csv archive/data/data.csv"}),
            )]),
            ChatResponse::calls(vec![call(
                "5",
                "shell_run",
                json!({"command": "mv downloads/report.pdf archive/pdf/report-2.pdf"}),
            )]),
            ChatResponse::text("已提交归档审批：新建 archive 目录，移动 photo.jpg、data.csv、report.pdf（重名不覆盖，改存为 report-2.pdf）。notes.txt 无法分类，留在 downloads/。"),
            ChatResponse::text("已提交归档审批：新建 archive 目录，移动 photo.jpg、data.csv、report.pdf（重名不覆盖，改存为 report-2.pdf）。notes.txt 无法分类，留在 downloads/。"),
        ],
    ));
    let (r, sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "整理 downloads/：按类型归档，重名不覆盖，无法分类的列出来",
    );
    // The turn ends with the commands queued, not run: the agent
    // replaces the scripted answer with the pending-approval notice.
    assert!(answer(&r).contains("审批"), "{}", answer(&r));
    assert_eq!(s.approvals_with_status("pending"), 4);
    // The user approves and executes them in proposal order.
    let now = hatch_core::time::now();
    for call_id in ["2", "3", "4", "5"] {
        let result = s.tool_result(&sess, call_id);
        let id = approval_id(&result);
        hatch_policy::ApprovalQueue::new(&s.db)
            .decide(&id, true, "test approve", &now)
            .unwrap();
        let outcome = hatch_tools::execute_approved(&s.db, &s.exec, &id, &now).unwrap();
        assert!(
            matches!(outcome, hatch_tools::ExecOutcome::Executed(_)),
            "{call_id}: {outcome:?}"
        );
    }
    let files = s.files();
    assert_eq!(files["archive/images/photo.jpg"], "JPG-BYTES");
    assert_eq!(files["archive/data/data.csv"], "a,b\n1,2\n");
    assert_eq!(files["archive/pdf/report-2.pdf"], "NEW-REPORT");
    assert_eq!(
        files["archive/pdf/report.pdf"], old_report,
        "pre-existing file never overwritten"
    );
    assert_eq!(files["downloads/notes.txt"], "UNCLASSIFIED-NOTE");
    assert!(!files.contains_key("downloads/report.pdf"));
    assert!(!files.contains_key("downloads/photo.jpg"));
    assert!(!files.contains_key("downloads/data.csv"));
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let after_first = s.files();

    // Turn 2: verify the tree reads back as organized.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("6", "fs_list", json!({"path": "archive"}))]),
            ChatResponse::text("核对过：archive/ 下 photo.jpg、data.csv、report-2.pdf 都在，原 report.pdf 内容未变，downloads/ 只剩无法分类的 notes.txt。"),
            ChatResponse::text("核对过：archive/ 下 photo.jpg、data.csv、report-2.pdf 都在，原 report.pdf 内容未变，downloads/ 只剩无法分类的 notes.txt。"),
        ],
    ));
    let r = s.run_in(
        &Router::single(model),
        AgentConfig::default(),
        &sess,
        "核对一下归档结果",
    );
    let a = answer(&r);
    assert!(a.contains("report-2.pdf"), "{a}");

    // Turn 3: re-run the same goal — must be a no-op.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("7", "fs_list", json!({"path": "downloads"}))]),
            ChatResponse::text("已归档过：downloads/ 只剩无法分类的 notes.txt，没有需要新归档的文件，本次没有产生额外归档。"),
            ChatResponse::text("已归档过：downloads/ 只剩无法分类的 notes.txt，没有需要新归档的文件，本次没有产生额外归档。"),
        ],
    ));
    let r = s.run_in(
        &Router::single(model),
        AgentConfig::default(),
        &sess,
        "再整理一次 downloads/",
    );
    assert_eq!(s.files(), after_first, "re-run must not change the tree");
    let a = answer(&r);
    assert!(a.contains("notes.txt"), "{a}");
    assert!(!a.contains("已归档："), "{a}");
}
