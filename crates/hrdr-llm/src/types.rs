//! OpenAI chat-completions wire types — the subset hrdr speaks.
//!
//! hrdr only ever sends structured `messages[]` + `tools[]`; the server
//! (e.g. `infr`) owns chat-template application and tool-call parsing. We do
//! not render model prompt formats here.

use serde::{Deserialize, Deserializer, Serialize};

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Internal origin of a message — distinguishes a real user turn from synthetic
/// user-role context injected by the agent (steering, background results,
/// turn-end nudges, …).
///
/// Lets the agent tell a real user turn apart from synthetic `Role::User`
/// context it injected. Defaults to [`User`](MessageOrigin::User) so that
/// existing serialized data (session files) correctly treats all messages as
/// real user turns.
///
/// **Never serialized onto the provider wire** — only the session file preserves
/// it (see `persisted_messages` in `hrdr-app`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MessageOrigin {
    #[default]
    User,
    Steering,
    BackgroundResult,
    /// A synthetic prompt the harness injects when the model ends its turn
    /// with no tool calls while the shared TODO list still has unfinished
    /// items — never a real user turn. See `Agent::run`'s turn loop.
    Nudge,
}

/// A single chat message. Used for both request and response — `content` is
/// optional because assistant turns that only call tools carry no text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Model "thinking" channel (infr/Qwen3 etc). Received-only; **never sent**.
    /// `skip_serializing` (not `skip_serializing_if`) so it's always dropped on
    /// the wire: reasoning models degrade badly — repetition/gibberish — when a
    /// prior turn's `<think>` is fed back into the prompt, so history must carry
    /// only the final answer. Kept in the struct for display + deserialization.
    #[serde(default, skip_serializing)]
    pub reasoning_content: Option<String>,
    /// Anthropic extended-thinking blocks (type/thinking/signature triples, or
    /// type/data for redacted). Captured verbatim during streaming for re-emission
    /// in the native Anthropic assistant message when tool_use is also present —
    /// Anthropic requires the thinking block with its signature on the follow-up
    /// turn. **Never serialized** — same invariant as `reasoning_content`: these
    /// are Anthropic-wire-only and must not go on the OpenAI wire.
    #[serde(default, skip_serializing)]
    pub anthropic_thinking_blocks: Vec<serde_json::Value>,
    /// Internal origin marker — distinguishes real user turns from synthetic
    /// user-role context injected by the agent (steering, background results).
    /// Defaults to [`MessageOrigin::User`] (a real user turn) for backward
    /// compatibility with existing session files.
    ///
    /// Never written onto the provider wire (`skip_serializing`); the session
    /// file preserves it via `persisted_messages`.
    #[serde(default, skip_serializing)]
    pub origin: MessageOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Set on `Role::Tool` messages to bind the result to its call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(text.into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// A `Role::Tool` result message bound to `call_id`.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: None,
        }
    }
}

/// A native tool call emitted by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON string of arguments (OpenAI sends this as a string, not an object).
    pub arguments: String,
}

fn function_kind() -> String {
    "function".to_string()
}

/// A tool definition advertised to the model in the request `tools[]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the call arguments.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Request body for `POST /v1/chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Reasoning-effort hint for reasoning models (`minimal`/`low`/`medium`/
    /// `high`). OpenAI-standard field; Anthropic's OpenAI-compat maps it to a
    /// thinking budget. Unset for non-reasoning models / servers (which ignore
    /// unknown fields anyway).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Output-token cap. Sent only when configured. OpenAI's reasoning models
    /// (o-series, gpt-5) reject `max_tokens` and require `max_completion_tokens`,
    /// so the client routes the value to whichever the model expects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// `max_tokens` alias for OpenAI reasoning models (see `max_tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Nucleus-sampling probability mass. Opt-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Best-effort determinism seed (supported by some providers). Opt-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Stop sequences. Opt-in (agentic turns usually stop via tools/end-of-turn).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// Opt-in request parameters carried by the [`Client`](crate::Client) and applied
/// to each OpenAI-shape request. All default to "not sent" so no strict provider
/// 400s on an unexpected field; `include_usage` defaults on (for token stats).
#[derive(Debug, Clone)]
pub struct RequestParams {
    /// Output-token cap. Also the `max_tokens` the native Anthropic backend
    /// requires (falls back to its default when `None`).
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    /// Ask the server for a final usage chunk (`stream_options.include_usage`).
    /// A few strict/old servers reject it — set `false` to omit.
    pub include_usage: bool,
}

