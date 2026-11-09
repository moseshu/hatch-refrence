//! Gemini provider (`POST {base}/v1beta/models/{model}:generateContent`).
//!
//! Mapping: system messages → `systemInstruction`; assistant → role `model`;
//! tool calls → `functionCall` parts; tool results → `functionResponse` parts
//! (the function name is looked up from the earlier call with the same id).
//!
//! Two choices here are this project's own, not taken from any reference:
//! * `reasoning_effort` → `thinkingConfig.thinkingBudget`: low 1024,
//!   medium 8192, high 24576 tokens.
//! * Gemini call ids are optional and calls may carry a `thoughtSignature`
//!   that must be echoed back on the next turn. Both are packed into the
//!   neutral `ToolCall::id` as `<id>~sig~<signature>` and unpacked when the
//!   request is rebuilt, so no neutral type has to change.

use super::common::{sse_json, truncation_error, u64_at};
use super::gemini_stream::GeminiStreamState;
use super::http::{post_json, post_sse};
use crate::stream::{emit_response, StreamSink};
use crate::types::{ChatModel, ChatRequest, ChatResponse, Role, ToolCall, Usage};
use crate::LlmError;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const SIG: &str = "~sig~";
const SYNTH_PREFIX: &str = "gemini_call_";

pub struct Gemini {
    pub label: String,
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub timeout: Duration,
    /// Context window in tokens, from static configuration (§3.4).
    /// `None` = the model does not declare one.
    pub context_window: Option<usize>,
}

impl fmt::Debug for Gemini {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gemini")
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// JSON Schema keywords the Gemini function-declaration schema rejects.
const UNSUPPORTED: &[&str] = &["additionalProperties", "$schema", "$id", "$ref", "$defs", "definitions", "examples", "const", "default"];

/// Remove unsupported keywords recursively (property *names* are kept).
pub fn clean_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                if UNSUPPORTED.contains(&k.as_str()) {
                    continue;
                }
                let cleaned = if k == "properties" {
                    match val {
                        Value::Object(props) => Value::Object(props.iter().map(|(n, s)| (n.clone(), clean_schema(s))).collect()),
                        other => other.clone(),
                    }
                } else {
                    clean_schema(val)
                };
                out.insert(k.clone(), cleaned);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once(SIG) {
        Some((i, s)) => (i, Some(s)),
        None => (id, None),
    }
}

fn thinking_budget(effort: &str) -> Option<u32> {
    match effort {
        "low" | "minimal" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        _ => None,
    }
}

impl Gemini {
    pub fn new(label: &str, base_url: &str, model: &str, api_key: String) -> Self {
        let base = if base_url.trim().is_empty() { DEFAULT_BASE_URL } else { base_url };
        Gemini {
            label: label.to_string(),
            base_url: base.trim_end_matches('/').to_string(),
            model: model.trim_start_matches("models/").to_string(),
            api_key,
            timeout: Duration::from_secs(120),
            context_window: None,
        }
    }

