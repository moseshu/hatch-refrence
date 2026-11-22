//! Fixed evaluation set for L2 cross-step predicates (the Enforce
//! gate): correct cases, failure cases and undecidable cases per
//! predicate class, evaluated deterministically over hand-built
//! evidence bundles. Zero model calls. A mismatch here is either a
//! regression in `evaluate` or a wrong expectation in this table —
//! either way it must be resolved deliberately, never by flipping
//! the expectation to match.

use hatch_plan::acceptance::{
    evaluate, validate, AcceptanceItem, EvidenceBundle, FieldRef, Operand, PredicateOp,
};
use hatch_plan::{PlanStep, RiskLevel, StepStatus};
use hatch_tools::verification::Verdict;
use serde_json::{json, Value};

fn bundle(entries: &[(&str, &str, Value)]) -> EvidenceBundle {
    let mut b = EvidenceBundle::new();
    for (step, field, value) in entries {
        b.entry(step.to_string())
            .or_default()
            .fields
            .insert(field.to_string(), value.clone());
    }
    b
}

fn pred(
    id: &str,
    left: (&str, &str),
    op: PredicateOp,
    right: Option<Operand>,
    normalize: Option<&str>,
) -> AcceptanceItem {
    AcceptanceItem::Predicate {
        id: id.into(),
        left: FieldRef {
            step: left.0.into(),
            field: left.1.into(),
        },
        op,
        right,
        normalize: normalize.map(str::to_string),
    }
}

fn field(step: &str, field: &str) -> Option<Operand> {
    Some(Operand::Field {
        step: step.into(),
        field: field.into(),
    })
}

fn lit(v: Value) -> Option<Operand> {
    Some(Operand::Literal { value: v })
}

struct Case {
    class: &'static str,
    name: &'static str,
    item: AcceptanceItem,
    bundle: EvidenceBundle,
    expected: Verdict,
}

