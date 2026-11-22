//! One settlement path for immediate execution, approvals and crash recovery.
use crate::verification::{
    self, VerificationMode, VerificationReport, VerificationStore, MAX_OBSERVATIONS, RULESET,
};
use crate::{FailureKind, Plan, PlanStep, PlanStore, RunDeps, StepOutcome, StepStatus, StepVerify};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tools::verification::{requires_evidence, EffectState, EvidenceCheck, Verdict};
use hatch_tools::ExecContext;

pub(crate) fn finish(
    deps: &RunDeps,
    plan: &Plan,
    step: &PlanStep,
    idx: usize,
    now: &Timestamp,
) -> Result<StepOutcome> {
    let store = VerificationStore::with_fence(deps.db, deps.lease_fence.clone());
    let record = store
        .get(&plan.id, plan.version as i64, &step.id)?
        .ok_or_else(|| hatch_core::Error::Storage("执行缺少验证检查点".into()))?;
    if record.contract != verification::contract(plan, step)? {
        return Err(hatch_core::Error::Invalid(
            "验证契约或规则版本已变化；必须生成新计划版本".into(),
        ));
    }
    let output = match deps.output_map {
        Some(map) => map(&record.output),
        None => record.output.clone(),
    };
    let mut latest = store.latest(&plan.id, plan.version as i64, &step.id)?;
    // A saved Pass can finish a transaction interrupted before Done. A failed
    // check only spends its remaining observation budget, never repeats a write.
    if !latest.as_ref().is_some_and(|r| r.accepted()) {
        for attempt in record.attempts + 1..=MAX_OBSERVATIONS {
            let mut checks = if record.mode == VerificationMode::Off {
                vec![EvidenceCheck {
                    verdict: Verdict::NotApplicable,
                    ..EvidenceCheck::unknown("verification-off", "验证已显式关闭；不表示已验证")
                }]
            } else {
                vec![read_evidence(
                    deps,
                    step,
                    &record.output,
                    &record.approval_id,
                    now,
                )]
            };
            if record.mode != VerificationMode::Off {
                if let Some(v) = deps.verify {
                    let (verdict, reason) = match v(step, &output) {
                        StepVerify::Pass(receipt) => (Verdict::Pass, receipt.unwrap_or_default()),
                        StepVerify::Fail(s) => (Verdict::Fail, s),
                        StepVerify::Ambiguous(s) => (Verdict::Ambiguous, s),
                    };
                    checks.push(EvidenceCheck {
                        rule: "workflow-rubric-v1".into(),
                        verdict,
                        effect: EffectState::Unknown,
                        source: "workflow_rubric".into(),
                        resource: reason.clone(),
                        expected_digest: verification::digest(step.expected.as_bytes()),
                        observed_digest: String::new(),
                        reason,
                        layer: hatch_tools::verification::Layer::L1,
                        enforced: true,
                    });
                }
            }
            let verdict = verification::gate_verdict(&checks);
            let shadow = verification::verdict(&checks);
            let report = VerificationReport {
                owner: plan.id.clone(),
                version: plan.version as i64,
                step_id: step.id.clone(),
                contract_digest: verification::digest(record.contract.as_bytes()),
                rules_digest: verification::digest(RULESET.as_bytes()),
                mode: record.mode,
                attempt,
                observed_at: to_rfc3339(&hatch_core::time::now()),
                verdict,
                shadow_verdict: Some(shadow),
                checks,
            };
            let retry_read =
                report.verdict == Verdict::Ambiguous && record.mode == VerificationMode::Enforce;
            store.save(&report)?;
            latest = Some(report);
            if !retry_read {
                break;
            }
        }
    }
    let report =
        latest.ok_or_else(|| hatch_core::Error::Storage("验证预算耗尽但缺少报告".into()))?;
    let receipt = report
        .checks
        .iter()
        .find(|c| {
            c.rule == "workflow-rubric-v1" && c.verdict == Verdict::Pass && !c.resource.is_empty()
        })
        .map(|c| c.resource.clone())
        .or_else(|| {
            if record.receipt.is_empty() {
                None
            } else {
                Some(record.receipt.clone())
            }
        });
    let outcome = if report.accepted() {
        StepOutcome {
            receipt,
            ..StepOutcome::plain(step.id.clone(), StepStatus::Done, output, None)
        }
    } else {
        let ambiguous = report.verdict == Verdict::Ambiguous || !report.is_fresh();
        let kind = if ambiguous {
            FailureKind::NeedsHuman
        } else {
            FailureKind::Permanent
        };
        StepOutcome {
            receipt,
            ambiguous,
            ..StepOutcome::failed(step.id.clone(), report.reason(), kind)
        }
    };
    deps.db.transaction(|c| {
        crate::verification_store::guard(c, &plan.id, plan.version as i64, deps.lease_fence.as_ref())?;
        // Re-check the contract inside the commit boundary (set_goal historically
        // did not advance the plan version).
        let current = PlanStore::new(deps.db).get(&plan.id)?.unwrap();
        if verification::contract(&current, &current.steps[idx])? != record.contract {
            return Err(hatch_core::Error::Invalid("验证期间验收契约已改变".into()));
        }
        PlanStore::new(deps.db).record_step_classified(
            &plan.id,
            idx,
            outcome.status,
            &outcome.output,
            None,
            outcome.failure_kind,
            now,
        )
    })?;
    Ok(outcome)
}

