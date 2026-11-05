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
