//! Candidate answers are withheld when an applied effect lacks independent proof.
//! The guard has no worker reasoning context and cannot perform writes.
use crate::{Agent, TurnReport};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_plan::acceptance::{self as acc, AcceptanceEnforce, AcceptanceItem, NlItemVerdict};
use hatch_plan::verification::{
    digest, gate_verdict, verdict as aggregate_verdict, Executed, VerificationMode,
    VerificationReport, VerificationStore, MAX_OBSERVATIONS, RULESET,
};
use hatch_plan::PlanStore;
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
    pub turn_id: &'a str,
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
        turn_id,
        direct_effects,
        now,
        cancel,
        ..
    } = *input;
    let owner = format!("chat:{session}");
    let store = VerificationStore::new(agent.db);
    // Keep historical evidence for effects that ran or may have run. Only
    // this turn's unexecuted approvals can replace its answer. A resumed
    // turn may have queued an approval before rebuilding its report.
    let ids = {
        let c = agent.db.conn();
        let mut q = c
            .prepare(
                "SELECT id FROM approvals WHERE origin = ?1 AND (
                    status IN ('executed', 'needs_review')
                    OR (turn_id = ?2 AND status IN ('pending', 'approved'))
                ) ORDER BY created_at",
            )
            .db()?;
        let rows = q
            .query_map(
                params![agent.config.origin.as_deref().unwrap_or(&owner), turn_id],
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
                // Semantic evidence is the observed state, projected
                // by the same executor that verified the action —
                // the same evidence shape the plan path grades over.
                // An executor with no observed projection falls back
                // to the approved payload, labeled as such: it shows
                // what was approved, not what the world looks like.
                let observed = agent.executors.get(&a.kind).and_then(|exec| {
                    exec.observe(
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
                    .ok()
                    .filter(|fields| !fields.is_empty())
                });
                semantic_evidence.push(match observed {
                    Some(fields) => crate::independent_grader::GraderEvidence {
                        id: id.clone(),
                        source: format!("{}:observed", a.kind),
                        value: serde_json::to_value(&fields).unwrap_or(serde_json::Value::Null),
                    },
                    None => crate::independent_grader::GraderEvidence {
                        id: id.clone(),
                        source: "approved payload (L1-verified; no observed projection)".into(),
                        value: a.payload.clone(),
                    },
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

/// Resolve this turn's NL rubric items from the frozen acceptance
/// contract — the same source the plan path grades against. Order:
/// the agent's bound contract, then the bound plan's contract, else
/// the turn's own goal text (plain chat carries no contract). A
/// bound contract with no NL items means no semantic grade at all
/// (zero model calls), never a silent fallback to the goal text.
fn rubric_items(agent: &Agent<'_>, goal: &str) -> Result<Vec<AcceptanceItem>> {
    if !agent.config.acceptance.is_empty() {
        return Ok(acc::nl_contract_items(&agent.config.acceptance));
    }
    if let Some(plan_id) = &agent.config.plan_id {
        if let Some(plan) = PlanStore::new(agent.db).get(plan_id)? {
            return Ok(acc::nl_contract_items(&plan.acceptance));
        }
    }
    Ok(vec![AcceptanceItem::Nl {
        id: "user-goal".into(),
        expected: goal.into(),
    }])
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
    use hatch_tools::verification::Layer;
    let items = rubric_items(agent, goal)?;
    if items.is_empty() {
        return Ok(None);
    }
    let rubric: Vec<RubricItem> = items
        .iter()
        .map(|i| match i {
            AcceptanceItem::Nl { id, expected } => RubricItem {
                id: id.clone(),
                expected: expected.clone(),
            },
            AcceptanceItem::Predicate { .. } => unreachable!("nl_contract_items filters"),
        })
        .collect();
    // L3 gates only when the layer is enforced (the same switch the
    // plan path uses); in Observe the verdict is recorded as a
    // shadow verdict and never blocks the turn.
    let enforced = AcceptanceEnforce::configured()?.l3;
    let evidence_digest = digest(
        serde_json::to_string(
            &evidence
                .iter()
                .map(|e| (&e.id, &e.source, &e.value))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
        .as_bytes(),
    );
    let key = format!("semantic:{}", hatch_core::ids::random_id("grade"));
    let owner = format!("chat:{session}");
    let contract = serde_json::json!({"goal":goal,"rubric":rubric,"evidence_digest":evidence_digest}).to_string();
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
    let evidence_ids: std::collections::HashSet<String> =
        evidence.iter().map(|e| e.id.clone()).collect();
    let checks = match grade(agent.router, goal, &rubric, &evidence, candidate, cancel) {
        Ok((graded, usage)) => {
            crate::report::add_usage(&mut report.usage, &usage);
            let verdicts: Vec<NlItemVerdict> = graded
                .items
                .into_iter()
                .map(|g| NlItemVerdict {
                    id: g.id,
                    verdict: g.verdict,
                    evidence: g.evidence,
                    reason: g.reason,
                })
                .collect();
            acc::nl_checks(&items, &verdicts, &evidence_ids, &evidence_digest, enforced)
        }
        Err(e) => {
            cancel.check()?;
            items
                .iter()
                .map(|item| {
                    let mut check = EvidenceCheck::unknown(
                        &format!("cross-step-v1:{}", item.id()),
                        format!("语义评分不可用：{e}"),
                    )
                    .with_layer(Layer::L3, enforced);
                    check.source = "semantic_grader".into();
                    check.observed_digest = evidence_digest.clone();
                    check
                })
                .collect()
        }
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
        verdict: gate_verdict(&checks),
        shadow_verdict: Some(aggregate_verdict(&checks)),
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
