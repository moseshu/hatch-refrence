//! Executors for approved schedule changes: the only path that
//! actually creates or deletes a job. Each verifies by reading the
//! job store back (the same independent-readback shape as the mail
//! and calendar executors).

use crate::builtin::schedule::{CreateJobPayload, DeleteJobPayload, PROMPT_KIND};
use crate::executors::{ActionExecutor, ExecContext};
use crate::verification::{EffectContract, EvidenceCheck, Layer, Verdict};
use chrono_tz::Tz;
use hatch_core::time::to_rfc3339;
use hatch_core::{Error, Result};
use hatch_scheduler::{Delivery, JobStore, NewJob, Schedule};
use serde_json::Value;

/// The schedule after the once-rewrite: a non-recurring job fires
/// exactly once, at the next firing of the approved schedule.
fn normalized_schedule(p: &CreateJobPayload, now: &hatch_core::time::Timestamp, tz: &Tz) -> Result<String> {
    let mut schedule = p.schedule.clone();
    if !p.recurring {
        // Fire once: rewrite to the next single firing. A `once`
        // schedule is already single-shot and stays as written.
        let parsed = Schedule::parse(&schedule)?;
        if !matches!(parsed, Schedule::Once { .. }) {
            let next = parsed
                .next_after(now, now, tz)
                .ok_or_else(|| Error::Invalid(format!("schedule '{schedule}' never fires")))?;
            schedule = format!("once {}", to_rfc3339(&next));
        }
    }
    Ok(schedule)
}

/// The params as stored: for `agent_prompt` jobs the prompt is merged
/// into `params` at execution.
fn stored_params(p: &CreateJobPayload) -> Value {
    let mut params = p.params.clone();
    if p.kind == PROMPT_KIND {
        if let Value::Object(map) = &mut params {
            map.insert("prompt".into(), Value::String(p.prompt.clone()));
        }
    }
    params
}

/// The delivery as stored: empty means the engine default (`digest`).
fn stored_delivery(p: &CreateJobPayload) -> Result<Delivery> {
    if p.delivery.is_empty() {
        Ok(Delivery::Digest)
    } else {
        Delivery::parse(&p.delivery)
    }
}

/// Canonical stored shape of the approved job, for read-back
/// comparison: kind, schedule, params, scope, delivery,
/// done_condition and next firing. Shared by execute and verify so
/// the check compares exactly what execution stores. The schedule
/// is canonicalised through parse + Display: the store parses the
/// schedule, and Display normalises equivalent spellings (`every
/// 60m` is stored as `every 1h`), so comparing the approved string
/// as written would fail a legal definition for spelling alone.
fn expected_stored(p: &CreateJobPayload, now: &hatch_core::time::Timestamp, tz: &Tz) -> Result<Value> {
    let schedule = normalized_schedule(p, now, tz)?;
    let parsed = Schedule::parse(&schedule)?;
    let schedule = parsed.to_string();
    let next = parsed.next_after(now, now, tz);
    Ok(serde_json::json!({
        "name": p.name,
        "kind": p.kind,
        "schedule": schedule,
        "params": stored_params(p),
        "scope": p.scope,
        "delivery": stored_delivery(p)?.as_str(),
        "done_condition": p.done_condition,
        "next_run_at": next.map(|t| to_rfc3339(&t)),
    }))
}

fn stored_shape(job: &hatch_scheduler::Job) -> Value {
    serde_json::json!({
        "name": job.name,
        "kind": job.kind,
        "schedule": job.schedule.to_string(),
        "params": job.params,
        "scope": job.scope,
        "delivery": job.delivery.as_str(),
        "done_condition": job.done_condition,
        "next_run_at": job.next_run_at.map(|t| to_rfc3339(&t)),
    })
}

fn fail_check(rule: &str, resource: &str, reason: impl Into<String>) -> EvidenceCheck {
    EvidenceCheck {
        rule: rule.into(),
        verdict: Verdict::Fail,
        effect: crate::verification::EffectState::ConfirmedNotApplied,
        source: "job_store".into(),
        resource: resource.into(),
        expected_digest: String::new(),
        observed_digest: String::new(),
        reason: reason.into(),
        layer: Layer::L1,
        enforced: true,
    }
}

