//! OpenAI **Responses API** backend (`POST {base_url}/responses`).
//!
//! This is the wire the ChatGPT/Codex OAuth token talks to
//! (`https://chatgpt.com/backend-api/codex/responses`). hrdr's internal
//! conversation is OpenAI *chat-completions*-shaped (`role`/`content`/
//! `tool_calls`/`tool_call_id`); the Responses API is a different protocol —
//! `system` hoisted to a top-level `instructions` string, history carried as a
//! flat `input[]` array of typed items (`input_text` / `output_text` /
//! `function_call` / `function_call_output`), tools as flat
//! `{type:"function", name, description, parameters}`, and a streamed event
//! protocol (`response.output_text.delta`, `response.output_item.added`/`.done`,
//! `response.completed`, `response.reasoning*`) rather than
//! `chat.completion.chunk`s.
//!
//! This module translates hrdr's history into the Responses request body and
//! normalizes the Responses event stream back into the OpenAI-shaped
//! [`ChatChunk`] the [`Accumulator`] already understands, so the agent loop and
//! frontends are unchanged — the exact same structure as [`crate::anthropic`].
//!
//! Auth: `Authorization: Bearer <access_token>`, plus (when present as
//! provider-configured extra headers) `ChatGPT-Account-Id: <id>`. `originator:
//! hrdr` is always sent. The OAuth access token arrives as the client's
//! `api_key`; the account id arrives via [`crate::Client::set_headers`] (the
//! existing extra-headers mechanism) — no hrdr-agent dependency is introduced.
//!
//! [`Accumulator`]: crate::Accumulator

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::sse::SseDecoder;
use crate::types::{
    ChatChunk, ChatMessage, ChunkChoice, Delta, FunctionDelta, Role, ToolCallDelta, ToolDef, Usage,
};

/// Build the Responses API request body from hrdr's chat-completions-shaped
/// history.
///
/// - Every `Role::System` message is hoisted into the top-level `instructions`
///   string (joined with blank lines), matching how the Codex endpoint consumes
///   the system prompt.
/// - `Role::User` → `{ role:"user", content:[{type:"input_text", text}] }`.
/// - `Role::Assistant` text → `{ role:"assistant", content:[{type:"output_text",
///   text}] }`; each tool call → `{ type:"function_call", call_id, name,
///   arguments }`.
/// - `Role::Tool` → `{ type:"function_call_output", call_id, output }`.
/// - Tool defs → flat `{ type:"function", name, description, parameters }`.
///
/// `stream:true` and `store:false` are always set. `reasoning.effort` is sent
/// only for a recognized effort level; `max_output_tokens`/`temperature`/`top_p`
/// only when configured. `seed` and `stop` have no Responses equivalent and are
/// intentionally not threaded through.
///
/// Note: `reasoning_content` / `anthropic_thinking_blocks` are never sent back —
/// same invariant as the other backends (reasoning models degrade when prior
/// reasoning is fed back into the prompt), and the Responses API rejects replayed
/// reasoning items without their encrypted state anyway.
pub(crate) fn build_body(
    model: &str,
    effort: Option<&str>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Value {
    let (instructions, input) = split_instructions_and_input(messages);

    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "store": false,
    });

    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }

    if let Some(level) = effort.and_then(crate::normalize_effort) {
        body["reasoning"] = json!({ "effort": level });
    }
    if let Some(n) = max_tokens {
        body["max_output_tokens"] = json!(n);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = top_p {
        body["top_p"] = json!(p);
    }

    if !tools.is_empty() {
        let defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.function.name,
                    "description": t.function.description,
                    "parameters": t.function.parameters,
                })
            })
            .collect();
        body["tools"] = Value::Array(defs);
    }

    body
}

/// Split hrdr history into the top-level `instructions` string (all system
/// messages joined) plus the flat Responses `input[]` array.
fn split_instructions_and_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut instructions: Vec<&str> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for m in messages {
        match m.role {
            Role::System => {
                if let Some(text) = &m.content
                    && !text.is_empty()
                {
                    instructions.push(text);
                }
            }
            Role::User => {
                if let Some(text) = &m.content {
                    input.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }));
                }
            }
            Role::Assistant => {
                if let Some(text) = &m.content
                    && !text.is_empty()
                {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for call in m.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        // Responses `arguments` is a JSON *string*, exactly as
                        // hrdr stores it — pass through verbatim.
                        "arguments": call.function.arguments,
                    }));
                }
            }
            Role::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "output": m.content.clone().unwrap_or_default(),
                }));
            }
        }
    }

    (instructions.join("\n\n"), input)
}

