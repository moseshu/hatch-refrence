//! Live progress events from the turn loop and its sub-agents.
//!
//! Events are short lines (tool name plus a clipped argument summary), never
//! full tool output, so they are safe to print and to store.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    Step,
    ToolCall,
    ToolResult,
    Delegate,
    SubagentDone,
    Answer,
    Stopped,
}

impl ProgressKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProgressKind::Step => "step",
            ProgressKind::ToolCall => "tool_call",
            ProgressKind::ToolResult => "tool_result",
            ProgressKind::Delegate => "delegate",
            ProgressKind::SubagentDone => "subagent_done",
            ProgressKind::Answer => "answer",
            ProgressKind::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// 0 = the main agent, 1 = its sub-agents, …
    pub depth: usize,
    /// `main` or the sub-agent run id.
    pub agent: String,
    pub kind: ProgressKind,
    pub detail: String,
}

/// Receives progress from any thread (parallel sub-agents report concurrently).
pub type Observer<'a> = dyn Fn(&Progress) + Send + Sync + 'a;

/// Clip and redact a string for a progress line.
pub fn clip(s: &str, max: usize) -> String {
    let s = hatch_core::secrets::redact(s);
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_redacts_and_shortens() {
        assert_eq!(clip("a\n  b   c", 10), "a b c");
        assert_eq!(clip("一二三四五六", 3), "一二三…");
        assert!(!clip("密码：hunter22x 其他", 50).contains("hunter22x"));
    }
}
