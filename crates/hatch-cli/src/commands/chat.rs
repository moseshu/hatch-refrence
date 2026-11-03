//! `hatch chat` / `hatch ask`

use crate::app::App;
use crate::interrupt;
use anyhow::Result;
use hatch_agent::{Agent, AgentConfig, Delegation, Progress, ProgressKind, RunOptions, SessionStore, StopReason, TypeRegistry};
use hatch_llm::StreamEvent;
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::{add_general_tools, default_registry};
use hatch_tools::Workspace;
use hatch_workers::{handoff_lines, WorkerOutputs};
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

fn session_id(app: &App, requested: Option<String>, new: bool) -> Result<String> {
    let s = SessionStore::new(&app.db);
    let now = hatch_core::time::now();
    if let Some(id) = requested {
        if !s.exists(&id)? {
            anyhow::bail!("会话 {id} 不存在（`hatch session list` 查看）");
        }
        return Ok(id);
    }
    if !new {
        if let Some(id) = s.list(false, 1)?.into_iter().next() {
            return Ok(id.id);
        }
    }
    Ok(s.create(&format!("prompt {}", hatch_agent::prompt::prompt_version()), &now)?)
}

/// Built-in sub-agent types plus `<workspace>/agents/*.md`.
pub fn agent_types(ws: Option<&Workspace>) -> (TypeRegistry, Vec<String>) {
    let mut types = TypeRegistry::builtin();
    let warnings = ws.map(|w| types.load_dir(&w.root().join("agents"))).unwrap_or_default();
    (types, warnings)
}

fn delegation(app: &App, ws: Option<&Workspace>) -> Option<Delegation> {
    let a = &app.config.agent;
    if !a.subagents {
        return None;
    }
    let (types, warnings) = agent_types(ws);
    for w in warnings {
        eprintln!("（子代理类型）{w}");
    }
    let mut d = Delegation::root(Arc::new(types));
    d.max_depth = a.max_depth;
    d.max_parallel = a.max_parallel.max(1);
    Some(d)
}

/// Print sub-agent progress (indented) and, when not streaming, the main
/// agent's tool calls. Called from several threads at once.
fn print_progress(p: &Progress, streaming: bool) {
    let pad = "  ".repeat(p.depth + 1);
    let who = if p.depth == 0 { String::new() } else { format!("[{}] ", p.agent.trim_start_matches("sa_").chars().take(8).collect::<String>()) };
    let line = match p.kind {
        ProgressKind::Delegate => format!("{pad}↳ {who}委派 {}", p.detail),
        ProgressKind::SubagentDone => format!("{pad}✔ {who}{}", p.detail),
        ProgressKind::ToolCall if p.depth > 0 || !streaming => format!("{pad}· {who}{}", p.detail),
        ProgressKind::Stopped if p.depth > 0 => format!("{pad}■ {who}{}", p.detail),
        _ => return,
    };
    eprintln!("{line}");
}

/// Unread background-worker output (daily brief, memory report) for the
/// context, marked read once loaded. Its presence taints the turn.
fn background(app: &App) -> Result<Vec<String>> {
    let w = WorkerOutputs::new(&app.db);
    let unread = w.unread()?;
    if unread.is_empty() {
        return Ok(Vec::new());
    }
    let (lines, ids) = handoff_lines(&unread, &app.tz, BACKGROUND_BUDGET);
    w.mark_read(&ids, &hatch_core::time::now())?;
    let titles: Vec<&str> = unread.iter().map(|o| o.title.as_str()).collect();
    eprintln!("（已载入 {} 条后台更新：{}。本会话中对外动作与网页访问需要你确认）", unread.len(), titles.join("、"));
    Ok(lines)
}

const BACKGROUND_BUDGET: usize = 1500;

fn turn(app: &App, session: &str, text: &str, stream: bool, workspace: Option<&Path>, bg: &[String]) -> Result<()> {
    let router = app.router(app.default_limits())?;
    let gate = app.policy()?;
    let executors = app.executors(false)?;
    let general = app.general_tools(workspace)?;
    let deleg = delegation(app, general.workspace.as_ref());
    let mut tools = add_general_tools(default_registry(), &general);
    if let Some(b) = app.browser() {
        tools = add_browser_tools(tools, b);
    }
    let agent = Agent {
        db: &app.db,
        router: &router,
        tools: &tools,
        executors: &executors,
        gate: &gate,
        config: AgentConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            max_tokens: app.config.limits.max_tokens,
            max_steps: app.config.agent.max_steps,
            delegation: deleg,
            background: bg.to_vec(),
            ..Default::default()
        },
    };
    let now = hatch_core::time::now();
    let stream = stream || app.config.limits.stream;
    let observer = move |p: &Progress| print_progress(p, stream);
    let cancel = interrupt::begin_turn();
    let mut printed = false;
    let result = if stream {
        let mut sink = |e: StreamEvent| match e {
            StreamEvent::TextDelta(t) => {
                printed = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::ToolCallStarted { name } => {
                if printed {
                    println!();
                    printed = false;
                }
                eprintln!("  · 调用工具 {name}");
            }
            StreamEvent::Done => {
                if printed {
                    println!();
                    printed = false;
                }
            }
        };
        agent.run(session, text, &now, RunOptions { on_delta: Some(&mut sink), cancel, observer: Some(&observer) })
    } else {
        agent.run(session, text, &now, RunOptions { on_delta: None, cancel, observer: Some(&observer) })
    };
    interrupt::end_turn();
    app.record_usage(&router, &now)?;
    let r = result?;
    match &r.stop {
        // When streaming, the answer is already on screen.
        StopReason::Answered if stream => {}
        StopReason::Answered => println!("{}", r.answer.clone().unwrap_or_default()),
        StopReason::StepLimit | StopReason::Cancelled(_) => println!("{}", r.receipt()),
        StopReason::Budget(m) => println!("（模型调用预算已用尽：{m}）"),
        StopReason::ModelError(m) => println!("（模型调用失败：{m}）"),
    }
    if !r.approvals.is_empty() {
        println!("\n待你确认：{}（`hatch approvals show <id>`）", r.approvals.join(", "));
    }
    if !r.subagents.is_empty() && r.stop == StopReason::Answered {
        eprintln!("（本轮用了 {} 个子代理：`hatch agents runs --session {session}`）", r.subagents.len());
    }
    Ok(())
}

pub fn ask(home: Option<&Path>, text: &str, session: Option<String>, stream: bool, workspace: Option<&Path>) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, false)?;
    interrupt::install();
    let bg = background(&app)?;
    turn(&app, &id, text, stream, workspace, &bg)
}

pub fn repl(home: Option<&Path>, session: Option<String>, new: bool, stream: bool, workspace: Option<&Path>) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, new)?;
    interrupt::install();
    println!("hatch 会话 {id}。输入 /exit 退出，/approvals 查看待审批；Ctrl-C 停止当前任务，连按两次退出。");
    let bg = background(&app)?;
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/approvals" => {
                for a in hatch_policy::ApprovalQueue::new(&app.db).list(Some(hatch_policy::ApprovalStatus::Pending))? {
                    println!("{}  {}", a.id, a.title);
                }
            }
            text => {
                if let Err(e) = turn(&app, &id, text, stream, workspace, &bg) {
                    eprintln!("错误：{e:#}");
                }
            }
        }
    }
    Ok(())
}
