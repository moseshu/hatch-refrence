//! `hatch vault` — credential vault.
//!
//! Tokens live in `$HATCH_HOME/vault/tokens.json` (dir 0700, file 0600)
//! and are held at runtime by the separate `hatch-vault-helper` process.
//! `set` / `list` / `delete` go through the helper when it can start;
//! when it cannot, they fall back to the file store with an honest
//! warning on stderr. Values are never printed, never written to config,
//! and never passed on the command line: `set` reads from stdin (or
//! `--from-env VAR`).
//!
//! `list` shows names (and an expiry marker) only — never values.

use crate::app::App;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_vault::client::StoreRequest;
use std::io::{IsTerminal, Read};
use std::path::Path;

#[derive(Subcommand)]
pub enum VaultCmd {
    /// 存入一个 token（从 stdin 读一行；或 --from-env VAR 从环境变量读）
    Set {
        name: String,
        /// 从指定环境变量读取 token 值（值本身不出现在命令行或配置里）
        #[arg(long)]
        from_env: Option<String>,
    },
    /// 只列出 token 的名字（永不显示值；过期 token 会标注）
    List,
    /// 删除一个 token
    Delete { name: String },
    /// vault helper 运行状态 + 文件权限自检
    Status,
    /// 停止正在运行的 vault helper
    Stop,
    /// OAuth 授权码流程（仅 scaffold：对本地 mock 授权端点）
    Oauth {
        /// 授权端点 URL（测试用本地 mock 服务器）
        #[arg(long)]
        auth_url: String,
        /// token 端点 URL（测试用本地 mock 服务器）
        #[arg(long)]
        token_url: String,
        /// client id
        #[arg(long)]
        client_id: String,
        /// 存入 vault 的 token 名字
        #[arg(long)]
        token_name: String,
        /// scope（空格分隔）
        #[arg(long, default_value = "")]
        scope: String,
        /// token 归属的连接器（记在记录里）
        #[arg(long)]
        connector: Option<String>,
    },
}

/// Start the helper; on failure warn and return `None` (caller falls back
/// to the file store).
fn helper(app: &App) -> Option<hatch_vault::VaultClient> {
    match hatch_vault::VaultClient::start(&app.home) {
        Ok(vc) => Some(vc),
        Err(e) => {
            eprintln!("（vault helper 启动失败：{e}，本次操作改用文件存储）");
            None
        }
    }
}

pub fn run(home: Option<&Path>, cmd: VaultCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        VaultCmd::Set { name, from_env } => {
            if !hatch_vault::is_valid_name(&name) {
                bail!("无效的 token 名字 `{name}`（只允许字母、数字、_、-，最多 64 字符）");
            }
            let value = match from_env {
                Some(var) => {
                    std::env::var(&var).map_err(|_| anyhow!("环境变量 {var} 未设置或为空"))?
                }
                None => {
                    if std::io::stdin().is_terminal() {
                        println!("请输入 {name} 的值（输入后按回车；不会回显到日志里）：");
                    }
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf.trim().to_string()
                }
            };
            if value.trim().is_empty() {
                bail!("token 值为空，未写入");
            }
            match helper(&app) {
                Some(vc) => {
                    vc.store(
                        &name,
                        StoreRequest {
                            value: value.trim().to_string(),
                            ..Default::default()
                        },
                    )?;
                }
                None => {
                    hatch_vault::open(&app.home)?.set(&name, value.trim())?;
                }
            }
            println!("token {name} 已存入 vault（helper 进程持有，文件 0600）。");
        }
        VaultCmd::List => match helper(&app) {
            Some(vc) => {
                let items = vc.list()?;
                if items.is_empty() {
                    println!("vault 里没有 token（`hatch vault set <名字>` 存入）。");
                } else {
                    for s in items {
                        println!("{}{}", s.name, if s.expired { "  （已过期）" } else { "" });
                    }
                }
            }
            None => {
                let vault = hatch_vault::open(&app.home)?;
                let names = vault.list()?;
                if names.is_empty() {
                    println!("vault 里没有 token（`hatch vault set <名字>` 存入）。");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                }
            }
        },
        VaultCmd::Delete { name } => {
            let deleted = match helper(&app) {
                Some(vc) => vc.delete(&name)?,
                None => hatch_vault::open(&app.home)?.delete(&name)?,
            };
            if deleted {
                println!("token {name} 已删除。");
            } else {
                return Err(anyhow!("vault 里没有 token {name}"));
            }
        }
        VaultCmd::Status => {
            let st = hatch_vault::helper_status(&app.home);
            match (st.running, st.pid) {
                (true, Some(p)) => println!("✅ vault helper 运行中（pid {p}）"),
                (true, None) => println!("✅ vault helper 运行中（pid 未知）"),
                _ => println!("➖ vault helper 未运行（下次需要 token 时自动启动；`hatch vault stop` 可手动停止）"),
            }
            let vault = hatch_vault::open(&app.home)?;
            let problems = vault.check_permissions();
            if problems.is_empty() {
                println!(
                    "✅ vault 目录/文件权限正常（目录 0700，tokens.json 0600），{} 个 token",
                    vault.list().map(|v| v.len()).unwrap_or(0)
                );
            } else {
                for p in problems {
                    println!("⚠ {p}");
                }
            }
        }
        VaultCmd::Stop => {
            if hatch_vault::stop_helper(&app.home) {
                println!("vault helper 已停止。");
            } else {
                println!("vault helper 未运行，无需停止。");
            }
        }
        VaultCmd::Oauth {
            auth_url,
            token_url,
            client_id,
            token_name,
            scope,
            connector,
        } => {
            if !hatch_vault::is_valid_name(&token_name) {
                bail!("无效的 token 名字 `{token_name}`");
            }
            let vc = helper(&app).ok_or_else(|| {
                anyhow!("vault helper 启动失败，OAuth 流程无法把 token 存入 vault")
            })?;
            let config = hatch_vault::oauth::OAuthConfig {
                client_id,
                auth_url,
                token_url,
                scope,
                token_name: token_name.clone(),
                connector,
            };
            let (url, pending) = hatch_vault::oauth::begin_flow(config.clone())?;
            // `wait_for_code` consumes the pending flow, so take the
            // redirect URI first: the token exchange must present the
            // same URI the flow started with.
            let redirect_uri = pending.redirect_uri().to_string();
            println!("请在浏览器打开以下地址完成授权（2 分钟内）：");
            println!("{url}");
            let code = pending.wait_for_code(std::time::Duration::from_secs(120))?;
            let result = hatch_vault::oauth::exchange_code(&config, &redirect_uri, &code, &vc)?;
            println!(
                "token 已存入 vault，名字 `{}`{}。",
                result.token_name,
                match result.expires_at {
                    Some(_) => "（带过期时间）",
                    None => "",
                }
            );
        }
    }
    Ok(())
}
