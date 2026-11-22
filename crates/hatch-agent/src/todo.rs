//! Model-visible todo list for live progress display.
//!
//! The `todo_write` tool is a single full-overwrite list (the TodoWrite
//! shape): every call carries the complete current checklist, so renderers
//! never pair creates with updates — they just redraw the array. It is a
//! progress surface, not a task store: nothing is persisted here (the tool
//! call itself lands in the session transcript, which is the state), it
//! has no side effects, needs no approval, and never counts as coverage
//! for the turn-end completeness check.
//!
//! Design notes (from the Claude Code todo docs, verified 2026-11-22):
//! use it for tasks with 3+ distinct actions, user-provided lists, or on
//! request — never for single-step work; exactly one item `in_progress`
//! at a time; `activeForm` is the present-tense label shown while an item
//! runs. Argument keys are repaired leniently (models routinely emit
//! `active_form` / `subject` / `done` for `activeForm` / `content` /
//! `completed`), so a near-miss key never costs a step.

use crate::config::AgentConfig;
use hatch_llm::{ToolCall, ToolSpec};
use serde_json::{json, Value};

/// Tool name for the todo list writer.
pub const TODO_WRITE: &str = "todo_write";

/// Most items one list may carry.
pub const MAX_TODOS: usize = 50;

/// Longest item text (content / activeForm) kept, in characters.
pub const MAX_ITEM_CHARS: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    fn parse(raw: &str) -> Option<TodoStatus> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" | "not_started" => Some(TodoStatus::Pending),
            "in_progress" | "in-progress" | "inprogress" | "doing" | "active" => {
                Some(TodoStatus::InProgress)
            }
            "completed" | "complete" | "done" | "finished" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    /// Present-tense label shown while the item is in progress.
    pub active_form: Option<String>,
}

impl TodoItem {
    /// The label a renderer shows for this item right now.
    pub fn display(&self) -> &str {
        if self.status == TodoStatus::InProgress {
            self.active_form.as_deref().unwrap_or(&self.content)
        } else {
            &self.content
        }
    }
}

pub fn is_todo_call(name: &str) -> bool {
    name == TODO_WRITE
}

/// The `todo_write` tool definition, offered to the main agent only
/// (sub-agent progress is not user-facing) and only when enabled.
pub fn tool_spec(config: &AgentConfig) -> Option<ToolSpec> {
    if !config.todo_tools || config.depth != 0 {
        return None;
    }
    Some(ToolSpec {
        name: TODO_WRITE.into(),
        description: "Task-list tool (full overwrite: pass the complete latest list every time). Whenever a task needs 3 or more independent actions (e.g. reading several files, multi-step work, several items given at once), you must call this tool FIRST to lay out the plan, then start executing; mark each item in_progress as you start it (only one at a time) and call this tool again to mark it completed the moment it is done — do not batch the marks. Do not use it for single-step tasks (done in one tool call). content is an imperative title (e.g. \"Read f1.txt\"); activeForm is the present-continuous label (e.g. \"Reading f1.txt\") shown to the user while in progress. The list lets the user see progress live and keeps you from missing steps; it is for display only and never replaces actually doing the work.".into(),
        parameters: json!({"type": "object", "properties": {
            "todos": {"type": "array", "description": "the complete latest list (full overwrite, not incremental)", "items": {
                "type": "object", "properties": {
                    "content": {"type": "string", "description": "imperative title"},
                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                    "activeForm": {"type": "string", "description": "present-continuous label, e.g. \"Writing unit tests\" (optional)"}
                }, "required": ["content", "status"]
            }}
        }, "required": ["todos"]}),
    })
}

fn clip_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() > max {
        t.chars().take(max).collect()
    } else {
        t.to_string()
    }
}

