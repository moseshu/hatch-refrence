//! Scheduled-job tools: the model-facing surface over the persistent
//! job engine (`hatch-scheduler`).
//!
//! `schedule_create` / `schedule_delete` never act directly: they
//! build an approval with an immutable payload (the
//! [`ProposeEmail`](super::actions::ProposeEmail) pattern), and the
//! dispatcher decides whether it waits for the user or (under a
//! grant) executes through the `create_job` / `delete_job`
//! executors. `schedule_list` is a plain read.
//!
//! The tools are generic: a job is a schedule plus either a prompt
//! (kind `agent_prompt`, run as an agent turn by the scheduler) or
//! one of the engine's registered worker kinds with JSON params.
//! Nothing here knows what any particular job is *for*.

use crate::tool::{
    arg_opt_str, arg_str, arg_str_list, PlannedAction, Tool, ToolContext, ToolOutput,
};
use chrono::Duration;
use hatch_core::time::to_rfc3339;
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, DataLabel, NewApproval};
use hatch_scheduler::{JobStore, Schedule};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Job kind whose handler runs the stored prompt as an agent turn.
pub const PROMPT_KIND: &str = "agent_prompt";

/// Payload of a `create_job` approval: everything the executor needs
/// to store the job exactly as approved.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CreateJobPayload {
    pub name: String,
    pub kind: String,
    /// Schedule expression in the engine's own syntax
    /// (`daily 08:30`, `weekly MON,FRI 09:00`, `every 30m`,
    /// `once <RFC3339>`, `cron <5 fields>`).
    pub schedule: String,
    /// Prompt for `agent_prompt` jobs (stored into params at execution).
    #[serde(default)]
    pub prompt: String,
    /// `false` = fire once (the executor rewrites the schedule to the
    /// next single firing).
    #[serde(default = "default_true")]
    pub recurring: bool,
    /// Extra job params (worker-kind specific); `prompt` is merged in.
    #[serde(default)]
    pub params: Value,
    /// Action classes the job may perform under normal policy.
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub delivery: String,
    #[serde(default)]
    pub done_condition: String,
    /// Timezone the schedule was written in (the executor has no
    /// other way to know it).
    pub tz: String,
}

fn default_true() -> bool {
    true
}

impl CreateJobPayload {
    pub fn preview(&self) -> String {
        let mut s = format!(
            "新建定时任务: {}\n种类: {}\n调度: {}{}",
            self.name,
            self.kind,
            self.schedule,
            if self.recurring {
                ""
            } else {
                "（只跑一次）"
            }
        );
        if !self.prompt.is_empty() {
            s.push_str(&format!("\n内容: {}", self.prompt));
        }
        if !self.scope.is_empty() {
            s.push_str(&format!("\n授权范围: {}", self.scope.join(", ")));
        } else {
            s.push_str("\n授权范围: 空（该任务提出的任何对外动作都会进入审批队列）");
        }
        s
    }
}

fn create_payload(args: &Value, ctx: &ToolContext) -> Result<CreateJobPayload> {
    let schedule = arg_str(args, "schedule")?.to_string();
    // Validate now, at proposal time: an unparseable schedule must
    // never reach the approval queue.
    Schedule::parse(&schedule)?;
    let kind = arg_opt_str(args, "kind").unwrap_or(PROMPT_KIND).to_string();
    let prompt = arg_opt_str(args, "prompt").unwrap_or("").to_string();
    if kind == PROMPT_KIND && prompt.trim().is_empty() {
        return Err(Error::Invalid(
            "`prompt` is required for agent_prompt jobs".into(),
        ));
    }
    let name = match arg_opt_str(args, "name") {
        Some(n) => n.to_string(),
        None => ids::stable_id("sched", &[&kind, &schedule, &prompt]),
    };
    let params = match args.get("params") {
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(Error::Invalid("`params` must be a JSON object".into())),
        None => Value::Object(Default::default()),
    };
    let recurring = args
        .get("recurring")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(CreateJobPayload {
        name,
        kind,
        schedule,
        prompt,
        recurring,
        params,
        scope: arg_str_list(args, "scope"),
        delivery: arg_opt_str(args, "delivery").unwrap_or("").to_string(),
        done_condition: arg_opt_str(args, "done_condition")
            .unwrap_or("")
            .to_string(),
        tz: ctx.tz.name().to_string(),
    })
}

pub struct ScheduleCreate;

