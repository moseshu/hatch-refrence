//! `hatch eval --world eval/worlds/compaction-basic`

use crate::app::{App, Limits};
use crate::commands::eval::{write_report, Output};
use crate::commands::eval_record::check_failure;
use anyhow::Result;
use hatch_eval::compaction::{run_compaction_eval, CompactionWorld, CpMode};
use std::path::Path;

pub(super) fn run_compaction(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Output,
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
        }).map(hatch_eval::recording::attach)?;
        run_compaction_eval(&w, CpMode::Live { router: &router })?
    } else {
        run_compaction_eval(&w, CpMode::Pipeline)?
    };
    let md = report.render_markdown();
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}
