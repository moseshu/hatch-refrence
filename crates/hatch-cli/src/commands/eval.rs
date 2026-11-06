//! `hatch eval`

use crate::app::{App, Limits};
use crate::commands::eval_compaction::run_compaction;
use anyhow::{bail, Result};
use hatch_eval::bfcl::{run_bfcl_eval, BfclOpts};
use hatch_eval::browser::{run_browser_eval, BrMode, BrowserWorld};
use hatch_eval::compaction::CompactionWorld;
use hatch_eval::delegate::{run_delegate_eval, DelegateWorld, DgMode};
use hatch_eval::scheduler::{run_scheduler_eval, SchedMode, SchedulerWorld};
use hatch_eval::workspace::{run_workspace_eval, WorkspaceWorld, WsMode};
use hatch_eval::{run_eval, run_eval_repeat, Mode, RepeatMode, World};
use std::path::{Path, PathBuf};

fn world_dir(arg: Option<PathBuf>) -> PathBuf {
    if let Some(p) = arg {
        return p;
    }
    let local = PathBuf::from("eval/worlds/zh-office");
    if local.exists() {
        local
    } else {
        hatch_eval::world::default_world_dir()
    }
}

/// Eight CLI arguments because every `hatch eval` flag is threaded through;
/// grouping them into a struct would churn all four world dispatchers.
#[allow(clippy::too_many_arguments)]
pub fn run(
    home: Option<&Path>,
    world: Option<PathBuf>,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    repeat: u32,
    bfcl: BfclOpts,
) -> Result<()> {
    if bfcl.data_dir.is_some() {
        return run_bfcl(home, live, max_calls, max_tokens, reasoning_effort, &bfcl);
    }
    if repeat == 0 {
        bail!("--repeat 至少为 1");
    }
    let dir = world_dir(world);
    if repeat > 1
        && (BrowserWorld::is_browser_world(&dir)
            || WorkspaceWorld::is_workspace_world(&dir)
            || DelegateWorld::is_delegate_world(&dir)
            || SchedulerWorld::is_scheduler_world(&dir)
            || CompactionWorld::is_compaction_world(&dir))
    {
        bail!("--repeat 目前只支持邮件分诊世界（zh-office、en-office、thread-attachment 等）；workspace / delegate / scheduler / browser / compaction 世界请单次运行");
    }
    if CompactionWorld::is_compaction_world(&dir) {
        return run_compaction(
            home,
            &dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if BrowserWorld::is_browser_world(&dir) {
        return run_browser(
            home,
            &dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if WorkspaceWorld::is_workspace_world(&dir) {
        return run_workspace(
            home,
            &dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if DelegateWorld::is_delegate_world(&dir) {
        return run_delegate(
            home,
            &dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if SchedulerWorld::is_scheduler_world(&dir) {
        return run_scheduler(
            home,
            &dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    let w = World::load(&dir)?;
    if repeat > 1 {
        return run_repeated(
            home,
            &w,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
            repeat,
        );
    }
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort: reasoning_effort.clone(),
        })?;
        eprintln!(
            "live 模式：调用上限 {}，每次 max_tokens={max_tokens}，reasoning_effort={}",
            max_calls
                .map(|n| n.to_string())
                .unwrap_or_else(|| "不限".into()),
            reasoning_effort
                .as_deref()
                .unwrap_or("（未设置，用模型默认）")
        );
        run_eval(
            &w,
            Mode::Live {
                router: &router,
                max_calls,
            },
        )?
    } else {
        run_eval(&w, Mode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(dir.join("report.md"), &md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
    }
    Ok(())
}

/// Same eight-argument threading as [`run`]; see its note.
#[allow(clippy::too_many_arguments)]
fn run_repeated(
    home: Option<&Path>,
    w: &World,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    repeat: u32,
) -> Result<()> {
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort: reasoning_effort.clone(),
        })?;
        eprintln!(
            "live 模式重复运行 {} 次：调用上限 {}，每次 max_tokens={max_tokens}",
            repeat,
            max_calls
                .map(|n| n.to_string())
                .unwrap_or_else(|| "不限".into()),
        );
        run_eval_repeat(
            w,
            RepeatMode::Live {
                router: &router,
                max_calls,
            },
            repeat as usize,
        )?
    } else {
        run_eval_repeat(w, RepeatMode::Pipeline, repeat as usize)?
    };
    let md = report.render_markdown();
    println!("{md}");
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(dir.join("report.md"), &md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    if !report.all_passed() {
        let failed_runs = report
            .reports
            .iter()
            .enumerate()
            .filter(|(_, r)| r.passed() != r.checks.len())
            .map(|(i, r)| format!("第 {} 次：{}/{} 通过", i + 1, r.passed(), r.checks.len()))
            .collect::<Vec<_>>()
            .join("；");
        bail!("重复运行中有未通过的检查（{failed_runs}）");
    }
    Ok(())
}

fn write_report(out: Option<PathBuf>, json: String, md: &str) -> Result<()> {
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("report.json"), json)?;
        std::fs::write(dir.join("report.md"), md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    Ok(())
}

/// BFCL agent 级评估（Phase A 单轮）：题目走一遍真实 agent 回合，
/// 结果写成 BFCL 格式文件，评分用官方 `bfcl evaluate`。只支持 live 模式。
fn run_bfcl(
    home: Option<&Path>,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    bfcl: &BfclOpts,
) -> Result<()> {
    if !live {
        bail!("BFCL agent 级评估需要 --live（真实模型调用）");
    }
    let app = App::open(home)?;
    let reasoning_effort =
        reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
    let router = app.router(Limits {
        max_calls,
        max_tokens: Some(max_tokens),
        reasoning_effort,
    })?;
    run_bfcl_eval(bfcl, &router)?;
    Ok(())
}

fn run_workspace(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = WorkspaceWorld::load(dir)?;
    let app = App::open(home)?;
    let sandbox = app.sandbox()?;
    let report = if live {
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        })?;
        run_workspace_eval(&w, WsMode::Live { router: &router }, sandbox)?
    } else {
        run_workspace_eval(&w, WsMode::Pipeline, sandbox)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
    }
    Ok(())
}

fn run_delegate(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = DelegateWorld::load(dir)?;
    let app = App::open(home)?;
    let sandbox = app.sandbox()?;
    let report = if live {
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        })?;
        run_delegate_eval(&w, DgMode::Live { router: &router }, sandbox)?
    } else {
        run_delegate_eval(&w, DgMode::Pipeline, sandbox)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
    }
    Ok(())
}

fn run_scheduler(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = SchedulerWorld::load(dir)?;
    let mode = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let limits = Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        };
        // Fail early (and readably) if no model is configured.
        let label = app
            .router(limits.clone())?
            .chain(hatch_llm::Workload::Brief)
            .iter()
            .map(|m| m.name())
            .collect::<Vec<_>>()
            .join(" → ");
        let (config, home) = (app.config.clone(), app.home.clone());
        let factory: hatch_workers::RouterFactory = std::sync::Arc::new(move || {
            crate::app::build_router(&config, &home, limits.clone())
                .map_err(|e| hatch_core::Error::Config(format!("{e:#}")))
        });
        SchedMode::Live { factory, label }
    } else {
        SchedMode::Pipeline
    };
    let report = run_scheduler_eval(&w, mode)?;
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
    }
    Ok(())
}

fn run_browser(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = BrowserWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        })?;
        run_browser_eval(&w, BrMode::Live { router: &router })?
    } else {
        run_browser_eval(&w, BrMode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
    }
    Ok(())
}