impl Tool for ScheduleCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_create".into(),
            description: "Propose creating a persistent scheduled job (needs the user's approval; never created directly). The job runs unattended on the schedule: with the default kind agent_prompt the prompt is run as an agent turn each firing; other kinds are the engine's registered workers. Schedule syntax: `daily 08:30`, `weekly MON,FRI 09:00`, `every 30m`, `once <RFC3339>`, `cron <5 fields>`.".into(),
            parameters: json!({"type": "object", "properties": {
                "schedule": {"type": "string", "description": "daily HH:MM | weekly MON,FRI HH:MM | every 30m/2h | once <RFC3339> | cron <5 fields>"},
                "prompt": {"type": "string", "description": "what to do at each firing (required for agent_prompt)"},
                "name": {"type": "string", "description": "unique job name (optional; derived when omitted)"},
                "kind": {"type": "string", "description": "job kind (default agent_prompt)"},
                "recurring": {"type": "boolean", "description": "false = fire once at the next scheduled time (default true)"},
                "params": {"type": "object", "description": "extra job params for worker kinds (optional)"},
                "scope": {"type": "array", "items": {"type": "string"}, "description": "action classes the job may perform without per-action approval (default empty)"},
                "delivery": {"type": "string", "description": "notify | digest | silent (default digest)"},
                "done_condition": {"type": "string", "description": "what counts as done, recorded on the job (optional)"}
            }, "required": ["schedule"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteSchedule
    }

    fn plan(&self, args: &Value, ctx: &ToolContext) -> Result<PlannedAction> {
        let p = create_payload(args, ctx)?;
        let mut labels = Vec::new();
        if hatch_core::secrets::contains_secret(&p.prompt) {
            labels.push(DataLabel::Secret);
        }
        Ok(PlannedAction {
            summary: format!("新建定时任务 {}（{}）", p.name, p.schedule),
            recipients: vec![p.name],
            labels,
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid(
            "schedule_create only creates approvals".into(),
        ))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let p = create_payload(args, ctx)?;
        let payload = serde_json::to_value(&p)?;
        Ok(Some(NewApproval {
            turn_id: String::new(),
            idempotency_key: ids::stable_id("job", &[&p.name, &payload.to_string()]),
            kind: "create_job".into(),
            class: ActionClass::WriteSchedule,
            title: format!("新建定时任务：{}", p.name),
            preview: p.preview(),
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

pub struct ScheduleList;

impl Tool for ScheduleList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_list".into(),
            description: "List the persistent scheduled jobs: id, name, kind, schedule, enabled, next run time and a short summary of what each job does.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, _args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let jobs = JobStore::new(ctx.db).list()?;
        let rows: Vec<Value> = jobs
            .iter()
            .map(|j| {
                let summary = j
                    .params
                    .get("prompt")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.chars().take(80).collect::<String>())
                    .unwrap_or_else(|| j.done_condition.chars().take(80).collect());
                json!({
                    "id": j.id,
                    "name": j.name,
                    "kind": j.kind,
                    "schedule": j.schedule.to_string(),
                    "enabled": j.enabled,
                    "next_run_at": j.next_run_at.as_ref().map(to_rfc3339),
                    "summary": summary,
                })
            })
            .collect();
        Ok(ToolOutput::system(serde_json::to_string(
            &json!({ "jobs": rows }),
        )?))
    }
}

/// Payload of a `delete_job` approval.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeleteJobPayload {
    /// Display name of the job (as listed by `schedule_list`).
    pub job: String,
    /// Job id bound at approval time (`to_approval` resolves the
    /// name/id once and stores it here). The executor deletes by
    /// this id only — never by name — so a replacement reusing the
    /// name after approval cannot be deleted in its place. Empty
    /// when the job did not exist at approval time; executing such
    /// a payload fails, as its preview says.
    #[serde(default)]
    pub job_id: String,
}

pub struct ScheduleDelete;

impl Tool for ScheduleDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_delete".into(),
            description: "Propose deleting a scheduled job by id or name (needs the user's approval; run history is kept).".into(),
            parameters: json!({"type": "object", "properties": {
                "job": {"type": "string", "description": "job id or name, as listed by schedule_list"}
            }, "required": ["job"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::DeleteData
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let job = arg_str(args, "job")?.to_string();
        Ok(PlannedAction {
            summary: format!("删除定时任务 {job}"),
            recipients: vec![job],
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid(
            "schedule_delete only creates approvals".into(),
        ))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let job = arg_str(args, "job")?.to_string();
        // Resolve once, now: the approval binds the job id, so the
        // user approves deleting *that* job and execution deletes by
        // id even if the name is reused later. Name the job in the
        // preview when it exists; when it does not, the preview says
        // approval will error — and the payload carries no id, so
        // execution cannot fall back to deleting by bare name.
        let (job_id, display, preview) = match JobStore::new(ctx.db).get(&job) {
            Ok(j) => (
                j.id.clone(),
                j.name.clone(),
                format!(
                    "删除定时任务: {}（{}，{}）\n调度: {}\n运行记录保留。",
                    j.name, j.id, j.kind, j.schedule
                ),
            ),
            Err(_) => (
                String::new(),
                job.clone(),
                format!("删除定时任务: {job}（未找到该任务，批准后将报错）"),
            ),
        };
        let payload = serde_json::to_value(DeleteJobPayload {
            job: display.clone(),
            job_id: job_id.clone(),
        })?;
        Ok(Some(NewApproval {
            turn_id: String::new(),
            idempotency_key: ids::stable_id("jobdel", &[&ctx.origin, &job_id, &job]),
            kind: "delete_job".into(),
            class: ActionClass::DeleteData,
            title: format!("删除定时任务：{display}"),
            preview,
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now as now_ts;
    use hatch_store::Db;

    fn ctx<'a>(db: &'a Db) -> ToolContext<'a> {
        ToolContext {
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
        }
    }

    #[test]
    fn create_validates_schedule_and_prompt() {
        let db = Db::open_in_memory().unwrap();
        let ctx = ctx(&db);
        let tool = ScheduleCreate;
        // Bad schedule never reaches the queue.
        assert!(tool
            .to_approval(&json!({"schedule": "hourly", "prompt": "x"}), &ctx)
            .is_err());
        // agent_prompt needs a prompt.
        assert!(tool
            .to_approval(&json!({"schedule": "daily 08:30"}), &ctx)
            .is_err());
        // Worker kinds may omit the prompt.
        let a = tool
            .to_approval(
                &json!({"schedule": "daily 08:30", "kind": "daily_brief", "name": "brief-1"}),
                &ctx,
            )
            .unwrap()
            .unwrap();
        assert_eq!(a.kind, "create_job");
        assert_eq!(a.class, ActionClass::WriteSchedule);
        let p: CreateJobPayload = serde_json::from_value(a.payload.clone()).unwrap();
        assert_eq!(p.name, "brief-1");
        assert_eq!(p.tz, "Asia/Shanghai");
        assert!(a.preview.contains("授权范围: 空"));
        // Default kind is agent_prompt; name is derived when omitted.
        let a = tool
            .to_approval(
                &json!({"schedule": "every 30m", "prompt": "检查一下"}),
                &ctx,
            )
            .unwrap()
            .unwrap();
        let p: CreateJobPayload = serde_json::from_value(a.payload).unwrap();
        assert_eq!(p.kind, PROMPT_KIND);
        assert!(p.name.starts_with("sched_"), "{}", p.name);
        assert!(p.recurring);
    }

    #[test]
    fn delete_approval_binds_job_id_at_approval_time() {
        let db = Db::open_in_memory().unwrap();
        let ctx = ctx(&db);
        let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
        let mut job = hatch_scheduler::NewJob::new("remind", PROMPT_KIND, "daily 09:00");
        job.params = json!({"prompt": "提醒我喝水"});
        let created = JobStore::new(&db).add(&job, &ctx.now, &tz).unwrap();

        // By name and by id, the payload binds the same job id.
        for given in [created.name.as_str(), created.id.as_str()] {
            let a = ScheduleDelete
                .to_approval(&json!({"job": given}), &ctx)
                .unwrap()
                .unwrap();
            let p: DeleteJobPayload = serde_json::from_value(a.payload).unwrap();
            assert_eq!(p.job_id, created.id, "given {given}");
            assert_eq!(p.job, "remind");
            assert!(a.preview.contains(&created.id), "{}", a.preview);
        }
        // A missing job keeps the "批准后将报错" preview but binds
        // no id, so execution cannot delete by bare name.
        let a = ScheduleDelete
            .to_approval(&json!({"job": "ghost"}), &ctx)
            .unwrap()
            .unwrap();
        let p: DeleteJobPayload = serde_json::from_value(a.payload).unwrap();
        assert_eq!(p.job_id, "");
        assert_eq!(p.job, "ghost");
        assert!(a.preview.contains("批准后将报错"), "{}", a.preview);
    }

    #[test]
    fn list_renders_jobs_and_delete_preview_names_the_job() {
        let db = Db::open_in_memory().unwrap();
        let ctx = ctx(&db);
        let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
        let mut job = hatch_scheduler::NewJob::new("remind", PROMPT_KIND, "daily 09:00");
        job.params = json!({"prompt": "提醒我喝水"});
        let created = JobStore::new(&db).add(&job, &ctx.now, &tz).unwrap();

        let out = ScheduleList.call(&json!({}), &ctx).unwrap();
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["jobs"][0]["id"], created.id);
        assert_eq!(v["jobs"][0]["summary"], "提醒我喝水");
        assert!(v["jobs"][0]["next_run_at"].is_string());

        let a = ScheduleDelete
            .to_approval(&json!({"job": created.id}), &ctx)
            .unwrap()
            .unwrap();
        assert_eq!(a.kind, "delete_job");
        assert_eq!(a.class, ActionClass::DeleteData);
        assert!(a.preview.contains("remind"), "{}", a.preview);
    }
}
