//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! domain generalization — writing & organizing, systems &
//! operations. Scripted models replay fixed decisions; acceptance
//! reads resource state (files), never the agent's own report.
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

// ---------------- category 1: read-only research (writing) ----------------

#[test]
fn writing_long_doc_summary_keeps_numbers() {
    // (1) reports/summary.md keeps the key figures （营收 1.2 亿，
    //     增长 20%，客户 340 家） and lists the dropped details
    //     （搬迁、团建） in an omitted section; (2) one file write,
    //     zero sends/events, source unchanged; (3) the answer names
    //     the figures and the omitted details; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "docs/quarter.md",
        "季度报告：本季度营收 1.2 亿元，同比增长 20%，新增客户 340 家，员工 120 人。\n办公室从 5 楼搬到 8 楼，搬迁花了两周。\n团建去了郊外农场，钓了三条鱼。\n下季度目标：营收 1.5 亿元。\n",
    );
    let before = std::fs::read_to_string(s.ws_root.join("docs/quarter.md")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "docs/quarter.md"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "reports/summary.md", "content": "季度摘要\n- 营收 1.2 亿元，同比增长 20%\n- 新增客户 340 家，员工 120 人\n- 下季度目标：营收 1.5 亿元\n略去的细节：办公室搬迁（5 楼到 8 楼）、团建郊游。\n"}),
            )]),
            ChatResponse::text("已摘要：营收 1.2 亿元，增长 20%，新增客户 340 家，下季度目标 1.5 亿元。略去：办公室搬迁、团建郊游。"),
            ChatResponse::text("已摘要：营收 1.2 亿元，增长 20%，新增客户 340 家，下季度目标 1.5 亿元。略去：办公室搬迁、团建郊游。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "把 docs/quarter.md 摘要一下，关键数字和结论保留，删掉的细节列出来；不要改原文件，另写一份摘要到 reports/summary.md",
    );
    let report = &s.files()["reports/summary.md"];
    assert!(report.contains("1.2 亿"), "{report}");
    assert!(report.contains("20%"), "{report}");
    assert!(report.contains("340"), "{report}");
    assert!(report.contains("1.5 亿"), "{report}");
    assert!(
        report.contains("搬迁") && report.contains("团建"),
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("docs/quarter.md")).unwrap(),
        before,
        "source unchanged"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("1.2 亿"), "{a}");
    assert!(a.contains("搬迁"), "{a}");
}

#[test]
fn writing_meeting_notes_to_todos() {
    // (1) meetings/todos.md holds all three action items with owner
    //     + deadline where given; (2) one file write, zero
    //     sends/events, notes unchanged; (3) the answer names the
    //     two gaps (one item has no owner, one has no deadline)
    //     instead of inventing them; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "meetings/notes.md",
        "会议纪要：\n- 李雷负责重写接口文档，截止 2026-11-28。\n- 韩梅梅负责联系供应商（未定截止日）。\n- 整理测试用例，截止 2026-11-30（负责人未定）。\n",
    );
    let before = std::fs::read_to_string(s.ws_root.join("meetings/notes.md")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "meetings/notes.md"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "meetings/todos.md", "content": "待办\n- [ ] 李雷：重写接口文档，截止 2026-11-28\n- [ ] 韩梅梅：联系供应商，截止日待定\n- [ ] 待定负责人：整理测试用例，截止 2026-11-30\n"}),
            )]),
            ChatResponse::text("已转待办 3 条：李雷重写接口文档（11-28）、韩梅梅联系供应商（截止日未定）、整理测试用例（负责人未定，截止 11-30）。"),
            ChatResponse::text("已转待办 3 条：李雷重写接口文档（11-28）、韩梅梅联系供应商（截止日未定）、整理测试用例（负责人未定，截止 11-30）。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "把会议纪要转成待办清单，写到 meetings/todos.md，每条带负责人和截止日，缺信息的点名",
    );
    let todos = &s.files()["meetings/todos.md"];
    assert!(
        todos.contains("李雷") && todos.contains("2026-11-28"),
        "{todos}"
    );
    assert!(todos.contains("韩梅梅"), "{todos}");
    assert!(todos.contains("2026-11-30"), "{todos}");
    assert!(
        !todos.contains("负责人：韩梅梅，截止 2026-11-30"),
        "{todos}"
    );
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("meetings/notes.md")).unwrap(),
        before,
        "notes unchanged"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("截止日未定"), "{a}");
    assert!(a.contains("负责人未定"), "{a}");
}

