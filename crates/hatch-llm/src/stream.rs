//! Streaming events emitted while a model answer is being generated.
//!
//! Every provider produces the same [`StreamEvent`] sequence, and the final
//! [`ChatResponse`] assembled from a stream is equivalent to the one the
//! buffered call would have returned (text, tool calls and usage).

use crate::types::ChatResponse;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// A piece of the visible answer text.
    TextDelta(String),
    /// The model started a tool call (arguments arrive later and are only
    /// available in the final response).
    ToolCallStarted { name: String },
    /// The answer is complete.
    Done,
}

/// Callback receiving stream events.
pub type StreamSink<'a> = dyn FnMut(StreamEvent) + 'a;

/// Replay a complete response as stream events (used by buffered fallbacks
/// and by providers without native streaming).
pub fn emit_response(r: &ChatResponse, sink: &mut StreamSink<'_>) {
    if let Some(t) = r.content.as_deref().filter(|t| !t.is_empty()) {
        sink(StreamEvent::TextDelta(t.to_string()));
    }
    for c in &r.tool_calls {
        sink(StreamEvent::ToolCallStarted { name: c.name.clone() });
    }
    sink(StreamEvent::Done);
}

/// Collects events; handy in tests.
#[derive(Default, Debug)]
pub struct Collected {
    pub events: Vec<StreamEvent>,
}

impl Collected {
    pub fn text(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn sink(&mut self) -> impl FnMut(StreamEvent) + '_ {
        move |e| self.events.push(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;
    use serde_json::json;

    #[test]
    fn replays_text_tools_and_done() {
        let mut r = ChatResponse::text("你好");
        r.tool_calls.push(ToolCall { id: "1".into(), name: "memory_search".into(), arguments: json!({}) });
        let mut c = Collected::default();
        emit_response(&r, &mut c.sink());
        assert_eq!(c.text(), "你好");
        assert_eq!(c.events.last(), Some(&StreamEvent::Done));
        assert!(c.events.contains(&StreamEvent::ToolCallStarted { name: "memory_search".into() }));
    }
}
