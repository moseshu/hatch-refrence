//! Task-level acceptance contract: cross-step conditions frozen with
//! the plan before execution (external review item 5).
//!
//! Two item kinds share one frozen contract:
//! * `Predicate` — deterministic cross-step conditions over typed
//!   fields observed by independent readback (L2). Evaluated here,
//!   zero model calls.
//! * `Nl` — natural-language rubric items for the isolated semantic
//!   grader (L3). Graded once per task boundary over the same
//!   evidence bundle, in Observe (`enforced=false`) until a rule
//!   class earns Enforce on evaluation data.
//!
//! The contract is never learned from execution output: items are
//! validated against the plan's steps at freeze time, and the frozen
//! JSON joins the `@task-completion` verification contract, so a
//! changed contract after execution starts is a contract mismatch,
//! not a silent redefinition of "done".

use crate::plan::PlanStep;
use hatch_core::{Error, Result};
use hatch_tools::verification::{EffectState, EvidenceCheck, Layer, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub const MAX_ACCEPTANCE_ITEMS: usize = 32;

/// One typed field of one step's observed resource, e.g.
/// `{"step":"s1","field":"event.starts_at"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldRef {
    pub step: String,
    pub field: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateOp {
    Equals,
    Contains,
    Exists,
    CountAtLeast,
}

/// The right-hand side of a predicate: another observed field, or a
/// literal frozen into the contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operand {
    Field { step: String, field: String },
    Literal { value: Value },
}

