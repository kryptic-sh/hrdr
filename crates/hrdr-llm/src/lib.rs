//! `hrdr-llm` — a thin, provider-agnostic OpenAI chat-completions client.
//!
//! Points at any `/v1/chat/completions` endpoint via `base_url` (OpenAI,
//! `infr`, llama.cpp, OpenRouter, …). Supports native tool calls and SSE
//! streaming with tool-call reassembly via [`Accumulator`].

mod client;
mod types;

pub use client::{ChatStream, Client};
pub use types::{
    Accumulator, CacheMode, ChatChunk, ChatMessage, ChatRequest, ChunkChoice, Delta, FunctionCall,
    FunctionDef, Role, ToolCall, ToolDef, Usage, apply_cache_breakpoints, normalize_effort,
};
