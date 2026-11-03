// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Chat Completions compatible provider (`POST {base_url}/chat/completions`).

use super::http::post_json;
use crate::types::{ChatModel, ChatRequest, ChatResponse, Role, ToolCall, Usage};
use crate::LlmError;
use serde_json::{json, Map, Value};
use std::fmt;
use std::time::Duration;

pub struct OpenAiCompatible {
    pub label: String,
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub timeout: Duration,
}

impl fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatible")
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenAiCompatible {
    pub fn new(label: &str, base_url: &str, model: &str, api_key: String) -> Self {
        OpenAiCompatible {
            label: label.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            timeout: Duration::from_secs(120),
        }
    }

    /// Request body in the Chat Completions wire format.
    pub fn build_body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| match m.role {
                Role::Assistant if !m.tool_calls.is_empty() => json!({
                    "role": "assistant",
                    "content": if m.content.is_empty() { Value::Null } else { Value::String(m.content.clone()) },
                    "tool_calls": m.tool_calls.iter().map(|c| json!({
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.name, "arguments": c.arguments.to_string()}
                    })).collect::<Vec<_>>()
                }),
                Role::Tool => json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content
                }),
                r => json!({"role": r.as_str(), "content": m.content}),
            })
            .collect();
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("messages".into(), Value::Array(messages));
        if !req.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(
                    req.tools
                        .iter()
                        .map(|t| json!({"type": "function", "function": {
                            "name": t.name, "description": t.description, "parameters": t.parameters}}))
                        .collect(),
                ),
            );
        }
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(n) = req.max_tokens {
            body.insert("max_tokens".into(), json!(n));
        }
        if req.json_mode {
            body.insert("response_format".into(), json!({"type": "json_object"}));
        }
        if let Some(effort) = &req.reasoning_effort {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        Value::Object(body)
    }

    /// Parse a Chat Completions response.
    pub fn parse_response(v: &Value) -> Result<ChatResponse, LlmError> {
        let msg = v
            .pointer("/choices/0/message")
            .ok_or_else(|| LlmError::permanent("response has no choices[0].message"))?;
        let content = msg.get("content").and_then(Value::as_str).map(str::to_string);
        let mut tool_calls = Vec::new();
        if let Some(arr) = msg.get("tool_calls").and_then(Value::as_array) {
            for (i, c) in arr.iter().enumerate() {
                let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
                let raw = c.pointer("/function/arguments");
                let arguments = match raw {
                    Some(Value::String(s)) if s.trim().is_empty() => json!({}),
                    Some(Value::String(s)) => {
                        serde_json::from_str(s).unwrap_or_else(|_| json!({"_unparsed": s}))
                    }
                    Some(other) => other.clone(),
                    None => json!({}),
                };
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{i}"));
                tool_calls.push(ToolCall { id, name: name.to_string(), arguments });
            }
        }
        let u = v.get("usage");
        let n = |p: &str| u.and_then(|u| u.pointer(p)).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            prompt_tokens: n("/prompt_tokens"),
            completion_tokens: n("/completion_tokens"),
            cached_tokens: n("/prompt_tokens_details/cached_tokens").max(n("/prompt_cache_hit_tokens")),
            reasoning_tokens: n("/completion_tokens_details/reasoning_tokens"),
        };
        let finish = v.pointer("/choices/0/finish_reason").and_then(Value::as_str).unwrap_or("");
        let empty = content.as_deref().map(str::trim).unwrap_or("").is_empty();
        if finish == "length" && empty && tool_calls.is_empty() {
            // Reasoning models can spend the whole output budget thinking and return nothing.
            return Err(LlmError::permanent(format!(
                "output truncated before any answer (finish_reason=length, completion_tokens={}, reasoning_tokens={}); raise max_tokens or lower reasoning_effort",
                usage.completion_tokens, usage.reasoning_tokens
            )));
        }
        let model = v.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        Ok(ChatResponse { content, tool_calls, usage, model })
    }
}

impl ChatModel for OpenAiCompatible {
    fn name(&self) -> String {
        format!("{}:{}", self.label, self.model)
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let auth = format!("Bearer {}", self.api_key);
        let v = post_json(&url, &[("Authorization", &auth)], &self.build_body(req), self.timeout)?;
        let mut r = Self::parse_response(&v)?;
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
    fn body_shapes_tool_roundtrip() {
        let p = OpenAiCompatible::new("ds", "https://api.example.com/v1/", "m", "secret".into());
        assert!(!format!("{p:?}").contains("secret"));
        let mut req = ChatRequest::new(vec![
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            ChatMessage::assistant_tools("", vec![ToolCall { id: "c1".into(), name: "t".into(), arguments: json!({"q": 1}) }]),
            ChatMessage::tool("c1", "out"),
        ]);
        req.tools.push(ToolSpec { name: "t".into(), description: "d".into(), parameters: json!({"type":"object"}) });
        req.json_mode = true;
        req.max_tokens = Some(800);
        req.reasoning_effort = Some("low".into());
        let b = p.build_body(&req);
        assert_eq!(b["reasoning_effort"], "low");
        assert_eq!(b["messages"][2]["content"], Value::Null);
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"q\":1}");
        assert_eq!(b["messages"][3]["tool_call_id"], "c1");
        assert_eq!(b["tools"][0]["function"]["name"], "t");
        assert_eq!(b["response_format"]["type"], "json_object");
        assert_eq!(b["max_tokens"], 800);
        assert_eq!(p.base_url, "https://api.example.com/v1");
    }

    #[test]
    fn parses_tool_calls_and_cached_usage() {
        let v = json!({
            "model": "deepseek-chat",
            "choices": [{"message": {"content": null, "tool_calls": [
                {"id": "x", "type": "function", "function": {"name": "memory_search", "arguments": "{\"query\":\"老板\"}"}},
                {"id": "y", "type": "function", "function": {"name": "bad", "arguments": "{oops"}}
            ]}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 64}}
        });
        let r = OpenAiCompatible::parse_response(&v).unwrap();
        assert_eq!(r.content, None);
        assert_eq!(r.tool_calls[0].arguments["query"], "老板");
        assert_eq!(r.tool_calls[1].arguments["_unparsed"], "{oops");
        assert_eq!(r.usage, Usage { prompt_tokens: 100, completion_tokens: 7, cached_tokens: 64, reasoning_tokens: 0 });
        assert!(OpenAiCompatible::parse_response(&json!({})).is_err());
    }

    #[test]
    fn empty_answer_truncated_by_reasoning_is_an_error() {
        let v = json!({
            "choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": ""}}],
            "usage": {"prompt_tokens": 17, "completion_tokens": 30,
                      "completion_tokens_details": {"reasoning_tokens": 30}}
        });
        let e = OpenAiCompatible::parse_response(&v).unwrap_err();
        assert!(e.message.contains("reasoning_tokens=30"), "{}", e.message);
        // A truncated but non-empty answer is still returned.
        let ok = json!({"choices": [{"finish_reason": "length", "message": {"content": "部分"}}]});
        assert_eq!(OpenAiCompatible::parse_response(&ok).unwrap().content.as_deref(), Some("部分"));
    }
}
