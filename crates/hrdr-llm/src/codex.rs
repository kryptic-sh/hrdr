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
//! Because hrdr runs this endpoint statelessly (`store:false`), it also captures
//! the model's own encrypted `reasoning` output items off the stream and replays
//! them verbatim in the next request — otherwise a reasoning model re-derives
//! its entire plan on every tool round. See [`build_body`] for why that is
//! correct here when feeding back prior reasoning is wrong everywhere else.
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
    ChatChunk, ChatMessage, ChunkChoice, Delta, Role, ToolDef, Usage, reasoning_chunk, text_chunk,
    tool_call_chunk,
};

/// Build the Responses API request body from hrdr's chat-completions-shaped
/// history.
///
/// - Every `Role::System` message is hoisted into the top-level `instructions`
///   string (joined with blank lines), matching how the Codex endpoint consumes
///   the system prompt.
/// - `Role::User` → `{ role:"user", content:[{type:"input_text", text}] }`.
/// - `Role::Assistant` → its captured `responses_reasoning_items` verbatim
///   first, then text → `{ role:"assistant", content:[{type:"output_text",
///   text}] }`, then each tool call → `{ type:"function_call", call_id, name,
///   arguments }`.
/// - `Role::Tool` → `{ type:"function_call_output", call_id, output }`.
/// - Tool defs → flat `{ type:"function", name, description, parameters }`.
///
/// `stream:true` and `store:false` are always set. `reasoning.effort` is sent
/// only for a recognized effort level; `max_output_tokens`/`temperature`/`top_p`
/// only when configured. `seed` and `stop` have no Responses equivalent and are
/// intentionally not threaded through.
///
/// # Why prior reasoning *is* replayed here
///
/// `reasoning_content` and `anthropic_thinking_blocks` are still never sent —
/// the first is a plaintext `<think>` transcript (reasoning models degrade when
/// that is fed back), the second is an Anthropic-native object this endpoint
/// would reject. But [`ChatMessage::responses_reasoning_items`] are the
/// *provider's own* opaque, encrypted reasoning items, replayed verbatim to the
/// very provider that minted them. Under `store:false` the server keeps no
/// state, so this replay is the only way the model can see the plan it already
/// paid to derive; without it a reasoning model re-derives its entire chain of
/// thought on every tool round — a quality *and* an output-token regression.
/// This is what the stateless Responses API is designed for, and what the Codex
/// CLI does.
///
/// The blob is opaque: items go back unmodified, in their original order, and
/// items missing their `encrypted_content` are never stored in the first place
/// (see [`capture_reasoning_item`]) because that is exactly the shape the
/// endpoint rejects.
///
/// Known limitation: the items are bound to the model that produced them. If
/// the conversation switched models mid-flight, the stored items belong to the
/// previous model and the endpoint may reject them. Detecting that would mean
/// tagging every message with its originating model; it is not solved here.
///
/// # `prompt_cache_key`
///
/// The Responses API accepts the same top-level `prompt_cache_key` as
/// chat-completions, and it matters more here than anywhere else in hrdr: this
/// endpoint runs `store:false`, so every round re-sends the whole conversation —
/// instructions, history, replayed reasoning items — and that entire prefix is
/// re-billed unless it hits the prompt cache. OpenAI combines the key with the
/// prefix hash when matching, and **on GPT-5.6 models setting it is mandatory
/// for reliable cache matching**. Passed through verbatim and omitted when
/// `None`; see [`crate::Client::set_prompt_cache_key`] for why the caller must
/// scope it to one conversation (roughly 15 requests per minute per key).
///
/// [`ChatMessage::responses_reasoning_items`]: crate::ChatMessage::responses_reasoning_items
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_body(
    model: &str,
    effort: Option<&str>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    prompt_cache_key: Option<&str>,
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
    if let Some(key) = prompt_cache_key {
        body["prompt_cache_key"] = json!(key);
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
///
/// An assistant turn that reasoned and then called a tool re-enters `input[]`
/// as the model produced it: `{type:"reasoning", …}` items first, then the
/// `output_text` message, then one `function_call` per call (its
/// `function_call_output` arrives with the following `Role::Tool` message). See
/// [`build_body`] for why the reasoning items are replayed at all.
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
                // Reasoning items come first, verbatim and in stream order:
                // that is the order the model emitted them (it reasons, then
                // speaks/calls), and the endpoint validates the encrypted state
                // against the items that follow it. Cloned as-is — never
                // rewritten, reordered, or partially dropped.
                for item in &m.responses_reasoning_items {
                    input.push(item.clone());
                }
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
/// the returned [`crate::ChatStream`] future. Writes its own `request` /
/// `error_response` / `sse` wire-log records (see [`crate::client::log_wire`]),
/// mirroring [`crate::anthropic::chat_stream`].
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
    prompt_cache_key: Option<&str>,
    extra_headers: &[(String, String)],
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<crate::ChatStream> {
    let body = build_body(
        model,
        effort,
        temperature,
        top_p,
        max_tokens,
        prompt_cache_key,
        messages,
        tools,
    );
    let url = format!("{base_url}/responses");
    let mut req = http
        .post(&url)
        // Codex identifies the client via `originator`; the endpoint expects it.
        .header("originator", "hrdr")
        .json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    // Provider-configured extra headers carry `ChatGPT-Account-Id` (and anything
    // else the integrator sets via `Client::set_headers`). Auth-type names are
    // filtered out, so the Bearer above stays the only credential on the request
    // (see `crate::client::apply_extra_headers`).
    req = crate::client::apply_extra_headers(req, extra_headers);

    // Log before the send, not after: the round-trip and the status check below
    // both happen here, so logging afterwards would miss exactly the requests
    // the wire log exists to explain (an expired OAuth token, a rejected item
    // in `input[]`). Only the body goes in — the credential is the `Bearer`
    // header, and `build_body` never sees it.
    crate::client::log_wire("request", || json!({"url": url, "body": body}));
    let resp = req.send().await.context("chat stream request failed")?;
    if !resp.status().is_success() {
        return Err(crate::client::error_from_response(resp).await);
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
                    // Type a mid-body transport error as Transient (safe to
                    // retry); an untyped error would slip past the agent's
                    // retry classifier.
                    let chunk = chunk.map_err(|e| crate::client::ChatError {
                        status: None,
                        retry_after: None,
                        kind: crate::client::ChatErrorKind::Transient,
                        message: format!(
                            "incomplete stream: transport error mid-response \
                             ({e}) (partial response, safe to retry)"
                        ),
                    })?;
                    if decoder.push(&chunk).is_err() {
                        let _ = decoder.drain(); // discard truncated events
                        Err(crate::client::ChatError {
                            status: None,
                            retry_after: None,
                            kind: crate::client::ChatErrorKind::Other,
                            message: "SSE stream overflow: received data exceeding \
                                      32 MiB limit; broken or hostile server"
                                .to_string(),
                        })?;
                    }
                    (decoder.drain(), false)
                }
                None => {
                    // If overflow was flagged during the stream, the final
                    // events may be truncated — never parse them.
                    let events = match decoder.finish() {
                        Ok(events) => events,
                        Err(_) => Err(crate::client::ChatError {
                            status: None,
                            retry_after: None,
                            kind: crate::client::ChatErrorKind::Other,
                            message: "SSE stream overflow: received data exceeding \
                                      32 MiB limit; broken or hostile server"
                                .to_string(),
                        })?,
                    };
                    (events, true)
                }
            };
            for sse_ev in events {
                let data = sse_ev.data.trim();
                if data.is_empty() { continue; }
                // Raw line, before parsing: a payload we fail to decode is the
                // one worth having in the log.
                crate::client::log_wire("sse", || json!({"data": data}));
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
        // Emit every captured reasoning item as one synthetic chunk, in stream
        // order, so the Accumulator can hang them off the assistant message for
        // the next request's `input[]` — the same shape of solution as
        // `crate::anthropic::chat_stream`'s thinking-block flush.
        if !state.reasoning_items.is_empty() {
            yield crate::types::ChatChunk {
                choices: vec![],
                usage: None,
                anthropic_thinking_blocks: vec![],
                responses_reasoning_items: std::mem::take(&mut state.reasoning_items),
            };
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
    Ok(Box::pin(stream))
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
    /// Complete `{"type":"reasoning", …}` output items, in stream order, for
    /// replay in the next request's `input[]` (see [`build_body`]). Only items
    /// carrying `encrypted_content` land here.
    reasoning_items: Vec<Value>,
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
        // and emit id+name+args in one go. For a reasoning item, stash the
        // whole item for replay.
        "response.output_item.done" => {
            let item = ev.get("item");
            let item_type = item.and_then(|i| i.get("type")).and_then(Value::as_str);
            // `.done` — not `.added` — is the point at which a reasoning item is
            // whole (`.added` announces it before `encrypted_content` exists).
            if item_type == Some("reasoning") {
                capture_reasoning_item(state, item);
                return Ok(None);
            }
            if item_type != Some("function_call") {
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
            let finish = map_finish_reason(
                response,
                state.saw_function_call,
                kind == "response.incomplete",
            );
            Ok(Some(ChatChunk {
                choices: vec![ChunkChoice {
                    delta: Delta::default(),
                    finish_reason: Some(finish),
                }],
                usage,
                anthropic_thinking_blocks: vec![],
                responses_reasoning_items: vec![],
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
                kind: {
                    let k = classify_codex_error(code);
                    if k == crate::client::ChatErrorKind::Transient
                        && crate::retry::is_usage_limit_text(&msg)
                    {
                        crate::client::ChatErrorKind::UsageLimit
                    } else {
                        k
                    }
                },
                message: format!("responses stream failed: {msg}"),
            }))
        }
        "error" => {
            // The payload shape varies by backend build: flat
            // (`{"type":"error","code":…,"message":…}`) or nested
            // (`{"type":"error","error":{…}}`, like `response.failed`). Read
            // whichever is present — reading only the top level turned a
            // nested `server_error` (transient, retryable) into a terminal
            // "unknown error" that killed the turn.
            let err_obj = ev.get("error").filter(|e| e.is_object()).unwrap_or(ev);
            // Hybrids of the two shapes turn up as well — the code at the top
            // level beside a nested object carrying only the message. Read each
            // field from the nested object first and fall back to the outer
            // event, because taking the nested object's missing `code` at face
            // value classifies a top-level `server_error` as terminal and kills
            // a turn that was worth retrying. When there is no nested object
            // `err_obj` *is* `ev` and the fallback is a no-op.
            let code = err_obj
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| ev.get("code").and_then(Value::as_str));
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| ev.get("message").and_then(Value::as_str));
            let msg = join_code_message(code, message).unwrap_or_else(|| {
                // Nothing recognizable at either level: carry the raw event
                // (bounded) so the failure stays diagnosable instead of the
                // dead-end "unknown error".
                let raw: String = ev.to_string().chars().take(300).collect();
                format!("unrecognized error event: {raw}")
            });
            Err(anyhow::Error::new(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind: {
                    let k = classify_codex_error(code);
                    if k == crate::client::ChatErrorKind::Transient
                        && crate::retry::is_usage_limit_text(&msg)
                    {
                        crate::client::ChatErrorKind::UsageLimit
                    } else {
                        k
                    }
                },
                message: format!("responses stream error: {msg}"),
            }))
        }
        _ => Ok(None), // response.created, .in_progress, output_item.added(non-fn), part events, …
    }
}

