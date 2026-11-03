//! `hatch brief` — daily briefs produced by the `daily_brief` job.

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_workers::BriefStore;
use std::path::Path;

#[derive(Subcommand)]
pub enum BriefCmd {
    /// 显示简报（默认最新一份）
    Show {
        /// 日期 YYYY-MM-DD
        #[arg(long)]
        date: Option<String>,
    },
    /// 列出最近的简报
    List {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

pub fn run(home: Option<&Path>, cmd: BriefCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = BriefStore::new(&app.db);
    match cmd {
        BriefCmd::Show { date } => match store.latest(date.as_deref())? {
            Some(b) => println!("{}", b.content),
            None => println!(
                "还没有{}简报。启用任务：`hatch jobs enable daily-brief` 并运行 `hatch daemon run`，或立即生成：`hatch jobs run-now daily-brief`。",
                date.map(|d| format!(" {d} 的")).unwrap_or_default()
            ),
        },
        BriefCmd::List { limit } => {
            for b in store.list(limit)? {
                let first = b.content.lines().find(|l| l.starts_with("共 ") || l.starts_with("无新事项")).unwrap_or("");
                println!("{}  生成于 {}  {}", b.local_date, fmt_local(&b.created_at, &app.tz), first);
            }
        }
    }
    Ok(())
}
