//! `hatch __subagent-run` — the child side of sub-agent processes.
//!
//! Hidden subcommand (dispatched before clap, like `__sandbox-helper`).
//! Connects to the parent's Unix socket (`HATCH_SUBAGENT_SOCKET`; see
//! `hatch_agent::subagent::process`), reads one JSON line ([`ProcessTask`]),
//! runs the delegated turn, and writes JSONL events back over the socket
//! (`progress` lines, then exactly one `result`). Nothing else goes over
//! the socket; diagnostics go to stderr. Without the env var (manual
//! runs, debugging) it falls back to stdin/stdout with the same shapes.
//!
//! The child watches the socket for EOF on a watchdog thread: the parent
//! keeps the connection open for the whole run, so a parent that dies
//! closes the socket and the child exits (130) instead of becoming an
//! orphan.
//!
//! The child works in the parent-owned scratch directory with its own
//! database file and never touches the shared database. Approvals it
//! queued and grants it consumed come back inside the result event, and
//! the parent replays them.
//!
//! Test mode: when `HATCH_TEST_SUBAGENT_SCRIPT` points at a JSON file of
//! canned model responses, the child runs with a scripted model and a
//! minimal tool set instead of reading config. Never set in production.

use super::subagent_test_env::{filtered_tools, test_env, ChildEnv, TEST_SCRIPT_ENV};
use crate::app::App;
use crate::commands::chat::{agent_types, setup_turn};
use anyhow::{anyhow, bail, Context, Result};
use hatch_agent::{
    subagent::{ApprovalWire, ChildEvent, ChildResult, ConsumedGrant, ProcessTask, SOCKET_ENV},
    Agent, AgentConfig, Delegation, Progress, RunOptions, SessionStore, SpawnMode, StopReason,
    TypeRegistry,
};
use hatch_core::time::{now, parse_tz, to_rfc3339};
use hatch_llm::Workload;
use hatch_policy::{ApprovalQueue, GrantStore, NewGrant};
use hatch_store::Db;
use hatch_tools::Workspace;
use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The hidden subcommand name, checked before clap parsing.
pub const SUBCOMMAND: &str = "__subagent-run";

pub fn main() -> i32 {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("子代理启动失败：{e:#}");
            2
        }
    }
}

fn prod_env(task: &ProcessTask, db: Arc<Db>) -> Result<(App, ChildEnv)> {
    let home = task
        .home
        .clone()
        .ok_or_else(|| anyhow!("子代理任务缺少 home，无法读取配置"))?;
    let mut app = App::open_with_db(Some(&home), db)?;
    // No browser in a child process: sessions and Chromium profiles are
    // owned by the parent process (see BROWSER_TOOLS in hatch-agent).
    app.config.browser.enabled = false;
    let router = app.router(app.default_limits())?;
    let setup = setup_turn(&app, task.workspace.as_deref())?;
    let (tools, unavailable) = filtered_tools(setup.tools, &task.tools);
    Ok((
        app,
        ChildEnv {
            router,
            tools,
            executors: setup.executors,
            gate: setup.gate,
            delegation: setup.delegation,
            skill_tools: setup.skill_tools,
            unavailable,
        },
    ))
}

fn wire(a: &hatch_policy::Approval) -> ApprovalWire {
    ApprovalWire {
        idempotency_key: a.idempotency_key.clone(),
        kind: a.kind.clone(),
        class: a.class.as_str().to_string(),
        title: a.title.clone(),
        preview: a.preview.clone(),
        payload: a.payload.clone(),
        reasons: a.reasons.clone(),
        origin: a.origin.clone(),
        created_at: to_rfc3339(&a.created_at),
        expires_at: to_rfc3339(&a.expires_at),
        status: a.status.as_str().to_string(),
    }
}

