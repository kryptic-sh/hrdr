//! OpenAI chat-completions wire types — the subset hrdr speaks.
//!
//! hrdr only ever sends structured `messages[]` + `tools[]`; the server
//! (e.g. `infr`) owns chat-template application and tool-call parsing. We do
//! not render model prompt formats here.

use serde::{Deserialize, Deserializer, Serialize};

use crate::client::{ChatError, ChatErrorKind};

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
/// user-role context the agent injected (tool products, turn-end nudges,
/// compaction summaries).
///
/// One variant per genuine kind of `Role::User` message. Only [`User`] is the
/// user speaking; the rest are the harness talking to itself, and compaction
/// counts turn boundaries on that distinction.
///
/// [`User`] must stay `#[default]`: the session file writes `origin` only when
/// it differs from the default (see `persisted_messages` in `hrdr-agent`), so a
/// message with the field omitted loads back as `User`.
///
/// **Never serialized onto the provider wire** — only the session file preserves
/// it.
///
/// [`User`]: MessageOrigin::User
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MessageOrigin {
    /// The user speaking — opening a turn or steering mid-turn. A steer is the
    /// user talking either way, so both are real turns; and a steer arriving
    /// from a main agent to its sub-agent is no different, because the main
    /// agent acts on the user's behalf.
    #[default]
    User,
    /// A synthetic prompt the harness injects when the model ends its turn
    /// with no tool calls while the shared TODO list still has unfinished
    /// items — never a real user turn. See `Agent::run`'s turn loop.
    Nudge,
    /// The product of a tool call, delivered as a `Role::User` message because
    /// it arrives after the round that requested it closed — a detached
    /// sub-agent's report. Never a real user turn.
    Tool,
    /// A compaction summary standing in for the history it replaced, carrying
    /// what triggered the compaction that produced it. Never a real user turn,
    /// and never re-summarized: the next compaction folds its text into the new
    /// summary and emits one that supersedes it.
    ///
    /// The reason rides on the message rather than on a log line so provenance
    /// survives into the transcript and across a session resume — nothing else
    /// can tell a `/compact` the user asked for from an overflow rescue.
    Summary(CompactionReason),
}

/// What triggered a compaction.
///
/// Lives here because [`MessageOrigin::Summary`] carries it onto the summary
/// message and therefore through session serialization; the compaction path
/// that produces it lives in `hrdr-agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionReason {
    /// The user ran `/compact`.
    UserRequested,
    /// Context usage crossed the proactive trigger, before any request failed.
    ContextFilling,
    /// A request was rejected for exceeding the context window, and compaction
    /// is the rescue.
    ContextOverflow,
}

impl CompactionReason {
    /// How the reason reads at the head of a transcript notice.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserRequested => "compacted on request",
            Self::ContextFilling => "context was filling up — compacted",
            Self::ContextOverflow => "context window exceeded — compacted",
        }
    }
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
    /// OpenAI **Responses API** reasoning items (`{"type":"reasoning", "id",
    /// "summary", "encrypted_content"}`), captured verbatim off the stream in
    /// [`crate::codex`] and replayed verbatim in the next request's `input[]`.
    ///
    /// Deliberately *not* merged with `anthropic_thinking_blocks`: they are
    /// different wire objects, and one shared field would let an Anthropic
    /// thinking block reach the Responses request builder (and vice versa),
    /// which each provider rejects. Two fields make that mistake unrepresentable.
    ///
    /// Replaying these does **not** violate the `reasoning_content` rule above:
    /// these are the provider's own opaque, encrypted items, handed straight
    /// back to the provider that minted them — the stateless (`store:false`)
    /// mode of the Responses API is built around exactly this, and the model
    /// would otherwise re-derive its whole plan (and pay output tokens for it)
    /// on every tool round.
    ///
    /// **Never serialized** — same invariant as `anthropic_thinking_blocks`:
    /// these are Responses-wire-only and must not go on the OpenAI
    /// chat-completions wire or the Anthropic wire.
    #[serde(default, skip_serializing)]
    pub responses_reasoning_items: Vec<serde_json::Value>,
    /// Internal origin marker — distinguishes real user turns from synthetic
    /// user-role context injected by the agent (tool products, nudges,
    /// compaction summaries). Defaults to [`MessageOrigin::User`], a real user
    /// turn.
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
            responses_reasoning_items: vec![],
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
            responses_reasoning_items: vec![],
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
    /// Memoized [`Self::parsed_input`] result, filled when the call is
    /// finalized ([`Accumulator::into_message`]) so the Anthropic request
    /// builder never re-parses a historical call's arguments on every round.
    /// Never serialized — `arguments` stays the wire's canonical form.
    #[serde(default, skip_serializing)]
    pub parsed_arguments: Option<serde_json::Value>,
}

