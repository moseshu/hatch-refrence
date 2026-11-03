//! Tool SDK.

use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, DataLabel, NewApproval};
use hatch_store::Db;
use serde_json::Value;
use std::sync::Arc;

/// Everything a tool may touch.
pub struct ToolContext<'a> {
    pub db: &'a Db,
    pub now: Timestamp,
    pub tz: Tz,
    /// The user's own address (sender of outgoing mail).
    pub user_email: String,
    /// `chat:<session>` or `triage:<run>` — recorded on approvals and claims.
    pub origin: String,
    /// The current context contains untrusted content.
    pub tainted: bool,
    /// Id of the model's tool call (distinguishes repeated identical calls).
    pub call_id: String,
    /// Fires on Ctrl-C or when the parent turn / agent is cancelled.
    pub cancel: CancelToken,
}

/// What a tool returns to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub trust: Trust,
    /// Optional widget payload (card JSON, see `hatch-artifacts`). The
    /// text `content` is always the full fallback; the widget is a
    /// structured side-channel for clients with a UI, rendered only when
    /// its embed token is in the message.
    pub widget: Option<Value>,
}

impl ToolOutput {
    pub fn system(c: impl Into<String>) -> Self {
        ToolOutput { content: c.into(), trust: Trust::System, widget: None }
    }
    pub fn untrusted(c: impl Into<String>) -> Self {
        ToolOutput { content: c.into(), trust: Trust::Untrusted, widget: None }
    }
    pub fn with_widget(mut self, widget: Value) -> Self {
        self.widget = Some(widget);
        self
    }
}

/// What the gate needs to know about a call before it runs.
#[derive(Clone, Debug, Default)]
pub struct PlannedAction {
    /// Class of this particular call when it differs from [`Tool::class`]
    /// (a browser click can be a navigation, a submission or a payment).
    pub class: Option<ActionClass>,
    pub summary: String,
    pub recipients: Vec<String>,
    pub labels: Vec<DataLabel>,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn class(&self) -> ActionClass;

    fn name(&self) -> String {
        self.spec().name
    }

    /// Describe the effect for the gatekeeper.
    fn plan(&self, _args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction { summary: self.name(), ..Default::default() })
    }

    /// Run immediately. Only called after the gate allowed it.
    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput>;

    /// Deferred form: an approval whose stored payload an executor will run.
    /// Tools with external effects implement this instead of `call`.
    fn to_approval(&self, _args: &Value, _ctx: &ToolContext) -> Result<Option<NewApproval>> {
        Ok(None)
    }
}

/// Ordered set of tools offered to the model.
#[derive(Default, Clone)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, t: Arc<dyn Tool>) -> Self {
        self.register(t);
        self
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        self.tools.retain(|x| x.name() != t.name());
        self.tools.push(t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// A registry with only the named tools (order preserved). Used to give a
    /// sub-agent a narrowed tool set; it can never add tools.
    pub fn filtered(&self, names: &[&str]) -> Registry {
        Registry { tools: self.tools.iter().filter(|t| names.contains(&t.name().as_str())).cloned().collect() }
    }
}

/// Typed argument helpers.
pub fn arg_str<'v>(args: &'v Value, key: &str) -> Result<&'v str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Invalid(format!("missing string argument `{key}`")))
}

pub fn arg_opt_str<'v>(args: &'v Value, key: &str) -> Option<&'v str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

pub fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

pub fn arg_str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) => s.split([',', ';', '，']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    }
}
