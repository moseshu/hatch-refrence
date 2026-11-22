//! Candidate answers are withheld when an applied effect lacks independent proof.
//! The guard has no worker reasoning context and cannot perform writes.
use crate::{Agent, TurnReport};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_plan::verification::{
    digest, Executed, VerificationMode, VerificationReport, VerificationStore, MAX_OBSERVATIONS,
    RULESET,
};
use hatch_policy::{ActionLedger, ApprovalQueue, ApprovalStatus};
use hatch_store::{params, DbResultExt};
use hatch_tools::verification::{EvidenceCheck, Verdict};
use hatch_tools::ExecContext;

pub(crate) struct Decision {
    pub text: Option<String>,
    pub blocked: bool,
}

/// Everything the turn-end verification needs besides the agent and
/// the mutable report.
pub(crate) struct CheckInput<'a> {
    pub session: &'a str,
    pub goal: &'a str,
    pub candidate: &'a str,
    pub direct_effects: &'a [String],
    pub now: &'a Timestamp,
    pub cancel: &'a CancelToken,
}

pub(crate) fn check(
    agent: &Agent<'_>,
    input: &CheckInput,
    report: &mut TurnReport,
) -> Result<Decision> {
    let CheckInput {
        session,
        direct_effects,
        now,
        cancel,
        ..
    } = *input;
    let owner = format!("chat:{session}");
    let store = VerificationStore::new(agent.db);
    // Include approvals from interrupted/earlier turns. A fresh model context
    // cannot erase an unverified effect by forgetting its approval id.
    let ids = {
        let c = agent.db.conn();
        let mut q = c.prepare("SELECT id FROM approvals WHERE origin=?1 AND status IN ('executed','needs_review','pending','approved') ORDER BY created_at").db()?;
        let rows = q
            .query_map(
                params![agent.config.origin.as_deref().unwrap_or(&owner)],
                |r| r.get::<_, String>(0),
            )
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>()
            .db()?
    };
    let mut ids = ids;
    for id in &report.approvals {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    // A retry supersedes the failed attempt it replaced: approvals
    // sharing a base idempotency key (`K`, `K#retry-…`) are one logical
    // action, and only the latest executed attempt speaks for it. An
    // earlier attempt's error receipt must not block the turn forever
    // once a retry has succeeded.
    let superseded: std::collections::HashSet<String> = {
        let mut latest_by_base: std::collections::HashMap<String, String> = Default::default();
        let mut executed: Vec<(String, String)> = Vec::new();
        for id in &ids {
            if let Ok(a) = ApprovalQueue::new(agent.db).get(id) {
                if a.status == ApprovalStatus::Executed {
                    let base = a.idempotency_key.split("#retry-").next().unwrap_or("").to_string();
                    latest_by_base.insert(base.clone(), id.clone());
                    executed.push((base, id.clone()));
                }
            }
        }
        executed
            .into_iter()
            .filter(|(base, id)| latest_by_base.get(base) != Some(id))
            .map(|(_, id)| id)
            .collect()
    };
    let mut blockers = Vec::new();
    let mut pending = 0;
    let mut semantic_evidence = Vec::new();
    for id in ids {
        cancel.check()?;
        if superseded.contains(&id) {
            continue;
        }
        let a = ApprovalQueue::new(agent.db).get(&id)?;
        if matches!(a.status, ApprovalStatus::Pending | ApprovalStatus::Approved) {
            pending += 1;
            continue;
        }
        if a.status == ApprovalStatus::NeedsReview {
            blockers.push(format!("审批 {id} 执行结果不明，需要人工核对"));
            continue;
        }
        if a.status != ApprovalStatus::Executed {
            continue;
        }
        let contract = serde_json::json!({"rules":digest(RULESET.as_bytes()),"payload":a.payload,"kind":a.kind}).to_string();
        let mode = VerificationMode::configured()?;
        if store.begin(&owner, 0, &id, &contract, mode, now)? {
            let output = ActionLedger::new(agent.db)
                .result(&a.idempotency_key)?
                .unwrap_or_default();
            store.executed(&Executed {
                owner: &owner,
                version: 0,
                step: &id,
                output: &output,
                receipt: &a.idempotency_key,
                approval: &id,
                now,
            })?;
        }
        let mut record = store.get(&owner, 0, &id)?.unwrap();
        if record.phase == "dispatching" {
            let output = ActionLedger::new(agent.db)
                .result(&a.idempotency_key)?
                .unwrap_or_default();
            store.executed(&Executed {
                owner: &owner,
                version: 0,
                step: &id,
                output: &output,
                receipt: &a.idempotency_key,
                approval: &id,
                now,
            })?;
            record = store.get(&owner, 0, &id)?.unwrap();
        }
        if record.contract != contract {
            blockers.push(format!("审批 {id} 的校验规则已变化，需要重新核对"));
            continue;
        }
        let mut latest = store.latest(&owner, 0, &id)?;
        if !latest.as_ref().is_some_and(|r| r.accepted()) {
            for attempt in record.attempts + 1..=MAX_OBSERVATIONS {
                cancel.check()?;
                let check = if record.mode == VerificationMode::Off {
                    EvidenceCheck {
                        verdict: Verdict::NotApplicable,
                        ..EvidenceCheck::unknown("verification-off", "验证已关闭")
                    }
                } else if !a.payload_intact() {
                    EvidenceCheck::unknown("approval-integrity", "审批内容被修改")
                } else if let Some(exec) = agent.executors.get(&a.kind) {
                    exec.verify(
                        &a.payload,
                        &record.output,
                        &ExecContext {
                            db: agent.db,
                            now: *now,
                            approval_id: &id,
                            origin: &a.origin,
                            cancel: Some(cancel),
                        },
                    )
                    .unwrap_or_else(|e| {
                        EvidenceCheck::unknown("readback-error", format!("独立回读失败：{e}"))
                    })
                } else {
                    EvidenceCheck::unknown("executor-missing", "当前没有对应账户的读取执行器")
                };
                let retry =
                    check.verdict == Verdict::Ambiguous && record.mode == VerificationMode::Enforce;
                let r = VerificationReport {
                    owner: owner.clone(),
                    version: 0,
                    step_id: id.clone(),
                    contract_digest: digest(contract.as_bytes()),
                    rules_digest: digest(RULESET.as_bytes()),
                    mode: record.mode,
                    attempt,
                    observed_at: to_rfc3339(&hatch_core::time::now()),
                    verdict: check.verdict.clone(),
                    shadow_verdict: Some(check.verdict.clone()),
                    checks: vec![check],
                };
                store.save(&r)?;
                latest = Some(r);
                if !retry {
                    break;
                }
            }
        }
        if let Some(r) = latest {
            if !r.accepted() {
                blockers.push(r.reason());
            }
            if r.verdict == Verdict::Pass && report.approvals.contains(&id) && a.payload_intact() {
                semantic_evidence.push(crate::independent_grader::GraderEvidence {
                    id: id.clone(),
                    source: "独立回读已确认与批准内容一致".into(),
                    value: a.payload.clone(),
                });
            }
            report.verification.push(r);
        } else {
            blockers.push(format!("审批 {id} 没有验证报告"));
        }
    }
    for (key, version, record) in store.records(&owner)? {
        if key.starts_with("direct:") && record.mode == VerificationMode::Enforce {
            blockers.push(format!(
                "直接动作 {key} 缺少独立验收记录（{}）；需要人工核对",
                record.phase
            ));
        }
        let _ = version;
    }
    // Direct state-changing tools have no immutable approval payload to read
    // back against. They must not inherit success from the model's answer.
    if VerificationMode::configured()? == VerificationMode::Enforce {
        blockers.extend(direct_effects.iter().cloned());
    }
    if !blockers.is_empty() {
        return Ok(Decision {
            text: Some(format!(
                "任务尚未通过验证：{}。已经生效的动作不会自动重做。",
                blockers.join("；")
            )),
            blocked: true,
        });
    }
    if pending > 0 {
        return Ok(Decision {
            text: Some(format!(
                "已有 {pending} 项动作等待审批或执行，任务尚未完成；可在审批列表中查看。"
            )),
            blocked: false,
        });
    }
    if !semantic_evidence.is_empty()
        && std::env::var("HATCH_SEMANTIC_VERIFICATION").ok().as_deref() == Some("1")
    {
        let text = semantic_grade(agent, input, semantic_evidence, report)?;
        return Ok(Decision {
            blocked: text.is_some(),
            text,
        });
    }
    Ok(Decision {
        text: None,
        blocked: false,
    })
}

/// Record a direct effect intent before dispatch so a process loss cannot
/// erase an unverified mutation. Approvals carry their own durable ledger.
pub(crate) fn intent(
    agent: &Agent<'_>,
    session: &str,
    call: &hatch_llm::ToolCall,
    now: &Timestamp,
) -> Result<Option<String>> {
    if !agent
        .tools
        .get(&call.name)
        .is_some_and(|t| hatch_tools::verification::requires_evidence(t.class()))
    {
        return Ok(None);
    }
    let owner = format!("chat:{session}");
    let key = format!("direct:{}", hatch_core::ids::random_id("effect"));
    let contract = serde_json::json!({"tool":call.name,"arguments":call.arguments,"rules":digest(RULESET.as_bytes())}).to_string();
    VerificationStore::new(agent.db).begin(
        &owner,
        0,
        &key,
        &contract,
        VerificationMode::configured()?,
        now,
    )?;
    Ok(Some(key))
}

pub(crate) fn settle_intent(
    agent: &Agent<'_>,
    session: &str,
    key: Option<&str>,
    output: &str,
    has_approval: bool,
    now: &Timestamp,
) -> Result<()> {
    let Some(key) = key else {
        return Ok(());
    };
    let owner = format!("chat:{session}");
    let store = VerificationStore::new(agent.db);
    if has_approval || output.starts_with("ERROR:") {
        store.rejected(&owner, 0, key)
    } else {
        store.executed(&Executed {
            owner: &owner,
            version: 0,
            step: key,
            output,
            receipt: "",
            approval: "",
            now,
        })
    }
}

fn semantic_grade(
    agent: &Agent<'_>,
    input: &CheckInput,
    evidence: Vec<crate::independent_grader::GraderEvidence>,
    report: &mut TurnReport,
) -> Result<Option<String>> {
    let CheckInput {
        session,
        goal,
        candidate,
        now,
        cancel,
        ..
    } = *input;
    use crate::independent_grader::{grade, RubricItem};
    let rubric = vec![RubricItem {
        id: "user-goal".into(),
        expected: goal.into(),
    }];
    let key = format!("semantic:{}", hatch_core::ids::random_id("grade"));
    let owner = format!("chat:{session}");
    let contract = serde_json::json!({"goal":goal,"rubric":rubric,"evidence_ids":evidence.iter().map(|e| &e.id).collect::<Vec<_>>()}).to_string();
    let mode = VerificationMode::configured()?;
    let store = VerificationStore::new(agent.db);
    store.begin(&owner, 0, &key, &contract, mode, now)?;
    store.executed(&Executed {
        owner: &owner,
        version: 0,
        step: &key,
        output: candidate,
        receipt: "",
        approval: "",
        now,
    })?;
    report.model_calls += 1;
    let checks = match grade(agent.router, goal, &rubric, &evidence, candidate, cancel) {
        Ok((graded, usage)) => {
            crate::report::add_usage(&mut report.usage, &usage);
            graded
                .items
                .into_iter()
                .map(|item| EvidenceCheck {
                    rule: format!("semantic-rubric-v1:{}", item.id),
                    verdict: item.verdict,
                    source: "isolated_grader".into(),
                    resource: item.evidence.join(","),
                    reason: item.reason,
                    expected_digest: digest(goal.as_bytes()),
                    ..EvidenceCheck::unknown("semantic-rubric-v1", "")
                })
                .collect::<Vec<_>>()
        }
        Err(e) => {
            cancel.check()?;
            vec![EvidenceCheck::unknown(
                "semantic-rubric-v1",
                format!("独立 grader 未能可靠裁决：{e}"),
            )]
        }
    };
    let verdict = if checks.iter().any(|c| c.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if checks.iter().any(|c| c.verdict == Verdict::Ambiguous) {
        Verdict::Ambiguous
    } else {
        Verdict::Pass
    };
    let r = VerificationReport {
        owner,
        version: 0,
        step_id: key,
        contract_digest: digest(contract.as_bytes()),
        rules_digest: digest(RULESET.as_bytes()),
        mode,
        attempt: 1,
        observed_at: to_rfc3339(&hatch_core::time::now()),
        shadow_verdict: Some(verdict.clone()),
        verdict,
        checks,
    };
    store.save(&r)?;
    let blocked = if r.accepted() {
        None
    } else {
        Some(format!(
            "任务未通过独立语义验收：{}。请核对，不会自动重复已执行动作。",
            r.reason()
        ))
    };
    report.verification.push(r);
    Ok(blocked)
}
