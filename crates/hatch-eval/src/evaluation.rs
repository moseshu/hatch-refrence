//! Comparable round statistics and first-pass evaluation failure attribution.

use crate::scorecard::{applies, matching, Metadata, Round, Rule};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Process,
    Quality,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub kind: FailureKind,
    /// Stable reason suitable for grouping; the unmodified evidence is in the round report.
    pub reason: String,
    pub review_pending: bool,
}

pub fn classify(round: &mut Round, report: &Value, metadata: Option<&Metadata>, mode: &str) {
    let tasks = report["tasks"].as_array().into_iter().flatten().chain(
        report["days"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|day| day["sessions"].as_array().into_iter().flatten()),
    );
    let stops: BTreeMap<_, _> = tasks
        .filter_map(|task| {
            let id = task["id"].as_str()?;
            let terminal = task["turns"]
                .as_array()
                .and_then(|turns| turns.last())
                .unwrap_or(task);
            Some((id, terminal["stop"].as_str()?))
        })
        .collect();
    round.failures = round
        .verdicts()
        .filter(|(_, verdict)| *verdict == Some(false))
        .map(|(name, _)| {
            let related: BTreeSet<_> = metadata
                .into_iter()
                .flat_map(|meta| &meta.tasks)
                .filter(|(_, rules)| {
                    rules.iter().any(|rule| {
                        applies(rule, mode)
                            && rule.check.strip_suffix('*').map_or_else(
                                || &rule.check == name,
                                |prefix| name.starts_with(prefix),
                            )
                    })
                })
                .map(|(task, _)| task.as_str())
                .collect();
            let process = if report["budget_stopped"].as_bool() == Some(true) {
                Some("call_budget")
            } else {
                related
                    .iter()
                    .filter_map(|task| stops.get(task))
                    .find_map(|stop| match stop.split('(').next().unwrap_or_default() {
                        "StepLimit" => Some("step_limit"),
                        "Budget" => Some("call_budget"),
                        "ModelError" => Some("model_error"),
                        "PlanDeviation" => Some("plan_deviation"),
                        "VerificationBlocked" => Some("verification_blocked"),
                        _ => None,
                    })
            };
            let (kind, reason) = match process {
                Some(reason) => (FailureKind::Process, reason),
                None if !related.is_empty()
                    && related
                        .iter()
                        .all(|task| stops.get(task) == Some(&"Answered")) =>
                {
                    (FailureKind::Quality, "answered_with_failed_check")
                }
                None => (
                    FailureKind::Quality,
                    "failed_check_without_terminal_evidence",
                ),
            };
            (
                name.clone(),
                Failure {
                    kind,
                    reason: reason.into(),
                    review_pending: true,
                },
            )
        })
        .collect();
}

#[derive(Debug, Default, Serialize)]
pub struct CheckVariation {
    pub passed: usize,
    pub failed: usize,
    pub insufficient: usize,
    pub missing: usize,
    pub pass_rate: Option<f64>,
    /// Minimum and maximum binary pass rates among rounds with a verdict.
    pub pass_range: Option<[f64; 2]>,
}