fn get_str<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Parse a `todo_write` call's arguments into items, repairing near-miss
/// key names and status values. Errors name the fix so the model can
/// retry in one step.
pub fn parse_todos(args: &Value) -> Result<Vec<TodoItem>, String> {
    let arr = ["todos", "items", "tasks", "list"]
        .iter()
        .find_map(|k| args.get(k).and_then(Value::as_array))
        .ok_or_else(|| "missing todos array (the complete latest list)".to_string())?;
    if arr.len() > MAX_TODOS {
        return Err(format!("list is capped at {MAX_TODOS} items, got {}", arr.len()));
    }
    let mut items = Vec::with_capacity(arr.len());
    for (i, raw) in arr.iter().enumerate() {
        let content = get_str(raw, &["content", "subject", "title", "text"])
            .ok_or_else(|| format!("item {} is missing content", i + 1))?;
        let status_raw = get_str(raw, &["status", "state"])
            .ok_or_else(|| format!("item {} is missing status", i + 1))?;
        let status = TodoStatus::parse(status_raw).ok_or_else(|| {
            format!(
                "item {} has invalid status \"{status_raw}\"; must be pending / in_progress / completed",
                i + 1
            )
        })?;
        let active_form = get_str(raw, &["activeForm", "active_form", "active"])
            .map(|s| clip_chars(s, MAX_ITEM_CHARS));
        items.push(TodoItem {
            content: clip_chars(content, MAX_ITEM_CHARS),
            status,
            active_form,
        });
    }
    Ok(items)
}

/// One-line progress summary: `2/5 完成 · 正在：…`.
pub fn summary(items: &[TodoItem]) -> String {
    let done = items
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let current = items
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
        .map(|t| t.display());
    match current {
        Some(c) => format!("{done}/{} 完成 · 正在：{c}", items.len()),
        None => format!("{done}/{} 完成", items.len()),
    }
}

/// Multi-line checklist for progress display and the tool result echo.
pub fn render_list(items: &[TodoItem]) -> String {
    let mut lines = vec![format!("清单（{}）", summary(items))];
    for t in items {
        let mark = match t.status {
            TodoStatus::Completed => "✔",
            TodoStatus::InProgress => "▶",
            TodoStatus::Pending => "○",
        };
        lines.push(format!("{mark} {}", t.display()));
    }
    lines.join("\n")
}

/// Outcome of one `todo_write` call: the tool-result text, the progress
/// detail to emit (None on error), and the audit detail (counts only).
pub struct TodoOutcome {
    pub content: String,
    pub progress_detail: Option<String>,
    pub audit_detail: String,
}

/// Handle a `todo_write` call. Stateless: validate, render, echo.
pub fn handle_todo_call(call: &ToolCall) -> TodoOutcome {
    match parse_todos(&call.arguments) {
        Ok(items) => {
            let done = items
                .iter()
                .filter(|t| t.status == TodoStatus::Completed)
                .count();
            let all_done = !items.is_empty() && done == items.len();
            let rendered = render_list(&items);
            let content = if all_done {
                format!("已更新。{rendered}\n（清单全部完成，已清空。）")
            } else {
                format!("已更新。{rendered}")
            };
            TodoOutcome {
                content,
                progress_detail: Some(rendered),
                audit_detail: format!("n={} done={done}", items.len()),
            }
        }
        Err(e) => TodoOutcome {
            content: format!("ERROR: [invalid] {e}"),
            progress_detail: None,
            audit_detail: format!("invalid: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_repairs_keys() {
        let args = json!({"items": [
            {"subject": "写测试", "state": "done"},
            {"content": "跑回归", "status": "doing", "active_form": "正在跑回归"},
            {"text": "发 PR", "status": "todo"},
        ]});
        let items = parse_todos(&args).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[1].display(), "正在跑回归");
        assert_eq!(items[2].status, TodoStatus::Pending);
        assert_eq!(summary(&items), "1/3 完成 · 正在：正在跑回归");
    }

    #[test]
    fn rejects_bad_status_with_fix_hint() {
        let args = json!({"todos": [{"content": "x", "status": "blocked"}]});
        let err = parse_todos(&args).unwrap_err();
        assert!(err.contains("pending / in_progress / completed"));
    }

    #[test]
    fn rejects_missing_array_and_content() {
        assert!(parse_todos(&json!({})).is_err());
        assert!(parse_todos(&json!({"todos": [{"status": "pending"}]})).is_err());
    }
}