/// The job id in a create receipt
/// (`定时任务已创建：<name>（<id>，<schedule>）下次运行 …`).
/// The receipt is the only channel that carries the id the executor
/// actually stored, so verification reads back through it — never
/// through the name, which a later job may reuse.
fn created_job_id(receipt: &str) -> Option<String> {
    let after = receipt.split('（').nth(1)?;
    let id = after.split('，').next()?.trim();
    (!id.is_empty() && id.starts_with("job_")).then(|| id.to_string())
}

/// Creates jobs of the kinds the app registers with the scheduler
/// engine. Kinds outside this list are rejected, so an approved
/// payload can never create a job nothing can run.
pub struct CreateJobExecutor {
    allowed_kinds: Vec<String>,
}

impl CreateJobExecutor {
    pub fn new(allowed_kinds: Vec<String>) -> Self {
        Self { allowed_kinds }
    }
}

impl ActionExecutor for CreateJobExecutor {
    fn kind(&self) -> &'static str {
        "create_job"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: CreateJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad create_job payload: {e}")))?;
        if !self.allowed_kinds.iter().any(|k| k == &p.kind) {
            return Err(Error::Invalid(format!(
                "job kind '{}' is not registered (available: {})",
                p.kind,
                self.allowed_kinds.join(", ")
            )));
        }
        let tz: Tz =
            p.tz.parse()
                .map_err(|_| Error::Invalid(format!("bad timezone '{}'", p.tz)))?;
        let schedule = normalized_schedule(&p, &ctx.now, &tz)?;
        let mut job = NewJob::new(&p.name, &p.kind, &schedule);
        let params = stored_params(&p);
        if p.kind == PROMPT_KIND && p.prompt.trim().is_empty() {
            return Err(Error::Invalid("agent_prompt job without a prompt".into()));
        }
        job.params = params;
        job.scope = p.scope.clone();
        if !p.delivery.is_empty() {
            job.delivery = Delivery::parse(&p.delivery)?;
        }
        if !p.done_condition.is_empty() {
            job.done_condition = p.done_condition.clone();
        }
        let created = JobStore::new(ctx.db).add(&job, &ctx.now, &tz)?;
        Ok(format!(
            "定时任务已创建：{}（{}，{}）下次运行 {}",
            created.name,
            created.id,
            created.schedule,
            created
                .next_run_at
                .as_ref()
                .map(to_rfc3339)
                .unwrap_or_else(|| "无".into())
        ))
    }

    fn contract(&self) -> EffectContract {
        EffectContract::LocalArtifact
    }

    fn verify(&self, payload: &Value, receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        let p: CreateJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad create_job payload: {e}")))?;
        let tz: Tz = p
            .tz
            .parse()
            .map_err(|_| Error::Invalid(format!("bad timezone '{}'", p.tz)))?;
        // Read back through the id in the receipt, never the name:
        // a later job may reuse the name with a different schedule
        // or content, and name-only read-back would bless it.
        let Some(id) = created_job_id(receipt) else {
            return Ok(EvidenceCheck::unknown(
                "create-job-v1",
                "执行回执未包含任务 ID，无法独立回读",
            ));
        };
        let job = match JobStore::new(ctx.db).get_by_id(&id) {
            Ok(j) => j,
            // Genuinely absent: the created job is gone.
            Err(Error::NotFound(_)) => {
                return Ok(fail_check(
                    "create-job-v1",
                    &id,
                    "回执中的任务 ID 在任务库中不存在，创建未生效",
                ));
            }
            // A read fault is not an absence: Ambiguous, never Fail
            // (which would claim a confirmed non-application) and
            // never Pass.
            Err(e) => {
                return Ok(EvidenceCheck::unknown(
                    "create-job-v1",
                    format!("回读任务失败：{e}"),
                ));
            }
        };
        let expected = expected_stored(&p, &ctx.now, &tz)?;
        let observed = stored_shape(&job);
        if expected == observed {
            return Ok(EvidenceCheck::compare(
                "create-job-v1",
                "job_store",
                &id,
                expected.to_string().as_bytes(),
                observed.to_string().as_bytes(),
            )
            .with_layer(Layer::L1, true));
        }
        // Name the first differing field so the gate report says what
        // changed instead of only "不符".
        let reason = ["name", "kind", "schedule", "params", "scope", "delivery", "done_condition", "next_run_at"]
            .iter()
            .find(|k| expected.get(*k) != observed.get(*k))
            .map(|k| {
                format!(
                    "独立回读的任务与批准内容不符：{k} 批准为 {}，实际为 {}",
                    expected.get(*k).unwrap_or(&Value::Null),
                    observed.get(*k).unwrap_or(&Value::Null)
                )
            })
            .unwrap_or_else(|| "独立回读的任务与批准内容不符".into());
        Ok(fail_check("create-job-v1", &id, reason))
    }
}

