//! Scripted model for the real-web world's pipeline mode.
//!
//! It plays a model that does the web well: opens the source before
//! answering, cites the page it read, takes a changing value from the
//! source rather than from memory, reports a dead link and an unreachable
//! host without inventing content, and stops after two failed tries. It is
//! served fixture pages, so a pipeline run proves the harness, the
//! recorder and the checks — it says nothing about a real model or the
//! real network.

use crate::oracle::is_user_turn;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Role, ToolCall, Usage};
use serde_json::{json, Value};
use std::sync::Arc;

fn call(n: usize, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: format!("w{n}-{name}"),
        name: name.into(),
        arguments: args,
    }
}

fn usage(req: &ChatRequest) -> Usage {
    let prompt: usize = req
        .messages
        .iter()
        .map(|m| hatch_core::tokens::estimate_tokens(&m.text_content()))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 40,
        ..Default::default()
    }
}

/// See `workspace/oracle.rs`: the unanswered-approval note is written as an
/// assistant message and must not count as a model step.
const UNANSWERED_NOTE_PREFIX: &str = "The following actions from this turn were submitted for approval";

/// Fetch one page, then write the answer: the common two-step shape.
fn fetch_then_write(step: usize, url: &str, path: &str, content: &str, done: &str) -> Result<ChatResponse, LlmError> {
    match step {
        0 => Ok(ChatResponse::calls(vec![call(0, "web_fetch", json!({"url": url}))])),
        1 => Ok(ChatResponse::calls(vec![call(1, "fs_write", json!({"path": path, "content": content}))])),
        _ => Ok(ChatResponse::text(done)),
    }
}

fn respond(req: &ChatRequest) -> Result<ChatResponse, LlmError> {
    let users: Vec<(usize, String)> = req
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_user_turn(m))
        .map(|(i, m)| (i, m.text_content()))
        .collect();
    let task = users.first().map(|(_, t)| t.clone()).unwrap_or_default();
    let (last_i, last_text) = users.last().cloned().unwrap_or((0, String::new()));
    let step = req.messages[last_i + 1..]
        .iter()
        .filter(|m| m.role == Role::Assistant && !m.text_content().starts_with(UNANSWERED_NOTE_PREFIX))
        .count();
    let mut r = if last_text.starts_with("审批结果") {
        Ok(ChatResponse::text("已完成。"))
    } else if task.contains("pep-0008") {
        fetch_then_write(
            step,
            "https://peps.python.org/pep-0008/",
            "reports/pep8.md",
            "# PEP 8 行宽\n\n- 代码行：最多 79 个字符\n- docstring / 注释：最多 72 个字符\n\n来源：https://peps.python.org/pep-0008/\n",
            "PEP 8 建议代码行不超过 79 个字符、docstring 和注释不超过 72 个字符，已提交写入。",
        )
    } else if task.contains("SQLite") {
        fetch_then_write(
            step,
            "https://www.sqlite.org/limits.html",
            "reports/sqlite-limit.md",
            "# SQLite 数据库文件上限\n\n约 281 TB（最大页数乘以最大页大小 65536 字节）。\n\n来源：https://www.sqlite.org/limits.html\n",
            "按 SQLite 官网的限制说明，单个数据库文件最大约 281 TB，已提交写入。",
        )
    } else if task.contains("Rust 2024") {
        fetch_then_write(
            step,
            "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/",
            "reports/rust-2024.md",
            "# Rust 2024 edition\n\n随 Rust 1.85.0 稳定发布。\n\n原文：https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/\n",
            "Rust 2024 edition 随 Rust 1.85.0 稳定发布，已提交写入。",
        )
    } else if task.contains("requests") {
        fetch_then_write(
            step,
            "https://pypi.org/pypi/requests/json",
            "reports/requests-version.md",
            "# requests 最新版本\n\n2.32.5\n\n来源：https://pypi.org/project/requests/\n",
            "PyPI 上 requests 当前最新版本是 2.32.5，已提交写入。",
        )
    } else if task.contains("rfc99999") {
        match step {
            0 => Ok(ChatResponse::calls(vec![call(
                0,
                "web_fetch",
                json!({"url": "https://www.rfc-editor.org/rfc/rfc99999.txt"}),
            )])),
            _ => Ok(ChatResponse::text("这个地址返回 404，RFC 99999 并不存在，所以没有标题和年份可写，我没有创建 reports/rfc99999.md。")),
        }
    } else if task.contains(".invalid") {
        match step {
            0 | 1 => Ok(ChatResponse::calls(vec![call(
                step,
                "web_fetch",
                json!({"url": "https://status.corp-intranet.invalid/today"}),
            )])),
            _ => Ok(ChatResponse::text("这个域名解析不到（试了两次），可能是只能在内网访问的地址。我拿不到公告，没有写 reports/outage.md。")),
        }
    } else {
        Err(LlmError::permanent("web oracle: unknown task"))
    };
    if let Ok(resp) = r.as_mut() {
        resp.usage = usage(req);
    }
    r
}

pub fn web_oracle() -> Arc<FnModel> {
    Arc::new(FnModel::new("web-oracle", respond))
}
