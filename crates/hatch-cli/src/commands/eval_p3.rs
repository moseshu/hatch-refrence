//! P3 evaluation comparisons.

use super::eval_record::{read_json, write_atomic};
use anyhow::{ensure, Context, Result};
use hatch_eval::evaluation::{pair_round, PairCounts};
use hatch_eval::scorecard::{Run, RUN_SCHEMA_VERSION};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(super) fn round_dir(out: &Path, index: u32) -> PathBuf {
    out.join(format!("round-{index}"))
}

fn read_run(dir: &Path) -> Result<Run> {
    let run: Run = serde_json::from_value(read_json(&dir.join("run.json"))?)?;
    ensure!(
        run.schema_version == RUN_SCHEMA_VERSION,
        "不支持的运行清单版本"
    );
    Ok(run)
}

fn count(run: &Run) -> Result<u32> {
    match run.settings.get("repeat") {
        None => {
            ensure!(
                run.rounds.len() <= 1,
                "旧式多轮报告需要重新运行以保留逐轮清单"
            );
            Ok(1)
        }
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 1)
            .context("运行清单中的 repeat 无效"),
    }
}

fn round_run(root: &Run, dir: &Path, index: u32, count: u32) -> Result<Option<Run>> {
    if count == 1 {
        return Ok(Some(root.clone()));
    }
    let directory = round_dir(dir, index);
    if !directory.join("run.json").try_exists()? {
        return Ok(None);
    }
    let child = read_run(&directory)?;
    let mut settings = root.settings.clone();
    settings
        .as_object_mut()
        .context("运行设置必须是对象")?
        .remove("repeat");
    ensure!(
        child.world == root.world
            && child.commit == root.commit
            && child.dirty == root.dirty
            && child.fixture_fingerprint == root.fixture_fingerprint
            && child.mode == root.mode
            && child.heldout == root.heldout
            && child.metadata == root.metadata
            && child.settings == settings
            && child.round_origin.as_ref().is_some_and(|origin| {
                origin.parent_run_id == root.run_id && origin.index == index
            })
            && child.rounds.len() <= 1
            && root
                .model
                .as_ref()
                .zip(child.model.as_ref())
                .is_none_or(|(a, b)| a == b),
        "{} 与父运行配置不一致",
        directory.display()
    );
    Ok(Some(child))
}

fn usage(run: Option<&Run>) -> Value {
    run.map_or(Value::Null, |run| {
        json!({
            "status": run.status,
            "provider_usage": run.provider_usage,
            "cost_usd": run.cost_usd,
        })
    })
}

