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

pub struct Harness {
    pub db: Db,
    pub tools: hatch_tools::Registry,
    pub exec: ExecutorRegistry,
    pub gate: Gatekeeper,
}

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
