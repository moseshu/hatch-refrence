//! Provider-neutral chat types.

use crate::stream::{emit_response, StreamSink};
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

/// One part of a message's content. The models hatch talks to are
/// multimodal, so a message can carry images alongside text; text-only
/// call sites keep working through the `ChatMessage::user/...`
/// constructors, which build a single `Text` part.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ContentPart {
    Text(String),
    Image {
        /// Base64-encoded image bytes (PNG or JPEG).
        data: String,
        /// MIME type, e.g. `"image/png"`.
        media_type: String,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text(s.into())
    }

    pub fn image(data: impl Into<String>, media_type: impl Into<String>) -> Self {
        ContentPart::Image { data: data.into(), media_type: media_type.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text(t) => Some(t),
            ContentPart::Image { .. } => None,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, ContentPart::Image { .. })
    }
}

impl From<String> for ContentPart {
    fn from(s: String) -> Self {
        ContentPart::Text(s)
    }
}

impl From<&str> for ContentPart {
    fn from(s: &str) -> Self {
        ContentPart::Text(s.to_string())
    }
}

/// Envelope under which a message with image parts is stored in the
/// `session_messages.content` column (see [`ChatMessage::encode_content`]).
#[derive(Serialize, Deserialize)]
struct StoredContent {
    v: u32,
    parts: Vec<ContentPart>,
}

