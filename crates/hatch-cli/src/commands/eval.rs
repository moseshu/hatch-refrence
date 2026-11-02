// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch eval`

use crate::app::{App, Limits};
use anyhow::{bail, Result};
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

pub fn run(home: Option<&Path>, world: Option<PathBuf>, live: bool, max_calls: u64, max_tokens: u32, out: Option<PathBuf>) -> Result<()> {
    let w = World::load(&world_dir(world))?;
    let report = if live {
        let app = App::open(home)?;
        let router = app.router(Limits { max_calls: Some(max_calls), max_tokens: Some(max_tokens) })?;
        eprintln!("live 模式：最多 {max_calls} 次模型调用，每次 max_tokens={max_tokens}");
        run_eval(&w, Mode::Live { router: &router, max_calls: Some(max_calls) })?
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