pub fn repeat_checks(
    rounds: &[Option<Round>],
    metadata: Option<&Metadata>,
    mode: &str,
) -> BTreeMap<String, CheckVariation> {
    let expected = metadata
        .into_iter()
        .flat_map(|meta| meta.tasks.values().flatten().chain(&meta.suite_checks))
        .filter(|rule| applies(rule, mode) && !rule.check.ends_with('*'))
        .map(|rule| &rule.check);
    let names: BTreeSet<_> = rounds
        .iter()
        .flatten()
        .flat_map(|round| round.checks.keys())
        .chain(expected)
        .collect();
    names
        .into_iter()
        .map(|name| {
            let mut stat = CheckVariation::default();
            for round in rounds {
                match round.as_ref().and_then(|round| {
                    round.checks.get(name).map(|&passed| {
                        (!round.insufficient_evidence.contains(name)).then_some(passed)
                    })
                }) {
                    Some(Some(true)) => stat.passed += 1,
                    Some(Some(false)) => stat.failed += 1,
                    Some(None) => stat.insufficient += 1,
                    None => stat.missing += 1,
                }
            }
            if stat.passed + stat.failed > 0 {
                stat.pass_rate = Some(stat.passed as f64 / (stat.passed + stat.failed) as f64);
                stat.pass_range = Some([
                    f64::from(u8::from(stat.failed == 0)),
                    f64::from(u8::from(stat.passed > 0)),
                ]);
            }
            (name.clone(), stat)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairOutcome {
    FailToPass,
    PassToFail,
    BothPass,
    BothFail,
    Insufficient,
    Voided,
}

#[derive(Debug, Default, Serialize)]
pub struct PairCounts {
    pub fail_to_pass: usize,
    pub pass_to_fail: usize,
    pub both_pass: usize,
    pub both_fail: usize,
    pub insufficient: usize,
    pub voided: usize,
}

impl PairCounts {
    pub fn record(&mut self, outcome: PairOutcome) {
        match outcome {
            PairOutcome::FailToPass => self.fail_to_pass += 1,
            PairOutcome::PassToFail => self.pass_to_fail += 1,
            PairOutcome::BothPass => self.both_pass += 1,
            PairOutcome::BothFail => self.both_fail += 1,
            PairOutcome::Insufficient => self.insufficient += 1,
            PairOutcome::Voided => self.voided += 1,
        }
    }

    pub fn delta(&self) -> Option<f64> {
        let count = self.both_pass + self.both_fail + self.pass_to_fail + self.fail_to_pass;
        (count > 0).then(|| (self.fail_to_pass as f64 - self.pass_to_fail as f64) / count as f64)
    }
}

#[derive(Debug, Serialize)]
pub struct TaskPair<'a> {
    pub task: &'a str,
    /// None is the verdict across every applicable task rule.
    pub ability: Option<u8>,
    pub outcome: PairOutcome,
}

pub fn pair_round<'a>(
    metadata: &'a Metadata,
    mode: &str,
    baseline: Option<&Round>,
    candidate: Option<&Round>,
) -> Vec<TaskPair<'a>> {
    let verdict = |round: &Round, rules: &[&Rule]| {
        let mut unknown = false;
        for rule in rules {
            let values = matching(rule, round);
            if values.iter().any(|(_, value)| *value == Some(false)) {
                return Some(false);
            }
            unknown |= values.is_empty() || values.iter().any(|(_, value)| value.is_none());
        }
        (!unknown).then_some(true)
    };
    let compare = |rules: &[&Rule]| match baseline.zip(candidate) {
        None => PairOutcome::Voided,
        Some((before, after)) => match (verdict(before, rules), verdict(after, rules)) {
            (Some(false), Some(true)) => PairOutcome::FailToPass,
            (Some(true), Some(false)) => PairOutcome::PassToFail,
            (Some(true), Some(true)) => PairOutcome::BothPass,
            (Some(false), Some(false)) => PairOutcome::BothFail,
            _ => PairOutcome::Insufficient,
        },
    };
    let mut pairs = Vec::new();
    for (task, rules) in &metadata.tasks {
        let rules: Vec<_> = rules.iter().filter(|rule| applies(rule, mode)).collect();
        if rules.is_empty() {
            continue;
        }
        pairs.push(TaskPair {
            task,
            ability: None,
            outcome: compare(&rules),
        });
        let abilities: BTreeSet<_> = rules
            .iter()
            .flat_map(|rule| &rule.abilities)
            .copied()
            .collect();
        for ability in abilities {
            let rules: Vec<_> = rules
                .iter()
                .copied()
                .filter(|rule| rule.abilities.contains(&ability))
                .collect();
            pairs.push(TaskPair {
                task,
                ability: Some(ability),
                outcome: compare(&rules),
            });
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metadata() -> Metadata {
        serde_json::from_value(json!({
            "world":"fixture", "domain":"test", "difficulty":["simple"],
            "inputs":["text"], "artifacts":["text"],
            "tasks": {
                "one": [{"check":"check", "abilities":[6]}],
                "two": [{"check":"expected", "abilities":[9]}]
            }
        }))
        .unwrap()
    }

    #[test]
    fn repeat_distinguishes_failures_missing_and_insufficient() {
        let mut missing_evidence = Round::default();
        missing_evidence.checks.insert("check".into(), false);
        missing_evidence
            .insufficient_evidence
            .insert("check".into());
        let rounds = [
            Some(Round {
                checks: BTreeMap::from([("check".into(), true)]),
                ..Round::default()
            }),
            Some(Round {
                checks: BTreeMap::from([("check".into(), false)]),
                ..Round::default()
            }),
            Some(missing_evidence),
            None,
        ];
        let checks = repeat_checks(&rounds, Some(&metadata()), "pipeline");
        let check = &checks["check"];
        assert_eq!(
            (
                check.passed,
                check.failed,
                check.insufficient,
                check.missing
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(check.pass_rate, Some(0.5));
        assert_eq!(check.pass_range, Some([0.0, 1.0]));
        assert_eq!(checks["expected"].missing, 4);
    }

    #[test]
    fn classifier_uses_only_terminal_evidence_for_the_related_task() {
        let mut round = Round {
            checks: BTreeMap::from([("check".into(), false), ("expected".into(), false)]),
            ..Round::default()
        };
        let report = json!({"tasks": [
            {"id":"one", "turns":[{"stop":"ModelError(\"transient\")"},{"stop":"Answered"}]},
            {"id":"two", "stop":"StepLimit"}
        ]});
        classify(&mut round, &report, Some(&metadata()), "pipeline");
        assert_eq!(round.failures["check"].kind, FailureKind::Quality);
        assert_eq!(round.failures["expected"].kind, FailureKind::Process);
        assert_eq!(round.failures["expected"].reason, "step_limit");
    }

    #[test]
    fn pairing_preserves_void_and_missing_and_scores_each_ability() {
        let meta = metadata();
        let before = Round {
            checks: BTreeMap::from([("check".into(), false)]),
            ..Round::default()
        };
        let after = Round {
            checks: BTreeMap::from([("check".into(), true)]),
            ..Round::default()
        };
        let pairs = pair_round(&meta, "pipeline", Some(&before), Some(&after));
        assert_eq!(pairs[0].outcome, PairOutcome::FailToPass);
        assert_eq!(pairs[1].ability, Some(6));
        assert_eq!(pairs[1].outcome, PairOutcome::FailToPass);
        assert_eq!(pairs[2].outcome, PairOutcome::Insufficient);
        assert!(pair_round(&meta, "pipeline", None, Some(&after))
            .iter()
            .all(|pair| pair.outcome == PairOutcome::Voided));
    }
}
