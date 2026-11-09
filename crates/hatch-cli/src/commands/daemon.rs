//! `hatch daemon` — run scheduled jobs in the foreground; status; stop.
//!
//! `daemon run --api` also serves the product API (`/v1/...`) on its
//! own thread, bound before the tick loop starts so a bad bind or a
//! non-loopback bind without a token fails fast. The API thread shuts
//! down when the daemon exits; `--once` never starts it.

use crate::api::auth::{check_api_bind, resolve_api_token};
use crate::api::client_auth::{registry_path, DaemonRegistry};
use crate::api::server::{ctx_for, ApiServer, ApiServerConfig};
use crate::commands::daemon_token::DaemonTokenCmd;
use crate::app::App;
use crate::sched;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_core::CancelToken;
use hatch_scheduler::daemon::{request_stop, status};
use hatch_scheduler::{Daemon, DaemonConfig, JobStore, SystemClock};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// 前台常驻运行（systemd / tmux 中使用）；SIGTERM 或 Ctrl-C 优雅退出
    Run {
        /// 两次检查之间的秒数（默认取配置 [scheduler] tick_secs）
        #[arg(long)]
        tick_secs: Option<u64>,
        /// 只检查一次到期任务后退出
        #[arg(long)]
        once: bool,
        /// 同时提供产品 API（/v1/...，默认绑 127.0.0.1:8787；也可由配置 [daemon] http_enabled 开启）
        #[arg(long)]
        api: bool,
        /// 产品 API 的监听地址（覆盖配置 [daemon] http_bind）
        #[arg(long)]
        api_bind: Option<String>,
    },
    /// 查看 daemon 与任务状态
    Status,
    /// 按客户端 token 管理（产品 API 的鉴权）
    Token {
        #[command(subcommand)]
        cmd: DaemonTokenCmd,
    },
    /// 请求正在运行的 daemon 停止
    Stop {
        /// 最多等待多少秒
        #[arg(long, default_value_t = 30)]
        wait_secs: u64,
    },
}

/// Resolve the API bind address + token, bind the server (fail fast),
/// and spawn its serve thread. Returns the shutdown flag and the
/// thread handle; the caller sets the flag and joins when the daemon
/// exits.
fn start_api(
    app: &Arc<App>,
    api_bind: Option<String>,
) -> Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    let bind_str = api_bind.unwrap_or_else(|| app.config.daemon.http_bind.clone());
    let bind: SocketAddr = bind_str.parse().map_err(|_| {
        anyhow!("产品 API 监听地址无效：{bind_str}（应为 127.0.0.1:8787 这样的地址）")
    })?;
    let token = resolve_api_token(&app.home);
    // Per-client tokens count as a credential for the non-loopback
    // bind rule, same as the legacy shared token. A corrupt registry
    // fails startup closed instead of silently serving without it.
    let reg_path = registry_path(&app.home);
    let registry_clients = match DaemonRegistry::load_if_present(&reg_path) {
        Ok(Some(reg)) => reg.active_count(),
        Ok(None) => 0,
        Err(e) => {
            return Err(anyhow!("daemon token 注册表不可读：{e}"));
        }
    };
    let has_shared_token = token.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
    check_api_bind(bind, has_shared_token || registry_clients > 0)?;
    let server = ApiServer::bind(
        ApiServerConfig {
            bind,
            token: token.clone(),
            registry_path: Some(reg_path),
            rate_limit_per_sec: 20,
        },
        ctx_for(app.clone()),
    )?;
    let addr = server.local_addr()?;
    let auth_note = if token.is_some() {
        "已配置共享 bearer token".to_string()
    } else if registry_clients > 0 {
        format!("已配置 {registry_clients} 个客户端 token")
    } else {
        "未配置 token，仅回环可达".to_string()
    };
    eprintln!("产品 API 已启动：http://{addr}/v1（{auth_note}）");
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let handle = std::thread::spawn(move || {
        if let Err(e) = server.run(&flag) {
            eprintln!("产品 API 异常退出：{e:#}");
        }
    });
    Ok((shutdown, handle))
}