/// Stream a completion from the Responses API, yielding OpenAI-shaped
/// [`ChatChunk`]s.
///
/// Takes slices to avoid cloning the full history on every retry. The request
/// body is serialized before any network I/O, so the borrow does not extend into
/// the returned [`crate::ChatStream`] future. Returns the serialized body (for
/// the wire log) alongside the stream, mirroring [`crate::anthropic::chat_stream`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_stream(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    effort: Option<&str>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    extra_headers: &[(String, String)],
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<(Value, crate::ChatStream)> {
    let body = build_body(
        model,
        effort,
        temperature,
        top_p,
        max_tokens,
        messages,
        tools,
    );
    let mut req = http
        .post(format!("{base_url}/responses"))
        // Codex identifies the client via `originator`; the endpoint expects it.
        .header("originator", "hrdr")
        .json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    // Provider-configured extra headers carry `ChatGPT-Account-Id` (and anything
    // else the integrator sets via `Client::set_headers`).
    for (k, v) in extra_headers {
        req = req.header(k, v);
    }

    let resp = req.send().await.context("chat stream request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = crate::client::retry_after_from_headers(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        let status_u16 = status.as_u16();
        return Err(anyhow::Error::new(crate::client::ChatError {
            status: Some(status_u16),
            retry_after,
            kind: crate::client::classify_status(status_u16),
            message: format!(
                "chat endpoint returned {status}: {text}{}",
                crate::client::retry_after_suffix_from(retry_after)
            ),
        }));
    }

    let stream = async_stream::try_stream! {
        let mut bytes = resp.bytes_stream();
        let mut state = StreamState::default();
        // Responses SSE carries `event:` and `data:` lines; every `data:`
        // payload is a complete JSON object carrying its own `type`, so the
        // `event:` line is redundant and ignored. Splitting on 0x0A is safe for
        // UTF-8 (see SseDecoder docs).
        let mut decoder = SseDecoder::new();
        loop {
            // On EOF, `finish()` flushes a final `data:` line that arrived
            // without a blank-line terminator, so a trailing `response.completed`
            // isn't lost (which would falsely look like a cut stream).
            let (events, at_eof) = match bytes.next().await {
                Some(chunk) => {
                    decoder.push(&chunk.context("reading stream chunk")?);
                    (decoder.drain(), false)
                }
                None => (decoder.finish(), true),
            };
            for sse_ev in events {
                let data = sse_ev.data.trim();
                if data.is_empty() { continue; }
                // The Responses stream has no `[DONE]` sentinel — it terminates
                // with `response.completed`/`.incomplete`/`.failed`.
                let ev: Value = serde_json::from_str(data)
                    .with_context(|| format!("decoding stream event: {data}"))?;
                if let Some(out) = map_event(&mut state, &ev)? {
                    yield out;
                }
            }
            if at_eof { break; }
        }
        // No terminal event (`response.completed`/`.incomplete`) means the stream
        // was cut mid-response. Classify as transient so the retry loop can
        // re-request. (`response.failed`/`error` already surfaced as terminal
        // Err above.)
        if !state.terminal_seen {
            Err(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind: crate::client::ChatErrorKind::Transient,
                message: "incomplete stream: Responses stream ended without \
                          response.completed (partial response, safe to retry)"
                    .to_string(),
            })?;
        }
    };
    Ok((body, Box::pin(stream)))
}