/// Stash a completed `{"type":"reasoning", …}` output item for replay in the
/// next request's `input[]`, preserving stream order.
///
/// The item is stored **verbatim** (`id`, `summary`, `encrypted_content`, and
/// anything else the server attached): the blob is opaque and must go back
/// unmodified, so re-assembling a "clean" item here would only risk invalidating
/// it.
///
/// An item with no (or an empty) `encrypted_content` is dropped rather than
/// stored. Under `store:false` the encrypted state is what makes an item
/// replayable; a stateful (`store:true`) deployment or a non-OpenAI provider
/// variant may emit reasoning items without it, and replaying one of those is
/// precisely the request the endpoint rejects. Dropping it costs a little
/// context; sending it would fail the whole turn.
fn capture_reasoning_item(state: &mut StreamState, item: Option<&Value>) {
    let Some(item) = item else { return };
    let has_state = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_state {
        return;
    }
    state.reasoning_items.push(item.clone());
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
    join_code_message(
        err.get("code").and_then(Value::as_str),
        err.get("message").and_then(Value::as_str),
    )
}

/// Render an already-resolved `code`/`message` pair, for callers that had to
/// source the two fields from different levels of the payload.
fn join_code_message(code: Option<&str>, message: Option<&str>) -> Option<String> {
    match (code, message) {
        (Some(c), Some(m)) => Some(format!("{c}: {m}")),
        (_, Some(m)) => Some(m.to_string()),
        (Some(c), None) => Some(c.to_string()),
        (None, None) => None,
    }
}

