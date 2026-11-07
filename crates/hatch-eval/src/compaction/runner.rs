//! Run the compaction evaluation: the world's transcript through both
//! real compression paths, then deterministic scoring.

use super::oracle::compaction_oracle;
use super::report::{score, CompactionReport};
use super::CompactionWorld;
use crate::report::Usage;
use hatch_agent::compaction;
use hatch_core::Result;
use hatch_llm::{Role, Router, Workload};

pub enum CpMode<'a> {
    /// Fixed oracle summary — tests the compression paths, not a model.
    Pipeline,
    /// A real model through the given router.
    Live { router: &'a Router },
}

/// Run both compression paths on the world's transcript and score them.
///
/// The model path is the production `compaction::summarize` (same
/// instruction, same transcript rendering, same extractive fallback on
/// model failure). The extractive path is the production
/// `compaction::extractive`. Nothing about the production logic is
/// reimplemented here — if it changes, this evaluation follows it.
pub fn run_compaction_eval(world: &CompactionWorld, mode: CpMode) -> Result<CompactionReport> {
    let msgs = world.messages();
    let oracle_router;
    let (router, mode_name, model_name) = match mode {
        CpMode::Pipeline => {
            oracle_router = Router::single(compaction_oracle(world.oracle_summary.clone()));
            (
                &oracle_router,
                "pipeline",
                "compaction-oracle（脚本模型）".to_string(),
            )
        }
        CpMode::Live { router } => {
            let label = router
                .chain(Workload::Compaction)
                .iter()
                .map(|m| m.name())
                .collect::<Vec<_>>()
                .join(" → ");
            (router, "live", label)
        }
    };
    let (model_summary, used_model) = compaction::summarize(router, None, &msgs);
    let extractive_summary = compaction::extractive(None, &msgs);

    // Sanity: with a tiny threshold the plan must trigger and must cut
    // at a user boundary (the first kept message is a user turn).
    let (plan_ok, plan_detail) = match compaction::plan(&msgs, 100, 2) {
        Some(p) => {
            let first_kept = msgs.get(p.count);
            let ok = first_kept.map(|m| m.message.role == Role::User).unwrap_or(false);
            (
                ok,
                format!(
                    "阈值 100 tokens 时计划压缩 {} 条（upto_seq={}），首条保留消息是用户消息：{ok}",
                    p.count, p.upto_seq
                ),
            )
        }
        None => (
            false,
            "阈值 100 tokens 时 plan() 未触发（transcript 太短或用户轮次不足）".into(),
        ),
    };

    let mut model_errors: Vec<String> = Vec::new();
    for e in router.events() {
        let e: String = hatch_core::secrets::redact(&e).chars().take(300).collect();
        if !model_errors.contains(&e) && model_errors.len() < 5 {
            model_errors.push(e);
        }
    }
    let totals = router.totals();
    Ok(score(
        world,
        mode_name,
        &model_name,
        model_summary,
        used_model,
        extractive_summary,
        plan_ok,
        plan_detail,
        model_errors,
        Usage {
            calls: totals.calls,
            max_calls: None,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cached_tokens: totals.cached_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            // Pipeline totals come from the oracle's fabricated usage
            // (chars/4 prompt estimate + a fixed completion count), not
            // from any provider; live totals are provider-reported.
            estimated: mode_name == "pipeline",
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::default_world_dir;
    use std::sync::Arc;

    #[test]
    fn pipeline_run_passes_all_checks() {
        let w = CompactionWorld::load(&default_world_dir()).unwrap();
        let r = run_compaction_eval(&w, CpMode::Pipeline).unwrap();
        assert!(r.model_used_model, "pipeline must use the oracle model");
        assert_eq!((r.model_retained, r.model_total), (12, 13));
        assert_eq!((r.extractive_retained, r.extractive_total), (13, 13));
        assert!(
            r.extractive_ratio < 1.0,
            "extractive must compress below the source on this world, ratio {}",
            r.extractive_ratio
        );
        assert!(r.plan_ok, "{}", r.plan_detail);
        assert_eq!(r.passed(), r.checks.len(), "{}", r.render_markdown());
    }

    #[test]
    fn model_failure_falls_back_to_extractive_and_is_flagged() {
        use hatch_llm::scripted::ScriptedModel;
        use hatch_llm::LlmError;
        let w = CompactionWorld::load(&default_world_dir()).unwrap();
        let bad = Router::single(Arc::new(ScriptedModel::new(
            "down",
            vec![Err(LlmError::permanent("down"))],
        )));
        let r = run_compaction_eval(&w, CpMode::Live { router: &bad }).unwrap();
        assert!(!r.model_used_model, "failed model must fall back");
        assert_eq!(r.model_summary, r.extractive_summary);
        assert!(r.render_markdown().contains("未用到模型"));
        // The fallback is honest: it still passes the extractive bar.
        assert!(r.extractive_retained >= 6);
    }
}
