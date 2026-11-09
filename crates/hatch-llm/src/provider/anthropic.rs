//! Messages API provider (`POST {base_url}/v1/messages`).

use super::anthropic_stream::AnthropicStreamState;
use super::common::sse_json;
use super::http::{post_json, post_sse};
use crate::stream::{emit_response, StreamSink};
use crate::types::{ChatModel, ChatRequest, ChatResponse, Role, ToolCall, Usage};
use crate::LlmError;
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 1024;

pub struct AnthropicMessages {
    pub label: String,
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub timeout: Duration,
    /// Context window in tokens, from static configuration (§3.4).
    /// `None` = the model does not declare one.
    pub context_window: Option<usize>,
}

impl fmt::Debug for AnthropicMessages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessages")
            .field("label", &self.label)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Anthropic prompt-cache TTL label. Only `5m` (300s) and `1h` (3600s)
/// are accepted by the API; anything else is clamped to the nearer of
/// the two (config validation already rejects other values, this is a
/// last-resort guard for direct API use).
pub fn cache_ttl_label(secs: u64) -> &'static str {
    if secs >= 3600 {
        "1h"
    } else {
        "5m"
    }
}

fn with_cache_control(block: &mut Value, secs: u64) {
    block["cache_control"] = json!({"type": "ephemeral", "ttl": cache_ttl_label(secs)});
}