/// Read through the executor selected by the immutable, executed approval.
/// Unknown direct mutations cannot be passed by a workflow's textual rubric.
pub(crate) fn read_evidence(
    deps: &RunDeps,
    step: &PlanStep,
    output: &str,
    approval_id: &str,
    now: &Timestamp,
) -> EvidenceCheck {
    let read = || -> Result<EvidenceCheck> {
        if !approval_id.is_empty() {
            let a = ApprovalQueue::new(deps.db).get(approval_id)?;
            if a.status != ApprovalStatus::Executed || !a.payload_intact() {
                return Ok(EvidenceCheck::unknown(
                    "approval-integrity",
                    "审批没有可靠执行记录或内容被修改",
                ));
            }
            let Some(exec) = deps.executors.get(&a.kind) else {
                return Ok(EvidenceCheck::unknown(
                    "executor-missing",
                    "无法构造原账户对应的回读执行器",
                ));
            };
            return exec.verify(
                &a.payload,
                output,
                &ExecContext {
                    db: deps.db,
                    now: *now,
                    approval_id,
                    origin: &a.origin,
                    cancel: None,
                },
            );
        }
        let Some(tool) = deps.registry.get(&step.tool) else {
            return Ok(EvidenceCheck::unknown("tool-missing", "找不到原工具"));
        };
        if requires_evidence(tool.class()) {
            Ok(EvidenceCheck::unknown(
                "checker-missing",
                "有副作用的直接调用缺少独立检查器",
            ))
        } else {
            Ok(EvidenceCheck {
                verdict: Verdict::NotApplicable,
                source: "read_only_tool".into(),
                ..EvidenceCheck::unknown(
                    "read-only",
                    "读取步骤无外部写入；业务条件由工作流或任务验收检查",
                )
            })
        }
    };
    read().unwrap_or_else(|e| {
        EvidenceCheck::unknown(
            "readback-error",
            format!("独立读取失败：{e}；不会重复原动作"),
        )
    })
}

