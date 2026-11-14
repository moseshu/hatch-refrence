//! `hatch browser status|sessions|takeover|resume|close`

use crate::app::App;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_browser::launch::{launch, pid_alive, running_endpoint};
use hatch_browser::{PageCondition, SessionState, SessionStore};
use hatch_core::time::fmt_local;
use std::path::Path;

#[derive(Subcommand)]
pub enum BrowserCmd {
    /// 各 profile 的浏览器进程与会话数
    Status,
    /// 浏览器会话列表
    Sessions {
        /// 包括已关闭的会话
        #[arg(long)]
        all: bool,
    },
    /// 用有界面的浏览器打开会话所在页面（同一个 profile），由你完成登录或人机验证；关闭窗口后结束
    Takeover { session: String },
    /// 接管完成后恢复会话：助手下一次使用时会在原地址重新打开页面
    Resume { session: String },
    /// 关闭会话
    Close { session: String },
}

fn has_display() -> bool {
    cfg!(target_os = "macos")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

pub fn run(home: Option<&Path>, cmd: BrowserCmd) -> Result<()> {
    let app = App::open(home)?;
    let broker = app.browser_for_management();
    let store = SessionStore::new(&app.db);
    match cmd {
        BrowserCmd::Status => {
            if !app.config.browser.enabled {
                println!("浏览器工具未开启（[browser] enabled = false）。");
            }
            for p in broker.status(&app.db) {
                let proc = match (p.running, p.pid) {
                    (true, Some(pid)) => format!("运行中 pid {pid}"),
                    _ => "未运行".into(),
                };
                println!(
                    "{:<10} {:<16} 打开的会话 {}",
                    p.profile, proc, p.open_sessions
                );
            }
        }
        BrowserCmd::Sessions { all } => {
            let list = store.list(all)?;
            if list.is_empty() {
                println!("没有浏览器会话。");
            }
            for s in list {
                println!(
                    "{}  {:<10} {:<10} {}  {}  {}{}",
                    s.id,
                    s.profile,
                    s.state.as_str(),
                    fmt_local(&s.updated_at, &app.tz),
                    s.origin,
                    s.url,
                    if s.reason.is_empty() {
                        String::new()
                    } else {
                        format!("  （{}）", s.reason)
                    }
                );
            }
        }
        BrowserCmd::Takeover { session } => {
            let rec = store.get(&session)?;
            if rec.state == SessionState::Closed {
                bail!("会话 {session} 已关闭");
            }
            if !has_display() {
                bail!(
                    "当前环境没有图形界面（DISPLAY / WAYLAND_DISPLAY 未设置），无法显示浏览器窗口。\
                     请在有桌面的电脑上运行 hatch 后再接管；profile 目录：{}",
                    broker.cfg.profile_dir(&rec.profile).display()
                );
            }
            if let Some(ep) = running_endpoint(&broker.cfg, &rec.profile) {
                if ep.owner_pid != std::process::id() && pid_alive(ep.owner_pid) {
                    bail!(
                        "profile {} 正被 hatch 进程 {} 使用（例如一个正在运行的 hatch chat）。请先退出它，再运行接管。",
                        rec.profile,
                        ep.owner_pid
                    );
                }
            }
            let url = if rec.url.is_empty() {
                "about:blank"
            } else {
                rec.url.as_str()
            };
            println!(
                "正在打开 {url}（profile {}）。请在浏览器窗口中完成登录或验证，完成后关闭窗口。",
                rec.profile
            );
            let mut l = launch(&broker.cfg, &rec.profile, false, url)?;
            for n in &l.notes {
                eprintln!("注意：{n}");
            }
            let _ = l.child.wait();
            app.db
                .audit("user", "browser_takeover", &format!("{session} {url}"))?;
            println!(
                "窗口已关闭。确认已经完成后运行 `hatch browser resume {session}`，再让助手继续。"
            );
        }
        BrowserCmd::Resume { session } => {
            let view = broker.resume(&app.db, &session)?;
            if view.needs_user() {
                let what = match &view.condition {
                    PageCondition::LoginRequired(d) => format!("需要登录：{d}"),
                    PageCondition::Captcha(d) => format!("人机验证：{d}"),
                    PageCondition::Blocked(d) => format!("被拦截：{d}"),
                    PageCondition::Ok => "仍需接管".to_string(),
                };
                println!(
                    "会话 {session} 仍需接管：{what}（{}）。请完成登录或验证后，再运行 `hatch browser resume {session}`。",
                    view.snapshot.url
                );
            } else {
                println!("会话 {session} 已恢复，页面已正常打开（{}）。助手下一次使用它时会继续。", view.snapshot.url);
            }
        }
        BrowserCmd::Close { session } => {
            broker.close(&app.db, &session)?;
            println!("已关闭会话 {session}。");
        }
    }
    Ok(())
}
