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
//! additionally needs a bearer token off loopback (per-client tokens
//! from `hatch mcp token create`, or the legacy shared token in vault
//! `mcp-http` / `HATCH_MCP_TOKEN`); see `docs/SECURITY.md` §9d. MCP client tools
//! (`mcp_*`) and skill-management tools are deliberately not exposed.

use super::mcp_auth::{check_http_bind, resolve_serve_token};
use super::mcp_token::McpTokenCmd;
use crate::app::App;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_mcp::{
    is_read_only_tool, registry_path, AnyClient, ClientRegistry, HttpServer, HttpServerConfig,
    McpServerConfig, OAuthSession, Server,
};
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::{add_general_tools, default_registry};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Subcommand)]
pub enum McpCmd {
    /// 列出配置的 MCP 服务器（启用的会尝试连接并显示工具数）
    List,
    /// 按客户端 token 管理（HTTP 服务端的鉴权）：每个客户端一个命名
    /// token，带作用域，可单独撤销、立即生效。token 明文只在创建时
    /// 显示一次，注册表（<home>/mcp-clients.json，0600）里只存哈希。
    Token {
        #[command(subcommand)]
        cmd: McpTokenCmd,
    },
    /// 连接一个服务器：ping + 列出工具/资源/提示词
    Test { server: String },
    /// 登录一个需要 OAuth 的远程 MCP 服务器（PKCE + loopback 回调）：
    /// 打印授权地址，在浏览器打开登录，回调后 token 存进
    /// `<home>/mcp-oauth/<server>/tokens.json`（0600）。登录一次后
    /// `test`/`call` 与对话里的工具调用会自动刷新 token。
    Login { server: String },
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
        /// TLS 证书（PEM 链），与 --tls-key 成对出现；HTTP 传输时在
        /// 进程内终止 TLS。非回环绑定默认要求 TLS + token。
        #[arg(long)]
        tls_cert: Option<String>,
        /// TLS 私钥（PEM），与 --tls-cert 成对出现
        #[arg(long)]
        tls_key: Option<String>,
        /// 明确允许非回环绑定不用 TLS（明文 HTTP；启动时打印警告）
        #[arg(long)]
        allow_plain_http: bool,
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
    // OAuth 服务器走 OAuth 会话（token_ref 对它无效）；其余走 vault。
    if let Some(oauth_cfg) = &cfg.oauth {
        let session = std::sync::Arc::new(
            OAuthSession::open(&app.home, &cfg.name, oauth_cfg, &cfg.url)
                .map_err(|e| anyhow!("{e}"))?,
        );
        return AnyClient::connect_oauth(cfg, &headers_map, session).map_err(|e| anyhow!("{e}"));
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
        McpCmd::Token { cmd } => super::mcp_token::run_token(&app.home, cmd)?,
        McpCmd::Login { server } => {
            let cfg = find_server(&app, &server)?;
            let oauth_cfg = cfg.oauth.clone().ok_or_else(|| {
                anyhow!(
                    "MCP 服务器 {server} 没有配置 OAuth（config.toml 的 [[mcp_servers]] 里加 [mcp_servers.oauth]，填 client_id 与 scopes）"
                )
            })?;
            if cfg.transport != "http" {
                bail!("MCP 服务器 {server} 的 OAuth 需要 http 传输（当前：{}）", cfg.transport);
            }
            let session = OAuthSession::open(&app.home, &cfg.name, &oauth_cfg, &cfg.url)
                .map_err(|e| anyhow!("{e}"))?;
            let (url, pending) = session.begin_login().map_err(|e| anyhow!("{e}"))?;
            println!("在浏览器打开以下地址登录 {server}：\n{url}");
            println!("（等待浏览器回调，最长 3 分钟；登录完成后可以关闭浏览器标签页。）");
            session.finish_login(pending).map_err(|e| anyhow!("{e}"))?;
            println!(
                "MCP 服务器 {server} 登录成功，token 已保存（<home>/mcp-oauth/{server}/tokens.json，0600）。"
            );
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
            tls_cert,
            tls_key,
            allow_plain_http,
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
                // HTTP 传输：基础审批 origin（mcp:http）与协议版本
                // （2025-03-26），门禁/审批路径与 stdio 完全相同。
                // 按客户端 token 的请求在 HTTP 层把 origin 扩展为
                // `mcp:http:<客户端名>`（见 hatch-mcp 的 authenticate）。
                let server = server
                    .with_origin(hatch_mcp::MCP_HTTP_ORIGIN)
                    .with_protocol_version(hatch_mcp::SERVER_HTTP_PROTOCOL_VERSION);
                let addr: SocketAddr = bind
                    .parse()
                    .map_err(|e| anyhow!("--bind 不是合法地址：{e}（如 127.0.0.1:8080）"))?;
                let reg_path = registry_path(&app.home);
                let registry =
                    ClientRegistry::load_if_present(&reg_path).map_err(|e| anyhow!("{e}"))?;
                let active_clients = registry.as_ref().map(|r| r.active_count()).unwrap_or(0);
                let token = resolve_serve_token(&app.home);
                let has_token = token.is_some() || active_clients > 0;
                let tls = tls_cert.is_some() || tls_key.is_some();
                check_http_bind(addr, has_token, tls, allow_plain_http)?;
                let config = HttpServerConfig {
                    bind: addr,
                    token,
                    registry_path: Some(reg_path),
                    tls_cert: tls_cert.map(PathBuf::from),
                    tls_key: tls_key.map(PathBuf::from),
                    ..Default::default()
                };
                let http = HttpServer::bind(config, Arc::new(server))
                    .map_err(|e| anyhow!("绑定 {addr} 失败：{e}"))?;
                let actual = http.local_addr().map_err(|e| anyhow!("{e}"))?;
                let auth_summary = if active_clients > 0 {
                    format!("按客户端 token 认证（{active_clients} 个客户端）")
                } else if has_token {
                    "共享 token 认证（过渡方案，建议迁移到按客户端 token）".to_string()
                } else {
                    "未设 token，仅回环地址可达".to_string()
                };
                eprintln!(
                    "hatch MCP 服务器已启动（HTTP streamable，协议 2025-03-26；监听 {actual}；{}；{auth_summary}；Ctrl-C/SIGTERM 时退出）。",
                    if http.tls_enabled() {
                        "TLS 已开启"
                    } else {
                        "明文 HTTP"
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
