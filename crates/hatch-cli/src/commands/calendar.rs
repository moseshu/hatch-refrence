//! `hatch calendar ...`
//!
//! * `import` / `list` — local .ics import (one-way) and listing.
//! * `sync` / `conflicts` / `resolve` / `status` — CalDAV bidirectional
//!   sync (Phase 6). Credentials come from the vault by token *name*
//!   only; the sync summary can be delivered to a chat through the
//!   transport relay (`--chat <id>`) instead of only printed — the
//!   relay runs the discretion check and writes the outbox, never
//!   this command directly.

use crate::app::App;
use anyhow::{bail, Result};
use chrono::Duration;
use clap::{Subcommand, ValueEnum};
use hatch_connectors::caldav::{
    CaldavClient, CaldavState, Keep, SyncEngine, SyncSettings, VaultHttp,
};
use hatch_connectors::calendar::ics::parse_ics;
use hatch_connectors::CalendarRepo;
use hatch_core::time::fmt_local;
use hatch_transports::{InboundMessage, PipeTransport, Relay, RelayOutcome, TurnOutcome};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Subcommand)]
pub enum CalendarCmd {
    /// 导入 .ics 文件（可从 126/QQ/飞书/Outlook 日历导出；单向导入，不会同步回服务器）
    Import { file: PathBuf },
    /// 列出接下来几天的日程
    List {
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// 与 CalDAV 服务器双向同步（先拉取服务器变化，再推送本地变化）
    Sync {
        /// 只列出会发生的变化，不写入本地数据库、不发 PUT/DELETE
        #[arg(long)]
        dry_run: bool,
        /// 把同步摘要通过传输转述发到指定聊天（走 Relay::deliver_main，
        /// 经过分享规则检查；不写此项时只打印到终端）
        #[arg(long)]
        chat: Option<String>,
    },
    /// 列出未解决的同步冲突（服务器版本已保留，本地版本记在冲突记录里）
    Conflicts,
    /// 处理一个冲突：--keep local 恢复本地版本（下次同步推送），
    /// --keep server 接受已保留的服务器版本
    Resolve {
        uid: String,
        #[arg(long, value_enum)]
        keep: KeepArg,
    },
    /// 同步状态：服务器、最近一次同步、sync-token、待推送的本地变化
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum KeepArg {
    Local,
    Server,
}

/// Build the sync engine from `[caldav]` config. Errors honestly when
/// the section is missing or the vault helper cannot start.
fn engine(app: &App, settings: SyncSettings) -> Result<SyncEngine<'_>> {
    let vc = hatch_vault::VaultClient::start(&app.home).map_err(|e| {
        anyhow::anyhow!("vault helper 启动失败（CalDAV token 须经 vault 提供）：{e}")
    })?;
    let http = VaultHttp::new(vc, settings.token_ref.clone());
    let client = CaldavClient::new(&settings.server_url, Box::new(http))
        .map_err(|e| anyhow::anyhow!("CalDAV 配置无效：{e}"))?;
    Ok(SyncEngine::new(&app.db, app.tz, client, settings))
}

fn settings_or_err(app: &App) -> Result<SyncSettings> {
    app.config.caldav_settings().ok_or_else(|| {
        anyhow::anyhow!(
            "未配置 [caldav] server_url（见 config/hatch.example.toml）；token 只存 vault（`hatch vault set caldav`），不写配置文件"
        )
    })
}

/// Deliver a sync summary to a chat through the transport relay: the
/// discretion check, widget fallback and artifact delivery all run
/// exactly as for a main-agent turn answer. Nothing here writes an
/// outbox file directly.
fn deliver_to_chat(app: &App, chat: &str, text: String) -> Result<()> {
    let chats = app.chat_store();
    let pipe = Arc::new(PipeTransport::new(
        app.pipe_dirs(),
        chats.clone(),
        app.artifacts_dir(),
        app.db.clone(),
    ));
    let device = app.capability_store().local().ok();
    let relay = Relay::new(pipe, chats, app.artifacts_dir(), device);
    let msg = InboundMessage {
        channel: "cli".into(),
        chat: chat.into(),
        sender: app.config.user.name.clone(),
        text: "calendar sync".into(),
        image: None,
    };
    let outcome = TurnOutcome {
        text,
        widget: None,
        artifact_id: None,
        labels: vec![],
        pending_approvals: vec![],
    };
    match relay.deliver_main(&msg, &outcome)? {
        RelayOutcome::Sent { .. } => println!("同步摘要已通过 pipe 传输发送到聊天 {chat}。"),
        RelayOutcome::Blocked { reason } => {
            println!("同步摘要未发送到聊天 {chat}（被分享规则拦截）：{reason}")
        }
    }
    Ok(())
}

pub fn run(home: Option<&Path>, cmd: CalendarCmd) -> Result<()> {
    let app = App::open(home)?;
    let cal = CalendarRepo::new(&app.db);
    match cmd {
        CalendarCmd::Import { file } => {
            let text = std::fs::read_to_string(&file)?;
            let source = file
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let events = parse_ics(&text, &app.tz, &source)?;
            for e in &events {
                cal.upsert(e)?;
            }
            println!(
                "已导入 {} 个日程（重复导入会更新而不是重复添加；导入是单向的，不会同步回服务器）",
                events.len()
            );
        }
        CalendarCmd::List { days } => {
            let now = hatch_core::time::now();
            let evs = cal.between(&now, &(now + Duration::days(days)))?;
            if evs.is_empty() {
                println!("接下来 {days} 天没有日程。");
            }
            for e in evs {
                println!(
                    "{} — {}  {}{}",
                    fmt_local(&e.starts_at, &app.tz),
                    fmt_local(&e.ends_at, &app.tz),
                    e.title,
                    if e.location.is_empty() {
                        String::new()
                    } else {
                        format!(" @{}", e.location)
                    }
                );
            }
        }
        CalendarCmd::Sync { dry_run, chat } => {
            let settings = settings_or_err(&app)?;
            let eng = engine(&app, settings)?;
            let report = eng.sync(dry_run)?;
            let summary = report.summary();
            println!("{summary}");
            if let Some(chat) = chat {
                deliver_to_chat(&app, &chat, summary)?;
            }
        }
        CalendarCmd::Conflicts => {
            let state = CaldavState::new(&app.db);
            let list = state.list_conflicts(true)?;
            if list.is_empty() {
                println!("没有未解决的同步冲突。");
                return Ok(());
            }
            println!("未解决的同步冲突（{} 个，服务器版本已保留）：", list.len());
            for c in &list {
                let local = c
                    .local_event()
                    .map(|e| {
                        format!(
                            "“{}” {}—{}",
                            e.title,
                            fmt_local(&e.starts_at, &app.tz),
                            fmt_local(&e.ends_at, &app.tz)
                        )
                    })
                    .unwrap_or_else(|| "（本地已删除）".into());
                println!(
                    "  {}  [{}] 本地更新 {}  服务器更新 {}\n      本地版本：{local}\n      处理：`hatch calendar resolve {} --keep local|server`",
                    c.uid,
                    c.kind,
                    if c.local_updated_at.is_empty() { "未知（本地修改无时间戳，记录的是发现时间）" } else { &c.local_updated_at },
                    if c.server_updated_at.is_empty() { "未知" } else { &c.server_updated_at },
                    c.uid,
                );
            }
        }
        CalendarCmd::Resolve { uid, keep } => {
            let state = CaldavState::new(&app.db);
            let keep = match keep {
                KeepArg::Local => Keep::Local,
                KeepArg::Server => Keep::Server,
            };
            println!("{}", state.resolve(&uid, keep)?);
        }
        CalendarCmd::Status => {
            let Some(settings) = app.config.caldav_settings() else {
                bail!("未配置 [caldav] server_url（见 config/hatch.example.toml）");
            };
            let state = CaldavState::new(&app.db);
            let st = state.status(&settings)?;
            println!("CalDAV 服务器：{}", st.server_url);
            println!(
                "账号：{}  token_ref：{}（只存名字，值在 vault 里）",
                if st.username.is_empty() {
                    "（未填）"
                } else {
                    &st.username
                },
                st.token_ref
            );
            if st.calendars.is_empty() {
                println!(
                    "还没有同步过任何日历（`hatch calendar sync` 后这里会列出最近一次同步）。"
                );
            }
            for (url, c) in &st.calendars {
                println!(
                    "  {url}\n    最近同步：{}  sync-token：{}",
                    c.last_sync_at.as_deref().unwrap_or("从未"),
                    if c.sync_token.is_empty() {
                        "无（下次全量拉取）"
                    } else {
                        "有"
                    }
                );
            }
            println!(
                "待推送的本地变化：新增 {}、修改 {}、删除 {}；未解决冲突 {} 个。",
                st.pending_new, st.pending_edit, st.pending_delete, st.open_conflicts
            );
        }
    }
    Ok(())
}