impl FunctionCall {
    /// The call's arguments as a JSON value for the Anthropic wire: `{}` for a
    /// no-argument call, the parsed object when `arguments` is valid JSON, or
    /// the raw string preserved verbatim when it is not (a malformed args
    /// string must surface as the model's original intent, never be rewritten).
    ///
    /// Serves the memoized [`Self::parsed_arguments`] when the call was
    /// finalized through [`Accumulator::into_message`]; a call that arrives
    /// with a cold cache (restored, hand-built, or deserialized) parses on
    /// demand — correct, just not cached.
    pub fn parsed_input(&self) -> serde_json::Value {
        if let Some(parsed) = &self.parsed_arguments {
            return parsed.clone();
        }
        if self.arguments.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&self.arguments)
                .unwrap_or_else(|_| serde_json::json!(self.arguments.clone()))
        }
    }
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
    /// The model id, omitted entirely when the caller has none to name.
    ///
    /// Optional because of one endpoint shape hrdr has to work on out of the
    /// box: a local server that serves exactly ONE model, where the id is a
    /// detail of how the server was launched (llama.cpp reports its `id` as the
    /// gguf's file path) and the user has named nothing. llama.cpp ignores the
    /// field; vLLM validates it against its served names and answers a 404 for
    /// anything else, so a placeholder id is strictly worse than no id at all —
    /// vLLM's own `model` is nullable and falls back to the served model.
    /// See [`crate::UNNAMED_MODEL`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
/// first `system` message and the **newest markable** message each get a
/// `cache_control` marker, converting their string `content` into a
/// content-parts array. A supporting provider (e.g. OpenRouter) caches the
/// prefix up to and including each marked block (≤4 breakpoints allowed; we use
/// ≤3), so the stable system+tools prefix and the growing conversation prefix
/// are reused turn to turn. Only call this for endpoints known to accept the
/// marker — see [`CacheMode::Ephemeral`]. No-op when there are no messages, or
/// when no message's `content` is a plain string (all already parts, or
/// tool-call-only assistant turns with no text).
///
/// `system_cache_split` is the byte offset where the assembled system prompt's
/// volatile environment tail begins (see `Agent`'s `system_cache_split`). Given
/// one, the system message is emitted as **two** marked text parts — stable
/// prefix, volatile tail — so a tail change (cwd, date) only invalidates the
/// second. OpenRouter forwards per-part `cache_control` to Anthropic, so this
/// mirrors the native path (`anthropic::split_system_for_cache`); that takes
/// the breakpoint count to ≤3 (prefix + tail + rolling last message), still
/// inside Anthropic's limit of 4.
pub fn apply_cache_breakpoints(
    body: &mut serde_json::Value,
    ttl_1h: bool,
    system_cache_split: Option<usize>,
) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let system = messages
        .iter()
        .position(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"));
    if let Some(i) = system {
        mark_system_cache(&mut messages[i], ttl_1h, system_cache_split);
    }
    // Rolling breakpoint on the newest message that can carry one — skipping the
    // system message we just marked, and skipping anything [`mark_cache`] would
    // no-op on.
    //
    // Marking `messages[last]` unconditionally, as this used to, meant a turn
    // ending in a tool-call-only assistant message (`content: null`) got **no**
    // rolling breakpoint at all: `mark_cache` silently returned, and the next
    // request re-read a cached prefix that stopped at the previous turn instead
    // of just before the tool calls. That is the common shape in an agentic
    // loop, so the rolling breakpoint was missing far more often than not.
    // Walking backward keeps the marker as new as it can be. A `role:"tool"`
    // result is a legal breakpoint position (Anthropic allows `cache_control` on
    // tool definitions, `system` blocks, and message content blocks including
    // tool_use / tool_result), so it counts as markable like any other.
    //
    // Breakpoint budget: Anthropic allows **4** per request; this places at most
    // 3 (system stable prefix + system volatile tail + this rolling one).
    if let Some(i) = (0..messages.len())
        .rev()
        .find(|&i| Some(i) != system && is_markable(&messages[i]))
    {
        mark_cache(&mut messages[i], ttl_1h);
    }
}

