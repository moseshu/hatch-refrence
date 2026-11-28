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
use hatch_sandbox::{RunOutput, Sandbox};
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

/// Head+tail cap for long page text: answers often sit mid-page, so a
/// head-only cut loses them. Keeps the first two thirds and the last
/// third of the budget, with the omitted middle counted.
pub(crate) fn cap_head_tail(s: &str, cap_chars: usize) -> String {
    let total = s.chars().count();
    if total <= cap_chars {
        return s.to_string();
    }
    let head_n = cap_chars * 2 / 3;
    let tail_n = cap_chars - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(total - tail_n).collect();
    format!("{head}\n…（中间省略 {} 字符）…\n{tail}", total - head_n - tail_n)
}

/// Errors as guidance: keep the original failure, append the next
/// step the model can take instead of a bare exception string.
fn search_guidance(e: Error) -> Error {
    match e {
        Error::Connector(m) => Error::Connector(format!(
            "{m}。建议：换一组关键词重试，或用 web_fetch 直接打开已知站点（或其检索接口）。若要查的数据在某站点的自有数据库或目录里，搜索引擎查不到，应直接抓该站点。"
        )),
        other => other,
    }
}

fn fetch_guidance(e: Error) -> Error {
    match e {
        Error::Denied(m) => Error::Denied(format!(
            "{m}。该地址被安全策略拒绝；请换用公开 https 地址，或用 web_search 找该页面的公开链接。"
        )),
        Error::NotFound(m) => Error::NotFound(format!(
            "{m}。建议：检查域名拼写，或先用 web_search 确认正确地址。"
        )),
        Error::Invalid(m) => Error::Invalid(format!(
            "{m}。该地址不是可读的文本页面；如需其中信息，请换其他来源。"
        )),
        Error::Connector(m) if m.starts_with("HTTP 4") => Error::Connector(format!(
            "{m}。页面不存在或拒绝访问；建议用 web_search 搜标题找其他来源。"
        )),
        Error::Connector(m) => Error::Connector(format!(
            "{m}。建议：稍后重试，或用 web_search 找同一内容的其他页面。"
        )),
        other => other,
    }
}

