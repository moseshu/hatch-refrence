//! `hatch chat` / `hatch ask`

use crate::app::App;
use crate::interrupt;
use anyhow::Result;
use hatch_agent::{
    Agent, AgentConfig, Delegation, Progress, ProgressKind, RunOptions, SessionStore, StopReason,
    TypeRegistry,
};
use hatch_llm::StreamEvent;
use hatch_policy::Gatekeeper;
use hatch_tools::{ExecutorRegistry, Registry, Workspace};
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
    d.timeout_secs = a.subagent_timeout_secs.max(1);
    // The child process reads its model config from this home and works
    // in this workspace; the binary defaults to the current exe.
    d.home = Some(app.home.clone());
    d.workspace = ws.map(|w| w.root().to_path_buf());
    d.max_depth = a.max_depth;
    d.max_parallel = a.max_parallel.max(1);
    // Async delegation: `delegate` returns the run id at once and the
    // result returns as a handoff in a later turn. The registry is this
    // process's own (chat, daemon and transport share it); a child
    // process clears both fields again in `subagent_run`, so nested
    // delegation stays synchronous.
    d.async_mode = true;
    d.registry = Some(app.subagent_registry.clone());
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

/// Everything one turn needs except the [`Agent`] itself: router,
/// gatekeeper, executors, tool registry, delegation and the active
/// skills. Shared by the CLI chat and the product API so both turns run
/// through the identical tool set and policy.
pub(crate) struct TurnSetup {
    pub gate: Gatekeeper,
    pub executors: ExecutorRegistry,
    pub tools: Registry,
    pub delegation: Option<Delegation>,
    pub skill_bodies: Vec<String>,
    pub skill_tools: Option<Vec<String>>,
}

/// The router is built by the caller (the CLI from config, the product
/// API from its injectable factory) so tests can run turns against a
/// scripted model.
pub(crate) fn setup_turn(app: &App, workspace: Option<&Path>) -> Result<TurnSetup> {
    let gate = app.policy()?;
    let executors = app.executors(false)?;
    // One registry construction, shared with the `hatch plan` commands
    // (see commands::tools): the planner validates against exactly the
    // registry the executor runs with.
    let set = super::tools::tool_registry(app, workspace)?;
    let deleg = delegation(app, set.workspace.as_ref());
    let skill_bodies: Vec<String> = set
        .active_skills
        .iter()
        .map(|s| format!("技能 {}：\n{}", s.name, s.body))
        .collect();
    Ok(TurnSetup {
        gate,
        executors,
        tools: set.tools,
        delegation: deleg,
        skill_bodies,
        skill_tools: set.skill_tools,
    })
}

/// Unread background-worker output (daily brief, memory report) for the
/// context, marked read once loaded. Its presence taints the turn.
pub(crate) fn background(app: &App) -> Result<Vec<String>> {
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

/// How this turn relates to a plan: none, make a new one (`--plan`), or
/// bind an existing one (`--plan-id`, D2). The two are mutually exclusive.
struct PlanOpt {
    make: bool,
    bind: Option<String>,
}

fn turn(
    app: &App,
    session: &str,
    text: &str,
    stream: bool,
    workspace: Option<&Path>,
    bg: &[String],
    plan: PlanOpt,
) -> Result<()> {
    let router = app.router(app.default_limits())?;
    let setup = setup_turn(app, workspace)?;
    // D2: `--plan` (make a new plan) and `--plan-id` (bind an existing
    // one) contradict each other; a bound plan must exist and be
    // approved/executing — fail fast here instead of silently running
    // unanchored.
    if plan.make && plan.bind.is_some() {
        anyhow::bail!("--plan 与 --plan-id 不能同时用：前者制定新计划，后者绑定已有计划");
    }
    let bound = match &plan.bind {
        Some(id) => {
            let p = hatch_plan::PlanStore::new(&app.db)
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("计划 {id} 不存在"))?;
            if !matches!(
                p.status,
                hatch_plan::PlanStatus::Approved | hatch_plan::PlanStatus::Executing
            ) {
                anyhow::bail!(
                    "计划 {id} 处于 {}，不能绑定（须已批准或执行中）",
                    p.status.as_str()
                );
            }
            Some(id.clone())
        }
        None => None,
    };
    // Plan mode (CLI `--plan` or `[agent] plan_mode = true`): the model
    // produces a structured plan first and the turn pauses for approval
    // instead of executing. Same Plan types as `hatch plan new`; the turn
    // loop itself is untouched.
    if plan.make || app.config.agent.plan_mode {
        let now = hatch_core::time::now();
        let ws = app.workspace(workspace)?;
        let (steps, model) =
            hatch_plan::request_plan(&router, &setup.tools, ws.as_ref(), text)?;
        let plan = hatch_plan::PlanStore::new(&app.db).create(text, "", &model, steps, false, &now)?;
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
        tools: &setup.tools,
        executors: &setup.executors,
        gate: &setup.gate,
        config: AgentConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            max_tokens: app.config.limits.max_tokens,
            max_steps: app.config.agent.max_steps,
            salience_half_life_days: app.config.agent.salience_half_life_days,
            total_budget: app.config.agent.total_budget,
            window_ratio: app.config.agent.window_ratio,
            context_window: router.context_window(hatch_llm::Workload::Main),
            delegation: setup.delegation,
            background: bg.to_vec(),
            skills: setup.skill_bodies,
            skill_tools: setup.skill_tools,
            plan_id: bound,
            reanchor_every: app.config.agent.reanchor_every,
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
                on_step_event: None,
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
                on_step_event: None,
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
        StopReason::StepLimit | StopReason::Cancelled(_) | StopReason::PlanDeviation(_) => {
            println!("{}", r.receipt())
        }
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
    plan_id: Option<String>,
) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, false)?;
    interrupt::install();
    let bg = background(&app)?;
    turn(&app, &id, text, stream, workspace, &bg, PlanOpt { make: plan, bind: plan_id })
}

pub fn repl(
    home: Option<&Path>,
    session: Option<String>,
    new: bool,
    stream: bool,
    workspace: Option<&Path>,
    plan: bool,
    plan_id: Option<String>,
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
                if let Err(e) = turn(&app, &id, text, stream, workspace, &bg, PlanOpt { make: plan, bind: plan_id.clone() }) {
                    eprintln!("错误：{e:#}");
                }
            }
        }
    }
    Ok(())
}