pub fn compare(baseline: &Path, candidate: &Path, out: Option<&Path>) -> Result<()> {
    let before = read_run(baseline)?;
    let after = read_run(candidate)?;
    let metadata = before
        .metadata
        .as_ref()
        .context("基线缺少任务与检查元数据")?;
    metadata.validate()?;
    ensure!(
        before.world == after.world
            && before.world == metadata.world
            && before.fixture_fingerprint == after.fixture_fingerprint
            && before.heldout == after.heldout
            && before.mode == after.mode
            && before.settings == after.settings
            && before.metadata == after.metadata
            && before
                .model
                .as_ref()
                .zip(after.model.as_ref())
                .is_none_or(|(a, b)| a == b),
        "配对要求相同的世界、夹具、模型、模式、评分规则和预算设置"
    );
    ensure!(
        !before.heldout || out.is_some(),
        "留出集配对要求 --out <私有文件>"
    );
    let count = count(&before)?;
    let mut rounds = Vec::new();
    let mut seen = BTreeSet::from([before.run_id.clone()]);
    ensure!(
        seen.insert(after.run_id.clone()),
        "同一次运行不能与自身配对"
    );
    let mut totals = PairCounts::default();
    let mut abilities = BTreeMap::<u8, PairCounts>::new();
    let mut deltas = Vec::new();
    let mut models = BTreeSet::new();
    for index in 1..=count {
        let base_round = round_run(&before, baseline, index, count)?;
        let candidate_round = round_run(&after, candidate, index, count)?;
        let (base_round, candidate_round) = (base_round.as_ref(), candidate_round.as_ref());
        for run in base_round.into_iter().chain(candidate_round) {
            if count > 1 {
                ensure!(seen.insert(run.run_id.clone()), "重复使用了同一次子运行");
            }
            models.extend(run.model.iter().cloned());
        }
        let (base_observed, candidate_observed) = (
            base_round.and_then(Run::observation),
            candidate_round.and_then(Run::observation),
        );
        ensure!(
            before.mode != "live"
                || base_observed.zip(candidate_observed).is_none()
                || base_round.and_then(|run| run.model.as_ref()).is_some()
                    && candidate_round.and_then(|run| run.model.as_ref()).is_some(),
            "缺少可判定轮次的实际模型"
        );
        let outcomes = pair_round(metadata, &before.mode, base_observed, candidate_observed);
        let mut subtotal = PairCounts::default();
        for pair in &outcomes {
            match pair.ability {
                Some(ability) => abilities.entry(ability).or_default().record(pair.outcome),
                None => {
                    totals.record(pair.outcome);
                    subtotal.record(pair.outcome);
                }
            }
        }
        deltas.extend(subtotal.delta());
        rounds.push(json!({
            "round": index,
            "counts": subtotal,
            "paired_pass_rate_delta": subtotal.delta(),
            "baseline": usage(base_round),
            "candidate": usage(candidate_round),
            "tasks": outcomes,
        }));
    }
    ensure!(models.len() <= 1, "配对轮次使用了不同模型");
    let report = json!({
        "schema_version": 1,
        "world": before.world,
        "heldout": before.heldout,
        "mode": before.mode,
        "model": before.model_label().or_else(|| after.model_label()),
        "baseline_run_id": before.run_id,
        "candidate_run_id": after.run_id,
        "baseline_commit": before.commit,
        "candidate_commit": after.commit,
        "baseline_dirty": before.dirty,
        "candidate_dirty": after.dirty,
        "counts": totals,
        "abilities": abilities,
        "paired_pass_rate_delta": totals.delta(),
        "round_delta_range": deltas.iter().copied().reduce(f64::min)
            .zip(deltas.iter().copied().reduce(f64::max))
            .map(|(min, max)| [min, max]),
        "rounds": rounds,
    });
    if let Some(out) = out {
        write_atomic(out, &report)?;
    }
    if before.heldout {
        println!("{}", serde_json::to_string_pretty(&report["counts"])?);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

pub fn calibrate(anchors: &Path, out: Option<&Path>) -> Result<()> {
    let anchors: hatch_eval::calibration::AnchorSet = serde_json::from_value(read_json(anchors)?)?;
    let report = anchors.evaluate()?;
    if let Some(out) = out {
        write_atomic(out, &report)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DivergenceCase {
    case_id: String,
    diff: PathBuf,
    kind: hatch_eval::evaluation::FailureKind,
    signature: String,
    reproduction_id: Option<String>,
}

pub fn group(cases: &Path, out: Option<&Path>) -> Result<()> {
    let rows: Vec<DivergenceCase> = serde_json::from_value(read_json(cases)?)?;
    ensure!(!rows.is_empty(), "至少需要一个开发集差异");
    let parent = cases.parent().unwrap_or_else(|| Path::new("."));
    let mut seen_cases = BTreeSet::new();
    let mut seen_diffs = BTreeSet::new();
    let mut groups = BTreeMap::<_, (Vec<_>, BTreeSet<_>)>::new();
    for row in rows {
        ensure!(
            !row.case_id.trim().is_empty() && seen_cases.insert(row.case_id.clone()),
            "差异个案 ID 为空或重复"
        );
        ensure!(
            !row.signature.is_empty()
                && row.signature.len() <= 120
                && row
                    .signature
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)),
            "差异特征需要稳定的小写标识符"
        );
        let diff = parent.join(row.diff).canonicalize()?;
        ensure!(
            seen_diffs.insert(diff.clone()),
            "同一份回放差异不能重复计数"
        );
        let diff = read_json(&diff)?;
        let stage = diff["observed_stage"]
            .as_str()
            .context("回放结果缺少差异阶段")?;
        ensure!(
            matches!(
                stage,
                "context"
                    | "tool_request"
                    | "approval"
                    | "readback"
                    | "answer"
                    | "final_state"
                    | "branch"
            ) && diff["first_difference"]["stage"].as_str() == Some(stage),
            "回放结果没有可归组的首个差异"
        );
        let (ids, reproductions) = groups
            .entry((stage.to_owned(), row.kind, row.signature))
            .or_default();
        ids.push(row.case_id);
        reproductions.extend(row.reproduction_id);
    }
    let groups: Vec<_> = groups
        .into_iter()
        .map(|((stage, kind, signature), (ids, reproductions))| {
            let kind_tag = match kind {
                hatch_eval::evaluation::FailureKind::Process => "process",
                hatch_eval::evaluation::FailureKind::Quality => "quality",
            };
            json!({
                "group_id": hatch_core::ids::digest(&[&stage, kind_tag, &signature]),
                "first_stage": stage,
                "failure_kind": kind,
                "signature": signature,
                "occurrences": ids.len(),
                "cases": ids,
                "reproduction_ids": reproductions,
            })
        })
        .collect();
    let report = json!({"schema_version": 1, "groups": groups});
    if let Some(out) = out {
        write_atomic(out, &report)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
