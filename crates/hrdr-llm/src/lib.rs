//! `hrdr-llm` — a thin, provider-agnostic OpenAI chat-completions client.
//!
//! Points at any `/v1/chat/completions` endpoint via `base_url` (OpenAI,
//! `infr`, llama.cpp, OpenRouter, …). Supports native tool calls and SSE
//! streaming with tool-call reassembly via [`Accumulator`].

// Every test in this crate — including one written tomorrow by someone who read none
// of this — runs with `$HOME` and the XDG roots pointed at a throwaway directory. The
// `extern crate` is what links `hrdr-test-support`'s life-before-main ctor into this
// test binary; rustc drops a dependency nothing references, and a dropped ctor is a
// test writing the developer's real sessions. Do not remove it.
#[cfg(test)]
extern crate hrdr_test_support;

mod anthropic;
pub mod capped_read;
pub mod catalog;
mod client;
mod codex;
pub mod fs;
pub mod sse;
mod types;

pub use client::{
    ChatError, ChatErrorKind, ChatStream, Client, take_request_log_warning, url_host, wire_protocol,
};
pub use fs::unique_sibling_path;
pub use sse::{SseDecoder, SseEvent};
pub use types::{
    Accumulator, CacheMode, ChatChunk, ChatMessage, ChatRequest, ChunkChoice, Delta, FunctionCall,
    FunctionDef, MessageOrigin, RequestParams, Role, ToolCall, ToolDef, Usage,
    apply_cache_breakpoints, normalize_effort,
};
