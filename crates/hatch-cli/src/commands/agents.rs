//! `hatch agents` — sub-agent types and runs.

use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_agent::RunStore;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum AgentsCmd {
    /// 可用的子代理类型（内置 + 工作区 agents/*.md）
    List {
        /// 工作区目录（覆盖配置 [workspace] root）
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 子代理运行记录（最新在前）
    Runs {
        /// 只看某个对话会话
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 一次运行的详情与进度事件
    Show { id: String },
    /// 取消一个运行中的子代理（通过 daemon API；子进程被 kill，状态记 cancelled）
    Cancel { id: String },
}

pub fn run(home: Option<&Path>, cmd: AgentsCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        AgentsCmd::List { workspace } => {
            let ws = app.workspace(workspace.as_deref())?;
            let (types, warnings) = super::chat::agent_types(ws.as_ref());
            for t in types.list() {
                let tools = match &t.tools {
                    None => "父代理的全部工具（写/执行仍需审批）".to_string(),
                    Some(v) => v.join(", "),
                };
                println!("{}  [{}]  最多 {} 步", t.name, t.source, t.max_steps);
                println!("    {}", t.description);
                println!("    工具：{tools}");
            }
            for w in warnings {
                println!("⚠ {w}");
            }
            if !app.config.agent.subagents {
                println!("\n注意：配置 [agent] subagents = false，对话中不会提供委派工具。");
            }
        }
        AgentsCmd::Runs { session, limit } => {
            let runs = RunStore::new(&app.db).list(session.as_deref(), limit)?;
            if runs.is_empty() {
                println!("没有子代理运行记录。");
            }
            for r in runs {
                println!(
                    "{}  {:<9}  深度 {}  {:<8}  {} 步  {} 次调用  {}",
                    r.id,
                    r.status.as_str(),
                    r.depth,
                    r.agent_type,
                    r.steps,
                    r.model_calls,
                    r.task
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect::<String>()
                );
            }
        }
        AgentsCmd::Show { id } => {
            let store = RunStore::new(&app.db);
            let r = store
                .get(&id)?
                .ok_or_else(|| anyhow!("没有子代理运行 {id}"))?;
            println!("运行 {}（类型 {}，深度 {}）", r.id, r.agent_type, r.depth);
            println!(
                "状态：{}   上级：{}   会话：{}",
                r.status.as_str(),
                r.parent,
                r.session
            );
            println!(
                "开始：{}   结束：{}",
                r.started_at,
                r.finished_at.as_deref().unwrap_or("—")
            );
            println!(
                "步数 {}，模型调用 {} 次，tokens {}/{}",
                r.steps, r.model_calls, r.prompt_tokens, r.completion_tokens
            );
            if !r.child_session.is_empty() {
                println!("子代理对话：`hatch session show {}`", r.child_session);
            }
            println!("\n任务书：\n{}", r.task);
            if !r.answer.is_empty() {
                println!("\n结论：\n{}", r.answer);
            }
            if !r.error.is_empty() {
                println!("\n未完成原因：{}", r.error);
            }
            if let Some((handoff, _)) = store.get_handoff(&id)? {
                println!("\n交接（handoff）：\n{handoff}");
            }
            println!("\n进度事件：");
            for e in store.events(&id)? {
                println!("  {:>3}  {}  {:<13} {}", e.seq, e.ts, e.kind, e.detail);
            }
        }
        AgentsCmd::Cancel { id } => {
            // Cancellation goes through the daemon: it owns the child
            // processes. A lone `hatch chat` process cannot be reached
            // from another terminal — start the daemon to cancel from here.
            let addr = app.config.daemon.http_bind.clone();
            let url = format!("http://{addr}/v1/agents/{id}/cancel");
            let mut req = ureq::post(&url);
            if let Some(token) = crate::api::auth::resolve_api_token(&app.home) {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
            match req.call() {
                Ok(resp) => {
                    let body: serde_json::Value = resp.into_json().unwrap_or(serde_json::Value::Null);
                    if body["cancelled"].as_bool() == Some(true) {
                        println!("已取消子代理 {id}（子进程将被终止，状态记为 cancelled）。");
                    } else {
                        println!(
                            "子代理 {id} 未取消：当前状态 {}。",
                            body["status"].as_str().unwrap_or("未知")
                        );
                    }
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let body: serde_json::Value = resp.into_json().unwrap_or(serde_json::Value::Null);
                    let msg = body["error"]["message"].as_str().unwrap_or("");
                    return Err(anyhow!("取消失败（HTTP {code}）：{msg}"));
                }
                Err(e) => {
                    return Err(anyhow!(
                        "无法连接 daemon API（{url}）：{e}。请先启动 daemon（`hatch daemon run --api`），或在对话里说“取消子代理”让模型从本进程取消。"
                    ));
                }
            }
        }
    }
    Ok(())
}
