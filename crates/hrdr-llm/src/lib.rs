//! `hrdr-llm` — a thin, provider-agnostic OpenAI chat-completions client.
//!
//! Points at any `/v1/chat/completions` endpoint via `base_url` (OpenAI,
//! `infr`, llama.cpp, OpenRouter, …). Supports native tool calls and SSE
//! streaming with tool-call reassembly via [`Accumulator`].

mod anthropic;
pub mod catalog;
mod client;
mod codex;
pub mod sse;
mod types;

pub use client::{ChatError, ChatErrorKind, ChatStream, Client, url_host, wire_protocol};
pub use sse::{SseDecoder, SseEvent};
pub use types::{
    Accumulator, CacheMode, ChatChunk, ChatMessage, ChatRequest, ChunkChoice, Delta, FunctionCall,
    FunctionDef, RequestParams, Role, ToolCall, ToolDef, Usage, apply_cache_breakpoints,
    normalize_effort,
};