impl Default for RequestParams {
    fn default() -> Self {
        Self {
            max_tokens: None,
            top_p: None,
            seed: None,
            stop: Vec::new(),
            include_usage: true,
        }
    }
}

/// Normalize a reasoning-effort label to a value worth sending as
/// `reasoning_effort`, or `None` for anything unrecognized (a display-only label
/// like `off`, or garbage) so it's never put on the wire. The full ladder is
/// what models.dev catalogs across models (`none` … `max`); which subset a
/// given model accepts is the model's own `reasoning_options` — the `/effort`
/// picker only offers that subset.
pub fn normalize_effort(label: &str) -> Option<String> {
    match label.trim().to_ascii_lowercase().as_str() {
        s @ ("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max") => {
            Some(s.to_string())
        }
        _ => None,
    }
}

/// Streaming options. `include_usage` asks the server to emit a final chunk
/// carrying token counts (OpenAI / llama-server support this).
#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Prompt-caching strategy for outgoing requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// No cache breakpoints emitted.
    #[default]
    Off,
    /// Emit `cache_control: {"type": "ephemeral"}` breakpoints. Useful for
    /// endpoints that consume the marker — OpenRouter (for its
    /// Anthropic/Gemini/Qwen models) and the **native Anthropic Messages API**
    /// (where breakpoints land on system, the last tool, and the last message).
    /// Some direct provider endpoints **reject** an unknown `cache_control` field
    /// with a `400` (OpenAI, Groq, xAI) and others silently ignore it, so which
    /// endpoints get this is decided upstream (hrdr's `resolve_cache_mode`), not
    /// here. The exact placement differs by backend (OpenAI-shape vs Anthropic).
    Ephemeral,
}

/// Mark cache breakpoints on a serialized chat-request body (`messages[]`): the
/// first `system` message and the last message each get a `cache_control`
/// marker, converting their string `content` into a one-element content-parts
/// array. A supporting provider (e.g. OpenRouter) caches the prefix up to and
/// including each marked block (≤4 breakpoints allowed; we use ≤2), so the
/// stable system+tools prefix and the growing conversation prefix are reused
/// turn to turn. Only call this for endpoints known to accept the marker — see
/// [`CacheMode::Ephemeral`]. No-op when there are no messages, or a target's
/// `content` isn't a plain string (already parts, or a tool-call-only assistant
/// turn with no text).
pub fn apply_cache_breakpoints(body: &mut serde_json::Value, ttl_1h: bool) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let last = messages.len() - 1;
    let system = messages
        .iter()
        .position(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"));
    if let Some(i) = system {
        mark_cache(&mut messages[i], ttl_1h);
    }
    // Rolling breakpoint on the last message (unless it's the system we marked).
    if Some(last) != system {
        mark_cache(&mut messages[last], ttl_1h);
    }
}

/// A `cache_control` marker; `ttl_1h` requests the extended 1-hour cache TTL
/// (default is the provider's ~5-minute ephemeral).
pub(crate) fn cache_control(ttl_1h: bool) -> serde_json::Value {
    if ttl_1h {
        serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
    } else {
        serde_json::json!({ "type": "ephemeral" })
    }
}

/// Rewrite a message's string `content` into `[{type:text, text, cache_control}]`.
fn mark_cache(msg: &mut serde_json::Value, ttl_1h: bool) {
    let Some(text) = msg
        .get("content")
        .and_then(|c| c.as_str())
        .map(str::to_owned)
    else {
        return;
    };
    msg["content"] = serde_json::json!([{
        "type": "text",
        "text": text,
        "cache_control": cache_control(ttl_1h),
    }]);
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    /// OpenAI-style breakdown of the prompt (`cached_tokens` = prompt-cache hits).
    #[serde(default)]
    pub prompt_tokens_details: TokenDetails,
    /// OpenAI-style breakdown of the completion (`reasoning_tokens`).
    #[serde(default)]
    pub completion_tokens_details: TokenDetails,
}

