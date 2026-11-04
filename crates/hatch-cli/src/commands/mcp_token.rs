//! `hatch mcp token` — per-client bearer tokens for the MCP HTTP
//! server. Split out of `commands::mcp` to keep that file under the
//! 400-line limit.
//!
//! Each client gets its own named token with its own scopes,
//! revocable on its own. The registry (`<home>/mcp-clients.json`,
//! mode 0600) holds only SHA-256 hashes; the plaintext is shown
//! exactly once, at creation. The server reloads the registry on
//! every request, so revoke takes effect on the next request with no
//! restart.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_mcp::{registry_path, ClientRegistry, Scope};
use std::path::Path;

/// Per-client token management commands (see `McpCmd::Token`).
#[derive(Subcommand)]
pub enum McpTokenCmd {
    /// 创建一个按客户端 token（只显示一次完整 token，请立即保存）
    Create {
        /// 客户端名字（字母/数字/_/-，最多 64 字符；会出现在审批 origin 里）
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

pub fn run_token(home: &Path, cmd: McpTokenCmd) -> Result<()> {
    let path = registry_path(home);
    let mut reg = ClientRegistry::load(&path).map_err(|e| anyhow!("{e}"))?;
    match cmd {
        McpTokenCmd::Create { name, scope } => {
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
            println!("已为客户端 `{}` 创建 token：", rec.name);
            println!("{token}");
            println!("（只显示一次，请立即保存。注册表里只存哈希，丢了只能 revoke 后重建。）");
            println!(
                "id：{}；scope：{scopes}；创建时间：{}",
                rec.id, rec.created_at
            );
        }
        McpTokenCmd::List => {
            if reg.clients.is_empty() {
                println!("还没有注册的客户端（`hatch mcp token create <名字>` 创建）。");
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
        McpTokenCmd::Revoke { name_or_id } => {
            let (name, id) = {
                let c = reg.revoke(&name_or_id).map_err(|e| anyhow!("{e}"))?;
                (c.name.clone(), c.id.clone())
            };
            reg.save(&path).map_err(|e| anyhow!("{e}"))?;
            println!("已撤销客户端 `{name}`（id {id}），下一个请求起立即拒绝，不用重启服务端。");
        }
    }
    Ok(())
}
