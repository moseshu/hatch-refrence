//! HTTP `serve` 的启动门禁：旧共享 token 的解析与非回环绑定的
//! token/TLS 要求。Split out of `commands::mcp` to keep that file
//! under the 400-line limit.

use anyhow::{bail, Result};
use std::net::SocketAddr;
use std::path::Path;

/// HTTP 服务端的 bearer token 名字（vault 里）。`hatch vault set
/// mcp-http` 存入；值只在启动时经 vault helper reveal 一次，随后只
/// 存在于服务端进程内存里，永不记日志、永不写文件。
const SERVE_TOKEN_NAME: &str = "mcp-http";

/// 解析 HTTP 服务端的 bearer token：vault 里的 `mcp-http` 优先（helper
/// reveal，起不来时回退文件读取并警告——与 `connect` 同一套做法），
/// 其次是 `HATCH_MCP_TOKEN` 环境变量；都没有就是 `None`（只允许回环绑定）。
pub fn resolve_serve_token(home: &Path) -> Option<String> {
    let from_vault = hatch_vault::VaultClient::start(home)
        .and_then(|vc| vc.reveal(SERVE_TOKEN_NAME))
        .ok()
        .or_else(|| match hatch_vault::open(home).and_then(|v| v.get(SERVE_TOKEN_NAME)) {
            Ok(Some(v)) => {
                eprintln!(
                    "（vault helper 未启动：token {SERVE_TOKEN_NAME} 从文件读取，短暂经过本进程内存）"
                );
                Some(v)
            }
            _ => None,
        });
    if from_vault.is_some() {
        return from_vault;
    }
    std::env::var("HATCH_MCP_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
}

/// 非回环绑定的启动门禁。回环绑定什么都不要求（只有本机进程能
/// 连）。非回环绑定必须有可用的 token（按客户端 token 或旧的共享
/// token），并且必须配 TLS——除非调用方用 `--allow-plain-http` 明确
/// 确认接受明文传输（此时启动时打印警告）。
pub fn check_http_bind(
    addr: SocketAddr,
    has_token: bool,
    tls: bool,
    allow_plain_http: bool,
) -> Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if !has_token {
        bail!("MCP HTTP 服务端绑定到非回环地址 {addr} 时必须配置 bearer token：按客户端 token 用 `hatch mcp token create <名字>`（或旧的 `hatch vault set mcp-http` / HATCH_MCP_TOKEN）；否则只绑定 127.0.0.1。");
    }
    if !tls && !allow_plain_http {
        bail!("MCP HTTP 服务端绑定到非回环地址 {addr} 时必须配置 TLS：--tls-cert/--tls-key 指定证书与私钥；如果确实要在内网用明文 HTTP，加 --allow-plain-http 明确确认（token 会以明文传输）。");
    }
    if !tls {
        eprintln!("⚠ 警告：MCP HTTP 服务端在非回环地址 {addr} 上用明文 HTTP 运行（--allow-plain-http），bearer token 以明文传输；建议配置 --tls-cert/--tls-key。");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_bind_requires_token_and_tls_off_loopback() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let loopback6: SocketAddr = "[::1]:8080".parse().unwrap();
        let external: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let lan: SocketAddr = "192.168.1.10:8080".parse().unwrap();
        // Loopback: anything goes.
        assert!(check_http_bind(loopback, false, false, false).is_ok());
        assert!(check_http_bind(loopback6, false, false, false).is_ok());
        assert!(check_http_bind(loopback, true, true, false).is_ok());
        // Non-loopback without a token: refused, TLS or not.
        assert!(check_http_bind(external, false, true, false).is_err());
        assert!(check_http_bind(lan, false, true, true).is_err());
        // Non-loopback with a token but no TLS: needs the explicit
        // --allow-plain-http opt-out.
        assert!(check_http_bind(external, true, false, false).is_err());
        assert!(check_http_bind(lan, true, false, true).is_ok());
        // Non-loopback with token + TLS: fine.
        assert!(check_http_bind(external, true, true, false).is_ok());
        assert!(check_http_bind(lan, true, true, false).is_ok());
    }
}
