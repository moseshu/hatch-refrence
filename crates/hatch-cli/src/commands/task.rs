//! `hatch task` — durable tasks (M0 item 6, M1 extended).
//!
//! - `task create [--kind K] [--max-model-calls N]` — create a task
//!   (queued).
//! - `task show <id>` — task row + plan version + budget + steps
//!   (with approval/receipt ids) + recent events.
//! - `task events <id> [--after <seq>] [--limit <n>]` — the event log
//!   from a cursor (a reconnecting client resumes here).
//! - `task run --world <dir> --out <dir> [--task <id>] [--kind K]
//!   [--live] [--max-model-calls N]` — run the worker on this host's
//!   database until the task completes, pauses on an approval, or
//!   pauses on its budget. The client may exit while it runs: the
//!   task lives in the database, and `task show` / `task events`
//!   read its progress back.

use super::task_kinds::{is_known_kind, DEFAULT_KIND, KNOWN_KINDS};
use super::task_worker::{run_worker, WorkerOpts};
use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_core::time::now as now_ts;
use hatch_tasks::{EventLog, TaskStatus, TaskStore, UnitStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Subcommand)]
pub enum TaskCmd {
    /// Create a task (queued, not yet claimed).
    Create {
        /// Human-readable title.
        #[arg(long)]
        title: String,
        /// Task kind (default `meeting-brief`).
        #[arg(long)]
        kind: Option<String>,
        /// Model-call budget (0 = unlimited, default 0).
        #[arg(long, default_value_t = 0)]
        max_model_calls: u64,
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
    /// Run the worker for a task (or a fresh task of `--kind`).
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
        /// Task kind for a fresh task (default `meeting-brief`).
        #[arg(long)]
        kind: Option<String>,
        /// Run the steps on the real model (same env vars as
        /// `hatch eval --live`); default is the scripted model.
        #[arg(long)]
        live: bool,
        /// Model-call budget for this run (0 = the task's stored
        /// budget, 0 on a fresh task = unlimited).
        #[arg(long, default_value_t = 0)]
        max_model_calls: u64,
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
        TaskCmd::Create {
            title,
            kind,
            max_model_calls,
        } => {
            let kind = kind.unwrap_or_else(|| DEFAULT_KIND.into());
            if !is_known_kind(&kind) {
                return Err(anyhow!(
                    "未知任务 kind {kind}（已知 kind：{}）",
                    KNOWN_KINDS.join("、")
                ));
            }
            let task = store.create(&kind, &title, "cli", "", 3, None, &now)?;
            if max_model_calls > 0 {
                store.set_budget(&task.id, max_model_calls, &now)?;
            }
            println!("task {}  queued", task.id);
            println!("kind: {}", task.kind);
            println!("title: {}", task.title);
            if max_model_calls > 0 {
                println!("budget: {max_model_calls} 次模型调用");
            }
            Ok(())
        }
        TaskCmd::Show { id } => {
            let task = store.get(&id)?.ok_or_else(|| anyhow!("task {id} 不存在"))?;
            println!("task {}  {}", task.id, status_cn(&task.status));
            println!("kind: {}  title: {}", task.kind, task.title);
            if task.plan_id.is_empty() {
                println!(
                    "plan: v{}  budget: {} 次模型调用（0 = 不限）",
                    task.plan_version, task.max_model_calls
                );
            } else {
                println!(
                    "plan: {} v{}  budget: {} 次模型调用（0 = 不限）",
                    task.plan_id, task.plan_version, task.max_model_calls
                );
            }
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
            println!("模型调用：{} 次", units.total_model_calls(&id)?);
            for step in units.list_steps(&id)? {
                let extra = [
                    (!step.approval_id.is_empty())
                        .then(|| format!("approval={}", step.approval_id)),
                    (!step.receipt_id.is_empty()).then(|| format!("receipt={}", step.receipt_id)),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
                println!(
                    "  step {:<9} {:<8} attempts={} {} {}",
                    step.step_id,
                    step.status.as_str(),
                    step.attempts,
                    step.output,
                    extra
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
            kind,
            live,
            max_model_calls,
            lease_ttl_secs,
        } => {
            // M1: `--live` runs the steps on the real model (the same
            // env vars as `hatch eval --live`); the default stays the
            // scripted model, so CI makes zero real calls.
            let router = if live {
                Some(Arc::new(app.router(app.default_limits())?))
            } else {
                None
            };
            let report = run_worker(
                &app.db,
                &world,
                &out,
                WorkerOpts {
                    task_id: task,
                    kind,
                    lease_ttl_secs,
                    now_override: None,
                    kill: Default::default(),
                    router,
                    max_model_calls: if max_model_calls > 0 {
                        Some(max_model_calls)
                    } else {
                        None
                    },
                },
            )?;
            if report.cancelled {
                println!("task {} 已取消", report.task_id);
            } else if let Some(reason) = &report.blocked {
                println!(
                    "task {} 已暂停：{} / {} 步",
                    report.task_id, report.steps_done, report.steps_total
                );
                println!("{reason}");
                println!(
                    "批准后重新运行：hatch task run --world <dir> --out <dir> --task {}",
                    report.task_id
                );
            } else if report.budget_stopped {
                println!(
                    "task {} 已暂停：超出模型调用预算（已完成 {} / {} 步，已完成部分见 task show）",
                    report.task_id, report.steps_done, report.steps_total
                );
                println!(
                    "用更大的预算继续：hatch task run --world <dir> --out <dir> --task {} --max-model-calls <N>",
                    report.task_id
                );
            } else {
                println!(
                    "task {} 完成：{} / {} 步",
                    report.task_id, report.steps_done, report.steps_total
                );
            }
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
