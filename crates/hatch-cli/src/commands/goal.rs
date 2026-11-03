//! `hatch goal` — goal workspaces (`$HATCH_HOME/goals/<id>/`).

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::now;
use hatch_goals::{goal_attention, GoalStatus, GoalStore};
use std::path::Path;

#[derive(Subcommand)]
pub enum GoalCmd {
    /// 新建目标（生成 GOAL.md + files/ + hidden_files/）
    Create {
        /// 目标标题
        title: String,
    },
    /// 列出目标（默认只列未完成；--all 列出全部）
    List {
        #[arg(long)]
        all: bool,
    },
    /// 查看一个目标（GOAL.md + 关注度 + 支撑文件）
    Show {
        id: String,
    },
    /// 标记完成
    Complete {
        id: String,
    },
    /// 暂停
    Pause {
        id: String,
    },
    /// 从暂停恢复为进行中
    Resume {
        id: String,
    },
    /// 放弃（终态，不可恢复）
    Abandon {
        id: String,
    },
}

pub fn run(home: Option<&Path>, cmd: GoalCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = GoalStore::new(&app.home);
    let t = now();
    match cmd {
        GoalCmd::Create { title } => {
            let g = store.create(&title, &t)?;
            println!("已创建目标 {}（{}）", g.id, g.title);
            println!("目录：{}", store.dir(&g.id).display());
        }
        GoalCmd::List { all } => {
            for g in store.list()? {
                if !all && g.status != GoalStatus::Active && g.status != GoalStatus::Paused {
                    continue;
                }
                let att = goal_attention(&app.db, &store, &g.id, &t)
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|_| "—".into());
                println!("{:<22} {:<10} {:<22} {}", g.id, g.status.as_str(), att, g.title);
            }
        }
        GoalCmd::Show { id } => {
            let g = store.get(&id)?;
            let att = goal_attention(&app.db, &store, &g.id, &t)?;
            println!("{}  [{}]  关注度：{}", g.title, g.status.as_str(), att.as_str());
            println!("创建：{}  更新：{}", g.created_at, g.updated_at);
            if !g.body.is_empty() {
                println!("\n{}", g.body);
            }
            let files = store.files(&id)?;
            if !files.is_empty() {
                println!("\n支撑文件（files/）：");
                for f in files {
                    println!("  {f}");
                }
            }
            println!("\n目录：{}", store.dir(&id).display());
        }
        GoalCmd::Complete { id } => transition(&store, &id, GoalStatus::Completed, &t)?,
        GoalCmd::Pause { id } => transition(&store, &id, GoalStatus::Paused, &t)?,
        GoalCmd::Resume { id } => transition(&store, &id, GoalStatus::Active, &t)?,
        GoalCmd::Abandon { id } => transition(&store, &id, GoalStatus::Abandoned, &t)?,
    }
    Ok(())
}

fn transition(store: &GoalStore, id: &str, next: GoalStatus, t: &chrono::DateTime<chrono::Utc>) -> Result<()> {
    let g = store.transition(id, next, t)?;
    println!("目标 {}（{}）→ {}", g.id, g.title, g.status.as_str());
    Ok(())
}
