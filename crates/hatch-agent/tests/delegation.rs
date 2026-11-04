//! Sub-agents end to end: narrowing, parallelism, depth limit, approvals,
//! taint inheritance and cancellation (including a running command).

use hatch_agent::{Agent, AgentConfig, Delegation, Progress, ProgressKind, RunOptions, RunStatus, RunStore, SessionStore, StopReason, TypeRegistry};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::{MailRepo, ParsedMail};
use hatch_core::{CancelToken, Trust};
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, Role, Router, ToolCall};
use hatch_policy::{ActionClass, ApprovalQueue, Gatekeeper, GrantStore, NewGrant, PolicyConfig};
use hatch_sandbox::{HelperCmd, Limits, Sandbox, SandboxMode};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{ExecCommandExecutor, ExecutorRegistry, Registry, SendEmailExecutor, Workspace};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args }
}

fn is_child(r: &ChatRequest) -> bool {
    r.messages[0].content.contains("你的角色：子代理")
}

fn task_of(r: &ChatRequest) -> String {
    r.messages.iter().find(|m| m.role == Role::User && m.content.starts_with("任务书：")).map(|m| m.content.clone()).unwrap_or_default()
}

fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

fn last_tool(r: &ChatRequest) -> String {
    r.messages.iter().rev().find(|m| m.role == Role::Tool).map(|m| m.content.clone()).unwrap_or_default()
}

type Seen = Arc<Mutex<Vec<ChatRequest>>>;

fn model(f: impl Fn(&ChatRequest) -> ChatResponse + Send + Sync + 'static) -> (Router, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let m = FnModel::new("routed", move |r| {
        s2.lock().unwrap().push(r.clone());
        Ok(f(r))
    });
    (Router::single(Arc::new(m)), seen)
}

fn config(max_depth: usize, max_parallel: usize) -> AgentConfig {
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.max_depth = max_depth;
    d.max_parallel = max_parallel;
    // These tests use scripted (in-process) models that cannot cross a
    // process boundary; the process path is covered by hatch-cli tests.
    d.spawn = hatch_agent::SpawnMode::InProcess;
    AgentConfig {
        delegation: Some(d),
        ..Default::default()
    }
}

struct Env {
    db: Db,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    Env {
        db: Db::open_in_memory().unwrap(),
        exec: ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(outbox, "me@126.com"))),
        gate: Gatekeeper::new(PolicyConfig::default()),
        _dir: dir,
    }
}

#[test]
fn explore_subagent_gets_a_narrowed_tool_set_and_reports_back() {
    let e = env();
    let now = hatch_core::time::now();
    let (router, seen) = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                ChatResponse::calls(vec![call("c1", "memory_search", json!({"query": "预算"}))])
            } else {
                ChatResponse::text("子结论：记忆里没有预算信息。")
            }
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![call("p1", "delegate", json!({"task": "查一下记忆里有没有预算信息", "type": "explore"}))])
        } else {
            ChatResponse::text(format!("汇总：{}", last_tool(r).lines().next().unwrap_or("")))
        }
    });
    let tools = default_registry();
    let agent = Agent { db: &e.db, router: &router, tools: &tools, executors: &e.exec, gate: &e.gate, config: config(2, 3) };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let events = Mutex::new(Vec::<Progress>::new());
    let observe = |p: &Progress| events.lock().unwrap().push(p.clone());
    let r = agent.run(&s, "帮我查预算", &now, RunOptions { observer: Some(&observe), ..Default::default() }).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.subagents.len(), 1);
    assert!(r.answer.unwrap().starts_with("汇总：[子代理 sa_"));

    let seen = seen.lock().unwrap();
    let parent_tools: Vec<String> = seen[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(parent_tools.contains(&"delegate".to_string()) && parent_tools.contains(&"propose_email".to_string()));
    let child_req = seen.iter().find(|r| is_child(r)).unwrap();
    let child_tools: Vec<String> = child_req.tools.iter().map(|t| t.name.clone()).collect();
    assert!(!child_tools.contains(&"propose_email".to_string()), "explore is read-only: {child_tools:?}");
    assert!(!child_tools.contains(&"delegate".to_string()), "explore cannot delegate");
    assert!(task_of(child_req).contains("预算"));
    assert!(!child_req.messages.iter().any(|m| m.content.contains("帮我查预算")), "child must not see the parent conversation");

    let run = RunStore::new(&e.db).get(&r.subagents[0]).unwrap().unwrap();
    assert_eq!((run.status, run.depth, run.agent_type.as_str()), (RunStatus::Completed, 1, "explore"));
    assert_eq!(run.parent, format!("session:{s}"));
    assert_eq!(run.model_calls, 2);
    let kinds: Vec<String> = RunStore::new(&e.db).events(&run.id).unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"tool_call".to_string()) && kinds.contains(&"answer".to_string()), "{kinds:?}");
    let ev = events.lock().unwrap();
    assert!(ev.iter().any(|p| p.kind == ProgressKind::Delegate && p.depth == 1));
    assert!(ev.iter().any(|p| p.kind == ProgressKind::SubagentDone && p.detail == "completed"));
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let handoff = history.iter().find(|m| m.message.content.starts_with("[子代理")).unwrap();
    assert_eq!(handoff.trust, Trust::Trusted);
    assert!(handoff.message.content.contains("结论：\n子结论") && handoff.message.content.contains("过程摘录"));
}