/// Re-read effects at the task boundary. This catches a later step overwriting
/// an earlier deliverable; individual historical Pass reports are insufficient.
pub(crate) fn finish_task(
    deps: &RunDeps,
    plan: &Plan,
    now: &Timestamp,
) -> Result<Option<StepOutcome>> {
    let store = VerificationStore::with_fence(deps.db, deps.lease_fence.clone());
    let key = "@task-completion";
    // The acceptance contract joins the frozen task contract only
    // when present: plans without task-level conditions keep the
    // exact pre-acceptance contract bytes (and digest).
    let mut contract_obj = serde_json::json!({"goal":plan.goal,"steps":plan.steps.iter()
        .map(|s| verification::contract(plan,s)).collect::<Result<Vec<_>>>()?});
    if !plan.acceptance.is_empty() {
        contract_obj["acceptance"] = serde_json::to_value(&plan.acceptance)?;
    }
    let contract = contract_obj.to_string();
    let mode = store
        .records(&plan.id)?
        .into_iter()
        .find(|(_, v, _)| *v == plan.version as i64)
        .map(|(_, _, r)| r.mode)
        .map(Ok)
        .unwrap_or_else(VerificationMode::configured)?;
    if store.begin(&plan.id, plan.version as i64, key, &contract, mode, now)? {
        store.executed(&verification::Executed {
            owner: &plan.id,
            version: plan.version as i64,
            step: key,
            output: "",
            receipt: "",
            approval: "",
            now,
        })?;
    }
    let record = store.get(&plan.id, plan.version as i64, key)?.unwrap();
    if record.contract != contract {
        return Err(hatch_core::Error::Invalid(
            "任务验收契约变化，需新计划版本".into(),
        ));
    }
    let mut latest = store.latest(&plan.id, plan.version as i64, key)?;
    if !latest.as_ref().is_some_and(|r| r.accepted()) {
        for attempt in record.attempts + 1..=MAX_OBSERVATIONS {
            let mut checks = Vec::new();
            if record.mode != VerificationMode::Off {
                let mut resources = std::collections::HashSet::new();
                for step in plan.steps.iter().rev() {
                    let executed = store.get(&plan.id, plan.version as i64, &step.id)?.unwrap();
                    let mut check =
                        read_evidence(deps, step, &executed.output, &executed.approval_id, now);
                    // A planned sequence may edit the same file twice: the last
                    // writer defines its final state, not every intermediate state.
                    if !check.resource.is_empty()
                        && !resources.insert((check.source.clone(), check.resource.clone()))
                    {
                        continue;
                    }
                    check.resource = format!("{}:{}", step.id, check.resource);
                    checks.push(check);
                }
                // L2: deterministic cross-step predicates over the
                // shared evidence bundle, collected from the same
                // readback. Batch 1 runs Observe (`enforced=false`):
                // verdicts land as shadow verdicts and never move the
                // gate. NL items are carried in the contract for L3
                // and produce no check yet.
                let predicates: Vec<&crate::acceptance::AcceptanceItem> = plan
                    .acceptance
                    .iter()
                    .filter(|i| i.is_predicate())
                    .collect();
                if !predicates.is_empty() {
                    let bundle = collect_bundle(deps, plan, &store, now)?;
                    for item in predicates {
                        checks.push(crate::acceptance::evaluate(item, &bundle, false));
                    }
                }
            } else {
                checks.push(EvidenceCheck {
                    verdict: Verdict::NotApplicable,
                    ..EvidenceCheck::unknown("verification-off", "验证已关闭")
                });
            }
            let r = VerificationReport {
                owner: plan.id.clone(),
                version: plan.version as i64,
                step_id: key.into(),
                contract_digest: verification::digest(contract.as_bytes()),
                rules_digest: verification::digest(RULESET.as_bytes()),
                mode: record.mode,
                attempt,
                observed_at: to_rfc3339(&hatch_core::time::now()),
                verdict: verification::gate_verdict(&checks),
                shadow_verdict: Some(verification::verdict(&checks)),
                checks,
            };
            let retry = r.verdict == Verdict::Ambiguous && r.mode == VerificationMode::Enforce;
            store.save(&r)?;
            latest = Some(r);
            if !retry {
                break;
            }
        }
    }
    let r = latest.ok_or_else(|| hatch_core::Error::Storage("任务缺少验收报告".into()))?;
    if r.accepted() {
        return Ok(None);
    }
    let kind = if r.verdict == Verdict::Ambiguous || !r.is_fresh() {
        FailureKind::NeedsHuman
    } else {
        FailureKind::Permanent
    };
    Ok(Some(StepOutcome {
        ambiguous: r.verdict == Verdict::Ambiguous || !r.is_fresh(),
        ..StepOutcome::failed(key.into(), r.reason(), kind)
    }))
}

/// Collect the shared evidence bundle for L2: typed observed fields
/// for exactly the steps the frozen predicates reference, keyed by
/// step id (named-step evidence is preserved per step; final-state
/// predicates reference the last writer explicitly). A step that
/// cannot be observed contributes no entry — L2 reads that as
/// Ambiguous, never a guess. Observation errors never fail the task
/// boundary; they only withhold evidence.
fn collect_bundle(
    deps: &RunDeps,
    plan: &Plan,
    store: &VerificationStore,
    now: &Timestamp,
) -> Result<crate::acceptance::EvidenceBundle> {
    let mut bundle = crate::acceptance::EvidenceBundle::new();
    for step_id in crate::acceptance::referenced_steps(&plan.acceptance) {
        let Some(step) = plan.steps.iter().find(|s| s.id == step_id) else {
            continue;
        };
        let Some(executed) = store.get(&plan.id, plan.version as i64, &step.id)? else {
            continue;
        };
        if let Some(ev) = observe_step(deps, step, &executed.output, &executed.approval_id, now) {
            bundle.insert(step.id.clone(), ev);
        }
    }
    Ok(bundle)
}

/// Observe one step's resource through the executor selected by its
/// immutable, executed approval — the same resolution as
/// [`read_evidence`]. Direct (unapproved) steps have no immutable
/// payload to observe against in v1 and yield no evidence.
fn observe_step(
    deps: &RunDeps,
    step: &PlanStep,
    output: &str,
    approval_id: &str,
    now: &Timestamp,
) -> Option<crate::acceptance::StepEvidence> {
    if approval_id.is_empty() {
        return None;
    }
    let a = ApprovalQueue::new(deps.db).get(approval_id).ok()?;
    if a.status != ApprovalStatus::Executed || !a.payload_intact() {
        return None;
    }
    let exec = deps.executors.get(&a.kind)?;
    let fields = exec
        .observe(
            &a.payload,
            output,
            &ExecContext {
                db: deps.db,
                now: *now,
                approval_id,
                origin: &a.origin,
                cancel: None,
            },
        )
        .ok()?;
    if fields.is_empty() {
        return None;
    }
    Some(crate::acceptance::StepEvidence {
        resource: format!("{}:{}", step.id, a.kind),
        source: "executor_observe".into(),
        observed_at: to_rfc3339(&hatch_core::time::now()),
        fields,
    })
}
