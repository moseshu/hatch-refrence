//! `hatch eval --world eval/worlds/compaction-basic`

use crate::app::{App, Limits};
use anyhow::{bail, Result};
use hatch_eval::compaction::{run_compaction_eval, CompactionWorld, CpMode};
use std::path::{Path, PathBuf};

/// Same eight-argument threading as `eval::run`; see its note.
#[allow(clippy::too_many_arguments)]
pub fn run_compaction(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = CompactionWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        })?;
        run_compaction_eval(&w, CpMode::Live { router: &router })?
    } else {
        run_compaction_eval(&w, CpMode::Pipeline)?
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
