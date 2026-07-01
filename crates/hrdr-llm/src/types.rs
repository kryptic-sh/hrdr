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
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// Streaming options. `include_usage` asks the server to emit a final chunk
/// carrying token counts (OpenAI / llama-server support this).
#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Non-streaming response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
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
    pub role: Option<String>,
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
    pub finish_reason: Option<String>,
    /// Token usage from the final `include_usage` chunk, if the server sent it.
    pub usage: Option<Usage>,
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
        if let Some(reason) = &choice.finish_reason {
            self.finish_reason = Some(reason.clone());
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

    /// Whether the model asked to call at least one tool.
    pub fn has_tool_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    /// Assemble the final assistant message.
    pub fn into_message(self) -> ChatMessage {
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

    /// Build a minimal ChatChunk with optional text content and tool-call deltas.
    fn chunk(content: Option<&str>, tool_calls: Option<Vec<ToolCallDelta>>) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    role: None,
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
                    name: Some("read_fi".to_string()),
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
                    name: Some("le".to_string()),
                    arguments: Some("th\": \"x\"}".to_string()),
                }),
            }]),
        ));

        let msg = acc.into_message();
        assert!(msg.content.is_none());
        let calls = msg.tool_calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read_file");
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
}
