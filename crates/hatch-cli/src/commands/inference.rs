//! Inference proxy commands: run the proxy, show proxied usage.
//!
//! The proxy is off by default. To use it, set `[inference]
//! proxy_url` (e.g. `http://127.0.0.1:8199` — no `/v1`: the
//! provider client appends `/responses` itself) so the agent routes
//! model calls through it, then start it with
//! `hatch inference proxy` (upstream + key from `[inference]` and
//! the environment).

use crate::app::App;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use hatch_inference::ledger::{read_all, summarize};
use hatch_inference::{ledger_path, ProxyConfig, ProxyServer};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Subcommand)]
pub enum InferenceCmd {
    /// Run the inference proxy (loopback only, Ctrl-C stops it).
    Proxy {
        /// Listen address (default: [inference] bind, else 127.0.0.1:8199).
        #[arg(long)]
        bind: Option<String>,
        /// Upstream base URL (default: [inference] upstream_base_url).
        #[arg(long)]
        upstream: Option<String>,
        /// Env var holding the upstream API key (default: [inference]
        /// api_key_env, else HATCH_LLM_API_KEY).
        #[arg(long)]
        api_key_env: Option<String>,
    },
    /// Show proxied usage totals (tokens + estimated cost).
    Usage {
        /// Only this day (YYYY-MM-DD); default is all days.
        #[arg(long)]
        day: Option<String>,
        /// Only this model; default is all models.
        #[arg(long)]
        model: Option<String>,
    },
}

pub fn run(home: Option<&Path>, cmd: InferenceCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        InferenceCmd::Proxy {
            bind,
            upstream,
            api_key_env,
        } => run_proxy(&app, bind, upstream, api_key_env),
        InferenceCmd::Usage { day, model } => show_usage(&app, day.as_deref(), model.as_deref()),
    }
}

fn run_proxy(
    app: &App,
    bind: Option<String>,
    upstream: Option<String>,
    api_key_env: Option<String>,
) -> Result<()> {
    let cfg = &app.config.inference;
    let bind: SocketAddr = bind
        .unwrap_or_else(|| cfg.bind().to_string())
        .parse()
        .map_err(|e| anyhow!("--bind 不是合法地址：{e}（如 127.0.0.1:8199）"))?;
    let upstream = upstream
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| cfg.upstream_base_url.clone());
    if upstream.trim().is_empty() {
        anyhow::bail!(
            "没有配置上游地址。请在 config.toml 的 [inference] 里写 upstream_base_url，或传 --upstream"
        );
    }
    let key_env = api_key_env.unwrap_or_else(|| cfg.api_key_env().to_string());
    let api_key = std::env::var(&key_env).ok().filter(|v| !v.is_empty());
    if api_key.is_none() {
        eprintln!("提示：环境变量 {key_env} 未设置，代理将原样转发客户端的 Authorization（上游可能返回 401）。");
    }
    let mut proxy = ProxyConfig::new(bind, upstream.clone(), ledger_path(&app.home));
    proxy.api_key = api_key;
    proxy.max_retries = cfg
        .max_retries
        .unwrap_or(hatch_inference::config::DEFAULT_MAX_RETRIES);
    proxy.max_requests_per_min = cfg
        .rate_limit_per_min
        .unwrap_or(hatch_inference::config::DEFAULT_RATE_LIMIT_PER_MIN);
    proxy.price_input_per_million = cfg.price_input_per_million;
    proxy.price_output_per_million = cfg.price_output_per_million;
    proxy.currency = cfg.currency().to_string();
    proxy.validate().map_err(|e| anyhow!(e))?;
    let server = ProxyServer::bind(proxy).context("启动 inference proxy 失败")?;
    let addr = server.local_addr()?;
    println!("inference proxy 监听 {addr} → 上游 {upstream}（Ctrl-C 停止）");
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .map_err(|e| anyhow!("安装 Ctrl-C 处理失败：{e}"))?;
    server.run(&shutdown).context("inference proxy 运行失败")?;
    println!("inference proxy 已停止。");
    Ok(())
}

fn show_usage(app: &App, day: Option<&str>, model: Option<&str>) -> Result<()> {
    let path = ledger_path(&app.home);
    let (entries, skipped) = read_all(&path);
    if entries.is_empty() {
        println!("暂无推理用量记录（代理未运行或尚未有调用）。");
        return Ok(());
    }
    let pricing = app.config.inference.pricing();
    let s = summarize(&entries, day, model, &pricing);
    let scope = match (day, model) {
        (Some(d), Some(m)) => format!("{d} · {m}"),
        (Some(d), None) => d.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => "全部".to_string(),
    };
    println!(
        "推理用量 [{scope}]（{} 次调用，成功 {}）",
        s.calls, s.ok_calls
    );
    println!(
        "  prompt {} tokens（缓存 {}）  completion {} tokens  reasoning {}",
        s.prompt_tokens, s.cached_tokens, s.completion_tokens, s.reasoning_tokens
    );
    match s.cost {
        Some(c) => println!(
            "  费用估算：{c:.4} {}（按配置单价算；缓存 token 按输入价计，未计折扣）",
            s.currency
        ),
        None => println!(
            "  费用：未配置单价（[inference] price_input_per_million / price_output_per_million）"
        ),
    }
    if s.per_model.len() > 1 {
        println!("  按模型：");
        for (m, t) in &s.per_model {
            println!(
                "    {m}  {} 次  prompt {}  completion {}",
                t.calls, t.prompt_tokens, t.completion_tokens
            );
        }
    }
    if skipped > 0 {
        println!("  （跳过 {skipped} 行损坏的记账记录）");
    }
    Ok(())
}
