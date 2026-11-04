//! Sub-agents as child processes: spawn fallback, the orphan watchdog,
//! and grant/approval replay across the process boundary.

#[path = "subagent_process/common.rs"]
mod common;
use common::*;

use hatch_agent::{
    subagent::ProcessTask, Agent, RunOptions, RunStatus, RunStore, SessionStore, StopReason,
};
use hatch_llm::ChatResponse;
use hatch_policy::{ActionClass, ApprovalQueue, GrantStore, NewGrant};
use hatch_tools::builtin::default_registry;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn spawn_failure_falls_back_to_in_process() {
    let e = env();
    let now = hatch_core::time::now();
    let router = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                ChatResponse::calls(vec![call("c1", "memory_search", json!({"query": "预算"}))])
            } else {
                ChatResponse::text("子结论：没有预算信息。")
            }
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "查预算", "type": "explore"}),
            )])
        } else {
            ChatResponse::text("汇总完成")
        }
    });
    let mut cfg = proc_config(&e, 60);
    cfg.delegation.as_mut().unwrap().binary = Some(PathBuf::from("/nonexistent/hatch"));
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: cfg,
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent
        .run(&s, "查预算", &now, RunOptions::default())
        .unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(
        r.answer.as_deref().unwrap().contains("汇总完成"),
        "{:?}",
        r.answer
    );
    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    let tail = e.db.audit_tail(10).unwrap();
    assert!(tail.iter().any(|a| a.event == "spawn_fallback"), "{tail:?}");
}

#[test]
fn orphan_watchdog_exits_when_stdin_closes() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "slow.json",
        json!([{"sleep_secs": 30}, {"text": "never"}]),
    );
    let _guard = ScriptGuard::set(&script);
    let task = ProcessTask {
        run_id: "sa_watchdog".into(),
        agent_type: "explore".into(),
        task: "慢慢来".into(),
        instructions: String::new(),
        max_steps: 12,
        depth: 1,
        max_depth: 2,
        max_parallel: 3,
        max_tasks: 6,
        session: "ses_test".into(),
        parent_ref: "session:ses_test".into(),
        tainted: false,
        origin: "subagent:sa_watchdog".into(),
        user_name: "用户".into(),
        user_email: String::new(),
        tz: "Asia/Shanghai".into(),
        workload: "main".into(),
        tools: vec!["memory_search".into()],
        memory_context: vec![],
        skills: vec![],
        home: None,
        workspace: None,
        scratch_dir: dir.path().join("scratch"),
        timeout_secs: 120,
        grants: vec![],
    };
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_hatch"))
        .arg("__subagent-run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdin
        .write_all(serde_json::to_string(&task).unwrap().as_bytes())
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    // Wait for the first progress line (the child is running the turn).
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        while stdout.read_line(&mut buf).is_ok() {
            if buf.is_empty() {
                break;
            }
            if tx.send(std::mem::take(&mut buf)).is_err() {
                break;
            }
        }
    });
    let first = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("no progress from the child");
    assert!(first.contains("\"type\":\"progress\""), "{first}");
    // Simulate the parent dying: close stdin. The child must exit.
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                panic!("child survived stdin EOF (orphan watchdog failed)")
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.wait();
}

#[test]
fn process_child_grant_is_consumed_once() {
    let e = env();
    let now = hatch_core::time::now();
    GrantStore::new(&e.db)
        .add(
            &NewGrant {
                class: ActionClass::SendMessage,
                recipients: vec!["*@corp.com".into()],
                max_uses: None,
                allow_when_tainted: false,
                expires_at: None,
                note: "team replies".into(),
            },
            &now,
        )
        .unwrap();
    let script = write_script(
        e._dir.path(),
        "mail.json",
        json!([
            {"tool_calls": [{"id": "c1", "name": "propose_email", "arguments": {"to": ["partner@corp.com"], "subject": "Re: 报价", "body": "确认。"}}]},
            {"text": "已提交审批，尚未发送。"}
        ]),
    );
    let _guard = ScriptGuard::set(&script);
    let router = model(|r| {
        if tool_results(r) == 0 {
            ChatResponse::calls(vec![call(
                "p1",
                "delegate",
                json!({"task": "给 partner@corp.com 回信确认报价", "type": "execute"}),
            )])
        } else {
            ChatResponse::text("回信已处理。")
        }
    });
    let tools = default_registry();
    let agent = Agent {
        db: &e.db,
        router: &router,
        tools: &tools,
        executors: &e.exec,
        gate: &e.gate,
        config: proc_config(&e, 60),
    };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent
        .run(&s, "处理报价邮件", &now, RunOptions::default())
        .unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.approvals.len(), 1, "child approval bubbles up: {r:?}");
    let a = ApprovalQueue::new(&e.db).get(&r.approvals[0]).unwrap();
    assert!(a.origin.starts_with("subagent:sa_"), "{}", a.origin);
    assert_eq!(
        a.status.as_str(),
        "executed",
        "the grant auto-executed in the child"
    );
    let grants = GrantStore::new(&e.db).list().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].used, 1, "the parent consumed the grant once");
    assert!(no_subagent_process());
}
