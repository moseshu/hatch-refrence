//! `hatch grants ...`

use crate::app::App;
use anyhow::Result;
use chrono::Duration;
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_policy::{ActionClass, GrantStore, NewGrant};
use std::path::Path;

#[derive(Subcommand)]
pub enum GrantsCmd {
    /// 新增常设授权，例如：--class send_message --to "*@corp.com" --days 30
    Add {
        #[arg(long)]
        class: String,
        /// 收件人模式（可多次）：精确地址、*@domain 或 *
        #[arg(long = "to")]
        to: Vec<String>,
        #[arg(long)]
        max_uses: Option<i64>,
        #[arg(long)]
        days: Option<i64>,
        /// 允许在“源自外部内容”的场景下也自动执行（谨慎）
        #[arg(long)]
        allow_tainted: bool,
        #[arg(long, default_value = "")]
        note: String,
    },
    List,
    Revoke {
        id: String,
    },
}

pub fn run(home: Option<&Path>, cmd: GrantsCmd) -> Result<()> {
    let app = App::open(home)?;
    let s = GrantStore::new(&app.db);
    let now = hatch_core::time::now();
    match cmd {
        GrantsCmd::Add {
            class,
            to,
            max_uses,
            days,
            allow_tainted,
            note,
        } => {
            let g = NewGrant {
                class: ActionClass::parse(&class)?,
                recipients: to,
                max_uses,
                allow_when_tainted: allow_tainted,
                expires_at: days.map(|d| now + Duration::days(d)),
                note,
            };
            let id = s.add(&g, &now)?;
            println!("已添加 {id}");
            if allow_tainted {
                println!("⚠ 注意：此授权在回复外部来信时也会自动执行，请确认收件人范围足够窄。");
            }
        }
        GrantsCmd::List => {
            for g in s.list()? {
                let state = if g.is_active(&now) {
                    "有效"
                } else {
                    "失效"
                };
                let exp = g
                    .expires_at
                    .map(|e| fmt_local(&e, &app.tz))
                    .unwrap_or_else(|| "不过期".into());
                println!(
                    "{}  [{state}] {}  → {}  已用 {}/{}  到期 {exp}{}",
                    g.id,
                    g.class.as_str(),
                    g.recipients.join(", "),
                    g.used,
                    g.max_uses
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "∞".into()),
                    if g.allow_when_tainted {
                        "  [含外部内容场景]"
                    } else {
                        ""
                    }
                );
            }
        }
        GrantsCmd::Revoke { id } => {
            s.revoke(&id)?;
            println!("已撤销 {id}");
        }
    }
    Ok(())
}
