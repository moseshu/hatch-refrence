//! `hatch daemon` — run scheduled jobs in the foreground; status; stop.

use crate::app::App;
use crate::sched;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_core::CancelToken;
use hatch_scheduler::daemon::{request_stop, status};
use hatch_scheduler::{Daemon, DaemonConfig, JobStore, SystemClock};
use std::path::Path;
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
    },
    /// 查看 daemon 与任务状态
    Status,
    /// 请求正在运行的 daemon 停止
    Stop {
        /// 最多等待多少秒
        #[arg(long, default_value_t = 30)]
        wait_secs: u64,
    },
}

pub fn run(home: Option<&Path>, cmd: DaemonCmd) -> Result<()> {
    let app = App::open(home)?;
    let paths = sched::daemon_paths(&app);
    match cmd {
        DaemonCmd::Run { tick_secs, once } => {
            let engine = sched::engine(&app)?;
            if sched::router_factory(&app).is_none() {
                eprintln!("提示：没有配置模型。每日简报会只列出事项原文，邮件分诊任务会失败。");
            }
            let tick = Duration::from_secs(tick_secs.unwrap_or(app.config.scheduler.tick_secs).max(1));
            let enabled = JobStore::new(&app.db).list()?.iter().filter(|j| j.enabled).count();
            eprintln!("hatch daemon 启动：pid {}，每 {}s 检查一次，已启用任务 {enabled} 个。停止：Ctrl-C、SIGTERM 或 `hatch daemon stop`。", std::process::id(), tick.as_secs());
            let d = Daemon {
                engine: &engine,
                db: &app.db,
                paths,
                config: DaemonConfig { tick, max_ticks: once.then_some(1), use_signals: true },
                clock: &SystemClock,
            };
            let exit = d.run(&CancelToken::new())?;
            eprintln!("hatch daemon 已退出：{}（检查 {} 次，运行任务 {} 次）", exit.reason, exit.ticks, exit.runs);
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
                println!("  最近启动 {}；最近检查 {}；最近停止 {}", s.started_at, if s.last_tick.is_empty() { "—" } else { &s.last_tick }, if s.stopped_at.is_empty() { "—" } else { &s.stopped_at });
            }
            println!();
            crate::commands::jobs::run(home, crate::commands::jobs::JobsCmd::List)?;
        }
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
            bail!("{wait_secs} 秒内 daemon 没有退出（pid {pid}）；它会在当前任务的下一个取消点停下");
        }
    }
    Ok(())
}