impl Operand {
    fn as_field(&self) -> Option<FieldRef> {
        match self {
            Operand::Field { step, field } => Some(FieldRef {
                step: step.clone(),
                field: field.clone(),
            }),
            Operand::Literal { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptanceItem {
    Predicate {
        id: String,
        left: FieldRef,
        op: PredicateOp,
        #[serde(default)]
        right: Option<Operand>,
        /// `rfc3339_tz` (compare instants) or `text` (trim + CRLF→LF);
        /// `None` compares raw JSON values.
        #[serde(default)]
        normalize: Option<String>,
    },
    Nl {
        id: String,
        expected: String,
    },
}

impl AcceptanceItem {
    pub fn id(&self) -> &str {
        match self {
            AcceptanceItem::Predicate { id, .. } | AcceptanceItem::Nl { id, .. } => id,
        }
    }
    pub fn is_predicate(&self) -> bool {
        matches!(self, AcceptanceItem::Predicate { .. })
    }
}

/// String acceptance criteria (the pre-contract `submit_goal` shape)
/// become NL items, so existing callers keep working unchanged.
pub fn nl_items(criteria: &[String]) -> Vec<AcceptanceItem> {
    criteria
        .iter()
        .enumerate()
        .map(|(i, text)| AcceptanceItem::Nl {
            id: format!("nl-{}", i + 1),
            expected: text.clone(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldClass {
    Time,
    Text,
    List,
}

/// The typed fields an executor's readback can observe (v1). A field
/// not in this registry cannot be referenced — the contract fails at
/// freeze time, not after the task ran.
fn field_class(field: &str) -> Option<FieldClass> {
    Some(match field {
        "event.starts_at" | "event.ends_at" | "mail.meeting_time" => FieldClass::Time,
        "event.title" | "event.location" | "mail.from" | "mail.subject" | "mail.body" => {
            FieldClass::Text
        }
        "event.attendees" | "mail.to" => FieldClass::List,
        _ => return None,
    })
}

/// Validate a contract against the plan's steps before it freezes:
/// step references, field names, operator/field-type fit and literal
/// shapes. Anything wrong here is a submission error.
pub fn validate(items: &[AcceptanceItem], steps: &[PlanStep]) -> Result<()> {
    if items.len() > MAX_ACCEPTANCE_ITEMS {
        return Err(Error::Invalid(format!(
            "验收条件最多 {MAX_ACCEPTANCE_ITEMS} 条，实际 {} 条",
            items.len()
        )));
    }
    let step_ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    let mut ids = HashSet::new();
    for item in items {
        if item.id().trim().is_empty() || !ids.insert(item.id()) {
            return Err(Error::Invalid(format!(
                "验收条件 id `{}` 为空或重复",
                item.id()
            )));
        }
        match item {
            AcceptanceItem::Nl { expected, .. } => {
                if expected.trim().is_empty() {
                    return Err(Error::Invalid(format!(
                        "验收条件 {} 的自然语言描述为空",
                        item.id()
                    )));
                }
            }
            AcceptanceItem::Predicate {
                left,
                op,
                right,
                normalize,
                ..
            } => validate_predicate(item.id(), left, *op, right, normalize, &step_ids)?,
        }
    }
    Ok(())
}

fn check_field(id: &str, f: &FieldRef, step_ids: &HashSet<&str>) -> Result<FieldClass> {
    if !step_ids.contains(f.step.as_str()) {
        return Err(Error::Invalid(format!(
            "验收条件 {id} 引用了不存在的步骤 {}",
            f.step
        )));
    }
    field_class(&f.field).ok_or_else(|| {
        Error::Invalid(format!("验收条件 {id} 引用了未知字段 {}", f.field))
    })
}

fn validate_predicate(
    id: &str,
    left: &FieldRef,
    op: PredicateOp,
    right: &Option<Operand>,
    normalize: &Option<String>,
    step_ids: &HashSet<&str>,
) -> Result<()> {
    let lc = check_field(id, left, step_ids)?;
    match normalize.as_deref() {
        None => {}
        Some("rfc3339_tz") if lc == FieldClass::Time => {}
        Some("text") if lc == FieldClass::Text => {}
        Some(other) => {
            return Err(Error::Invalid(format!(
                "验收条件 {id} 的 normalize `{other}` 与字段 {} 不匹配",
                left.field
            )))
        }
    }
    let right_field = right.as_ref().and_then(|r| r.as_field());
    let rc = match &right_field {
        Some(f) => Some(check_field(id, f, step_ids)?),
        None => None,
    };
    let literal = match right {
        Some(Operand::Literal { value }) => Some(value),
        _ => None,
    };
    match op {
        PredicateOp::Exists => {
            if right.is_some() {
                return Err(Error::Invalid(format!(
                    "验收条件 {id} 的 exists 不需要右操作数"
                )));
            }
        }
        PredicateOp::Equals => {
            let Some(_) = right else {
                return Err(Error::Invalid(format!(
                    "验收条件 {id} 的 equals 缺少右操作数"
                )));
            };
            if let Some(rc) = rc {
                if rc != lc {
                    return Err(Error::Invalid(format!(
                        "验收条件 {id} 两侧字段类型不一致"
                    )));
                }
            }
            if let Some(v) = literal {
                check_literal(id, lc, v)?;
            }
        }
        PredicateOp::Contains => {
            if lc != FieldClass::Text {
                return Err(Error::Invalid(format!(
                    "验收条件 {id} 的 contains 左侧必须是文本字段"
                )));
            }
            match (rc, literal) {
                (Some(FieldClass::Text), _) => {}
                (_, Some(Value::String(_))) => {}
                _ => {
                    return Err(Error::Invalid(format!(
                        "验收条件 {id} 的 contains 右侧必须是文本字段或字符串"
                    )))
                }
            }
        }
        PredicateOp::CountAtLeast => {
            if lc != FieldClass::List {
                return Err(Error::Invalid(format!(
                    "验收条件 {id} 的 count_at_least 左侧必须是列表字段"
                )));
            }
            match literal {
                Some(v) if v.as_u64().is_some() => {}
                _ => {
                    return Err(Error::Invalid(format!(
                        "验收条件 {id} 的 count_at_least 右侧必须是非负整数"
                    )))
                }
            }
        }
    }
    Ok(())
}

fn check_literal(id: &str, class: FieldClass, v: &Value) -> Result<()> {
    let ok = match class {
        FieldClass::Text => v.is_string(),
        FieldClass::List => v.is_array(),
        FieldClass::Time => v
            .as_str()
            .is_some_and(|s| hatch_core::time::parse_rfc3339(s).is_ok()),
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "验收条件 {id} 的字面量与字段类型不匹配"
        )))
    }
}

/// Steps whose observed fields the predicates need. Only these steps
/// pay an `observe()` read at the task boundary.
pub fn referenced_steps(items: &[AcceptanceItem]) -> HashSet<String> {
    let mut out = HashSet::new();
    for item in items {
        if let AcceptanceItem::Predicate { left, right, .. } = item {
            out.insert(left.step.clone());
            if let Some(f) = right.as_ref().and_then(|r| r.as_field()) {
                out.insert(f.step);
            }
        }
    }
    out
}

/// One step's observed resource: typed fields from the same
/// independent readback that produced its L1 check. Body-sized text
/// is capped by the executor; the bundle never carries worker prose.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StepEvidence {
    pub resource: String,
    pub source: String,
    pub observed_at: String,
    pub fields: BTreeMap<String, Value>,
}

/// Step id → observed evidence, collected once at the task boundary
/// and shared by L2 now and L3 later.
pub type EvidenceBundle = BTreeMap<String, StepEvidence>;

fn resolve(bundle: &EvidenceBundle, f: &FieldRef) -> Option<Value> {
    bundle.get(&f.step)?.fields.get(&f.field).cloned()
}

fn normalize_value(v: &Value, normalize: Option<&str>) -> std::result::Result<Value, String> {
    match normalize {
        Some("rfc3339_tz") => {
            let s = v.as_str().ok_or("时间字段不是字符串")?;
            let ts = hatch_core::time::parse_rfc3339(s)
                .map_err(|_| format!("时间无法解析：{s}"))?;
            Ok(Value::String(hatch_core::time::to_rfc3339(&ts)))
        }
        Some("text") => Ok(match v {
            Value::String(s) => Value::String(s.replace("\r\n", "\n").trim_end().to_string()),
            other => other.clone(),
        }),
        _ => Ok(v.clone()),
    }
}

/// List equality is multiset equality over stringified elements, so
/// recipient/attendee order never decides a verdict.
fn list_eq(a: &Value, b: &Value) -> bool {
    let norm = |v: &Value| -> Option<Vec<String>> {
        v.as_array().map(|arr| {
            let mut items: Vec<String> = arr
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.trim().to_ascii_lowercase(),
                    other => other.to_string(),
                })
                .collect();
            items.sort();
            items
        })
    };
    match (norm(a), norm(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn brief(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut out: String = s.chars().take(120).collect();
    if s.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// Evaluate one predicate item against the evidence bundle. Missing
/// or unextractable evidence is `Ambiguous` (never guessed); two
/// present-but-different values are `Fail`. `enforced` is the layer's
/// current gate state — batch 1 passes `false` (Observe).
pub fn evaluate(item: &AcceptanceItem, bundle: &EvidenceBundle, enforced: bool) -> EvidenceCheck {
    let rule = format!("cross-step-v1:{}", item.id());
    let unknown = |reason: String| {
        EvidenceCheck::unknown(&rule, reason).with_layer(Layer::L2, enforced)
    };
    let AcceptanceItem::Predicate {
        left,
        op,
        right,
        normalize,
        ..
    } = item
    else {
        return unknown("自然语言条目不由 L2 求值".into());
    };
    let Some(lv_raw) = resolve(bundle, left) else {
        return unknown(format!(
            "步骤 {} 的字段 {} 无观测值，无法判定",
            left.step, left.field
        ));
    };
    let lv = match normalize_value(&lv_raw, normalize.as_deref()) {
        Ok(v) => v,
        Err(e) => return unknown(e),
    };
    let verdict_reason: (Verdict, String) = match op {
        PredicateOp::Exists => {
            let empty = matches!(&lv, Value::String(s) if s.trim().is_empty())
                || matches!(&lv, Value::Array(a) if a.is_empty())
                || lv.is_null();
            if empty {
                (
                    Verdict::Fail,
                    format!("字段 {} 存在但为空", left.field),
                )
            } else {
                (Verdict::Pass, format!("字段 {} 有观测值", left.field))
            }
        }
        PredicateOp::CountAtLeast => {
            let n = right
                .as_ref()
                .and_then(|r| match r {
                    Operand::Literal { value } => value.as_u64(),
                    _ => None,
                })
                .unwrap_or(0);
            match lv.as_array() {
                Some(a) if (a.len() as u64) >= n => (
                    Verdict::Pass,
                    format!("字段 {} 共 {} 项（要求 ≥{n}）", left.field, a.len()),
                ),
                Some(a) => (
                    Verdict::Fail,
                    format!("字段 {} 共 {} 项（要求 ≥{n}）", left.field, a.len()),
                ),
                None => return unknown(format!("字段 {} 不是列表，无法计数", left.field)),
            }
        }
        PredicateOp::Equals | PredicateOp::Contains => {
            let rv_raw = match right {
                Some(Operand::Field { step, field }) => {
                    let f = FieldRef {
                        step: step.clone(),
                        field: field.clone(),
                    };
                    match resolve(bundle, &f) {
                        Some(v) => v,
                        None => {
                            return unknown(format!(
                                "步骤 {} 的字段 {} 无观测值，无法判定",
                                f.step, f.field
                            ))
                        }
                    }
                }
                Some(Operand::Literal { value }) => value.clone(),
                None => return unknown("谓词缺少右操作数".into()),
            };
            let rv = match normalize_value(&rv_raw, normalize.as_deref()) {
                Ok(v) => v,
                Err(e) => return unknown(e),
            };
            match op {
                PredicateOp::Equals => {
                    let eq = if lv.is_array() || rv.is_array() {
                        list_eq(&lv, &rv)
                    } else {
                        lv == rv
                    };
                    if eq {
                        (
                            Verdict::Pass,
                            format!("{} 与右侧一致：{}", left.field, brief(&lv)),
                        )
                    } else {
                        (
                            Verdict::Fail,
                            format!(
                                "{}={} 与右侧 {} 不一致",
                                left.field,
                                brief(&lv),
                                brief(&rv)
                            ),
                        )
                    }
                }
                _ => match (lv.as_str(), rv.as_str()) {
                    (Some(a), Some(b)) if a.contains(b) => (
                        Verdict::Pass,
                        format!("{} 包含 {}", left.field, brief(&rv)),
                    ),
                    (Some(_), Some(_)) => (
                        Verdict::Fail,
                        format!("{} 不包含 {}", left.field, brief(&rv)),
                    ),
                    _ => return unknown("contains 两侧不是文本".into()),
                },
            }
        }
    };
    let (verdict, reason) = verdict_reason;
    EvidenceCheck {
        rule,
        verdict,
        effect: EffectState::Unknown,
        source: "evidence_bundle".into(),
        resource: format!("{}:{}", left.step, left.field),
        expected_digest: crate::verification::digest(
            serde_json::to_string(item).unwrap_or_default().as_bytes(),
        ),
        observed_digest: crate::verification::digest(lv.to_string().as_bytes()),
        reason,
        layer: Layer::L2,
        enforced,
    }
}

/// One observed step resource handed to the L3 grader. `id` is the
/// step id (what verdicts cite); `value` is the step's observed
/// fields — the same bundle L2 evaluates, never worker prose.
#[derive(Clone, Debug)]
pub struct NlEvidence {
    pub id: String,
    pub source: String,
    pub value: Value,
}

/// The grader's verdict on one NL item.
#[derive(Clone, Debug)]
pub struct NlItemVerdict {
    pub id: String,
    pub verdict: Verdict,
    /// Evidence ids (step ids) the grader cites as support.
    pub evidence: Vec<String>,
    pub reason: String,
}

/// The isolated semantic grader (L3), supplied by the caller that
/// owns a model router: hatch-plan never calls a model itself.
/// Called once per task boundary with the whole NL group; never
/// called when the contract has no NL items.
pub type NlGrader<'a> =
    &'a dyn Fn(&str, &[AcceptanceItem], &[NlEvidence], &str) -> Result<Vec<NlItemVerdict>>;

/// The NL items of a contract, in contract order.
pub fn nl_contract_items(items: &[AcceptanceItem]) -> Vec<AcceptanceItem> {
    items
        .iter()
        .filter(|i| matches!(i, AcceptanceItem::Nl { .. }))
        .cloned()
        .collect()
}

/// Digest of the bundle's observed fields (timestamps excluded:
/// re-observing the same state later is the same evidence). L3
/// verdicts are reused by this digest — unchanged evidence reuses
/// the prior grade instead of paying for another model call.
pub fn bundle_digest(bundle: &EvidenceBundle) -> String {
    let canonical: BTreeMap<&String, &BTreeMap<String, Value>> =
        bundle.iter().map(|(k, v)| (k, &v.fields)).collect();
    crate::verification::digest(
        serde_json::to_string(&canonical)
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// The bundle as grader evidence, one entry per observed step.
pub fn nl_evidence(bundle: &EvidenceBundle) -> Vec<NlEvidence> {
    bundle
        .iter()
        .map(|(step, ev)| NlEvidence {
            id: step.clone(),
            source: ev.resource.clone(),
            value: serde_json::to_value(&ev.fields).unwrap_or(Value::Null),
        })
        .collect()
}

/// Map grader verdicts onto report checks. A verdict missing for an
/// item, or citing evidence that was not in the bundle, is
/// `Ambiguous` — the grader's word alone never passes an item.
pub fn nl_checks(
    items: &[AcceptanceItem],
    verdicts: &[NlItemVerdict],
    evidence_ids: &HashSet<String>,
    digest: &str,
    enforced: bool,
) -> Vec<EvidenceCheck> {
    items
        .iter()
        .map(|item| {
            let rule = format!("cross-step-v1:{}", item.id());
            let mut check = match verdicts.iter().find(|v| v.id == item.id()) {
                Some(v)
                    if v.verdict != Verdict::NotApplicable
                        && v.evidence.iter().all(|e| evidence_ids.contains(e))
                        && (v.verdict != Verdict::Pass || !v.evidence.is_empty()) =>
                {
                    EvidenceCheck {
                        verdict: v.verdict.clone(),
                        reason: format!("{}（证据：{}）", v.reason, v.evidence.join("、")),
                        ..EvidenceCheck::unknown(&rule, String::new())
                    }
                }
                _ => EvidenceCheck::unknown(&rule, String::from("语义评分缺少有效裁决或证据引用")),
            }
            .with_layer(Layer::L3, enforced);
            check.source = "semantic_grader".into();
            check.observed_digest = digest.to_string();
            check
        })
        .collect()
}

/// L3 checks from an earlier report over the *same* evidence digest
/// and the *same* NL item set, reusable without a new model call.
pub fn reusable_l3_checks(
    reports: &[crate::verification::VerificationReport],
    digest: &str,
    nl_ids: &HashSet<String>,
) -> Option<Vec<EvidenceCheck>> {
    for report in reports.iter().rev() {
        let l3: Vec<EvidenceCheck> = report
            .checks
            .iter()
            .filter(|c| c.layer == Layer::L3 && c.observed_digest == digest)
            .cloned()
            .collect();
        if l3.is_empty() {
            continue;
        }
        let ids: HashSet<String> = l3
            .iter()
            .filter_map(|c| c.rule.strip_prefix("cross-step-v1:"))
            .map(str::to_string)
            .collect();
        if &ids == nl_ids {
            return Some(l3);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanStep, RiskLevel, StepStatus};
    use serde_json::json;

    fn step(id: &str) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: id.into(),
            tool: "stub".into(),
            arguments: json!({}),
            files: vec![],
            expected: "ok".into(),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }
    }

    fn time_pred() -> AcceptanceItem {
        AcceptanceItem::Predicate {
            id: "cal-mail-time".into(),
            left: FieldRef {
                step: "s1".into(),
                field: "event.starts_at".into(),
            },
            op: PredicateOp::Equals,
            right: Some(Operand::Field {
                step: "s2".into(),
                field: "mail.meeting_time".into(),
            }),
            normalize: Some("rfc3339_tz".into()),
        }
    }

    fn bundle(event_start: &str, mail_time: Option<&str>) -> EvidenceBundle {
        let mut b = EvidenceBundle::new();
        let mut ev = StepEvidence::default();
        ev.fields
            .insert("event.starts_at".into(), json!(event_start));
        b.insert("s1".into(), ev);
        let mut mail = StepEvidence::default();
        if let Some(t) = mail_time {
            mail.fields.insert("mail.meeting_time".into(), json!(t));
        }
        b.insert("s2".into(), mail);
        b
    }

    #[test]
    fn three_way_time_comparison() {
        let item = time_pred();
        // Consistent (same instant, different offset spelling) → Pass.
        let b = bundle("2026-11-22T10:00:00+08:00", Some("2026-11-22T02:00:00Z"));
        assert_eq!(evaluate(&item, &b, false).verdict, Verdict::Pass);
        // Each side observed but contradictory → Fail.
        let b = bundle("2026-11-22T10:00:00+08:00", Some("2026-11-22T11:00:00+08:00"));
        assert_eq!(evaluate(&item, &b, false).verdict, Verdict::Fail);
        // Mail time unconfirmable (absent) → Ambiguous.
        let b = bundle("2026-11-22T10:00:00+08:00", None);
        assert_eq!(evaluate(&item, &b, false).verdict, Verdict::Ambiguous);
    }

    #[test]
    fn validate_rejects_bad_references() {
        let steps = vec![step("s1"), step("s2")];
        assert!(validate(&[time_pred()], &steps).is_ok());
        let bad_step = AcceptanceItem::Predicate {
            id: "x".into(),
            left: FieldRef {
                step: "nope".into(),
                field: "event.starts_at".into(),
            },
            op: PredicateOp::Exists,
            right: None,
            normalize: None,
        };
        assert!(validate(&[bad_step], &steps).is_err());
        let bad_field = AcceptanceItem::Predicate {
            id: "x".into(),
            left: FieldRef {
                step: "s1".into(),
                field: "event.bogus".into(),
            },
            op: PredicateOp::Exists,
            right: None,
            normalize: None,
        };
        assert!(validate(&[bad_field], &steps).is_err());
        let bad_op = AcceptanceItem::Predicate {
            id: "x".into(),
            left: FieldRef {
                step: "s1".into(),
                field: "event.title".into(),
            },
            op: PredicateOp::CountAtLeast,
            right: Some(Operand::Literal { value: json!(1) }),
            normalize: None,
        };
        assert!(validate(&[bad_op], &steps).is_err());
    }

    #[test]
    fn exists_and_count_semantics() {
        let exists = AcceptanceItem::Predicate {
            id: "body".into(),
            left: FieldRef {
                step: "s2".into(),
                field: "mail.body".into(),
            },
            op: PredicateOp::Exists,
            right: None,
            normalize: None,
        };
        let mut b = EvidenceBundle::new();
        let mut mail = StepEvidence::default();
        mail.fields.insert("mail.body".into(), json!(""));
        b.insert("s2".into(), mail);
        assert_eq!(evaluate(&exists, &b, false).verdict, Verdict::Fail);
        let count = AcceptanceItem::Predicate {
            id: "to".into(),
            left: FieldRef {
                step: "s2".into(),
                field: "mail.to".into(),
            },
            op: PredicateOp::CountAtLeast,
            right: Some(Operand::Literal { value: json!(2) }),
            normalize: None,
        };
        let mut mail = StepEvidence::default();
        mail.fields
            .insert("mail.to".into(), json!(["a@x.com", "b@x.com"]));
        b.insert("s2".into(), mail);
        assert_eq!(evaluate(&count, &b, false).verdict, Verdict::Pass);
    }

    fn nl(id: &str) -> AcceptanceItem {
        AcceptanceItem::Nl {
            id: id.into(),
            expected: "邀请邮件写明会议时间".into(),
        }
    }

    #[test]
    fn nl_checks_require_cited_bundle_evidence() {
        let items = vec![nl("nl-1")];
        let ids: HashSet<String> = ["s2".to_string()].into_iter().collect();
        let pass = |evidence: Vec<String>| NlItemVerdict {
            id: "nl-1".into(),
            verdict: Verdict::Pass,
            evidence,
            reason: "正文写明 10:00".into(),
        };
        let ok = nl_checks(&items, &[pass(vec!["s2".into()])], &ids, "d", false);
        assert_eq!(ok[0].verdict, Verdict::Pass);
        assert_eq!(ok[0].layer, Layer::L3);
        assert!(!ok[0].enforced);
        assert_eq!(ok[0].observed_digest, "d");
        // Pass without evidence, or citing evidence outside the
        // bundle, degrades to Ambiguous — never a free pass.
        let no_ev = nl_checks(&items, &[pass(vec![])], &ids, "d", false);
        assert_eq!(no_ev[0].verdict, Verdict::Ambiguous);
        let bad_ev = nl_checks(&items, &[pass(vec!["s9".into()])], &ids, "d", false);
        assert_eq!(bad_ev[0].verdict, Verdict::Ambiguous);
        let missing = nl_checks(&items, &[], &ids, "d", false);
        assert_eq!(missing[0].verdict, Verdict::Ambiguous);
    }

    #[test]
    fn l3_reuse_needs_same_digest_and_item_set() {
        use crate::verification::{VerificationMode, VerificationReport};
        let items = vec![nl("nl-1")];
        let ids: HashSet<String> = ["s2".to_string()].into_iter().collect();
        let checks = nl_checks(
            &items,
            &[NlItemVerdict {
                id: "nl-1".into(),
                verdict: Verdict::Pass,
                evidence: vec!["s2".into()],
                reason: "ok".into(),
            }],
            &ids,
            "digest-a",
            false,
        );
        let report = VerificationReport {
            owner: "p".into(),
            version: 1,
            step_id: "@task-completion".into(),
            contract_digest: "c".into(),
            rules_digest: "r".into(),
            mode: VerificationMode::Enforce,
            attempt: 1,
            observed_at: "2026-11-22T00:00:00Z".into(),
            verdict: Verdict::Pass,
            shadow_verdict: Some(Verdict::Pass),
            checks,
        };
        let nl_ids: HashSet<String> = ["nl-1".to_string()].into_iter().collect();
        let reused = reusable_l3_checks(std::slice::from_ref(&report), "digest-a", &nl_ids)
            .expect("同摘要同条目应复用");
        assert_eq!(reused[0].verdict, Verdict::Pass);
        assert!(
            reusable_l3_checks(std::slice::from_ref(&report), "digest-b", &nl_ids).is_none(),
            "证据变了不得复用旧裁决"
        );
        let other: HashSet<String> = ["nl-2".to_string()].into_iter().collect();
        assert!(reusable_l3_checks(&[report], "digest-a", &other).is_none());
    }
}
