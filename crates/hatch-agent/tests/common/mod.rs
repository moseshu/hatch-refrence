//! Shared harness for the tool-bridge integration tests: an in-memory DB,
//! the default registry, and a scripted-model turn runner.

use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_llm::{Router, ToolCall};
use hatch_memory::{MemoryStore, NewClaim};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;

pub fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

// Shared across test targets; not every target uses every helper.
#[allow(dead_code)]
pub struct Harness {
    pub db: Db,
    pub tools: hatch_tools::Registry,
    pub exec: ExecutorRegistry,
    pub gate: Gatekeeper,
}

#[allow(dead_code)]
impl Harness {
    pub fn new() -> Self {
        Harness::with_policy(PolicyConfig::default())
    }

    pub fn with_policy(policy: PolicyConfig) -> Self {
        Harness {
            db: Db::open_in_memory().unwrap(),
            tools: default_registry(),
            exec: ExecutorRegistry::new(),
            gate: Gatekeeper::new(policy),
        }
    }

    // Shared across test targets; not every target uses every helper.
    #[allow(dead_code)]
    pub fn seed_claim(&self, text: &str) -> String {
        let now = hatch_core::time::now();
        MemoryStore::new(&self.db)
            .add(&NewClaim::stated("user", text, "user:chat"), &now)
            .unwrap()
            .id()
            .to_string()
    }

    pub fn run(
        &self,
        router: Router,
        config: AgentConfig,
        input: &str,
    ) -> (hatch_agent::TurnReport, String) {
        let agent = Agent {
            db: &self.db,
            router: &router,
            tools: &self.tools,
            executors: &self.exec,
            gate: &self.gate,
            config,
        };
        let now = hatch_core::time::now();
        let s = SessionStore::new(&self.db).create("t", &now).unwrap();
        (agent.run_turn(&s, input, &now).unwrap(), s)
    }

    pub fn tool_result(&self, session: &str, call_id: &str) -> String {
        SessionStore::new(&self.db)
            .transcript(session)
            .unwrap()
            .into_iter()
            .find(|r| r.tool_call_id.as_deref() == Some(call_id))
            .map(|r| r.content.clone())
            .unwrap_or_default()
    }
}

// --- Sub-agent tests (D5): a scripted model that records requests,
// a throwaway environment, and a live sleeper a test can pause. ---
// Shared across test targets; not every target uses every helper.
#[allow(dead_code)]
pub type Seen = std::sync::Arc<std::sync::Mutex<Vec<hatch_llm::ChatRequest>>>;

#[allow(dead_code)]
pub fn model(
    f: impl Fn(&hatch_llm::ChatRequest) -> hatch_llm::ChatResponse + Send + Sync + 'static,
) -> (hatch_llm::Router, Seen) {
    let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let m = hatch_llm::scripted::FnModel::new("routed", move |r| {
        s2.lock().unwrap().push(r.clone());
        Ok(f(r))
    });
    (hatch_llm::Router::single(std::sync::Arc::new(m)), seen)
}

#[allow(dead_code)]
pub struct Env {
    pub db: std::sync::Arc<Db>,
    pub exec: ExecutorRegistry,
    pub gate: Gatekeeper,
    pub _dir: tempfile::TempDir,
}

#[allow(dead_code)]
pub fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    Env {
        db: std::sync::Arc::new(Db::open_in_memory().unwrap()),
        exec: ExecutorRegistry::new(),
        gate: Gatekeeper::new(PolicyConfig::default()),
        _dir: dir,
    }
}

#[allow(dead_code)]
pub fn async_config(registry: std::sync::Arc<hatch_agent::ChildRegistry>) -> AgentConfig {
    let mut d = hatch_agent::Delegation::root(std::sync::Arc::new(hatch_agent::TypeRegistry::builtin()));
    d.spawn = hatch_agent::SpawnMode::InProcess;
    d.async_mode = true;
    d.registry = Some(registry);
    AgentConfig { delegation: Some(d), ..Default::default() }
}

/// A live process a test can pause and resume. Killed on drop, so a
/// failed assertion never leaks a stopped sleeper.
#[allow(dead_code)]
pub struct LiveSleeper {
    child: std::process::Child,
}

#[allow(dead_code)]
impl LiveSleeper {
    pub fn spawn() -> (Self, u32) {
        // Null stdio: even if the test binary dies hard, a stopped
        // sleeper must not hold the test runner's pipes open.
        let child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("sleep must exist for this test");
        let pid = child.id();
        (LiveSleeper { child }, pid)
    }

    pub fn state(pid: u32) -> String {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after_comm = stat.rfind(')').unwrap();
        stat[after_comm + 2..].split_whitespace().next().unwrap().to_string()
    }
}

#[allow(dead_code)]
impl Drop for LiveSleeper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A state transition after a signal is not instant: the kernel
/// stops/continues the task asynchronously, so poll briefly instead
/// of asserting the state of a single read.
#[allow(dead_code)]
pub fn wait_state(pid: u32, want: &str) -> bool {
    for _ in 0..100 {
        if LiveSleeper::state(pid) == want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[allow(dead_code)]
pub fn wait_not_state(pid: u32, want: &str) -> bool {
    for _ in 0..100 {
        if LiveSleeper::state(pid) != want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}
