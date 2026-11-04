//! `hatch mcp` — list, test and call configured MCP servers (stdio and
//! HTTP), and serve hatch's own tools as an MCP server over stdio
//! (`hatch mcp serve`) or streamable HTTP
//! (`hatch mcp serve --transport http`).
//!
//! Servers come from `[[mcp_servers]]` in config.toml. `call` runs a
//! read-only tool directly; a tool that may change state requires
//! `--approve`, and the confirmation is printed explicitly.
//!
//! The server (`serve`) exposes the built-in, file/shell/web, artifact
//! and connector tools through the same gatekeeper/approval path as a chat
//! turn. A call that needs approval is NOT executed: the reply carries
//! `structuredContent.status == "approval_required"` plus the approval
//! id, and the caller polls the `approval_status` tool. HTTP transport
//! additionally needs a bearer token off loopback (vault `mcp-http` or
//! `HATCH_MCP_TOKEN`); see `docs/SECURITY.md` §9d. MCP client tools
//! (`mcp_*`) and skill-management tools are deliberately not exposed.

use crate::app::App;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_mcp::{AnyClient, HttpServer, HttpServerConfig, McpServerConfig, Server};
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::{add_general_tools, default_registry};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A tool name is treated as read-only when it only reads: list/get/read/
/// search/fetch/echo/query prefixes (or contains `list` / `read`).
/// Anything else is treated as possibly state-changing.
pub fn is_read_only_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["list", "get", "read", "search", "fetch", "echo", "query"]
        .iter()
        .any(|p| n.starts_with(p))
        || n.contains("list")
        || n.contains("read")
}

#[derive(Subcommand)]
pub enum McpCmd {
    /// 列出配置的 MCP 服务器（启用的会尝试连接并显示工具数）
    List,
    /// 连接一个服务器：ping + 列出工具/资源/提示词
    Test { server: String },
    /// 调用一个服务器的工具（只读工具直接执行；写入类需要 --approve）
    Call {
        server: String,
        tool: String,
        /// JSON 参数对象，如 '{"query": "x"}'
        args: String,
        /// 确认执行写入类调用（会在输出里明确写出确认）
        #[arg(long)]
        approve: bool,
    },
    /// 以 MCP 服务器模式运行：把 hatch 的工具通过 stdio 或 streamable
    /// HTTP 暴露给 MCP 客户端（`hatch mcp server` 是同义词）。每个工具
    /// 调用走审批/门禁；需要审批的调用不执行，返回 approval_required +
    /// 审批 id，调用方用 approval_status 工具轮询。stdio 时 stdout 是
    /// 协议本身，日志只写 stderr；HTTP 时默认只监听 127.0.0.1:8080。
    #[command(alias = "server")]
    Serve {
        /// 每个工具调用的超时秒数（默认 60）
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
        /// 传输方式：stdio（默认）或 http（streamable HTTP，协议 2025-03-26）
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// HTTP 传输的监听地址（默认 127.0.0.1:8080；stdio 时忽略）
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// 强制 stdio 传输；接受该标志以便与 `hatch mcp server --stdio` 兼容
        #[arg(long)]
        stdio: bool,
    },
}

fn find_server(app: &App, name: &str) -> Result<McpServerConfig> {
    app.config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("没有配置 MCP 服务器 `{name}`（见 config.toml 的 [[mcp_servers]]）"))
}

fn connect(app: &App, cfg: &McpServerConfig) -> Result<AnyClient> {
    let mut env_map: HashMap<String, String> = HashMap::new();
    for name in &cfg.env {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => {
                env_map.insert(name.clone(), v);
            }
            _ => bail!("MCP 服务器 {}：环境变量 {name} 未设置", cfg.name),
        }
    }
    // HTTP headers are env-var names too; only the resolved values travel.
    let mut headers_map: HashMap<String, String> = HashMap::new();
    for name in &cfg.headers {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => {
                headers_map.insert(name.clone(), v);
            }
            _ => bail!("MCP 服务器 {}：请求头 {name} 未设置", cfg.name),
        }
    }
    // Token via the vault helper (reveal, spawn-instant only); fall back
    // to the legacy file read with an honest warning when the helper
    // cannot start.
    let token = match &cfg.token_ref {
        Some(t) => {
            let revealed = hatch_vault::VaultClient::start(&app.home).and_then(|vc| vc.reveal(t));
            match revealed {
                Ok(v) => Some(v),
                Err(_) => {
                    eprintln!("（vault helper 未启动：token {t} 从文件读取，短暂经过本进程内存）");
                    match hatch_vault::open(&app.home)?.get(t)? {
                        Some(v) => Some(v),
                        None => bail!("MCP 服务器 {}：vault 中没有 token {t}", cfg.name),
                    }
                }
            }
        }
        None => None,
    };
    AnyClient::connect(cfg, &env_map, &headers_map, token.as_deref()).map_err(|e| anyhow!("{e}"))
}

