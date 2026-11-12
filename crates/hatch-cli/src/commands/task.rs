//! `hatch task` — durable tasks (M0, item 6).
//!
//! - `task create` — create a meeting-brief task (queued).
//! - `task show <id>` — task row + steps + recent events.
//! - `task events <id> [--after <seq>] [--limit <n>]` — the event log
//!   from a cursor (a reconnecting client resumes here).
//! - `task run --world <dir> --out <dir> [--task <id>]` — run the
//!   worker on this host's database until the task completes. The
//!   client may exit while it runs: the task lives in the database,
//!   and `task show` / `task events` read its progress back.

use super::task_worker::{run_worker, WorkerOpts};
use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_core::time::now as now_ts;
use hatch_tasks::{EventLog, TaskStatus, TaskStore, UnitStore};
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum TaskCmd {
    /// Create a meeting-brief task (queued, not yet claimed).
    Create {
        /// Human-readable title.
        #[arg(long)]
        title: String,
    },
    /// Show a task: status, lease, steps, recent events.
    Show {
        /// Task id (`tsk_…`).
        id: String,
    },
    /// List events for a task from a sequence cursor.
    Events {
        /// Task id (`tsk_…`).
        id: String,
        /// Only events with `seq` greater than this (default 0).
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum events to return (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Run the worker for a task (or a fresh meeting-brief task).
    Run {
        /// Eval world directory (e.g. `eval/worlds/meeting-brief`).
        #[arg(long)]
        world: PathBuf,
        /// Output directory for the brief artifact.
        #[arg(long)]
        out: PathBuf,
        /// Resume this task instead of creating a new one.
        #[arg(long)]
        task: Option<String>,
        /// Lease TTL in seconds (default 300).
        #[arg(long, default_value_t = 300)]
        lease_ttl_secs: u64,
    },
}

pub fn run(home: Option<&Path>, cmd: TaskCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = TaskStore::new(&app.db);
    let now = now_ts();
    match cmd {
        TaskCmd::Create { title } => {
            let task = store.create("meeting-brief", &title, "cli", "", 3, None, &now)?;
            println!("task {}  queued", task.id);
            println!("kind: {}", task.kind);
            println!("title: {}", task.title);
            Ok(())
        }
        TaskCmd::Show { id } => {
            let task = store.get(&id)?.ok_or_else(|| anyhow!("task {id} 不存在"))?;
            println!("task {}  {}", task.id, status_cn(&task.status));
            println!("kind: {}  title: {}", task.kind, task.title);
            println!(
                "retries: {}/{}  error: {}",
                task.retries, task.max_retries, task.error
            );
            if let Some(lease) = task.lease() {
                println!(
                    "lease: {} (generation {}, expires {})",
                    lease.owner, lease.generation, task.lease_expires_at
                );
            }
            if !task.blocked_kind.as_str().is_empty() {
                println!("blocked: {}", task.blocked_kind.as_str());
            }
            let units = UnitStore::new(&app.db);
            for step in units.list_steps(&id)? {
                println!(
                    "  step {:<9} {:<8} attempts={} {}",
                    step.step_id,
                    step.status.as_str(),
                    step.attempts,
                    step.output
                );
            }
            let log = EventLog::new(&app.db);
            for ev in log.events_since(&id, 0, 20)? {
                println!("  #{} {:<16} {} {}", ev.seq, ev.kind, ev.step_id, ev.detail);
            }
            Ok(())
        }
        TaskCmd::Events { id, after, limit } => {
            let log = EventLog::new(&app.db);
            for ev in log.events_since(&id, after, limit)? {
                println!("{} {:<16} {} {}", ev.seq, ev.kind, ev.step_id, ev.detail);
            }
            Ok(())
        }
        TaskCmd::Run {
            world,
            out,
            task,
            lease_ttl_secs,
        } => {
            let report = run_worker(
                &app.db,
                &world,
                &out,
                WorkerOpts {
                    task_id: task,
                    lease_ttl_secs,
                    now_override: None,
                    kill: Default::default(),
                },
            )?;
            println!("task {} 完成：{} / 3 步", report.task_id, report.steps_done);
            println!(
                "模型调用 {} 次，{} prompt / {} completion tokens，耗时 {} ms",
                report.model_calls,
                report.prompt_tokens,
                report.completion_tokens,
                report.duration_ms
            );
            if let Some(a) = report.artifact {
                println!("简报：{}", a.display());
            }
            Ok(())
        }
    }
}

fn status_cn(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Queued => "queued（排队）",
        TaskStatus::Running => "running（运行中）",
        TaskStatus::Waiting => "waiting（等待中）",
        TaskStatus::Blocked => "blocked（阻塞）",
        TaskStatus::Done => "done（完成）",
        TaskStatus::Failed => "failed（失败）",
        TaskStatus::Cancelled => "cancelled（已取消）",
    }
}
