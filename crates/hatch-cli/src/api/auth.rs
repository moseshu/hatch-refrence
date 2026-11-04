//! Bearer token for the product API (`hatch daemon run --api`).
//!
//! Same rules as the MCP HTTP server: the token lives in the vault
//! under `daemon-api` (preferred, revealed once at startup) or in the
//! `HATCH_DAEMON_API_TOKEN` environment variable — never on the command
//! line, never logged, never written to a file. A non-loopback bind
//! without a token is refused before the server starts.

use anyhow::{bail, Result};
use std::net::SocketAddr;
use std::path::Path;

/// Vault token name for the product API. `hatch vault set daemon-api`.
pub const API_TOKEN_NAME: &str = "daemon-api";
/// Environment variable fallback for the API token.
pub const API_TOKEN_ENV: &str = "HATCH_DAEMON_API_TOKEN";

/// Resolve the API bearer token: vault `daemon-api` first (helper
/// reveal, falling back to a file read with a warning — the same
/// approach as the MCP server), then `HATCH_DAEMON_API_TOKEN`.
/// `None` = no token configured (loopback-only binds only).
pub fn resolve_api_token(home: &Path) -> Option<String> {
    let from_vault = hatch_vault::VaultClient::start(home)
        .and_then(|vc| vc.reveal(API_TOKEN_NAME))
        .ok()
        .or_else(|| match hatch_vault::open(home).and_then(|v| v.get(API_TOKEN_NAME)) {
            Ok(Some(v)) => {
                eprintln!(
                    "（vault helper 未启动：token {API_TOKEN_NAME} 从文件读取，短暂经过本进程内存）"
                );
                Some(v)
            }
            _ => None,
        });
    if from_vault.is_some() {
        return from_vault;
    }
    std::env::var(API_TOKEN_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

/// A non-loopback bind requires a bearer token, otherwise the server
/// refuses to start. A loopback bind without a token is fine (only
/// local processes can reach it).
pub fn check_api_bind(addr: SocketAddr, token: Option<&str>) -> Result<()> {
    if !addr.ip().is_loopback() && token.map(str::is_empty).unwrap_or(true) {
        bail!("产品 API 绑定到非回环地址 {addr} 时必须配置 bearer token：用 `hatch vault set daemon-api` 存入 vault（或设置 HATCH_DAEMON_API_TOKEN 环境变量）；否则只绑定 127.0.0.1。");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_needs_no_token_but_off_loopback_does() {
        let loopback: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        assert!(check_api_bind(loopback, None).is_ok());
        assert!(check_api_bind(loopback, Some("")).is_ok());
        let lan: SocketAddr = "192.168.1.10:8787".parse().unwrap();
        assert!(check_api_bind(lan, None).is_err());
        assert!(check_api_bind(lan, Some("")).is_err());
        assert!(check_api_bind(lan, Some("secret")).is_ok());
    }

    #[test]
    fn env_token_is_used_when_no_vault_token() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(API_TOKEN_ENV, "env-token-123");
        assert_eq!(
            resolve_api_token(dir.path()).as_deref(),
            Some("env-token-123")
        );
        std::env::remove_var(API_TOKEN_ENV);
        assert_eq!(resolve_api_token(dir.path()), None);
    }
}