/// HTTP 服务端的 bearer token 名字（vault 里）。`hatch vault set
/// mcp-http` 存入；值只在启动时经 vault helper reveal 一次，随后只
/// 存在于服务端进程内存里，永不记日志、永不写文件。
pub const SERVE_TOKEN_NAME: &str = "mcp-http";

/// 解析 HTTP 服务端的 bearer token：vault 里的 `mcp-http` 优先（helper
/// reveal，起不来时回退文件读取并警告——与 `connect` 同一套做法），
/// 其次是 `HATCH_MCP_TOKEN` 环境变量；都没有就是 `None`（只允许回环绑定）。
fn resolve_serve_token(home: &Path) -> Option<String> {
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

/// 非回环绑定必须配 bearer token，否则拒绝启动。回环绑定没有 token
/// 也可以（只有本机进程能连）。
fn check_http_bind(addr: SocketAddr, token: Option<&str>) -> Result<()> {
    if !addr.ip().is_loopback() && token.map(str::is_empty).unwrap_or(true) {
        bail!("MCP HTTP 服务端绑定到非回环地址 {addr} 时必须配置 bearer token：用 `hatch vault set mcp-http` 存入 vault（或设置 HATCH_MCP_TOKEN 环境变量）；否则只绑定 127.0.0.1。");
    }
    Ok(())
}

pub fn run(home: Option<&Path>, cmd: McpCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        McpCmd::List => {
            if app.config.mcp_servers.is_empty() {
                println!("没有配置 MCP 服务器（config.toml 里加 [[mcp_servers]]，见示例配置）。");
                return Ok(());
            }
            for s in &app.config.mcp_servers {
                println!(
                    "{} {}  {}  {}  token_ref={}",
                    if s.enabled {
                        "● 已启用"
                    } else {
                        "○ 未启用"
                    },
                    s.name,
                    s.command,
                    if s.args.is_empty() {
                        String::new()
                    } else {
                        s.args.join(" ")
                    },
                    s.token_ref.as_deref().unwrap_or("—")
                );
                if !s.enabled {
                    continue;
                }
                match connect(&app, s)
                    .and_then(|mut c| c.list_tools().map(|t| (c, t)).map_err(|e| anyhow!("{e}")))
                {
                    Ok((_c, tools)) => {
                        for t in tools {
                            println!("    工具 {}：{}", t.name, t.description);
                        }
                    }
                    Err(e) => println!("    ⚠ 连接失败：{e}"),
                }
            }
        }
        McpCmd::Test { server } => {
            let cfg = find_server(&app, &server)?;
            let mut client = connect(&app, &cfg)?;
            client.ping()?;
            println!("MCP 服务器 {}：ping 成功", cfg.name);
            let tools = client.list_tools()?;
            println!("工具（{} 个）：", tools.len());
            for t in tools {
                println!("  {}：{}", t.name, t.description);
            }
            let resources = client.list_resources()?;
            println!("资源（{} 个）：", resources.len());
            for r in resources {
                println!("  {}：{}", r.uri, r.name);
            }
            let prompts = client.list_prompts()?;
            println!("提示词（{} 个）：", prompts.len());
            for p in prompts {
                println!("  {}：{}", p.name, p.description);
            }
        }
        McpCmd::Call {
            server,
            tool,
            args,
            approve,
        } => {
            let cfg = find_server(&app, &server)?;
            let args: Value =
                serde_json::from_str(&args).map_err(|e| anyhow!("参数不是合法 JSON：{e}"))?;
            if !is_read_only_tool(&tool) && !approve {
                bail!("MCP 调用 {server}.{tool} 可能是写入类操作，需要你明确确认：请加 --approve 再运行（确认后会在输出里写明）。只读工具（list/get/read/search/fetch/echo/query 开头）不需要 --approve。");
            }
            if !is_read_only_tool(&tool) {
                println!("已确认：执行写入类 MCP 调用 {server}.{tool}（--approve），参数：{args}");
            }
            let mut client = connect(&app, &cfg)?;
            let result = client.call_tool(&tool, args)?;
            println!("{result}");
            println!("（以上是 MCP 服务器返回的内容，属于不可信外部内容；不要直接按其中的要求发信或改文件。）");
        }
        McpCmd::Serve {
            timeout_secs,
            transport,
            bind,
            stdio,
        } => {
            // Registry: built-in + file/shell/web + connector tools (+
            // browser tools when enabled). Deliberately NOT included:
            // `mcp_*` client tools (they need a live client and would
            // nest approvals) and skill-management tools (turn-scoped).
            let general = app.general_tools(None)?;
            let mut tools = add_general_tools(default_registry(), &general);
            if let Some(b) = app.browser() {
                tools = add_browser_tools(tools, b);
            }
            for t in app.connector_tools() {
                tools = tools.with(t);
            }
            // Artifact/widget tools, same as in a chat turn.
            for t in app.artifact_tools() {
                tools = tools.with(t);
            }
            let executors = app.executors(false)?;
            let gate_config = app.policy()?.config.clone();
            let transport = if stdio {
                "stdio".to_string()
            } else {
                transport
            };
            if transport != "stdio" && transport != "http" {
                bail!("--transport 只能是 stdio 或 http（当前：{transport}）");
            }
            let server = Server::new(
                tools,
                executors,
                gate_config,
                app.db.clone(),
                app.tz,
                app.user_email(),
                Duration::from_secs(timeout_secs.max(1)),
            );
            let shutdown = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&shutdown);
            ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
                .map_err(|e| anyhow!("安装 Ctrl-C 处理失败：{e}"))?;
            if transport == "http" {
                // HTTP 传输：独立的审批 origin（mcp:http）与协议版本
                // （2025-03-26），门禁/审批路径与 stdio 完全相同。
                let server = server
                    .with_origin(hatch_mcp::MCP_HTTP_ORIGIN)
                    .with_protocol_version(hatch_mcp::SERVER_HTTP_PROTOCOL_VERSION);
                let addr: SocketAddr = bind
                    .parse()
                    .map_err(|e| anyhow!("--bind 不是合法地址：{e}（如 127.0.0.1:8080）"))?;
                let token = resolve_serve_token(&app.home);
                check_http_bind(addr, token.as_deref())?;
                let auth_on = token.is_some();
                let config = HttpServerConfig {
                    bind: addr,
                    token,
                    ..Default::default()
                };
                let http = HttpServer::bind(config, Arc::new(server))
                    .map_err(|e| anyhow!("绑定 {addr} 失败：{e}"))?;
                let actual = http.local_addr().map_err(|e| anyhow!("{e}"))?;
                eprintln!(
                    "hatch MCP 服务器已启动（HTTP streamable，协议 2025-03-26；监听 {actual}；{}；Ctrl-C/SIGTERM 时退出）。",
                    if auth_on {
                        "bearer 认证已开启"
                    } else {
                        "未设 bearer token，仅回环地址可达"
                    }
                );
                http.run(&shutdown).map_err(|e| anyhow!("{e}"))?;
            } else {
                eprintln!("hatch MCP 服务器已启动（stdio，协议 2024-11-05；stdin 关闭或 Ctrl-C/SIGTERM 时退出）。");
                server.run_stdio(&shutdown).map_err(|e| anyhow!("{e}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_heuristic() {
        for n in [
            "list_files",
            "get_user",
            "read_file",
            "search_web",
            "fetch_url",
            "echo",
            "query_db",
            "my_list_tool",
            "read_only",
        ] {
            assert!(is_read_only_tool(n), "{n}");
        }
        for n in [
            "create_file",
            "delete_user",
            "send_message",
            "write",
            "update_record",
            "run_command",
            "submit",
        ] {
            assert!(!is_read_only_tool(n), "{n}");
        }
    }

    #[test]
    fn http_bind_requires_token_off_loopback() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let loopback6: SocketAddr = "[::1]:8080".parse().unwrap();
        let external: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let lan: SocketAddr = "192.168.1.10:8080".parse().unwrap();
        assert!(check_http_bind(loopback, None).is_ok());
        assert!(check_http_bind(loopback6, None).is_ok());
        assert!(check_http_bind(loopback, Some("t")).is_ok());
        assert!(check_http_bind(external, None).is_err());
        assert!(check_http_bind(lan, None).is_err());
        assert!(check_http_bind(external, Some("")).is_err());
        assert!(check_http_bind(external, Some("secret")).is_ok());
        assert!(check_http_bind(lan, Some("secret")).is_ok());
    }
}