/// Per-stream state threaded through [`map_event`]. Responses keys function
/// calls by an opaque output-item id (`fc_…`); we map each to a flat tool-call
/// index for the [`Accumulator`].
#[derive(Default)]
struct StreamState {
    /// Responses output-item id (`fc_…`) → our flat tool-call index.
    tool_slot: std::collections::HashMap<String, usize>,
    /// Next flat tool-call index to assign.
    next_tool: usize,
    /// Output-item ids that received `function_call_arguments.delta` events, so
    /// `output_item.done` doesn't re-emit the (now-complete) arguments and
    /// double them in the accumulator.
    args_streamed: std::collections::HashSet<String>,
    /// Whether any function call was seen (drives the `tool_calls` finish reason).
    saw_function_call: bool,
    /// Whether a terminal `response.completed`/`.incomplete` arrived (truncation
    /// detection).
    terminal_seen: bool,
}

/// Translate one Responses stream event into a [`ChatChunk`] (or `None` for
/// events with nothing for the accumulator). `response.failed`/`error` return
/// `Err` (terminal, non-retryable), mirroring the OpenAI + Anthropic paths.
fn map_event(state: &mut StreamState, ev: &Value) -> Result<Option<ChatChunk>> {
    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        // Incremental assistant text.
        "response.output_text.delta" => {
            let delta = ev.get("delta").and_then(Value::as_str).unwrap_or("");
            Ok((!delta.is_empty()).then(|| text_chunk(delta.to_string())))
        }
        // Incremental reasoning summary/text (only surfaced when the server
        // streams it; the Codex models may or may not).
        "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_summary.delta" => {
            let delta = ev.get("delta").and_then(Value::as_str).unwrap_or("");
            Ok((!delta.is_empty()).then(|| reasoning_chunk(delta.to_string())))
        }
        // A new output item started. Only function calls matter here — they carry
        // the call id + name up front; arguments arrive via later delta events.
        "response.output_item.added" => {
            let item = ev.get("item");
            if item.and_then(|i| i.get("type")).and_then(Value::as_str) != Some("function_call") {
                return Ok(None);
            }
            let fc_id = item_str(item, "id");
            let call_id = item
                .and_then(|i| i.get("call_id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&fc_id)
                .to_string();
            let name = item_str(item, "name");
            let slot = state.assign_slot(&fc_id);
            state.saw_function_call = true;
            Ok(Some(tool_call_chunk(slot, Some(call_id), Some(name), None)))
        }
        // Streamed function-call argument fragment.
        "response.function_call_arguments.delta" => {
            let fc_id = ev.get("item_id").and_then(Value::as_str).unwrap_or("");
            let frag = ev.get("delta").and_then(Value::as_str).unwrap_or("");
            if fc_id.is_empty() || frag.is_empty() {
                return Ok(None);
            }
            // An unknown item id (no matching `output_item.added`) must not
            // silently default to slot 0 — that would corrupt slot 0's arguments
            // with a stray fragment. Ignore it.
            let Some(&slot) = state.tool_slot.get(fc_id) else {
                return Ok(None);
            };
            state.args_streamed.insert(fc_id.to_string());
            Ok(Some(tool_call_chunk(
                slot,
                None,
                None,
                Some(frag.to_string()),
            )))
        }
        // An output item finished. For a function call, emit the complete
        // arguments — but only when they were NOT already streamed via deltas
        // (else they'd double). If we never saw the item start, allocate a slot
        // and emit id+name+args in one go.
        "response.output_item.done" => {
            let item = ev.get("item");
            if item.and_then(|i| i.get("type")).and_then(Value::as_str) != Some("function_call") {
                return Ok(None);
            }
            let fc_id = item_str(item, "id");
            let args = item
                .and_then(|i| i.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match state.tool_slot.get(&fc_id).copied() {
                Some(slot) => {
                    if state.args_streamed.contains(&fc_id) || args.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(tool_call_chunk(slot, None, None, Some(args))))
                    }
                }
                None => {
                    let call_id = item
                        .and_then(|i| i.get("call_id"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&fc_id)
                        .to_string();
                    let name = item_str(item, "name");
                    let slot = state.assign_slot(&fc_id);
                    state.saw_function_call = true;
                    Ok(Some(tool_call_chunk(
                        slot,
                        Some(call_id),
                        Some(name),
                        (!args.is_empty()).then_some(args),
                    )))
                }
            }
        }
        // Clean finish: carries usage + (optional) incomplete reason.
        "response.completed" | "response.incomplete" => {
            state.terminal_seen = true;
            let response = ev.get("response");
            let usage = map_usage(response.and_then(|r| r.get("usage")));
            let finish = map_finish_reason(response, state.saw_function_call);
            Ok(Some(ChatChunk {
                choices: vec![ChunkChoice {
                    delta: Delta::default(),
                    finish_reason: Some(finish),
                }],
                usage,
                anthropic_thinking_blocks: vec![],
            }))
        }
        // Hard failures — surface as terminal (non-retryable) errors carrying the
        // provider's message, mirroring the mid-stream error handling elsewhere.
        "response.failed" => {
            let err_obj = ev.get("response").and_then(|r| r.get("error"));
            let msg = err_obj
                .and_then(error_message)
                .unwrap_or_else(|| "response failed".to_string());
            let code = err_obj.and_then(|e| e.get("code")).and_then(Value::as_str);
            Err(anyhow::Error::new(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind: classify_codex_error(code),
                message: format!("responses stream failed: {msg}"),
            }))
        }
        "error" => {
            let code = ev.get("code").and_then(Value::as_str);
            let msg = error_message(ev).unwrap_or_else(|| "unknown error".to_string());
            Err(anyhow::Error::new(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind: classify_codex_error(code),
                message: format!("responses stream error: {msg}"),
            }))
        }
        _ => Ok(None), // response.created, .in_progress, output_item.added(non-fn), part events, …
    }
}

