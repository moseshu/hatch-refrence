//! Stable task identities and required checks for the cross-world scorecard.

use crate::report::Usage;
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const METADATA_FILE: &str = "capabilities.json";
pub const RUN_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VariationKind {
    Surface,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub world: String,
    pub domain: String,
    pub difficulty: Vec<String>,
    pub inputs: Vec<String>,
    pub artifacts: Vec<String>,
    pub tasks: BTreeMap<String, Vec<Rule>>,
    /// World-level evidence, reported separately from individual task scores.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suite_checks: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variation_kind: Option<VariationKind>,
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
        for task in self.tasks.keys() {
            if !valid_id(task) {
                return Err(Error::Invalid(format!(
                    "invalid capability task ID: {task}"
                )));
            }
        }
        for rule in self.tasks.values().flatten().chain(&self.suite_checks) {
            let check = rule.check.strip_suffix('*').unwrap_or(&rule.check);
            let abilities: BTreeSet<_> = rule.abilities.iter().collect();
            if check.is_empty()
                || check.contains('*')
                || abilities.len() != rule.abilities.len()
                || abilities.iter().any(|&&id| !(1..=14).contains(&id))
                || !matches!(rule.mode.as_deref(), None | Some("pipeline" | "live"))
            {
                return Err(Error::Invalid(format!(
                    "invalid capability check rule: {}",
                    rule.check
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Round {
    pub checks: BTreeMap<String, bool>,
    /// An inconclusive check is recorded with passed=false for older readers.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub insufficient_evidence: BTreeSet<String>,
}

impl Round {
    pub fn verdicts(&self) -> impl Iterator<Item = (&String, Option<bool>)> {
        self.checks.iter().map(|(name, passed)| {
            (name, (!self.insufficient_evidence.contains(name)).then_some(*passed))
        })
    }
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
    /// All recorded model request attempts, including retries and memory extraction.
    pub provider_usage: Usage,
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
    task_checks_insufficient: usize,
    suite_checks: Results,
    suite_missing: usize,
    calls: u64,
    tokens: u64,
}

fn matching<'a>(rule: &Rule, round: &'a Round) -> Vec<(&'a String, Option<bool>)> {
    round
        .verdicts()
        .filter(|(name, _)| match rule.check.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => *name == &rule.check,
        })
        .collect()
}

fn applies(rule: &Rule, mode: &str) -> bool {
    rule.mode.as_deref().is_none_or(|required| required == mode)
}

#[derive(Default)]
struct Diagnostics {
    unmapped: usize,
    missing: usize,
    insufficient: usize,
    suite_failed: usize,
    suite_insufficient: usize,
    suite_missing: usize,
}

fn score_round(
    meta: &Metadata,
    mode: &str,
    round: &Round,
    abilities: &mut [Ability],
    recorded: bool,
) -> Diagnostics {
    let mut diagnostics = Diagnostics::default();
    let mut missing = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let mut task_checks_insufficient = BTreeSet::new();
    let mut active = BTreeSet::new();
    for (task, rules) in &meta.tasks {
        let mut results = BTreeMap::<u8, Results>::new();
        for rule in rules.iter().filter(|rule| applies(rule, mode)) {
            let found = matching(rule, round);
            covered.extend(found.iter().map(|(name, _)| name.as_str()));
            if found.is_empty() {
                missing.insert(&rule.check);
            }
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
                    result.record(*passed);
                    if passed.is_none() {
                        task_checks_insufficient.insert((*id, *name));
                    }
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
    for (id, _) in task_checks_insufficient {
        abilities[usize::from(id - 1)].task_checks_insufficient += 1;
    }
    let mut suite_failed = BTreeSet::new();
    let mut suite_insufficient = BTreeSet::new();
    let mut suite_missing = BTreeSet::new();
    let mut suite_missing_by_ability = BTreeSet::new();
    let mut suite_results = BTreeSet::new();
    for rule in meta.suite_checks.iter().filter(|rule| applies(rule, mode)) {
        let found = matching(rule, round);
        if found.is_empty() {
            missing.insert(&rule.check);
            suite_missing.insert(&rule.check);
            suite_missing_by_ability.extend(rule.abilities.iter().map(|&id| (id, &rule.check)));
        }
        for (name, passed) in found {
            covered.insert(name.as_str());
            match passed {
                Some(false) => { suite_failed.insert(name); }
                None => { suite_insufficient.insert(name); }
                Some(true) => {}
            }
            suite_results.extend(rule.abilities.iter().map(|&id| (id, name, passed)));
        }
    }
    for (id, _) in suite_missing_by_ability {
        let ability = &mut abilities[usize::from(id - 1)];
        active.insert(id);
        ability.worlds.insert(meta.world.clone());
        ability.suite_missing += 1;
    }
    for (id, _, verdict) in suite_results {
        let ability = &mut abilities[usize::from(id - 1)];
        active.insert(id);
        ability.worlds.insert(meta.world.clone());
        ability.suite_checks.record(verdict);
    }
    if recorded {
        for id in active {
            abilities[usize::from(id - 1)].rounds += 1;
        }
    }
    diagnostics.unmapped = round.checks.len() - covered.len();
    diagnostics.missing = missing.len();
    diagnostics.insufficient = round.verdicts().filter(|(_, verdict)| verdict.is_none()).count();
    diagnostics.suite_failed = suite_failed.len();
    diagnostics.suite_insufficient = suite_insufficient.len();
    diagnostics.suite_missing = suite_missing.len();
    diagnostics
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
    let mut diagnostics = Diagnostics::default();
    let mut base_tasks = BTreeSet::new();
    let mut surface_tasks = BTreeSet::new();
    let mut unchecked_tasks = BTreeSet::new();
    let mut surface_abilities: Vec<_> = (0..14).map(|_| Ability::default()).collect();
    let mut calls = 0;
    let mut tokens = 0;
    let mut rounds = 0;
    let mut interrupted = 0;
    let mut activity = Activity::default();
    for run in runs {
        if run.schema_version != RUN_SCHEMA_VERSION
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
        let (task_ids, scored_abilities) = match meta.variation_kind {
            Some(VariationKind::Surface) => (&mut surface_tasks, &mut surface_abilities),
            None => (&mut base_tasks, &mut abilities),
        };
        for (id, rules) in &meta.tasks {
            let tasks = if rules.iter().any(|rule| applies(rule, &run.mode)) {
                &mut *task_ids
            } else {
                &mut unchecked_tasks
            };
            tasks.insert((&meta.world, id));
        }
        calls += run.provider_usage.calls;
        let run_tokens = run.provider_usage.prompt_tokens + run.provider_usage.completion_tokens;
        tokens += run_tokens;
        activity.tool_steps += run.activity.tool_steps;
        activity.searches += run.activity.searches;
        activity.questions += run.activity.questions;
        let scored: BTreeSet<_> = meta
            .tasks
            .values()
            .flatten()
            .chain(&meta.suite_checks)
            .filter(|rule| applies(rule, &run.mode))
            .flat_map(|rule| rule.abilities.iter().copied())
            .collect();
        for id in scored {
            let ability = &mut scored_abilities[usize::from(id - 1)];
            ability.calls += run.provider_usage.calls;
            ability.tokens += run_tokens;
        }
        interrupted += usize::from(run.rounds.is_empty());
        rounds += run.rounds.len();
        let empty = Round::default();
        let recorded = !run.rounds.is_empty();
        let evidence = if recorded {
            &run.rounds[..]
        } else {
            std::slice::from_ref(&empty)
        };
        for round in evidence {
            let result = score_round(meta, &run.mode, round, scored_abilities, recorded);
            diagnostics.unmapped += result.unmapped;
            diagnostics.missing += result.missing;
            diagnostics.insufficient += result.insufficient;
            diagnostics.suite_failed += result.suite_failed;
            diagnostics.suite_insufficient += result.suite_insufficient;
            diagnostics.suite_missing += result.suite_missing;
        }
    }
    let mut text = format!(
        "# 能力记分卡\n\n模式：{} · 世界 {} · 去重任务 {} · 表层变体 {} · 无检查任务 {} · 运行目录 {} · 完成轮次 {} · 无报告 {}\n\n模型请求尝试 {} · token {}{} · 工具步骤 {} · 搜索 {} · 提问 {} · 未标注检查 {} · 缺失检查 {} · 世界级检查失败 {} · 世界级检查缺失 {} · 证据不足检查 {} · 世界级检查证据不足 {}\n\n",
        first.mode, configurations.len(), base_tasks.len(), surface_tasks.len(), unchecked_tasks.len(), runs.len(), rounds, interrupted,
        calls, tokens, if first.mode == "pipeline" { "（估算）" } else { "" },
        activity.tool_steps, activity.searches, activity.questions, diagnostics.unmapped,
        diagnostics.missing, diagnostics.suite_failed, diagnostics.suite_missing,
        diagnostics.insufficient, diagnostics.suite_insufficient,
    );
    render_abilities(&mut text, &abilities, Some(target));
    if !surface_tasks.is_empty() {
        text.push_str("\n## 表层变体\n\n");
        render_abilities(&mut text, &surface_abilities, None);
    }
    text.push_str("\n每道题在每项能力只计一次。「离目标差」按可判定题数计算，表层变体单列。当前模式下没有适用检查规则的任务计入「无检查任务」。未标注和缺失检查按轮计，世界级检查也纳入核对。能力表按轮统计已记录但证据不足的任务检查。世界级检查按标签归入各能力，按轮统计通过、失败、证据不足和缺失。费用未计价。模型请求尝试含重试，关联用量按涉及该能力的整轮计，同一轮可关联多项能力。工具步骤、搜索和提问按模型提出的调用计。\n");
    Ok(text)
}

fn render_abilities(text: &mut String, abilities: &[Ability], target: Option<usize>) {
    text.push_str("| 能力 | 世界 | 去重任务 | 可判定 | 全通过 | 全失败 | 有波动 | 无法判定 | 检查数 | 轮次 | 缺失检查 | 离目标差 | 关联请求尝试 | 关联 token | 世界级检查通过 | 世界级检查失败 | 世界级检查缺失 | 任务检查证据不足 | 世界级检查证据不足 |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
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
            "| C{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
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
            target.map_or_else(
                || "—".into(),
                |target| target.saturating_sub(determinate).to_string()
            ),
            ability.calls,
            ability.tokens,
            ability.suite_checks.pass,
            ability.suite_checks.fail,
            ability.suite_missing,
            ability.task_checks_insufficient,
            ability.suite_checks.unknown,
        ));
    }
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
            let expected = fixture_task_ids(&path, &metadata.world);
            assert_eq!(
                metadata.tasks.keys().cloned().collect::<BTreeSet<_>>(),
                expected,
                "task IDs for {}",
                metadata.world
            );
            count += 1;
        }
        assert_eq!(count, 15);
    }

    fn fixture_task_ids(path: &Path, world: &str) -> BTreeSet<String> {
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(path.join(name)).unwrap()).unwrap()
        };
        let ids = |items: &serde_json::Value, field: &str| -> BTreeSet<String> {
            items
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item[field].as_str().unwrap().to_owned())
                .collect()
        };

        let task_fixture = match world {
            "act-or-ask" => Some("caution.json"),
            "browser-basic" => Some("browser.json"),
            "tool-faults" => Some("faults.json"),
            "web-live" => Some("web.json"),
            "workspace-basic" => Some("tasks.json"),
            _ => None,
        };
        if let Some(file) = task_fixture {
            return ids(&read(file)["tasks"], "id");
        }
        match world {
            "memory-crossday" | "memory-pressure" => read("memory.json")["days"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|day| ids(&day["sessions"], "id"))
                .collect(),
            "scheduled-tasks" => ids(&read("schedule.json")["jobs"], "name"),
            // The delegate runner executes these named fixture scenarios directly.
            "delegate-basic" => {
                let fixture = read("delegate.json");
                ["split", "cancel", "depth", "injection"]
                    .into_iter()
                    .map(|id| {
                        assert!(!fixture[id].is_null());
                        id.to_owned()
                    })
                    .collect()
            }
            // The scheduler runs two fixture-driven jobs and one built-in weekly report.
            "scheduler-basic" => {
                let fixture = read("scheduler.json");
                for id in ["brief", "outbound"] {
                    assert!(!fixture[id].is_null());
                }
                ["brief", "outbound", "weekly"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            }
            // The compaction harness runs one model summary and an extractive control.
            "compaction-basic" => ["model-summary".to_owned()].into(),
            "en-office" | "zh-office" | "meeting-brief" | "thread-attachment" => {
                let fixture = read("expectations.json");
                let mut tasks = BTreeSet::from(["mailbox-batch".to_owned()]);
                for (key, id) in [("brief", "meeting-context"), ("thread", "thread-revision")] {
                    if !fixture[key].is_null() {
                        tasks.insert(id.to_owned());
                    }
                }
                tasks
            }
            _ => panic!("missing task fixture mapping for {world}"),
        }
    }

    #[test]
    fn a_recorded_insufficient_check_is_not_missing_or_failed() {
        let round: Round = serde_json::from_value(serde_json::json!({
            "checks": {"known": true, "unknown": false}, "insufficient_evidence": ["unknown"]
        })).unwrap();
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "world": "demo", "domain": "software", "difficulty": ["fixture"],
            "inputs": ["text"], "artifacts": ["file"],
            "tasks": {"one": [{"check": "unknown", "abilities": [6]}]},
            "suite_checks": [{"check": "known", "abilities": [6]}, {"check": "unknown", "abilities": [6]}]
        })).unwrap();
        let mut abilities: Vec<_> = (0..14).map(|_| Ability::default()).collect();
        let diagnostics = score_round(&metadata, "pipeline", &round, &mut abilities, true);
        assert_eq!((diagnostics.missing, diagnostics.suite_failed, diagnostics.suite_missing), (0, 0, 0));
        assert_eq!((diagnostics.insufficient, diagnostics.suite_insufficient), (1, 1));
        let ability = &abilities[5];
        assert_eq!((ability.suite_checks.pass, ability.suite_checks.fail, ability.suite_checks.unknown), (1, 0, 1));
        assert_eq!(ability.suite_missing, 0);
        assert_eq!(ability.task_checks_insufficient, 1);
        let mut table = String::new();
        render_abilities(&mut table, &abilities, None);
        assert!(table.contains("世界级检查缺失 | 任务检查证据不足 | 世界级检查证据不足"));
        let row = table.lines().find(|line| line.starts_with("| C6 |")).unwrap();
        assert!(row.ends_with("| 1 | 0 | 0 | 1 | 1 |"));
        let results = ability.tasks.values().next().unwrap();
        assert_eq!((results.pass, results.fail, results.unknown), (0, 0, 1));
    }

    #[test]
    fn multiple_checks_and_rounds_do_not_create_new_tasks() {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "world":"demo", "domain":"software", "difficulty":["fixture"],
            "inputs":["text"], "artifacts":["file"],
            "suite_checks": [
                {"check":"shared-*", "abilities":[6,10]},
                {"check":"shared-fail", "abilities":[10]},
                {"check":"live-only", "abilities":[10], "mode":"live"}
            ],
            "tasks": {
                "one": [
                    {"check":"first", "abilities":[6]},
                    {"check":"second", "abilities":[6,7]}
                ],
                "setup": []
            }
        }))
        .unwrap();
        let mut abilities: Vec<_> = (0..14).map(|_| Ability::default()).collect();
        let first = Round {
            checks: [
                ("first".into(), true),
                ("second".into(), true),
                ("shared-pass".into(), true),
                ("shared-fail".into(), false),
            ]
            .into(),
            ..Round::default()
        };
        let second = Round {
            checks: [("first".into(), false)].into(),
            ..Round::default()
        };
        let first_diagnostics = score_round(&metadata, "pipeline", &first, &mut abilities, true);
        assert_eq!(
            (
                first_diagnostics.unmapped,
                first_diagnostics.missing,
                first_diagnostics.suite_failed
            ),
            (0, 0, 1)
        );
        let second_diagnostics = score_round(&metadata, "pipeline", &second, &mut abilities, true);
        assert_eq!(
            (
                second_diagnostics.unmapped,
                second_diagnostics.missing,
                second_diagnostics.suite_missing
            ),
            (0, 3, 2)
        );
        let quality = &abilities[5];
        assert_eq!(quality.tasks.len(), 1);
        assert_eq!(quality.checks.len(), 2);
        assert_eq!(quality.rounds, 2);
        assert_eq!(
            (
                quality.suite_checks.pass,
                quality.suite_checks.fail,
                quality.suite_checks.unknown
            ),
            (1, 1, 0)
        );
        assert_eq!(quality.suite_missing, 1);
        let result = quality.tasks.values().next().unwrap();
        assert_eq!((result.pass, result.fail, result.unknown), (1, 1, 0));
        let robustness = abilities[6].tasks.values().next().unwrap();
        assert_eq!(
            (robustness.pass, robustness.fail, robustness.unknown),
            (1, 0, 1)
        );
        let safety = &abilities[9];
        assert_eq!(safety.worlds.len(), 1);
        assert_eq!(safety.rounds, 2);
        assert!(safety.tasks.is_empty());
        assert_eq!(
            (
                safety.suite_checks.pass,
                safety.suite_checks.fail,
                safety.suite_checks.unknown
            ),
            (1, 1, 0)
        );
        assert_eq!(safety.suite_missing, 2);

        let card = render(&[Run {
            schema_version: RUN_SCHEMA_VERSION,
            run_id: "demo-1".into(),
            world: metadata.world.clone(),
            fixture_fingerprint: "fixture".into(),
            heldout: false,
            commit: None,
            dirty: None,
            mode: "pipeline".into(),
            model: None,
            started_at: String::new(),
            finished_at: String::new(),
            hatch_version: String::new(),
            status: "completed".into(),
            error: None,
            checks_passed: 3,
            checks_total: 5,
            provider_usage: Usage::default(),
            activity: Activity::default(),
            cost_usd: None,
            settings: serde_json::Value::Null,
            metadata: Some(metadata),
            rounds: vec![first, second],
        }])
        .unwrap();
        assert!(card.contains("去重任务 1 · 表层变体 0 · 无检查任务 1"));
        let c10 = card
            .lines()
            .find(|line| line.starts_with("| C10 |"))
            .unwrap();
        assert!(c10.ends_with(" | 1 | 1 | 2 | 0 | 0 |"), "{c10}");
    }
}
