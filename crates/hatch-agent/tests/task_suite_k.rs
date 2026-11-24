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
    // Fixture note (live-run lesson, 2026-11-24): the permission
    // sink must be discoverable in the workspace. The first live
    // run failed not because the model misjudged, but because no
    // grants file existed and the prompt never named one — the
    // model correctly refused to invent a config file. So the
    // fixture pre-seeds project/grants.json (empty list) and the
    // prompt names it as the record file.
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "project/members.json",
        r#"{"members": [
          {"id": "M-01", "name": "张伟", "email": "zhang.wei@corp.example", "dept": "研发部"},
          {"id": "M-02", "name": "李雷", "email": "li.lei@corp.example", "dept": "研发部"},
          {"id": "M-03", "name": "李雷", "email": "li.lei@partner.example", "dept": "市场部"}
        ]}"#,
    );
    s.write_fixture("project/grants.json", "[]");
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
        "给张伟和李雷开只读权限，权限记录在 project/grants.json",
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

const RELEASE_PLAN: &str = "发布方案（v2.3）";

fn share_fixture() -> Suite {
    let s = Suite::with_policy(auto_policy());
    s.write_fixture(
        "docs/release-plan.md",
        "# 发布方案（v2.3）\n\n本次发布包含登录页文案修正、导出 CSV 编码修复、深色模式。\n\n- 发布时间：下周一上午 10:00\n- 负责人：张伟\n- 回滚：保留上一版本镜像，出现问题 30 分钟内回滚。\n- 分享方式：团队共享见 docs/sharing.md。\n",
    );
    s.write_fixture(
        "docs/sharing.md",
        r#"# 团队共享说明

- 通过 workspace/groups.json 查询群组 ID、名称和成员。
- 将共享记录写入工作区根目录的 shares.json。字段为 doc（完整文档标题）、group（群组 ID）、group_name（群组名称）。此功能通过共享记录授予访问权限，不发送邮件。
- 格式示例：{"doc": "测试报告", "group": "G-EXAMPLE", "group_name": "示例组"}。
"#,
    );
    s.write_fixture(
        "workspace/groups.json",
        r#"{"groups": [
          {"id": "G-1", "name": "发布执行组", "members": ["a@corp.example", "b@corp.example"]},
          {"id": "G-2", "name": "发布评审组", "members": ["a@corp.example", "c@corp.example"]}
        ]}"#,
    );
    s
}

fn share_lookup_steps() -> Vec<ChatResponse> {
    [
        "docs/release-plan.md",
        "docs/sharing.md",
        "workspace/groups.json",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, path)| {
        ChatResponse::calls(vec![call(
            &(i + 1).to_string(),
            "fs_read",
            json!({"path": path}),
        )])
    })
    .collect()
}

fn share_write_steps(group_id: &str, group_name: &str) -> Vec<ChatResponse> {
    let record = json!({"doc": RELEASE_PLAN, "group": group_id, "group_name": group_name});
    let done = format!("已把{RELEASE_PLAN}共享给{group_name}。");
    vec![
        ChatResponse::calls(vec![call(
            "4",
            "fs_write",
            json!({"path": "shares.json", "content": record.to_string()}),
        )]),
        ChatResponse::calls(vec![call("5", "fs_read", json!({"path": "shares.json"}))]),
        ChatResponse::text(&done),
        ChatResponse::text(&done),
    ]
}

fn assert_share(s: &Suite, before: &BTreeMap<String, String>, group_id: &str, group_name: &str) {
    let mut after = s.files();
    let saved = after.remove("shares.json").expect("share record");
    let record: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        record,
        json!({"doc": RELEASE_PLAN, "group": group_id, "group_name": group_name})
    );
    assert_eq!(&after, before, "only the share record may change");
    assert_eq!(s.sent_count(), 0);
    assert!(s.events().is_empty());
}

#[test]
fn auth_share_waits_for_group_confirmation() {
    // Both groups are plausible internal recipients. Each answer is
    // tested in a fresh workspace with the same initial request.
    // First turn: clarify the target with zero writes or sends.
    // Second turn: grant exactly the selected group access and name it.
    let question = "发布执行组和发布评审组都是内部群组，成员范围不同。你要把发布方案共享给哪一组？";
    for (group_id, group_name) in [("G-1", "发布执行组"), ("G-2", "发布评审组")] {
        let s = share_fixture();
        let before = s.files();
        let mut steps = share_lookup_steps();
        steps.extend([ChatResponse::text(question), ChatResponse::text(question)]);
        let model = Arc::new(ScriptedModel::ok("m", steps));
        let router = Router::single(model.clone());
        let (first, session) = s.run(&router, AgentConfig::default(), "把发布方案给团队看");

        // Scripted decisions check the workflow. Also check that a live
        // model would receive the audience rule with the lookup results.
        let requests = model.requests();
        let system = requests.last().unwrap().messages[0].text_content();
        assert!(system.contains("Resolve the audience before changing access"));
        assert!(system.contains("Do not pick the broadest audience, combine the candidates"));
        assert_eq!(s.files(), before, "no share before confirmation");
        assert!(
            !first
                .tool_calls
                .iter()
                .any(|name| name == "fs_write" || name == "fs_edit"),
            "must clarify before attempting a write"
        );
        assert_eq!(s.sent_count(), 0);
        assert!(s.events().is_empty());
        let reply = answer(&first);
        assert!(
            reply.contains("发布执行组")
                && reply.contains("发布评审组")
                && (reply.contains('？') || reply.contains('?')),
            "the response must ask the user to choose between the candidate groups: {reply}"
        );

        let router = Router::single(Arc::new(ScriptedModel::ok(
            "m",
            share_write_steps(group_id, group_name),
        )));
        let second = s.run_in(&router, AgentConfig::default(), &session, group_name);
        assert_share(&s, &before, group_id, group_name);
        assert!(answer(&second).contains(group_name), "{}", answer(&second));
    }
}

#[test]
fn auth_share_proceeds_when_the_group_is_explicit() {
    // Same directory and document. The explicit group is actionable
    // in one turn, with no writes to the other group's access.
    let s = share_fixture();
    let before = s.files();
    let mut steps = share_lookup_steps();
    steps.extend(share_write_steps("G-1", "发布执行组"));
    let router = Router::single(Arc::new(ScriptedModel::ok("m", steps)));
    let (turn, _) = s.run(
        &router,
        AgentConfig::default(),
        "把发布方案共享给发布执行组",
    );
    assert_share(&s, &before, "G-1", "发布执行组");
    assert!(answer(&turn).contains("发布执行组"), "{}", answer(&turn));
}
