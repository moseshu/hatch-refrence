//! `GET /v1/inference/usage` — proxied usage totals from the
//! inference ledger (`<home>/inference/usage.jsonl`).
//!
//! Query params: `day=YYYY-MM-DD`, `model=<name>`. Cost is an
//! estimate from the configured prices; `estimated_cost` is absent
//! when no prices are configured.

use super::{ApiCtx, ApiError};
use hatch_inference::ledger::{read_all, summarize};
use hatch_inference::ledger_path;
use hatch_mcp::http_proto::HttpRequest;
use serde_json::{json, Value};

pub fn usage(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let (entries, skipped) = read_all(&ledger_path(&ctx.app.home));
    let day = req.query_param("day");
    let model = req.query_param("model");
    let pricing = ctx.app.config.inference.pricing();
    let s = summarize(&entries, day.as_deref(), model.as_deref(), &pricing);
    let mut out = json!({
        "calls": s.calls,
        "ok_calls": s.ok_calls,
        "prompt_tokens": s.prompt_tokens,
        "completion_tokens": s.completion_tokens,
        "cached_tokens": s.cached_tokens,
        "reasoning_tokens": s.reasoning_tokens,
        "currency": s.currency,
        "skipped_lines": skipped,
        "per_model": s.per_model.iter().map(|(m, t)| json!({
            "model": m,
            "calls": t.calls,
            "prompt_tokens": t.prompt_tokens,
            "completion_tokens": t.completion_tokens,
            "cached_tokens": t.cached_tokens,
        })).collect::<Vec<_>>(),
    });
    if let Some(c) = s.cost {
        out["estimated_cost"] = json!(c);
    }
    Ok((200, out))
}
