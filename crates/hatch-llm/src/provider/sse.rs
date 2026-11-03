//! Incremental Server-Sent Events parser.
//!
//! Handles `data:` lines spread over several lines, `event:` names, comment
//! lines (`:`), CRLF line endings and input split at arbitrary byte positions
//! (multi-byte UTF-8 characters included). `[DONE]` is passed through as data;
//! providers decide what it means.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default, Debug)]
pub struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(ev) = self.line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let line = String::from_utf8_lossy(&rest).trim_end_matches('\r').to_string();
            if let Some(ev) = self.line(&line) {
                return Some(ev);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        Some(SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_data_events_comments_and_crlf() {
        let mut p = SseParser::new();
        let evs = p.push(b": keep-alive\r\nevent: message_start\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"a\":\n1}");
        assert_eq!(evs[1], SseEvent { event: None, data: "[DONE]".into() });
    }

    #[test]
    fn split_anywhere_including_inside_utf8() {
        let raw = "data: {\"t\":\"你好\"}\n\ndata: x\n\n".as_bytes();
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for b in raw {
            got.extend(p.push(std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, "{\"t\":\"你好\"}");
        // Unterminated trailing event is flushed by finish().
        let mut p = SseParser::new();
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(p.finish().unwrap().data, "tail");
        assert!(p.finish().is_none());
    }
}
