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
            println!("\n进度事件：");
            for e in store.events(&id)? {
                println!("  {:>3}  {}  {:<13} {}", e.seq, e.ts, e.kind, e.detail);
            }
        }
    }
    Ok(())
}
