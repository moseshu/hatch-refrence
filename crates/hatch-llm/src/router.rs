//! Workload routing with a fallback ladder and a hard call budget.
//!
//! Each workload (main chat, mail triage, drafting, compaction, memory flush)
//! maps to an ordered chain of models. For each model in the chain:
//! transient errors get exactly one retry, then the router moves to the next
//! model. A configured call budget is checked *before* every attempt (failed
//! attempts count too) so a misbehaving loop cannot burn through credits.

use crate::stream::{emit_response, StreamEvent, StreamSink};
use crate::types::{ChatModel, ChatRequest, ChatResponse, Usage};
use crate::{LlmError, LlmErrorKind};
pub use crate::workload::Workload;
use hatch_core::CancelToken;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    pub reasoning_tokens: u64,
}

/// Per-workload prompt-cache behaviour, aggregated from successful calls
/// only. `hit_ratio` is `cached_tokens / prompt_tokens` — a ratio of what
/// the provider *reported* as served from cache, never a claim about
/// cache placement or TTL expiry, which providers do not expose.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorkloadCacheStats {
    /// Successful calls recorded for this workload.
    pub calls: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
    pub hit_ratio: f64,
}

/// What triggered a ladder step. Classified from the error the backend
/// returned (see [`classify_trigger`]) — a heuristic over the existing
/// error kind + message text, not provider semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LadderTrigger {
    /// 4xx / bad request / refused parameters.
    #[default]
    RefusedParams,
    /// Timeout / network failure.
    Timeout,
    /// 5xx / server-side failure.
    ServerError,
    /// The backend answered with no text and no tool calls.
    EmptyAnswer,
}

impl LadderTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            LadderTrigger::RefusedParams => "refused_params",
            LadderTrigger::Timeout => "timeout",
            LadderTrigger::ServerError => "server_error",
            LadderTrigger::EmptyAnswer => "empty_answer",
        }
    }

    pub fn parse(s: &str) -> Option<LadderTrigger> {
        match s {
            "refused_params" => Some(LadderTrigger::RefusedParams),
            "timeout" => Some(LadderTrigger::Timeout),
            "server_error" => Some(LadderTrigger::ServerError),
            "empty_answer" => Some(LadderTrigger::EmptyAnswer),
            _ => None,
        }
    }
}

/// What the ladder does at one level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LadderAction {
    /// Retry the same model once (only meaningful for transient errors),
    /// then move to the next model.
    #[default]
    RetrySame,
    /// Skip the retry: move straight to the next model in the chain.
    NextModel,
    /// Stop here: return the error, do not try another model.
    Stop,
}

