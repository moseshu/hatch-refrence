//! `hatch chat` / `hatch ask`

use crate::app::App;
use crate::interrupt;
use anyhow::Result;
use hatch_agent::{
    Agent, AgentConfig, Delegation, Progress, ProgressKind, RunOptions, SessionStore, StopReason,
    TypeRegistry,
};
use hatch_llm::StreamEvent;
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::skills::add_skill_tools;
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
    Ok(s.create(
        &format!("prompt {}", hatch_agent::prompt::prompt_version()),
        &now,
    )?)
}

/// Built-in sub-agent types plus `<workspace>/agents/*.md`.
pub fn agent_types(ws: Option<&Workspace>) -> (TypeRegistry, Vec<String>) {
    let mut types = TypeRegistry::builtin();
    let warnings = ws
        .map(|w| types.load_dir(&w.root().join("agents")))
        .unwrap_or_default();
    (types, warnings)
}

/// Sub-agent delegation for a turn. Public so the transport runner
/// (`commands::transport`) builds the same delegation as `chat`.
pub fn delegation(app: &App, ws: Option<&Workspace>) -> Option<Delegation> {
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
    let who = if p.depth == 0 {
        String::new()
    } else {
        format!(
            "[{}] ",
            p.agent
                .trim_start_matches("sa_")
                .chars()
                .take(8)
                .collect::<String>()
        )
    };
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
    eprintln!(
        "（已载入 {} 条后台更新：{}。本会话中对外动作与网页访问需要你确认）",
        unread.len(),
        titles.join("、")
    );
    Ok(lines)
}

const BACKGROUND_BUDGET: usize = 1500;

fn turn(
    app: &App,
    session: &str,
    text: &str,
    stream: bool,
    workspace: Option<&Path>,
    bg: &[String],
    plan_flag: bool,
) -> Result<()> {
    let router = app.router(app.default_limits())?;
    let gate = app.policy()?;
    let executors = app.executors(false)?;
    let general = app.general_tools(workspace)?;
    let deleg = delegation(app, general.workspace.as_ref());
    let mut tools = add_general_tools(default_registry(), &general);
    if let Some(b) = app.browser() {
        tools = add_browser_tools(tools, b);
    }
    // MCP tools: connected once per process; a failed server only warns.
    // Added before the skill filter below so an active skill allowlist
    // also applies to them.
    let mcp = app.mcp();
    for w in &mcp.warnings {
        eprintln!("（{w}）");
    }
    for (server, info) in &mcp.tools {
        if let Some(client) = mcp.clients.lock().unwrap().get(server).cloned() {
            tools = tools.with(hatch_mcp::mcp_tool(server, info.clone(), client));
        }
    }
    // Connector tools: installed manifests under $HATCH_HOME/connectors.
    // A bad manifest only warns (see connector_tools); side-effecting ops
    // go through the approval queue like any other tool.
    for t in app.connector_tools() {
        tools = tools.with(t);
    }
    // Artifact/widget tools: show_options / create_artifact /
    // share_artifact. Added before the plan branch below, so plan mode
    // validates against the same registry. The chat has no UI: widgets
    // surface as their text fallback; the card JSON travels in the tool
    // result for MCP clients.
    for t in app.artifact_tools() {
        tools = tools.with(t);
    }
    // Skills: list/load tools, plus active skills as a context layer.
    // A skill's `tools` allowlist narrows the registry for this turn;
    // list_skills / load_skill are never filtered out.
    let skill_reg = Arc::new(app.skill_registry());
    tools = add_skill_tools(tools, skill_reg, &app.home);
    let active = app.active_skills();
    let mut skill_tools: Option<Vec<String>> = None;
    if !active.is_empty() {
        let mut allowed: Vec<String> = Vec::new();
        for s in &active {
            for t in &s.tools {
                if !allowed.contains(t) {
                    allowed.push(t.clone());
                }
            }
        }
        let names = active
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
            .join("、");
        if allowed.is_empty() {
            eprintln!("（已加载技能：{names}）");
        } else {
            skill_tools = Some(allowed.clone());
            let mut keep: Vec<&str> = allowed.iter().map(String::as_str).collect();
            keep.extend(["list_skills", "load_skill"]);
            tools = tools.filtered(&keep);
            eprintln!(
                "（已加载技能：{names}；工具限定为：{}）",
                allowed.join("、")
            );
        }
    }
    let skill_bodies: Vec<String> = active
        .iter()
        .map(|s| format!("技能 {}：\n{}", s.name, s.body))
        .collect();
    // Plan mode (CLI `--plan` or `[agent] plan_mode = true`): the model
    // produces a structured plan first and the turn pauses for approval
    // instead of executing. Same Plan types as `hatch plan new`; the turn
    // loop itself is untouched.
    if plan_flag || app.config.agent.plan_mode {
        let now = hatch_core::time::now();
        let (steps, model) =
            hatch_plan::request_plan(&router, &tools, general.workspace.as_ref(), text)?;
        let plan = hatch_plan::PlanStore::new(&app.db).create(text, &model, steps, false, &now)?;
        app.record_usage(&router, &now)?;
        crate::commands::plan::print_plan(&plan);
        println!(
            "计划已制定，尚未执行。批准：`hatch plan approve {}`；查看：`hatch plan show {}`；执行：`hatch plan run {}`。",
            plan.id, plan.id, plan.id
        );
        return Ok(());
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
            skills: skill_bodies,
            skill_tools,
            ..Default::default()
        },
    };
    // A compaction job this session requested earlier may be due; run it
    // in the background (see chat_drain) so the turn never waits.
    super::chat_drain::drain_compaction(app, session);
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
        agent.run(
            session,
            text,
            &now,
            RunOptions {
                on_delta: Some(&mut sink),
                cancel,
                observer: Some(&observer),
            },
        )
    } else {
        agent.run(
            session,
            text,
            &now,
            RunOptions {
                on_delta: None,
                cancel,
                observer: Some(&observer),
            },
        )
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
        println!(
            "\n待你确认：{}（`hatch approvals show <id>`）",
            r.approvals.join(", ")
        );
    }
    if !r.subagents.is_empty() && r.stop == StopReason::Answered {
        eprintln!(
            "（本轮用了 {} 个子代理：`hatch agents runs --session {session}`）",
            r.subagents.len()
        );
    }
    Ok(())
}

pub fn ask(
    home: Option<&Path>,
    text: &str,
    session: Option<String>,
    stream: bool,
    workspace: Option<&Path>,
    plan: bool,
) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, false)?;
    interrupt::install();
    let bg = background(&app)?;
    turn(&app, &id, text, stream, workspace, &bg, plan)
}

pub fn repl(
    home: Option<&Path>,
    session: Option<String>,
    new: bool,
    stream: bool,
    workspace: Option<&Path>,
    plan: bool,
) -> Result<()> {
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
                for a in hatch_policy::ApprovalQueue::new(&app.db)
                    .list(Some(hatch_policy::ApprovalStatus::Pending))?
                {
                    println!("{}  {}", a.id, a.title);
                }
            }
            text => {
                if let Err(e) = turn(&app, &id, text, stream, workspace, &bg, plan) {
                    eprintln!("错误：{e:#}");
                }
            }
        }
    }
    Ok(())
}
