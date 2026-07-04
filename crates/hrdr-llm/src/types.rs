//! OpenAI chat-completions wire types — the subset hrdr speaks.
//!
//! hrdr only ever sends structured `messages[]` + `tools[]`; the server
//! (e.g. `infr`) owns chat-template application and tool-call parsing. We do
//! not render model prompt formats here.

use serde::{Deserialize, Serialize};

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
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
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// Normalize a reasoning-effort label to a value worth sending as
/// `reasoning_effort`, or `None` for anything unrecognized (a display-only label
/// like `off`, or garbage) so it's never put on the wire.
pub fn normalize_effort(label: &str) -> Option<String> {
    match label.trim().to_ascii_lowercase().as_str() {
        s @ ("minimal" | "low" | "medium" | "high") => Some(s.to_string()),
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
pub fn apply_cache_breakpoints(body: &mut serde_json::Value) {
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
        mark_cache(&mut messages[i]);
    }
    // Rolling breakpoint on the last message (unless it's the system we marked).
    if Some(last) != system {
        mark_cache(&mut messages[last]);
    }
}

/// Rewrite a message's string `content` into `[{type:text, text, cache_control}]`.
fn mark_cache(msg: &mut serde_json::Value) {
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
        "cache_control": { "type": "ephemeral" },
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

/// One `chat.completion.chunk` SSE event. The final chunk (when `include_usage`
/// is set) carries `usage` with empty `choices`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
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
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge one chunk. Returns the freshly-appended text delta (for live
    /// rendering), if any.
    pub fn push(&mut self, chunk: &ChatChunk) -> Option<String> {
        // The usage chunk arrives with empty `choices`, so capture it before
        // the early return below.
        if chunk.usage.is_some() {
            self.usage = chunk.usage.clone();
        }
        let choice = chunk.choices.first()?;
        if let Some(fr) = &choice.finish_reason {
            self.finish_reason = Some(fr.clone());
        }
        if let Some(r) = &choice.delta.reasoning_content {
            self.reasoning.push_str(r);
        }
        for tc in choice.delta.tool_calls.iter().flatten() {
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
        // multiple calls in one turn don't collide on an empty id.
        for (i, call) in self.calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{i}");
            }
        }
        ChatMessage {
            role: Role::Assistant,
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
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
        apply_cache_breakpoints(&mut body);
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
        apply_cache_breakpoints(&mut body);
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
        apply_cache_breakpoints(&mut body);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body["messages"][1].get("content").is_none());
    }

    #[test]
    fn cache_breakpoints_noop_without_messages() {
        let mut body = json!({ "model": "x" });
        apply_cache_breakpoints(&mut body);
        assert!(body.get("messages").is_none());
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
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[1].id, "call_1");
        assert_ne!(calls[0].id, calls[1].id);
    }

    fn usage_chunk(prompt_tokens: u32, completion_tokens: u32) -> ChatChunk {
        ChatChunk {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens,
                ..Default::default()
            }),
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
    fn chat_request_reasoning_effort_serialized_when_set() {
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            temperature: None,
            reasoning_effort: Some("high".to_string()),
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
