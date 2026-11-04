//! Extracting provider-reported usage from upstream responses.
//!
//! Two wire shapes are understood, both best-effort (`None` when the
//! shape is unknown — usage is then recorded as zero tokens, never
//! estimated):
//!
//! * Responses API: `usage: {input_tokens, output_tokens,
//!   input_tokens_details: {cached_tokens},
//!   output_tokens_details: {reasoning_tokens}}` — in a buffered
//!   body, or in the `response.completed` SSE event's `response`
//!   object.
//! * Chat Completions: `usage: {prompt_tokens, completion_tokens,
//!   prompt_tokens_details: {cached_tokens},
//!   completion_tokens_details: {reasoning_tokens}}` — in a buffered
//!   body, or in the final streamed chunk when the client asked for
//!   `stream_options.include_usage`.

use hatch_llm::Usage;
use serde_json::Value;

fn u64_at(v: Option<&Value>, pointer: &str) -> u64 {
    v.and_then(|v| v.pointer(pointer))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Usage from one JSON value that carries a `usage` object (a whole
/// response body, or one SSE event's `data`). Returns `None` when the
/// value has no recognizable usage object.
pub fn usage_from_json(v: &Value) -> Option<Usage> {
    // Responses API nests usage under `response` in `response.completed`.
    let usage = v
        .get("usage")
        .or_else(|| v.get("response").and_then(|r| r.get("usage")))?;
    if !usage.is_object() {
        return None;
    }
    let prompt = u64_at(Some(usage), "/input_tokens").max(u64_at(Some(usage), "/prompt_tokens"));
    let completion =
        u64_at(Some(usage), "/output_tokens").max(u64_at(Some(usage), "/completion_tokens"));
    let cached = u64_at(Some(usage), "/input_tokens_details/cached_tokens")
        .max(u64_at(Some(usage), "/prompt_tokens_details/cached_tokens"));
    let reasoning = u64_at(Some(usage), "/output_tokens_details/reasoning_tokens").max(u64_at(
        Some(usage),
        "/completion_tokens_details/reasoning_tokens",
    ));
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: cached,
        reasoning_tokens: reasoning,
    })
}

/// Model name from a request body (`-` when the body is not JSON or
/// carries no `model` string). Best-effort, for logs and the ledger.
pub fn model_from_body(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "-".to_string())
}

/// Incremental SSE scanner: feed raw response bytes as they arrive;
/// when an event carries usage, it is returned (once — the first
/// usage found wins; later usage objects in the same stream are
/// ignored so a retried event cannot double-count).
///
/// Events are split on blank lines; `data:` lines of one event are
/// joined with `\n` before parsing. Chunk boundaries inside an event
/// are handled by buffering the tail.
#[derive(Default)]
pub struct SseUsageScanner {
    buf: Vec<u8>,
    found: Option<Usage>,
}

impl SseUsageScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Option<Usage> {
        if self.found.is_some() {
            return self.found;
        }
        self.buf.extend_from_slice(chunk);
        while let Some(end) = find_event_end(&self.buf) {
            let event = self.buf.drain(..end).collect::<Vec<u8>>();
            if let Some(u) = usage_in_event(&event) {
                self.found = Some(u);
                return self.found;
            }
        }
        self.found
    }

    /// Usage found so far, if any.
    pub fn found(&self) -> Option<Usage> {
        self.found
    }
}

/// Index just past the blank line ending one SSE event (`\n\n` or
/// `\r\n\r\n`), or `None` when the buffer holds no complete event.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 2);
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

/// Usage in one SSE event's raw bytes, or `None`.
fn usage_in_event(event: &[u8]) -> Option<Usage> {
    let text = String::from_utf8_lossy(event);
    let mut data = String::new();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(d.trim_start());
        }
    }
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(&data)
        .ok()
        .and_then(|v| usage_from_json(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_from_both_wire_shapes() {
        let responses = json!({"usage": {"input_tokens": 50, "output_tokens": 20,
            "input_tokens_details": {"cached_tokens": 32},
            "output_tokens_details": {"reasoning_tokens": 12}}});
        assert_eq!(
            usage_from_json(&responses),
            Some(Usage {
                prompt_tokens: 50,
                completion_tokens: 20,
                cached_tokens: 32,
                reasoning_tokens: 12
            })
        );
        let completed = json!({"type": "response.completed",
            "response": {"usage": {"input_tokens": 7, "output_tokens": 3}}});
        assert_eq!(
            usage_from_json(&completed).map(|u| (u.prompt_tokens, u.completion_tokens)),
            Some((7, 3))
        );
        let chat = json!({"usage": {"prompt_tokens": 9, "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 5}}});
        assert_eq!(
            usage_from_json(&chat).map(|u| (u.prompt_tokens, u.completion_tokens, u.cached_tokens)),
            Some((9, 4, 5))
        );
        assert_eq!(usage_from_json(&json!({"output": []})), None);
        assert_eq!(
            model_from_body(b"{\"model\": \"gpt-6-luna\"}"),
            "gpt-6-luna"
        );
        assert_eq!(model_from_body(b"not json"), "-");
    }

    #[test]
    fn sse_scanner_survives_chunk_boundaries() {
        let mut s = SseUsageScanner::new();
        // Event split mid-JSON across three chunks.
        assert_eq!(
            s.push(b"event: response.output_text.delta\ndata: {\"type\": \"response."),
            None
        );
        assert_eq!(s.push(b"output_text.delta\"}\n\ndata: {\"type\": \"response.completed\", \"response\": {\"usage\": {\"input_tokens\": 11"), None);
        let found = s.push(b", \"output_tokens\": 2}}}\n\n");
        assert_eq!(
            found.map(|u| (u.prompt_tokens, u.completion_tokens)),
            Some((11, 2))
        );
        // Later usage in the same stream does not overwrite the first.
        assert_eq!(
            s.push(b"data: {\"usage\": {\"input_tokens\": 99, \"output_tokens\": 99}}\n\n"),
            found
        );
        // [DONE] is ignored.
        let mut s2 = SseUsageScanner::new();
        assert_eq!(s2.push(b"data: [DONE]\n\n"), None);
    }
}
