//! Evaluate scorer predictions against independently adjudicated anchors.

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchorSet {
    pub component: String,
    pub controls: BTreeSet<String>,
    pub forbidden_violations: BTreeSet<String>,
    pub anchors: Vec<Anchor>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub id: String,
    pub expected: String,
    pub observed: String,
    pub controls: BTreeMap<String, String>,
    #[serde(default)]
    pub violations: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Hits {
    pub total: usize,
    pub observed: usize,
    pub controls: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
pub struct Calibration<'a> {
    pub component: &'a str,
    pub overall: Hits,
    pub by_category: BTreeMap<&'a str, Hits>,
    pub violations: BTreeMap<&'a str, usize>,
    pub passes_predeclared_rule: bool,
}

impl AnchorSet {
    pub fn evaluate(&self) -> Result<Calibration<'_>> {
        if self.component.trim().is_empty() || self.controls.is_empty() || self.anchors.is_empty() {
            return Err(Error::Invalid(
                "component, controls and anchors are required".into(),
            ));
        }
        let mut result = Calibration {
            component: &self.component,
            overall: Hits::default(),
            by_category: BTreeMap::new(),
            violations: BTreeMap::new(),
            passes_predeclared_rule: false,
        };
        let mut ids = BTreeSet::new();
        for anchor in &self.anchors {
            if anchor.id.trim().is_empty()
                || !ids.insert(&anchor.id)
                || anchor.expected.trim().is_empty()
                || anchor.observed.trim().is_empty()
                || anchor
                    .controls
                    .values()
                    .any(|label| label.trim().is_empty())
                || !anchor.controls.keys().eq(&self.controls)
            {
                return Err(Error::Invalid(
                    "invalid anchor ID, prediction or control set".into(),
                ));
            }
            let forbidden = !anchor.violations.is_disjoint(&self.forbidden_violations);
            for violation in &anchor.violations {
                *result.violations.entry(violation).or_default() += 1;
            }
            let category = result.by_category.entry(&anchor.expected).or_default();
            for hits in [&mut result.overall, category] {
                hits.total += 1;
                hits.observed += usize::from(anchor.observed == anchor.expected && !forbidden);
                for (control, prediction) in &anchor.controls {
                    *hits.controls.entry(control.clone()).or_default() +=
                        usize::from(prediction == &anchor.expected);
                }
            }
        }
        result.passes_predeclared_rule = result
            .overall
            .controls
            .values()
            .all(|&control| result.overall.observed > control);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::AnchorSet;
    use serde_json::json;

    #[test]
    fn invalid_citations_lose_credit_even_when_verdict_matches() {
        let mut anchors: AnchorSet = serde_json::from_value(json!({
            "component": "l3",
            "controls": ["always_pass", "always_fail"],
            "forbidden_violations": ["bad_citation"],
            "anchors": [
                {"id": "a", "expected": "pass", "observed": "pass",
                 "controls": {"always_pass": "pass", "always_fail": "fail"}},
                {"id": "b", "expected": "fail", "observed": "fail",
                 "controls": {"always_pass": "pass", "always_fail": "fail"},
                 "violations": ["bad_citation"]}
            ]
        }))
        .unwrap();
        let report = anchors.evaluate().unwrap();
        assert_eq!(report.by_category["fail"].observed, 0);
        assert_eq!(report.violations["bad_citation"], 1);
        assert!(!report.passes_predeclared_rule);
        anchors.anchors[1].violations.clear();
        assert!(anchors.evaluate().unwrap().passes_predeclared_rule);
    }
}
