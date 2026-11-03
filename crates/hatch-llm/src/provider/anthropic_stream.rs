//! Assembling a Messages API stream.
//!
//! `message_start` carries the model and input usage; `content_block_start`
//! opens a text or `tool_use` block; `content_block_delta` brings
//! `text_delta` or `input_json_delta` fragments; `message_delta` carries the
//! final output-token count; `message_stop` ends the stream.

use super::common::{parse_arguments, u64_at};
use crate::stream::{StreamEvent, StreamSink};
use crate::types::{ChatResponse, ToolCall, Usage};
use crate::LlmError;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug)]
enum Block {
    Text,
    Tool { id: String, name: String, json: String },
    Other,
}

#[derive(Default, Debug)]
pub(crate) struct AnthropicStreamState {
    text: String,
    blocks: BTreeMap<u64, Block>,
    input_tokens: u64,
    cache_read: u64,
    output_tokens: u64,
    model: String,
}

impl AnthropicStreamState {
    /// Handle one event; returns `Ok(false)` at `message_stop`.
    pub(crate) fn on_event(&mut self, v: &Value, sink: &mut StreamSink<'_>) -> Result<bool, LlmError> {
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let m = v.get("message");
                self.model = m.and_then(|m| m.get("model")).and_then(Value::as_str).unwrap_or("").to_string();
                self.input_tokens = u64_at(m, "/usage/input_tokens");
                self.cache_read = u64_at(m, "/usage/cache_read_input_tokens");
                self.output_tokens = u64_at(m, "/usage/output_tokens");
            }
            "content_block_start" => {
                let b = v.get("content_block");
                let block = match b.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("text") => Block::Text,
                    Some("tool_use") => {
                        let name = b.and_then(|b| b.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
                        sink(StreamEvent::ToolCallStarted { name: name.clone() });
                        Block::Tool {
                            id: b.and_then(|b| b.get("id")).and_then(Value::as_str).unwrap_or("").to_string(),
                            name,
                            json: String::new(),
                        }
                    }
                    _ => Block::Other,
                };
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let d = v.get("delta");
                match (d.and_then(|d| d.get("type")).and_then(Value::as_str), self.blocks.get_mut(&index)) {
                    (Some("text_delta"), _) => {
                        let t = d.and_then(|d| d.get("text")).and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            self.text.push_str(t);
                            sink(StreamEvent::TextDelta(t.to_string()));
                        }
                    }
                    (Some("input_json_delta"), Some(Block::Tool { json, .. })) => {
                        json.push_str(d.and_then(|d| d.get("partial_json")).and_then(Value::as_str).unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let out = u64_at(Some(v), "/usage/output_tokens");
                if out > 0 {
                    self.output_tokens = out;
                }
            }
            "message_stop" => return Ok(false),
            "error" => {
                return Err(LlmError::permanent(format!("stream error: {}", v.get("error").cloned().unwrap_or(Value::Null))));
            }
            _ => {} // ping, content_block_stop
        }
        Ok(true)
    }

    pub(crate) fn finish(self, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let tool_calls = self
            .blocks
            .into_values()
            .filter_map(|b| match b {
                Block::Tool { id, name, json } => Some(ToolCall { id, name, arguments: parse_arguments(Some(&Value::String(json))) }),
                _ => None,
            })
            .collect();
        sink(StreamEvent::Done);
        Ok(ChatResponse {
            content: if self.text.is_empty() { None } else { Some(self.text) },
            tool_calls,
            usage: Usage {
                prompt_tokens: self.input_tokens + self.cache_read,
                completion_tokens: self.output_tokens,
                cached_tokens: self.cache_read,
                ..Default::default()
            },
            model: self.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::provider::AnthropicMessages;
    use crate::stream::Collected;
    use serde_json::json;

    #[test]
    fn stream_equals_buffered() {
        let events = [
            json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "好"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "的"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "calendar_list", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"da"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "ys\": 1}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ];
        let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
        let mut c = Collected::default();
        let mut st = AnthropicStreamState::default();
        let mut sink = c.sink();
        for ev in SseParser::new().push(sse.as_bytes()) {
            if !st.on_event(&serde_json::from_str(&ev.data).unwrap(), &mut sink).unwrap() {
                break;
            }
        }
        let r = st.finish(&mut sink).unwrap();
        drop(sink);
        let buffered = AnthropicMessages::parse_response(&json!({"model": "m", "content": [
            {"type": "text", "text": "好的"},
            {"type": "tool_use", "id": "t1", "name": "calendar_list", "input": {"days": 1}}],
            "usage": {"input_tokens": 10, "output_tokens": 3, "cache_read_input_tokens": 90}}))
        .unwrap();
        assert_eq!(r, buffered);
        assert_eq!(c.text(), "好的");
        assert!(c.events.contains(&StreamEvent::ToolCallStarted { name: "calendar_list".into() }));
    }
}
