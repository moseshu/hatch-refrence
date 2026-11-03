//! `hatch feed` — feed items produced by the `daily_feed` job.
//!
//! Every item cites its source facts; the only exception is the fixed
//! intro item, which is marked as such.

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_goals::FeedStore;
use std::path::Path;

#[derive(Subcommand)]
pub enum FeedCmd {
    /// 列出最近的动态
    List {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 显示一条动态（默认最新一条）
    Show {
        #[arg(long)]
        id: Option<String>,
    },
}

pub fn run(home: Option<&Path>, cmd: FeedCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = FeedStore::new(&app.db);
    match cmd {
        FeedCmd::List { limit } => {
            for it in store.list(limit)? {
                let mark = if it.is_intro { "（开场）" } else { "" };
                println!(
                    "{}  {}{}  〔{}〕",
                    fmt_local(&it.created_at, &app.tz),
                    it.title,
                    mark,
                    it.source_facts.join(",")
                );
            }
        }
        FeedCmd::Show { id } => {
            let item = match id {
                Some(id) => store.list(1000)?.into_iter().find(|i| i.id == id),
                None => store.latest()?,
            };
            match item {
                Some(it) => {
                    println!("{}\n", it.title);
                    println!("{}", it.content);
                    if it.is_intro {
                        println!("\n（开场白，固定文本，不引用事实）");
                    } else {
                        println!("\n来源事实：{}；生成：{}", it.source_facts.join(", "), it.model);
                    }
                }
                None => println!("还没有动态。启用任务：`hatch jobs enable daily-feed` 并运行 `hatch daemon run`，或立即生成：`hatch jobs run-now daily-feed`。"),
            }
        }
    }
    Ok(())
}
