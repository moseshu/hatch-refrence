//! Fixed evaluation set for L3 semantic grading (the Enforce gate
//! for NL items): correct cases, failure cases and
//! insufficient-evidence cases over hand-built evidence bundles,
//! graded by the real isolated grader. Ignored by default — it
//! makes real model calls. Run explicitly with the env model set:
//!
//! ```sh
//! HATCH_LLM_BASE_URL=http://127.0.0.1:18083/compatible-mode/v1 \
//! HATCH_LLM_MODEL=qwen4-max HATCH_LLM_API_KEY=placeholder \
//! cargo test -p hatch-agent --test l3_eval -- --ignored --nocapture
//! ```
//!
//! The gate metric is the false-pass count (expected Fail graded
//! Pass): an Enforce flip needs it at zero on this set, plus a
//! false-block and Ambiguous rate the user accepts.

use hatch_agent::independent_grader::{grade, GraderEvidence, RubricItem};
use hatch_core::CancelToken;
use hatch_llm::provider::OpenAiCompatible;
use hatch_llm::Router;
use hatch_tools::verification::Verdict;
use serde_json::{json, Value};
use std::sync::Arc;

fn ev(id: &str, value: Value) -> GraderEvidence {
    GraderEvidence {
        id: id.into(),
        source: format!("{id}:observed"),
        value,
    }
}

fn event(attendees: Value) -> Value {
    json!({
        "event.title": "项目评审会",
        "event.starts_at": "2026-11-23T02:00:00Z",
        "event.ends_at": "2026-11-23T03:00:00Z",
        "event.location": "3 号会议室",
        "event.attendees": attendees,
    })
}

fn mail(to: Value, subject: &str, body: &str) -> Value {
    json!({
        "mail.from": "smoke@example.com",
        "mail.to": to,
        "mail.subject": subject,
        "mail.body": body,
    })
}

fn file(content: &str) -> Value {
    json!({
        "file.path": "brief.md",
        "file.content": content,
        "file.bytes": content.len(),
    })
}

struct Case {
    name: &'static str,
    rubric: Vec<(&'static str, &'static str)>,
    evidence: Vec<GraderEvidence>,
    expected: Vec<Verdict>,
}

