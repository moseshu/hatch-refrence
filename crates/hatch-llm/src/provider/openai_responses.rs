//! Responses API provider (`POST {base_url}/responses`).
//!
//! Mapping from the neutral types: system messages → `instructions`; user and
//! assistant text → `input` messages; assistant tool calls → `function_call`
//! items; tool results → `function_call_output` items. Requests set
//! `store: false` so the provider keeps no server-side conversation state.

use super::common::{parse_arguments, sse_json, truncation_error, u64_at};
use super::http::{post_json, post_sse};
use super::responses_stream::ResponsesStreamState;
use crate::stream::{emit_response, StreamSink};
use crate::types::{ChatModel, ChatRequest, ChatResponse, Role, ToolCall, Usage};
use crate::LlmError;
use serde_json::{json, Map, Value};
use std::fmt;
use std::time::Duration;

pub struct OpenAiResponses {
    pub label: String,
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub timeout: Duration,
}

impl fmt::Debug for OpenAiResponses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiResponses")
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenAiResponses {
    pub fn new(label: &str, base_url: &str, model: &str, api_key: String) -> Self {
        OpenAiResponses {
            label: label.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn build_body(&self, req: &ChatRequest) -> Value {
        let mut instructions: Vec<&str> = Vec::new();
        let mut input: Vec<Value> = Vec::new();
        for m in &req.messages {
            match m.role {
                Role::System => instructions.push(&m.content),
                Role::User => input.push(json!({"role": "user", "content": m.content})),
                Role::Assistant => {
                    if !m.content.is_empty() {
                        input.push(json!({"role": "assistant", "content": m.content}));
                    }
                    for c in &m.tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": c.id,
                            "name": c.name,
                            "arguments": c.arguments.to_string()
                        }));
                    }
                }
                Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "output": m.content
                })),
            }
        }
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("input".into(), Value::Array(input));
        body.insert("store".into(), json!(false));
        if !instructions.is_empty() {
            body.insert("instructions".into(), json!(instructions.join("\n\n")));
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    // `strict` defaults to true here, which requires closed schemas;
                    // our tool schemas are ordinary JSON Schema, so opt out.
                    json!({"type": "function", "name": t.name, "description": t.description,
                           "parameters": t.parameters, "strict": false})
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(n) = req.max_tokens {
            body.insert("max_output_tokens".into(), json!(n));
        }
        if let Some(effort) = &req.reasoning_effort {
            body.insert("reasoning".into(), json!({"effort": effort}));
        }
        if req.json_mode {
            body.insert("text".into(), json!({"format": {"type": "json_object"}}));
        }
        Value::Object(body)
    }

    /// Parse a full Responses object (also the payload of `response.completed`).
    pub fn parse_response(v: &Value) -> Result<ChatResponse, LlmError> {
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(LlmError::permanent(format!("response error: {err}")));
        }
        let output = v
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::permanent("response has no output array"))?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("refusal") => text.push_str(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
                            _ => {}
                        }
                    }
                }
                Some("function_call") => tool_calls.push(function_call(item, tool_calls.len())),
                _ => {} // reasoning summaries and other item kinds are not part of the answer
            }
        }
        let usage = parse_usage(v.get("usage"));
        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "incomplete" && text.trim().is_empty() && tool_calls.is_empty() {
            let reason = v.pointer("/incomplete_details/reason").and_then(Value::as_str).unwrap_or("unknown");
            return Err(truncation_error(&format!("status=incomplete, reason={reason}"), usage.completion_tokens, usage.reasoning_tokens));
        }
        if status == "failed" {
            return Err(LlmError::permanent(format!("response failed: {}", v.get("error").cloned().unwrap_or(Value::Null))));
        }
        Ok(ChatResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            usage,
            model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

/// A `function_call` output item.
pub(crate) fn function_call(item: &Value, ordinal: usize) -> ToolCall {
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{ordinal}"));
    ToolCall {
        id,
        name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        arguments: parse_arguments(item.get("arguments")),
    }
}

pub(crate) fn parse_usage(u: Option<&Value>) -> Usage {
    Usage {
        prompt_tokens: u64_at(u, "/input_tokens"),
        completion_tokens: u64_at(u, "/output_tokens"),
        cached_tokens: u64_at(u, "/input_tokens_details/cached_tokens"),
        reasoning_tokens: u64_at(u, "/output_tokens_details/reasoning_tokens"),
    }
}

/// Whether `err` is an HTTP 400 that names `temperature` as an unsupported
/// parameter.
///
/// Reasoning models do not take `temperature` — there is nothing to sample —
/// and some Responses backends reject the parameter outright. Bianxie's
/// gateway did exactly that for `gpt-6-luna` in the 2026-11-04 Colab live
/// eval (`HTTP 400: Unsupported parameter: 'temperature' is not supported
/// with this model`, `rix_api_error`), failing every triage call, while a
/// direct probe of the same request shape returned 200 the same day:
/// gateway validation is inconsistent across time/backends. The caller still
/// sets `temperature` (triage wants 0.0 for determinism), so the provider
/// retries once without it instead of failing a call that would succeed.
fn temperature_rejected(err: &LlmError) -> bool {
    let m = err.message.to_ascii_lowercase();
    m.contains("http 400") && m.contains("temperature")
}

impl ChatModel for OpenAiResponses {
    fn name(&self) -> String {
        format!("{}:{}", self.label, self.model)
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/responses", self.base_url);
        let auth = format!("Bearer {}", self.api_key);
        let mut body = self.build_body(req);
        let v = match post_json(&url, &[("Authorization", &auth)], &body, self.timeout) {
            Ok(v) => v,
            Err(e) if temperature_rejected(&e) && body.get("temperature").is_some() => {
                // The backend refuses `temperature`; retry once without it.
                if let Some(o) = body.as_object_mut() {
                    o.remove("temperature");
                }
                post_json(&url, &[("Authorization", &auth)], &body, self.timeout)?
            }
            Err(e) => return Err(e),
        };
        let mut r = Self::parse_response(&v)?;
        if r.model.is_empty() {
            r.model = self.model.clone();
        }
        Ok(r)
    }

    fn chat_stream(
        &self,
        req: &ChatRequest,
        sink: &mut StreamSink<'_>,
    ) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/responses", self.base_url);
        let auth = format!("Bearer {}", self.api_key);
        let mut body = self.build_body(req);
        body["stream"] = json!(true);
        // Retry once without `temperature` if the backend rejects it (same
        // rule as `chat`), but only before any event was emitted, so a retry
        // can never duplicate output already sent to the sink.
        let mut retried = false;
        loop {
            let mut state = ResponsesStreamState::default();
            let mut buffered: Option<ChatResponse> = None;
            let mut saw_event = false;
            let res = post_sse(&url, &[("Authorization", &auth)], &body, &mut |ev| {
                saw_event = true;
                if ev.event.as_deref() == Some("__json__") {
                    let r = Self::parse_response(&sse_json(&ev.data)?)?;
                    emit_response(&r, sink);
                    buffered = Some(r);
                    return Ok(false);
                }
                if ev.data.trim() == "[DONE]" {
                    return Ok(false);
                }
                state.on_event(&sse_json(&ev.data)?, sink)
            });
            match res {
                Ok(()) => {
                    let mut r = match buffered {
                        Some(r) => r,
                        None => state.finish(sink)?,
                    };
                    if r.model.is_empty() {
                        r.model = self.model.clone();
                    }
                    return Ok(r);
                }
                Err(e)
                    if !retried
                        && !saw_event
                        && temperature_rejected(&e)
                        && body.get("temperature").is_some() =>
                {
                    if let Some(o) = body.as_object_mut() {
                        o.remove("temperature");
                    }
                    retried = true;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ToolSpec};

    #[test]
    fn body_maps_instructions_items_and_options() {
        let p = OpenAiResponses::new("r", "https://api.example.com/v1/", "m", "secret".into());
        assert!(!format!("{p:?}").contains("secret"));
        let mut req = ChatRequest::new(vec![
            ChatMessage::system("规则"),
            ChatMessage::user("查一下"),
            ChatMessage::assistant_tools("", vec![ToolCall { id: "c1".into(), name: "t".into(), arguments: json!({"q": 1}) }]),
            ChatMessage::tool("c1", "结果"),
        ]);
        req.tools.push(ToolSpec { name: "t".into(), description: "d".into(), parameters: json!({"type": "object"}) });
        req.max_tokens = Some(200);
        req.reasoning_effort = Some("low".into());
        req.json_mode = true;
        let b = p.build_body(&req);
        assert_eq!(b["instructions"], "规则");
        assert_eq!(b["input"][0], json!({"role": "user", "content": "查一下"}));
        assert_eq!(b["input"][1]["type"], "function_call");
        assert_eq!(b["input"][1]["arguments"], "{\"q\":1}");
        assert_eq!(b["input"][2], json!({"type": "function_call_output", "call_id": "c1", "output": "结果"}));
        assert_eq!(b["tools"][0]["strict"], false);
        assert_eq!(b["max_output_tokens"], 200);
        assert_eq!(b["reasoning"]["effort"], "low");
        assert_eq!(b["text"]["format"]["type"], "json_object");
        assert_eq!(b["store"], false);
    }

    #[test]
    fn parses_text_calls_usage_and_skips_reasoning() {
        let v = json!({"model": "m", "status": "completed", "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "好的"}]},
            {"type": "function_call", "call_id": "call_9", "name": "calendar_list", "arguments": "{\"days\":2}"}],
            "usage": {"input_tokens": 50, "output_tokens": 20,
                      "input_tokens_details": {"cached_tokens": 32}, "output_tokens_details": {"reasoning_tokens": 12}}});
        let r = OpenAiResponses::parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("好的"));
        assert_eq!(r.tool_calls[0].id, "call_9");
        assert_eq!(r.tool_calls[0].arguments["days"], 2);
        assert_eq!(r.usage, Usage { prompt_tokens: 50, completion_tokens: 20, cached_tokens: 32, reasoning_tokens: 12 });
    }

    #[test]
    fn incomplete_without_answer_is_truncation() {
        let v = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
                       "output": [{"type": "reasoning", "summary": []}],
                       "usage": {"output_tokens": 64, "output_tokens_details": {"reasoning_tokens": 64}}});
        let e = OpenAiResponses::parse_response(&v).unwrap_err();
        assert!(e.message.contains("max_output_tokens") && e.message.contains("reasoning_tokens=64"), "{}", e.message);
    }

    #[test]
    fn temperature_rejected_matches_only_400_naming_temperature() {
        let temp_400 = LlmError::permanent(
            "HTTP 400: {\"error\":{\"message\":\"Unsupported parameter: 'temperature' is not supported with this model.\",\"type\":\"rix_api_error\",\"param\":\"temperature\",\"code\":\"invalid_request_error\"}}",
        );
        assert!(temperature_rejected(&temp_400));
        assert!(!temperature_rejected(&LlmError::permanent(
            "HTTP 400: {\"error\":{\"message\":\"bad input\"}}"
        )));
        assert!(!temperature_rejected(&LlmError::permanent(
            "HTTP 500: temperature spike"
        )));
        assert!(!temperature_rejected(&LlmError::transient(
            "transport: connection reset"
        )));
    }
}