/// Collapse whitespace runs and trim a snippet to `max` chars.
fn tidy_snippet(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        format!("{}…", collapsed.chars().take(max).collect::<String>())
    } else {
        collapsed
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
    fn verify(&self, payload: &Value, _receipt: &str, _ctx: &ExecContext) -> Result<crate::verification::EvidenceCheck> {
        crate::evidence_readers::file(payload)
    }

    fn observe(
        &self,
        payload: &Value,
        _receipt: &str,
        _ctx: &ExecContext,
    ) -> Result<std::collections::BTreeMap<String, Value>> {
        crate::evidence_readers::file_fields(payload)
    }

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

impl ExecCommandExecutor {
    /// Execute once and return the standard receipt together with the sandbox result.
    pub fn execute_with_output(&self, payload: &Value, ctx: &ExecContext) -> Result<(String, RunOutput)> {
        let p: ExecPayload = serde_json::from_value(payload.clone())?;
        let ws = Workspace::new(&p.root)?;
        let cwd = ws.resolve(if p.cwd.is_empty() { "." } else { &p.cwd })?;
        let timeout = Duration::from_secs(p.timeout_secs.clamp(1, 3600));
        let out = self.sandbox.run_shell_with(ws.root(), &cwd, &p.command, timeout, ctx.cancel)?;
        let rendered = out.render(RESULT_CAP / 2);
        let full = match exec_guidance(&out) {
            Some(g) => format!("{rendered}{g}\n"),
            None => rendered,
        };
        // Head+tail so the status line and the guidance both survive.
        Ok((cap_head_tail(&full, RESULT_CAP), out))
    }
}

/// Next-step guidance for a command that did not succeed cleanly.
/// `None` when it exited 0 or was cancelled by the caller.
fn exec_guidance(out: &hatch_sandbox::RunOutput) -> Option<String> {
    if out.cancelled {
        return None;
    }
    if out.timed_out {
        return Some("建议：命令超时被终止。可拆小任务、减少输出量，或调大 timeout_secs 后重试；不要原样重跑同一条长命令。".into());
    }
    match out.exit_code {
        Some(0) => None,
        Some(c) => Some(format!(
            "建议：命令以 exit={c} 失败。先看 stderr 定位原因；若是路径或文件不存在，用 fs_list/fs_glob 确认后再改命令重试，不要原样重跑。"
        )),
        None => Some("建议：命令未正常结束（无退出码）。先看 stderr，再调整命令重试。".into()),
    }
}

impl ActionExecutor for ExecCommandExecutor {
    fn verify(&self, _payload: &Value, _receipt: &str, _ctx: &ExecContext) -> Result<crate::verification::EvidenceCheck> {
        // A sandbox command's effects have no generic independent
        // read-back: the captured output is the only record, and
        // re-running the command would repeat the action. Workspace
        // file changes that need independent verification must go
        // through write_file (file-content-v1), not a shell command.
        Ok(crate::verification::EvidenceCheck::not_applicable(
            "exec-command-v1",
            "沙箱命令无通用独立回读方式（效果由命令输出呈现）；工作区文件变更应走 write_file 以便独立回读",
        ))
    }

    fn kind(&self) -> &'static str {
        "exec_command"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        self.execute_with_output(payload, ctx).map(|(receipt, _)| receipt)
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
    fn contract(&self) -> crate::verification::EffectContract {
        crate::verification::EffectContract::ReadOnly
    }

    fn kind(&self) -> &'static str {
        "fetch_url"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: FetchPayload = serde_json::from_value(payload.clone())?;
        let f = fetch_with(&p.url, &self.limits, ctx.cancel).map_err(fetch_guidance)?;
        let mut head = format!("URL: {}\nHTTP {}，{}", f.final_url, f.status, if f.content_type.is_empty() { "未知类型" } else { &f.content_type });
        if f.truncated {
            head.push_str("，正文已截断");
        }
        let body = cap_head_tail(&f.text, RESULT_CAP);
        Ok(cap(&format!("{head}\n\n{body}")))
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
    fn contract(&self) -> crate::verification::EffectContract {
        crate::verification::EffectContract::ReadOnly
    }

    fn kind(&self) -> &'static str {
        "web_search"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let p: SearchPayload = serde_json::from_value(payload.clone())?;
        let hits = self.backend.search(&p.query, p.limit.clamp(1, 10)).map_err(search_guidance)?;
        if hits.is_empty() {
            return Ok(format!(
                "{}：没有结果。建议：去掉引号、减少关键词、换英文词，或用 web_fetch 直接打开目标站点（或其检索接口）。若要查的数据在某站点的自有数据库或目录里，搜索引擎查不到，应直接抓该站点。",
                self.backend.name()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let mut hit_lines: Vec<String> = Vec::new();
        for h in &hits {
            if !seen.insert(h.url.trim_end_matches('/').to_ascii_lowercase()) {
                continue;
            }
            hit_lines.push(format!("{}. {}\n   {}\n   {}", hit_lines.len() + 1, tidy_snippet(&h.title, 120), h.url, tidy_snippet(&h.snippet, 300)));
        }
        let header = format!("共 {} 条结果（{}，查询：{}）", hit_lines.len(), self.backend.name(), p.query);
        let footer = "（摘要是二手信息；若多个候选互相冲突或拿不准，用 web_fetch 打开原文核对后再下结论。）";
        Ok(cap(&format!("{header}\n{}\n{footer}", hit_lines.join("\n"))))
    }
}

/// Executor kinds whose results carry outside content and must be treated as untrusted.
pub const UNTRUSTED_RESULT_KINDS: &[&str] = &[
    "exec_command",
    "fetch_url",
    "web_search",
    "browser_open",
    "browser_click",
    "mcp_call",
    "connector_call",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::SearchHit;
    use hatch_store::Db;
    use serde_json::json;

    struct FakeBackend(Vec<SearchHit>);
    impl SearchBackend for FakeBackend {
        fn name(&self) -> String {
            "fake".into()
        }
        fn search(&self, _q: &str, _l: usize) -> Result<Vec<SearchHit>> {
            Ok(self.0.clone())
        }
    }

    struct FailingBackend;
    impl SearchBackend for FailingBackend {
        fn name(&self) -> String {
            "fake".into()
        }
        fn search(&self, _q: &str, _l: usize) -> Result<Vec<SearchHit>> {
            Err(Error::Connector("search: timeout".into()))
        }
    }

    fn ctx<'a>(db: &'a Db) -> ExecContext<'a> {
        ExecContext { db, now: hatch_core::time::now(), approval_id: "a1", origin: "test", cancel: None }
    }

    #[test]
    fn search_output_dedupes_trims_and_counts() {
        let db = Db::open_in_memory().unwrap();
        let long = "word ".repeat(200);
        let backend = Arc::new(FakeBackend(vec![
            SearchHit { title: "  A   title ".into(), url: "https://x.example/a".into(), snippet: long.clone() },
            SearchHit { title: "dup".into(), url: "https://x.example/a/".into(), snippet: "s".into() },
            SearchHit { title: "B".into(), url: "https://x.example/b".into(), snippet: "short".into() },
        ]));
        let ex = WebSearchExecutor { backend };
        let out = ex.execute(&json!({"query": "q", "limit": 5}), &ctx(&db)).unwrap();
        assert!(out.starts_with("共 2 条结果"), "{out}");
        assert!(out.contains("1. A title"), "{out}");
        assert!(out.contains("2. B"), "{out}");
        assert!(out.contains("…"), "long snippet trimmed with ellipsis");
        assert!(!out.contains("dup"), "duplicate URL dropped");
    }

    #[test]
    fn search_empty_and_error_carry_guidance() {
        let db = Db::open_in_memory().unwrap();
        let ex = WebSearchExecutor { backend: Arc::new(FakeBackend(vec![])) };
        let out = ex.execute(&json!({"query": "q", "limit": 5}), &ctx(&db)).unwrap();
        assert!(out.contains("没有结果") && out.contains("建议"), "{out}");
        assert!(out.contains("自有数据库"), "site-database hint missing: {out}");
        let ex = WebSearchExecutor { backend: Arc::new(FailingBackend) };
        let e = ex.execute(&json!({"query": "q", "limit": 5}), &ctx(&db)).unwrap_err();
        assert!(e.to_string().contains("timeout") && e.to_string().contains("建议"), "{e}");
        assert!(e.to_string().contains("自有数据库"), "site-database hint missing: {e}");
    }

    #[test]
    fn search_success_suggests_verifying_conflicting_candidates() {
        let db = Db::open_in_memory().unwrap();
        let backend = Arc::new(FakeBackend(vec![SearchHit {
            title: "T".into(),
            url: "https://x.example/a".into(),
            snippet: "s".into(),
        }]));
        let ex = WebSearchExecutor { backend };
        let out = ex.execute(&json!({"query": "q", "limit": 5}), &ctx(&db)).unwrap();
        assert!(out.contains("核对"), "verify note missing: {out}");
    }

    #[test]
    fn exec_guidance_covers_failure_modes() {
        let ok = hatch_sandbox::RunOutput { exit_code: Some(0), ..Default::default() };
        assert!(exec_guidance(&ok).is_none());
        let cancelled = hatch_sandbox::RunOutput { cancelled: true, ..Default::default() };
        assert!(exec_guidance(&cancelled).is_none());
        let timed = hatch_sandbox::RunOutput { timed_out: true, ..Default::default() };
        assert!(exec_guidance(&timed).unwrap().contains("timeout_secs"));
        let failed = hatch_sandbox::RunOutput { exit_code: Some(2), ..Default::default() };
        let g = exec_guidance(&failed).unwrap();
        assert!(g.contains("exit=2") && g.contains("stderr"), "{g}");
        let killed = hatch_sandbox::RunOutput::default();
        assert!(exec_guidance(&killed).unwrap().contains("无退出码"));
    }

    #[test]
    fn fetch_guidance_maps_each_error_kind() {
        assert!(fetch_guidance(Error::Denied("x".into())).to_string().contains("安全策略"));
        assert!(fetch_guidance(Error::NotFound("x".into())).to_string().contains("域名"));
        assert!(fetch_guidance(Error::Connector("HTTP 404 Not Found".into())).to_string().contains("web_search"));
        assert!(fetch_guidance(Error::Connector("timed out".into())).to_string().contains("重试"));
        assert!(fetch_guidance(Error::Invalid("x".into())).to_string().contains("文本页面"));
    }

    #[test]
    fn cap_head_tail_keeps_both_ends_and_counts_omitted() {
        let s = format!("{}{}{}", "a".repeat(100), "b".repeat(100), "c".repeat(100));
        let out = cap_head_tail(&s, 90);
        assert!(out.starts_with("aaa") && out.ends_with("ccc"), "{out}");
        assert!(out.contains("中间省略 210 字符"), "{out}");
        assert_eq!(cap_head_tail("short", 90), "short");
    }

    #[test]
    fn tidy_snippet_collapses_and_truncates() {
        assert_eq!(tidy_snippet("  a\n\n b\t c ", 10), "a b c");
        let out = tidy_snippet(&"x".repeat(50), 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }
}