/// Classify a Codex error code as transient or terminal. Only clearly transient
/// codes (rate limit, server overload/error, timeout) are marked retryable; all
/// others (auth, bad request, etc.) are terminal (`Other`).
fn classify_codex_error(code: Option<&str>) -> crate::client::ChatErrorKind {
    match code {
        Some("rate_limit_exceeded" | "server_error" | "server_is_overloaded" | "timeout") => {
            crate::client::ChatErrorKind::Transient
        }
        _ => crate::client::ChatErrorKind::Other,
    }
}

/// Map the Responses finish to hrdr's OpenAI `finish_reason` vocabulary.
/// `incomplete_details.reason == "max_output_tokens"` → `length` (so truncation
/// is detected); a plain completion → `tool_calls` when a function call was
/// emitted, else `stop`.
///
/// `is_incomplete` is whether the source event was `response.incomplete`
/// (rather than `response.completed`) — the server explicitly said this reply
/// is not the whole story. Any `response.incomplete` whose reason isn't one of
/// the two recognized values (a future/unrecognized reason, or the field
/// missing) must still map to a truncation-signalling finish reason rather
/// than falling through to a clean `stop`/`tool_calls`:
/// [`crate::Accumulator::truncated`] only checks `finish_reason` for
/// `"length"`/`"max_tokens"`, so a clean mapping here would make it wrongly
/// report `false` for a reply the server itself flagged as cut short.
fn map_finish_reason(
    response: Option<&Value>,
    saw_function_call: bool,
    is_incomplete: bool,
) -> String {
    let reason = response
        .and_then(|r| r.get("incomplete_details"))
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => "length".to_string(),
        Some("content_filter") => "content_filter".to_string(),
        _ if is_incomplete => "length".to_string(),
        _ if saw_function_call => "tool_calls".to_string(),
        _ => "stop".to_string(),
    }
}

