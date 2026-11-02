// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Deterministic models for tests and offline (pipeline-mode) evaluation.

use crate::types::{ChatModel, ChatRequest, ChatResponse};
use crate::LlmError;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Replays a fixed queue of responses and records every request it saw.
pub struct ScriptedModel {
    name: String,
    queue: Mutex<VecDeque<Result<ChatResponse, LlmError>>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedModel {
    pub fn new(name: &str, responses: Vec<Result<ChatResponse, LlmError>>) -> Self {
        ScriptedModel {
            name: name.to_string(),
            queue: Mutex::new(responses.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Convenience: all responses succeed.
    pub fn ok(name: &str, responses: Vec<ChatResponse>) -> Self {
        Self::new(name, responses.into_iter().map(Ok).collect())
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

impl ChatModel for ScriptedModel {
    fn name(&self) -> String {
        format!("scripted:{}", self.name)
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.seen.lock().unwrap().push(req.clone());
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::permanent("scripted model exhausted")))
    }
}

type Handler = dyn Fn(&ChatRequest) -> Result<ChatResponse, LlmError> + Send + Sync;

/// A model backed by a closure — used by the evaluation oracle.
pub struct FnModel {
    name: String,
    f: Box<Handler>,
}

impl FnModel {
    pub fn new(
        name: &str,
        f: impl Fn(&ChatRequest) -> Result<ChatResponse, LlmError> + Send + Sync + 'static,
    ) -> Self {
        FnModel { name: name.to_string(), f: Box::new(f) }
    }
}

impl ChatModel for FnModel {
    fn name(&self) -> String {
        format!("fn:{}", self.name)
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        (self.f)(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    #[test]
    fn scripted_replays_then_errors() {
        let m = ScriptedModel::ok("t", vec![ChatResponse::text("a")]);
        let req = ChatRequest::new(vec![ChatMessage::user("x")]);
        assert_eq!(m.chat(&req).unwrap().content.as_deref(), Some("a"));
        assert!(m.chat(&req).is_err());
        assert_eq!(m.requests().len(), 2);
    }

    #[test]
    fn fn_model_calls_closure() {
        let m = FnModel::new("echo", |r| Ok(ChatResponse::text(r.messages[0].content.clone())));
        let req = ChatRequest::new(vec![ChatMessage::user("你好")]);
        assert_eq!(m.chat(&req).unwrap().content.as_deref(), Some("你好"));
    }
}