impl LadderAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            LadderAction::RetrySame => "retry_same",
            LadderAction::NextModel => "next_model",
            LadderAction::Stop => "stop",
        }
    }

    pub fn parse(s: &str) -> Option<LadderAction> {
        match s {
            "retry_same" => Some(LadderAction::RetrySame),
            "next_model" => Some(LadderAction::NextModel),
            "stop" => Some(LadderAction::Stop),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FallbackRule {
    pub trigger: LadderTrigger,
    pub action: LadderAction,
}

/// The fallback ladder, per workload. The default is conservative: a
/// refused-params or server error retries the same model once (transient
/// only) and then moves on; an empty answer stops outright instead of
/// burning a second model's call on a backend that produced nothing.
/// The default never switches provider — the chain a workload is
/// configured with is the chain it walks; switching provider needs an
/// explicit `[[routing.fallback]]` entry.
#[derive(Clone, Debug)]
pub struct FallbackTable {
    rules: HashMap<Workload, Vec<FallbackRule>>,
}

impl Default for FallbackTable {
    fn default() -> Self {
        let rules = vec![
            FallbackRule { trigger: LadderTrigger::RefusedParams, action: LadderAction::NextModel },
            FallbackRule { trigger: LadderTrigger::Timeout, action: LadderAction::RetrySame },
            FallbackRule { trigger: LadderTrigger::ServerError, action: LadderAction::RetrySame },
            FallbackRule { trigger: LadderTrigger::EmptyAnswer, action: LadderAction::Stop },
        ];
        FallbackTable { rules: Workload::ALL.into_iter().map(|w| (w, rules.clone())).collect() }
    }
}

impl FallbackTable {
    pub fn new(rules: HashMap<Workload, Vec<FallbackRule>>) -> Self {
        FallbackTable { rules }
    }

    /// Layer overrides for one workload onto the default ladder: rules
    /// for triggers not mentioned keep their default action.
    pub fn with_rules(mut self, w: Workload, overrides: Vec<FallbackRule>) -> Self {
        let base = self.rules.entry(w).or_insert_with(|| FallbackTable::default().rules.remove(&w).unwrap_or_default());
        for o in overrides {
            if let Some(existing) = base.iter_mut().find(|r| r.trigger == o.trigger) {
                existing.action = o.action;
            } else {
                base.push(o);
            }
        }
        self
    }

    pub fn action_for(&self, w: Workload, trigger: LadderTrigger) -> LadderAction {
        self.rules
            .get(&w)
            .and_then(|rs| rs.iter().find(|r| r.trigger == trigger))
            .map(|r| r.action)
            .unwrap_or_else(|| FallbackTable::default().action_for(w, trigger))
    }
}

/// Heuristic classification of an error into a ladder trigger, from the
/// error kind and message text. Timeouts and 5xx both arrive as
/// [`LlmErrorKind::Transient`], so the message decides between them;
/// anything transient without a marker is treated as a timeout.
pub fn classify_trigger(e: &LlmError) -> LadderTrigger {
    let m = e.message.to_lowercase();
    if m.contains("timeout")
        || m.contains("timed out")
        || m.contains("deadline")
        || m.contains("connection")
    {
        return LadderTrigger::Timeout;
    }
    if m.contains("500")
        || m.contains("502")
        || m.contains("503")
        || m.contains("504")
        || m.contains("5xx")
        || m.contains("server error")
        || m.contains("internal error")
        || m.contains("overloaded")
    {
        return LadderTrigger::ServerError;
    }
    match e.kind {
        LlmErrorKind::Permanent => LadderTrigger::RefusedParams,
        LlmErrorKind::Transient => LadderTrigger::Timeout,
        _ => LadderTrigger::ServerError,
    }
}

/// An answer with no text and no tool calls is unusable, not a success.
fn is_empty_answer(r: &ChatResponse) -> bool {
    r.content.as_deref().map(str::trim).unwrap_or("").is_empty() && r.tool_calls.is_empty()
}

pub struct Router {
    routes: HashMap<Workload, Vec<Arc<dyn ChatModel>>>,
    default_chain: Vec<Arc<dyn ChatModel>>,
    records: Mutex<Vec<UsageRecord>>,
    events: Mutex<Vec<String>>,
    calls: AtomicU64,
    max_calls: Option<u64>,
    default_max_tokens: Option<u32>,
    default_reasoning_effort: Option<String>,
    backoff: Duration,
    /// Prompt-cache TTL per workload (seconds). Only Anthropic honours it.
    cache_ttl: HashMap<Workload, u64>,
    fallback: FallbackTable,
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
            default_reasoning_effort: None,
            backoff: Duration::from_millis(800),
            cache_ttl: HashMap::new(),
            fallback: FallbackTable::default(),
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

    /// Default `reasoning_effort` for requests that do not set one.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.default_reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_backoff(mut self, d: Duration) -> Self {
        self.backoff = d;
        self
    }

    /// Prompt-cache TTL (seconds) for one workload. Only Anthropic
    /// honours it; other providers ignore the value. Validated upstream
    /// to 300 or 3600 — the only TTLs Anthropic accepts.
    pub fn with_cache_ttl(mut self, w: Workload, secs: u64) -> Self {
        self.cache_ttl.insert(w, secs);
        self
    }

    pub fn cache_ttl_for(&self, w: Workload) -> Option<u64> {
        self.cache_ttl.get(&w).copied()
    }

    /// Replace the fallback ladder (default: see [`FallbackTable`]).
    pub fn with_fallback_table(mut self, table: FallbackTable) -> Self {
        self.fallback = table;
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

    fn prepare(&self, w: Workload, req: &ChatRequest) -> ChatRequest {
        let mut req = req.clone();
        if req.max_tokens.is_none() {
            req.max_tokens = self.default_max_tokens;
        }
        if req.reasoning_effort.is_none() {
            req.reasoning_effort = self.default_reasoning_effort.clone();
        }
        if req.cache_ttl_secs.is_none() {
            req.cache_ttl_secs = self.cache_ttl.get(&w).copied();
        }
        req
    }

    fn record(&self, w: Workload, model: &Arc<dyn ChatModel>, mut r: ChatResponse) -> ChatResponse {
        if r.model.is_empty() {
            r.model = model.name();
        }
        self.records.lock().unwrap().push(UsageRecord { workload: w, model: model.name(), usage: r.usage });
        r
    }

    fn chain_for(&self, w: Workload) -> Result<&[Arc<dyn ChatModel>], LlmError> {
        let chain = self.chain(w);
        if chain.is_empty() {
            return Err(LlmError::permanent(format!("no model configured for workload {}", w.as_str())));
        }
        Ok(chain)
    }

    /// One attempt against `model`, cancellable when a token is given.
    fn attempt(
        model: &Arc<dyn ChatModel>,
        req: &ChatRequest,
        cancel: Option<&CancelToken>,
        sink: Option<&mut StreamSink<'_>>,
    ) -> Result<ChatResponse, LlmError> {
        match (cancel, sink) {
            (Some(t), s) => crate::cancel::call(model, req, t, s),
            (None, Some(s)) => model.chat_stream(req, s),
            (None, None) => model.chat(req),
        }
    }

    fn check(cancel: Option<&CancelToken>) -> Result<(), LlmError> {
        match cancel {
            Some(t) if t.is_cancelled() => Err(LlmError::cancelled(t.reason().unwrap_or_else(|| "cancelled".into()))),
            _ => Ok(()),
        }
    }

    fn pause(&self, cancel: Option<&CancelToken>) -> Result<(), LlmError> {
        match cancel {
            Some(t) => {
                t.sleep(self.backoff);
                Self::check(cancel)
            }
            None => {
                std::thread::sleep(self.backoff);
                Ok(())
            }
        }
    }

    pub fn chat(&self, w: Workload, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.chat_with(w, req, None)
    }

    /// [`Router::chat`] that stops as soon as `cancel` fires (no retries or
    /// fallbacks after a cancellation).
    pub fn chat_with(&self, w: Workload, req: &ChatRequest, cancel: Option<&CancelToken>) -> Result<ChatResponse, LlmError> {
        let chain = self.chain_for(w)?;
        let req = self.prepare(w, req);
        let mut last = LlmError::permanent("no attempt made");
        for (level, model) in chain.iter().enumerate() {
            let level = level + 1;
            let mut retried = false;
            loop {
                Self::check(cancel)?;
                self.take_call()?;
                match Self::attempt(model, &req, cancel, None) {
                    Ok(r) if is_empty_answer(&r) => {
                        self.event(format!(
                            "ladder {} level={} empty_answer: {} returned an empty answer, stopping (BackendError)",
                            w.as_str(),
                            level,
                            model.name()
                        ));
                        return Err(LlmError::backend(format!("{} returned an empty answer", model.name())));
                    }
                    Ok(r) => return Ok(self.record(w, model, r)),
                    Err(e) if e.is_cancelled() => return Err(e),
                    Err(e) => {
                        let trigger = classify_trigger(&e);
                        let action = self.fallback.action_for(w, trigger);
                        match action {
                            LadderAction::Stop => {
                                self.event(format!(
                                    "ladder {} level={} stop: {}: {} ({})",
                                    w.as_str(),
                                    level,
                                    model.name(),
                                    e.message,
                                    trigger.as_str()
                                ));
                                return Err(e);
                            }
                            LadderAction::RetrySame if e.kind == LlmErrorKind::Transient && !retried => {
                                self.event(format!(
                                    "ladder {} level={} retry_same: {}: transient error, retrying once: {}",
                                    w.as_str(),
                                    level,
                                    model.name(),
                                    e.message
                                ));
                                retried = true;
                                self.pause(cancel)?;
                            }
                            _ => {
                                self.event(format!(
                                    "ladder {} level={} {}: {}: giving up on this model: {}",
                                    w.as_str(),
                                    level,
                                    action.as_str(),
                                    model.name(),
                                    e.message
                                ));
                                last = e;
                                break;
                            }
                        }
                    }
                }
            }
        }
        Err(last)
    }

    /// Streaming chat with the same retry / fallback / budget rules as
    /// [`Router::chat`], plus two streaming-specific ones:
    ///
    /// * once any text or tool call has been emitted, a failure is returned as
    ///   is — switching models then would print a second, different answer;
    /// * if the endpoint rejects the streaming request before sending anything
    ///   (`StreamRejected`), the same model is asked once more in buffered mode
    ///   and its answer replayed as events.
    pub fn chat_stream(&self, w: Workload, req: &ChatRequest, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
        self.chat_stream_with(w, req, sink, None)
    }

    /// [`Router::chat_stream`] that stops as soon as `cancel` fires.
    pub fn chat_stream_with(
        &self,
        w: Workload,
        req: &ChatRequest,
        sink: &mut StreamSink<'_>,
        cancel: Option<&CancelToken>,
    ) -> Result<ChatResponse, LlmError> {
        let chain = self.chain_for(w)?;
        let req = self.prepare(w, req);
        let mut last = LlmError::permanent("no attempt made");
        for (level, model) in chain.iter().enumerate() {
            let level = level + 1;
            let mut retried = false;
            loop {
                Self::check(cancel)?;
                self.take_call()?;
                let mut emitted = false;
                let result = {
                    let mut wrapped = |e: StreamEvent| {
                        if e != StreamEvent::Done {
                            emitted = true;
                        }
                        sink(e)
                    };
                    Self::attempt(model, &req, cancel, Some(&mut wrapped))
                };
                let err = match result {
                    Ok(r) if is_empty_answer(&r) => {
                        self.event(format!(
                            "ladder {} level={} empty_answer: {} returned an empty answer, stopping (BackendError)",
                            w.as_str(),
                            level,
                            model.name()
                        ));
                        return Err(LlmError::backend(format!("{} returned an empty answer", model.name())));
                    }
                    Ok(r) => return Ok(self.record(w, model, r)),
                    Err(e) if e.is_cancelled() => return Err(e),
                    Err(e) => e,
                };
                if emitted {
                    self.event(format!("{}: failed mid-stream, not switching models: {}", model.name(), err.message));
                    return Err(err);
                }
                if err.kind == LlmErrorKind::StreamRejected {
                    self.event(format!("{}: streaming rejected ({}), retrying once buffered", model.name(), err.message));
                    Self::check(cancel)?;
                    self.take_call()?;
                    match Self::attempt(model, &req, cancel, None) {
                        Ok(r) if is_empty_answer(&r) => {
                            self.event(format!(
                                "ladder {} level={} empty_answer: {} returned an empty answer, stopping (BackendError)",
                                w.as_str(),
                                level,
                                model.name()
                            ));
                            return Err(LlmError::backend(format!("{} returned an empty answer", model.name())));
                        }
                        Ok(r) => {
                            emit_response(&r, sink);
                            return Ok(self.record(w, model, r));
                        }
                        Err(e) if e.is_cancelled() => return Err(e),
                        Err(e) => {
                            self.event(format!("{}: buffered retry failed: {}", model.name(), e.message));
                            last = e;
                            break;
                        }
                    }
                }
                let trigger = classify_trigger(&err);
                let action = self.fallback.action_for(w, trigger);
                if action == LadderAction::Stop {
                    self.event(format!(
                        "ladder {} level={} stop: {}: {} ({})",
                        w.as_str(),
                        level,
                        model.name(),
                        err.message,
                        trigger.as_str()
                    ));
                    return Err(err);
                }
                if err.kind == LlmErrorKind::Transient && !retried && action == LadderAction::RetrySame {
                    self.event(format!(
                        "ladder {} level={} retry_same: {}: transient error, retrying once: {}",
                        w.as_str(),
                        level,
                        model.name(),
                        err.message
                    ));
                    retried = true;
                    self.pause(cancel)?;
                    continue;
                }
                self.event(format!(
                    "ladder {} level={} {}: {}: giving up on this model: {}",
                    w.as_str(),
                    level,
                    action.as_str(),
                    model.name(),
                    err.message
                ));
                last = err;
                break;
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
            t.reasoning_tokens += r.usage.reasoning_tokens;
        }
        t
    }

    pub fn budget_exhausted(&self) -> bool {
        matches!(self.max_calls, Some(m) if self.calls.load(Ordering::SeqCst) >= m)
    }

    /// Per-workload cache stats from successful calls only (see
    /// [`WorkloadCacheStats`]). Failed attempts never record usage, so
    /// they do not appear here; `calls` counts successes, not attempts.
    pub fn cache_stats(&self) -> BTreeMap<Workload, WorkloadCacheStats> {
        let recs = self.records.lock().unwrap();
        let mut map: BTreeMap<Workload, WorkloadCacheStats> = BTreeMap::new();
        for r in recs.iter() {
            let s = map.entry(r.workload).or_default();
            s.calls += 1;
            s.prompt_tokens += r.usage.prompt_tokens;
            s.cached_tokens += r.usage.cached_tokens;
            s.total_tokens += r.usage.prompt_tokens + r.usage.completion_tokens;
        }
        for s in map.values_mut() {
            s.hit_ratio = if s.prompt_tokens == 0 {
                0.0
            } else {
                s.cached_tokens as f64 / s.prompt_tokens as f64
            };
        }
        map
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
    fn cancellation_is_never_retried_or_routed_to_a_fallback() {
        let a = Arc::new(ScriptedModel::ok("a", vec![ChatResponse::text("a")]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("b")]));
        let r = Router::new(vec![a.clone(), b.clone()]).with_backoff(Duration::ZERO);
        let t = CancelToken::new();
        t.cancel("Ctrl-C");
        let e = r.chat_with(Workload::Main, &req(), Some(&t)).unwrap_err();
        assert!(e.is_cancelled());
        let mut sink = |_e: StreamEvent| {};
        assert!(r.chat_stream_with(Workload::Main, &req(), &mut sink, Some(&t)).unwrap_err().is_cancelled());
        assert_eq!(a.requests().len() + b.requests().len(), 0);
        assert_eq!(r.totals().calls, 0);
        // A live token behaves like a plain call.
        let ok = r.chat_with(Workload::Main, &req(), Some(&CancelToken::new())).unwrap();
        assert_eq!(ok.content.as_deref(), Some("a"));
        assert!(matches!(hatch_core::Error::from(e), hatch_core::Error::Cancelled(_)));
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

    /// One scripted streaming attempt: events emitted, then the outcome.
    type StreamStep = Result<(Vec<StreamEvent>, ChatResponse), (Vec<StreamEvent>, LlmError)>;

    /// A model whose streaming path is scripted separately from its buffered path.
    struct StreamModel {
        stream: Mutex<Vec<StreamStep>>,
        buffered: Mutex<Vec<Result<ChatResponse, LlmError>>>,
    }

    impl ChatModel for StreamModel {
        fn name(&self) -> String {
            "stream-model".into()
        }
        fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.buffered.lock().unwrap().remove(0)
        }
        fn chat_stream(&self, _req: &ChatRequest, sink: &mut StreamSink<'_>) -> Result<ChatResponse, LlmError> {
            match self.stream.lock().unwrap().remove(0) {
                Ok((evs, r)) => {
                    evs.into_iter().for_each(&mut *sink);
                    Ok(r)
                }
                Err((evs, e)) => {
                    evs.into_iter().for_each(&mut *sink);
                    Err(e)
                }
            }
        }
    }

    fn collect(r: &Router, w: Workload) -> (Result<ChatResponse, LlmError>, Vec<StreamEvent>) {
        let mut evs = Vec::new();
        let out = r.chat_stream(w, &req(), &mut |e| evs.push(e));
        (out, evs)
    }

    #[test]
    fn stream_rejected_falls_back_to_buffered_once() {
        let m = Arc::new(StreamModel {
            stream: Mutex::new(vec![Err((vec![], LlmError::stream_rejected("HTTP 400")))]),
            buffered: Mutex::new(vec![Ok(ChatResponse::text("buffered"))]),
        });
        let r = Router::single(m);
        let (out, evs) = collect(&r, Workload::Main);
        assert_eq!(out.unwrap().content.as_deref(), Some("buffered"));
        assert_eq!(evs, vec![StreamEvent::TextDelta("buffered".into()), StreamEvent::Done]);
        assert_eq!(r.totals().calls, 2);
        assert!(r.events()[0].contains("retrying once buffered"));
    }

    #[test]
    fn no_model_switch_after_output_started() {
        let a = Arc::new(StreamModel {
            stream: Mutex::new(vec![Err((vec![StreamEvent::TextDelta("半".into())], LlmError::transient("reset")))]),
            buffered: Mutex::new(vec![]),
        });
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("other")]));
        let r = Router::new(vec![a, b.clone()]).with_backoff(Duration::ZERO);
        let (out, evs) = collect(&r, Workload::Main);
        assert!(out.is_err());
        assert_eq!(evs, vec![StreamEvent::TextDelta("半".into())]);
        assert!(b.requests().is_empty(), "fallback model must not run after partial output");
    }

    #[test]
    fn stream_errors_before_output_use_the_ladder_and_default_impl() {
        let a = Arc::new(StreamModel {
            stream: Mutex::new(vec![Err((vec![], LlmError::transient("503"))), Err((vec![], LlmError::transient("503")))]),
            buffered: Mutex::new(vec![]),
        });
        // ScriptedModel has no native streaming: the trait default replays its answer.
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a, b]).with_backoff(Duration::ZERO).with_default_max_tokens(77);
        let (out, evs) = collect(&r, Workload::Main);
        assert_eq!(out.unwrap().content.as_deref(), Some("from b"));
        assert_eq!(evs.last(), Some(&StreamEvent::Done));
        assert_eq!(r.totals().calls, 3);
        assert_eq!(r.take_records()[0].model, "scripted:b");
    }

    fn usage(prompt: u64, cached: u64, completion: u64) -> Usage {
        Usage { prompt_tokens: prompt, cached_tokens: cached, completion_tokens: completion, reasoning_tokens: 0 }
    }

    #[test]
    fn cache_stats_aggregate_per_workload_from_observed_usage() {
        let a = Arc::new(ScriptedModel::ok("a", vec![
            ChatResponse { content: Some("x".into()), usage: usage(100, 60, 10), ..Default::default() },
            ChatResponse { content: Some("y".into()), usage: usage(200, 0, 5), ..Default::default() },
        ]));
        let r = Router::single(a).with_backoff(Duration::ZERO);
        r.chat(Workload::Main, &req()).unwrap();
        r.chat(Workload::Main, &req()).unwrap();
        let stats = r.cache_stats();
        let m = stats.get(&Workload::Main).unwrap();
        assert_eq!((m.calls, m.prompt_tokens, m.cached_tokens, m.total_tokens), (2, 300, 60, 315));
        assert!((m.hit_ratio - 0.2).abs() < 1e-9, "hit ratio is cached/prompt as reported");
        assert!(!stats.contains_key(&Workload::Brief), "workloads with no calls have no entry");
    }

    #[test]
    fn cache_ttl_is_applied_per_workload_and_never_overrides_the_request() {
        let a = Arc::new(ScriptedModel::ok("a", vec![ChatResponse::text("1"), ChatResponse::text("2")]));
        let r = Router::single(a.clone()).with_cache_ttl(Workload::Brief, 300).with_backoff(Duration::ZERO);
        r.chat(Workload::Brief, &req()).unwrap();
        assert_eq!(a.requests()[0].cache_ttl_secs, Some(300));
        r.chat(Workload::Main, &req()).unwrap();
        assert_eq!(a.requests()[1].cache_ttl_secs, None, "TTL is per workload");
        let mut explicit = req();
        explicit.cache_ttl_secs = Some(3600);
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("3")]));
        let r2 = Router::single(b.clone()).with_cache_ttl(Workload::Brief, 300);
        r2.chat(Workload::Brief, &explicit).unwrap();
        assert_eq!(b.requests()[0].cache_ttl_secs, Some(3600));
        assert_eq!(r.cache_ttl_for(Workload::Brief), Some(300));
    }

    #[test]
    fn ladder_refused_params_goes_to_next_model_without_retry() {
        let a = Arc::new(ScriptedModel::new("a", vec![Err(LlmError::permanent("400 bad request: unknown parameter"))]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a.clone(), b]).with_backoff(Duration::ZERO);
        assert_eq!(r.chat(Workload::Main, &req()).unwrap().content.as_deref(), Some("from b"));
        assert_eq!(a.requests().len(), 1, "refused params must not be retried on the same model");
        assert!(r.events().iter().any(|e| e.contains("ladder main") && e.contains("next_model")));
    }

    #[test]
    fn ladder_timeout_retries_once_then_moves_on() {
        let a = Arc::new(ScriptedModel::new("a", vec![
            Err(LlmError::transient("request timeout")),
            Err(LlmError::transient("request timeout")),
        ]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a.clone(), b]).with_backoff(Duration::ZERO);
        assert_eq!(r.chat(Workload::Main, &req()).unwrap().content.as_deref(), Some("from b"));
        assert_eq!(a.requests().len(), 2);
        assert_eq!(classify_trigger(&LlmError::transient("request timeout")), LadderTrigger::Timeout);
        assert_eq!(classify_trigger(&LlmError::transient("HTTP 503")), LadderTrigger::ServerError);
        assert_eq!(classify_trigger(&LlmError::permanent("400")), LadderTrigger::RefusedParams);
    }

    #[test]
    fn ladder_empty_answer_stops_with_backend_error_and_records_nothing() {
        let a = Arc::new(ScriptedModel::ok("a", vec![ChatResponse::text("   ")]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a.clone(), b.clone()]).with_backoff(Duration::ZERO);
        let e = r.chat(Workload::Main, &req()).unwrap_err();
        assert_eq!(e.kind, LlmErrorKind::Backend);
        assert!(b.requests().is_empty(), "empty answer must not fall through to the next model");
        assert!(r.take_records().is_empty(), "empty answers record no usage");
        assert!(r.events().iter().any(|e| e.contains("empty_answer") && e.contains("BackendError")));
    }

    #[test]
    fn ladder_stop_action_is_honoured_for_server_errors() {
        let table = FallbackTable::new(
            [(Workload::Main, vec![FallbackRule { trigger: LadderTrigger::ServerError, action: LadderAction::Stop }])]
                .into_iter()
                .collect(),
        );
        let a = Arc::new(ScriptedModel::new("a", vec![Err(LlmError::transient("HTTP 503"))]));
        let b = Arc::new(ScriptedModel::ok("b", vec![ChatResponse::text("from b")]));
        let r = Router::new(vec![a.clone(), b.clone()]).with_backoff(Duration::ZERO).with_fallback_table(table);
        assert!(r.chat(Workload::Main, &req()).is_err());
        assert!(b.requests().is_empty(), "stop action must not try the next model");
        assert!(r.events().iter().any(|e| e.contains("level=1 stop")));
    }
}
