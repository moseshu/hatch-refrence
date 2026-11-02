// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch triage run`

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_policy::Notifier;
use hatch_triage::{TriageConfig, TriageWorkflow};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum TriageCmd {
    /// 拉取新邮件并分诊
    Run {
        /// 任何外发只写入 outbox 目录，不真正发送
        #[arg(long)]
        dry_run: bool,
        /// 使用 JSON 夹具邮箱代替 IMAP
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// 覆盖本次模型调用上限
        #[arg(long)]
        max_calls: Option<u64>,
    },
    /// 显示最近一次分诊回执
    Last,
}

pub fn run(home: Option<&Path>, cmd: TriageCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        TriageCmd::Run { dry_run, fixture, max_calls } => {
            let mut limits = app.default_limits();
            if max_calls.is_some() {
                limits.max_calls = max_calls;
            }
            let router = app.router(limits)?;
            let gate = app.policy()?;
            let executors = app.executors(dry_run)?;
            let mut source = app.source(fixture.as_deref())?;
            let now = hatch_core::time::now();
            let wf = TriageWorkflow {
                db: &app.db,
                router: &router,
                gate: &gate,
                executors: &executors,
                attention: app.attention(),
                config: TriageConfig {
                    user_name: app.config.user.name.clone(),
                    user_email: app.user_email(),
                    tz: app.tz,
                    fetch_limit: app.config.mail.fetch_limit,
                    approval_ttl_hours: 72,
                },
            };
            let receipt = wf.run(source.as_mut(), &now)?;
            app.record_usage(&router, &now)?;
            if dry_run {
                println!("（dry-run：外发动作只会写入 {}）\n", app.outbox_dir().display());
            }
            println!("{}", receipt.render_markdown());
            let immediate: Vec<_> = Notifier::new(&app.db, app.attention())
                .pending(false)?
                .into_iter()
                .filter(|n| n.delivery == "immediate")
                .collect();
            if !immediate.is_empty() {
                println!("### 需要立即关注");
                for n in immediate {
                    println!("- {}", n.text);
                }
            }
        }
        TriageCmd::Last => match hatch_triage::repo::last_receipt(&app.db)? {
            Some(json) => {
                let r: hatch_triage::Receipt = serde_json::from_str(&json)?;
                println!("{}", r.render_markdown());
            }
            None => println!("还没有分诊记录。运行 `hatch triage run`。"),
        },
    }
    Ok(())
}
