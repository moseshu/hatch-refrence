//! Drill for task 6 (a): twice-compacted, deliberately corrupted
//! summaries cannot move the ledger — the context, task list, plan
//! version, pending approvals and receipts reload intact from a
//! file-backed DB. Everything runs on the scripted tools / test
//! doubles: no live model, no real mail.

#![cfg(test)]

use hatch_agent::SessionStore;
use hatch_core::time::Timestamp;
use hatch_core::Trust;
use hatch_llm::ChatMessage;
use hatch_plan::{latest_plan_approval, PlanStep, PlanStore, RiskLevel, StepStatus};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use hatch_tasks::{
    EvidenceRef, ExecStatus, StepSpec, TaskApprovals, TaskContext, TaskContextStore, TaskStore,
    UnitStore,
};
use serde_json::json;
use tempfile::TempDir;

fn now() -> Timestamp {
    hatch_core::time::now()
}

fn step(id: &str, tool: &str) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: format!("步骤 {id}"),
        tool: tool.into(),
        arguments: json!({}),
        files: vec![],
        expected: "完成".into(),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

fn step_spec(id: &str, tool: &str) -> StepSpec {
    StepSpec {
        step_id: id.into(),
        title: format!("步骤 {id}"),
        tool: tool.into(),
        arguments: json!({}),
        expected: "完成".into(),
    }
}

/// (a) §10.3 acceptance: the summary is deliberately corrupted twice
/// (a todo dropped, a pending approval marked done, an old recipient
/// restated) and the DB is reopened from disk — the ledger must still
/// load exactly what was saved.
#[test]
fn compression_twice_with_corrupted_summary_keeps_the_ledger() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hatch.db");
    let db = Db::open(&path).unwrap();
    let tasks = TaskStore::new(&db);
    let task = tasks
        .create(
            "test",
            "长任务：整理会议材料",
            "worker",
            "",
            3,
            None,
            &now(),
        )
        .unwrap();
    tasks.set_budget(&task.id, 10, &now()).unwrap();
    let plan = PlanStore::new(&db)
        .create(
            "长任务：整理会议材料",
            "完成长任务：整理会议材料",
            "",
            vec![step("s1", "fs_read"), step("s2", "fs_read")],
            false,
            &now(),
        )
        .unwrap();
    tasks.set_plan_id(&task.id, &plan.id, &now()).unwrap();
    // The long-task context: goal, facts, a permission constraint,
    // evidence, open items.
    let ctx = TaskContext {
        task_id: task.id.clone(),
        goal: "完成长任务：整理会议材料".into(),
        facts: vec![
            "已确认：李婷是工作邮箱的联系人".into(),
            "会议时间：11月20日 10:00".into(),
        ],
        constraints: vec![
            "不得向未确认的邮箱地址发邮件".into(),
            "发件前必须经我批准".into(),
        ],
        evidence: vec![
            EvidenceRef::new("mail", "m1", "李婷的回复"),
            EvidenceRef::new("receipt", "rcpt-s1", "s1 的回执"),
        ],
        open_items: vec!["等王宇回复会议时间".into()],
    };
    TaskContextStore::new(&db).save(&ctx, &now()).unwrap();
    // A session whose history will be compacted twice. The first
    // summary drops a todo and marks the pending approval done; the
    // second restates the old recipient after the user changed it.
    let sessions = SessionStore::new(&db);
    let session = sessions.create("长任务会话", &now()).unwrap();
    sessions
        .append(
            &session,
            &ChatMessage::user("待办：1. 读李婷的邮件 2. 写简报 3. 发邀请给 li.ting@corp.example"),
            Trust::System,
            &now(),
        )
        .unwrap();
    sessions
        .append(
            &session,
            &ChatMessage::assistant("已读 5 封邮件；发邀请的审批 apr_x 待批准"),
            Trust::System,
            &now(),
        )
        .unwrap();
    sessions
        .compact(&session, 2, "待办已全部完成；发邀请已批准并发出", &now())
        .unwrap();
    sessions
        .append(
            &session,
            &ChatMessage::user("收件人改成 wang.yu@corp.example"),
            Trust::System,
            &now(),
        )
        .unwrap();
    sessions
        .append(
            &session,
            &ChatMessage::assistant("收到，已更新收件人"),
            Trust::System,
            &now(),
        )
        .unwrap();
    sessions
        .compact(&session, 5, "邀请已发给 li.ting@corp.example", &now())
        .unwrap();
    // A done step with a receipt, a pending approval linked to the
    // task, and 3 spent model calls.
    let claimed = tasks.start(&task.id, "drill", 60, &now()).unwrap();
    let lease = claimed.lease().unwrap();
    let units = UnitStore::new(&db);
    units
        .ensure_steps(
            &task.id,
            &lease,
            &[step_spec("s1", "fs_read"), step_spec("s2", "fs_read")],
            &now(),
        )
        .unwrap();
    units
        .complete_step(
            &task.id,
            &lease,
            "s1",
            ExecStatus::Done,
            "s1 的结果",
            &now(),
        )
        .unwrap();
    units
        .set_step_receipt(&task.id, &lease, "s1", "rcpt-s1", &now())
        .unwrap();
    let unit = units
        .begin_unit(&task.id, &lease, "s1", &[], &now())
        .unwrap();
    units
        .finish_unit(
            &task.id,
            &lease,
            unit.unit_seq,
            ExecStatus::Done,
            3,
            1,
            &now(),
        )
        .unwrap();
    let entry = latest_plan_approval(&db, &plan).unwrap().unwrap();
    TaskApprovals::new(&db)
        .link(&task.id, &entry.id, 1, "s1", &now())
        .unwrap();
    drop(db);
    // Reopen from the file: everything must load from the ledger,
    // exactly as saved — the corrupted summaries changed nothing.
    let db = Db::open(&path).unwrap();
    let loaded = TaskContextStore::new(&db).load(&task.id).unwrap().unwrap();
    assert_eq!(loaded, ctx, "上下文必须原样加载");
    assert_eq!(
        loaded.constraints,
        vec!["不得向未确认的邮箱地址发邮件", "发件前必须经我批准"]
    );
    let list = TaskStore::new(&db).list(None, 10).unwrap();
    assert_eq!(list.len(), 1, "任务列表以 DB 为准，恰好一个任务");
    assert_eq!(list[0].id, task.id);
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.version, 1);
    assert_eq!(plan.goal, "完成长任务：整理会议材料");
    let approval = ApprovalQueue::new(&db).get(&entry.id).unwrap();
    assert_eq!(
        approval.status,
        ApprovalStatus::Pending,
        "待审批仍是待审批，摘要里写的“已批准”不算数"
    );
    let steps = UnitStore::new(&db).list_steps(&task.id).unwrap();
    assert_eq!(steps[0].receipt_id, "rcpt-s1", "回执仍在");
    assert_eq!(steps[0].status, ExecStatus::Done);
    let view = TaskContextStore::new(&db).view(&task.id, 10).unwrap();
    assert_eq!(view.model_calls_used, 3);
    assert_eq!(view.remaining_budget, Some(7));
    // The corruption did land in the summary — that is where it
    // belongs, and where it stays harmless.
    let (summary, _) = SessionStore::new(&db).active(&session).unwrap();
    assert!(summary.unwrap().contains("li.ting@corp.example"));
}
