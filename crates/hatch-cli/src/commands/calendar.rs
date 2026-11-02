// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch calendar ...`

use crate::app::App;
use anyhow::Result;
use chrono::Duration;
use clap::Subcommand;
use hatch_connectors::calendar::ics::parse_ics;
use hatch_connectors::CalendarRepo;
use hatch_core::time::fmt_local;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum CalendarCmd {
    /// 导入 .ics 文件（可从 126/QQ/飞书/Outlook 日历导出）
    Import { file: PathBuf },
    /// 列出接下来几天的日程
    List {
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
}

pub fn run(home: Option<&Path>, cmd: CalendarCmd) -> Result<()> {
    let app = App::open(home)?;
    let cal = CalendarRepo::new(&app.db);
    match cmd {
        CalendarCmd::Import { file } => {
            let text = std::fs::read_to_string(&file)?;
            let source = file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let events = parse_ics(&text, &app.tz, &source)?;
            for e in &events {
                cal.upsert(e)?;
            }
            println!("已导入 {} 个日程（重复导入会更新而不是重复添加）", events.len());
        }
        CalendarCmd::List { days } => {
            let now = hatch_core::time::now();
            let evs = cal.between(&now, &(now + Duration::days(days)))?;
            if evs.is_empty() {
                println!("接下来 {days} 天没有日程。");
            }
            for e in evs {
                println!("{} — {}  {}{}", fmt_local(&e.starts_at, &app.tz), fmt_local(&e.ends_at, &app.tz), e.title, if e.location.is_empty() { String::new() } else { format!(" @{}", e.location) });
            }
        }
    }
    Ok(())
}
