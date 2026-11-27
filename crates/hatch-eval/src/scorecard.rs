//! Stable task identities and required checks for the cross-world scorecard.

use crate::report::Usage;
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const METADATA_FILE: &str = "capabilities.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub world: String,
    pub domain: String,
    pub difficulty: Vec<String>,
    pub inputs: Vec<String>,
    pub artifacts: Vec<String>,
    pub tasks: BTreeMap<String, Vec<Rule>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Exact check name, or a prefix followed by '*'.
    pub check: String,
    /// Empty for engineering checks with no agent capability attribution.
    pub abilities: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl Metadata {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        match std::fs::read(dir.join(METADATA_FILE)) {
            Ok(bytes) => {
                let value: Self = serde_json::from_slice(&bytes)?;
                value.validate()?;
                Ok(Some(value))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let valid_id = |id: &str| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if !valid_id(&self.world)
            || self.domain.is_empty()
            || self.difficulty.is_empty()
            || self.inputs.is_empty()
            || self.artifacts.is_empty()
            || self.tasks.is_empty()
        {
            return Err(Error::Invalid(
                "incomplete world capability metadata".into(),
            ));
        }
        for (task, rules) in &self.tasks {
            if !valid_id(task) || rules.is_empty() {
                return Err(Error::Invalid(format!(
                    "invalid capability task ID or missing checks: {task}"
                )));
            }
            for rule in rules {
                let check = rule.check.strip_suffix('*').unwrap_or(&rule.check);
                let abilities: BTreeSet<_> = rule.abilities.iter().collect();
                if check.is_empty()
                    || check.contains('*')
                    || abilities.len() != rule.abilities.len()
                    || abilities.iter().any(|&&id| !(1..=14).contains(&id))
                    || !matches!(rule.mode.as_deref(), None | Some("pipeline" | "live"))
                {
                    return Err(Error::Invalid(format!(
                        "invalid capability check rule for task {task}"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Round {
    pub checks: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Activity {
    pub tool_steps: usize,
    pub searches: usize,
    pub questions: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub schema_version: u32,
    pub run_id: String,
    pub world: String,
    pub fixture_fingerprint: String,
    pub heldout: bool,
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub mode: String,
    pub model: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub hatch_version: String,
    pub status: String,
    pub error: Option<String>,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub usage: Usage,
    pub activity: Activity,
    /// Price varies by provider and billing tier. Null means unmeasured.
    pub cost_usd: Option<f64>,
    pub settings: serde_json::Value,
    pub metadata: Option<Metadata>,
    pub rounds: Vec<Round>,
}

#[derive(Default)]
struct Results {
    pass: usize,
    fail: usize,
    unknown: usize,
}

impl Results {
    fn record(&mut self, verdict: Option<bool>) {
        match verdict {
            Some(true) => self.pass += 1,
            Some(false) => self.fail += 1,
            None => self.unknown += 1,
        }
    }
}

#[derive(Default)]
struct Ability {
    worlds: BTreeSet<String>,
    tasks: BTreeMap<(String, String), Results>,
    checks: BTreeSet<(String, String)>,
    rounds: usize,
    missing: usize,
    calls: u64,
    tokens: u64,
}

fn matching<'a>(rule: &Rule, round: &'a Round) -> Vec<(&'a String, &'a bool)> {
    round
        .checks
        .iter()
        .filter(|(name, _)| match rule.check.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => *name == &rule.check,
        })
        .collect()
}

fn applies(rule: &Rule, mode: &str) -> bool {
    rule.mode.as_deref().is_none_or(|required| required == mode)
}

fn score_round(
    meta: &Metadata,
    mode: &str,
    round: &Round,
    abilities: &mut [Ability],
    recorded: bool,
) -> usize {
    let mut covered = BTreeSet::new();
    let mut active = BTreeSet::new();
    for (task, rules) in &meta.tasks {
        let mut results = BTreeMap::<u8, Results>::new();
        for rule in rules.iter().filter(|rule| applies(rule, mode)) {
            let found = matching(rule, round);
            covered.extend(found.iter().map(|(name, _)| name.as_str()));
            for id in &rule.abilities {
                let ability = &mut abilities[usize::from(*id - 1)];
                active.insert(*id);
                ability.worlds.insert(meta.world.clone());
                let result = results.entry(*id).or_default();
                if found.is_empty() {
                    result.record(None);
                    ability.missing += 1;
                }
                for (name, passed) in &found {
                    ability.checks.insert((meta.world.clone(), (*name).clone()));
                    result.record(Some(**passed));
                }
            }
        }
        for (id, result) in results {
            let verdict = if result.fail > 0 {
                Some(false)
            } else if result.unknown > 0 {
                None
            } else {
                Some(true)
            };
            abilities[usize::from(id - 1)]
                .tasks
                .entry((meta.world.clone(), task.clone()))
                .or_default()
                .record(verdict);
        }
    }
    if recorded {
        for id in active {
            abilities[usize::from(id - 1)].rounds += 1;
        }
    }
    round.checks.len() - covered.len()
}

pub fn render(runs: &[Run]) -> Result<String> {
    let first = runs
        .first()
        .ok_or_else(|| Error::Invalid("scorecard needs at least one run".into()))?;
    let target: usize = if first.heldout { 20 } else { 30 };
    let mut abilities: Vec<_> = (0..14).map(|_| Ability::default()).collect();
    let mut configurations =
        BTreeMap::<&String, (&String, Option<&str>, &serde_json::Value)>::new();
    let mut seen_runs = BTreeSet::new();
    let mut unmapped = 0;
    let mut calls = 0;
    let mut tokens = 0;
    let mut rounds = 0;
    let mut interrupted = 0;
    let mut activity = Activity::default();
    for run in runs {
        if run.schema_version != 1
            || run.heldout != first.heldout
            || run.mode != first.mode
            || run.commit != first.commit
            || run.dirty != first.dirty
        {
            return Err(Error::Invalid(
                "scorecard requires one evaluation mode, holdout status and checkout per batch"
                    .into(),
            ));
        }
        let key = &run.run_id;
        if !seen_runs.insert(key) {
            return Err(Error::Invalid("the same run was supplied twice".into()));
        }
        let config = (
            &run.fixture_fingerprint,
            run.model.as_deref(),
            &run.settings,
        );
        let previous = configurations.entry(&run.world).or_insert(config);
        if previous.0 != config.0
            || previous.2 != config.2
            || previous.1.zip(config.1).is_some_and(|(a, b)| a != b)
        {
            return Err(Error::Invalid(format!(
                "world {} changed fixture, model or settings within the batch",
                run.world
            )));
        }
        previous.1 = previous.1.or(config.1);
        let meta = run
            .metadata
            .as_ref()
            .ok_or_else(|| Error::Invalid(format!("missing metadata for {}", run.world)))?;
        meta.validate()?;
        if meta.world != run.world {
            return Err(Error::Invalid(
                "run metadata does not match its world".into(),
            ));
        }
        calls += run.usage.calls;
        let run_tokens = run.usage.prompt_tokens + run.usage.completion_tokens;
        tokens += run_tokens;
        activity.tool_steps += run.activity.tool_steps;
        activity.searches += run.activity.searches;
        activity.questions += run.activity.questions;
        let scored: BTreeSet<_> = meta
            .tasks
            .values()
            .flatten()
            .filter(|rule| applies(rule, &run.mode))
            .flat_map(|rule| rule.abilities.iter().copied())
            .collect();
        for id in scored {
            let ability = &mut abilities[usize::from(id - 1)];
            ability.calls += run.usage.calls;
            ability.tokens += run_tokens;
        }
        interrupted += usize::from(run.rounds.is_empty());
        rounds += run.rounds.len();
        if run.rounds.is_empty() {
            score_round(meta, &run.mode, &Round::default(), &mut abilities, false);
        }
        for round in &run.rounds {
            unmapped += score_round(meta, &run.mode, round, &mut abilities, true);
        }
    }
    let unique_tasks: BTreeSet<_> = abilities.iter().flat_map(|a| a.tasks.keys()).collect();
    let mut text = format!(
        "# 能力记分卡\n\n模式：{} · 世界 {} · 去重任务 {} · 运行目录 {} · 完成轮次 {} · 无报告 {}\n\n模型调用 {} · token {}{} · 工具步骤 {} · 搜索 {} · 提问 {} · 未标注检查（按轮）{}\n\n",
        first.mode, configurations.len(), unique_tasks.len(), runs.len(), rounds, interrupted,
        calls, tokens, if first.mode == "pipeline" { "（估算）" } else { "" },
        activity.tool_steps, activity.searches, activity.questions, unmapped,
    );
    text.push_str("| 能力 | 世界 | 去重任务 | 可判定 | 全通过 | 全失败 | 有波动 | 无法判定 | 检查数 | 轮次 | 缺失检查 | 离目标差 | 关联调用 | 关联 token |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for (i, ability) in abilities.iter().enumerate() {
        let mut categories = [0usize; 4];
        for r in ability.tasks.values() {
            let category = match (r.pass > 0, r.fail > 0, r.unknown > 0) {
                (true, true, _) => 2,
                (_, _, true) => 3,
                (true, false, false) => 0,
                (false, true, false) => 1,
                _ => 3,
            };
            categories[category] += 1;
        }
        let determinate = categories[..3].iter().sum::<usize>();
        text.push_str(&format!(
            "| C{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            ability.worlds.len(),
            ability.tasks.len(),
            determinate,
            categories[0],
            categories[1],
            categories[2],
            categories[3],
            ability.checks.len(),
            ability.rounds,
            ability.missing,
            target.saturating_sub(determinate),
            ability.calls,
            ability.tokens,
        ));
    }
    text.push_str("\n每道题在每项能力只计一次。「离目标差」按可判定题数计算。费用未计价。关联用量按涉及该能力的整轮计，同一轮可关联多项能力。工具步骤、搜索和提问按模型提出的调用计。\n");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_worlds_declare_valid_tasks() {
        let worlds = crate::world::default_world_dir();
        let worlds = worlds.parent().unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(worlds).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_dir() {
                continue;
            }
            let metadata = Metadata::load(&path)
                .unwrap()
                .expect("bundled world metadata");
            assert_eq!(
                Some(metadata.world.as_str()),
                path.file_name().and_then(|n| n.to_str())
            );
            count += 1;
        }
        assert_eq!(count, 15);
    }

    #[test]
    fn multiple_checks_and_rounds_do_not_create_new_tasks() {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "world":"demo", "domain":"software", "difficulty":["fixture"],
            "inputs":["text"], "artifacts":["file"],
            "tasks": {"one":[
                {"check":"first", "abilities":[6]},
                {"check":"second", "abilities":[6,7]}
            ]}
        }))
        .unwrap();
        let mut abilities: Vec<_> = (0..14).map(|_| Ability::default()).collect();
        let first = Round {
            checks: [("first".into(), true), ("second".into(), true)].into(),
        };
        let second = Round {
            checks: [("first".into(), false)].into(),
        };
        assert_eq!(
            score_round(&metadata, "pipeline", &first, &mut abilities, true),
            0
        );
        assert_eq!(
            score_round(&metadata, "pipeline", &second, &mut abilities, true),
            0
        );
        let quality = &abilities[5];
        assert_eq!(quality.tasks.len(), 1);
        assert_eq!(quality.checks.len(), 2);
        assert_eq!(quality.rounds, 2);
        let result = quality.tasks.values().next().unwrap();
        assert_eq!((result.pass, result.fail, result.unknown), (1, 1, 0));
        let robustness = abilities[6].tasks.values().next().unwrap();
        assert_eq!(
            (robustness.pass, robustness.fail, robustness.unknown),
            (1, 0, 1)
        );
    }
}
