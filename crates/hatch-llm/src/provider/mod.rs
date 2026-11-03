//! HTTP model providers.
//!
//! * [`openai_compat`] — the Chat Completions wire format spoken by DeepSeek,
//!   Qwen (DashScope compatible mode), Moonshot, Zhipu, vLLM, Ollama and others.
//! * [`anthropic`]     — the Messages API format.
//!
//! API keys are held in memory only; `Debug` output never shows them.

pub mod anthropic;
mod http;
mod net;
pub mod openai_compat;

pub use anthropic::AnthropicMessages;
pub use openai_compat::OpenAiCompatible;