impl StreamState {
    /// Return the flat tool index for `fc_id`, assigning a fresh one if unseen.
    fn assign_slot(&mut self, fc_id: &str) -> usize {
        if let Some(&slot) = self.tool_slot.get(fc_id) {
            return slot;
        }
        let slot = self.next_tool;
        self.tool_slot.insert(fc_id.to_string(), slot);
        self.next_tool += 1;
        slot
    }
}

/// Read a string field from a stream item, defaulting to empty.
fn item_str(item: Option<&Value>, key: &str) -> String {
    item.and_then(|i| i.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Build a `{code}: {message}` (or bare message/code) from an error payload,
/// matching the two shapes Responses uses (top-level `error` event and
/// `response.error`).
fn error_message(err: &Value) -> Option<String> {
    let message = err.get("message").and_then(Value::as_str);
    let code = err.get("code").and_then(Value::as_str);
    match (code, message) {
        (Some(c), Some(m)) => Some(format!("{c}: {m}")),
        (_, Some(m)) => Some(m.to_string()),
        (Some(c), None) => Some(c.to_string()),
        (None, None) => None,
    }
}

/// Classify a Codex error code as transient or terminal. Only clearly transient
/// codes (rate limit, server error, timeout) are marked retryable; all others
/// (auth, bad request, etc.) are terminal (`Other`).
fn classify_codex_error(code: Option<&str>) -> crate::client::ChatErrorKind {
    match code {
        Some("rate_limit_exceeded" | "server_error" | "timeout") => {
            crate::client::ChatErrorKind::Transient
        }
        _ => crate::client::ChatErrorKind::Other,
    }
}

/// Map the Responses finish to hrdr's OpenAI `finish_reason` vocabulary.
/// `incomplete_details.reason == "max_output_tokens"` → `length` (so truncation
/// is detected); a plain completion → `tool_calls` when a function call was
/// emitted, else `stop`.
fn map_finish_reason(response: Option<&Value>, saw_function_call: bool) -> String {
    let reason = response
        .and_then(|r| r.get("incomplete_details"))
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => "length".to_string(),
        Some("content_filter") => "content_filter".to_string(),
        _ if saw_function_call => "tool_calls".to_string(),
        _ => "stop".to_string(),
    }
}

/// Map the Responses usage object into hrdr's [`Usage`]. `input_tokens` /
/// `output_tokens` are already inclusive totals; `cached_tokens` and
/// `reasoning_tokens` are surfaced as the standard OpenAI detail subsets.
fn map_usage(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage?;
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).map(|n| n as u32);
    let prompt = field("input_tokens").unwrap_or(0);
    let completion = field("output_tokens").unwrap_or(0);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    if prompt == 0 && completion == 0 && cached.is_none() && reasoning.is_none() {
        return None;
    }
    let mut u = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        ..Default::default()
    };
    u.prompt_tokens_details.cached_tokens = cached;
    u.completion_tokens_details.reasoning_tokens = reasoning;
    Some(u)
}