/// Connect to the parent's socket, retrying briefly: the parent binds
/// before spawning, so the first attempt normally succeeds.
fn connect_socket(path: &Path) -> Result<UnixStream> {
    let mut last = String::new();
    for _ in 0..100 {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    Err(anyhow!("连接子代理 socket 失败（{}）：{last}", path.display()))
}

/// Watch one stream for EOF: the parent closing it means the parent is
/// gone, so exit (130) instead of becoming an orphan.
fn spawn_watchdog(stream: UnixStream) {
    std::thread::spawn(move || {
        let mut s = stream;
        let mut buf = [0u8; 512];
        loop {
            match s.read(&mut buf) {
                Ok(0) => std::process::exit(130),
                Ok(_) => continue,
                Err(_) => std::process::exit(130),
            }
        }
    });
}

/// Read the task line and return the task plus the event sink.
///
/// UDS when the parent passed `HATCH_SUBAGENT_SOCKET`, stdin/stdout
/// otherwise (manual runs, debugging). The message shapes are the same
/// either way: one JSON task line in, JSONL events out.
fn read_task() -> Result<(ProcessTask, Box<dyn Write + Send>)> {
    if let Some(path) = std::env::var_os(SOCKET_ENV).map(PathBuf::from) {
        let stream = connect_socket(&path)?;
        let task = {
            // Read the task from the stream itself, then drop the
            // reader: the watchdog (a clone) must never see this line.
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).context("读取 socket 任务")?;
            if line.trim().is_empty() {
                bail!("socket 任务为空：需要一行 JSON 任务");
            }
            serde_json::from_str(&line).context("解析子代理任务")?
        };
        spawn_watchdog(stream.try_clone()?);
        return Ok((task, Box::new(stream)));
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .context("读取 stdin 任务")?;
    if line.trim().is_empty() {
        bail!("stdin 为空：需要一行 JSON 任务");
    }
    let task: ProcessTask = serde_json::from_str(&line).context("解析子代理任务")?;
    // Parent gone (stdin EOF) → exit, never orphan.
    std::thread::spawn(|| {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        loop {
            buf.clear();
            match stdin.lock().read_line(&mut buf) {
                Ok(0) => std::process::exit(130),
                Ok(_) => continue,
                Err(_) => std::process::exit(130),
            }
        }
    });
    Ok((task, Box::new(std::io::stdout())))
}

fn run() -> Result<i32> {
    let (task, sink) = read_task()?;
    let now = now();
    std::fs::create_dir_all(&task.scratch_dir).context("创建子代理工作区")?;
    let db = Arc::new(Db::open(&task.scratch_dir.join("hatch.db")).context("打开子代理数据库")?);
    let db_ref: &Db = &db;
    let test_mode = std::env::var_os(TEST_SCRIPT_ENV).is_some();
    // In production mode the `App` shares this same database handle; keep
    // it alive for the whole run (its `db` field is the same `Arc`).
    let _app: Option<App>;
    let env;
    if test_mode {
        env = test_env(&task)?;
        _app = None;
    } else {
        let (a, e) = prod_env(&task, db.clone())?;
        _app = Some(a);
        env = e;
    }
    let sink = Arc::new(Mutex::new(sink));
    let emit = |ev: &ChildEvent| {
        let line = serde_json::to_string(ev).unwrap_or_default();
        let mut out = sink.lock().unwrap();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    let failed = |error: String| {
        emit(&ChildEvent::Result(ChildResult {
            status: "failed".into(),
            error,
            answer: None,
            steps: 0,
            model_calls: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            tainted: false,
            approvals: vec![],
            consumed_grants: vec![],
            unavailable_tools: env.unavailable.clone(),
        }));
        Ok(0)
    };
    // Seed the parent's standing grants into the scratch database.
    for g in &task.grants {
        let Ok(class) = hatch_policy::ActionClass::parse(&g.class) else {
            continue;
        };
        let _ = GrantStore::new(db_ref).add(
            &NewGrant {
                class,
                recipients: g.recipients.clone(),
                max_uses: g.max_uses,
                allow_when_tainted: g.allow_when_tainted,
                expires_at: g
                    .expires_at
                    .as_deref()
                    .and_then(|e| hatch_core::time::parse_rfc3339(e).ok()),
                note: g.note.clone(),
            },
            &now,
        );
    }
    // The child's view of the agent types (same workspace as the parent).
    let (types, _) = match &task.workspace {
        Some(ws) => {
            let w = Workspace::new(ws).ok();
            agent_types(w.as_ref())
        }
        None => (TypeRegistry::builtin(), vec![]),
    };
    let Some(ty) = types.get(&task.agent_type) else {
        return failed(format!("未知的子代理类型 `{}`", task.agent_type));
    };
    let workload = Workload::parse(&task.workload).unwrap_or(Workload::Main);
    let mut note = if ty.instructions.is_empty() {
        hatch_agent::subagent::SUBAGENT_ROLE.to_string()
    } else {
        format!(
            "{}\n\n## 类型说明（{}）\n{}",
            hatch_agent::subagent::SUBAGENT_ROLE,
            ty.name,
            ty.instructions
        )
    };
    // The task also carries the parent's narrowed instructions (e.g. the
    // no-browser note): append whatever the type text did not already say.
    let extra = task
        .instructions
        .strip_prefix(ty.instructions.as_str())
        .unwrap_or(task.instructions.as_str())
        .trim();
    if !extra.is_empty() {
        note.push_str("\n\n");
        note.push_str(extra);
    }
    let delegation = ty.may_delegate().then(|| {
        let mut d = env
            .delegation
            .clone()
            .unwrap_or_else(|| Delegation::root(Arc::new(TypeRegistry::builtin())));
        d.depth = task.depth;
        d.session = task.session.clone();
        d.agent_id = task.run_id.clone();
        d.max_depth = task.max_depth;
        d.max_parallel = task.max_parallel;
        d.max_tasks = task.max_tasks;
        d.spawn = SpawnMode::Process;
        d.binary = None;
        d.home = task.home.clone();
        d.workspace = task.workspace.clone();
        d.timeout_secs = task.timeout_secs;
        d
    });
    let config = AgentConfig {
        user_name: task.user_name.clone(),
        user_email: task.user_email.clone(),
        tz: parse_tz(&task.tz).unwrap_or(chrono_tz::Asia::Shanghai),
        max_steps: task.max_steps,
        workload,
        system_note: Some(note),
        origin: Some(task.origin.clone()),
        start_tainted: task.tainted,
        background: Vec::new(),
        skills: task.skills.clone(),
        skill_tools: env.skill_tools.clone(),
        memory_context: task.memory_context.clone(),
        delegation,
        depth: task.depth,
        agent_id: task.run_id.clone(),
        ..Default::default()
    };
    let child_session =
        SessionStore::new(db_ref).create(&format!("subagent:{}", task.run_id), &now)?;
    let observe = |p: &Progress| {
        emit(&ChildEvent::Progress {
            kind: p.kind.as_str().to_string(),
            detail: p.detail.clone(),
        });
    };
    let agent = Agent {
        db: db_ref,
        router: &env.router,
        tools: &env.tools,
        executors: &env.executors,
        gate: &env.gate,
        config,
    };
    let rep = match agent.run(
        &child_session,
        &format!("任务书：\n{}", task.task),
        &now,
        RunOptions {
            on_delta: None,
            cancel: hatch_core::CancelToken::new(),
            observer: Some(&observe),
        },
    ) {
        Ok(r) => r,
        Err(e) => return failed(format!("子代理回合失败：{e}")),
    };
    let (status, error) = match &rep.stop {
        StopReason::Answered => ("completed", String::new()),
        StopReason::Cancelled(why) => ("cancelled", why.clone()),
        StopReason::StepLimit => ("failed", format!("达到步数上限（{} 步）", rep.steps)),
        StopReason::Budget(m) => ("failed", format!("调用预算用尽：{m}")),
        StopReason::ModelError(m) => ("failed", m.clone()),
    };
    let approvals: Vec<ApprovalWire> = rep
        .approvals
        .iter()
        .filter_map(|id| ApprovalQueue::new(db_ref).get(id).ok().map(|a| wire(&a)))
        .collect();
    let consumed: Vec<ConsumedGrant> = GrantStore::new(db_ref)
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.used > 0)
        .map(|g| ConsumedGrant {
            class: g.class.as_str().to_string(),
            recipients: g.recipients,
            note: g.note,
            uses: g.used,
        })
        .collect();
    emit(&ChildEvent::Result(ChildResult {
        status: status.into(),
        error,
        answer: rep.answer,
        steps: rep.steps,
        model_calls: rep.model_calls,
        prompt_tokens: rep.usage.prompt_tokens,
        completion_tokens: rep.usage.completion_tokens,
        tainted: rep.tainted,
        approvals,
        consumed_grants: consumed,
        unavailable_tools: env.unavailable.clone(),
    }));
    Ok(0)
}
