//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! moses's three supplementary groups — write-then-verify, identity
//! disambiguation & authorization boundaries, multimodal & UI work.
//! Priority tasks (batch update timeout, same-name grant) first.
//! Scripted models replay fixed decisions; acceptance reads resource
//! state (record files, grants, shares), never the agent's report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.
//!
//! The "work-order system", "permission store" and "admin backend"
//! here are record files in the workspace fixture: the suite has no
//! live ticket system, so the file is the system of record and the
//! test asserts on record IDs. The timeout behaviour (write lands,
//! response lost, re-check before retry) is the same UnknownOutcome
//! contract the mail sender documents in task_suite_c.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_core::{Error, Result};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
use hatch_tools::{ActionExecutor, ExecContext, WriteFileExecutor};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// WriteFileExecutor wrapper: the Nth write lands on disk, then the
/// response is lost (UnknownOutcome) — the "write succeeded, reply
/// timed out" shape. Later writes succeed normally.
struct FlakyWriteFile {
    inner: WriteFileExecutor,
    fail_on: usize,
    count: AtomicUsize,
}

impl ActionExecutor for FlakyWriteFile {
    fn kind(&self) -> &'static str {
        "write_file"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let receipt = self.inner.execute(payload, ctx)?;
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_on {
            return Err(Error::UnknownOutcome(
                "写入成功后响应超时，无法确认是否落盘".into(),
            ));
        }
        Ok(receipt)
    }

    fn verify(
        &self,
        payload: &Value,
        receipt: &str,
        ctx: &ExecContext,
    ) -> Result<hatch_tools::verification::EvidenceCheck> {
        self.inner.verify(payload, receipt, ctx)
    }

    fn observe(
        &self,
        payload: &Value,
        receipt: &str,
        ctx: &ExecContext,
    ) -> Result<BTreeMap<String, Value>> {
        self.inner.observe(payload, receipt, ctx)
    }
}

fn records_json(updated_through: usize) -> String {
    let mut recs = Vec::new();
    for i in 1..=10 {
        let owner = if i <= updated_through {
            "李雷"
        } else {
            "张伟"
        };
        recs.push(json!({"id": format!("R-{i:02}"), "owner": owner}));
    }
    serde_json::to_string_pretty(&json!({"records": recs})).unwrap()
}

// ---------------- category 5: write & recovery (PRIORITY) ----------------