fn text_chunk(text: String) -> ChatChunk {
    delta_chunk(Delta {
        content: Some(text),
        ..Delta::default()
    })
}

fn reasoning_chunk(text: String) -> ChatChunk {
    delta_chunk(Delta {
        reasoning_content: Some(text),
        ..Delta::default()
    })
}

fn tool_call_chunk(
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

fn delta_chunk(delta: Delta) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            delta,
            finish_reason: None,
        }],
        usage: None,
        anthropic_thinking_blocks: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Accumulator, FunctionCall, MessageOrigin, ToolCall};

    fn sys(t: &str) -> ChatMessage {
        ChatMessage::system(t)
    }
    fn user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
    }

    #[test]
    fn serializes_system_user_toolcall_and_result() {
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: Some("let me check".into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let result = ChatMessage::tool_result("call_1", "file body");
        let tools = vec![ToolDef::function(
            "read",
            "read a file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )];
        let body = build_body(
            "gpt-5.5",
            None,
            None,
            None,
            None,
            &[sys("you are hrdr"), user("go"), assistant, result],
            &tools,
        );

        // System hoisted to the top-level `instructions` string.
        assert_eq!(body["instructions"], "you are hrdr");
        // Streaming + stateless.
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["model"], "gpt-5.5");

        let input = body["input"].as_array().unwrap();
        // user, assistant(output_text), function_call, function_call_output.
        assert_eq!(input.len(), 4);

        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "go");

        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "let me check");

        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "read");
        assert_eq!(input[2]["arguments"], r#"{"path":"a.rs"}"#);

        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "file body");

        // Tools flattened (no nested `function` wrapper).
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["description"], "read a file");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn instructions_omitted_without_system_and_tools_omitted_when_empty() {
        let body = build_body("gpt-5.5", None, None, None, None, &[user("hi")], &[]);
        assert!(body.get("instructions").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn effort_and_generation_params_serialize_when_set() {
        let body = build_body(
            "gpt-5.5",
            Some("high"),
            Some(0.3),
            Some(0.9),
            Some(4096),
            &[user("hi")],
            &[],
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 4096);
        let t = body["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6);
        let p = body["top_p"].as_f64().unwrap();
        assert!((p - 0.9).abs() < 1e-6);
    }

    #[test]
    fn multiple_system_messages_join_into_instructions() {
        let body = build_body(
            "gpt-5.5",
            None,
            None,
            None,
            None,
            &[sys("first"), user("hi"), sys("second")],
            &[],
        );
        assert_eq!(body["instructions"], "first\n\nsecond");
    }

    /// Drive a captured Responses event sequence through `map_event` and fold the
    /// resulting chunks into an `Accumulator`, exactly as `chat_stream` does.
    #[test]
    fn parses_text_then_function_call_then_completed_usage() {
        let events = vec![
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_0", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "item_id": "msg_0", "delta": "lo"}),
            json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_abc", "name": "read"
            }}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"pa"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "th\":\"a.rs\"}"}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_abc", "name": "read",
                "arguments": "{\"path\":\"a.rs\"}"
            }}),
            json!({"type": "response.completed", "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 120,
                    "input_tokens_details": {"cached_tokens": 100},
                    "output_tokens": 30,
                    "output_tokens_details": {"reasoning_tokens": 12}
                }
            }}),
        ];

        let mut state = StreamState::default();
        let mut acc = Accumulator::new();
        for ev in &events {
            if let Some(chunk) = map_event(&mut state, ev).unwrap() {
                acc.push(&chunk);
            }
        }
        assert!(state.terminal_seen);

        // Usage folded in (from `response.completed`).
        let usage = acc.usage.as_ref().expect("usage captured");
        assert_eq!(usage.prompt_tokens, 120);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.cached_tokens(), Some(100));
        assert_eq!(usage.reasoning_tokens(), Some(12));
        assert_eq!(acc.finish_reason.as_deref(), Some("tool_calls"));

        let msg = acc.into_message();
        assert_eq!(msg.content.as_deref(), Some("Hello"));
        let calls = msg.tool_calls.expect("tool call accumulated");
        assert_eq!(calls.len(), 1);
        // The correlation id must be the Responses `call_id`, not the `fc_…`
        // output-item id — the follow-up `function_call_output` keys on call_id.
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read");
        // Arguments assembled once (deltas only — `output_item.done` must not
        // double them).
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn function_call_arguments_only_on_done_are_emitted() {
        // A server that sends the full arguments only on `output_item.done`
        // (no `function_call_arguments.delta`) must still surface them.
        let events = vec![
            json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "id": "fc_9", "call_id": "call_9", "name": "grep"
            }}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": "fc_9", "call_id": "call_9", "name": "grep",
                "arguments": "{\"pattern\":\"foo\"}"
            }}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ];
        let mut state = StreamState::default();
        let mut acc = Accumulator::new();
        for ev in &events {
            if let Some(chunk) = map_event(&mut state, ev).unwrap() {
                acc.push(&chunk);
            }
        }
        let calls = acc.into_message().tool_calls.expect("tool call");
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].function.name, "grep");
        assert_eq!(calls[0].function.arguments, r#"{"pattern":"foo"}"#);
    }

    #[test]
    fn incomplete_max_output_tokens_maps_to_length() {
        let ev = json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 5, "output_tokens": 5}
        }});
        let mut state = StreamState::default();
        let chunk = map_event(&mut state, &ev).unwrap().unwrap();
        assert!(state.terminal_seen);
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn plain_completion_without_tool_calls_maps_to_stop() {
        let ev = json!({"type": "response.completed", "response": {
            "usage": {"input_tokens": 5, "output_tokens": 5}
        }});
        let mut state = StreamState::default();
        let chunk = map_event(&mut state, &ev).unwrap().unwrap();
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn response_failed_and_error_surface_as_err() {
        let mut state = StreamState::default();
        let failed = json!({"type": "response.failed", "response": {
            "error": {"code": "rate_limit_exceeded", "message": "slow down"}
        }});
        let err = map_event(&mut state, &failed).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(
            chat_err.message.contains("rate_limit_exceeded"),
            "{}",
            chat_err.message
        );
        assert!(
            chat_err.message.contains("slow down"),
            "{}",
            chat_err.message
        );

        let top = json!({"type": "error", "code": "server_error", "message": "boom"});
        let err = map_event(&mut state, &top).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(
            chat_err.message.contains("server_error"),
            "{}",
            chat_err.message
        );
        assert!(chat_err.message.contains("boom"), "{}", chat_err.message);
    }

    #[test]
    fn codex_terminal_error_is_not_transient() {
        // Terminal error codes (auth, bad request, etc.) must remain Other,
        // not Transient — a 401-like error is not retryable.
        let mut state = StreamState::default();
        let failed = json!({"type": "response.failed", "response": {
            "error": {"code": "invalid_api_key", "message": "bad key"}
        }});
        let err = map_event(&mut state, &failed).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Other);
        assert!(
            chat_err.message.contains("invalid_api_key"),
            "{}",
            chat_err.message
        );

        let top =
            json!({"type": "error", "code": "invalid_request_error", "message": "bad params"});
        let err = map_event(&mut state, &top).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Other);
        assert!(
            chat_err.message.contains("invalid_request_error"),
            "{}",
            chat_err.message
        );
    }

    #[test]
    fn unknown_item_id_argument_delta_is_ignored() {
        // A `function_call_arguments.delta` for an item that never had an
        // `output_item.added` must be dropped, not routed to slot 0.
        let mut state = StreamState::default();
        let ev = json!({"type": "response.function_call_arguments.delta", "item_id": "fc_ghost", "delta": "{\"x\""});
        assert!(map_event(&mut state, &ev).unwrap().is_none());
    }

    #[test]
    fn reasoning_deltas_map_to_reasoning_channel() {
        let mut state = StreamState::default();
        let ev = json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_0", "delta": "thinking"});
        let chunk = map_event(&mut state, &ev).unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("thinking")
        );
    }
}