/// Per-side token breakdown some providers report (`prompt_tokens_details` /
/// `completion_tokens_details`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenDetails {
    /// Prompt tokens served from the prompt cache (a cache hit).
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    /// Completion tokens spent on reasoning/thinking.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl Usage {
    /// Prompt tokens that were a cache hit, if the provider reported it.
    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details.cached_tokens
    }

    /// Completion tokens spent on reasoning/thinking, if reported.
    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details.reasoning_tokens
    }
}

// ---- streaming ----

/// Deserialize a field that tolerates an explicit JSON `null` the same as an
/// absent key, falling back to `T::default()`. Plain `#[serde(default)]` only
/// supplies the default when the *key* is missing — some OpenAI-compatible
/// proxies instead emit an explicit `"choices": null` or `"delta": null`,
/// which `Vec`'s / a struct's own `Deserialize` impl rejects (`null` isn't a
/// sequence or a map), turning what should be an empty/no-op chunk into a
/// terminal stream failure.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// One `chat.completion.chunk` SSE event. The final chunk (when `include_usage`
/// is set) carries `usage` with empty `choices`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    /// `null_as_default`: some proxies send `"choices": null` instead of
    /// omitting the key; treat it the same as absent (empty chunk).
    #[serde(default, deserialize_with = "null_as_default")]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Completed Anthropic thinking blocks accumulated during streaming (emitted
    /// as a single synthetic chunk after the byte loop). Only populated on the
    /// native Anthropic path; ignored by the OpenAI path via `#[serde(skip)]`.
    /// Never serialized.
    #[serde(skip)]
    pub anthropic_thinking_blocks: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    /// `null_as_default`: some proxies send `"delta": null` on a choice that
    /// carries nothing new (e.g. alongside `finish_reason`); treat it the same
    /// as absent (an empty delta).
    #[serde(default, deserialize_with = "null_as_default")]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// OpenAI-shaped [`ChatChunk`] constructors shared by the backend event-mappers
// (`anthropic`, `codex`): they translate a provider's native stream event into
// the chunk shape the [`Accumulator`] consumes. Protocol-agnostic — the only
// per-backend part is deciding *which* to emit for a given event.

/// A chunk carrying a text delta.
pub(crate) fn text_chunk(text: String) -> ChatChunk {
    delta_chunk(Delta {
        content: Some(text),
        ..Delta::default()
    })
}

/// A chunk carrying a reasoning/thinking delta.
pub(crate) fn reasoning_chunk(text: String) -> ChatChunk {
    delta_chunk(Delta {
        reasoning_content: Some(text),
        ..Delta::default()
    })
}

/// A chunk carrying one tool-call delta (fragment of a streamed tool call).
pub(crate) fn tool_call_chunk(
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
) -> ChatChunk {
    delta_chunk(Delta {
        tool_calls: Some(vec![ToolCallDelta {
            index,
            id,
            function: Some(FunctionDelta { name, arguments }),
        }]),
        ..Delta::default()
    })
}

/// Wrap a [`Delta`] into a single-choice [`ChatChunk`] with no finish reason.
pub(crate) fn delta_chunk(delta: Delta) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            delta,
            finish_reason: None,
        }],
        usage: None,
        anthropic_thinking_blocks: vec![],
    }
}

/// Folds streaming chunks back into a single assistant [`ChatMessage`].
///
/// Tool-call deltas arrive fragmented (name on the first delta, arguments
/// split across many); this reassembles them by `index`.
#[derive(Debug, Default)]
pub struct Accumulator {
    pub content: String,
    pub reasoning: String,
    /// Token usage from the final `include_usage` chunk, if the server sent it.
    pub usage: Option<Usage>,
    /// The last `finish_reason` the server reported (`stop`, `tool_calls`,
    /// `length`, …). `length` means the reply was cut off at the output cap.
    pub finish_reason: Option<String>,
    calls: Vec<ToolCall>,
    /// Anthropic thinking blocks (with signature) for re-emission in the native
    /// Messages API request. Never serialized — same invariant as reasoning_content.
    anthropic_thinking_blocks: Vec<serde_json::Value>,
    /// Per-accumulator draw from [`NEXT_ACCUMULATOR_NONCE`], mixed into
    /// synthesized tool-call ids in [`Self::into_message`] so they're unique
    /// across turns, not just within one (see that method's doc comment).
    nonce: u64,
}

