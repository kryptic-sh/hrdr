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
mod retry;
pub mod sse;
mod types;

#[doc(hidden)]
pub use client::{
    Backend, ChatError, ChatErrorKind, ChatStream, Client, UNNAMED_MODEL, is_anthropic_backend,
    is_local_host, serve_response, take_client_warning, url_host,
};
pub use fs::{
    owner_only_options, owner_only_options_no_follow, sibling_with_suffix, unique_sibling_path,
    write_atomic,
};
pub use retry::{
    MAX_BACKOFF, RetryAttempt, RetryBudget, RetryPolicy, UnsupportedParam, is_context_overflow,
    is_transient, retry_after_hint, unsupported_param,
};
pub use sse::{SseDecoder, SseEvent, SseOverflow};
pub use types::{
    Accumulator, CacheMode, ChatChunk, ChatMessage, ChatRequest, ChunkChoice, CompactionReason,
    Delta, FunctionCall, FunctionDef, FunctionDelta, MessageOrigin, RequestParams, Role, ToolCall,
    ToolCallDelta, ToolDef, Usage, apply_cache_breakpoints, normalize_effort,
};
