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
use serde_json::Value;
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
    /// 撤销一个 token：先通知供应商（若记录了撤销端点），无论如何
    /// 都在本地清除 secret 并标记撤销；撤销后所有使用该 token 的
    /// 请求都会被拒绝，引用该连接器的待审批也会列出来请你重新核对
    Revoke { name: String },
    /// 刷新一个 token（校验供应商地址、账户、范围；并发轮换安全）
    Refresh {
        name: String,
        /// 期望的账户（与记录不一致时拒绝刷新）
        #[arg(long)]
        account: Option<String>,
        /// 期望的范围（超出已授权范围时拒绝刷新）
        #[arg(long)]
        scope: Option<String>,
    },
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
        /// 撤销端点 URL（记在记录里；无则撤销只在本地生效）
        #[arg(long)]
        revoke_url: Option<String>,
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
                        let mut marks = String::new();
                        if s.revoked {
                            marks.push_str("  （已撤销）");
                        } else if s.expired {
                            marks.push_str("  （已过期）");
                        }
                        if let Some(a) = &s.account {
                            marks.push_str(&format!("  账户 {a}"));
                        }
                        println!("{}{}", s.name, marks);
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
        VaultCmd::Revoke { name } => {
            let vc = helper(&app).ok_or_else(|| {
                anyhow!("vault helper 启动失败，无法撤销 token")
            })?;
            let summary = vc.status(&name)?;
            let out = vc.revoke(&name)?;
            let told = out.get("provider_revoked").and_then(Value::as_bool).unwrap_or(false);
            println!(
                "token {name} 已撤销（{}）。此后所有使用它的请求都会被拒绝。",
                if told { "已通知供应商" } else { "未通知供应商（无撤销端点或通知失败），本地已阻断" }
            );
            if let Some(e) = out.get("provider_error").and_then(Value::as_str) {
                println!("供应商通知失败：{e}");
            }
            // Related approvals need re-checking (§3.3): list the pending
            // ones naming this connector so the user can re-examine them.
            // They are NOT auto-invalidated here — the connector executor
            // refuses a revoked token at execution time, so an approval
            // granted before the revoke can never run against it.
            if let Some(connector) = &summary.connector {
                let q = hatch_policy::ApprovalQueue::new(&app.db);
                let pending = q
                    .list(Some(hatch_policy::ApprovalStatus::Pending))
                    .unwrap_or_default();
                let hits: Vec<&hatch_policy::Approval> = pending
                    .iter()
                    .filter(|a| {
                        a.payload
                            .get("connector")
                            .and_then(Value::as_str)
                            .map(|c| c == connector)
                            .unwrap_or(false)
                    })
                    .collect();
                if hits.is_empty() {
                    println!("没有引用连接器 {connector} 的待审批。");
                } else {
                    println!("以下待审批引用连接器 {connector}，请重新核对后再决定：");
                    for a in hits {
                        println!("  {}  {}  {}", a.id, a.kind, a.title);
                    }
                }
                app.db.audit(
                    "user",
                    "vault_revoked",
                    &format!("{name} connector={connector} provider_revoked={told}"),
                )?;
            }
        }
        VaultCmd::Refresh { name, account, scope } => {
            let vc = helper(&app).ok_or_else(|| {
                anyhow!("vault helper 启动失败，无法刷新 token")
            })?;
            vc.refresh_with(&name, account.as_deref(), scope.as_deref())?;
            println!("token {name} 已刷新。");
        }
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
            revoke_url,
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
                revoke_url,
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
