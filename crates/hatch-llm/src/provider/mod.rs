//! HTTP model providers.
//!
//! * [`openai_compat`] — the Chat Completions wire format spoken by DeepSeek,
//!   Qwen (DashScope compatible mode), Moonshot, Zhipu, vLLM, Ollama and others.
//! * [`openai_responses`] — the Responses API format (`/responses`).
//! * [`anthropic`]     — the Messages API format.
//! * [`gemini`]        — Gemini `generateContent`.
//!
//! All four support true streaming (Server-Sent Events) through
//! [`crate::ChatModel::chat_stream`]; the assembled response equals the
//! buffered one.
//!
//! API keys are held in memory only; `Debug` output never shows them.

pub mod anthropic;
mod anthropic_stream;
mod chat_stream;
mod common;
pub mod gemini;
mod gemini_stream;
mod http;
mod json_mode;
mod net;
pub mod openai_compat;
pub mod openai_responses;
mod responses_stream;
pub(crate) mod sse;

pub use anthropic::AnthropicMessages;
pub use net::{http_agent, uses_proxy};
pub use gemini::Gemini;
pub use openai_compat::OpenAiCompatible;
pub use openai_responses::OpenAiResponses;