pub fn run(home: Option<&Path>, cmd: DaemonCmd) -> Result<()> {
    let app = Arc::new(App::open(home)?);
    let paths = sched::daemon_paths(&app);
    match cmd {
        DaemonCmd::Run {
            tick_secs,
            once,
            api,
            api_bind,
        } => {
            // P1 D6: reconcile tasks a previous process left in `running`
            // (stale heartbeat → waiting, or failed when retries are out),
            // before the first tick. Best-effort: a DB error never stops
            // the daemon from starting.
            let reconciled = hatch_tasks::TaskStore::new(&app.db)
                .reconcile(&hatch_core::time::now(), hatch_tasks::DEFAULT_STALE_AFTER_SECS)
                .unwrap_or_default();
            if reconciled.changed() {
                eprintln!(
                    "任务账本：启动时收敛 {} 个未完成任务（{} 个放回等待，{} 个标记失败）。",
                    reconciled.revived + reconciled.failed,
                    reconciled.revived,
                    reconciled.failed
                );
            }
            if once && api {
                bail!("--once 只检查一次后退出，无法提供常驻的产品 API；去掉 --once 再用 --api。");
            }
            let api_enabled = api || app.config.daemon.http_enabled;
            if once && app.config.daemon.http_enabled {
                eprintln!("提示：--once 单次运行不启动产品 API（配置 [daemon] http_enabled 被忽略）。");
            }
            let api_handle = if api_enabled && !once {
                Some(start_api(&app, api_bind)?)
            } else {
                None
            };
            let engine = sched::engine(&app)?;
            if sched::router_factory(&app).is_none() {
                eprintln!("提示：没有配置模型。每日简报会只列出事项原文，邮件分诊任务会失败。");
            }
            let tick =
                Duration::from_secs(tick_secs.unwrap_or(app.config.scheduler.tick_secs).max(1));
            let enabled = JobStore::new(&app.db)
                .list()?
                .iter()
                .filter(|j| j.enabled)
                .count();
            eprintln!("hatch daemon 启动：pid {}，每 {}s 检查一次，已启用任务 {enabled} 个。停止：Ctrl-C、SIGTERM 或 `hatch daemon stop`。", std::process::id(), tick.as_secs());
            let d = Daemon {
                engine: &engine,
                db: &app.db,
                paths,
                config: DaemonConfig {
                    tick,
                    max_ticks: once.then_some(1),
                    use_signals: true,
                },
                clock: &SystemClock,
            };
            let exit = d.run(&CancelToken::new())?;
            if let Some((shutdown, handle)) = api_handle {
                shutdown.store(true, Ordering::SeqCst);
                let _ = handle.join();
            }
            eprintln!(
                "hatch daemon 已退出：{}（检查 {} 次，运行任务 {} 次）",
                exit.reason, exit.ticks, exit.runs
            );
        }
        DaemonCmd::Status => {
            let s = status(&app.db, &paths)?;
            let state = match (s.alive, s.pid) {
                (true, Some(p)) => format!("运行中（pid {p}）"),
                (false, Some(p)) => format!("未运行（锁文件残留 pid {p}，下次启动会自动清理）"),
                _ => "未运行".to_string(),
            };
            println!("daemon：{state}");
            if !s.started_at.is_empty() {
                println!(
                    "  最近启动 {}；最近检查 {}；最近停止 {}",
                    s.started_at,
                    if s.last_tick.is_empty() {
                        "—"
                    } else {
                        &s.last_tick
                    },
                    if s.stopped_at.is_empty() {
                        "—"
                    } else {
                        &s.stopped_at
                    }
                );
            }
            println!();
            crate::commands::jobs::run(home, crate::commands::jobs::JobsCmd::List)?;
        }
        DaemonCmd::Token { cmd } => crate::commands::daemon_token::run_token(&app, cmd)?,
        DaemonCmd::Stop { wait_secs } => {
            let pid = request_stop(&paths)?;
            let Some(pid) = pid else {
                let _ = std::fs::remove_file(&paths.stop);
                println!("daemon 没有在运行。");
                return Ok(());
            };
            println!("已请求 daemon（pid {pid}）停止，等待当前任务在取消点结束…");
            let deadline = Instant::now() + Duration::from_secs(wait_secs);
            while Instant::now() < deadline {
                if !paths.lock.exists() {
                    println!("daemon 已停止。");
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            bail!(
                "{wait_secs} 秒内 daemon 没有退出（pid {pid}）；它会在当前任务的下一个取消点停下"
            );
        }
    }
    Ok(())
}