/// One message in a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn new(role: Role, content: impl Into<String>) -> Self {
        ChatMessage { role, content: vec![ContentPart::Text(content.into())], tool_calls: vec![], tool_call_id: None }
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

    /// All text parts concatenated, in order. Images contribute nothing.
    /// This is what every text-only reader (summaries, desensitisation
    /// checks, eval assertions, transcript display) uses.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let ContentPart::Text(t) = p {
                out.push_str(t);
            }
        }
        out
    }

    /// Whether the message carries at least one image part.
    pub fn has_images(&self) -> bool {
        self.content.iter().any(ContentPart::is_image)
    }

    /// `(base64 data, MIME type)` of each image part, in order.
    pub fn images(&self) -> impl Iterator<Item = (&str, &str)> {
        self.content.iter().filter_map(|p| match p {
            ContentPart::Image { data, media_type } => Some((data.as_str(), media_type.as_str())),
            _ => None,
        })
    }

    /// Append an image part, returning the message (builder style).
    pub fn with_image(mut self, data: impl Into<String>, media_type: impl Into<String>) -> Self {
        self.content.push(ContentPart::image(data, media_type));
        self
    }

    /// Attach an image only when one is present; a no-op for `None`.
    /// The pair is `(base64 bytes, media type)`.
    pub fn with_optional_image(self, image: Option<(String, String)>) -> Self {
        match image {
            Some((data, media_type)) => self.with_image(data, media_type),
            None => self,
        }
    }

    /// Attach tool-call bookkeeping decoded from the session store.
    pub fn with_meta(mut self, tool_calls: Vec<ToolCall>, tool_call_id: Option<String>) -> Self {
        self.tool_calls = tool_calls;
        self.tool_call_id = tool_call_id;
        self
    }

    /// Tool result carrying an image (e.g. a browser screenshot): the
    /// text first, then the image bytes.
    pub fn tool_with_image(
        call_id: impl Into<String>,
        c: impl Into<String>,
        data: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        ChatMessage { tool_call_id: Some(call_id.into()), ..Self::new(Role::Tool, c) }.with_image(data, media_type)
    }

    /// Serialize for the session store: plain text when the message is a
    /// single text part (the historical format — existing rows and the
    /// transcript display need no migration), otherwise a JSON envelope
    /// carrying every part. Images persist as inline base64, so history
    /// stays consistent even if the screenshot file is later deleted.
    pub fn encode_content(&self) -> String {
        if self.content.len() == 1 {
            if let ContentPart::Text(t) = &self.content[0] {
                return t.clone();
            }
        }
        if self.content.is_empty() {
            return String::new();
        }
        serde_json::to_string(&StoredContent { v: 1, parts: self.content.clone() }).unwrap_or_default()
    }

    /// Inverse of [`ChatMessage::encode_content`]: a JSON envelope decodes
    /// to parts; anything else is one text part (old rows, plain text). A
    /// user message that happens to be exactly the envelope JSON would
    /// decode as parts — pathological, and it still renders the same text.
    pub fn decode_content(role: Role, stored: &str) -> ChatMessage {
        if let Ok(env) = serde_json::from_str::<StoredContent>(stored) {
            if env.v == 1 {
                return ChatMessage { role, content: env.parts, tool_calls: vec![], tool_call_id: None };
            }
        }
        ChatMessage::new(role, stored)
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
    /// Prompt-cache TTL in seconds, when the router configured one for the
    /// workload. Only Anthropic honours it (300 = 5m, 3600 = 1h); other
    /// providers ignore the field. Never invented by the router.
    pub cache_ttl_secs: Option<u64>,
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
    /// Context window in tokens, when the model declares one. The value
    /// comes from static configuration (the user writes it in config or
    /// env) — never a per-model hardcoded number, and never a provider
    /// usage reading (usage is a meter for display only, never a
    /// compaction input). `None` = unknown: callers fall back to the
    /// absolute budget only (§3.4, OMB #23).
    fn context_window(&self) -> Option<usize> {
        None
    }
    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;

    /// Streaming variant. The default runs the buffered call and replays the
    /// result as events, so every model can be used where streaming is wanted.
    fn chat_stream(&self, req: &ChatRequest, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let r = self.chat(req)?;
        emit_response(&r, sink);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors() {
        let m = ChatMessage::tool("c1", "ok");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(m.text_content(), "ok");
        assert_eq!(Role::parse("assistant"), Some(Role::Assistant));
        let json = serde_json::to_string(&ChatMessage::user("hi")).unwrap();
        assert!(!json.contains("tool_calls"));
        assert!(json.contains("\"content\":[{\"type\":\"text\",\"value\":\"hi\"}]"), "{json}");
    }

    #[test]
    fn content_parts_and_text_view() {
        let m = ChatMessage::user("see this").with_image("aGVsbG8=", "image/png");
        assert_eq!(m.text_content(), "see this");
        assert!(m.has_images());
        assert_eq!(m.images().collect::<Vec<_>>(), vec![("aGVsbG8=", "image/png")]);
        assert!(!ChatMessage::user("plain").has_images());
        let tool = ChatMessage::tool_with_image("c9", "shot saved", "eHk=", "image/png");
        assert_eq!(tool.tool_call_id.as_deref(), Some("c9"));
        assert_eq!(tool.text_content(), "shot saved");
        assert_eq!(tool.images().count(), 1);
    }

    #[test]
    fn stored_content_round_trips() {
        // Text-only messages keep the historical plain-text format.
        let plain = ChatMessage::user("hello");
        assert_eq!(plain.encode_content(), "hello");
        assert_eq!(ChatMessage::decode_content(Role::User, "hello").text_content(), "hello");
        // Messages with images go through the JSON envelope.
        let shot = ChatMessage::tool_with_image("c1", "截图已保存：/tmp/a.png", "aGVsbG8=", "image/png");
        let stored = shot.encode_content();
        assert!(stored.contains("\"v\":1"), "{stored}");
        let back = ChatMessage::decode_content(Role::Tool, &stored);
        assert_eq!(back.text_content(), "截图已保存：/tmp/a.png");
        assert_eq!(back.images().collect::<Vec<_>>(), vec![("aGVsbG8=", "image/png")]);
        // Empty content stays empty; unknown JSON stays plain text.
        assert_eq!(ChatMessage::decode_content(Role::User, "").text_content(), "");
        assert_eq!(
            ChatMessage::decode_content(Role::User, "{\"v\":9,\"parts\":[]}").text_content(),
            "{\"v\":9,\"parts\":[]}"
        );
    }
}
