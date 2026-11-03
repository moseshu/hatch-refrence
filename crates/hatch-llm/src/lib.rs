//! `hatch-llm` — everything that talks to a language model.
//!
//! * [`types`]        — provider-neutral chat request/response types.
//! * [`provider`]     — Chat-Completions-compatible and Anthropic Messages backends.
//! * [`scripted`]     — deterministic models for tests and offline evaluation.
//! * [`router`]       — per-workload routing, fallback ladder, usage and call budget.
//! * [`json_extract`] — robust JSON extraction from model text.

pub mod error;
pub mod json_extract;
pub mod provider;
pub mod router;
pub mod scripted;
pub mod types;

pub use error::{LlmError, LlmErrorKind};
pub use router::{Router, UsageRecord, UsageTotals, Workload};
pub use types::{ChatMessage, ChatModel, ChatRequest, ChatResponse, Role, ToolCall, ToolSpec, Usage};
