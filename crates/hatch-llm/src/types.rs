// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Provider-neutral chat types.

use crate::LlmError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

/// A tool invocation requested by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// One message in a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn new(role: Role, content: impl Into<String>) -> Self {
        ChatMessage { role, content: content.into(), tool_calls: vec![], tool_call_id: None }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::new(Role::System, c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::new(Role::User, c)
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self::new(Role::Assistant, c)
    }
    pub fn assistant_tools(c: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        ChatMessage { tool_calls: calls, ..Self::new(Role::Assistant, c) }
    }
    pub fn tool(call_id: impl Into<String>, c: impl Into<String>) -> Self {
        ChatMessage { tool_call_id: Some(call_id.into()), ..Self::new(Role::Tool, c) }
    }
}

/// JSON-schema description of a tool offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Ask the provider for a JSON object (where supported).
    pub json_mode: bool,
    /// Reasoning effort for reasoning models (`low` / `medium` / `high`); omitted when `None`.
    pub reasoning_effort: Option<String>,
}

impl ChatRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        ChatRequest { messages, ..Default::default() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// Hidden reasoning tokens (part of `completion_tokens`) reported by reasoning models.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model: String,
}

impl ChatResponse {
    /// Plain text answer (used heavily by tests).
    pub fn text(s: impl Into<String>) -> Self {
        ChatResponse { content: Some(s.into()), ..Default::default() }
    }

    /// A response that requests tool calls.
    pub fn calls(calls: Vec<ToolCall>) -> Self {
        ChatResponse { tool_calls: calls, ..Default::default() }
    }
}

/// A chat-capable model backend.
pub trait ChatModel: Send + Sync {
    /// Stable display name (`provider:model`), used in usage records and receipts.
    fn name(&self) -> String;
    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors() {
        let m = ChatMessage::tool("c1", "ok");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(Role::parse("assistant"), Some(Role::Assistant));
        let json = serde_json::to_string(&ChatMessage::user("hi")).unwrap();
        assert!(!json.contains("tool_calls"));
    }
}
