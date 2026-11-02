// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! HTTP model providers.
//!
//! * [`openai_compat`] — the Chat Completions wire format spoken by DeepSeek,
//!   Qwen (DashScope compatible mode), Moonshot, Zhipu, vLLM, Ollama and others.
//! * [`anthropic`]     — the Messages API format.
//!
//! API keys are held in memory only; `Debug` output never shows them.

pub mod anthropic;
mod http;
pub mod openai_compat;

pub use anthropic::AnthropicMessages;
pub use openai_compat::OpenAiCompatible;