/// Deletes a job by id or name. Run history is kept (the store only
/// removes the definition).
pub struct DeleteJobExecutor;

impl ActionExecutor for DeleteJobExecutor {
    fn kind(&self) -> &'static str {
        "delete_job"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: DeleteJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad delete_job payload: {e}")))?;
        // Delete by the id bound at approval time, never by name: a
        // replacement reusing the name must never be deleted in the
        // approved job's place. An unbound payload (the job did not
        // exist when the approval was proposed) fails here, exactly
        // as its preview said it would.
        if p.job_id.trim().is_empty() {
            return Err(Error::NotFound(format!(
                "删除审批未绑定任务 ID（任务 {} 在审批时不存在），拒绝按名称删除",
                p.job
            )));
        }
        let store = JobStore::new(ctx.db);
        let job = store.get_by_id(&p.job_id)?;
        store.remove(&job.id)?;
        Ok(format!("定时任务已删除：{}（{}）", job.name, job.id))
    }

    fn contract(&self) -> EffectContract {
        EffectContract::LocalArtifact
    }

    fn verify(&self, payload: &Value, _receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        let p: DeleteJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad delete_job payload: {e}")))?;
        if p.job_id.trim().is_empty() {
            return Ok(fail_check(
                "delete-job-v1",
                &p.job,
                "删除审批未绑定任务 ID，无法确认删除对象",
            ));
        }
        // Deleted = the bound id no longer resolves. A same-name
        // replacement does not count as the job still being there —
        // and a read fault is Ambiguous, not a confirmed absence.
        match JobStore::new(ctx.db).find_by_id(&p.job_id) {
            Ok(None) => Ok(EvidenceCheck::compare(
                "delete-job-v1",
                "job_store",
                &p.job_id,
                b"absent",
                b"absent",
            )
            .with_layer(Layer::L1, true)),
            Ok(Some(_)) => Ok(fail_check(
                "delete-job-v1",
                &p.job_id,
                "批准删除的任务仍在任务库中",
            )),
            Err(e) => Ok(EvidenceCheck::unknown(
                "delete-job-v1",
                format!("回读任务失败：{e}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::schedule::CreateJobPayload;
    use hatch_core::time::now as now_ts;
    use hatch_store::Db;
    use serde_json::json;

    fn exec_ctx<'a>(db: &'a Db) -> ExecContext<'a> {
        ExecContext {
            db,
            now: now_ts(),
            approval_id: "a1",
            origin: "test",
            cancel: None,
        }
    }

    fn payload(recurring: bool) -> Value {
        serde_json::to_value(CreateJobPayload {
            name: "remind".into(),
            kind: PROMPT_KIND.into(),
            schedule: "daily 09:00".into(),
            prompt: "提醒我喝水".into(),
            recurring,
            params: json!({}),
            scope: vec![],
            delivery: String::new(),
            done_condition: String::new(),
            tz: "Asia/Shanghai".into(),
        })
        .unwrap()
    }

    #[test]
    fn create_executor_stores_job_and_verifies() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        let receipt = exec.execute(&payload(true), &ctx).unwrap();
        assert!(receipt.contains("remind"), "{receipt}");
        let job = JobStore::new(&db).find("remind").unwrap().unwrap();
        assert_eq!(job.params["prompt"], "提醒我喝水");
        assert!(job.next_run_at.is_some());
        let check = exec.verify(&payload(true), &receipt, &ctx).unwrap();
        assert_eq!(check.verdict, crate::verification::Verdict::Pass);
        // Duplicate name is rejected by the store.
        assert!(exec.execute(&payload(true), &ctx).is_err());
    }

    #[test]
    fn create_executor_rejects_unregistered_kind() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec!["daily_brief".into()]);
        let err = exec.execute(&payload(true), &ctx).unwrap_err();
        assert!(err.to_string().contains("not registered"), "{err}");
    }

    #[test]
    fn non_recurring_becomes_a_single_firing() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        exec.execute(&payload(false), &ctx).unwrap();
        let job = JobStore::new(&db).find("remind").unwrap().unwrap();
        assert!(
            matches!(job.schedule, Schedule::Once { .. }),
            "{}",
            job.schedule
        );
        assert_eq!(
            job.next_run_at,
            job.schedule
                .next_after(&ctx.now, &ctx.now, &"Asia/Shanghai".parse().unwrap())
        );
    }

    /// RED (review round 2, P2): `every 60m` and `every 1h` are the
    /// same schedule — the store parses it and its Display writes
    /// `every 1h`. Verification must compare the canonical form, or
    /// a legal definition fails read-back for spelling alone.
    #[test]
    fn create_verify_accepts_equivalent_interval_spelling() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        let mut p = payload(true);
        p["schedule"] = json!("every 60m");
        let receipt = exec.execute(&p, &ctx).unwrap();
        let job = JobStore::new(&db).find("remind").unwrap().unwrap();
        assert_eq!(job.schedule.to_string(), "every 1h", "stored form");
        let check = exec.verify(&p, &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Pass,
            "every 60m is the same schedule as the stored every 1h: {check:?}"
        );
        // The canonical spelling verifies against the same job too.
        let mut p2 = payload(true);
        p2["schedule"] = json!("every 1h");
        let check = exec.verify(&p2, &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Pass,
            "every 1h must verify the same job: {check:?}"
        );
        // But a genuinely different interval still fails.
        let mut p3 = payload(true);
        p3["schedule"] = json!("every 90m");
        let check = exec.verify(&p3, &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Fail,
            "every 90m is a different schedule: {check:?}"
        );
    }

    #[test]
    fn delete_executor_removes_and_verifies() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let create = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        create.execute(&payload(true), &ctx).unwrap();
        let delete = DeleteJobExecutor;
        // The approval-time payload binds the job id (see
        // delete_approval_payload); a bare name no longer executes.
        let p = delete_approval_payload(&db, "remind");
        let receipt = delete.execute(&p, &ctx).unwrap();
        assert!(receipt.contains("已删除"), "{receipt}");
        assert!(JobStore::new(&db).find("remind").unwrap().is_none());
        let check = delete.verify(&p, &receipt, &ctx).unwrap();
        assert_eq!(check.verdict, crate::verification::Verdict::Pass);
        // Deleting again errors instead of silently succeeding.
        assert!(delete.execute(&p, &ctx).is_err());
        // An unbound payload (job missing at approval time) never
        // deletes by bare name.
        let unbound = json!({"job": "remind", "job_id": ""});
        assert!(delete.execute(&unbound, &ctx).is_err());
    }

    /// RED (issue 1): verification must compare the stored job against
    /// the approved schedule/content/scope, not just the name. A job
    /// with the same name but a different schedule and prompt must
    /// fail verification.
    #[test]
    fn create_verify_fails_when_same_name_job_differs() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        let receipt = exec.execute(&payload(true), &ctx).unwrap();
        let mut other = payload(true);
        other["schedule"] = json!("daily 10:00");
        other["prompt"] = json!("提醒我运动");
        other["scope"] = json!(["send_message"]);
        let check = exec.verify(&other, &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Fail,
            "same name, different schedule/prompt/scope must not verify: {check:?}"
        );
    }

    /// RED (issue 1): a read fault is Ambiguous, while a job that is
    /// genuinely gone is Fail — a fault must never read as a pass or
    /// as a confirmed absence.
    #[test]
    fn create_verify_read_fault_is_ambiguous_not_absent() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        let receipt = exec.execute(&payload(true), &ctx).unwrap();
        // Break the read path itself: the store query errors.
        db.conn()
            .execute("DROP TABLE jobs", [])
            .unwrap();
        let check = exec.verify(&payload(true), &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Ambiguous,
            "read fault must be Ambiguous, never Fail/Pass: {check:?}"
        );
    }

    #[test]
    fn create_verify_missing_job_is_fail_not_ambiguous() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let exec = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        let receipt = exec.execute(&payload(true), &ctx).unwrap();
        let job = JobStore::new(&db).find("remind").unwrap().unwrap();
        JobStore::new(&db).remove(&job.id).unwrap();
        let check = exec.verify(&payload(true), &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Fail,
            "created job genuinely gone must be Fail: {check:?}"
        );
    }

    fn delete_approval_payload(db: &Db, job: &str) -> Value {
        use crate::builtin::schedule::ScheduleDelete;
        use crate::tool::{Tool, ToolContext};
        let ctx = ToolContext {
            turn_id: String::new(),
            db,
            now: now_ts(),
            tz: "Asia/Shanghai".parse().unwrap(),
            user_email: "me@example.com".into(),
            user_name: "Tester".into(),
            origin: "test".into(),
            tainted: false,
            call_id: "c1".into(),
            cancel: hatch_core::CancelToken::new(),
            intent_review: None,
        };
        ScheduleDelete
            .to_approval(&json!({"job": job}), &ctx)
            .unwrap()
            .unwrap()
            .payload
    }

    /// RED (issue 1): the delete approval binds the job id at approval
    /// time. If the named job is deleted and a different job reuses
    /// the name before execution, executing must fail safely — never
    /// delete the replacement.
    #[test]
    fn delete_bound_to_approval_time_id_survives_name_reuse() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let create = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        create.execute(&payload(true), &ctx).unwrap();
        let approval = delete_approval_payload(&db, "remind");
        // The approved job is deleted and a new job reuses its name.
        let old = JobStore::new(&db).find("remind").unwrap().unwrap();
        JobStore::new(&db).remove(&old.id).unwrap();
        let mut other = payload(true);
        other["prompt"] = json!("提醒我运动");
        create.execute(&other, &ctx).unwrap();
        let delete = DeleteJobExecutor;
        let err = delete
            .execute(&approval, &ctx)
            .expect_err("executing a stale delete must not delete the replacement");
        assert!(
            err.to_string().contains("remind") || err.to_string().contains("job"),
            "{err}"
        );
        let replacement = JobStore::new(&db).find("remind").unwrap().unwrap();
        assert_eq!(replacement.params["prompt"], "提醒我运动");
        assert_ne!(replacement.id, old.id);
    }

    /// RED (issue 1): delete verification is bound to the approved id,
    /// not the name — after the approved job is gone, a replacement
    /// reusing the name must not flip the verdict to Fail.
    #[test]
    fn delete_verify_is_bound_to_id_not_name() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let create = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        create.execute(&payload(true), &ctx).unwrap();
        let approval = delete_approval_payload(&db, "remind");
        let delete = DeleteJobExecutor;
        let receipt = delete.execute(&approval, &ctx).unwrap();
        let mut other = payload(true);
        other["prompt"] = json!("提醒我运动");
        create.execute(&other, &ctx).unwrap();
        let check = delete.verify(&approval, &receipt, &ctx).unwrap();
        assert_eq!(
            check.verdict,
            crate::verification::Verdict::Pass,
            "approved job is gone; a same-name replacement must not fail verification: {check:?}"
        );
    }
}
