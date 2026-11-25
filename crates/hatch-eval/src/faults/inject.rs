//! Fault injection for evaluations: wrappers around real tools and
//! executors that fail chosen calls. Nothing here is used outside
//! `hatch eval`; product code paths are unchanged.

use super::{FaultKind, ToolFault, WriteFault};
use hatch_core::{Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, NewApproval};
use hatch_tools::verification::{EffectContract, EvidenceCheck};
use hatch_tools::{ActionExecutor, ExecContext, PlannedAction, Tool, ToolContext, ToolOutput};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One call a wrapped tool or executor saw.
#[derive(Clone, Debug, Serialize)]
pub struct CallLog {
    pub task: String,
    pub tool: String,
    pub args: String,
    /// The injected error, if this call was failed.
    pub fault: Option<String>,
}

#[derive(Default)]
struct Inner {
    task: String,
    tool_rules: Vec<ToolFault>,
    write_rules: Vec<WriteFault>,
    /// Rule index → matching calls so far (tool rules, then write rules
    /// offset by the tool-rule count).
    counts: BTreeMap<usize, usize>,
    log: Vec<CallLog>,
}

/// Shared between the wrappers and the runner; rules are per task.
#[derive(Default)]
pub struct FaultState {
    inner: Mutex<Inner>,
}

fn error(kind: FaultKind, message: &str) -> Error {
    match kind {
        FaultKind::Unavailable => Error::Connector(message.to_string()),
        FaultKind::Denied => Error::Denied(message.to_string()),
    }
}

impl FaultState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Start a task: its rules apply, counts restart.
    pub fn begin(&self, task: &str, tool_rules: &[ToolFault], write_rules: &[WriteFault]) {
        let mut g = self.lock();
        g.task = task.to_string();
        g.tool_rules = tool_rules.to_vec();
        g.write_rules = write_rules.to_vec();
        g.counts.clear();
    }

    pub fn log(&self) -> Vec<CallLog> {
        self.lock().log.clone()
    }

    /// The error to inject for this tool call, if a rule fires.
    fn tool_fault(&self, tool: &str, args: &str) -> Option<Error> {
        let mut g = self.lock();
        let mut hit = None;
        for (i, r) in g.tool_rules.clone().iter().enumerate() {
            if !(r.tools.is_empty() || r.tools.iter().any(|t| t == tool)) || !args.contains(&r.path) {
                continue;
            }
            let n = g.counts.entry(i).or_insert(0);
            *n += 1;
            if r.first.is_none_or(|first| *n <= first) {
                hit = Some(error(r.kind, &r.message));
                break;
            }
        }
        let task = g.task.clone();
        g.log.push(CallLog {
            task,
            tool: tool.to_string(),
            args: args.to_string(),
            fault: hit.as_ref().map(|e| e.to_string()),
        });
        hit
    }

    /// Paths whose content must not leak through other tools while their
    /// rule keeps failing (a search over the whole workspace, say).
    fn hidden_paths(&self) -> Vec<String> {
        self.lock()
            .tool_rules
            .iter()
            .filter(|r| r.first.is_none())
            .map(|r| r.path.clone())
            .collect()
    }

    /// Should this write report a lost response? (It still lands first.)
    fn write_fault(&self, path: &str) -> Option<String> {
        let mut g = self.lock();
        let offset = g.tool_rules.len();
        let mut hit = None;
        for (i, r) in g.write_rules.clone().iter().enumerate() {
            if r.path != path {
                continue;
            }
            let n = g.counts.entry(offset + i).or_insert(0);
            *n += 1;
            if *n <= r.first {
                hit = Some(r.message.clone());
                break;
            }
        }
        let task = g.task.clone();
        g.log.push(CallLog {
            task,
            tool: "write_file(executor)".into(),
            args: path.to_string(),
            fault: hit.clone(),
        });
        hit
    }
}

/// A real tool whose matching calls fail as the task's rules say.
pub struct FaultyTool {
    pub inner: Arc<dyn Tool>,
    pub state: Arc<FaultState>,
}

impl Tool for FaultyTool {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn class(&self) -> ActionClass {
        self.inner.class()
    }

    fn name(&self) -> String {
        self.inner.name()
    }

    fn plan(&self, args: &Value, ctx: &ToolContext) -> Result<PlannedAction> {
        self.inner.plan(args, ctx)
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let name = self.inner.name();
        if let Some(e) = self.state.tool_fault(&name, &args.to_string()) {
            return Err(e);
        }
        let mut out = self.inner.call(args, ctx)?;
        let hidden = self.state.hidden_paths();
        if !hidden.is_empty() {
            out.content = out
                .content
                .lines()
                .filter(|l| !hidden.iter().any(|p| l.contains(p.as_str())))
                .collect::<Vec<_>>()
                .join("\n");
        }
        Ok(out)
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        self.inner.to_approval(args, ctx)
    }
}

/// A real executor whose matching runs land and then lose their response.
pub struct LostResponseExecutor {
    pub inner: Arc<dyn ActionExecutor>,
    pub state: Arc<FaultState>,
}

impl ActionExecutor for LostResponseExecutor {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let path = payload["path"].as_str().unwrap_or("").to_string();
        let fault = self.state.write_fault(&path);
        let receipt = self.inner.execute(payload, ctx)?;
        match fault {
            Some(msg) => Err(Error::UnknownOutcome(msg)),
            None => Ok(receipt),
        }
    }

    fn contract(&self) -> EffectContract {
        self.inner.contract()
    }

    fn verify(&self, payload: &Value, receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        self.inner.verify(payload, receipt, ctx)
    }

    fn observe(&self, payload: &Value, receipt: &str, ctx: &ExecContext) -> Result<BTreeMap<String, Value>> {
        self.inner.observe(payload, receipt, ctx)
    }
}