/// Process-wide monotonic counter, one draw per [`Accumulator::new`]. Backs the
/// `nonce` field: synthesized tool-call ids (`call_{nonce}_{i}`) must not
/// collide across turns, and a plain per-turn index (`call_{i}`) alone repeats
/// every turn. A counter (not a random id) keeps ids deterministic — this
/// crate has no RNG handy, and doesn't need one just for this.
static NEXT_ACCUMULATOR_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Accumulator {
    pub fn new() -> Self {
        Self {
            nonce: NEXT_ACCUMULATOR_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..Self::default()
        }
    }

    /// Merge one chunk. Returns the freshly-appended text delta (for live
    /// rendering), if any.
    pub fn push(&mut self, chunk: &ChatChunk) -> Option<String> {
        // The usage chunk arrives with empty `choices`, so capture it before
        // the early return below.
        if let Some(new) = &chunk.usage {
            match &mut self.usage {
                None => self.usage = chunk.usage.clone(),
                Some(existing) => {
                    // Anthropic emits usage in two events: message_start (prompt + cache
                    // counters) then message_delta (completion only). Taking max preserves
                    // both without knowing the emission order.
                    existing.prompt_tokens = existing.prompt_tokens.max(new.prompt_tokens);
                    existing.completion_tokens =
                        existing.completion_tokens.max(new.completion_tokens);
                    // Keep existing detail field if new chunk has None (don't clobber).
                    if new.prompt_tokens_details.cached_tokens.is_some() {
                        existing.prompt_tokens_details.cached_tokens =
                            new.prompt_tokens_details.cached_tokens;
                    }
                    if new.completion_tokens_details.reasoning_tokens.is_some() {
                        existing.completion_tokens_details.reasoning_tokens =
                            new.completion_tokens_details.reasoning_tokens;
                    }
                }
            }
        }
        if !chunk.anthropic_thinking_blocks.is_empty() {
            self.anthropic_thinking_blocks
                .extend(chunk.anthropic_thinking_blocks.iter().cloned());
        }
        let choice = chunk.choices.first()?;
        if let Some(fr) = &choice.finish_reason {
            self.finish_reason = Some(fr.clone());
        }
        if let Some(r) = &choice.delta.reasoning_content {
            self.reasoning.push_str(r);
        }
        for tc in choice.delta.tool_calls.iter().flatten() {
            // `index` is server-supplied. A garbage value (billions, or
            // usize::MAX which overflows `+ 1`) would OOM or panic on the resize,
            // so cap it. No real provider emits more than a handful of parallel
            // calls per turn.
            const MAX_TOOL_CALLS: usize = 1024;
            if tc.index >= MAX_TOOL_CALLS {
                continue;
            }
            if self.calls.len() <= tc.index {
                self.calls.resize_with(tc.index + 1, || ToolCall {
                    id: String::new(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: String::new(),
                        arguments: String::new(),
                    },
                });
            }
            let slot = &mut self.calls[tc.index];
            if let Some(id) = &tc.id
                && !id.is_empty()
            {
                slot.id = id.clone();
            }
            if let Some(f) = &tc.function {
                if let Some(name) = &f.name {
                    slot.function.name.push_str(name);
                }
                if let Some(args) = &f.arguments {
                    slot.function.arguments.push_str(args);
                }
            }
        }
        let delta = choice.delta.content.clone();
        if let Some(text) = &delta {
            self.content.push_str(text);
        }
        delta
    }

    /// Whether the reply was cut off at the model's output cap (`finish_reason`
    /// `length`, or Anthropic's `max_tokens`) rather than finishing naturally.
    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length" | "max_tokens"))
    }

    /// Assemble the final assistant message.
    pub fn into_message(mut self) -> ChatMessage {
        // Some servers omit tool-call ids. Synthesize a stable one per call so
        // the assistant message and its `role:"tool"` results correlate — and so
        // multiple calls in one turn don't collide on an empty id. The index
        // alone is only unique *within* this turn; mix in this accumulator's
        // nonce so replaying two id-less tool turns to the native Anthropic
        // backend never sends the same `tool_use` id twice (Anthropic rejects
        // duplicates).
        for (i, call) in self.calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{}_{i}", self.nonce);
            }
        }
        ChatMessage {
            role: Role::Assistant,
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
            anthropic_thinking_blocks: self.anthropic_thinking_blocks,
            origin: MessageOrigin::User,
            tool_calls: (!self.calls.is_empty()).then_some(self.calls),
            tool_call_id: None,
            name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_parses_cached_and_reasoning_details() {
        let u: Usage = serde_json::from_str(
            r#"{"prompt_tokens":1200,"completion_tokens":400,
                "prompt_tokens_details":{"cached_tokens":900},
                "completion_tokens_details":{"reasoning_tokens":120}}"#,
        )
        .unwrap();
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.cached_tokens(), Some(900));
        assert_eq!(u.reasoning_tokens(), Some(120));
        // Absent details → None (not zero), so we don't render a bogus "0 cached".
        let plain: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":5}"#).unwrap();
        assert_eq!(plain.cached_tokens(), None);
        assert_eq!(plain.reasoning_tokens(), None);
    }

    #[test]
    fn chat_chunk_tolerates_explicit_null_choices_and_null_delta() {
        // Some OpenAI-compatible proxies emit an explicit `null` instead of
        // omitting the field entirely — plain `#[serde(default)]` doesn't cover
        // that case (only a missing key), so without `null_as_default` this
        // would be a terminal deserialization error instead of a no-op chunk.
        let c: ChatChunk = serde_json::from_str(r#"{"choices": null}"#).unwrap();
        assert!(c.choices.is_empty());

        let c2: ChatChunk =
            serde_json::from_str(r#"{"choices": [{"delta": null, "finish_reason": "stop"}]}"#)
                .unwrap();
        assert_eq!(c2.choices.len(), 1);
        assert!(c2.choices[0].delta.content.is_none());
        assert_eq!(c2.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn accumulator_captures_finish_reason_and_truncation() {
        let mut acc = Accumulator::new();
        assert!(!acc.truncated());
        // A `length` finish_reason flags truncation.
        acc.push(&ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta::default(),
                finish_reason: Some("length".into()),
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
        });
        assert_eq!(acc.finish_reason.as_deref(), Some("length"));
        assert!(acc.truncated());
        // A normal `stop` does not.
        let mut acc2 = Accumulator::new();
        acc2.push(&ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta::default(),
                finish_reason: Some("stop".into()),
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
        });
        assert!(!acc2.truncated());
    }

    #[test]
    fn cache_breakpoints_mark_system_and_last_only() {
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "u1" },
                { "role": "assistant", "content": "a1" },
                { "role": "user", "content": "u2" },
            ]
        });
        apply_cache_breakpoints(&mut body, false);
        let msgs = body["messages"].as_array().unwrap();
        // System marked: content became a one-element parts array with the marker.
        assert_eq!(msgs[0]["content"][0]["text"], "sys");
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        // Middle messages left as plain strings.
        assert_eq!(msgs[1]["content"], "u1");
        assert_eq!(msgs[2]["content"], "a1");
        // Last marked.
        assert_eq!(msgs[3]["content"][0]["text"], "u2");
        assert_eq!(msgs[3]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_breakpoints_single_message_marked_once() {
        let mut body = json!({ "messages": [{ "role": "system", "content": "only" }] });
        apply_cache_breakpoints(&mut body, false);
        let c = &body["messages"][0]["content"];
        assert_eq!(c.as_array().unwrap().len(), 1);
        assert_eq!(c[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_breakpoints_skip_contentless_last_message() {
        // A tool-call-only assistant turn (no `content`) can't be marked; the
        // system breakpoint still applies.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "assistant", "tool_calls": [{ "id": "1" }] },
            ]
        });
        apply_cache_breakpoints(&mut body, false);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body["messages"][1].get("content").is_none());
    }

    #[test]
    fn cache_breakpoints_noop_without_messages() {
        let mut body = json!({ "model": "x" });
        apply_cache_breakpoints(&mut body, false);
        assert!(body.get("messages").is_none());
    }

    #[test]
    fn cache_control_carries_1h_ttl_when_requested() {
        assert_eq!(cache_control(false), json!({ "type": "ephemeral" }));
        assert_eq!(
            cache_control(true),
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
        let mut body = json!({ "messages": [{ "role": "system", "content": "s" }] });
        apply_cache_breakpoints(&mut body, true);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    /// Build a minimal ChatChunk with optional text content and tool-call deltas.
    fn chunk(content: Option<&str>, tool_calls: Option<Vec<ToolCallDelta>>) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    content: content.map(|s| s.to_string()),
                    reasoning_content: None,
                    tool_calls,
                },
                finish_reason: None,
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
        }
    }

    #[test]
    fn reasoning_content_is_never_serialized_but_still_parses() {
        // The accumulator carries the model's <think> into the history message…
        let mut acc = Accumulator::new();
        acc.reasoning = "the user said hi, greet back".to_string();
        acc.content = "Hello!".to_string();
        let msg = acc.into_message();
        assert_eq!(
            msg.reasoning_content.as_deref(),
            Some("the user said hi, greet back")
        );

        // …but it must NOT go back on the wire — reasoning models degrade when a
        // prior turn's reasoning is fed back into the prompt.
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("reasoning_content"),
            "reasoning_content leaked onto the wire: {json}"
        );
        assert!(json.contains("Hello!"));

        // Deserialization still accepts it (non-streaming / compact responses).
        let parsed: ChatMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"hi","reasoning_content":"x"}"#)
                .unwrap();
        assert_eq!(parsed.reasoning_content.as_deref(), Some("x"));
    }

    #[test]
    fn accumulator_reassembles_text_across_chunks() {
        let mut acc = Accumulator::new();
        assert_eq!(acc.push(&chunk(Some("hel"), None)), Some("hel".to_string()));
        assert_eq!(acc.push(&chunk(Some("lo"), None)), Some("lo".to_string()));
        assert_eq!(acc.push(&chunk(None, None)), None);
        let msg = acc.into_message();
        assert_eq!(msg.content, Some("hello".to_string()));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn accumulator_reassembles_fragmented_tool_call_arguments() {
        let mut acc = Accumulator::new();

        // First chunk: id + start of name + start of arguments.
        acc.push(&chunk(
            None,
            Some(vec![ToolCallDelta {
                index: 0,
                id: Some("call_abc".to_string()),
                function: Some(FunctionDelta {
                    name: Some("re".to_string()),
                    arguments: Some("{\"pa".to_string()),
                }),
            }]),
        ));

        // Second chunk: rest of name + rest of arguments.
        acc.push(&chunk(
            None,
            Some(vec![ToolCallDelta {
                index: 0,
                id: None,
                function: Some(FunctionDelta {
                    name: Some("ad".to_string()),
                    arguments: Some("th\": \"x\"}".to_string()),
                }),
            }]),
        ));

        let msg = acc.into_message();
        assert!(msg.content.is_none());
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, "{\"path\": \"x\"}");
    }

    #[test]
    fn accumulator_handles_multiple_tool_calls_by_index() {
        let mut acc = Accumulator::new();

        acc.push(&chunk(
            None,
            Some(vec![
                ToolCallDelta {
                    index: 0,
                    id: Some("id0".to_string()),
                    function: Some(FunctionDelta {
                        name: Some("tool_a".to_string()),
                        arguments: Some("{}".to_string()),
                    }),
                },
                ToolCallDelta {
                    index: 1,
                    id: Some("id1".to_string()),
                    function: Some(FunctionDelta {
                        name: Some("tool_b".to_string()),
                        arguments: Some("{\"k\":".to_string()),
                    }),
                },
            ]),
        ));
        acc.push(&chunk(
            None,
            Some(vec![ToolCallDelta {
                index: 1,
                id: None,
                function: Some(FunctionDelta {
                    name: None,
                    arguments: Some("\"v\"}".to_string()),
                }),
            }]),
        ));

        let msg = acc.into_message();
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "tool_a");
        assert_eq!(calls[0].function.arguments, "{}");
        assert_eq!(calls[1].function.name, "tool_b");
        assert_eq!(calls[1].function.arguments, "{\"k\":\"v\"}");
    }

    #[test]
    fn accumulator_empty_produces_no_content_no_calls() {
        let acc = Accumulator::new();
        let msg = acc.into_message();
        assert!(msg.content.is_none());
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn tool_calls_without_ids_get_synthesized_distinct_ids() {
        // A server that omits `id` on its tool-call deltas.
        let mut acc = Accumulator::new();
        acc.push(&chunk(
            None,
            Some(vec![
                ToolCallDelta {
                    index: 0,
                    id: None,
                    function: Some(FunctionDelta {
                        name: Some("tool_a".to_string()),
                        arguments: Some("{}".to_string()),
                    }),
                },
                ToolCallDelta {
                    index: 1,
                    id: None,
                    function: Some(FunctionDelta {
                        name: Some("tool_b".to_string()),
                        arguments: Some("{}".to_string()),
                    }),
                },
            ]),
        ));
        let calls = acc.into_message().tool_calls.expect("has tool calls");
        // Synthesized, non-empty, and distinct so results can be correlated.
        assert!(!calls[0].id.is_empty());
        assert!(!calls[1].id.is_empty());
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn synthesized_tool_call_ids_are_unique_across_turns() {
        // Two separate turns (two Accumulators), each with one id-less tool
        // call at index 0. A session replaying both turns to the native
        // Anthropic backend must not send the same `tool_use` id twice —
        // Anthropic rejects duplicate ids. A per-turn index alone (`call_0`)
        // would collide here.
        let make_call_0 = || {
            let mut acc = Accumulator::new();
            acc.push(&chunk(
                None,
                Some(vec![ToolCallDelta {
                    index: 0,
                    id: None,
                    function: Some(FunctionDelta {
                        name: Some("tool_a".to_string()),
                        arguments: Some("{}".to_string()),
                    }),
                }]),
            ));
            acc.into_message().tool_calls.unwrap()[0].id.clone()
        };
        let id_turn1 = make_call_0();
        let id_turn2 = make_call_0();
        assert_ne!(
            id_turn1, id_turn2,
            "the same tool-call index in two different turns must not collide"
        );
    }

    fn usage_chunk(prompt_tokens: u32, completion_tokens: u32) -> ChatChunk {
        ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens,
                ..Default::default()
            }),
            anthropic_thinking_blocks: vec![],
        }
    }

    fn reasoning_chunk(text: &str) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    content: None,
                    reasoning_content: Some(text.to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
        }
    }

    #[test]
    fn accumulator_usage_only_chunk_captured() {
        // A usage-only chunk (empty choices) must store the usage but return None
        // from push (no text delta).
        let mut acc = Accumulator::new();
        let result = acc.push(&usage_chunk(100, 20));
        assert!(result.is_none(), "usage-only chunk should return None");
        let u = acc.usage.as_ref().expect("usage should be stored");
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 20);
    }

    #[test]
    fn accumulator_usage_merge_preserves_all_fields() {
        // Simulate Anthropic's two-phase usage: message_start (prompt+cached),
        // then message_delta (completion only). The merge must keep all three.
        let mut acc = Accumulator::new();
        // First chunk: prompt + cached (message_start shape).
        acc.push(&ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                prompt_tokens_details: TokenDetails {
                    cached_tokens: Some(80),
                    ..Default::default()
                },
                completion_tokens_details: TokenDetails::default(),
            }),
            anthropic_thinking_blocks: vec![],
        });
        // Second chunk: completion only (message_delta shape).
        acc.push(&ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 50,
                ..Default::default()
            }),
            anthropic_thinking_blocks: vec![],
        });
        let u = acc.usage.as_ref().unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.cached_tokens(), Some(80));
    }

    #[test]
    fn accumulator_reasoning_accumulated_across_chunks() {
        // Multi-chunk reasoning_content deltas must concatenate, and no text
        // content should leak into the `content` field.
        let mut acc = Accumulator::new();
        acc.push(&reasoning_chunk("think "));
        acc.push(&reasoning_chunk("harder"));
        let msg = acc.into_message();
        assert_eq!(msg.reasoning_content.as_deref(), Some("think harder"));
        assert!(
            msg.content.is_none(),
            "no content expected when only reasoning came in"
        );
    }

    #[test]
    fn accumulator_content_and_tool_calls_same_turn() {
        // A model turn that emits text AND requests a tool call in the same chunk.
        let mut acc = Accumulator::new();
        acc.push(&chunk(
            Some("searching..."),
            Some(vec![ToolCallDelta {
                index: 0,
                id: Some("call_x".to_string()),
                function: Some(FunctionDelta {
                    name: Some("grep".to_string()),
                    arguments: Some("{\"pattern\":\"foo\"}".to_string()),
                }),
            }]),
        ));
        let msg = acc.into_message();
        assert_eq!(msg.content.as_deref(), Some("searching..."));
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "grep");
    }

    #[test]
    fn chat_request_tools_omitted_when_empty() {
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            temperature: Some(0.5),
            reasoning_effort: None,
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: false,
            stream_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("\"tools\""),
            "tools should be omitted when empty: {json}"
        );
    }

    #[test]
    fn chat_request_temperature_omitted_when_none() {
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            temperature: None,
            reasoning_effort: None,
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: false,
            stream_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("\"temperature\""),
            "temperature should be omitted when None: {json}"
        );
    }

    #[test]
    fn reasoning_effort_normalizes_known_levels_only() {
        assert_eq!(normalize_effort("High").as_deref(), Some("high"));
        assert_eq!(normalize_effort(" low ").as_deref(), Some("low"));
        assert_eq!(normalize_effort("minimal").as_deref(), Some("minimal"));
        assert_eq!(normalize_effort("off"), None);
        assert_eq!(normalize_effort("turbo"), None);
        assert_eq!(normalize_effort(""), None);
    }

    #[test]
    fn opt_in_params_omitted_by_default_and_sent_when_set() {
        // Defaults: none of the opt-in params appear on the wire.
        let base = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            temperature: None,
            reasoning_effort: None,
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: false,
            stream_options: None,
        };
        let json = serde_json::to_string(&base).unwrap();
        for key in ["max_tokens", "top_p", "seed", "stop"] {
            assert!(!json.contains(key), "{key} should be omitted: {json}");
        }
        // Set: they serialize.
        let set = ChatRequest {
            max_tokens: Some(4096),
            top_p: Some(0.9),
            seed: Some(7),
            stop: vec!["<STOP>".to_string()],
            ..base
        };
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains("\"max_tokens\":4096"), "{json}");
        assert!(json.contains("\"top_p\":0.9"), "{json}");
        assert!(json.contains("\"seed\":7"), "{json}");
        assert!(json.contains("\"stop\":[\"<STOP>\"]"), "{json}");
    }

    #[test]
    fn anthropic_thinking_blocks_never_serialized_onto_openai_wire() {
        // Regression for the `#[serde(default, skip_serializing)]` invariant on
        // `anthropic_thinking_blocks`. These blocks (type/thinking/signature
        // triples) are Anthropic-native; sending them on the OpenAI wire would
        // either cause a 400 from strict providers or be silently ignored — but
        // more dangerously, reasoning models degrade when prior reasoning is fed
        // back verbatim. The field must be completely absent from the serialized
        // JSON output even when non-empty.
        let msg = ChatMessage {
            role: Role::Assistant,
            content: Some("I'll read that file.".into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![serde_json::json!({
                "type": "thinking",
                "thinking": "The user wants me to read a file.",
                "signature": "SIG_ABCDEF"
            })],
            origin: MessageOrigin::User,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };

        let json = serde_json::to_string(&msg).unwrap();

        // The key must be completely absent — not null, not [], just missing.
        assert!(
            !json.contains("anthropic_thinking_blocks"),
            "anthropic_thinking_blocks must not appear on the OpenAI wire: {json}"
        );
        // The text content must still be present.
        assert!(
            json.contains("I'll read that file."),
            "content must survive serialization: {json}"
        );

        // Deserialization round-trip: if a JSON blob arrived with the field
        // (e.g. from a compact non-streaming response), it must be accepted and
        // stored for display — but then dropped on the next outbound serialization.
        let parsed: ChatMessage = serde_json::from_str(
            r#"{
                "role": "assistant",
                "content": "hi",
                "anthropic_thinking_blocks": [{"type":"thinking","thinking":"x","signature":"S"}]
            }"#,
        )
        .unwrap();
        assert_eq!(
            parsed.anthropic_thinking_blocks.len(),
            1,
            "deserialization must accept and store the field"
        );
        // Re-serialize: blocks still dropped.
        let re_json = serde_json::to_string(&parsed).unwrap();
        assert!(
            !re_json.contains("anthropic_thinking_blocks"),
            "blocks must be dropped even after a round-trip: {re_json}"
        );
    }

    #[test]
    fn chat_request_reasoning_effort_serialized_when_set() {
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            temperature: None,
            reasoning_effort: Some("high".to_string()),
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: false,
            stream_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains("\"reasoning_effort\":\"high\""),
            "reasoning_effort should serialize: {json}"
        );
    }
}
