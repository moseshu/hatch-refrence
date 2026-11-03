//! `hatch ideas` — the ideas corpus (`ideas` table).
//!
//! Dismissed ideas never resurface: the listing excludes them by
//! construction (see `hatch_goals::ideas`).

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::now;
use hatch_goals::IdeaStore;
use std::path::Path;

#[derive(Subcommand)]
pub enum IdeasCmd {
    /// 列出未处理/已采纳的点子（已忽略的永不出现）
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 记一个点子
    Add {
        text: String,
        /// 来源（默认 user）
        #[arg(long, default_value = "user")]
        source: String,
    },
    /// 忽略一个点子（永不再出现）
    Dismiss {
        id: String,
    },
    /// 标记已采纳/已做
    Act {
        id: String,
    },
}

pub fn run(home: Option<&Path>, cmd: IdeasCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = IdeaStore::new(&app.db);
    let t = now();
    match cmd {
        IdeasCmd::List { limit } => {
            for i in store.list(limit)? {
                println!(
                    "{:<22} {:<10} {:<5} {:<16} {}",
                    i.id,
                    i.status.as_str(),
                    format!("{:.1}", i.confidence),
                    i.source,
                    i.text
                );
            }
        }
        IdeasCmd::Add { text, source } => {
            let id = store.add(&text, &source, 1.0, &t)?;
            println!("已记录点子 {id}");
        }
        IdeasCmd::Dismiss { id } => {
            store.dismiss(&id)?;
            println!("已忽略点子 {id}（不再出现）");
        }
        IdeasCmd::Act { id } => {
            store.mark_acted(&id)?;
            println!("点子 {id} 已标记为已采纳");
        }
    }
    Ok(())
}
