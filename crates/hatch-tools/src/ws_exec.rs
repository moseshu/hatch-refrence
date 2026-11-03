//! Executors for workspace and web actions: write a file, run a command,
//! fetch a page, search the web.
//!
//! Each payload carries the absolute workspace root that was in effect when
//! the action was proposed. The payload is hash-protected by the approval
//! queue, so what runs is exactly what was reviewed — independent of the
//! directory or flags `hatch approvals approve` is later invoked with.

use crate::checkpoint::{content_hash, CheckpointStore};
use crate::executors::{ActionExecutor, ExecContext};
use crate::web::{fetch_with, FetchLimits, SearchBackend};
use crate::workspace::Workspace;
use hatch_core::{Error, Result};
use hatch_sandbox::Sandbox;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Model-facing cap on executor output.
pub const RESULT_CAP: usize = 8000;

fn cap(s: &str) -> String {
    if s.chars().count() > RESULT_CAP {
        format!("{}\n…（已截断，共 {} 字符）", s.chars().take(RESULT_CAP).collect::<String>(), s.chars().count())
    } else {
        s.to_string()
    }
}

// ------------------------------------------------------------ write_file

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WritePayload {
    pub root: PathBuf,
    pub path: String,
    pub content: String,
    /// Hash of the file when the change was proposed; `None` = it did not exist.
    pub base_hash: Option<String>,
}

pub struct WriteFileExecutor;

impl ActionExecutor for WriteFileExecutor {
    fn kind(&self) -> &'static str {
        "write_file"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: WritePayload = serde_json::from_value(payload.clone())?;
        let ws = Workspace::new(&p.root)?;
        let current = ws.read_bytes(&p.path, crate::checkpoint::MAX_SNAPSHOT_BYTES)?;
        let now_hash = current.as_deref().map(content_hash);
        if now_hash != p.base_hash {
            return Err(Error::Invalid(format!("{} 在审批之后被修改过，未写入；请重新生成修改", p.path)));
        }
        let cp = CheckpointStore::new(ctx.db).snapshot(&ws, &p.path, ctx.origin, ctx.approval_id, &ctx.now)?;
        ws.write_atomic(&p.path, p.content.as_bytes())?;
        Ok(format!("已写入 {}（{} 字节）。检查点 {cp}，可用 `hatch checkpoint rewind {cp}` 撤销。", p.path, p.content.len()))
    }
}

// ---------------------------------------------------------- exec_command

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecPayload {
    pub root: PathBuf,
    /// Working directory relative to the root ("" = root).
    pub cwd: String,
    pub command: String,
    pub timeout_secs: u64,
}

impl ExecPayload {
    pub fn preview(&self) -> String {
        format!(
            "在沙箱中执行（工作区 {}，目录 {}，超时 {} 秒）：\n$ {}",
            self.root.display(),
            if self.cwd.is_empty() { "." } else { &self.cwd },
            self.timeout_secs,
            self.command
        )
    }
}

pub struct ExecCommandExecutor {
    pub sandbox: Arc<Sandbox>,
}

impl ActionExecutor for ExecCommandExecutor {
    fn kind(&self) -> &'static str {
        "exec_command"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: ExecPayload = serde_json::from_value(payload.clone())?;
        let ws = Workspace::new(&p.root)?;
        let cwd = ws.resolve(if p.cwd.is_empty() { "." } else { &p.cwd })?;
        let timeout = Duration::from_secs(p.timeout_secs.clamp(1, 3600));
        let out = self.sandbox.run_shell_with(ws.root(), &cwd, &p.command, timeout, ctx.cancel)?;
        Ok(cap(&out.render(RESULT_CAP / 2)))
    }
}

// ------------------------------------------------------------- fetch_url

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FetchPayload {
    pub url: String,
}

pub struct FetchUrlExecutor {
    pub limits: FetchLimits,
}

impl ActionExecutor for FetchUrlExecutor {
    fn kind(&self) -> &'static str {
        "fetch_url"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: FetchPayload = serde_json::from_value(payload.clone())?;
        let f = fetch_with(&p.url, &self.limits, ctx.cancel)?;
        let mut head = format!("URL: {}\nHTTP {}，{}", f.final_url, f.status, if f.content_type.is_empty() { "未知类型" } else { &f.content_type });
        if f.truncated {
            head.push_str("，正文已截断");
        }
        Ok(cap(&format!("{head}\n\n{}", f.text)))
    }
}

// ------------------------------------------------------------ web_search

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SearchPayload {
    pub query: String,
    pub limit: usize,
}

pub struct WebSearchExecutor {
    pub backend: Arc<dyn SearchBackend>,
}

impl ActionExecutor for WebSearchExecutor {
    fn kind(&self) -> &'static str {
        "web_search"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let p: SearchPayload = serde_json::from_value(payload.clone())?;
        let hits = self.backend.search(&p.query, p.limit.clamp(1, 10))?;
        if hits.is_empty() {
            return Ok(format!("{}：没有结果。", self.backend.name()));
        }
        let lines: Vec<String> = hits.iter().enumerate().map(|(i, h)| format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, h.snippet)).collect();
        Ok(cap(&lines.join("\n")))
    }
}

/// Executor kinds whose results carry outside content and must be treated as untrusted.
pub const UNTRUSTED_RESULT_KINDS: &[&str] = &["exec_command", "fetch_url", "web_search"];