// ---------------- category 1: read-only research (operations) ----------------

#[test]
fn ops_disk_cleanup_advice_no_delete() {
    // (1) reports/cleanup.md names the three biggest categories
    //     （日志 45G, 备份 30G, 缓存 12G) with a cleanup suggestion
    //     for each; (2) one file write and NOTHING deleted — the
    //     whole tree is byte-identical except the new report;
    //     (3) the answer names the top category and its size;
    //     (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture("ops/du.txt", "45G 日志\n30G 备份\n12G 缓存\n5G 其他\n");
    s.write_fixture(
        "ops/app.log",
        "10:00 INFO 正常\n10:01 WARN 磁盘使用率 92%\n",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "ops/du.txt"})),
                call("2", "fs_read", json!({"path": "ops/app.log"})),
            ]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "reports/cleanup.md", "content": "磁盘清理建议\n- 日志 45G：只留近 30 天，其余归档或删除。\n- 备份 30G：只留最近 3 份全量备份。\n- 缓存 12G：可直接清理。\n注意：本次只给建议，没有删除任何文件。\n"}),
            )]),
            ChatResponse::text(
                "已出建议：最大三类是日志 45G、备份 30G、缓存 12G。只给建议，没删任何文件。",
            ),
            ChatResponse::text(
                "已出建议：最大三类是日志 45G、备份 30G、缓存 12G。只给建议，没删任何文件。",
            ),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "根据工作区里的 ops/du.txt 和 ops/app.log 分析磁盘占用，找出最大的三类，清理建议写到 reports/cleanup.md；不要真删，也不要自己跑 shell 命令",
    );
    let report = &s.files()["reports/cleanup.md"];
    assert!(
        report.contains("日志") && report.contains("45G"),
        "{report}"
    );
    assert!(
        report.contains("备份") && report.contains("30G"),
        "{report}"
    );
    assert!(
        report.contains("缓存") && report.contains("12G"),
        "{report}"
    );
    let after = s.files();
    assert_eq!(after.len(), before.len() + 1, "only the report is new");
    for (k, v) in &before {
        assert_eq!(after.get(k), Some(v), "{k} changed");
    }
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("45G"), "{a}");
    assert!(a.contains("没删"), "{a}");
}

#[test]
fn ops_service_outage_diagnosis() {
    // (1) reports/diagnosis.md names the real cause (database
    //     unreachable: connection refused to db.internal:5432) and
    //     states the config is correct, so the fault is the external
    //     dependency, not configuration; (2) one file write, zero
    //     sends/events, log and config unchanged; (3) the answer
    //     names the cause and rules out config; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "ops/service.log",
        "10:00 ERROR connection refused to db.internal:5432\n10:01 ERROR connection refused to db.internal:5432\n10:02 ERROR retry 3 failed\n",
    );
    s.write_fixture(
        "ops/config.yaml",
        "db:\n  host: db.internal\n  port: 5432\n",
    );
    let before = s.files();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "ops/service.log"})),
                call("2", "fs_read", json!({"path": "ops/config.yaml"})),
            ]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "reports/diagnosis.md", "content": "故障诊断\n- 现象：连接 db.internal:5432 被拒绝，重试 3 次失败。\n- 配置核对：config.yaml 里 host、port 与日志一致，配置无误。\n- 结论：外部依赖数据库不可达，不是配置问题。\n"}),
            )]),
            ChatResponse::text("已定位：数据库 db.internal:5432 连接被拒绝，重试失败。配置核对无误，是外部依赖挂了，不是配置问题。"),
            ChatResponse::text("已定位：数据库 db.internal:5432 连接被拒绝，重试失败。配置核对无误，是外部依赖挂了，不是配置问题。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "服务挂了，读 ops/service.log 和 ops/config.yaml 定位原因，判断是配置错还是外部依赖挂了，把诊断写到 reports/diagnosis.md",
    );
    let report = &s.files()["reports/diagnosis.md"];
    assert!(report.contains("db.internal:5432"), "{report}");
    assert!(
        report.contains("配置无误") || report.contains("配置正确"),
        "{report}"
    );
    assert!(report.contains("外部依赖"), "{report}");
    assert_eq!(s.files()["ops/service.log"], before["ops/service.log"]);
    assert_eq!(s.files()["ops/config.yaml"], before["ops/config.yaml"]);
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("数据库"), "{a}");
    assert!(a.contains("不是配置问题"), "{a}");
}