/// Whether [`mark_cache`] would actually mark this message — i.e. its `content`
/// is a plain string. Anything else (absent, `null`, or already a content-parts
/// array) is a no-op there, and must not consume the rolling breakpoint.
fn is_markable(msg: &serde_json::Value) -> bool {
    msg.get("content").and_then(|c| c.as_str()).is_some()
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

/// Mark the system message, splitting its text at `at` into a stable prefix and
/// a volatile tail so each carries its own breakpoint.
///
/// Falls back to the single-block [`mark_cache`] — as before — when there is no
/// boundary, when it lands outside the text, or when it is not a char boundary
/// (it always is: it is a sum of section lengths, but slicing on a bad index
/// would panic and a mis-cached prompt is not worth that). Content that isn't a
/// plain string (already parts) is left to `mark_cache`, which no-ops on it.
fn mark_system_cache(msg: &mut serde_json::Value, ttl_1h: bool, at: Option<usize>) {
    let Some(at) = at else {
        return mark_cache(msg, ttl_1h);
    };
    let Some(text) = msg
        .get("content")
        .and_then(|c| c.as_str())
        .map(str::to_owned)
    else {
        return mark_cache(msg, ttl_1h);
    };
    if at == 0 || at >= text.len() || !text.is_char_boundary(at) {
        return mark_cache(msg, ttl_1h);
    }
    msg["content"] = serde_json::json!([
        {
            "type": "text",
            "text": &text[..at],
            "cache_control": cache_control(ttl_1h),
        },
        {
            "type": "text",
            "text": &text[at..],
            "cache_control": cache_control(ttl_1h),
        },
    ]);
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
    /// `null_as_default`: some backends (e.g. GLM) send the key explicitly as
    /// `null` rather than omitting it — `#[serde(default)]` alone only fills an
    /// *absent* field, so a present `null` would fail to deserialize the struct.
    #[serde(default, deserialize_with = "null_as_default")]
    pub prompt_tokens_details: TokenDetails,
    /// OpenAI-style breakdown of the completion (`reasoning_tokens`). Same
    /// `null_as_default` handling as `prompt_tokens_details` above.
    #[serde(default, deserialize_with = "null_as_default")]
    pub completion_tokens_details: TokenDetails,
    /// Prompt tokens **written into** the provider's prompt cache on this call
    /// (Anthropic's `cache_creation_input_tokens`), when the provider reports
    /// it. Part of `prompt_tokens`, disjoint from
    /// `prompt_tokens_details.cached_tokens` — a token is either read from the
    /// cache or written to it, never both.
    ///
    /// Carried separately because a cache **write** is *more* expensive than
    /// plain input (1.25x at the 5-minute TTL, 2x at the 1-hour one) while a
    /// read is *cheaper* (0.1x); folding writes into the plain-input bucket, as
    /// hrdr did before, silently under-bills every turn — and hrdr writes the
    /// cache on essentially every turn via its rolling breakpoint. See
    /// [`crate::catalog::ModelCost::call_cost`].
    ///
    /// Named for the Anthropic wire field so a proxy that forwards the key
    /// verbatim (rather than remapping it into the OpenAI shape) is picked up
    /// for free. `Option`, not `u32`: absent must stay distinguishable from a
    /// reported zero, so a provider that never sends it can't be mistaken for
    /// one reporting "no cache writes".
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
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

    /// Prompt tokens written into the prompt cache on this call, if the
    /// provider reported it. Priced at a **premium**, not the plain input rate
    /// — see [`cache_creation_input_tokens`](Self::cache_creation_input_tokens).
    pub fn cache_creation_tokens(&self) -> Option<u32> {
        self.cache_creation_input_tokens
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
    /// Complete Responses API reasoning items accumulated during streaming
    /// (emitted as a single synthetic chunk after the byte loop, in stream
    /// order). Only populated on the [`crate::codex`] path; ignored by the
    /// OpenAI path via `#[serde(skip)]`. Never serialized.
    #[serde(skip)]
    pub responses_reasoning_items: Vec<serde_json::Value>,
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
        responses_reasoning_items: vec![],
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
    /// Bytes appended so far across `content`, `reasoning` and the tool-call
    /// fragments. The per-event cap alone does not bound a reply: an endpoint
    /// can stream arbitrarily many small events for the whole request timeout,
    /// so the accumulated total needs its own ceiling (see [`Self::budget`]).
    bytes: usize,
    /// Ceiling on [`Self::bytes`]; the stream errors past it. Defaults to
    /// [`MAX_ACCUMULATED_BYTES`]; a smaller value is test-only.
    budget: usize,
    /// Anthropic thinking blocks (with signature) for re-emission in the native
    /// Messages API request. Never serialized — same invariant as reasoning_content.
    anthropic_thinking_blocks: Vec<serde_json::Value>,
    /// Responses API reasoning items (with their `encrypted_content`) for
    /// replay in the next Responses request's `input[]`. Never serialized —
    /// same invariant as `anthropic_thinking_blocks`.
    responses_reasoning_items: Vec<serde_json::Value>,
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

/// Ceiling on the bytes one [`Accumulator`] may hold before the stream errors.
/// Independent of the per-event cap ([`crate::sse::SseDecoder`]'s), because a
/// hostile endpoint can emit many small complete events for the whole request
/// timeout — without this, memory grows network-bound × 300 s and the inflated
/// message then rides in history for the next request.
const MAX_ACCUMULATED_BYTES: usize = 64 * 1024 * 1024;

impl Accumulator {
    pub fn new() -> Self {
        Self {
            budget: MAX_ACCUMULATED_BYTES,
            nonce: NEXT_ACCUMULATOR_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..Self::default()
        }
    }

    /// An accumulator with a smaller ceiling than the default — tests only, so
    /// the overflow path is reachable without allocating 64 MiB.
    #[cfg(test)]
    pub(crate) fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            nonce: NEXT_ACCUMULATOR_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..Self::default()
        }
    }

    /// Merge one chunk. Returns the freshly-appended text delta (for live
    /// rendering), if any. Errors once the accumulated reply exceeds the byte
    /// budget — a flooding endpoint must not grow memory for the whole request
    /// timeout (mirrors the SSE-overflow handling).
    pub fn push(&mut self, chunk: &ChatChunk) -> Result<Option<String>, ChatError> {
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
                    // Same don't-clobber rule for the cache-write counter, and
                    // for the same reason: Anthropic reports it on
                    // `message_start` only, so the later `message_delta` usage
                    // (completion tokens) carries `None` and must not erase it.
                    if new.cache_creation_input_tokens.is_some() {
                        existing.cache_creation_input_tokens = new.cache_creation_input_tokens;
                    }
                }
            }
        }
        if !chunk.anthropic_thinking_blocks.is_empty() {
            self.anthropic_thinking_blocks
                .extend(chunk.anthropic_thinking_blocks.iter().cloned());
        }
        // Like the thinking blocks above, this must be folded in *before* the
        // `choices.first()?` early return: the synthetic chunk that carries the
        // reasoning items has no choices at all.
        if !chunk.responses_reasoning_items.is_empty() {
            self.responses_reasoning_items
                .extend(chunk.responses_reasoning_items.iter().cloned());
        }
        let Some(choice) = chunk.choices.first() else {
            return Ok(None);
        };
        if let Some(fr) = &choice.finish_reason {
            self.finish_reason = Some(fr.clone());
        }
        if let Some(r) = &choice.delta.reasoning_content {
            self.bytes += r.len();
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
                        parsed_arguments: None,
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
                    self.bytes += name.len();
                    slot.function.name.push_str(name);
                }
                if let Some(args) = &f.arguments {
                    self.bytes += args.len();
                    slot.function.arguments.push_str(args);
                }
            }
        }
        let delta = choice.delta.content.clone();
        if let Some(text) = &delta {
            self.bytes += text.len();
            self.content.push_str(text);
        }
        if self.bytes > self.budget {
            return Err(ChatError {
                status: None,
                retry_after: None,
                kind: ChatErrorKind::Other,
                message: format!(
                    "stream overflow: accumulated response exceeding {} MiB limit; \
                     broken or hostile server",
                    MAX_ACCUMULATED_BYTES / (1024 * 1024)
                ),
            });
        }
        Ok(delta)
    }

    /// A rough token count for the tool calls streamed so far — the same
    /// `len / 4` estimate used for message content, over each call's name and
    /// arguments.
    ///
    /// These are completion tokens the model was billed for and spent time
    /// generating, but they appear in neither `content` nor `reasoning`, so any
    /// estimate built from those alone misses a tool-calling round almost
    /// entirely. Only needed when the server reports no usage of its own.
    pub fn tool_call_tokens(&self) -> u32 {
        let bytes: usize = self
            .calls
            .iter()
            .map(|c| c.function.name.len() + c.function.arguments.len())
            .sum();
        (bytes / 4) as u32
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
            // Arguments are final once the stream ends — memoize the parsed
            // form now so the Anthropic request builder never re-parses this
            // call's arguments on every subsequent round (see
            // [`FunctionCall::parsed_input`]).
            call.function.parsed_arguments = Some(call.function.parsed_input());
        }
        ChatMessage {
            role: Role::Assistant,
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
            anthropic_thinking_blocks: self.anthropic_thinking_blocks,
            responses_reasoning_items: self.responses_reasoning_items,
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
        // The cache-write counter is likewise absent for every provider that
        // doesn't publish one — `None`, never `Some(0)`, so "unreported" can't
        // be mistaken for "wrote nothing".
        assert_eq!(u.cache_creation_tokens(), None);
        assert_eq!(plain.cache_creation_tokens(), None);
        // …and is picked up when a provider does forward the Anthropic key.
        let written: Usage = serde_json::from_str(
            r#"{"prompt_tokens":1000,"completion_tokens":5,"cache_creation_input_tokens":300}"#,
        )
        .unwrap();
        assert_eq!(written.cache_creation_tokens(), Some(300));
        // Some backends (GLM) send the details key EXPLICITLY as `null` rather
        // than omitting it — `#[serde(default)]` alone only covers a missing key,
        // so without `null_as_default` this whole chunk failed to decode.
        let nulled: Usage = serde_json::from_str(
            r#"{"prompt_tokens":18894,"completion_tokens":27,"total_tokens":18921,
                "prompt_tokens_details":null}"#,
        )
        .unwrap();
        assert_eq!(nulled.prompt_tokens, 18894);
        assert_eq!(nulled.cached_tokens(), None);
        assert_eq!(nulled.reasoning_tokens(), None);
    }

    #[test]
    fn chat_chunk_tolerates_glm_terminal_tool_calls_chunk() {
        // The exact terminal chunk GLM-5.2 emits: finish_reason=tool_calls with an
        // empty/null delta and a usage block whose `prompt_tokens_details` is null.
        let data = r#"{"id":"chatcmpl-x","object":"chat.completion.chunk","created":1784680698,
            "model":"glm-5.2","choices":[{"index":0,"finish_reason":"tool_calls","logprobs":null,
            "delta":{"role":"assistant","content":"","reasoning_content":null,"tool_calls":null}}],
            "usage":{"prompt_tokens":18894,"completion_tokens":27,"total_tokens":18921,
            "prompt_tokens_details":null}}"#;
        let c: ChatChunk = serde_json::from_str(data).expect("GLM terminal chunk decodes");
        assert_eq!(c.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(c.usage.unwrap().prompt_tokens, 18894);
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
            responses_reasoning_items: vec![],
        })
        .unwrap();
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
            responses_reasoning_items: vec![],
        })
        .unwrap();
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
        apply_cache_breakpoints(&mut body, false, None);
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
        apply_cache_breakpoints(&mut body, false, None);
        let c = &body["messages"][0]["content"];
        assert_eq!(c.as_array().unwrap().len(), 1);
        assert_eq!(c[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_breakpoints_skip_contentless_last_message() {
        // A tool-call-only assistant turn (no `content`) can't be marked, and
        // with nothing else markable behind it the system breakpoint is all
        // that applies.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "assistant", "tool_calls": [{ "id": "1" }] },
            ]
        });
        apply_cache_breakpoints(&mut body, false, None);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body["messages"][1].get("content").is_none());
    }

    /// The rolling breakpoint walks **backward** to the newest markable message
    /// when the last one can't carry it.
    ///
    /// A tool-call-only assistant turn has `content: null`, so marking
    /// `messages[last]` unconditionally (as this used to) silently placed no
    /// rolling breakpoint at all — and that turn shape is the common one in an
    /// agentic loop, so the request re-read a needlessly short cached prefix on
    /// most rounds.
    #[test]
    fn rolling_breakpoint_walks_back_past_an_unmarkable_last_message() {
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "u1" },
                { "role": "assistant", "content": "a1" },
                // Newest, but unmarkable: tool calls, no text.
                { "role": "assistant", "tool_calls": [{ "id": "1" }] },
            ]
        });
        apply_cache_breakpoints(&mut body, false, None);
        let msgs = body["messages"].as_array().unwrap();
        // Landed on the newest message that *can* carry it, not on an older one
        // and not nowhere.
        assert_eq!(msgs[2]["content"][0]["text"], "a1");
        assert_eq!(msgs[2]["content"][0]["cache_control"]["type"], "ephemeral");
        // Everything else untouched.
        assert_eq!(msgs[1]["content"], "u1");
        assert!(msgs[3].get("content").is_none());
    }

    /// A `role:"tool"` result is a legal breakpoint position (Anthropic allows
    /// `cache_control` on tool_use / tool_result content blocks), so it is
    /// markable like any other message and is not walked past.
    #[test]
    fn rolling_breakpoint_may_land_on_a_tool_result() {
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "assistant", "tool_calls": [{ "id": "1" }] },
                { "role": "tool", "tool_call_id": "1", "content": "result" },
            ]
        });
        apply_cache_breakpoints(&mut body, false, None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["content"][0]["text"], "result");
        assert_eq!(msgs[2]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    /// Nothing markable → still a no-op, and the system message is never taken
    /// as the rolling target twice.
    #[test]
    fn rolling_breakpoint_noop_when_nothing_is_markable() {
        let mut body = json!({
            "messages": [
                { "role": "assistant", "tool_calls": [{ "id": "1" }] },
                { "role": "assistant", "content": null },
            ]
        });
        apply_cache_breakpoints(&mut body, false, None);
        for m in body["messages"].as_array().unwrap() {
            assert!(m["content"].as_array().is_none(), "{m}");
        }
    }

    /// The breakpoint budget: Anthropic allows **4** `cache_control` markers per
    /// request and this function must never exceed that. Worst case is the split
    /// system prompt (2) plus the rolling one (3) — counted over the whole body
    /// so an extra marker anywhere would trip it.
    #[test]
    fn cache_breakpoints_stay_within_the_four_allowed() {
        fn count(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Object(o) => o
                    .iter()
                    .map(|(k, v)| usize::from(k == "cache_control") + count(v))
                    .sum(),
                serde_json::Value::Array(a) => a.iter().map(count).sum(),
                _ => 0,
            }
        }
        let sys = "stable|volatile";
        let at = sys.find('|');
        for split in [None, at] {
            for msgs in [
                json!([{ "role": "system", "content": sys }]),
                json!([
                    { "role": "system", "content": sys },
                    { "role": "user", "content": "u1" },
                    { "role": "assistant", "content": "a1" },
                    { "role": "assistant", "tool_calls": [{ "id": "1" }] },
                    { "role": "tool", "tool_call_id": "1", "content": "r" },
                ]),
            ] {
                let mut body = json!({ "messages": msgs });
                apply_cache_breakpoints(&mut body, false, split);
                let n = count(&body);
                assert!(n <= 4, "{n} breakpoints, split={split:?}: {body}");
                assert!(n >= 1, "at least the rolling one must land: {body}");
            }
        }
    }

    #[test]
    fn cache_breakpoints_noop_without_messages() {
        let mut body = json!({ "model": "x" });
        apply_cache_breakpoints(&mut body, false, None);
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
        apply_cache_breakpoints(&mut body, true, None);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    #[test]
    fn cache_breakpoints_split_system_at_offset() {
        let sys = "stable prefix|volatile tail";
        let at = sys.find('|').unwrap();
        let mut body = json!({
            "messages": [
                { "role": "system", "content": sys },
                { "role": "user", "content": "u1" },
            ]
        });
        apply_cache_breakpoints(&mut body, false, Some(at));
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(parts[1]["cache_control"]["type"], "ephemeral");
        // The two halves reassemble the original system prompt exactly.
        let joined = format!(
            "{}{}",
            parts[0]["text"].as_str().unwrap(),
            parts[1]["text"].as_str().unwrap()
        );
        assert_eq!(joined, sys);
        // Rolling marker on the last message still applies (≤3 breakpoints).
        assert_eq!(body["messages"][1]["content"][0]["text"], "u1");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn cache_breakpoints_split_honors_1h_ttl() {
        let mut body = json!({ "messages": [{ "role": "system", "content": "ab" }] });
        apply_cache_breakpoints(&mut body, true, Some(1));
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["cache_control"]["ttl"], "1h");
        assert_eq!(parts[1]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn cache_breakpoints_bad_split_falls_back_to_one_block() {
        // "héllo" — byte 2 lands inside the two-byte 'é'.
        let sys = "héllo";
        for at in [None, Some(0), Some(sys.len()), Some(sys.len() + 9), Some(2)] {
            let mut body = json!({ "messages": [{ "role": "system", "content": sys }] });
            apply_cache_breakpoints(&mut body, false, at);
            let parts = body["messages"][0]["content"].as_array().unwrap();
            assert_eq!(parts.len(), 1, "offset {at:?} should not split");
            assert_eq!(parts[0]["text"], sys);
            assert_eq!(parts[0]["cache_control"]["type"], "ephemeral");
        }
    }

    #[test]
    fn cache_breakpoints_split_skips_non_string_system_content() {
        // Already parts: left exactly as-is, split offset or not.
        let mut body = json!({
            "messages": [{ "role": "system", "content": [{ "type": "text", "text": "sys" }] }]
        });
        apply_cache_breakpoints(&mut body, false, Some(1));
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "sys");
        assert!(parts[0].get("cache_control").is_none());
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
            responses_reasoning_items: vec![],
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

        // The same holds through the serialized request body, for every host:
        // `ChatRequest` serializes its messages straight, so no endpoint can
        // see the field from the struct alone. (DeepSeek is the one exception
        // to that, and it is grafted later, host-gated, in `Client::body_json`
        // — client.rs — never here.)
        let request = ChatRequest {
            model: Some("deepseek-v4-pro".to_string()),
            messages: vec![msg.clone(), ChatMessage::user("continue")],
            tools: vec![],
            temperature: None,
            reasoning_effort: Some("high".to_string()),
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: true,
            stream_options: None,
        };
        let body = serde_json::to_value(&request).unwrap();
        assert!(
            !body.to_string().contains("reasoning_content"),
            "reasoning_content leaked into the request body: {body}"
        );
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"], "Hello!");

        // Deserialization still accepts it (non-streaming / compact responses).
        let parsed: ChatMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"hi","reasoning_content":"x"}"#)
                .unwrap();
        assert_eq!(parsed.reasoning_content.as_deref(), Some("x"));
    }

    #[test]
    fn accumulator_reassembles_text_across_chunks() {
        let mut acc = Accumulator::new();
        assert_eq!(
            acc.push(&chunk(Some("hel"), None)).unwrap(),
            Some("hel".to_string())
        );
        assert_eq!(
            acc.push(&chunk(Some("lo"), None)).unwrap(),
            Some("lo".to_string())
        );
        assert_eq!(acc.push(&chunk(None, None)).unwrap(), None);
        let msg = acc.into_message();
        assert_eq!(msg.content, Some("hello".to_string()));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn accumulator_errors_past_the_byte_budget() {
        // Content past the ceiling errors the stream (terminal, like the
        // SSE-overflow path) instead of growing memory for the request timeout.
        let mut acc = Accumulator::with_budget(8);
        assert_eq!(
            acc.push(&chunk(Some("hello"), None)).unwrap(),
            Some("hello".to_string())
        );
        let err = acc.push(&chunk(Some("world"), None)).unwrap_err();
        assert_eq!(err.kind, ChatErrorKind::Other);
        assert!(err.message.contains("accumulated response exceeding"));
        // Reasoning and tool-call fragments count against the same budget.
        let mut acc = Accumulator::with_budget(4);
        assert!(acc.push(&reasoning_chunk("think")).is_err());
        let mut acc = Accumulator::with_budget(3);
        assert!(
            acc.push(&chunk(
                None,
                Some(vec![ToolCallDelta {
                    index: 0,
                    id: None,
                    function: Some(FunctionDelta {
                        name: Some("read".to_string()),
                        arguments: None,
                    }),
                }]),
            ))
            .is_err()
        );
        // Under the budget, a large-ish reply still lands whole.
        let mut acc = Accumulator::with_budget(16);
        let text = "a".repeat(16);
        assert_eq!(
            acc.push(&chunk(Some(text.as_str()), None)).unwrap(),
            Some(text.clone())
        );
        assert_eq!(acc.content, text);
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
        ))
        .unwrap();

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
        ))
        .unwrap();

        let msg = acc.into_message();
        assert!(msg.content.is_none());
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, "{\"path\": \"x\"}");
        // `into_message` finalizes the call, so the parsed form is memoized —
        // the Anthropic request builder must never re-parse it per round.
        assert_eq!(
            calls[0].function.parsed_arguments,
            Some(json!({ "path": "x" }))
        );
    }

    #[test]
    fn parsed_input_covers_cold_cache_and_edge_shapes() {
        // A call that never went through `into_message` (restored, hand-built,
        // or deserialized) has a cold cache and parses on demand — the same
        // shapes the request builder used to handle inline.
        let valid = FunctionCall {
            name: "read".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            parsed_arguments: None,
        };
        assert_eq!(valid.parsed_input(), json!({ "path": "a.rs" }));
        assert!(
            valid.parsed_arguments.is_none(),
            "a cold cache parses on demand without mutating the call"
        );
        assert_eq!(
            valid.parsed_input(),
            json!({ "path": "a.rs" }),
            "repeated calls agree"
        );

        // No-argument call: empty args is a no-arg call, not lost intent.
        let no_args = FunctionCall {
            name: "read".into(),
            arguments: "  ".into(),
            parsed_arguments: None,
        };
        assert_eq!(no_args.parsed_input(), json!({}));

        // Malformed JSON is preserved as a JSON string, never rewritten.
        let malformed = FunctionCall {
            name: "read".into(),
            arguments: "not valid json".into(),
            parsed_arguments: None,
        };
        assert_eq!(malformed.parsed_input(), json!("not valid json"));
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
        ))
        .unwrap();
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
        ))
        .unwrap();

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
        ))
        .unwrap();
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
            ))
            .unwrap();
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
            responses_reasoning_items: vec![],
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
            responses_reasoning_items: vec![],
        }
    }

    #[test]
    fn accumulator_usage_only_chunk_captured() {
        // A usage-only chunk (empty choices) must store the usage but return None
        // from push (no text delta).
        let mut acc = Accumulator::new();
        let result = acc.push(&usage_chunk(100, 20)).unwrap();
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
                cache_creation_input_tokens: None,
            }),
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        })
        .unwrap();
        // Second chunk: completion only (message_delta shape).
        acc.push(&ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 50,
                ..Default::default()
            }),
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        })
        .unwrap();
        let u = acc.usage.as_ref().unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.cached_tokens(), Some(80));
    }

    /// The cache-**write** counter survives the same two-event merge. Anthropic
    /// reports it on `message_start` only; the later `message_delta` usage
    /// (completion tokens) carries `None` and must not erase it, or every
    /// Anthropic call would be priced as if it wrote nothing to the cache.
    #[test]
    fn accumulator_usage_merge_keeps_cache_creation_across_events() {
        let start = ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 1_000,
                prompt_tokens_details: TokenDetails {
                    cached_tokens: Some(600),
                    ..Default::default()
                },
                cache_creation_input_tokens: Some(300),
                ..Default::default()
            }),
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        };
        let delta = ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                completion_tokens: 50,
                ..Default::default()
            }),
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
        };
        // Order-independent: the merge takes a max / refuses to clobber a
        // `Some`, so neither emission order can lose a counter.
        for order in [[&start, &delta], [&delta, &start]] {
            let mut acc = Accumulator::new();
            for c in order {
                acc.push(c).unwrap();
            }
            let u = acc.usage.as_ref().unwrap();
            assert_eq!(u.prompt_tokens, 1_000);
            assert_eq!(u.completion_tokens, 50);
            assert_eq!(u.cached_tokens(), Some(600));
            assert_eq!(u.cache_creation_tokens(), Some(300));
        }
    }

    #[test]
    fn accumulator_reasoning_accumulated_across_chunks() {
        // Multi-chunk reasoning_content deltas must concatenate, and no text
        // content should leak into the `content` field.
        let mut acc = Accumulator::new();
        acc.push(&reasoning_chunk("think ")).unwrap();
        acc.push(&reasoning_chunk("harder")).unwrap();
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
        ))
        .unwrap();
        let msg = acc.into_message();
        assert_eq!(msg.content.as_deref(), Some("searching..."));
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "grep");
    }

    #[test]
    fn chat_request_tools_omitted_when_empty() {
        let req = ChatRequest {
            model: Some("m".to_string()),
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
            model: Some("m".to_string()),
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
            model: Some("m".to_string()),
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
            responses_reasoning_items: vec![],
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

    /// Regression for the `#[serde(default, skip_serializing)]` invariant on
    /// `responses_reasoning_items`. These are OpenAI **Responses API** items,
    /// meaningful only to the endpoint that minted them; on the
    /// chat-completions wire (or the Anthropic wire) they are at best ignored
    /// and at worst a 400. `ChatRequest` serializes `Vec<ChatMessage>` straight
    /// onto that wire, so the invariant has to hold on the message type itself.
    #[test]
    fn responses_reasoning_items_never_serialized_onto_openai_wire() {
        let item = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "ENC_SECRET",
        });
        let msg = ChatMessage {
            role: Role::Assistant,
            content: Some("I'll read that file.".into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![item.clone()],
            origin: MessageOrigin::User,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };

        // Not just the key — the encrypted payload must not leak either.
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("responses_reasoning_items") && !json.contains("ENC_SECRET"),
            "responses_reasoning_items must not appear on the OpenAI wire: {json}"
        );
        assert!(json.contains("I'll read that file."), "{json}");

        // Nor via a whole `ChatRequest`, which is how they actually reach the wire.
        let req = ChatRequest {
            model: Some("m".to_string()),
            messages: vec![msg],
            tools: vec![],
            temperature: None,
            reasoning_effort: None,
            max_tokens: None,
            max_completion_tokens: None,
            top_p: None,
            seed: None,
            stop: vec![],
            stream: true,
            stream_options: None,
        };
        let req_json = serde_json::to_string(&req).unwrap();
        assert!(
            !req_json.contains("responses_reasoning_items") && !req_json.contains("ENC_SECRET"),
            "ChatRequest must not carry Responses reasoning items: {req_json}"
        );

        // `#[serde(default)]` keeps the decode side working: a session file that
        // stored the items must load them back (see `persisted_messages` in
        // hrdr-agent), and re-serializing must still drop them.
        let parsed: ChatMessage = serde_json::from_str(
            r#"{
                "role": "assistant",
                "content": "hi",
                "responses_reasoning_items": [
                    {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC_SECRET"}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.responses_reasoning_items, vec![item]);
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("ENC_SECRET")
        );

        // Absent key → empty, not a deserialization error.
        let bare: ChatMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
        assert!(bare.responses_reasoning_items.is_empty());
    }

    #[test]
    fn chat_request_reasoning_effort_serialized_when_set() {
        let req = ChatRequest {
            model: Some("m".to_string()),
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