fn cases() -> Vec<Case> {
    let both = json!(["li.ting@corp.example", "han.mei@partner.example"]);
    vec![
        Case {
            name: "time consistent across event and file",
            rubric: vec![("nl-1", "纪要文件写明了会议时间，且与日程的开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s3",
                    file("项目评审会纪要\n会议时间：2026-11-23T10:00:00+08:00\n结论：方案通过。"),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "file time one hour late",
            rubric: vec![("nl-1", "纪要文件写明了会议时间，且与日程的开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s3",
                    file("项目评审会纪要\n会议时间：2026-11-23T11:00:00+08:00\n结论：方案通过。"),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "file has no meeting time",
            rubric: vec![("nl-1", "纪要文件写明了会议时间，且与日程的开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n结论：方案通过。")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "recipients match attendees",
            rubric: vec![("nl-1", "邀请邮件发给了全部参会人")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s2", mail(both.clone(), "邀请：项目评审会", "请准时参加。")),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "one attendee not mailed",
            rubric: vec![("nl-1", "邀请邮件发给了全部参会人")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        json!(["li.ting@corp.example"]),
                        "邀请：项目评审会",
                        "请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "subject names the meeting",
            rubric: vec![("nl-1", "邮件主题写明了会议名称")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(both.clone(), "邀请：项目评审会（11月23日）", "请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "subject unrelated to the meeting",
            rubric: vec![("nl-1", "邮件主题写明了会议名称")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s2", mail(both.clone(), "本周安排", "请准时参加。")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "location consistent in mail body",
            rubric: vec![("nl-1", "邀请邮件写明的地点与日程地点一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        both.clone(),
                        "邀请：项目评审会",
                        "地点：3 号会议室，请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "location differs in mail body",
            rubric: vec![("nl-1", "邀请邮件写明的地点与日程地点一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        both.clone(),
                        "邀请：项目评审会",
                        "地点：5 号会议室，请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            // The file content is fully observed, so the absence of
            // a budget cap is positively shown — Fail, not
            // Ambiguous. (First live run, 2026-11-23: grader said
            // Fail with exactly this reasoning; the original
            // Ambiguous expectation was the wrong one. Ambiguous is
            // for evidence that cannot decide, like the
            // attendance-confirmation case below.)
            name: "budget absent from fully-observed file",
            rubric: vec![("nl-1", "纪要文件记录了预算上限")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n结论：方案通过。")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "attendance confirmation not in evidence",
            rubric: vec![("nl-1", "参会人已经确认出席")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s2", mail(both.clone(), "邀请：项目评审会", "请准时参加。")),
            ],
            expected: vec![Verdict::Ambiguous],
        },
        Case {
            name: "two items graded as one group",
            rubric: vec![
                ("nl-1", "邀请邮件发给了全部参会人"),
                ("nl-2", "邮件主题写明了会议名称"),
            ],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(json!(["li.ting@corp.example"]), "本周安排", "请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Fail, Verdict::Fail],
        },
        // --- Expansion (2026-11-23): patterns the first set did
        // not cover. Expectations follow the same rule as the
        // budget case: fully-observed absence is Fail; a missing
        // evidence entry or an undecidable statement is Ambiguous.
        Case {
            name: "mail body time consistent across timezones",
            rubric: vec![("nl-1", "邀请邮件写明的会议时间与日程开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        both.clone(),
                        "邀请：项目评审会",
                        "会议时间：2026-11-23T10:00:00+08:00，请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "mail body time one day late",
            rubric: vec![("nl-1", "邀请邮件写明的会议时间与日程开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        both.clone(),
                        "邀请：项目评审会",
                        "会议时间：2026-11-24T10:00:00+08:00，请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "mail sent to a non-attendee only",
            rubric: vec![("nl-1", "邀请邮件发给了全部参会人")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(json!(["wang.yu@corp.example"]), "邀请：项目评审会", "请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "mail also sent beyond the attendees",
            rubric: vec![("nl-1", "邀请邮件只发给了参会人")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        json!([
                            "li.ting@corp.example",
                            "han.mei@partner.example",
                            "wang.yu@corp.example"
                        ]),
                        "邀请：项目评审会",
                        "请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "recipients in reverse order still match",
            rubric: vec![("nl-1", "邀请邮件发给了全部参会人")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        json!(["han.mei@partner.example", "li.ting@corp.example"]),
                        "邀请：项目评审会",
                        "请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "file duration contradicts the event",
            rubric: vec![("nl-1", "纪要写明的会议时长与日程一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n会议时长：2 小时\n结论：方案通过。")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "budget recorded in the file",
            rubric: vec![("nl-1", "纪要文件记录了预算上限")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n预算上限：50 万元\n结论：方案通过。")),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "conclusion recorded as passed",
            rubric: vec![("nl-1", "纪要记录了方案通过的结论")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n结论：方案通过。")),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            name: "conclusion recorded as undecided",
            rubric: vec![("nl-1", "纪要记录了方案通过的结论")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s3", file("项目评审会纪要\n结论：方案待定，下次再议。")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            // No mail evidence entry at all: whether the mail was
            // sent cannot be decided from this bundle.
            name: "no mail evidence in the bundle",
            rubric: vec![("nl-1", "邀请邮件发给了全部参会人")],
            evidence: vec![ev("s1", event(both.clone()))],
            expected: vec![Verdict::Ambiguous],
        },
        Case {
            name: "no file evidence in the bundle",
            rubric: vec![("nl-1", "纪要文件写明了会议时间，且与日程的开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s2", mail(both.clone(), "邀请：项目评审会", "请准时参加。")),
            ],
            expected: vec![Verdict::Ambiguous],
        },
        Case {
            // "上午" is compatible with 10:00 but does not establish
            // the same start time — undecidable, not a contradiction.
            name: "mail body time too vague to confirm",
            rubric: vec![("nl-1", "邀请邮件写明的会议时间与日程开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(both.clone(), "邀请：项目评审会", "会议时间：11月23日上午，请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Ambiguous],
        },
        Case {
            // Reverse guard for the granularity rule: the mail
            // states the precise start time and it matches — this
            // must stay Pass, not collapse into Ambiguous.
            name: "mail body precise time matches event",
            rubric: vec![("nl-1", "邀请邮件写明的会议时间与日程开始时间一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(
                        both.clone(),
                        "邀请：项目评审会",
                        "会议时间：2026-11-23T10:00:00+08:00，请准时参加。",
                    ),
                ),
            ],
            expected: vec![Verdict::Pass],
        },
        Case {
            // Granularity is not time-specific: a room-less
            // location statement cannot confirm the exact room.
            name: "mail body location too coarse to confirm",
            rubric: vec![("nl-1", "邀请邮件写明的地点与日程地点一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(both.clone(), "邀请：项目评审会", "地点：公司会议室，请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Ambiguous],
        },
        Case {
            // The body is fully observed and empty: the missing
            // location is positively shown (budget-case logic).
            name: "empty mail body has no location",
            rubric: vec![("nl-1", "邀请邮件写明的地点与日程地点一致")],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev("s2", mail(both.clone(), "邀请：项目评审会", "")),
            ],
            expected: vec![Verdict::Fail],
        },
        Case {
            name: "two items, one passes one fails",
            rubric: vec![
                ("nl-1", "邀请邮件发给了全部参会人"),
                ("nl-2", "邀请邮件写明的地点与日程地点一致"),
            ],
            evidence: vec![
                ev("s1", event(both.clone())),
                ev(
                    "s2",
                    mail(both.clone(), "邀请：项目评审会", "地点：5 号会议室，请准时参加。"),
                ),
            ],
            expected: vec![Verdict::Pass, Verdict::Fail],
        },
    ]
}

const CANDIDATE: &str = "step s1 (propose_event): event evt_eval created （项目评审会）\n\
    step s2 (propose_email): sent msg_eval to li.ting@corp.example\n\
    step s3 (fs_write): 已写入 brief.md";

#[test]
#[ignore = "live model eval; run explicitly with the env model set"]
fn l3_eval_set_live() {
    let base = std::env::var("HATCH_LLM_BASE_URL").expect("set HATCH_LLM_BASE_URL");
    let model = std::env::var("HATCH_LLM_MODEL").expect("set HATCH_LLM_MODEL");
    let key = std::env::var("HATCH_LLM_API_KEY").unwrap_or_else(|_| "placeholder".into());
    let router = Router::new(vec![Arc::new(OpenAiCompatible::new(
        "eval", &base, &model, key,
    ))]);
    let cancel = CancelToken::new();
    let (mut correct, mut false_pass, mut false_block, mut amb_decidable) = (0, 0, 0, 0);
    let (mut prompt_tokens, mut completion_tokens) = (0u64, 0u64);
    let mut total = 0;
    for case in cases() {
        let rubric: Vec<RubricItem> = case
            .rubric
            .iter()
            .map(|(id, expected)| RubricItem {
                id: id.to_string(),
                expected: expected.to_string(),
            })
            .collect();
        let (grade, usage) = grade(
            &router,
            "创建项目评审会日程，发送邀请邮件，并把会议时间写进纪要文件",
            &rubric,
            &case.evidence,
            CANDIDATE,
            &cancel,
        )
        .unwrap_or_else(|e| panic!("grader failed on {}: {e}", case.name));
        prompt_tokens += usage.prompt_tokens;
        completion_tokens += usage.completion_tokens;
        for (item, expected) in grade.items.iter().zip(&case.expected) {
            total += 1;
            let mark = if &item.verdict == expected {
                "ok"
            } else {
                "MISMATCH"
            };
            println!(
                "[{mark}] {} / {}: expected {:?}, got {:?} — {}",
                case.name, item.id, expected, item.verdict, item.reason
            );
            match (expected, &item.verdict) {
                (a, b) if a == b => correct += 1,
                (Verdict::Fail, Verdict::Pass) => false_pass += 1,
                (Verdict::Pass, Verdict::Fail) => false_block += 1,
                (Verdict::Pass | Verdict::Fail, Verdict::Ambiguous) => amb_decidable += 1,
                _ => {}
            }
        }
    }
    println!(
        "L3 eval: {correct}/{total} correct, false-pass {false_pass}, \
         false-block {false_block}, ambiguous-on-decidable {amb_decidable}; \
         usage {prompt_tokens} prompt + {completion_tokens} completion tokens"
    );
    assert_eq!(false_pass, 0, "grader must never pass a failing case");
}