fn cases() -> Vec<Case> {
    let ev = |start: &str| ("s1", "event.starts_at", json!(start));
    let mail_t = |t: &str| ("s2", "mail.meeting_time", json!(t));
    vec![
        // --- equals / time (rfc3339_tz) ---
        Case {
            class: "equals-time",
            name: "same instant, different zone",
            item: pred(
                "c",
                ("s1", "event.starts_at"),
                PredicateOp::Equals,
                field("s2", "mail.meeting_time"),
                Some("rfc3339_tz"),
            ),
            bundle: bundle(&[
                ev("2026-11-23T02:00:00Z"),
                mail_t("2026-11-23T10:00:00+08:00"),
            ]),
            expected: Verdict::Pass,
        },
        Case {
            class: "equals-time",
            name: "one hour apart",
            item: pred(
                "c",
                ("s1", "event.starts_at"),
                PredicateOp::Equals,
                field("s2", "mail.meeting_time"),
                Some("rfc3339_tz"),
            ),
            bundle: bundle(&[
                ev("2026-11-23T02:00:00Z"),
                mail_t("2026-11-23T11:00:00+08:00"),
            ]),
            expected: Verdict::Fail,
        },
        Case {
            class: "equals-time",
            name: "mail time not extracted",
            item: pred(
                "c",
                ("s1", "event.starts_at"),
                PredicateOp::Equals,
                field("s2", "mail.meeting_time"),
                Some("rfc3339_tz"),
            ),
            bundle: bundle(&[
                ev("2026-11-23T02:00:00Z"),
                ("s2", "mail.subject", json!("邀请")),
            ]),
            expected: Verdict::Ambiguous,
        },
        Case {
            class: "equals-time",
            name: "unparseable observed time",
            item: pred(
                "c",
                ("s1", "event.starts_at"),
                PredicateOp::Equals,
                field("s2", "mail.meeting_time"),
                Some("rfc3339_tz"),
            ),
            bundle: bundle(&[ev("2026-11-23T02:00:00Z"), mail_t("明天上午十点")]),
            expected: Verdict::Ambiguous,
        },
        Case {
            class: "equals-time",
            name: "literal instant, same zone",
            item: pred(
                "c",
                ("s1", "event.starts_at"),
                PredicateOp::Equals,
                lit(json!("2026-11-23T10:00:00+08:00")),
                Some("rfc3339_tz"),
            ),
            bundle: bundle(&[ev("2026-11-23T02:00:00Z")]),
            expected: Verdict::Pass,
        },
        // --- equals / text ---
        Case {
            class: "equals-text",
            name: "identical titles",
            item: pred(
                "c",
                ("s1", "event.title"),
                PredicateOp::Equals,
                lit(json!("项目评审会")),
                None,
            ),
            bundle: bundle(&[("s1", "event.title", json!("项目评审会"))]),
            expected: Verdict::Pass,
        },
        Case {
            class: "equals-text",
            name: "different titles",
            item: pred(
                "c",
                ("s1", "event.title"),
                PredicateOp::Equals,
                lit(json!("项目评审会")),
                None,
            ),
            bundle: bundle(&[("s1", "event.title", json!("预算会"))]),
            expected: Verdict::Fail,
        },
        Case {
            class: "equals-text",
            name: "CRLF and trailing space under text normalize",
            item: pred(
                "c",
                ("s1", "file.content"),
                PredicateOp::Equals,
                lit(json!("纪要\n结论")),
                Some("text"),
            ),
            bundle: bundle(&[("s1", "file.content", json!("纪要\r\n结论  "))]),
            expected: Verdict::Pass,
        },
        Case {
            class: "equals-text",
            name: "subject vs title field-to-field",
            item: pred(
                "c",
                ("s2", "mail.subject"),
                PredicateOp::Equals,
                field("s1", "event.title"),
                None,
            ),
            bundle: bundle(&[
                ("s1", "event.title", json!("项目评审会")),
                ("s2", "mail.subject", json!("项目评审会")),
            ]),
            expected: Verdict::Pass,
        },
        // --- equals / list ---
        Case {
            class: "equals-list",
            name: "recipients equal attendees, order differs",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::Equals,
                field("s1", "event.attendees"),
                None,
            ),
            bundle: bundle(&[
                ("s1", "event.attendees", json!(["a@x.com", "b@x.com"])),
                ("s2", "mail.to", json!(["b@x.com", "a@x.com"])),
            ]),
            expected: Verdict::Pass,
        },
        Case {
            class: "equals-list",
            name: "one attendee not mailed",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::Equals,
                field("s1", "event.attendees"),
                None,
            ),
            bundle: bundle(&[
                ("s1", "event.attendees", json!(["a@x.com", "b@x.com"])),
                ("s2", "mail.to", json!(["a@x.com"])),
            ]),
            expected: Verdict::Fail,
        },
        Case {
            class: "equals-list",
            name: "case-insensitive addresses",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::Equals,
                field("s1", "event.attendees"),
                None,
            ),
            bundle: bundle(&[
                ("s1", "event.attendees", json!(["A@X.com"])),
                ("s2", "mail.to", json!(["a@x.com"])),
            ]),
            expected: Verdict::Pass,
        },
        // --- equals / number ---
        Case {
            class: "equals-number",
            name: "byte count matches",
            item: pred(
                "c",
                ("s1", "file.bytes"),
                PredicateOp::Equals,
                lit(json!(87)),
                None,
            ),
            bundle: bundle(&[("s1", "file.bytes", json!(87))]),
            expected: Verdict::Pass,
        },
        Case {
            class: "equals-number",
            name: "byte count differs",
            item: pred(
                "c",
                ("s1", "file.bytes"),
                PredicateOp::Equals,
                lit(json!(87)),
                None,
            ),
            bundle: bundle(&[("s1", "file.bytes", json!(46))]),
            expected: Verdict::Fail,
        },
        // --- contains ---
        Case {
            class: "contains",
            name: "file content contains the instant",
            item: pred(
                "c",
                ("s1", "file.content"),
                PredicateOp::Contains,
                lit(json!("2026-11-23T10:00:00+08:00")),
                None,
            ),
            bundle: bundle(&[(
                "s1",
                "file.content",
                json!("会议时间：2026-11-23T10:00:00+08:00"),
            )]),
            expected: Verdict::Pass,
        },
        Case {
            class: "contains",
            name: "file content lacks the instant",
            item: pred(
                "c",
                ("s1", "file.content"),
                PredicateOp::Contains,
                lit(json!("2026-11-23T10:00:00+08:00")),
                None,
            ),
            bundle: bundle(&[(
                "s1",
                "file.content",
                json!("会议时间：2026-11-23T11:00:00+08:00"),
            )]),
            expected: Verdict::Fail,
        },
        Case {
            class: "contains",
            name: "mail body contains the title field",
            item: pred(
                "c",
                ("s2", "mail.body"),
                PredicateOp::Contains,
                field("s1", "event.title"),
                None,
            ),
            bundle: bundle(&[
                ("s1", "event.title", json!("项目评审会")),
                (
                    "s2",
                    "mail.body",
                    json!("邀请您参加项目评审会，请准时出席。"),
                ),
            ]),
            expected: Verdict::Pass,
        },
        Case {
            class: "contains",
            name: "left field unobserved",
            item: pred(
                "c",
                ("s1", "file.content"),
                PredicateOp::Contains,
                lit(json!("x")),
                None,
            ),
            bundle: bundle(&[("s1", "file.path", json!("brief.md"))]),
            expected: Verdict::Ambiguous,
        },
        // --- exists ---
        Case {
            class: "exists",
            name: "location set",
            item: pred(
                "c",
                ("s1", "event.location"),
                PredicateOp::Exists,
                None,
                None,
            ),
            bundle: bundle(&[("s1", "event.location", json!("3 号会议室"))]),
            expected: Verdict::Pass,
        },
        Case {
            class: "exists",
            name: "location empty string",
            item: pred(
                "c",
                ("s1", "event.location"),
                PredicateOp::Exists,
                None,
                None,
            ),
            bundle: bundle(&[("s1", "event.location", json!(""))]),
            expected: Verdict::Fail,
        },
        Case {
            class: "exists",
            name: "attendees empty list",
            item: pred(
                "c",
                ("s1", "event.attendees"),
                PredicateOp::Exists,
                None,
                None,
            ),
            bundle: bundle(&[("s1", "event.attendees", json!([]))]),
            expected: Verdict::Fail,
        },
        Case {
            class: "exists",
            name: "field not observed at all",
            item: pred(
                "c",
                ("s1", "event.location"),
                PredicateOp::Exists,
                None,
                None,
            ),
            bundle: bundle(&[("s1", "event.title", json!("项目评审会"))]),
            expected: Verdict::Ambiguous,
        },
        // --- count_at_least ---
        Case {
            class: "count-at-least",
            name: "two recipients, need two",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::CountAtLeast,
                lit(json!(2)),
                None,
            ),
            bundle: bundle(&[("s2", "mail.to", json!(["a@x.com", "b@x.com"]))]),
            expected: Verdict::Pass,
        },
        Case {
            class: "count-at-least",
            name: "one recipient, need two",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::CountAtLeast,
                lit(json!(2)),
                None,
            ),
            bundle: bundle(&[("s2", "mail.to", json!(["a@x.com"]))]),
            expected: Verdict::Fail,
        },
        Case {
            class: "count-at-least",
            name: "zero required always passes",
            item: pred(
                "c",
                ("s2", "mail.to"),
                PredicateOp::CountAtLeast,
                lit(json!(0)),
                None,
            ),
            bundle: bundle(&[("s2", "mail.to", json!([]))]),
            expected: Verdict::Pass,
        },
    ]
}

