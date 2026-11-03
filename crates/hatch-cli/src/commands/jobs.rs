//! `hatch jobs` — scheduled jobs.

use crate::app::App;
use crate::sched;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_core::CancelToken;
use hatch_policy::ActionClass;
use hatch_scheduler::{CatchUp, Delivery, Job, JobStore, NewJob, RunStatus};
use std::path::Path;

#[derive(Subcommand)]
pub enum JobsCmd {
    /// 新建任务
    Add {
        /// 任务种类：daily_brief / triage_sync / memory_flush
        #[arg(long)]
        kind: String,
        /// 调度：`daily 08:30`、`weekly MON,FRI 09:00`、`every 30m`、`once <RFC3339>`、`cron <5 段>`
        #[arg(long)]
        schedule: String,
        /// 任务名（默认用种类名）
        #[arg(long)]
        name: Option<String>,
        /// 投递方式：notify / digest / silent
        #[arg(long, default_value = "digest")]
        delivery: String,
        /// 停机期间错过的触发：none（只记录）/ once（补跑一次）
        #[arg(long, default_value = "none")]
        catch_up: String,
        /// 授权范围：允许按你的常规策略处理的动作类别，逗号分隔（默认空 = 所有对外动作都进审批）
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// 连续失败多少次后自动禁用
        #[arg(long, default_value_t = 3)]
        max_failures: u32,
        /// 单次运行时间上限（秒）
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
        /// 完成条件说明（写进任务定义，便于日后核对）
        #[arg(long, default_value = "")]
        done_condition: String,
        /// 创建后先不启用
        #[arg(long)]
        disabled: bool,
    },
    /// 列出任务
    List,
    /// 查看任务定义与最近运行
    Show { job: String },
    /// 启用（重新计算下次运行时间并清除失败计数）
    Enable { job: String },
    /// 停用
    Disable {
        job: String,
        #[arg(long, default_value = "用户停用")]
        reason: String,
    },
    /// 删除任务定义（运行记录保留）
    Remove { job: String },
    /// 立即运行一次（不改变计划；失败不计入自动禁用）
    RunNow { job: String },
    /// 运行记录
    History {
        job: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub(crate) fn status_line(j: &Job, tz: &chrono_tz::Tz) -> String {
    let state = if !j.enabled {
        if j.disabled_reason.is_empty() { "已停用".to_string() } else { format!("已停用：{}", j.disabled_reason) }
    } else {
        match (j.next_run_at, j.retry_at) {
            (None, _) => "已完成（不再触发）".to_string(),
            (Some(_), Some(r)) => format!("失败 {} 次，{} 重试", j.consecutive_failures, fmt_local(&r, tz)),
            (Some(n), None) => format!("下次 {}", fmt_local(&n, tz)),
        }
    };
    let last = j.last_run_at.map(|t| format!("；上次 {} {}", fmt_local(&t, tz), j.last_status)).unwrap_or_default();
    format!("{state}{last}")
}

pub fn run(home: Option<&Path>, cmd: JobsCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = JobStore::new(&app.db);
    let now = hatch_core::time::now();
    match cmd {
        JobsCmd::Add { kind, schedule, name, delivery, catch_up, scope, max_failures, timeout_secs, done_condition, disabled } => {
            if !hatch_workers::KINDS.contains(&kind.as_str()) {
                bail!("未知任务种类 {kind}（可用：{}）", hatch_workers::KINDS.join(", "));
            }
            let scope: Vec<String> = scope.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            for s in &scope {
                ActionClass::parse(s)?;
            }
            let mut j = NewJob::new(name.as_deref().unwrap_or(&kind), &kind, &schedule);
            j.delivery = Delivery::parse(&delivery)?;
            j.catch_up = CatchUp::parse(&catch_up)?;
            j.scope = scope;
            j.max_failures = max_failures;
            j.timeout_secs = timeout_secs;
            j.done_condition = done_condition;
            j.enabled = !disabled;
            let job = store.add(&j, &now, &app.tz)?;
            println!("已创建任务 {}（{}）：{}  {}", job.name, job.id, job.schedule, status_line(&job, &app.tz));
            if job.scope.is_empty() {
                println!("授权范围为空：该任务提出的任何对外动作都会进入审批队列。");
            }
            println!("任务只在 `hatch daemon run` 运行时触发。");
        }
        JobsCmd::List => {
            let jobs = store.list()?;
            if jobs.is_empty() {
                println!("没有任务。`hatch init` 会创建默认任务（默认停用），或用 `hatch jobs add`。");
            }
            for j in jobs {
                println!("{:<16} {:<13} {:<22} {}", j.name, j.kind, j.schedule.to_string(), status_line(&j, &app.tz));
            }
        }
        JobsCmd::Show { job } => {
            let j = store.get(&job)?;
            println!("{}", serde_json::to_string_pretty(&j)?);
            println!("\n状态：{}", status_line(&j, &app.tz));
            print_history(&store, Some(&j.id), 10, &app.tz)?;
        }
        JobsCmd::Enable { job } => {
            let j = store.set_enabled(&job, true, "", &now, &app.tz)?;
            println!("已启用 {}：{}", j.name, status_line(&j, &app.tz));
        }
        JobsCmd::Disable { job, reason } => {
            let j = store.set_enabled(&job, false, &reason, &now, &app.tz)?;
            println!("已停用 {}", j.name);
        }
        JobsCmd::Remove { job } => {
            store.remove(&job)?;
            println!("已删除任务定义 {job}（运行记录保留）");
        }
        JobsCmd::RunNow { job } => {
            let engine = sched::engine(&app)?;
            let r = engine.run_now(&job, &now, &CancelToken::new())?;
            println!("{} [{}] {}{}", r.job_name, r.status.as_str(), r.summary, if r.error.is_empty() { String::new() } else { format!(" — {}", r.error) });
            if r.status == RunStatus::Failed {
                bail!("运行失败");
            }
        }
        JobsCmd::History { job, limit } => {
            let id = match job {
                Some(j) => Some(store.get(&j)?.id),
                None => None,
            };
            print_history(&store, id.as_deref(), limit, &app.tz)?;
        }
    }
    Ok(())
}

fn print_history(store: &JobStore, job_id: Option<&str>, limit: usize, tz: &chrono_tz::Tz) -> Result<()> {
    let runs = store.history(job_id, limit)?;
    if runs.is_empty() {
        println!("（没有运行记录）");
    }
    for r in runs {
        let detail = if r.error.is_empty() { r.summary.clone() } else { format!("{} {}", r.summary, r.error) };
        println!("{}  {:<9} {:<9} {}", fmt_local(&r.started_at, tz), r.trigger, r.status.as_str(), detail.trim());
    }
    Ok(())
}