#[test]
fn parallel_runs_respect_the_width_and_keep_order() {
    let e = env();
    let now = hatch_core::time::now();
    let (current, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (c2, p2) = (current.clone(), peak.clone());
    let (router, _) = model(move |r| {
        if is_child(r) {
            let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
            p2.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(250));
            c2.fetch_sub(1, Ordering::SeqCst);
            let t = task_of(r);
            ChatResponse::text(format!("完成:{}", t.trim_start_matches("任务书：\n")))
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![call("p1", "delegate_parallel", json!({"tasks": [
                {"task": "A", "type": "explore"}, {"task": "B", "type": "explore"}, {"task": "C", "type": "research"}]}))])
        } else {
            ChatResponse::text(last_tool(r))
        }
    });
    let tools = default_registry();
    let agent = Agent { db: &e.db, router: &router, tools: &tools, executors: &e.exec, gate: &e.gate, config: config(2, 2) };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run_turn(&s, "三件事一起做", &now).unwrap();
    let out = r.answer.unwrap();
    let (a, b, c) = (out.find("完成:A").unwrap(), out.find("完成:B").unwrap(), out.find("完成:C").unwrap());
    assert!(a < b && b < c, "{out}");
    assert_eq!(peak.load(Ordering::SeqCst), 2, "two at a time");
    assert_eq!(r.subagents.len(), 3);
    let runs = RunStore::new(&e.db).list(Some(&s), 10).unwrap();
    assert!(runs.iter().all(|x| x.status == RunStatus::Completed), "{runs:?}");
}

#[test]
fn depth_limit_and_unknown_types_are_refused_and_recorded() {
    let e = env();
    let now = hatch_core::time::now();
    let (router, seen) = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                // Misbehaving child: delegates although the tool is hidden at the limit.
                ChatResponse::calls(vec![call("c1", "delegate", json!({"task": "再分一层", "type": "execute"}))])
            } else {
                ChatResponse::text("子代理自己做完了")
            }
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![
                call("p1", "delegate", json!({"task": "做点事", "type": "execute"})),
                call("p2", "delegate", json!({"task": "提权", "type": "admin"})),
            ])
        } else {
            ChatResponse::text("好")
        }
    });
    let tools = default_registry();
    let agent = Agent { db: &e.db, router: &router, tools: &tools, executors: &e.exec, gate: &e.gate, config: config(1, 3) };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    agent.run_turn(&s, "开始", &now).unwrap();
    let child_req = seen.lock().unwrap().iter().find(|r| is_child(r)).cloned().unwrap();
    assert!(!child_req.tools.iter().any(|t| t.name == "delegate"), "delegate hidden at the depth limit");
    let runs = RunStore::new(&e.db).list(Some(&s), 10).unwrap();
    assert_eq!(runs.len(), 3, "{runs:?}");
    let deep = runs.iter().find(|r| r.depth == 2).unwrap();
    assert_eq!(deep.status, RunStatus::Rejected);
    assert!(deep.error.contains("超过上限 1"), "{}", deep.error);
    assert!(deep.parent.starts_with("subagent:sa_"));
    assert_eq!(RunStore::new(&e.db).events(&deep.id).unwrap()[0].kind, "rejected");
    let unknown = runs.iter().find(|r| r.agent_type == "admin").unwrap();
    assert_eq!(unknown.status, RunStatus::Rejected);
    assert!(unknown.error.contains("未知的子代理类型"));
    assert!(runs.iter().any(|r| r.depth == 1 && r.status == RunStatus::Completed));
}

