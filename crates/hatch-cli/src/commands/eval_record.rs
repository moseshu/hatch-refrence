//! Common run manifest for the different eval world drivers.

use anyhow::{bail, Context, Result};
use hatch_core::{ids, secrets, time};
use hatch_eval::memory::git_state;
use hatch_eval::recording::Capture;
use hatch_eval::scorecard::{Metadata, Round, Run};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

pub(super) fn run_recorded(
    world: &Path,
    out: &Path,
    live: bool,
    heldout: bool,
    settings: Value,
    run: impl FnOnce() -> Result<()>,
) -> Result<()> {
    std::fs::create_dir_all(out)?;
    if out.canonicalize()?.starts_with(world.canonicalize()?) {
        bail!("--out 必须位于评估世界目录之外");
    }
    if std::fs::read_dir(out)?.next().is_some() {
        bail!("--out 必须是新的空目录，避免混用旧运行的报告");
    }
    let metadata = Metadata::load(world)?;
    let fixture_fingerprint = hatch_eval::markers::fingerprint(world);
    let (commit, dirty) = git_state(Path::new("."));
    let started_at = time::to_rfc3339(&time::now());
    let name = world
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let world_name = metadata
        .as_ref()
        .map(|m| m.world.as_str())
        .unwrap_or(name)
        .to_owned();
    let max_calls = settings["max_calls"].as_u64();
    let mut manifest_run = Run {
        schema_version: 1,
        run_id: ids::random_id("eval"),
        world: world_name,
        fixture_fingerprint,
        heldout,
        commit,
        dirty,
        mode: if live { "live" } else { "pipeline" }.into(),
        model: None,
        started_at,
        finished_at: String::new(),
        hatch_version: env!("CARGO_PKG_VERSION").into(),
        status: "running".into(),
        error: None,
        checks_passed: 0,
        checks_total: 0,
        usage: Default::default(),
        activity: Default::default(),
        cost_usd: None,
        settings,
        metadata,
        rounds: Vec::new(),
    };
    let manifest = out.join("run.json");
    // Reserve this directory before calling any model. A hard stop leaves
    // a "running" manifest with the exact input configuration.
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&manifest)?
        .write_all(&serde_json::to_vec_pretty(&manifest_run)?)?;
    let capture = Capture::default();
    let result = capture.scope(run);
    let report_path = out.join("report.json");
    let report: Value = if report_path.exists() {
        read_json(&report_path)?
    } else {
        Value::Null
    };
    let rounds = report_rounds(&report)?;
    if manifest_run.metadata.is_none() {
        if let Some(name) = report["world"].as_str() {
            manifest_run.world = name.into();
        }
    }
    manifest_run.model = report["model"].as_str().map(secrets::redact);
    manifest_run.finished_at = time::to_rfc3339(&time::now());
    manifest_run.status = if rounds.is_empty() {
        "interrupted"
    } else if result.is_ok()
        && rounds
            .iter()
            .all(|r| r.checks.values().all(|passed| *passed))
    {
        "completed"
    } else {
        "checks_failed"
    }
    .into();
    manifest_run.error = result
        .as_ref()
        .err()
        .map(|e| secrets::redact(&format!("{e:#}")));
    manifest_run.checks_passed = rounds
        .iter()
        .map(|r| r.checks.values().filter(|&&pass| pass).count())
        .sum();
    manifest_run.checks_total = rounds.iter().map(|r| r.checks.len()).sum();
    (manifest_run.usage, manifest_run.activity) = capture.summarize(max_calls, !live);
    manifest_run.rounds = rounds;
    capture.write(out)?;
    // Memory evaluations have additional recall diagnostics; retain those fields.
    let mut previous = read_json(&manifest)?;
    if let (Some(previous), Value::Object(fields)) = (
        previous.as_object_mut(),
        serde_json::to_value(&manifest_run)?,
    ) {
        previous.extend(fields);
    }
    let pending = out.join(".run.json.tmp");
    std::fs::write(&pending, serde_json::to_vec_pretty(&previous)?)?;
    std::fs::rename(pending, manifest)?;
    result
}

pub(super) fn read_json(path: &Path) -> Result<Value> {
    let data = std::fs::read(path).with_context(|| format!("读取 {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("解析 {}", path.display()))
}

fn report_rounds(report: &Value) -> Result<Vec<Round>> {
    let reports: Vec<_> = match report["reports"].as_array() {
        Some(reports) => reports.iter().collect(),
        None if report.is_object() => vec![report],
        None => Vec::new(),
    };
    reports
        .into_iter()
        .map(|report| {
            let mut round = Round::default();
            let checks = report["checks"].as_array().context("report 缺少 checks")?;
            for check in checks {
                let name = check["name"].as_str().context("检查缺少 name")?;
                let passed = check["passed"].as_bool().context("检查缺少 passed")?;
                if round.checks.insert(name.into(), passed).is_some() {
                    bail!("同一轮报告有重复的检查名：{name}");
                }
            }
            Ok(round)
        })
        .collect()
}
