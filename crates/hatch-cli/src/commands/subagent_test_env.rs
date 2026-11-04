//! Test-mode environment for a `hatch __subagent-run` child process.
//!
//! When `HATCH_TEST_SUBAGENT_SCRIPT` points at a JSON file of canned model
//! responses, the child runs with a scripted model and a minimal tool set
//! instead of reading config. Never set in production.

use anyhow::{Context, Result};
use hatch_agent::subagent::ProcessTask;
use hatch_agent::Delegation;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_llm::{ChatModel, ChatRequest, ChatResponse, LlmError, Router, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_sandbox::{Limits as SandboxLimits, Sandbox, SandboxMode};
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{
    CreateEventExecutor, ExecCommandExecutor, ExecutorRegistry, FetchUrlExecutor, Registry,
    SendEmailExecutor, Workspace, WriteFileExecutor,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Test-only: path to a JSON file of canned model responses.
pub(crate) const TEST_SCRIPT_ENV: &str = "HATCH_TEST_SUBAGENT_SCRIPT";

/// A scripted model driven by a JSON file (test-only).
///
/// Entries, consumed in order:
/// `{"text": "…"}` → a text answer; `{"tool_calls": […]}` → tool calls;
/// `{"error": "…"}` → a model error; `{"sleep_secs": N}` → sleep, then
/// the next entry is the response; `{"exit": N}` → exit the process with
/// code N (simulates a child crash).
struct TestScriptModel {
    queue: Mutex<VecDeque<Value>>,
}

impl TestScriptModel {
    fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("读取测试脚本")?;
        let entries: Vec<Value> = serde_json::from_str(&text).context("解析测试脚本")?;
        Ok(Self {
            queue: Mutex::new(entries.into()),
        })
    }
}

impl ChatModel for TestScriptModel {
    fn name(&self) -> String {
        "test-script".into()
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let _ = req;
        loop {
            let entry = self
                .queue
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::permanent("scripted model exhausted"))?;
            if let Some(secs) = entry.get("sleep_secs").and_then(Value::as_u64) {
                std::thread::sleep(Duration::from_secs(secs));
                continue;
            }
            if let Some(code) = entry.get("exit").and_then(Value::as_i64) {
                std::process::exit(code as i32);
            }
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                return Ok(ChatResponse::text(text));
            }
            if let Some(calls) = entry.get("tool_calls").and_then(Value::as_array) {
                let calls: Vec<ToolCall> = calls
                    .iter()
                    .map(|c| serde_json::from_value(c.clone()))
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| LlmError::permanent(format!("bad tool_call in script: {e}")))?;
                return Ok(ChatResponse::calls(calls));
            }
            if let Some(err) = entry.get("error").and_then(Value::as_str) {
                return Err(LlmError::permanent(err.to_string()));
            }
            return Err(LlmError::permanent(format!("bad script entry: {entry}")));
        }
    }
}

/// What a child process runs with: model, tools, executors, gate.
pub(crate) struct ChildEnv {
    pub(crate) router: Router,
    pub(crate) tools: Registry,
    pub(crate) executors: ExecutorRegistry,
    pub(crate) gate: Gatekeeper,
    pub(crate) delegation: Option<Delegation>,
    pub(crate) skill_tools: Option<Vec<String>>,
    /// Tool names the parent allowed but this process cannot offer.
    pub(crate) unavailable: Vec<String>,
}

/// Keep only the parent-allowed tools; report the ones this process
/// cannot offer (missing from the registry).
pub(crate) fn filtered_tools(tools: Registry, allowed: &[String]) -> (Registry, Vec<String>) {
    let keep: Vec<&str> = allowed.iter().map(String::as_str).collect();
    let filtered = tools.filtered(&keep);
    let names = filtered.names();
    let unavailable: Vec<String> = allowed
        .iter()
        .filter(|t| !names.iter().any(|n| n == *t))
        .cloned()
        .collect();
    (filtered, unavailable)
}

/// Build the child environment for test mode (scripted model).
pub(crate) fn test_env(task: &ProcessTask) -> Result<ChildEnv> {
    let script = std::env::var(TEST_SCRIPT_ENV).context("测试脚本环境变量未设置")?;
    let router = Router::single(Arc::new(TestScriptModel::from_file(Path::new(&script))?));
    let ws = match &task.workspace {
        Some(p) => Some(Workspace::new(p)?),
        None => None,
    };
    let general = GeneralTools {
        workspace: ws,
        web_fetch: true,
        search_backend: None,
        max_command_timeout_secs: 60,
    };
    let tools = add_general_tools(default_registry(), &general);
    let outbox = Arc::new(FileOutbox::new(
        &task.scratch_dir.join("outbox"),
        &task.user_email,
    )?);
    let sandbox = Sandbox {
        mode: SandboxMode::None,
        helper: None,
        limits: SandboxLimits::default(),
        max_output: 16 * 1024,
    };
    let executors = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(outbox, &task.user_email)))
        .with(Arc::new(CreateEventExecutor))
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(ExecCommandExecutor {
            sandbox: Arc::new(sandbox),
        }))
        .with(Arc::new(FetchUrlExecutor {
            limits: Default::default(),
        }));
    let (tools, unavailable) = filtered_tools(tools, &task.tools);
    Ok(ChildEnv {
        router,
        tools,
        executors,
        gate: Gatekeeper::new(PolicyConfig::default()),
        delegation: None,
        skill_tools: None,
        unavailable,
    })
}