#[test]
fn subagent_approvals_are_tagged_and_inherit_the_parents_taint() {
    let e = env();
    let now = hatch_core::time::now();
    MailRepo::new(&e.db)
        .store("me", &ParsedMail {
            uid: Some(1),
            message_id: "<m1@x>".into(),
            from_addr: "partner@corp.com".into(),
            from_name: "P".into(),
            to: vec![],
            subject: "报价".into(),
            sent_at: Some(now),
            body: "请回复确认报价。".into(),
        }, &now)
        .unwrap();
    let (router, _) = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                ChatResponse::calls(vec![call("c1", "propose_email", json!({"to": ["partner@corp.com"], "subject": "Re: 报价", "body": "确认。"}))])
            } else {
                ChatResponse::text("已提交审批，尚未发送。")
            }
        } else {
            match tool_results(r) {
                0 => ChatResponse::calls(vec![call("p1", "mail_search", json!({"query": "报价"}))]),
                1 => ChatResponse::calls(vec![call("p2", "delegate", json!({"task": "给 partner@corp.com 回信确认报价", "type": "execute"}))]),
                _ => ChatResponse::text("回信已排队等你确认。"),
            }
        }
    });
    let tools = default_registry();
    let agent = Agent { db: &e.db, router: &router, tools: &tools, executors: &e.exec, gate: &e.gate, config: config(2, 3) };
    let s = SessionStore::new(&e.db).create("t", &now).unwrap();
    let r = agent.run_turn(&s, "处理报价邮件", &now).unwrap();
    assert_eq!(r.approvals.len(), 1, "child approval bubbles up: {r:?}");
    let a = ApprovalQueue::new(&e.db).get(&r.approvals[0]).unwrap();
    assert!(a.origin.starts_with("subagent:sa_"), "{}", a.origin);
    assert!(a.reasons.iter().any(|x| x.contains("不可信")), "taint inherited from the parent: {:?}", a.reasons);
    let (_, history) = SessionStore::new(&e.db).active(&s).unwrap();
    let handoff = history.iter().find(|m| m.message.content.starts_with("[子代理")).unwrap();
    assert_eq!(handoff.trust, Trust::Untrusted);
    assert!(handoff.message.content.contains("待审批"));
}

fn no_process_with_arg(arg: &str) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().all(|e| {
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let parts: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        !(parts.len() >= 2 && parts[0].ends_with(b"sleep") && parts[1] == arg.as_bytes())
    })
}

#[test]
fn cancelling_the_parent_stops_parallel_children_and_their_commands() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let sandbox = match HelperCmd::locate() {
        Some(h) => Sandbox { mode: SandboxMode::Namespaces, helper: Some(h), limits: Limits::default(), max_output: 16 * 1024 },
        None => Sandbox { mode: SandboxMode::None, helper: None, limits: Limits::default(), max_output: 16 * 1024 },
    };
    if sandbox.mode == SandboxMode::Namespaces && sandbox.probe().is_err() {
        eprintln!("SKIP cancelling_the_parent_stops_parallel_children: sandbox unavailable");
        return;
    }
    let db = Db::open_in_memory().unwrap();
    let now = hatch_core::time::now();
    GrantStore::new(&db)
        .add(&NewGrant { class: ActionClass::ExecCommand, recipients: vec!["sleep *".into()], max_uses: None, allow_when_tainted: false, expires_at: None, note: String::new() }, &now)
        .unwrap();
    let exec = ExecutorRegistry::new().with(Arc::new(ExecCommandExecutor { sandbox: Arc::new(sandbox) }));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let tools: Registry = add_general_tools(
        Registry::new(),
        &GeneralTools { workspace: Some(ws), web_fetch: false, search_backend: None, max_command_timeout_secs: 60 },
    );
    let (router, _) = model(|r| {
        if is_child(r) {
            if tool_results(r) == 0 {
                ChatResponse::calls(vec![call("c1", "shell_run", json!({"command": "sleep 39.75", "timeout_secs": 30}))])
            } else {
                ChatResponse::text(format!("命令结果：{}", last_tool(r)))
            }
        } else if tool_results(r) == 0 {
            ChatResponse::calls(vec![call("p1", "delegate_parallel", json!({"tasks": [
                {"task": "跑一个长命令", "type": "execute"}, {"task": "再跑一个长命令", "type": "execute"}]}))])
        } else {
            ChatResponse::text("不该走到这里")
        }
    });
    let agent = Agent { db: &db, router: &router, tools: &tools, executors: &exec, gate: &gate, config: config(2, 3) };
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let token = CancelToken::new();
    let t2 = token.clone();
    let started_cmds = AtomicUsize::new(0);
    let observe = |p: &Progress| {
        if p.kind == ProgressKind::ToolCall && p.detail.starts_with("shell_run") && started_cmds.fetch_add(1, Ordering::SeqCst) == 1 {
            let t3 = t2.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                t3.cancel("Ctrl-C");
            });
        }
    };
    let started = Instant::now();
    let r = agent.run(&s, "并行跑两个命令", &now, RunOptions { cancel: token, observer: Some(&observe), ..Default::default() }).unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "took {:?}", started.elapsed());
    assert_eq!(r.stop, StopReason::Cancelled("Ctrl-C".into()));
    let runs = RunStore::new(&db).list(Some(&s), 10).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|x| x.status == RunStatus::Cancelled), "{runs:?}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !no_process_with_arg("39.75") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(no_process_with_arg("39.75"), "commands survived the cancellation");
    let approvals = ApprovalQueue::new(&db).list(None).unwrap();
    assert_eq!(approvals.len(), 2, "both commands ran via the grant");
    assert!(approvals.iter().all(|a| a.note.contains("已取消")), "{:?}", approvals.iter().map(|a| &a.note).collect::<Vec<_>>());
}
