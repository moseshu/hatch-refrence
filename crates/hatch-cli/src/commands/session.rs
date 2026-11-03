//! `hatch session` — list, inspect, fork and rewind conversations.

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_agent::SessionStore;
use std::path::Path;

#[derive(Subcommand)]
pub enum SessionCmd {
    /// 会话列表（最新在前）
    List {
        /// 包括子代理的会话
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 查看会话全部记录（含已压缩的）
    Show { id: String },
    /// 复制一份会话历史到新会话（不复制审批、文件改动和子代理记录）
    Fork { id: String },
    /// 把会话截断到某一条（seq）之后的内容全部删除；文件改动不会回退
    Rewind {
        id: String,
        #[arg(long)]
        to: i64,
    },
}

fn preview(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

pub fn run(home: Option<&Path>, cmd: SessionCmd) -> Result<()> {
    let app = App::open(home)?;
    let s = SessionStore::new(&app.db);
    match cmd {
        SessionCmd::List { all, limit } => {
            for i in s.list(all, limit)? {
                println!("{}  {}  {} 条  {}", i.id, i.created_at, i.messages, i.title);
            }
        }
        SessionCmd::Show { id } => {
            for r in s.transcript(&id)? {
                let mark = if r.compacted { "（已压缩）" } else { "" };
                let extra = if r.tool_calls.is_empty() {
                    String::new()
                } else {
                    format!(
                        " → 调用 {}",
                        r.tool_calls
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                println!(
                    "{:>4}  {:<9}{mark} {}{extra}",
                    r.seq,
                    r.role,
                    preview(&r.content, 100)
                );
            }
        }
        SessionCmd::Fork { id } => {
            let new_id = s.fork(&id, &hatch_core::time::now())?;
            println!("已复制为新会话 {new_id}。继续它：hatch chat --session {new_id}");
        }
        SessionCmd::Rewind { id, to } => {
            let r = s.rewind(&id, to)?;
            println!(
                "已删除 seq {to} 之后的 {} 条记录{}。",
                r.removed,
                if r.uncompacted > 0 {
                    format!("，恢复了 {} 条被压缩的记录", r.uncompacted)
                } else {
                    String::new()
                }
            );
            println!("注意：这只回退对话。被删除的回合里改过的文件不会自动恢复，请用 `hatch checkpoint list` / `rewind` 处理。");
        }
    }
    Ok(())
}