impl AnthropicMessages {
    pub fn new(label: &str, base_url: &str, model: &str, api_key: String) -> Self {
        AnthropicMessages {
            label: label.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
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

    pub fn build_body(&self, req: &ChatRequest) -> Value {
        let mut system: Vec<String> = Vec::new();
        let mut msgs: Vec<(String, Vec<Value>)> = Vec::new();
        let mut push = |role: &str, block: Value| match msgs.last_mut() {
            Some((r, blocks)) if r == role => blocks.push(block),
            _ => msgs.push((role.to_string(), vec![block])),
        };
        for m in &req.messages {
            match m.role {
                Role::System => system.push(m.content.clone()),
                Role::User => push("user", json!({"type": "text", "text": m.content})),
                Role::Assistant => {
                    if !m.content.is_empty() {
                        push("assistant", json!({"type": "text", "text": m.content}));
                    }
                    for c in &m.tool_calls {
                        push("assistant", json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.arguments}));
                    }
                }
                Role::Tool => push(
                    "user",
                    json!({"type": "tool_result", "tool_use_id": m.tool_call_id.clone().unwrap_or_default(), "content": m.content}),
                ),
            }
        }
        if req.json_mode {
            system.push("Respond with a single JSON object and nothing else.".into());
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "messages": msgs.into_iter().map(|(r, c)| json!({"role": r, "content": c})).collect::<Vec<_>>(),
        });
        if !system.is_empty() {
            // With a cache TTL the system prompt is sent as blocks so the
            // last block can carry `cache_control` (ephemeral 5m/1h);
            // without one it stays the plain joined string as before.
            match req.cache_ttl_secs {
                Some(ttl) => {
                    let mut blocks: Vec<Value> = system
                        .iter()
                        .map(|t| json!({"type": "text", "text": t}))
                        .collect();
                    if let Some(last) = blocks.last_mut() {
                        with_cache_control(last, ttl);
                    }
                    body["system"] = json!(blocks);
                }
                None => body["system"] = json!(system.join("\n\n")),
            }
        } else if let Some(ttl) = req.cache_ttl_secs {
            // No system prompt: cache the last user text block instead.
            if let Some(msgs) = body["messages"].as_array_mut() {
                if let Some(m) = msgs.iter_mut().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("user")) {
                    if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
                        if let Some(last) = blocks.iter_mut().rev().find(|b| b.get("type").and_then(Value::as_str) == Some("text")) {
                            with_cache_control(last, ttl);
                        }
                    }
                }
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
                .collect::<Vec<_>>());
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        body
    }

    pub fn parse_response(v: &Value) -> Result<ChatResponse, LlmError> {
        let blocks = v
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::permanent("response has no content array"))?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                    name: b.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    arguments: b.get("input").cloned().unwrap_or(json!({})),
                }),
                _ => {}
            }
        }
        let n = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
        Ok(ChatResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            usage: Usage {
                prompt_tokens: n("input_tokens") + n("cache_read_input_tokens"),
                completion_tokens: n("output_tokens"),
                cached_tokens: n("cache_read_input_tokens"),
                ..Default::default()
            },
            model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

impl ChatModel for AnthropicMessages {
    fn name(&self) -> String {
        format!("{}:{}", self.label, self.model)
    }

    fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/v1/messages", self.base_url);
        let headers = [("x-api-key", self.api_key.as_str()), ("anthropic-version", API_VERSION)];
        let v = post_json(&url, &headers, &self.build_body(req), self.timeout)?;
        Self::parse_response(&v)
    }

    fn chat_stream(&self, req: &ChatRequest, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/v1/messages", self.base_url);
        let headers = [("x-api-key", self.api_key.as_str()), ("anthropic-version", API_VERSION)];
        let mut body = self.build_body(req);
        body["stream"] = json!(true);
        let mut state = AnthropicStreamState::default();
        let mut buffered: Option<ChatResponse> = None;
        post_sse(&url, &headers, &body, &mut |ev| {
            if ev.event.as_deref() == Some("__json__") {
                let r = Self::parse_response(&sse_json(&ev.data)?)?;
                emit_response(&r, sink);
                buffered = Some(r);
                return Ok(false);
            }
            state.on_event(&sse_json(&ev.data)?, sink)
        })?;
        match buffered {
            Some(r) => Ok(r),
            None => state.finish(sink),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    #[test]
    fn merges_tool_results_and_lifts_system() {
        let p = AnthropicMessages::new("a", "https://x", "m", "k".into());
        let req = ChatRequest::new(vec![
            ChatMessage::system("rules"),
            ChatMessage::user("q"),
            ChatMessage::assistant_tools("thinking", vec![
                ToolCall { id: "1".into(), name: "a".into(), arguments: json!({}) },
                ToolCall { id: "2".into(), name: "b".into(), arguments: json!({}) },
            ]),
            ChatMessage::tool("1", "r1"),
            ChatMessage::tool("2", "r2"),
        ]);
        let b = p.build_body(&req);
        assert_eq!(b["system"], "rules");
        assert_eq!(b["messages"].as_array().unwrap().len(), 3);
        assert_eq!(b["messages"][1]["content"].as_array().unwrap().len(), 3);
        assert_eq!(b["messages"][2]["content"][1]["tool_use_id"], "2");
        assert_eq!(b["max_tokens"], 1024);
    }

    #[test]
    fn cache_ttl_adds_ephemeral_cache_control() {
        let p = AnthropicMessages::new("a", "https://x", "m", "k".into());
        let mut req = ChatRequest::new(vec![ChatMessage::system("rules"), ChatMessage::user("q")]);
        req.cache_ttl_secs = Some(300);
        let b = p.build_body(&req);
        assert_eq!(b["system"][0]["cache_control"]["ttl"], "5m");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        req.cache_ttl_secs = Some(3600);
        assert_eq!(p.build_body(&req)["system"][0]["cache_control"]["ttl"], "1h");
        // No TTL: system stays the plain joined string.
        req.cache_ttl_secs = None;
        assert_eq!(p.build_body(&req)["system"], "rules");
        // No system prompt: the TTL lands on the last user text block.
        let mut req2 = ChatRequest::new(vec![ChatMessage::user("q")]);
        req2.cache_ttl_secs = Some(300);
        let b2 = p.build_body(&req2);
        assert_eq!(b2["messages"][0]["content"][0]["cache_control"]["ttl"], "5m");
        assert_eq!(cache_ttl_label(61), "5m");
        assert_eq!(cache_ttl_label(3600), "1h");
    }

    #[test]
    fn parses_blocks() {
        let v = json!({"model": "m", "content": [
            {"type": "text", "text": "好的"},
            {"type": "tool_use", "id": "t1", "name": "calendar_list", "input": {"days": 1}}],
            "usage": {"input_tokens": 10, "output_tokens": 3, "cache_read_input_tokens": 90}});
        let r = AnthropicMessages::parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("好的"));
        assert_eq!(r.tool_calls[0].arguments["days"], 1);
        assert_eq!(r.usage.prompt_tokens, 100);
        assert_eq!(r.usage.cached_tokens, 90);
    }
}
