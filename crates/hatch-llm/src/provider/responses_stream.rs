//! Assembling a Responses API stream.
//!
//! Text arrives as `response.output_text.delta`; a tool call is announced by
//! `response.output_item.added` (type `function_call`) and finished by
//! `response.output_item.done`. The terminal `response.completed` /
//! `response.incomplete` event carries the full response object, which is
//! parsed with the buffered parser so both paths agree exactly; deltas are
//! only used if a gateway omits it.

use super::common::truncation_error;
use super::openai_responses::{function_call, parse_usage, OpenAiResponses};
use crate::stream::{StreamEvent, StreamSink};
use crate::types::{ChatResponse, ToolCall, Usage};
use crate::LlmError;
use serde_json::Value;

#[derive(Default, Debug)]
pub(crate) struct ResponsesStreamState {
    text: String,
    calls: Vec<ToolCall>,
    usage: Usage,
    model: String,
    final_response: Option<Value>,
}

impl ResponsesStreamState {
    /// Handle one event; returns `Ok(false)` once the stream is complete.
    pub(crate) fn on_event(&mut self, v: &Value, sink: &mut StreamSink<'_>) -> Result<bool, LlmError> {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(Value::as_str) {
                    self.text.push_str(d);
                    sink(StreamEvent::TextDelta(d.to_string()));
                }
            }
            "response.output_item.added" => {
                if v.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let name = v.pointer("/item/name").and_then(Value::as_str).unwrap_or("").to_string();
                    sink(StreamEvent::ToolCallStarted { name });
                }
            }
            "response.output_item.done" => {
                if let Some(item) = v.get("item").filter(|i| i.get("type").and_then(Value::as_str) == Some("function_call")) {
                    self.calls.push(function_call(item, self.calls.len()));
                }
            }
            "response.created" | "response.in_progress" => {
                if let Some(m) = v.pointer("/response/model").and_then(Value::as_str) {
                    self.model = m.to_string();
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(r) = v.get("response") {
                    self.usage = parse_usage(r.get("usage"));
                    self.final_response = Some(r.clone());
                }
                return Ok(false);
            }
            "response.failed" | "error" => {
                let detail = v.pointer("/response/error").or_else(|| v.get("error")).or_else(|| v.get("message"));
                return Err(LlmError::permanent(format!("stream error: {}", detail.cloned().unwrap_or(Value::Null))));
            }
            _ => {}
        }
        Ok(true)
    }

    pub(crate) fn finish(self, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let r = match self.final_response {
            Some(v) => OpenAiResponses::parse_response(&v)?,
            None => {
                if self.text.trim().is_empty() && self.calls.is_empty() {
                    return Err(truncation_error("stream ended without output", self.usage.completion_tokens, self.usage.reasoning_tokens));
                }
                ChatResponse {
                    content: if self.text.is_empty() { None } else { Some(self.text) },
                    tool_calls: self.calls,
                    usage: self.usage,
                    model: self.model,
                }
            }
        };
        sink(StreamEvent::Done);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::stream::Collected;
    use serde_json::json;

    fn run(sse: &str) -> (Result<ChatResponse, LlmError>, Collected) {
        let mut c = Collected::default();
        let mut st = ResponsesStreamState::default();
        let mut sink = c.sink();
        for ev in SseParser::new().push(sse.as_bytes()) {
            if !st.on_event(&serde_json::from_str(&ev.data).unwrap(), &mut sink).unwrap() {
                break;
            }
        }
        let r = st.finish(&mut sink);
        drop(sink);
        (r, c)
    }

    #[test]
    fn deltas_and_completed_response_agree_with_buffered() {
        let full = json!({"model": "m", "status": "completed", "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "你好"}]},
            {"type": "function_call", "call_id": "c1", "name": "memory_search", "arguments": "{\"query\":\"x\"}"}],
            "usage": {"input_tokens": 5, "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 3}}});
        let sse = format!(
            "event: response.output_text.delta\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\n\
             event: response.output_item.added\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "你"}),
            json!({"type": "response.output_text.delta", "delta": "好"}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "name": "memory_search"}}),
            json!({"type": "response.completed", "response": full}),
        );
        let (r, c) = run(&sse);
        assert_eq!(r.unwrap(), OpenAiResponses::parse_response(&full).unwrap());
        assert_eq!(c.text(), "你好");
        assert!(c.events.contains(&StreamEvent::ToolCallStarted { name: "memory_search".into() }));
        assert_eq!(c.events.last(), Some(&StreamEvent::Done));
    }

    #[test]
    fn without_completed_event_deltas_are_used_and_errors_surface() {
        let sse = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "片段"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "k", "name": "t", "arguments": "{}"}}),
        );
        let (r, _) = run(&sse);
        let r = r.unwrap();
        assert_eq!(r.content.as_deref(), Some("片段"));
        assert_eq!(r.tool_calls[0].id, "k");
        let mut st = ResponsesStreamState::default();
        let mut sink = |_e: StreamEvent| {};
        assert!(st.on_event(&json!({"type": "response.failed", "response": {"error": {"message": "x"}}}), &mut sink).is_err());
    }
}
