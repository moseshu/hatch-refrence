//! Image content parts in each provider's wire format.
//!
//! The four providers each need the same `Text` / `Image` mapping in
//! their own shape. It lives here, once, so the data-URL rule and the
//! tool-result follow-up caption stay identical everywhere: a tool
//! result's image is never silently dropped, and a provider that cannot
//! carry images fails at the call site instead (there is no such
//! provider in the router today — all four carry images).

use crate::types::{ChatMessage, ContentPart};
use serde_json::{json, Value};

/// `data:<media_type>;base64,<data>` — the URL form both OpenAI wire
/// formats use for an image part.
pub(crate) fn data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

/// Chat Completions `content`: a plain string when the message is
/// text-only, else an array of `text` / `image_url` parts.
pub(crate) fn chat_content(m: &ChatMessage) -> Value {
    if !m.has_images() {
        return Value::String(m.text_content());
    }
    Value::Array(
        m.content
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => json!({"type": "text", "text": t}),
                ContentPart::Image { data, media_type } => json!({
                    "type": "image_url",
                    "image_url": {"url": data_url(media_type, data)}
                }),
            })
            .collect(),
    )
}

/// The parts array a tool message's images travel as in the Chat
/// Completions format: a tool message cannot carry images, so they are
/// sent as a trailing user message right after it, captioned so the
/// picture stays tied to the tool result above it.
pub(crate) fn chat_image_followup(m: &ChatMessage) -> Value {
    let mut parts = vec![json!({"type": "text", "text": "（附图：上一个工具结果的图片）"})];
    for (data, media_type) in m.images() {
        parts.push(json!({"type": "image_url", "image_url": {"url": data_url(media_type, data)}}));
    }
    Value::Array(parts)
}

/// Responses API `content`: a plain string when the message is
/// text-only, else an array of `input_text` / `input_image` parts.
pub(crate) fn responses_input_content(m: &ChatMessage) -> Value {
    if !m.has_images() {
        return Value::String(m.text_content());
    }
    Value::Array(
        m.content
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => json!({"type": "input_text", "text": t}),
                ContentPart::Image { data, media_type } => {
                    json!({"type": "input_image", "image_url": data_url(media_type, data)})
                }
            })
            .collect(),
    )
}

/// The user message item that carries a tool result's image parts in
/// the Responses API: a `function_call_output` holds text only, so the
/// images follow as their own item, captioned the same way as the Chat
/// Completions follow-up.
pub(crate) fn responses_image_followup(m: &ChatMessage) -> Value {
    let mut parts = vec![json!({"type": "input_text", "text": "（附图：上一个工具结果的图片）"})];
    for (data, media_type) in m.images() {
        parts.push(json!({"type": "input_image", "image_url": data_url(media_type, data)}));
    }
    json!({"type": "message", "role": "user", "content": parts})
}

/// Anthropic content blocks: `text` / `image` (base64 source). Empty
/// text parts are skipped, matching the pre-image behaviour.
pub(crate) fn anthropic_blocks(m: &ChatMessage) -> Vec<Value> {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) if !t.is_empty() => Some(json!({"type": "text", "text": t})),
            ContentPart::Text(_) => None,
            ContentPart::Image { data, media_type } => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            })),
        })
        .collect()
}

/// Gemini `parts`: `text` / `inline_data`. Empty text parts are
/// skipped, matching the pre-image behaviour.
pub(crate) fn gemini_parts(m: &ChatMessage) -> Vec<Value> {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) if !t.is_empty() => Some(json!({"text": t})),
            ContentPart::Text(_) => None,
            ContentPart::Image { data, media_type } => {
                Some(json!({"inline_data": {"mime_type": media_type, "data": data}}))
            }
        })
        .collect()
}
