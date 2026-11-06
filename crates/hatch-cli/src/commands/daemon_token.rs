//! `hatch daemon token` — per-client bearer tokens for the product
//! API. Mirrors `hatch mcp token` (same registry implementation, see
//! [`crate::api::client_auth`]).
//!
//! Each client gets its own named token with its own scopes,
//! revocable on its own. The registry (`<home>/daemon-clients.json`,
//! mode 0600) holds only SHA-256 hashes; the plaintext is shown
//! exactly once, at creation. The server reloads the registry on
//! every request, so revoke takes effect on the next request with no
//! restart. Create/revoke are audited (name and id only, never the
//! plaintext token).

use crate::api::client_auth::{registry_path, DaemonRegistry};
use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_core::client_tokens::Scope;

/// Per-client token management commands (see `DaemonCmd::Token`).
#[derive(Subcommand)]
pub enum DaemonTokenCmd {
    /// 创建一个按客户端 token（只显示一次完整 token，请立即保存）
    Create {
        /// 客户端名字（字母/数字/_/-，最多 64 字符）
        name: String,
        /// 作用域，逗号分隔：read（只读，默认）、write（可写，含 read）
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
    },
    /// 列出已注册的客户端（只显示名字、作用域与状态，不显示 token）
    List,
    /// 撤销一个客户端（按名字或 id；下一个请求起立即拒绝，不用重启服务端）
    Revoke {
        /// 客户端名字或 id
        name_or_id: String,
    },
}

pub fn run_token(app: &App, cmd: DaemonTokenCmd) -> Result<()> {
    let path = registry_path(&app.home);
    let mut reg = DaemonRegistry::load(&path).map_err(|e| anyhow!("{e}"))?;
    match cmd {
        DaemonTokenCmd::Create { name, scope } => {
            let mut scopes = Vec::new();
            for s in &scope {
                scopes.push(Scope::parse(s).map_err(|e| anyhow!("{e}"))?);
            }
            let (rec, token) = reg.create(&name, scopes).map_err(|e| anyhow!("{e}"))?;
            reg.save(&path).map_err(|e| anyhow!("{e}"))?;
            let scopes = rec
                .scopes
                .iter()
                .map(Scope::as_str)
                .collect::<Vec<_>>()
                .join(",");
            app.db.audit(
                "cli",
                "daemon_token_created",
                &format!("{} ({}) scope={scopes}", rec.name, rec.id),
            )?;
            println!("已为客户端 `{}` 创建 token：", rec.name);
            println!("{token}");
            println!("（只显示一次，请立即保存。注册表里只存哈希，丢了只能 revoke 后重建。）");
            println!(
                "id：{}；scope：{scopes}；创建时间：{}",
                rec.id, rec.created_at
            );
        }
        DaemonTokenCmd::List => {
            if reg.clients.is_empty() {
                println!("还没有注册的客户端（`hatch daemon token create <名字>` 创建）。");
                return Ok(());
            }
            for c in &reg.clients {
                let scopes = c
                    .scopes
                    .iter()
                    .map(Scope::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}  {:<20} scope={:<10} 创建于 {}  {}",
                    if c.revoked {
                        "✗ 已撤销"
                    } else {
                        "● 生效中"
                    },
                    c.name,
                    scopes,
                    c.created_at,
                    c.id
                );
            }
        }
        DaemonTokenCmd::Revoke { name_or_id } => {
            let (name, id) = {
                let c = reg.revoke(&name_or_id).map_err(|e| anyhow!("{e}"))?;
                (c.name.clone(), c.id.clone())
            };
            reg.save(&path).map_err(|e| anyhow!("{e}"))?;
            app.db.audit(
                "cli",
                "daemon_token_revoked",
                &format!("{name} ({id})"),
            )?;
            println!("已撤销客户端 `{name}`（id {id}），下一个请求起立即拒绝，不用重启服务端。");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn create_list_revoke_round_trip_with_audit() {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        // Create a read token and a write token.
        run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "reader".to_string(),
                scope: vec!["read".to_string()],
            },
        )
        .unwrap();
        run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "writer".to_string(),
                scope: vec!["write".to_string()],
            },
        )
        .unwrap();
        // The registry file holds only hashes, never plaintext.
        let path = registry_path(&app.home);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("reader") && text.contains("writer"));
        assert!(!text.contains("hatch_daemon_"), "注册表文件绝不能含 token 明文");
        // List works (prints only; no panic on populated registry).
        run_token(&app, DaemonTokenCmd::List).unwrap();
        // Revoke by name; a second revoke is an error.
        run_token(
            &app,
            DaemonTokenCmd::Revoke {
                name_or_id: "reader".to_string(),
            },
        )
        .unwrap();
        assert!(run_token(
            &app,
            DaemonTokenCmd::Revoke {
                name_or_id: "reader".to_string(),
            },
        )
        .is_err());
        // Audit trail has both events, with name/id but no plaintext.
        let tail = app.db.audit_tail(10).unwrap();
        let has = |event: &str, name: &str| {
            tail.iter()
                .any(|e| e.event == event && e.detail.contains(name))
        };
        assert!(has("daemon_token_created", "reader"), "{tail:?}");
        assert!(has("daemon_token_created", "writer"), "{tail:?}");
        assert!(has("daemon_token_revoked", "reader"), "{tail:?}");
        assert!(
            !tail.iter().any(|e| e.detail.contains("hatch_daemon_")),
            "审计里绝不能出现 token 明文"
        );
    }

    #[test]
    fn duplicate_and_bad_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "a".to_string(),
                scope: vec![],
            },
        )
        .unwrap();
        assert!(run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "a".to_string(),
                scope: vec![],
            },
        )
        .is_err());
        assert!(run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "bad name!".to_string(),
                scope: vec![],
            },
        )
        .is_err());
        assert!(run_token(
            &app,
            DaemonTokenCmd::Create {
                name: "b".to_string(),
                scope: vec!["admin".to_string()],
            },
        )
        .is_err());
    }
}