#[test]
fn write_batch_update_timeout_recheck_before_retry() {
    // (1) all 10 records end with owner 李雷, each exactly once,
    //     after the turn-2 re-check; (2) the write covering R-06
    //     lands but its response is lost: exactly one needs_review
    //     row, zero duplicate writes of the same content; (3) turn
    //     1 reports the timeout as unconfirmed, turn 2 reports all
    //     10 records by ID; (4) two turns.
    let s = Suite::with_policy(auto_policy()).with_executor(Arc::new(FlakyWriteFile {
        inner: WriteFileExecutor,
        fail_on: 2,
        count: AtomicUsize::new(0),
    }));
    s.write_fixture("records.json", &records_json(0));
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "records.json"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "records.json", "content": records_json(3)}),
            )]),
            // Write #2 covers R-01..R-06: it lands, then the reply
            // is lost (FlakyWriteFile fail_on = 2).
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "records.json", "content": records_json(6)}),
            )]),
            ChatResponse::text("已写 R-01 到 R-06，但第 6 条写完后响应超时，无法确认是否落盘，已记待核查，没有盲目重发。"),
            ChatResponse::text("已写 R-01 到 R-06，但第 6 条写完后响应超时，无法确认是否落盘，已记待核查，没有盲目重发。"),
        ],
    ));
    let router = Router::single(model);
    let (r1, _sess) = s.run(
        &router,
        AgentConfig::default(),
        "把 R-01 到 R-10 共 10 条记录的负责人都改成李雷",
    );
    // The 6th record's write landed despite the lost reply.
    let mid: Value = serde_json::from_str(&s.files()["records.json"]).unwrap();
    assert_eq!(mid["records"][5]["owner"], "李雷");
    assert_eq!(s.approvals_with_status("needs_review"), 1);
    // The turn-end check replaces the scripted answer with the
    // honest needs-review notice (the write landed but its outcome
    // could not be verified from the receipt).
    assert!(answer(&r1).contains("核对"), "{}", answer(&r1));
    assert!(answer(&r1).contains("不明"), "{}", answer(&r1));

    // Turn 2: re-check state first, then write only the remainder.
    // It runs in a FRESH session: the turn-1 needs_review approval
    // stays unresolved by design (a human must confirm it), and the
    // turn-end check re-reports any unresolved approval in the same
    // session — including the earlier write's verify record, whose
    // readback no longer matches because the file has since moved
    // on. The retry's own verification therefore belongs to a new
    // session; the old approval is still asserted needs_review below.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("4", "fs_read", json!({"path": "records.json"}))]),
            ChatResponse::calls(vec![call(
                "5",
                "fs_write",
                json!({"path": "records.json", "content": records_json(10)}),
            )]),
            ChatResponse::text("已核对：R-01 到 R-06 已是李雷，只补写了 R-07、R-08、R-09、R-10。最终 R-01、R-02、R-03、R-04、R-05、R-06、R-07、R-08、R-09、R-10 全部是李雷，无重复、无漏更。"),
            ChatResponse::text("已核对：R-01 到 R-06 已是李雷，只补写了 R-07、R-08、R-09、R-10。最终 R-01、R-02、R-03、R-04、R-05、R-06、R-07、R-08、R-09、R-10 全部是李雷，无重复、无漏更。"),
        ],
    ));
    let (r2, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "核对 records.json 的负责人，把没改完的改成李雷",
    );
    let final_state: Value = serde_json::from_str(&s.files()["records.json"]).unwrap();
    let owners: Vec<&str> = final_state["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["owner"].as_str().unwrap())
        .collect();
    assert_eq!(owners, vec!["李雷"; 10], "{final_state}");
    let ids: Vec<&str> = final_state["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 10);
    assert_eq!(
        s.approvals_with_status("needs_review"),
        1,
        "no new ambiguity"
    );
    assert_eq!(s.sent_count(), 0);
    let a = answer(&r2);
    for i in 1..=10 {
        assert!(a.contains(&format!("R-{i:02}")), "{a}");
    }
}

// ---------------- category 4: identity & authorization (PRIORITY) ----------------

#[test]
fn auth_same_name_grant_only_the_determinable_one() {
    // (1) grants.json holds exactly one grant: 张伟 (M-01); neither
    //     李雷 is granted; (2) one file write, zero sends/events;
    //     (3) the answer names both 李雷s (dept + email) and asks
    //     which one, while confirming 张伟 was done; (4) one turn.
    // The determinable part proceeds; only the authorization-
    // affecting ambiguity stops and asks.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "project/members.json",
        r#"{"members": [
          {"id": "M-01", "name": "张伟", "email": "zhang.wei@corp.example", "dept": "研发部"},
          {"id": "M-02", "name": "李雷", "email": "li.lei@corp.example", "dept": "研发部"},
          {"id": "M-03", "name": "李雷", "email": "li.lei@partner.example", "dept": "市场部"}
        ]}"#,
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "project/members.json"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "project/grants.json", "content": "[{\"member\": \"M-01\", \"name\": \"张伟\", \"permission\": \"只读\"}]"}),
            )]),
            ChatResponse::text("已给张伟（研发部，zhang.wei@corp.example）开只读权限。李雷有两个：研发部 li.lei@corp.example 和市场部 li.lei@partner.example，请确认是哪一个，我先没开。"),
            ChatResponse::text("已给张伟（研发部，zhang.wei@corp.example）开只读权限。李雷有两个：研发部 li.lei@corp.example 和市场部 li.lei@partner.example，请确认是哪一个，我先没开。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "给张伟和李雷开只读权限",
    );
    let grants: Value = serde_json::from_str(&s.files()["project/grants.json"]).unwrap();
    assert_eq!(grants.as_array().unwrap().len(), 1, "{grants}");
    assert_eq!(grants[0]["member"], "M-01");
    assert_eq!(grants[0]["permission"], "只读");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("张伟"), "{a}");
    assert!(
        a.contains("li.lei@corp.example") && a.contains("li.lei@partner.example"),
        "{a}"
    );
    assert!(a.contains("研发部") && a.contains("市场部"), "{a}");
    assert!(a.contains("确认"), "{a}");
}

// ---------------- category 2: write & verify (ticket intake) ----------------

