//! Independent executions of the normal world dispatcher.

use super::eval_record::{check_failure, is_check_failure, read_json, write_atomic, Recording};
use anyhow::{bail, Result};
use hatch_core::ids;
use hatch_eval::evaluation::repeat_checks;
use hatch_eval::scorecard::{Round, RoundOrigin, Run};
use serde_json::json;
use std::path::{Path, PathBuf};

pub(super) fn run(
    parent: &Recording<'_>,
    parent_run_id: &str,
    out: &Path,
    count: u32,
    execute: impl Fn(Option<PathBuf>) -> Result<()>,
) -> Result<()> {
    let mut settings = parent.settings.clone();
    settings
        .as_object_mut()
        .expect("eval settings are an object")
        .remove("repeat");
    let mut observations: Vec<Option<Round>> = Vec::new();
    let mut runs = Vec::new();
    let mut interrupted = 0;
    let mut not_passed = 0;
    let mut models = std::collections::BTreeSet::new();
    let mut report_world = None;

    for index in 1..=count {
        let directory = super::eval_p3::round_dir(out, index);
        let child = Recording {
            world: parent.world,
            live: parent.live,
            heldout: parent.heldout,
            settings: settings.clone(),
            round_origin: Some(RoundOrigin {
                parent_run_id: parent_run_id.to_owned(),
                index,
            }),
        };
        let execution = child.run(&directory, ids::random_id("eval"), || {
            execute(Some(directory.clone()))
        });
        let manifest = read_json(&directory.join("run.json"))
            .and_then(|value| Ok(serde_json::from_value::<Run>(value)?));
        let mut status = "interrupted";
        let mut observation = None;
        let mut entry = json!({"round": index});
        if let Ok(manifest) = manifest.as_ref() {
            status = manifest.status.as_str();
            report_world = Some(manifest.world.clone());
            models.extend(manifest.model.iter().cloned());
            observation = manifest.observation().cloned();
            entry["run_id"] = json!(manifest.run_id);
            entry["provider_usage"] = json!(manifest.provider_usage);
            entry["cost_usd"] = json!(manifest.cost_usd);
            entry["manifest"] = json!(format!("round-{index}/run.json"));
        }
        entry["status"] = json!(status);
        entry["checks_passed"] = json!(observation
            .as_ref()
            .map(|round| { round.verdicts().filter(|(_, v)| *v == Some(true)).count() }));
        entry["checks_total"] = json!(observation.as_ref().map(|round| round.checks.len()));
        if let Some(error) = execution
            .as_ref()
            .err()
            .filter(|error| !is_check_failure(error))
            .or_else(|| manifest.as_ref().err())
        {
            entry["error"] = json!(hatch_core::secrets::redact(&format!("{error:#}")));
            interrupted += 1;
        } else if observation.is_none() {
            interrupted += 1;
        }
        if execution.is_err() || status != "completed" {
            not_passed += 1;
        }
        runs.push(entry);
        observations.push(observation);
    }
    let metadata = hatch_eval::scorecard::Metadata::load(parent.world)?;
    let mode = if parent.live { "live" } else { "pipeline" };
    let checks = repeat_checks(&observations, metadata.as_ref(), mode);
    let mut markdown = format!(
        "# 多轮评估\n\n执行 {count} 轮，全部检查通过 {} 轮，中断 {interrupted} 轮。\n",
        count - not_passed,
    );
    if !parent.heldout {
        markdown.push_str("\n| 检查 | 通过 | 失败 | 证据不足 | 缺失 | 通过率范围 |\n|---|---:|---:|---:|---:|---:|\n");
        for (name, stat) in &checks {
            let range = match stat.pass_range {
                Some([min, max]) => format!("{:.0}%–{:.0}%", min * 100.0, max * 100.0),
                None => "—".into(),
            };
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                name.replace('|', "\\|"),
                stat.passed,
                stat.failed,
                stat.insufficient,
                stat.missing,
                range,
            ));
        }
    }
    let report = json!({
        "world": report_world,
        "model": (models.len() == 1).then(|| models.iter().next()).flatten(),
        "requested_rounds": count,
        "round_runs": runs,
        "checks": checks,
        "recorded_rounds": observations.into_iter().flatten().collect::<Vec<_>>(),
    });
    write_atomic(&out.join("report.json"), &report)?;
    std::fs::write(out.join("report.md"), &markdown)?;
    if parent.heldout {
        std::fs::write(out.join("summary.md"), &markdown)?;
    }
    print!("{markdown}");
    if models.len() > 1 {
        bail!("重复运行的模型发生变化");
    }
    if interrupted > 0 {
        bail!("{interrupted} 轮评估中断或无法取证");
    }
    if not_passed > 0 {
        return Err(check_failure(format!("{not_passed} 轮评估有未通过的检查")));
    }
    Ok(())
}
