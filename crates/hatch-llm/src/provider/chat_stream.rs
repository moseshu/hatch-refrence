//! Assembling a Chat Completions stream (`stream: true`) into a response.
//!
//! Text arrives as `choices[0].delta.content`; tool calls arrive as fragments
//! keyed by `index` whose `function.arguments` strings must be concatenated;
//! usage comes in a final chunk with empty `choices` when
//! `stream_options.include_usage` is set.

use super::common::{parse_arguments, truncation_error};
use super::openai_compat::parse_usage;
use crate::stream::{StreamEvent, StreamSink};
use crate::types::{ChatResponse, ToolCall, Usage};
use crate::LlmError;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default, Debug)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

#[derive(Default, Debug)]
pub(crate) struct ChatStreamState {
    text: String,
    calls: BTreeMap<u64, PartialCall>,
    finish: String,
    usage: Usage,
    model: String,
}

impl ChatStreamState {
    pub(crate) fn on_chunk(&mut self, v: &Value, sink: &mut StreamSink<'_>) -> Result<(), LlmError> {
        if let Some(err) = v.get("error") {
            return Err(LlmError::permanent(format!("stream error: {err}")));
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            if self.model.is_empty() {
                self.model = m.to_string();
            }
        }
        if v.get("usage").is_some_and(|u| !u.is_null()) {
            self.usage = parse_usage(v.get("usage"));
        }
        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            return Ok(());
        };
        for choice in choices {
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = f.to_string();
            }
            let Some(delta) = choice.get("delta") else { continue };
            if let Some(t) = delta.get("content").and_then(Value::as_str) {
                if !t.is_empty() {
                    self.text.push_str(t);
                    sink(StreamEvent::TextDelta(t.to_string()));
                }
            }
            for (pos, frag) in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let index = frag.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
                let call = self.calls.entry(index).or_default();
                if let Some(id) = frag.get("id").and_then(Value::as_str) {
                    call.id = id.to_string();
                }
                if let Some(n) = frag.pointer("/function/name").and_then(Value::as_str) {
                    call.name.push_str(n);
                }
                if let Some(a) = frag.pointer("/function/arguments").and_then(Value::as_str) {
                    call.arguments.push_str(a);
                }
                if !call.announced && !call.name.is_empty() {
                    call.announced = true;
                    sink(StreamEvent::ToolCallStarted { name: call.name.clone() });
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() { format!("call_{i}") } else { c.id },
                name: c.name,
                arguments: parse_arguments(Some(&Value::String(c.arguments))),
            })
            .collect();
        if self.finish == "length" && self.text.trim().is_empty() && tool_calls.is_empty() {
            return Err(truncation_error("finish_reason=length", self.usage.completion_tokens, self.usage.reasoning_tokens));
        }
        sink(StreamEvent::Done);
        Ok(ChatResponse {
            content: if self.text.is_empty() { None } else { Some(self.text) },
            tool_calls,
            usage: self.usage,
            model: self.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::provider::OpenAiCompatible;
    use crate::stream::Collected;
    use serde_json::json;

    fn run(sse: &str) -> (Result<ChatResponse, LlmError>, Collected) {
        let mut c = Collected::default();
        let mut st = ChatStreamState::default();
        let mut sink = c.sink();
        for ev in SseParser::new().push(sse.as_bytes()) {
            if ev.data == "[DONE]" {
                break;
            }
            st.on_chunk(&serde_json::from_str(&ev.data).unwrap(), &mut sink).unwrap();
        }
        let r = st.finish(&mut sink);
        drop(sink);
        (r, c)
    }

    #[test]
    fn stream_equals_buffered_text_tools_and_usage() {
        let sse = concat!(
            "data: {\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"memory_search\",\"arguments\":\"{\\\"que\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ry\\\":\\\"老板\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":9,\"completion_tokens_details\":{\"reasoning_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let (r, c) = run(sse);
        let r = r.unwrap();
        let buffered = OpenAiCompatible::parse_response(&json!({
            "model": "m",
            "choices": [{"finish_reason": "tool_calls", "message": {"content": "你好", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "memory_search", "arguments": "{\"query\":\"老板\"}"}}]}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 9, "completion_tokens_details": {"reasoning_tokens": 4}}
        }))
        .unwrap();
        assert_eq!(r, buffered);
        assert_eq!(c.text(), "你好");
        assert!(c.events.contains(&StreamEvent::ToolCallStarted { name: "memory_search".into() }));
        assert_eq!(c.events.last(), Some(&StreamEvent::Done));
    }

    #[test]
    fn empty_truncated_stream_is_an_error() {
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"completion_tokens\":30,\"completion_tokens_details\":{\"reasoning_tokens\":30}}}\n\n";
        let (r, _) = run(sse);
        assert!(r.unwrap_err().message.contains("reasoning_tokens=30"));
    }
}
