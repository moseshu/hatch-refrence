//! `triage_sync`: run mail triage on a schedule.
//!
//! Same workflow as `hatch triage run` (durable steps, idempotent approvals),
//! with one difference: the gatekeeper is **scoped** to the job. Action
//! classes outside the job's scope always go to the approval queue, even when
//! a standing grant would have let an interactive run proceed.

use crate::outputs::{deliver, WorkerOutputs};
use crate::{make_router, record_usage, RouterFactory};
use hatch_connectors::MailSource;
use hatch_core::{Error, Result};
use hatch_policy::{ActionClass, AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_tools::ExecutorRegistry;
use hatch_triage::{TriageConfig, TriageWorkflow};
use std::sync::Arc;

pub const KIND: &str = "triage_sync";

/// Opens the mail source for one run (IMAP connection, fixture file…).
pub type SourceFactory = Arc<dyn Fn() -> Result<Box<dyn MailSource>> + Send + Sync>;

pub struct TriageSyncWorker {
    pub router: Option<RouterFactory>,
    pub policy: PolicyConfig,
    pub executors: Arc<ExecutorRegistry>,
    pub source: SourceFactory,
    pub config: TriageConfig,
    pub attention: AttentionPolicy,
}

/// Parse a job's scope into action classes (unknown names are an error).
pub fn scope_classes(scope: &[String]) -> Result<Vec<ActionClass>> {
    scope.iter().map(|s| ActionClass::parse(s)).collect()
}

impl JobHandler for TriageSyncWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let router = make_router(&self.router).map_err(|why| Error::Config(format!("{why}，无法分诊")))?;
        let gate = Gatekeeper::scoped(self.policy.clone(), scope_classes(&ctx.job.scope)?);
        let mut source = (self.source)()?;
        ctx.cancel.check()?;
        let wf = TriageWorkflow {
            db: ctx.db,
            router: &router,
            gate: &gate,
            executors: &self.executors,
            attention: self.attention.clone(),
            config: self.config.clone(),
        };
        let receipt = wf.run(source.as_mut(), &ctx.now);
        record_usage(ctx.db, &router, &ctx.now)?;
        let receipt = receipt?;
        let queued = receipt.approvals_queued.len();
        let summary = format!(
            "分诊：拉取 {} 封，新 {} 封，处理 {} 封，排入审批 {queued} 个，自动执行 {} 个{}{}",
            receipt.fetched,
            receipt.new_mails,
            receipt.processed,
            receipt.auto_executed.len(),
            if receipt.errors.is_empty() { String::new() } else { format!("，错误 {}", receipt.errors.len()) },
            if receipt.budget_stopped { "，因调用上限中止" } else { "" }
        );
        if receipt.processed > 0 || queued > 0 {
            WorkerOutputs::new(ctx.db).add("triage_sync", "定时邮件分诊", &receipt.render_markdown(), &ctx.now)?;
            deliver(ctx.db, &self.attention, ctx.job, &summary, &format!("worker:{}", ctx.run_id), &ctx.now)?;
        }
        if !receipt.errors.is_empty() && receipt.processed == 0 {
            return Err(Error::Connector(format!("{summary}：{}", receipt.errors.join("; "))));
        }
        Ok(JobOutcome::Done { summary })
    }
}
