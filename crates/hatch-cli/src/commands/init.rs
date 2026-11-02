// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch init`

use crate::app::App;
use crate::config::{home_dir, EXAMPLE};
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(home: Option<&Path>, force: bool) -> Result<()> {
    let dir = home_dir(home);
    std::fs::create_dir_all(&dir)?;
    let cfg = dir.join("config.toml");
    if cfg.exists() && !force {
        bail!("{} 已存在（使用 --force 覆盖）", cfg.display());
    }
    std::fs::write(&cfg, EXAMPLE)?;
    let app = App::open(Some(&dir))?;
    app.db.audit("user", "init", &dir.display().to_string())?;
    println!("已创建 {}", dir.display());
    println!("  配置：{}", cfg.display());
    println!("  数据库：{}", dir.join("hatch.db").display());
    println!("\n下一步：");
    println!("  1. 编辑配置中的 [user] 与 [mail]，并设置模型 API key 的环境变量（或 HATCH_LLM_BASE_URL / HATCH_LLM_MODEL / HATCH_LLM_API_KEY）");
    println!("  2. 126/163 邮箱需在网页版开启 IMAP/SMTP 并生成授权码：export HATCH_MAIL_PASSWORD=<授权码>");
    println!("  3. hatch doctor   # 自检");
    println!("  4. hatch triage run --dry-run   # 首次建议 dry-run：任何外发只写入 outbox 目录");
    Ok(())
}
