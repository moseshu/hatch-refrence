//! Scorecard, seeded fixture generation, and offline differential replay.

use super::eval_record::{read_json, run_recorded};
use anyhow::{bail, ensure, Context, Result};
use hatch_eval::recording::Exchange;
use hatch_eval::replay::{compare, Tape};
use hatch_eval::scorecard::{self, Run};
use hatch_eval::weblive::{run_web_eval, WebWorld, WlMode};
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn scorecard(runs: &[PathBuf], out: Option<&Path>) -> Result<()> {
    let manifests = runs
        .iter()
        .map(|dir| {
            serde_json::from_value::<Run>(read_json(&dir.join("run.json"))?)
                .with_context(|| format!("运行目录缺少 P1 清单：{}", dir.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let text = scorecard::render(&manifests)?;
    if let Some(path) = out {
        std::fs::write(path, &text)?;
    }
    print!("{text}");
    Ok(())
}

pub fn variant(template: &Path, output: &Path, seed: u64) -> Result<()> {
    let digest = hatch_eval::variant::generate(template, output, seed)?;
    println!(
        "评估世界：{}\n指纹：{digest}",
        output.join("world").display()
    );
    Ok(())
}

fn trace(dir: &Path) -> Result<Vec<Exchange>> {
    Ok(serde_json::from_value(read_json(
        &dir.join("model-trace.json"),
    )?)?)
}

pub fn replay(world: &Path, baseline: &Path, out: &Path, expect: &str) -> Result<()> {
    const STAGES: &[&str] = &[
        "identical",
        "context",
        "tool_request",
        "approval",
        "readback",
        "answer",
        "final_state",
        "branch",
    ];
    ensure!(
        STAGES.contains(&expect),
        "--expect-stage 取值：{}",
        STAGES.join(", ")
    );
    let manifest: Run = serde_json::from_value(read_json(&baseline.join("run.json"))?)?;
    ensure!(
        manifest.schema_version == scorecard::RUN_SCHEMA_VERSION
            && manifest.mode == "pipeline"
            && !manifest.heldout
            && manifest.world == "web-live",
        "差分回放当前要求 web-live 的公开 pipeline 基线"
    );
    let mut web_world = WebWorld::load(world)?;
    ensure!(
        web_world.spec.name == "web-live"
            && hatch_eval::markers::fingerprint(world) == manifest.fixture_fingerprint,
        "回放要求相同的 web-live 夹具指纹"
    );
    web_world.spec.now = manifest.settings["fixture_clock"]
        .as_str()
        .context("基线缺少冻结的夹具时钟")?
        .to_owned();

    let baseline_trace = trace(baseline)?;
    let baseline_report = read_json(&baseline.join("report.json"))?;
    let tape = Tape::new(baseline_trace.clone())?;
    let result = run_recorded(world, out, false, false, manifest.settings, || {
        let report = run_web_eval(&web_world, WlMode::PipelineReplay(tape.model.clone()))?;
        std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        std::fs::write(out.join("report.md"), report.render_markdown())?;
        Ok(())
    });
    let candidate_trace = trace(out)?;
    let candidate_report = if out.join("report.json").exists() {
        read_json(&out.join("report.json"))?
    } else {
        serde_json::Value::Null
    };
    let divergence = tape.divergence();
    let difference = compare(
        (baseline, &baseline_trace, &baseline_report),
        (out, &candidate_trace, &candidate_report),
        divergence,
    )?;
    let observed_stage = difference
        .as_ref()
        .map(|diff| diff.stage)
        .unwrap_or("identical");
    let report = json!({
        "first_difference": difference,
        "observed_stage": observed_stage,
        "expected_stage": expect,
        "matches_expectation": observed_stage == expect,
        "incomparable_from_request": divergence.map(|index| index + 1),
    });
    let rendered = serde_json::to_string_pretty(&report)?;
    std::fs::write(out.join("diff.json"), &rendered)?;
    println!("{rendered}");
    result?;
    if observed_stage != expect {
        bail!("差分发生在 {observed_stage}，预期为 {expect}");
    }
    Ok(())
}
