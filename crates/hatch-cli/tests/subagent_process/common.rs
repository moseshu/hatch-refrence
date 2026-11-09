//! Shared harness for the sub-agent process tests: a temp-dir env, a
//! scripted-model router, and the serial guard for the process-global
//! test-script env var.

use hatch_agent::{AgentConfig, ChildRegistry, Delegation, SpawnMode, TypeRegistry};
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, Role, Router, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::{ExecutorRegistry, SendEmailExecutor};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// The test-script env var is process-global, so the tests that set it
/// run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

pub struct ScriptGuard {
    _lock: MutexGuard<'static, ()>,
}

impl ScriptGuard {
    pub fn set(path: &Path) -> Self {
        // A previous test may have panicked while holding the lock; the
        // env var is still usable, so recover instead of failing.
        let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HATCH_TEST_SUBAGENT_SCRIPT", path);
        ScriptGuard { _lock: lock }
    }
}

impl Drop for ScriptGuard {
    fn drop(&mut self) {
        std::env::remove_var("HATCH_TEST_SUBAGENT_SCRIPT");
    }
}

pub fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// Used by the replay tests; the main test binary does not need it.
#[allow(dead_code)]
pub fn is_child(r: &ChatRequest) -> bool {
    r.messages[0].content.contains("你的角色：子代理")
}

#[allow(dead_code)]
pub fn tool_results(r: &ChatRequest) -> usize {
    r.messages.iter().filter(|m| m.role == Role::Tool).count()
}

/// Used by the main tests; the replay test binary does not need it.
#[allow(dead_code)]
pub fn last_tool(r: &ChatRequest) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn model(f: impl Fn(&ChatRequest) -> ChatResponse + Send + Sync + 'static) -> Router {
    Router::single(Arc::new(FnModel::new("routed", move |r| Ok(f(r)))))
}

pub struct Env {
    pub db: Arc<Db>,
    pub exec: ExecutorRegistry,
    pub gate: Gatekeeper,
    pub _dir: tempfile::TempDir,
    pub project: PathBuf,
}

pub fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    Env {
        db: Arc::new(Db::open_in_memory().unwrap()),
        exec: ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(outbox, "me@126.com"))),
        gate: Gatekeeper::new(PolicyConfig::default()),
        project,
        _dir: dir,
    }
}

pub fn proc_config(e: &Env, timeout_secs: u64) -> AgentConfig {
    let mut d = Delegation::root(Arc::new(TypeRegistry::builtin()));
    d.max_depth = 2;
    d.max_parallel = 3;
    d.spawn = SpawnMode::Process;
    d.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_hatch")));
    d.home = Some(e._dir.path().join("home"));
    d.workspace = Some(e.project.clone());
    d.timeout_secs = timeout_secs;
    AgentConfig {
        delegation: Some(d),
        ..Default::default()
    }
}

/// Async variant of [`proc_config`]: the same process child, but the
/// delegation returns run ids at once and results come back as handoffs.
#[allow(dead_code)]
pub fn async_proc_config(e: &Env, registry: &Arc<ChildRegistry>, timeout_secs: u64) -> AgentConfig {
    let mut cfg = proc_config(e, timeout_secs);
    let d = cfg.delegation.as_mut().unwrap();
    d.async_mode = true;
    d.registry = Some(registry.clone());
    cfg
}

pub fn write_script(dir: &Path, name: &str, entries: Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_string(&entries).unwrap()).unwrap();
    p
}

/// No `hatch __subagent-run` process left behind (Linux only; no /proc
/// means there is nothing to inspect).
pub fn no_subagent_process() -> bool {
    let Ok(d) = std::fs::read_dir("/proc") else {
        return true;
    };
    d.flatten().all(|e| {
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        !cmd.split(|b| *b == 0).any(|p| p == b"__subagent-run")
    })
}

pub type Seen = Arc<Mutex<Vec<ChatRequest>>>;

/// Like [`model`], but keeps the requests so a test can inspect what
/// the parent model was told (handoffs, system prompts).
#[allow(dead_code)]
pub fn model_seen(f: impl Fn(&ChatRequest) -> ChatResponse + Send + Sync + 'static) -> (Router, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let m = FnModel::new("routed", move |r| {
        s2.lock().unwrap().push(r.clone());
        Ok(f(r))
    });
    (Router::single(Arc::new(m)), seen)
}

/// The system prompt of the n-th parent model request.
#[allow(dead_code)]
pub fn sys(seen: &Seen, n: usize) -> String {
    seen.lock().unwrap()[n].messages[0].content.clone()
}

/// Wait for a run to reach a terminal state (poll the store). A paused
/// run is not terminal — it waits for a resume, not for time.
#[allow(dead_code)]
pub fn wait_terminal(db: &Db, run_id: &str, timeout: std::time::Duration) -> hatch_agent::RunStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let run = hatch_agent::RunStore::new(db).get(run_id).unwrap().unwrap();
        if !matches!(
            run.status,
            hatch_agent::RunStatus::Spawned | hatch_agent::RunStatus::Running | hatch_agent::RunStatus::Paused
        ) {
            return run.status;
        }
        if std::time::Instant::now() >= deadline {
            panic!("run {run_id} did not finish in {timeout:?} (still {:?})", run.status);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The Linux state letter of a process (`T` = stopped, `S`/`R` = live).
#[allow(dead_code)]
pub fn proc_state(pid: u32) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after_comm = stat.rfind(')').unwrap();
    stat[after_comm + 2..].split_whitespace().next().unwrap().to_string()
}

/// A state transition after a signal is not instant: the kernel
/// stops/continues the task asynchronously, so poll briefly instead
/// of asserting the state of a single read.
#[allow(dead_code)]
pub fn wait_state(pid: u32, want: &str) -> bool {
    for _ in 0..100 {
        if proc_state(pid) == want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[allow(dead_code)]
pub fn wait_not_state(pid: u32, want: &str) -> bool {
    for _ in 0..100 {
        if proc_state(pid) != want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// An agent wired to the async process delegation config.
#[allow(dead_code)]
pub fn agent<'a>(e: &'a Env, router: &'a Router, registry: &'a Arc<ChildRegistry>, timeout_secs: u64) -> hatch_agent::Agent<'a> {
    let tools = hatch_tools::builtin::default_registry();
    // `tools` must outlive the agent: leak it for the test (process-local).
    let tools: &'static _ = Box::leak(Box::new(tools));
    hatch_agent::Agent {
        db: &e.db,
        router,
        tools,
        executors: &e.exec,
        gate: &e.gate,
        config: async_proc_config(e, registry, timeout_secs),
    }
}
