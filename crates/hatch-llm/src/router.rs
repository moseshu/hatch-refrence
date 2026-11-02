// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Workload routing with a fallback ladder and a hard call budget.
//!
//! Each workload (main chat, mail triage, drafting, compaction, memory flush)
//! maps to an ordered chain of models. For each model in the chain:
//! transient errors get exactly one retry, then the router moves to the next
//! model. A configured call budget is checked *before* every attempt (failed
//! attempts count too) so a misbehaving loop cannot burn through credits.

use crate::types::{ChatModel, ChatRequest, ChatResponse, Usage};
use crate::{LlmError, LlmErrorKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Workload {
    Main,
    Triage,
    Draft,
    Compaction,
    MemoryFlush,
}

impl Workload {
    pub const ALL: [Workload; 5] =
        [Workload::Main, Workload::Triage, Workload::Draft, Workload::Compaction, Workload::MemoryFlush];

    pub fn as_str(&self) -> &'static str {
        match self {
            Workload::Main => "main",
            Workload::Triage => "triage",
            Workload::Draft => "draft",
            Workload::Compaction => "compaction",
            Workload::MemoryFlush => "memory_flush",
        }
    }

    pub fn parse(s: &str) -> Option<Workload> {
        Self::ALL.into_iter().find(|w| w.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    pub workload: Workload,
    pub model: String,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    /// Attempts made, including failed ones.
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

pub struct Router {
    routes: HashMap<Workload, Vec<Arc<dyn ChatModel>>>,
    default_chain: Vec<Arc<dyn ChatModel>>,
    records: Mutex<Vec<UsageRecord>>,
    events: Mutex<Vec<String>>,
    calls: AtomicU64,
    max_calls: Option<u64>,
    default_max_tokens: Option<u32>,
    backoff: Duration,
}

impl Router {
    pub fn new(default_chain: Vec<Arc<dyn ChatModel>>) -> Self {
        Router {
            routes: HashMap::new(),
            default_chain,
            records: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
            max_calls: None,
            default_max_tokens: None,
            backoff: Duration::from_millis(800),
        }
    }

    /// Single-model router, handy in tests.
    pub fn single(model: Arc<dyn ChatModel>) -> Self {
        Self::new(vec![model]).with_backoff(Duration::ZERO)
    }

    pub fn route(mut self, w: Workload, chain: Vec<Arc<dyn ChatModel>>) -> Self {
        self.routes.insert(w, chain);
        self
    }

    pub fn with_max_calls(mut self, n: u64) -> Self {
        self.max_calls = Some(n);
        self
    }

    pub fn with_default_max_tokens(mut self, n: u32) -> Self {
        self.default_max_tokens = Some(n);
        self
    }

    pub fn with_backoff(mut self, d: Duration) -> Self {
        self.backoff = d;
        self
    }

    pub fn chain(&self, w: Workload) -> &[Arc<dyn ChatModel>] {
        self.routes.get(&w).map(Vec::as_slice).unwrap_or(&self.default_chain)
    }

    fn take_call(&self) -> Result<(), LlmError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(max) = self.max_calls {
            if n >= max {
                self.calls.fetch_sub(1, Ordering::SeqCst);
                return Err(LlmError::budget(format!("call budget of {max} exhausted")));
            }
        }
        Ok(())
    }

    pub fn chat(&self, w: Workload, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let chain = self.chain(w);
        if chain.is_empty() {
            return Err(LlmError::permanent(format!("no model configured for workload {}", w.as_str())));
        }
        let mut req = req.clone();
        if req.max_tokens.is_none() {
            req.max_tokens = self.default_max_tokens;
        }
        let mut last = LlmError::permanent("no attempt made");
        for model in chain {
            let mut retried = false;
            loop {
                self.take_call()?;
                match model.chat(&req) {
                    Ok(mut r) => {
                        if r.model.is_empty() {
                            r.model = model.name();
                        }
                        self.records.lock().unwrap().push(UsageRecord {
                            workload: w,
                            model: model.name(),
                            usage: r.usage,
                        });
                        return Ok(r);
                    }
                    Err(e) if e.kind == LlmErrorKind::Transient && !retried => {
                        self.event(format!("{}: transient error, retrying once: {}", model.name(), e.message));
                        retried = true;
                        std::thread::sleep(self.backoff);
                    }
                    Err(e) => {
                        self.event(format!("{}: giving up on this model: {}", model.name(), e.message));
                        last = e;
                        break;
                    }
                }
            }
        }
        Err(last)
    }

    fn event(&self, s: String) {
        self.events.lock().unwrap().push(s);
    }

    /// Fallback/retry events (for receipts and debugging).
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    /// Drain successful-call usage records (the CLI persists them).
    pub fn take_records(&self) -> Vec<UsageRecord> {
        std::mem::take(&mut *self.records.lock().unwrap())
    }

    pub fn totals(&self) -> UsageTotals {
        let recs = self.records.lock().unwrap();
        let mut t = UsageTotals { calls: self.calls.load(Ordering::SeqCst), ..Default::default() };
        for r in recs.iter() {
            t.prompt_tokens += r.usage.prompt_tokens;
            t.completion_tokens += r.usage.completion_tokens;
            t.cached_tokens += r.usage.cached_tokens;
        }
        t
    }

    pub fn budget_exhausted(&self) -> bool {
        matches!(self.max_calls, Some(m) if self.calls.load(Ordering::SeqCst) >= m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::ScriptedModel;
    use crate::types::ChatMessage;

    fn req() -> ChatRequest {
        ChatRequest::new(vec![ChatMessage::user("hi")])
    }

    #[test]
    fn transient_retry_then_fallback() {
        let a = Arc::new(ScriptedModel::new("a", vec![
            Err(LlmError::transient("503")),
            Err(LlmError::transient("503")),
        ]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a.clone(), b]).with_backoff(Duration::ZERO);
        let out = r.chat(Workload::Main, &req()).unwrap();
        assert_eq!(out.content.as_deref(), Some("from b"));
        assert_eq!(a.requests().len(), 2);
        assert_eq!(r.totals().calls, 3);
        assert_eq!(r.events().len(), 2);
    }

    #[test]
    fn permanent_error_skips_retry() {
        let a = Arc::new(ScriptedModel::new("a", vec![Err(LlmError::permanent("400"))]));
        let r = Router::single(a.clone());
        assert!(r.chat(Workload::Triage, &req()).is_err());
        assert_eq!(a.requests().len(), 1);
    }

    #[test]
    fn budget_stops_calls_and_default_max_tokens_applies() {
        let a = Arc::new(ScriptedModel::ok("a", vec![ChatResponse::text("1"), ChatResponse::text("2")]));
        let r = Router::single(a.clone()).with_max_calls(1).with_default_max_tokens(800);
        assert!(r.chat(Workload::Main, &req()).is_ok());
        let e = r.chat(Workload::Main, &req()).unwrap_err();
        assert!(e.is_budget());
        assert!(r.budget_exhausted());
        assert_eq!(a.requests()[0].max_tokens, Some(800));
        assert_eq!(r.totals().calls, 1);
    }

    #[test]
    fn per_workload_routes() {
        let main = Arc::new(ScriptedModel::ok("main", vec![]));
        let cheap = Arc::new(ScriptedModel::ok("cheap", vec![ChatResponse::text("summary")]));
        let r = Router::new(vec![main]).route(Workload::Compaction, vec![cheap]);
        let out = r.chat(Workload::Compaction, &req()).unwrap();
        assert_eq!(out.content.as_deref(), Some("summary"));
        assert_eq!(r.take_records()[0].model, "scripted:cheap");
        assert_eq!(Workload::parse("memory_flush"), Some(Workload::MemoryFlush));
    }
}