    /// Declare the model's context window (static configuration, §3.4).
    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
        self
    }

    fn url(&self, method: &str) -> String {
        let root = if self.base_url.ends_with("/v1beta") || self.base_url.ends_with("/v1") {
            self.base_url.clone()
        } else {
            format!("{}/v1beta", self.base_url)
        };
        format!("{root}/models/{}:{method}", self.model)
    }

    pub fn build_body(&self, req: &ChatRequest) -> Value {
        let mut system: Vec<&str> = Vec::new();
        let mut contents: Vec<(String, Vec<Value>)> = Vec::new();
        let mut names: HashMap<&str, &str> = HashMap::new();
        let mut push = |role: &str, part: Value| match contents.last_mut() {
            Some((r, parts)) if r == role => parts.push(part),
            _ => contents.push((role.to_string(), vec![part])),
        };
        for m in &req.messages {
            match m.role {
                Role::System => system.push(&m.content),
                Role::User => push("user", json!({"text": m.content})),
                Role::Assistant => {
                    if !m.content.is_empty() {
                        push("model", json!({"text": m.content}));
                    }
                    for c in &m.tool_calls {
                        names.insert(c.id.as_str(), c.name.as_str());
                        let (id, sig) = split_id(&c.id);
                        let mut call = json!({"name": c.name, "args": c.arguments});
                        if !id.starts_with(SYNTH_PREFIX) {
                            call["id"] = json!(id);
                        }
                        let mut part = json!({"functionCall": call});
                        if let Some(sig) = sig {
                            part["thoughtSignature"] = json!(sig);
                        }
                        push("model", part);
                    }
                }
                Role::Tool => {
                    let full_id = m.tool_call_id.clone().unwrap_or_default();
                    let name = names.get(full_id.as_str()).copied().unwrap_or("tool");
                    let (id, _) = split_id(&full_id);
                    let mut resp = json!({"name": name, "response": {"content": m.content}});
                    if !id.is_empty() && !id.starts_with(SYNTH_PREFIX) {
                        resp["id"] = json!(id);
                    }
                    push("user", json!({"functionResponse": resp}));
                }
            }
        }
        let mut body = Map::new();
        body.insert(
            "contents".into(),
            Value::Array(contents.into_iter().map(|(r, p)| json!({"role": r, "parts": p})).collect()),
        );
        if !system.is_empty() {
            body.insert("systemInstruction".into(), json!({"parts": [{"text": system.join("\n\n")}]}));
        }
        if !req.tools.is_empty() {
            let decls: Vec<Value> = req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "parameters": clean_schema(&t.parameters)}))
                .collect();
            body.insert("tools".into(), json!([{"functionDeclarations": decls}]));
        }
        let mut gen = Map::new();
        if let Some(n) = req.max_tokens {
            gen.insert("maxOutputTokens".into(), json!(n));
        }
        if let Some(t) = req.temperature {
            gen.insert("temperature".into(), json!(t));
        }
        if req.json_mode {
            gen.insert("responseMimeType".into(), json!("application/json"));
        }
        if let Some(b) = req.reasoning_effort.as_deref().and_then(thinking_budget) {
            gen.insert("thinkingConfig".into(), json!({"thinkingBudget": b}));
        }
        if !gen.is_empty() {
            body.insert("generationConfig".into(), Value::Object(gen));
        }
        Value::Object(body)
    }

    pub fn parse_response(v: &Value) -> Result<ChatResponse, LlmError> {
        if let Some(reason) = v.pointer("/promptFeedback/blockReason").and_then(Value::as_str) {
            return Err(LlmError::permanent(format!("prompt blocked by provider safety filter: {reason}")));
        }
        let cand = v
            .pointer("/candidates/0")
            .ok_or_else(|| LlmError::permanent("response has no candidates"))?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for part in cand.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                let base = fc
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{SYNTH_PREFIX}{}", tool_calls.len()));
                let id = match part.get("thoughtSignature").and_then(Value::as_str) {
                    Some(sig) => format!("{base}{SIG}{sig}"),
                    None => base,
                };
                tool_calls.push(ToolCall {
                    id,
                    name: fc.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                });
            }
        }
        let u = v.get("usageMetadata");
        let thoughts = u64_at(u, "/thoughtsTokenCount");
        let usage = Usage {
            prompt_tokens: u64_at(u, "/promptTokenCount"),
            completion_tokens: u64_at(u, "/candidatesTokenCount") + thoughts,
            cached_tokens: u64_at(u, "/cachedContentTokenCount"),
            reasoning_tokens: thoughts,
        };
        let finish = cand.get("finishReason").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() && tool_calls.is_empty() {
            match finish {
                "MAX_TOKENS" => return Err(truncation_error("finishReason=MAX_TOKENS", usage.completion_tokens, usage.reasoning_tokens)),
                "" | "STOP" | "FINISH_REASON_UNSPECIFIED" => {}
                other => return Err(LlmError::permanent(format!("no answer: finishReason={other}"))),
            }
        }
        Ok(ChatResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            usage,
            model: v.get("modelVersion").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

impl ChatModel for Gemini {
    fn name(&self) -> String {
        format!("{}:{}", self.label, self.model)
    }

    fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let headers = [("x-goog-api-key", self.api_key.as_str())];
        let v = post_json(&self.url("generateContent"), &headers, &self.build_body(req), self.timeout)?;
        let mut r = Self::parse_response(&v)?;
        if r.model.is_empty() {
            r.model = self.model.clone();
        }
        Ok(r)
    }

    fn chat_stream(&self, req: &ChatRequest, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let headers = [("x-goog-api-key", self.api_key.as_str())];
        let url = format!("{}?alt=sse", self.url("streamGenerateContent"));
        let mut state = GeminiStreamState::default();
        let mut buffered: Option<ChatResponse> = None;
        post_sse(&url, &headers, &self.build_body(req), &mut |ev| {
            let v = sse_json(&ev.data)?;
            if ev.event.as_deref() == Some("__json__") {
                // A JSON array of chunks (non-SSE streaming) or a single response.
                let chunks = match v {
                    Value::Array(a) => a,
                    single => vec![single],
                };
                for c in &chunks {
                    state.on_chunk(c, &mut |_| {})?;
                }
                let r = state.assembled()?;
                emit_response(&r, sink);
                buffered = Some(r);
                return Ok(false);
            }
            state.on_chunk(&v, sink)?;
            Ok(true)
        })?;
        let mut r = match buffered {
            Some(r) => r,
            None => state.finish(sink)?,
        };
        if r.model.is_empty() {
            r.model = self.model.clone();
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ToolSpec};

    #[test]
    fn body_maps_roles_tools_and_config() {
        let g = Gemini::new("g", "", "models/gemini-x", "secret".into());
        assert!(!format!("{g:?}").contains("secret"));
        assert_eq!(g.url("generateContent"), "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent");
        let mut req = ChatRequest::new(vec![
            ChatMessage::system("规则"),
            ChatMessage::user("今天有什么会"),
            ChatMessage::assistant_tools("", vec![
                ToolCall { id: "gemini_call_0~sig~SIGDATA".into(), name: "calendar_list".into(), arguments: json!({"days": 1}) },
            ]),
            ChatMessage::tool("gemini_call_0~sig~SIGDATA", "无会议"),
        ]);
        req.tools.push(ToolSpec {
            name: "calendar_list".into(),
            description: "d".into(),
            parameters: json!({"type": "object", "additionalProperties": false, "$schema": "x",
                "properties": {"default": {"type": "string", "default": "a"}, "days": {"type": "integer"}}}),
        });
        req.max_tokens = Some(200);
        req.json_mode = true;
        req.reasoning_effort = Some("low".into());
        let b = g.build_body(&req);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "规则");
        assert_eq!(b["contents"][1]["role"], "model");
        assert_eq!(b["contents"][1]["parts"][0]["thoughtSignature"], "SIGDATA");
        assert!(b["contents"][1]["parts"][0]["functionCall"].get("id").is_none(), "synthesized ids are not sent");
        assert_eq!(b["contents"][2]["parts"][0]["functionResponse"]["name"], "calendar_list");
        let params = &b["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("additionalProperties").is_none() && params.get("$schema").is_none());
        assert!(params["properties"].get("default").is_some(), "property names survive");
        assert!(params["properties"]["default"].get("default").is_none());
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 200);
        assert_eq!(b["generationConfig"]["responseMimeType"], "application/json");
        assert_eq!(b["generationConfig"]["thinkingConfig"]["thinkingBudget"], 1024);
    }

    #[test]
    fn parses_text_calls_signature_and_usage() {
        let v = json!({"modelVersion": "gemini-x", "candidates": [{"finishReason": "STOP", "content": {"role": "model", "parts": [
            {"text": "思考中", "thought": true},
            {"text": "好的"},
            {"functionCall": {"name": "memory_search", "args": {"query": "老板"}}, "thoughtSignature": "S1"}]}}],
            "usageMetadata": {"promptTokenCount": 40, "candidatesTokenCount": 6, "thoughtsTokenCount": 10, "cachedContentTokenCount": 8}});
        let r = Gemini::parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("好的"));
        assert_eq!(r.tool_calls[0].id, "gemini_call_0~sig~S1");
        assert_eq!(r.tool_calls[0].arguments["query"], "老板");
        assert_eq!(r.usage, Usage { prompt_tokens: 40, completion_tokens: 16, cached_tokens: 8, reasoning_tokens: 10 });
    }

    #[test]
    fn empty_answers_explain_why() {
        let max = json!({"candidates": [{"finishReason": "MAX_TOKENS", "content": {"parts": []}}],
                         "usageMetadata": {"thoughtsTokenCount": 64}});
        assert!(Gemini::parse_response(&max).unwrap_err().message.contains("MAX_TOKENS"));
        let safety = json!({"candidates": [{"finishReason": "SAFETY"}]});
        assert!(Gemini::parse_response(&safety).unwrap_err().message.contains("SAFETY"));
        let blocked = json!({"promptFeedback": {"blockReason": "OTHER"}});
        assert!(Gemini::parse_response(&blocked).unwrap_err().message.contains("blocked"));
    }
}