#[test]
fn write_ticket_intake_dedups_existing() {
    // (1) tickets.json ends with 8 tickets: the 2 pre-existing
    //     ones unchanged, 6 new ones with fresh IDs T-103..T-108,
    //     no duplicates; (2) one file write, zero sends/events;
    //     (3) the answer reports each of the 8 inputs by ID and
    //     says the 2 existing ones were not re-created; (4) one turn.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "tickets/tickets.json",
        r#"{"tickets": [
          {"id": "T-101", "title": "登录页错别字"},
          {"id": "T-102", "title": "导出 CSV 乱码"}
        ]}"#,
    );
    let new_tickets = serde_json::to_string_pretty(&json!({"tickets": [
        {"id": "T-101", "title": "登录页错别字"},
        {"id": "T-102", "title": "导出 CSV 乱码"},
        {"id": "T-103", "title": "深色模式"},
        {"id": "T-104", "title": "批量导入"},
        {"id": "T-105", "title": "搜索高亮"},
        {"id": "T-106", "title": "移动端适配"},
        {"id": "T-107", "title": "密码强度提示"},
        {"id": "T-108", "title": "导出 PDF"},
    ]}))
    .unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "tickets/tickets.json"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "tickets/tickets.json", "content": new_tickets}),
            )]),
            ChatResponse::text("已录入：T-101 登录页错别字和 T-102 导出 CSV 乱码已存在，未重复建。新建 T-103 深色模式、T-104 批量导入、T-105 搜索高亮、T-106 移动端适配、T-107 密码强度提示、T-108 导出 PDF。"),
            ChatResponse::text("已录入：T-101 登录页错别字和 T-102 导出 CSV 乱码已存在，未重复建。新建 T-103 深色模式、T-104 批量导入、T-105 搜索高亮、T-106 移动端适配、T-107 密码强度提示、T-108 导出 PDF。"),
        ],
    ));
    let (r, _sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "把这 8 条需求录入工单：登录页错别字、导出 CSV 乱码、深色模式、批量导入、搜索高亮、移动端适配、密码强度提示、导出 PDF",
    );
    let tickets: Value = serde_json::from_str(&s.files()["tickets/tickets.json"]).unwrap();
    let list = tickets["tickets"].as_array().unwrap();
    assert_eq!(list.len(), 8, "{tickets}");
    let mut ids: Vec<&str> = list.iter().map(|t| t["id"].as_str().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8, "IDs must be unique");
    assert_eq!(list[0]["title"], "登录页错别字");
    assert_eq!(list[1]["title"], "导出 CSV 乱码");
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    for id in [
        "T-101", "T-102", "T-103", "T-104", "T-105", "T-106", "T-107", "T-108",
    ] {
        assert!(a.contains(id), "{a}");
    }
    assert!(a.contains("未重复建") || a.contains("已存在"), "{a}");
}

// ---------------- category 3/4: share scope disambiguation ----------------

#[test]
fn auth_share_asks_before_external_group() {
    // (1) shares.json, written only after the user clarifies, names
    //     the internal group G-1 and never the external group G-2;
    //     (2) one file write, zero sends/events; (3) turn 1 names
    //     both groups and the external-member risk and asks; turn 2
    //     confirms the internal-only share; (4) two turns.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "workspace/groups.json",
        r#"{"groups": [
          {"id": "G-1", "name": "内部项目组", "members": ["a@corp.example", "b@corp.example"]},
          {"id": "G-2", "name": "项目协作群", "members": ["a@corp.example", "partner@vendor.example"]}
        ]}"#,
    );
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "workspace/groups.json"}))]),
            ChatResponse::text("团队有两个：内部项目组（全是内部成员）和项目协作群（含外部成员 partner@vendor.example）。把发布方案给哪个看？发给协作群会把内部方案共享给外部成员，请确认。"),
            ChatResponse::text("团队有两个：内部项目组（全是内部成员）和项目协作群（含外部成员 partner@vendor.example）。把发布方案给哪个看？发给协作群会把内部方案共享给外部成员，请确认。"),
        ],
    ));
    let router = Router::single(model);
    let (r1, sess) = s.run(&router, AgentConfig::default(), "把发布方案给团队看");
    assert!(
        !s.files().contains_key("shares.json"),
        "nothing shared before clarifying"
    );
    let a1 = answer(&r1);
    assert!(
        a1.contains("内部项目组") && a1.contains("项目协作群"),
        "{a1}"
    );
    assert!(a1.contains("partner@vendor.example"), "{a1}");

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "shares.json", "content": "{\"doc\": \"发布方案\", \"group\": \"G-1\", \"group_name\": \"内部项目组\"}"}),
            )]),
            ChatResponse::text("已把发布方案分享给内部项目组，没有分享给含外部成员的项目协作群。"),
            ChatResponse::text("已把发布方案分享给内部项目组，没有分享给含外部成员的项目协作群。"),
        ],
    ));
    let r2 = s.run_in(
        &Router::single(model),
        AgentConfig::default(),
        &sess,
        "内部项目组",
    );
    let share: Value = serde_json::from_str(&s.files()["shares.json"]).unwrap();
    assert_eq!(share["group"], "G-1");
    assert_eq!(share["doc"], "发布方案");
    assert!(!s.files()["shares.json"].contains("G-2"));
    assert_eq!(s.sent_count(), 0);
    let a2 = answer(&r2);
    assert!(a2.contains("内部项目组"), "{a2}");
}
