//! `hatch eval`

use crate::app::{App, Limits};
use anyhow::{bail, Result};
use hatch_eval::delegate::{run_delegate_eval, DelegateWorld, DgMode};
use hatch_eval::workspace::{run_workspace_eval, WorkspaceWorld, WsMode};
use hatch_eval::{run_eval, Mode, World};
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

pub fn run(
    home: Option<&Path>,
    world: Option<PathBuf>,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let dir = world_dir(world);
    if WorkspaceWorld::is_workspace_world(&dir) {
        return run_workspace(home, &dir, live, max_calls, max_tokens, reasoning_effort, out);
    }
    if DelegateWorld::is_delegate_world(&dir) {
        return run_delegate(home, &dir, live, max_calls, max_tokens, reasoning_effort, out);
    }
    let w = World::load(&dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort = reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits { max_calls, max_tokens: Some(max_tokens), reasoning_effort: reasoning_effort.clone() })?;
        eprintln!(
            "live 模式：调用上限 {}，每次 max_tokens={max_tokens}，reasoning_effort={}",
            max_calls.map(|n| n.to_string()).unwrap_or_else(|| "不限".into()),
            reasoning_effort.as_deref().unwrap_or("（未设置，用模型默认）")
        );
        run_eval(&w, Mode::Live { router: &router, max_calls })?
    } else {
        run_eval(&w, Mode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(dir.join("report.md"), &md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        bail!("{failed} 项检查未通过");
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
        let reasoning_effort = reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits { max_calls, max_tokens: Some(max_tokens), reasoning_effort })?;
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
        let reasoning_effort = reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits { max_calls, max_tokens: Some(max_tokens), reasoning_effort })?;
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