#[test]
fn l2_eval_set_matches_expectations() {
    let mut false_block = 0;
    let mut false_pass = 0;
    let mut ambiguous_on_decidable = 0;
    let mut wrong = Vec::new();
    for case in cases() {
        let got = evaluate(&case.item, &case.bundle, false).verdict;
        if got != case.expected {
            match (&case.expected, &got) {
                (Verdict::Pass, Verdict::Fail) => false_block += 1,
                (Verdict::Fail, Verdict::Pass) => false_pass += 1,
                (Verdict::Pass | Verdict::Fail, Verdict::Ambiguous) => ambiguous_on_decidable += 1,
                _ => {}
            }
            wrong.push(format!(
                "[{}] {}: expected {:?}, got {:?}",
                case.class, case.name, case.expected, got
            ));
        }
    }
    println!(
        "L2 eval: {} cases, false-block {false_block}, false-pass {false_pass}, \
         ambiguous-on-decidable {ambiguous_on_decidable}",
        cases().len()
    );
    assert!(wrong.is_empty(), "eval mismatches:\n{}", wrong.join("\n"));
}

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

/// Freeze-time validation is the first line of the eval set: a bad
/// contract must die at submission, not after the task ran.
#[test]
fn freeze_validation_rejects_bad_contracts() {
    let steps = vec![step("s1"), step("s2")];
    let bad = [
        // unknown field
        pred("c", ("s1", "event.color"), PredicateOp::Exists, None, None),
        // unknown step
        pred("c", ("s9", "event.title"), PredicateOp::Exists, None, None),
        // contains on a list field
        pred(
            "c",
            ("s2", "mail.to"),
            PredicateOp::Contains,
            lit(json!("a")),
            None,
        ),
        // count_at_least on text
        pred(
            "c",
            ("s1", "event.title"),
            PredicateOp::CountAtLeast,
            lit(json!(1)),
            None,
        ),
        // equals with mismatched classes
        pred(
            "c",
            ("s1", "event.title"),
            PredicateOp::Equals,
            field("s2", "mail.to"),
            None,
        ),
        // time literal without offset
        pred(
            "c",
            ("s1", "event.starts_at"),
            PredicateOp::Equals,
            lit(json!("2026-11-23 10:00")),
            Some("rfc3339_tz"),
        ),
        // normalize on the wrong class
        pred(
            "c",
            ("s1", "event.title"),
            PredicateOp::Equals,
            lit(json!("x")),
            Some("rfc3339_tz"),
        ),
        // exists with a right operand
        pred(
            "c",
            ("s1", "event.title"),
            PredicateOp::Exists,
            lit(json!("x")),
            None,
        ),
    ];
    for item in &bad {
        assert!(
            validate(std::slice::from_ref(item), &steps).is_err(),
            "should reject: {item:?}"
        );
    }
    // duplicate ids
    let dup = vec![
        pred("c", ("s1", "event.title"), PredicateOp::Exists, None, None),
        pred("c", ("s1", "event.title"), PredicateOp::Exists, None, None),
    ];
    assert!(validate(&dup, &steps).is_err());
    // a well-formed contract passes
    let good = vec![pred(
        "c",
        ("s1", "event.starts_at"),
        PredicateOp::Equals,
        field("s2", "mail.meeting_time"),
        Some("rfc3339_tz"),
    )];
    assert!(validate(&good, &steps).is_ok());
}
