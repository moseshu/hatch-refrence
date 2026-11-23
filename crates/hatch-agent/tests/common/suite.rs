//! Task-suite harness (spec: task-suite-spec-2026-11-23.md):
//! an isolated world per task — in-memory DB, temp workspace,
//! fixture outbox, real executors — plus resource snapshots so
//! acceptance reads state, never the agent's own report.
//!
//! Included by the task_suite* test targets via #[path].

// Each including target uses a subset of the helpers.
#![allow(dead_code)]

use hatch_agent::{Agent, AgentConfig, SessionStore, TurnReport};
use hatch_connectors::calendar::CalendarRepo;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_llm::Router;
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::workspace::Workspace;
use hatch_tools::{
    CreateEventExecutor, ExecutorRegistry, Registry, SendEmailExecutor, WriteFileExecutor,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct Suite {
    pub db: Db,
    pub dir: tempfile::TempDir,
    pub ws_root: std::path::PathBuf,
    pub outbox: Arc<FileOutbox>,
    pub exec: ExecutorRegistry,
    pub gate: Gatekeeper,
    pub tools: Registry,
}

impl Suite {
    pub fn new() -> Self {
        Self::with_policy(PolicyConfig::default())
    }

    pub fn with_policy(policy: PolicyConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Arc::new(FileOutbox::new(&dir.path().join("outbox"), "me@corp.example").unwrap());
        Self::custom(dir, policy, outbox.clone(), outbox)
    }

    /// Custom mail sender (recovery tests inject timeouts) while
    /// keeping the FileOutbox handle for side-effect counting.
    pub fn custom(
        dir: tempfile::TempDir,
        policy: PolicyConfig,
        sender: Arc<dyn hatch_connectors::MailSender>,
        outbox: Arc<FileOutbox>,
    ) -> Self {
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        let exec = ExecutorRegistry::new()
            .with(Arc::new(SendEmailExecutor::new(sender, "me@corp.example")))
            .with(Arc::new(CreateEventExecutor))
            .with(Arc::new(WriteFileExecutor));
        let tools = add_general_tools(
            default_registry(),
            &GeneralTools {
                workspace: Some(Workspace::new(&ws_root).unwrap()),
                web_fetch: false,
                search_backend: None,
                max_command_timeout_secs: 30,
            },
        );
        Suite {
            db: Db::open_in_memory().unwrap(),
            dir,
            ws_root,
            outbox,
            exec,
            gate: Gatekeeper::new(policy),
            tools,
        }
    }

    pub fn seed_grant(&self, class: hatch_policy::ActionClass, recipients: &[&str]) {
        hatch_policy::GrantStore::new(&self.db)
            .add(
                &hatch_policy::NewGrant {
                    class,
                    recipients: recipients.iter().map(|s| s.to_string()).collect(),
                    max_uses: None,
                    allow_when_tainted: false,
                    expires_at: None,
                    note: String::new(),
                },
                &hatch_core::time::now(),
            )
            .unwrap();
    }

    pub fn seed_person(&self, email: &str, name: &str) {
        hatch_memory::People::new(&self.db)
            .upsert(email, name, hatch_memory::Importance::Normal, "")
            .unwrap();
    }

    pub fn write_fixture(&self, rel: &str, content: &str) {
        let p = self.ws_root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    pub fn run(&self, router: &Router, config: AgentConfig, input: &str) -> (TurnReport, String) {
        let agent = Agent {
            db: &self.db,
            router,
            tools: &self.tools,
            executors: &self.exec,
            gate: &self.gate,
            config,
        };
        let now = hatch_core::time::now();
        let s = SessionStore::new(&self.db).create("suite", &now).unwrap();
        (agent.run_turn(&s, input, &now).unwrap(), s)
    }

    pub fn run_in(
        &self,
        router: &Router,
        config: AgentConfig,
        session: &str,
        input: &str,
    ) -> TurnReport {
        let agent = Agent {
            db: &self.db,
            router,
            tools: &self.tools,
            executors: &self.exec,
            gate: &self.gate,
            config,
        };
        let now = hatch_core::time::now();
        agent.run_turn(session, input, &now).unwrap()
    }

    // --- resource snapshots (acceptance reads these, not replies) ---

    pub fn events(&self) -> Vec<hatch_connectors::calendar::Event> {
        CalendarRepo::new(&self.db).all().unwrap()
    }

    pub fn sent_count(&self) -> usize {
        self.outbox.entries().unwrap().len()
    }

    pub fn files(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut BTreeMap<String, String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                    out.insert(rel, std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        walk(&self.ws_root, &self.ws_root, &mut out);
        out
    }

    pub fn approvals_with_status(&self, status: &str) -> i64 {
        self.db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM approvals WHERE status = ?1",
                [status],
                |r| r.get(0),
            )
            .unwrap()
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
