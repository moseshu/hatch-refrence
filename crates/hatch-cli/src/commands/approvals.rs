//! `hatch approvals ...`

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use std::path::Path;

#[derive(Subcommand)]
pub enum ApprovalsCmd {
    /// 列出待审批（--all 列出全部）
    List {
        #[arg(long)]
        all: bool,
    },
    /// 查看审批详情（将要执行的确切内容）
    Show { id: String },
    /// 批准并执行
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
        /// 只写入 outbox，不真正发送
        #[arg(long)]
        dry_run: bool,
    },
    /// 拒绝
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 审批历史
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub fn run(home: Option<&Path>, cmd: ApprovalsCmd) -> Result<()> {
    let app = App::open(home)?;
    let q = ApprovalQueue::new(&app.db);
    let now = hatch_core::time::now();
    q.expire_due(&now)?;
    match cmd {
        ApprovalsCmd::List { all } => {
            let list = q.list(if all {
                None
            } else {
                Some(ApprovalStatus::Pending)
            })?;
            if list.is_empty() {
                println!("没有{}审批。", if all { "" } else { "待处理的" });
            }
            for a in list {
                println!(
                    "{}  [{}]  {}  （{} 到期）",
                    a.id,
                    a.status.as_str(),
                    a.title,
                    fmt_local(&a.expires_at, &app.tz)
                );
            }
        }
        ApprovalsCmd::Show { id } => {
            let a = q.get(&id)?;
            println!(
                "{} — {}\n状态：{}  类别：{}  来源：{}",
                a.id,
                a.title,
                a.status.as_str(),
                a.class.as_str(),
                a.origin
            );
            println!(
                "创建：{}  到期：{}",
                fmt_local(&a.created_at, &app.tz),
                fmt_local(&a.expires_at, &app.tz)
            );
            println!("需要确认的原因：");
            for r in &a.reasons {
                println!("  - {r}");
            }
            println!("\n{}\n", a.preview);
            if !a.payload_intact() {
                println!("⚠ 内容校验失败：payload 在提交后被修改，批准也不会执行。");
            }
        }
        ApprovalsCmd::Approve { id, note, dry_run } => {
            // P0 (approval unification): the CLI, the HTTP API and
            // the worker recovery path share one service — the plan
            // version check, the cancellation guard and the Gmail
            // sender selection live there now, not in each caller.
            use super::approval_service::{ApprovalService, ServiceOutcome};
            match ApprovalService::new(&app).approve_and_execute(&id, &note, dry_run, &now)? {
                ServiceOutcome::Executed(r) => println!("已执行：{r}"),
                ServiceOutcome::AlreadyDone(r) => println!("此前已执行过，未重复：{r}"),
                ServiceOutcome::Ambiguous(r) => println!("未执行：{r}"),
                ServiceOutcome::Failed(r) => println!("执行失败：{r}"),
                ServiceOutcome::Refused(r) => println!("{r}"),
            }
        }
        ApprovalsCmd::Reject { id, note } => {
            let s = q.decide(&id, false, &note, &now)?;
            println!("{id} → {}", s.as_str());
        }
        ApprovalsCmd::History { limit } => {
            for a in q.list(None)?.into_iter().take(limit) {
                let when = a
                    .decided_at
                    .map(|t| fmt_local(&t, &app.tz))
                    .unwrap_or_else(|| "—".into());
                println!(
                    "{}  {:<9} {}  决定于 {when}  {}",
                    a.id,
                    a.status.as_str(),
                    a.title,
                    a.note
                );
            }
        }
    }
    Ok(())
}
