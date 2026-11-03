//! Assembling a Gemini `streamGenerateContent?alt=sse` stream.
//!
//! Each event is a partial `GenerateContentResponse`: text parts are
//! increments, `functionCall` parts arrive whole, `usageMetadata` is
//! cumulative and `finishReason` appears on the last chunk. The chunks are
//! folded into one synthetic response and parsed with the buffered parser, so
//! both paths agree exactly.

use super::gemini::Gemini;
use crate::stream::{StreamEvent, StreamSink};
use crate::types::ChatResponse;
use crate::LlmError;
use serde_json::{json, Value};

#[derive(Default, Debug)]
pub(crate) struct GeminiStreamState {
    text: String,
    calls: Vec<Value>,
    finish: Option<String>,
    usage: Option<Value>,
    model: Option<String>,
    blocked: Option<Value>,
}

impl GeminiStreamState {
    pub(crate) fn on_chunk(&mut self, v: &Value, sink: &mut StreamSink<'_>) -> Result<(), LlmError> {
        if let Some(err) = v.get("error") {
            return Err(LlmError::permanent(format!("stream error: {err}")));
        }
        if let Some(pf) = v.get("promptFeedback").filter(|p| p.get("blockReason").is_some()) {
            self.blocked = Some(pf.clone());
        }
        if let Some(u) = v.get("usageMetadata") {
            self.usage = Some(u.clone());
        }
        if let Some(m) = v.get("modelVersion").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        let Some(cand) = v.pointer("/candidates/0") else { return Ok(()) };
        if let Some(f) = cand.get("finishReason").and_then(Value::as_str) {
            self.finish = Some(f.to_string());
        }
        for part in cand.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                if !t.is_empty() {
                    self.text.push_str(t);
                    sink(StreamEvent::TextDelta(t.to_string()));
                }
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                sink(StreamEvent::ToolCallStarted { name });
                self.calls.push(part.clone());
            }
        }
        Ok(())
    }

    /// The equivalent buffered response.
    pub(crate) fn assembled(&self) -> Result<ChatResponse, LlmError> {
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(json!({"text": self.text}));
        }
        parts.extend(self.calls.iter().cloned());
        let mut v = json!({"candidates": [{"content": {"role": "model", "parts": parts}}]});
        if let Some(f) = &self.finish {
            v["candidates"][0]["finishReason"] = json!(f);
        }
        if let Some(u) = &self.usage {
            v["usageMetadata"] = u.clone();
        }
        if let Some(m) = &self.model {
            v["modelVersion"] = json!(m);
        }
        if let Some(b) = &self.blocked {
            v["promptFeedback"] = b.clone();
        }
        Gemini::parse_response(&v)
    }

    pub(crate) fn finish(self, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        let r = self.assembled()?;
        sink(StreamEvent::Done);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::stream::Collected;

    #[test]
    fn chunks_fold_into_the_buffered_response() {
        let chunks = [
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "你"}]}}], "modelVersion": "g"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "好"},
                {"functionCall": {"name": "calendar_list", "args": {"days": 1}}, "thoughtSignature": "S"}]}}]}),
            json!({"candidates": [{"finishReason": "STOP", "content": {"role": "model", "parts": []}}],
                   "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4, "thoughtsTokenCount": 2}}),
        ];
        let sse: String = chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect();
        let mut c = Collected::default();
        let mut st = GeminiStreamState::default();
        let mut sink = c.sink();
        for ev in SseParser::new().push(sse.as_bytes()) {
            st.on_chunk(&serde_json::from_str(&ev.data).unwrap(), &mut sink).unwrap();
        }
        let r = st.finish(&mut sink).unwrap();
        drop(sink);
        let buffered = Gemini::parse_response(&json!({"modelVersion": "g", "candidates": [{"finishReason": "STOP",
            "content": {"parts": [{"text": "你好"}, {"functionCall": {"name": "calendar_list", "args": {"days": 1}}, "thoughtSignature": "S"}]}}],
            "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4, "thoughtsTokenCount": 2}}))
        .unwrap();
        assert_eq!(r, buffered);
        assert_eq!(c.text(), "你好");
        assert_eq!(r.tool_calls[0].id, "gemini_call_0~sig~S");
    }
}