/// Map the Responses usage object into hrdr's [`Usage`]. `input_tokens` /
/// `output_tokens` are already inclusive totals; `cached_tokens` and
/// `reasoning_tokens` are surfaced as the standard OpenAI detail subsets.
fn map_usage(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage?;
    let field = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let prompt = field("input_tokens").unwrap_or(0);
    let completion = field("output_tokens").unwrap_or(0);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
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
            responses_reasoning_items: vec![],
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
        let body = build_body("gpt-5.5", None, None, None, None, None, &[user("hi")], &[]);
        assert!(body.get("instructions").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn effort_and_generation_params_serialize_when_set() {
        let body = build_body(
            "gpt-5.5",
            Some("high"),
            Some(0.3),
            Some(0.9),
            Some(4096),
            None,
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

    /// The Responses body carries `prompt_cache_key` at the top level when the
    /// caller set one, verbatim. Without it, GPT-5.6 does not reliably match the
    /// prompt cache, and this endpoint re-sends the entire conversation every
    /// round (`store:false`) — so the miss is charged on the whole prefix.
    #[test]
    fn prompt_cache_key_rides_at_the_top_level_when_set() {
        let body = build_body(
            "gpt-5.6",
            None,
            None,
            None,
            None,
            Some("hrdr-agent-0f1e2d3c"),
            &[sys("you are hrdr"), user("hi")],
            &[],
        );
        assert_eq!(body["prompt_cache_key"], "hrdr-agent-0f1e2d3c");
        // Nothing else moved: the key is additive, not a reshape.
        assert_eq!(body["instructions"], "you are hrdr");
        assert_eq!(body["store"], false);
    }

    /// The same builder, same conversation, two consecutive rounds: the key is
    /// whatever the client holds, so it does not drift request to request. A
    /// per-request value would share a prefix with nothing and defeat the
    /// parameter entirely.
    #[test]
    fn prompt_cache_key_is_identical_across_consecutive_requests() {
        let key = Some("hrdr-agent-0f1e2d3c");
        let first = build_body("gpt-5.6", None, None, None, None, key, &[user("one")], &[]);
        let second = build_body(
            "gpt-5.6",
            None,
            None,
            None,
            None,
            key,
            &[user("one"), user("two")],
            &[],
        );
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        assert_eq!(first["prompt_cache_key"], "hrdr-agent-0f1e2d3c");
    }

    #[test]
    fn multiple_system_messages_join_into_instructions() {
        let body = build_body(
            "gpt-5.5",
            None,
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

    /// The `content_filter` arm of [`map_finish_reason`]. Both of its
    /// neighbours — `max_output_tokens` above it and the incomplete catch-all
    /// below — map to `"length"`, so losing this arm changes nothing a test
    /// would otherwise notice: the stream still ends, the reply is still
    /// flagged truncated. The only casualty is what the user is told, and they
    /// are told they hit the token cap when the reply was filtered.
    #[test]
    fn incomplete_content_filter_maps_to_content_filter() {
        let ev = json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "content_filter"},
            "usage": {"input_tokens": 5, "output_tokens": 5}
        }});
        let mut state = StreamState::default();
        let chunk = map_event(&mut state, &ev).unwrap().unwrap();
        assert!(state.terminal_seen);
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("content_filter"),
            "a filtered reply must not be reported as a token-cap truncation"
        );
    }

    #[test]
    fn incomplete_with_unrecognized_reason_still_signals_truncation() {
        // A `response.incomplete` whose `incomplete_details.reason` is neither
        // of the two recognized values (a reason a future Responses API
        // version might add) must still flag truncation, not fall through to a
        // clean `stop` — the server explicitly said this reply is incomplete.
        let ev = json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "some_future_reason"},
            "usage": {"input_tokens": 5, "output_tokens": 5}
        }});
        let mut state = StreamState::default();
        let chunk = map_event(&mut state, &ev).unwrap().unwrap();
        assert!(state.terminal_seen);
        let mut acc = Accumulator::new();
        acc.push(&chunk);
        assert!(
            acc.truncated(),
            "unrecognized incomplete reason must signal truncation, got {:?}",
            chunk.choices[0].finish_reason
        );

        // Same for a `response.incomplete` with no `incomplete_details` at all.
        let ev_no_details = json!({"type": "response.incomplete", "response": {
            "usage": {"input_tokens": 5, "output_tokens": 5}
        }});
        let mut state2 = StreamState::default();
        let chunk2 = map_event(&mut state2, &ev_no_details).unwrap().unwrap();
        let mut acc2 = Accumulator::new();
        acc2.push(&chunk2);
        assert!(
            acc2.truncated(),
            "missing incomplete_details must still signal truncation"
        );
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

        let overloaded = json!({
            "type": "error",
            "code": "server_is_overloaded",
            "message": "Our servers are currently overloaded. Please try again later."
        });
        let err = map_event(&mut state, &overloaded).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
    }

    /// A `rate_limit_exceeded` whose message only names "usage quota" is a rate
    /// limit, not a spent cap: bare "quota" wording (no billing / credit /
    /// spend / insufficient_quota marker) stays retryable.
    #[test]
    fn rate_limit_with_quota_wording_stays_transient() {
        let mut state = StreamState::default();
        let failed = json!({"type": "response.failed", "response": {
            "error": {"code": "rate_limit_exceeded", "message": "you have reached your usage quota"}
        }});
        let err = map_event(&mut state, &failed).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(crate::retry::is_transient(&err));
        assert!(
            chat_err.message.contains("usage quota"),
            "{}",
            chat_err.message
        );

        let top = json!({"type": "error", "code": "rate_limit_exceeded",
            "message": "you have reached your usage quota"});
        let err = map_event(&mut state, &top).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(crate::retry::is_transient(&err));
    }

    /// An `error` event may nest its payload (`{"type":"error","error":{…}}`),
    /// like `response.failed` does. Reading only the top level turned a nested
    /// `server_error` — transient, retryable — into a terminal "unknown error"
    /// that killed the turn instead of retrying.
    #[test]
    fn nested_error_event_is_parsed_and_classified() {
        let mut state = StreamState::default();
        let nested = json!({"type": "error", "error": {
            "code": "server_error", "message": "overloaded"
        }});
        let err = map_event(&mut state, &nested).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(
            chat_err.message.contains("server_error") && chat_err.message.contains("overloaded"),
            "{}",
            chat_err.message
        );
    }

    /// The two shapes also mix: the code at the top level, a nested object
    /// holding only the message. Reading the code from the nested object alone
    /// yields `None`, and an unknown code classifies as terminal — so a
    /// retryable `server_error` would end the turn.
    #[test]
    fn hybrid_error_event_falls_back_to_the_outer_code() {
        let mut state = StreamState::default();
        let hybrid = json!({
            "type": "error",
            "code": "server_error",
            "error": {"message": "try later"}
        });
        let err = map_event(&mut state, &hybrid).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(
            chat_err.message.contains("server_error") && chat_err.message.contains("try later"),
            "{}",
            chat_err.message
        );
    }

    /// The mirror hybrid: the message at the top level, a nested object holding
    /// only the code. The message must not be lost to the empty nested slot.
    #[test]
    fn hybrid_error_event_falls_back_to_the_outer_message() {
        let mut state = StreamState::default();
        let hybrid = json!({
            "type": "error",
            "message": "try later",
            "error": {"code": "server_error"}
        });
        let err = map_event(&mut state, &hybrid).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Transient);
        assert!(
            chat_err.message.contains("server_error") && chat_err.message.contains("try later"),
            "{}",
            chat_err.message
        );
    }

    /// An `error` event with no recognizable `code`/`message` at either level
    /// must carry the raw event (bounded) in its message — a bare
    /// "unknown error" is undiagnosable.
    #[test]
    fn unrecognized_error_event_carries_the_raw_payload() {
        let mut state = StreamState::default();
        let opaque = json!({"type": "error", "detail": {"reason": "socket reset"}});
        let err = map_event(&mut state, &opaque).unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(chat_err.kind, crate::client::ChatErrorKind::Other);
        assert!(
            chat_err.message.contains("unrecognized error event")
                && chat_err.message.contains("socket reset"),
            "{}",
            chat_err.message
        );
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

    /// A completed reasoning item is captured verbatim off
    /// `response.output_item.done` — the point at which the item (and its
    /// `encrypted_content`) is whole. It yields no chunk of its own; it is
    /// flushed once at end of stream.
    #[test]
    fn reasoning_item_captured_from_output_item_done() {
        let mut state = StreamState::default();
        let item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "check the file"}],
            "encrypted_content": "ENC_ABC",
        });
        let ev = json!({"type": "response.output_item.done", "item": item});
        assert!(
            map_event(&mut state, &ev).unwrap().is_none(),
            "a reasoning item emits no chunk at capture time"
        );
        // Stored verbatim — the blob is opaque and must go back unmodified.
        assert_eq!(state.reasoning_items, vec![item]);
    }

    /// `response.output_item.added` announces the item before its encrypted
    /// state exists, so capturing there would store a useless (and
    /// endpoint-rejected) item. Only `.done` captures.
    #[test]
    fn reasoning_item_added_event_is_not_captured() {
        let mut state = StreamState::default();
        let ev = json!({"type": "response.output_item.added", "item": {
            "type": "reasoning", "id": "rs_1", "summary": []
        }});
        assert!(map_event(&mut state, &ev).unwrap().is_none());
        assert!(state.reasoning_items.is_empty());
    }

    /// A reasoning item with no `encrypted_content` (a stateful `store:true`
    /// deployment, or a provider variant) must be dropped at capture time:
    /// replaying an item without its encrypted state is exactly the request the
    /// Responses endpoint rejects, and one bad item fails the whole turn.
    #[test]
    fn reasoning_item_without_encrypted_content_is_dropped() {
        let mut state = StreamState::default();
        for item in [
            json!({"type": "reasoning", "id": "rs_1", "summary": []}),
            json!({"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": ""}),
            json!({"type": "reasoning", "id": "rs_3", "summary": [], "encrypted_content": null}),
        ] {
            let ev = json!({"type": "response.output_item.done", "item": item});
            assert!(map_event(&mut state, &ev).unwrap().is_none());
        }
        assert!(
            state.reasoning_items.is_empty(),
            "unencrypted reasoning items must never be stored: {:?}",
            state.reasoning_items
        );
    }

    /// Stream order is the replay order: the encrypted state is validated
    /// against the items that follow it, so a reorder is as bad as a drop.
    #[test]
    fn reasoning_items_preserve_stream_order() {
        let mut state = StreamState::default();
        for (id, enc) in [("rs_1", "E1"), ("rs_2", "E2"), ("rs_3", "E3")] {
            let ev = json!({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": id, "summary": [], "encrypted_content": enc
            }});
            map_event(&mut state, &ev).unwrap();
        }
        let ids: Vec<&str> = state
            .reasoning_items
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["rs_1", "rs_2", "rs_3"]);
    }

    /// End-to-end capture: reasoning items interleaved with a function call
    /// reach the assembled [`ChatMessage`] via the same synthetic-chunk flush
    /// `chat_stream` performs after the byte loop, without disturbing the text
    /// or tool-call accumulation.
    #[test]
    fn reasoning_items_reach_the_assembled_message() {
        let events = vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "E1"
            }}),
            json!({"type": "response.output_text.delta", "delta": "checking"}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "read", "arguments": "{}"
            }}),
            json!({"type": "response.completed", "response": {
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }}),
        ];
        let mut state = StreamState::default();
        let mut acc = Accumulator::new();
        for ev in &events {
            if let Some(chunk) = map_event(&mut state, ev).unwrap() {
                acc.push(&chunk);
            }
        }
        // The end-of-stream flush `chat_stream` yields.
        acc.push(&ChatChunk {
            choices: vec![],
            usage: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: std::mem::take(&mut state.reasoning_items),
        });

        let msg = acc.into_message();
        assert_eq!(msg.content.as_deref(), Some("checking"));
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].id, "call_1");
        assert_eq!(msg.responses_reasoning_items.len(), 1);
        assert_eq!(msg.responses_reasoning_items[0]["encrypted_content"], "E1");
    }

    /// The replay contract: an assistant turn that reasoned and then called a
    /// tool re-enters `input[]` as reasoning items → `output_text` →
    /// `function_call`, with the tool result following.
    #[test]
    fn reasoning_items_replay_before_text_and_function_calls() {
        let rs1 = json!({
            "type": "reasoning", "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "plan"}],
            "encrypted_content": "ENC_1",
        });
        let rs2 = json!({
            "type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "ENC_2",
        });
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: Some("let me check".into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![rs1.clone(), rs2.clone()],
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
        let body = build_body(
            "gpt-5.5",
            None,
            None,
            None,
            None,
            None,
            &[
                user("go"),
                assistant,
                ChatMessage::tool_result("call_1", "file body"),
            ],
            &[],
        );
        let input = body["input"].as_array().unwrap();
        // user, reasoning, reasoning, output_text, function_call, output.
        assert_eq!(input.len(), 6, "{input:#?}");
        assert_eq!(input[0]["role"], "user");
        // Replayed byte-for-byte, in stream order, ahead of everything the turn
        // produced after them.
        assert_eq!(input[1], rs1);
        assert_eq!(input[2], rs2);
        assert_eq!(input[3]["role"], "assistant");
        assert_eq!(input[3]["content"][0]["type"], "output_text");
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["call_id"], "call_1");
        assert_eq!(input[5]["type"], "function_call_output");
    }

    /// No-regression guard: a history with no stored reasoning items must build
    /// byte-identical to what it built before replay existed.
    #[test]
    fn messages_without_reasoning_items_build_an_unchanged_body() {
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: Some("done".into()),
            reasoning_content: Some("some plaintext thinking".into()),
            anthropic_thinking_blocks: vec![json!({"type": "thinking", "thinking": "x"})],
            responses_reasoning_items: vec![],
            origin: MessageOrigin::User,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let body = build_body(
            "gpt-5.5",
            None,
            None,
            None,
            None,
            None,
            &[sys("you are hrdr"), user("go"), assistant],
            &[],
        );
        assert_eq!(
            body,
            json!({
                "model": "gpt-5.5",
                "stream": true,
                "store": false,
                "instructions": "you are hrdr",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "go"}]},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
                ],
            })
        );
        // The other two reasoning channels stay off this wire: `reasoning_content`
        // is a plaintext transcript (feeding it back degrades the model) and
        // thinking blocks are Anthropic-native objects this endpoint rejects.
        let wire = body.to_string();
        assert!(!wire.contains("some plaintext thinking"), "{wire}");
        assert!(!wire.contains("thinking"), "{wire}");
    }

    /// Drive a canned SSE body through the real [`chat_stream`] and collect what
    /// came out: the chunks, then the error that terminated the stream (if any —
    /// `try_stream` stops at the first `Err`, so there is at most one).
    ///
    /// The forced backend is the whole point. [`crate::client::detect_backend`]
    /// keys on the HOST *and* the path, so a mock bound to `127.0.0.1` is
    /// `Backend::OpenAi` and `Client::chat_stream` would dispatch to the
    /// chat-completions path — none of this module would run. Everything after
    /// the byte loop above (the reasoning-item flush, the missing-terminator
    /// truncation error) is reachable from a test only this way.
    async fn codex_stream(body: &'static str) -> (Vec<ChatChunk>, Option<anyhow::Error>) {
        let base_url = crate::client::serve_once(body).await;
        let mut client = crate::Client::new(base_url, Some("test-token".to_string()), "gpt-5.5");
        client.set_backend_for_test(crate::client::Backend::Codex);
        let mut stream = client
            .chat_stream(&[user("hi")], &[])
            .await
            .expect("the mock server answers 200");
        let (mut chunks, mut err) = (Vec::new(), None);
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => chunks.push(chunk),
                Err(e) => err = Some(e),
            }
        }
        (chunks, err)
    }

    /// The control for the truncation test below: a stream that DOES terminate
    /// must come back clean, so a green truncation test cannot be explained by
    /// "everything through this path errors".
    #[tokio::test]
    async fn a_complete_stream_yields_text_and_the_mapped_finish_reason() {
        let body = "event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n\
                    event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n\
                    event: response.completed\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":5}}}\n\n";
        let (chunks, err) = codex_stream(body).await;
        assert!(
            err.is_none(),
            "a terminated stream must not error: {:#}",
            err.unwrap()
        );

        let text: String = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.content.clone())
            .collect();
        assert_eq!(text, "hello");

        let finish: Vec<&str> = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.finish_reason.as_deref())
            .collect();
        assert_eq!(
            finish,
            ["stop"],
            "a completion with no function call is a plain stop"
        );

        let usage = chunks
            .iter()
            .find_map(|c| c.usage.as_ref())
            .expect("response.completed carries usage");
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (11, 5));

        assert!(
            chunks
                .iter()
                .all(|c| c.responses_reasoning_items.is_empty()),
            "no reasoning items in the stream → no synthetic chunk"
        );
    }

    /// A stream cut before `response.completed` must be **Transient**, which is
    /// what makes the agent re-request instead of accepting half a reply as
    /// final. The OpenAI equivalent is covered end-to-end by hrdr-agent's
    /// `agent_run_incomplete_stream_then_retry`.
    #[tokio::test]
    async fn a_stream_without_a_terminal_event_is_a_transient_error() {
        let body = "event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"half a rep\"}\n\n";
        let (chunks, err) = codex_stream(body).await;

        // The partial text still arrives — the error is about the *ending*, not
        // about the chunks that got through.
        let text: String = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.content.clone())
            .collect();
        assert_eq!(text, "half a rep");

        let err = err.expect("the stream must have terminated with an error");
        let typed = err
            .downcast_ref::<crate::client::ChatError>()
            .unwrap_or_else(|| panic!("error must be a typed ChatError, got: {err:#}"));
        assert_eq!(
            typed.kind,
            crate::client::ChatErrorKind::Transient,
            "a cut stream must be retryable, not terminal"
        );
        assert!(
            typed.message.contains("response.completed"),
            "message must name the missing terminator: {}",
            typed.message
        );
    }

    /// The post-loop flush: reasoning items captured off the stream are re-emitted
    /// as one synthetic chunk so the [`Accumulator`] can hang them off the
    /// assistant message for the next request's `input[]`. Under `store:false`
    /// the encrypted blob is the model's only memory of its own plan, so the
    /// items' CONTENT is the assertion — and it must be verbatim, since the blob
    /// is opaque and re-encoding it would invalidate it.
    #[tokio::test]
    async fn reasoning_items_are_flushed_after_the_loop() {
        let body = "event: response.output_item.done\n\
                    data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"blob-one\"}}\n\n\
                    event: response.output_item.done\n\
                    data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_2\",\"encrypted_content\":\"\"}}\n\n\
                    event: response.output_text.delta\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n\
                    event: response.completed\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":5}}}\n\n";
        let (chunks, err) = codex_stream(body).await;
        assert!(err.is_none(), "stream terminated cleanly");

        let items: Vec<&Vec<Value>> = chunks
            .iter()
            .map(|c| &c.responses_reasoning_items)
            .filter(|i| !i.is_empty())
            .collect();
        assert_eq!(items.len(), 1, "exactly one synthetic flush chunk");
        assert_eq!(
            *items[0],
            vec![json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "blob-one",
            })],
            "the item is stored verbatim; the one with no encrypted state is dropped"
        );
    }
}
