//! Evaluation-only capture of provider exchanges and independent report evidence.

use hatch_core::{ids, secrets, Result};
use hatch_llm::{
    ChatModel, ChatRequest, ChatResponse, ContentPart, LlmError, Role, Router, ToolCall, Usage,
    Workload,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

thread_local! {
    static ACTIVE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

#[derive(Clone, Default)]
pub struct Capture {
    state: Arc<Mutex<Trace>>,
}

#[derive(Default)]
struct Trace {
    contexts: BTreeMap<String, String>,
    exchanges: Vec<Exchange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Exchange {
    pub workload: String,
    pub model: String,
    pub contexts: Vec<String>,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub settings: Value,
    pub response: Option<Response>,
    pub error: Option<String>,
    pub redacted_response: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

struct ActiveGuard(Option<Capture>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|slot| slot.replace(self.0.take()));
    }
}

pub fn current() -> Option<Capture> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

impl Capture {
    pub fn scope<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = ActiveGuard(ACTIVE.with(|slot| slot.replace(Some(self.clone()))));
        run()
    }

    pub fn attach(&self, router: &Router) {
        let capture = self.clone();
        router.observe_exchanges(Arc::new(move |workload, request, model, response| {
            capture.record(workload, request, model, response);
        }));
    }

    fn record(
        &self,
        workload: Workload,
        request: &ChatRequest,
        model: &str,
        response: &std::result::Result<ChatResponse, LlmError>,
    ) {
        let mut trace = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut contexts = Vec::new();
        let messages = request.messages.iter().map(|message| {
            if message.role == Role::System {
                let text = secrets::redact(&message.text_content());
                let digest = ids::digest(&[&text]);
                contexts.push(digest.clone());
                trace.contexts.entry(digest.clone()).or_insert(text);
                return json!({"role": message.role, "context": digest});
            }
            let images: Vec<_> = message.content.iter().filter_map(|part| match part {
                ContentPart::Image { data, media_type } => Some(json!({
                    "media_type": media_type, "sha256": ids::digest(&[data])
                })),
                _ => None,
            }).collect();
            json!({
                "role": message.role, "text": secrets::redact(&message.text_content()),
                "tool_call_id": message.tool_call_id, "tool_calls": safe_calls(&message.tool_calls),
                "images": images,
            })
        }).collect();
        let mut redacted_response = false;
        let (response, error) = match response {
            Ok(value) => {
                let content = value.content.as_deref().map(secrets::redact);
                let tool_calls = safe_calls(&value.tool_calls);
                redacted_response = content != value.content || tool_calls != value.tool_calls;
                (
                    Some(Response {
                        content,
                        tool_calls,
                        usage: value.usage,
                    }),
                    None,
                )
            }
            Err(err) => (None, Some(secrets::redact(&err.message))),
        };
        trace.exchanges.push(Exchange {
            workload: workload.as_str().into(),
            model: secrets::redact(model),
            contexts,
            messages,
            tools: request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": secrets::redact(&tool.name),
                        "description": secrets::redact(&tool.description),
                        "parameters": redact_value(tool.parameters.clone()),
                    })
                })
                .collect(),
            settings: json!({
                "max_tokens": request.max_tokens,
                "reasoning_effort": request.reasoning_effort,
                "temperature": request.temperature,
                "json_mode": request.json_mode,
                "cache_ttl_secs": request.cache_ttl_secs,
            }),
            response,
            error,
            redacted_response,
        });
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        let trace = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let contexts = dir.join("model-contexts");
        std::fs::create_dir_all(&contexts)?;
        for (digest, content) in &trace.contexts {
            std::fs::write(contexts.join(format!("{digest}.txt")), content)?;
        }
        std::fs::write(
            dir.join("model-trace.json"),
            serde_json::to_vec_pretty(&trace.exchanges)?,
        )?;
        Ok(())
    }

    pub fn summarize(
        &self,
        max_calls: Option<u64>,
        estimated: bool,
    ) -> (crate::report::Usage, crate::scorecard::Activity) {
        let trace = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut totals = crate::report::Usage {
            calls: trace.exchanges.len() as u64,
            max_calls,
            estimated,
            ..Default::default()
        };
        let mut activity = crate::scorecard::Activity::default();
        for exchange in &trace.exchanges {
            if let Some(response) = &exchange.response {
                totals.prompt_tokens += response.usage.prompt_tokens;
                totals.completion_tokens += response.usage.completion_tokens;
                totals.cached_tokens += response.usage.cached_tokens;
                totals.reasoning_tokens += response.usage.reasoning_tokens;
                activity.tool_steps += usize::from(!response.tool_calls.is_empty());
                for call in &response.tool_calls {
                    match call.name.as_str() {
                        "web_search" | "memory_search" => activity.searches += 1,
                        "ask_user" => activity.questions += 1,
                        _ => {}
                    }
                }
            }
        }
        (totals, activity)
    }
}

pub fn attach(router: Router) -> Router {
    if let Some(capture) = current() {
        capture.attach(&router);
    }
    router
}

pub fn single(model: Arc<dyn ChatModel>) -> Router {
    attach(Router::single(model))
}

pub fn factory(factory: hatch_workers::RouterFactory) -> hatch_workers::RouterFactory {
    let Some(capture) = current() else {
        return factory;
    };
    Arc::new(move || {
        let router = factory()?;
        capture.attach(&router);
        Ok(router)
    })
}

fn safe_calls(calls: &[ToolCall]) -> Vec<ToolCall> {
    calls
        .iter()
        .map(|call| ToolCall {
            id: secrets::redact(&call.id),
            name: secrets::redact(&call.name),
            arguments: redact_value(call.arguments.clone()),
        })
        .collect()
}

pub(crate) fn redact_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(secrets::redact(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(redact_value).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(key, val)| (secrets::redact(&key), redact_value(val)))
                .collect(),
        ),
        Value::Number(n) if secrets::contains_secret(&n.to_string()) => {
            Value::String(secrets::redact(&n.to_string()))
        }
        other => other,
    }
}
