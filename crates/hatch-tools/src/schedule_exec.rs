//! Executors for approved schedule changes: the only path that
//! actually creates or deletes a job. Each verifies by reading the
//! job store back (the same independent-readback shape as the mail
//! and calendar executors).

use crate::builtin::schedule::{CreateJobPayload, DeleteJobPayload, PROMPT_KIND};
use crate::executors::{ActionExecutor, ExecContext};
use crate::verification::{EffectContract, EvidenceCheck, Layer};
use chrono_tz::Tz;
use hatch_core::time::to_rfc3339;
use hatch_core::{Error, Result};
use hatch_scheduler::{Delivery, JobStore, NewJob, Schedule};
use serde_json::Value;

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
        let mut schedule = p.schedule.clone();
        if !p.recurring {
            // Fire once: rewrite to the next single firing. A `once`
            // schedule is already single-shot and stays as written.
            let parsed = Schedule::parse(&schedule)?;
            if !matches!(parsed, Schedule::Once { .. }) {
                let next = parsed
                    .next_after(&ctx.now, &ctx.now, &tz)
                    .ok_or_else(|| Error::Invalid(format!("schedule '{schedule}' never fires")))?;
                schedule = format!("once {}", to_rfc3339(&next));
            }
        }
        let mut job = NewJob::new(&p.name, &p.kind, &schedule);
        let mut params = p.params.clone();
        if p.kind == PROMPT_KIND {
            if p.prompt.trim().is_empty() {
                return Err(Error::Invalid("agent_prompt job without a prompt".into()));
            }
            if let Value::Object(map) = &mut params {
                map.insert("prompt".into(), Value::String(p.prompt.clone()));
            }
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

    fn verify(&self, payload: &Value, _receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        let p: CreateJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad create_job payload: {e}")))?;
        let observed = JobStore::new(ctx.db)
            .find(&p.name)
            .ok()
            .flatten()
            .map(|j| j.name)
            .unwrap_or_default();
        Ok(EvidenceCheck::compare(
            "create-job-v1",
            "job_store",
            &p.name,
            p.name.as_bytes(),
            observed.as_bytes(),
        )
        .with_layer(Layer::L1, true))
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
        let store = JobStore::new(ctx.db);
        let job = store.get(&p.job)?;
        store.remove(&job.id)?;
        Ok(format!("定时任务已删除：{}（{}）", job.name, job.id))
    }

    fn contract(&self) -> EffectContract {
        EffectContract::LocalArtifact
    }

    fn verify(&self, payload: &Value, _receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        let p: DeleteJobPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("bad delete_job payload: {e}")))?;
        // Deleted = neither id nor name resolves any more.
        let gone = JobStore::new(ctx.db).get(&p.job).is_err();
        let observed: &[u8] = if gone { b"absent" } else { b"present" };
        Ok(
            EvidenceCheck::compare("delete-job-v1", "job_store", &p.job, b"absent", observed)
                .with_layer(Layer::L1, true),
        )
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

    #[test]
    fn delete_executor_removes_and_verifies() {
        let db = Db::open_in_memory().unwrap();
        let ctx = exec_ctx(&db);
        let create = CreateJobExecutor::new(vec![PROMPT_KIND.into()]);
        create.execute(&payload(true), &ctx).unwrap();
        let delete = DeleteJobExecutor;
        let p = json!({"job": "remind"});
        let receipt = delete.execute(&p, &ctx).unwrap();
        assert!(receipt.contains("已删除"), "{receipt}");
        assert!(JobStore::new(&db).find("remind").unwrap().is_none());
        let check = delete.verify(&p, &receipt, &ctx).unwrap();
        assert_eq!(check.verdict, crate::verification::Verdict::Pass);
        // Deleting again errors instead of silently succeeding.
        assert!(delete.execute(&p, &ctx).is_err());
    }
}
