//! Common run manifest for the different eval world drivers.

use anyhow::{bail, Context, Result};
use hatch_core::{ids, secrets, time};
use hatch_eval::memory::git_state;
use hatch_eval::recording::Capture;
use hatch_eval::scorecard::{Metadata, Round, RoundOrigin, Run};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

pub(super) const MEMORY_MANIFEST: &str = ".memory-run.json";

#[derive(Debug)]
struct CheckFailure(String);

impl std::fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheckFailure {}

pub(super) fn check_failure(message: String) -> anyhow::Error {
    CheckFailure(message).into()
}

pub(super) fn is_check_failure(error: &anyhow::Error) -> bool {
    error.is::<CheckFailure>()
}

fn run_status(rounds: &[Round], result: &Result<()>) -> &'static str {
    let verdicts = || {
        rounds
            .iter()
            .flat_map(Round::verdicts)
            .map(|(_, verdict)| verdict)
    };
    if rounds.is_empty() {
        "interrupted"
    } else if result.as_ref().is_err_and(|error| !is_check_failure(error)) {
        "run_failed"
    } else if verdicts().any(|verdict| verdict == Some(false)) {
        "checks_failed"
    } else if verdicts().any(|verdict| verdict.is_none()) {
        "insufficient_evidence"
    } else if result.is_ok() {
        "completed"
    } else {
        "checks_failed"
    }
}

pub(super) fn run_recorded(
    world: &Path,
    out: &Path,
    live: bool,
    heldout: bool,
    settings: Value,
    run: impl FnOnce() -> Result<()>,
) -> Result<()> {
    Recording {
        world,
        live,
        heldout,
        settings,
        round_origin: None,
    }
    .run(out, ids::random_id("eval"), run)
}

pub(super) struct Recording<'a> {
    pub world: &'a Path,
    pub live: bool,
    pub heldout: bool,
    pub settings: Value,
    pub round_origin: Option<RoundOrigin>,
}

impl Recording<'_> {
    pub fn run(&self, out: &Path, run_id: String, run: impl FnOnce() -> Result<()>) -> Result<()> {
        let Self {
            world,
            live,
            heldout,
            settings,
            round_origin,
        } = self;
        let (world, live, heldout) = (*world, *live, *heldout);
        if resolve_location(out)?.starts_with(world.canonicalize()?) {
            bail!("--out 必须位于评估世界目录之外");
        }
        std::fs::create_dir_all(out)?;
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
        let mut manifest_run = Run {
            schema_version: hatch_eval::scorecard::RUN_SCHEMA_VERSION,
            run_id,
            round_origin: round_origin.clone(),
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
            provider_usage: Default::default(),
            activity: Default::default(),
            cost_usd: None,
            settings: settings.clone(),
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
        if let Some(parent) = hatch_eval::recording::current() {
            parent.merge(&capture);
        }
        let report_path = out.join("report.json");
        let mut report = Value::Null;
        let mut rounds = Vec::new();
        let evidence = (|| -> Result<()> {
            if report_path.try_exists()? {
                report = read_json(&report_path)?;
                rounds =
                    report_rounds(&report, manifest_run.metadata.as_ref(), &manifest_run.mode)?;
            }
            Ok(())
        })();
        let result = capture
            .write(out)
            .map_err(anyhow::Error::from)
            .and(evidence)
            .and(result);
        if manifest_run.metadata.is_none() {
            if let Some(name) = report["world"].as_str() {
                manifest_run.world = name.into();
            }
        }
        manifest_run.model = report["model"].as_str().map(secrets::redact);
        manifest_run.finished_at = time::to_rfc3339(&time::now());
        manifest_run.status = run_status(&rounds, &result).into();
        manifest_run.error = result
            .as_ref()
            .err()
            .map(|e| secrets::redact(&format!("{e:#}")));
        manifest_run.checks_passed = rounds
            .iter()
            .map(|r| {
                r.verdicts()
                    .filter(|(_, verdict)| *verdict == Some(true))
                    .count()
            })
            .sum();
        manifest_run.checks_total = rounds.iter().map(|r| r.checks.len()).sum();
        (manifest_run.provider_usage, manifest_run.activity) = capture.summarize(!live);
        manifest_run.rounds = rounds;
        // Memory evaluations have additional recall diagnostics; retain those fields.
        let diagnostics = out.join(MEMORY_MANIFEST);
        let mut fields = if diagnostics.is_file() {
            read_json(&diagnostics)?
        } else {
            json!({})
        };
        if let (Some(fields), Value::Object(common)) =
            (fields.as_object_mut(), serde_json::to_value(&manifest_run)?)
        {
            fields.extend(common);
        }
        write_atomic(&manifest, &fields)?;
        if diagnostics.is_file() {
            std::fs::remove_file(diagnostics)?;
        }
        result
    }
}

pub(super) fn write_atomic(path: &Path, data: &impl serde::Serialize) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut pending, data)?;
    pending.persist(path)?;
    Ok(())
}

