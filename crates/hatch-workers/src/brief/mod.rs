//! `daily_brief`: today's and tomorrow's essentials in one place.
//!
//! Facts are gathered by code ([`facts`]); the Brief-workload model only
//! orders and phrases them and must cite every item ([`compose`]); coverage
//! gaps and anything the model left out are written by code. One brief per
//! local day (dedup key `brief:<date>`); no facts means "无新事项" and no
//! model call.

pub mod compose;
pub mod facts;
pub mod store;

use crate::outputs::{deliver, WorkerOutputs};
use hatch_core::Result;
use crate::{make_router, record_usage, RouterFactory};
use hatch_policy::AttentionPolicy;
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
pub use store::{BriefStore, StoredBrief};

pub const KIND: &str = "daily_brief";

pub struct BriefWorker {
    pub router: Option<RouterFactory>,
    pub attention: AttentionPolicy,
    pub user_name: String,
    /// Whether a mail source is configured (otherwise that is a stated gap).
    pub mail_configured: bool,
}

/// Result of one generation (also used by the evaluation).
#[derive(Clone, Debug)]
pub struct Generated {
    pub date: String,
    pub content: String,
    pub gathered: facts::Gathered,
    pub composed: compose::Composed,
}

impl BriefWorker {
    /// Gather, compose and render — without storing anything.
    pub fn generate(&self, ctx: &JobContext) -> Result<Generated> {
        let date = ctx.now.with_timezone(&ctx.tz).date_naive().to_string();
        let gathered = facts::gather(ctx.db, &ctx.now, &ctx.tz, self.mail_configured)?;
        ctx.cancel.check()?;
        let composed = if gathered.facts.is_empty() {
            compose::Composed::default()
        } else {
            match make_router(&self.router) {
                Err(why) => compose::Composed { fallback: Some(why), ..Default::default() },
                Ok(router) => {
                    let c = compose::compose(Some(&router), &self.user_name, &date, &gathered, &ctx.cancel);
                    record_usage(ctx.db, &router, &ctx.now)?;
                    c?
                }
            }
        };
        let content = compose::render(&date, &gathered, &composed);
        Ok(Generated { date, content, gathered, composed })
    }
}

impl JobHandler for BriefWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let date = ctx.now.with_timezone(&ctx.tz).date_naive().to_string();
        let key = format!("brief:{date}");
        if ctx.dedup_seen(&key)? {
            return Ok(JobOutcome::Skipped { reason: format!("{date} 的简报已经生成过（同一天只生成一次）") });
        }
        let g = self.generate(ctx)?;
        ctx.cancel.check()?;
        let facts_json = serde_json::to_value(&g.gathered.facts)?;
        BriefStore::new(ctx.db).save(&date, &g.content, &facts_json, &g.composed.model, &ctx.now)?;
        WorkerOutputs::new(ctx.db).add("brief", &format!("每日简报 {date}"), &g.content, &ctx.now)?;
        let n = g.gathered.facts.len();
        let note = if n == 0 { "无新事项".to_string() } else { format!("{n} 条事项") };
        deliver(ctx.db, &self.attention, ctx.job, &format!("{date} 每日简报已生成（{note}），`hatch brief show` 查看"), &key, &ctx.now)?;
        ctx.dedup_mark(&key)?;
        let sections: usize = g.composed.sections.iter().map(|s| s.items.len()).sum();
        Ok(JobOutcome::Done {
            summary: format!(
                "{date} 简报：事实 {n} 条，条目 {sections} 条，丢弃无依据条目 {}，覆盖缺口 {}{}",
                g.composed.dropped,
                g.gathered.gaps.len(),
                g.composed.fallback.as_ref().map(|f| format!("；{f}")).unwrap_or_default()
            ),
        })
    }
}

#[cfg(test)]
mod tests;