// Resolve existing ancestors as well, so a symlink above a new directory
// cannot place a ledger inside the fixture or the reserved output tree.
fn resolve_location(path: &Path) -> Result<std::path::PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_name().context("无法解析目录")?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(resolve_location(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn with_ledger(
    ledger: Option<&Path>,
    out: &Path,
    world: &Path,
    heldout: bool,
    attempt: Option<&Path>,
    run: impl FnOnce(String) -> Result<()>,
) -> Result<()> {
    let id = ids::random_id("eval");
    let Some(ledger) = ledger else {
        return run(id);
    };
    let output = std::path::absolute(out)?;
    let ledger = std::path::absolute(ledger)?;
    let ledger_location = resolve_location(&ledger)?;
    anyhow::ensure!(
        !ledger_location.starts_with(resolve_location(&output)?)
            && !ledger_location.starts_with(resolve_location(world)?),
        "--ledger 必须位于 --out 和 --world 之外"
    );
    std::fs::create_dir_all(&ledger)?;
    let entry = ledger.join(format!("{id}.json"));
    let attempt = attempt.map(read_json).transpose()?;
    if let Some(attempt) = &attempt {
        for field in ["hypothesis", "change", "expected_impact"] {
            anyhow::ensure!(
                attempt[field]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "尝试记录缺少 {field}"
            );
        }
    }
    let mut record = json!({
        "schema_version": 1,
        "run_id": id,
        "started_at": time::to_rfc3339(&time::now()),
        "heldout": heldout,
        "out": output,
        "attempt": attempt,
        "status": "running",
        "phase": "setup"
    });
    write_atomic(&entry, &record)?;
    let result = run(id.clone());
    let manifest = output.join("run.json");
    let linked = read_json(&manifest)
        .ok()
        .filter(|data| data["run_id"].as_str() == Some(id.as_str()));
    record["finished_at"] = json!(time::to_rfc3339(&time::now()));
    record["manifest"] = if linked.is_some() {
        json!(manifest)
    } else {
        Value::Null
    };
    let status = linked.as_ref().and_then(|data| data["status"].as_str());
    record["phase"] = json!(match status {
        None => "setup",
        Some("running") => "finalization",
        Some("interrupted") => "execution",
        _ => "finished",
    });
    record["status"] = json!(match status {
        None | Some("running") => "interrupted",
        Some(status) => status,
    });
    record["available_evidence"] = json!(["report.json", "model-trace.json"]
        .into_iter()
        .filter(|file| linked.is_some() && output.join(file).is_file())
        .collect::<Vec<_>>());
    record["error"] = json!(result
        .as_ref()
        .err()
        .map(|err| secrets::redact(&format!("{err:#}"))));
    write_atomic(&entry, &record)?;
    result
}

pub(super) fn read_json(path: &Path) -> Result<Value> {
    let data = std::fs::read(path).with_context(|| format!("读取 {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("解析 {}", path.display()))
}

fn report_rounds(report: &Value, metadata: Option<&Metadata>, mode: &str) -> Result<Vec<Round>> {
    if let Some(rounds) = report.get("recorded_rounds") {
        return Ok(serde_json::from_value(rounds.clone())?);
    }
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
                let unknown = check["insufficient_evidence"].as_bool() == Some(true);
                if round
                    .checks
                    .insert(name.into(), passed && !unknown)
                    .is_some()
                {
                    bail!("同一轮报告有重复的检查名：{name}");
                }
                if unknown {
                    round.insufficient_evidence.insert(name.into());
                }
            }
            hatch_eval::evaluation::classify(&mut round, report, metadata, mode);
            Ok(round)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{check_failure, report_rounds, run_status};
    use serde_json::json;

    #[test]
    fn converts_report_checks_without_erasing_insufficient_evidence() {
        let report = json!({"checks": [
            {"name": "pass", "passed": true},
            {"name": "fail", "passed": false},
            {"name": "unreadable", "passed": false, "insufficient_evidence": true}
        ]});
        let rounds = report_rounds(&report, None, "pipeline").unwrap();
        let checks: std::collections::BTreeMap<&str, _> =
            rounds[0].verdicts().map(|(k, v)| (k.as_str(), v)).collect();
        assert_eq!(checks["pass"], Some(true));
        assert_eq!(checks["fail"], Some(false));
        assert_eq!(checks["unreadable"], None);
    }

    #[test]
    fn execution_error_takes_precedence_over_insufficient_check() {
        let rounds = report_rounds(
            &json!({"checks": [
                {"name": "unreadable", "passed": false, "insufficient_evidence": true}
            ]}),
            None,
            "pipeline",
        )
        .unwrap();
        let failed = Err(anyhow::anyhow!("failed to write report.md"));
        let inconclusive = Err(check_failure("1 项检查证据不足".into()));
        assert_eq!(run_status(&rounds, &failed), "run_failed");
        assert_eq!(run_status(&rounds, &inconclusive), "insufficient_evidence");
        assert_eq!(run_status(&[], &failed), "interrupted");
    }
}
