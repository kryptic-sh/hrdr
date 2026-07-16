//! Native Anthropic Messages API backend (`POST /v1/messages`).
//!
//! hrdr's internal conversation is OpenAI-shaped (`role`/`content`/`tool_calls`/
//! `tool_call_id`). This module translates it to Anthropic's native wire format
//! — `system` hoisted to a top-level block array, `messages` carrying typed
//! content blocks (`text` / `tool_use` / `tool_result`), `tools` with
//! `input_schema`, and a required `max_tokens` — and normalizes the streaming
//! response back into the OpenAI-shaped [`ChatChunk`] the [`Accumulator`] already
//! understands, so the agent loop and frontends are unchanged.
//!
//! Why native (not Anthropic's OpenAI-compat endpoint): the compat endpoint
//! **silently drops** `cache_control` and doesn't expose thinking, so prompt
//! caching and extended thinking are only reachable here. Covers: system +
//! messages + tools + streaming, prompt caching (`cache_control` on system, the
//! last tool, and the last message), and **extended thinking** (a reasoning
//! `effort` level turns on a `thinking` budget that scales with `max_tokens`;
//! interleaved-thinking is enabled alongside tools; `thinking_delta`s stream to
//! hrdr's reasoning channel).
//!
//! [`Accumulator`]: crate::Accumulator

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::sse::SseDecoder;

use crate::types::{
    CacheMode, ChatChunk, ChatMessage, ChunkChoice, Delta, Role, ToolDef, Usage, reasoning_chunk,
    text_chunk, tool_call_chunk,
};

/// Anthropic API version pinned in the `anthropic-version` header.
pub(crate) const API_VERSION: &str = "2023-06-01";

/// Build the native `/v1/messages` request body from hrdr's OpenAI-shaped
/// history. When `cache == Ephemeral`, `cache_control` breakpoints are placed on
/// the last system block, the last tool, and the last content block of the last
/// message (Anthropic allows ≤4; we use ≤3).
///
/// `top_p` and `stop` map the corresponding [`crate::RequestParams`] fields onto
/// the Messages API's `top_p` / `stop_sequences`. `seed` has no equivalent on
/// this endpoint (the Messages API doesn't support a determinism seed at all)
/// and is intentionally not threaded through here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_body(
    model: &str,
    max_tokens: u32,
    effort: Option<&str>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: &[String],
    cache: CacheMode,
    ttl_1h: bool,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Value {
    let ephemeral = cache == CacheMode::Ephemeral;
    let (system, msgs) = split_system_and_messages(messages);

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": msgs,
        "stream": true,
    });

    // Extended thinking: an effort level turns on a thinking budget (which fits
    // inside `max_tokens`). Anthropic requires the temperature to default to 1
    // and forbids `top_p` while thinking, so both are only sent when thinking
    // is off.
    match thinking_budget(effort, max_tokens) {
        Some(budget) => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
        None => {
            if let Some(t) = temperature {
                body["temperature"] = json!(t);
            }
            if let Some(p) = top_p {
                body["top_p"] = json!(p);
            }
        }
    }

    if !stop.is_empty() {
        body["stop_sequences"] = json!(stop);
    }

    if !system.is_empty() {
        let mut blocks = system;
        if ephemeral {
            mark_last_block(&mut blocks, ttl_1h);
        }
        body["system"] = Value::Array(blocks);
    }

    if !tools.is_empty() {
        let mut defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            })
            .collect();
        if ephemeral && let Some(last) = defs.last_mut() {
            last["cache_control"] = crate::types::cache_control(ttl_1h);
        }
        body["tools"] = Value::Array(defs);
    }

    // Rolling cache breakpoint on the last content block of the last message.
    if ephemeral
        && let Some(last) = body["messages"].as_array_mut().and_then(|m| m.last_mut())
        && let Some(blocks) = last.get_mut("content").and_then(|c| c.as_array_mut())
    {
        mark_last_block(blocks, ttl_1h);
    }

    body
}

/// Extended-thinking budget (tokens) for a reasoning `effort` level, or `None`
/// to leave thinking off (no effort set, an unrecognized label, or a
/// `max_tokens` window too small to fit a budget plus room for the answer).
/// The budget scales with `max_tokens` so raising the output cap gives Claude
/// more room to think; Anthropic requires `budget ≥ 1024` and `budget <
/// max_tokens`.
pub(crate) fn thinking_budget(effort: Option<&str>, max_tokens: u32) -> Option<u32> {
    let level = effort.and_then(crate::normalize_effort)?;
    let frac = match level.as_str() {
        "minimal" => 0.25,
        "low" => 0.40,
        "medium" => 0.60,
        "high" => 0.75,
        "xhigh" => 0.85,
        "max" => 0.95,
        // "none" (and anything unmapped) leaves extended thinking off.
        _ => return None,
    };
    // Reserve at least 1024 tokens below `max_tokens` for the actual answer.
    let ceiling = max_tokens.checked_sub(1024).filter(|c| *c >= 1024)?;
    Some(((max_tokens as f64 * frac) as u32).clamp(1024, ceiling))
}

/// Split hrdr history into Anthropic `system` blocks + `messages`. Consecutive
/// same-role messages (e.g. a run of tool results) are coalesced into one
/// message, since Anthropic requires alternating user/assistant turns and tool
/// results to ride in a `user` message.
fn split_system_and_messages(messages: &[ChatMessage]) -> (Vec<Value>, Vec<Value>) {
    let mut system: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for m in messages {
        match m.role {
            Role::System => {
                if let Some(text) = &m.content
                    && !text.is_empty()
                {
                    system.push(json!({ "type": "text", "text": text }));
                }
            }
            Role::User => append_blocks(&mut out, "user", user_text_blocks(m)),
            Role::Tool => append_blocks(&mut out, "user", vec![tool_result_block(m)]),
            Role::Assistant => append_blocks(&mut out, "assistant", assistant_blocks(m)),
        }
    }
    (system, out)
}

/// Append `blocks` to the last message if it shares `role`, else start a new one.
fn append_blocks(out: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
    {
        arr.extend(blocks);
        return;
    }
    out.push(json!({ "role": role, "content": blocks }));
}

fn user_text_blocks(m: &ChatMessage) -> Vec<Value> {
    match &m.content {
        Some(t) if !t.is_empty() => vec![json!({ "type": "text", "text": t })],
        _ => Vec::new(),
    }
}

/// A `tool_result` block bound to its call id. Non-string tool output isn't a
/// concern here — hrdr tool results are always text.
fn tool_result_block(m: &ChatMessage) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
        "content": m.content.clone().unwrap_or_default(),
    })
}

/// Assistant turn → optional leading `thinking`/`redacted_thinking` blocks
/// (required when tool_use is also present so the API can verify the signature),
/// then an optional `text` block, then one `tool_use` block per call.
fn assistant_blocks(m: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    // Thinking blocks MUST come first in the Anthropic assistant message when
    // the turn also contained tool_use — the API rejects the follow-up request
    // with a 400 if the signature is missing.
    for blk in &m.anthropic_thinking_blocks {
        blocks.push(blk.clone());
    }
    if let Some(t) = &m.content
        && !t.is_empty()
    {
        blocks.push(json!({ "type": "text", "text": t }));
    }
    for call in m.tool_calls.iter().flatten() {
        // A zero-argument tool call streams no `input_json_delta`, so `arguments`
        // is empty. Anthropic's schema needs `input` to be an object, and the
        // execution layer already treats empty args as `{}` — so an empty string
        // here is a no-arg call, not lost intent: send `{}`.
        //
        // A non-empty string that fails to parse is a genuinely malformed args
        // string. Preserve it as a JSON *string* value rather than silently
        // rewriting to `{}`: that erases the model's original intent from history
        // and hides the problem. It will likely still fail validation on resend,
        // but that failure is honest and visible.
        let input: Value = if call.function.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&call.function.arguments)
                .unwrap_or_else(|_| json!(call.function.arguments))
        };
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.function.name,
            "input": input,
        }));
    }
    blocks
}

/// Tag the last block in a block array with a cache breakpoint (`ttl_1h` selects
/// the 1-hour TTL).
fn mark_last_block(blocks: &mut [Value], ttl_1h: bool) {
    if let Some(last) = blocks.last_mut()
        && let Some(obj) = last.as_object_mut()
    {
        obj.insert("cache_control".into(), crate::types::cache_control(ttl_1h));
    }
}

/// Stream a completion from the native Messages API, yielding OpenAI-shaped
/// [`ChatChunk`]s.
///
/// Takes slices to avoid cloning the full history on every retry. The request
/// body is serialized before any network I/O, so the borrow does not extend
/// into the returned [`crate::ChatStream`] future.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_stream(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    effort: Option<&str>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: &[String],
    cache: CacheMode,
    ttl_1h: bool,
    extra_headers: &[(String, String)],
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<(Value, crate::ChatStream)> {
    let body = build_body(
        model,
        max_tokens,
        effort,
        temperature,
        top_p,
        stop,
        cache,
        ttl_1h,
        messages,
        tools,
    );
    let mut req = http
        .post(format!("{base_url}/messages"))
        .header("anthropic-version", API_VERSION)
        .json(&body);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    for (k, v) in extra_headers {
        req = req.header(k, v);
    }
    // Betas: interleaved thinking (reason between tool calls) when thinking is on
    // with tools; extended 1-hour cache TTL when requested.
    let mut betas: Vec<&str> = Vec::new();
    if body.get("thinking").is_some() && !tools.is_empty() {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if ttl_1h && cache == CacheMode::Ephemeral {
        betas.push("extended-cache-ttl-2025-04-11");
    }
    if !betas.is_empty() {
        req = req.header("anthropic-beta", betas.join(","));
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
        // Anthropic content-block index → our flat tool-call index.
        let mut tool_slot: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        let mut next_tool: usize = 0;
        // Accumulated Anthropic thinking blocks, keyed by content-block index
        // (thinking_text, signature). Emitted as one synthetic chunk after the
        // byte loop so the accumulator can store them for the next request.
        let mut thinking_slot: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        // Redacted thinking blocks — full `data` arrives in content_block_start,
        // no deltas. Collected in stream order alongside their block index.
        let mut redacted_order: Vec<(u64, Value)> = Vec::new();
        // Whether message_stop was received (for truncation detection).
        let mut message_stop_seen = false;
        // Feed raw byte chunks into the SSE decoder. Anthropic SSE carries
        // `event:` and `data:` lines; every `data:` payload is a complete JSON
        // object with its own `type`, so the `event:` line is redundant and
        // ignored (ev.event is unused). Splitting on 0x0A is safe for UTF-8.
        let mut decoder = SseDecoder::new();
        loop {
            // On EOF, `finish()` flushes a final `data:` line that arrived
            // without a blank-line terminator, so a trailing `message_stop`
            // event isn't lost (which would falsely look like a cut stream).
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
                    decoder.push(&chunk);
                    (decoder.drain(), false)
                }
                None => (decoder.finish(), true),
            };
            for sse_ev in events {
                let data = &sse_ev.data;
                if data.is_empty() { continue; }
                let ev: Value = serde_json::from_str(data)
                    .with_context(|| format!("decoding stream event: {data}"))?;
                if let Some(out) = map_event(
                    &ev,
                    &mut tool_slot,
                    &mut next_tool,
                    &mut thinking_slot,
                    &mut redacted_order,
                    &mut message_stop_seen,
                )? {
                    yield out;
                }
            }
            if at_eof { break; }
        }
        // Emit all accumulated thinking blocks (thinking+signature pairs and
        // redacted blocks) as one synthetic chunk, ordered by their stream index,
        // so the Accumulator can store them for the next request.
        let mut all_thinking: Vec<(u64, Value)> = thinking_slot
            .into_iter()
            // Keep a block that carries either text or a signature. A signed
            // block with empty text still MUST be replayed on the follow-up
            // request — dropping it makes Anthropic 400 the tool_use turn.
            .filter(|(_, (text, sig))| !text.is_empty() || !sig.is_empty())
            .map(|(idx, (text, sig))| {
                (idx, json!({"type": "thinking", "thinking": text, "signature": sig}))
            })
            .collect();
        all_thinking.extend(redacted_order);
        all_thinking.sort_by_key(|(idx, _)| *idx);
        let thinking_blocks: Vec<Value> = all_thinking.into_iter().map(|(_, b)| b).collect();
        if !thinking_blocks.is_empty() {
            yield crate::types::ChatChunk {
                choices: vec![],
                usage: None,
                anthropic_thinking_blocks: thinking_blocks,
            };
        }
        // If message_stop never arrived, the stream was cut mid-response.
        // This classifies as transient so the retry loop can re-request.
        if !message_stop_seen {
            Err(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind: crate::client::ChatErrorKind::Transient,
                message: "incomplete stream: Anthropic stream ended without message_stop \
                          (partial response, safe to retry)"
                    .to_string(),
            })?;
        }
    };
    Ok((body, Box::pin(stream)))
}

/// Translate one Anthropic stream event into a [`ChatChunk`] (or `None` for
/// events with nothing for the accumulator: `ping`, `content_block_stop`, …).
fn map_event(
    ev: &Value,
    tool_slot: &mut std::collections::HashMap<u64, usize>,
    next_tool: &mut usize,
    thinking_slot: &mut std::collections::HashMap<u64, (String, String)>,
    redacted_order: &mut Vec<(u64, Value)>,
    message_stop_seen: &mut bool,
) -> Result<Option<ChatChunk>> {
    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "message_start" => {
            let u = ev.get("message").and_then(|m| m.get("usage"));
            Ok(Some(message_start_usage(u)))
        }
        "content_block_start" => {
            let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = ev.get("content_block");
            let block_type = block.and_then(|b| b.get("type")).and_then(Value::as_str);
            if block_type == Some("tool_use") {
                let slot = *next_tool;
                tool_slot.insert(idx, slot);
                *next_tool += 1;
                let id = block
                    .and_then(|b| b.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Ok(Some(tool_call_chunk(slot, Some(id), Some(name), None)))
            } else if block_type == Some("thinking") {
                thinking_slot.insert(idx, (String::new(), String::new()));
                Ok(None)
            } else if block_type == Some("redacted_thinking") {
                let data = block
                    .and_then(|b| b.get("data"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                redacted_order.push((idx, json!({"type": "redacted_thinking", "data": data})));
                Ok(None)
            } else {
                Ok(None)
            }
        }
        "content_block_delta" => {
            let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
            let delta = ev.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                Some("text_delta") => {
                    let t = delta.and_then(|d| d.get("text")).and_then(Value::as_str);
                    Ok(t.map(|t| text_chunk(t.to_string())))
                }
                Some("thinking_delta") => {
                    let t = delta
                        .and_then(|d| d.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(entry) = thinking_slot.get_mut(&idx) {
                        entry.0.push_str(t);
                    }
                    Ok((!t.is_empty()).then(|| reasoning_chunk(t.to_string())))
                }
                Some("signature_delta") => {
                    let sig = delta
                        .and_then(|d| d.get("signature"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(entry) = thinking_slot.get_mut(&idx) {
                        entry.1.push_str(sig);
                    }
                    Ok(None)
                }
                Some("input_json_delta") => {
                    let frag = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // An unknown block index (no matching `content_block_start`
                    // recorded it) must not silently default to tool slot 0 —
                    // that would corrupt tool 0's arguments with a stray
                    // fragment belonging to a different block. Ignore the delta.
                    match tool_slot.get(&idx).copied() {
                        Some(slot) => Ok(Some(tool_call_chunk(
                            slot,
                            None,
                            None,
                            Some(frag.to_string()),
                        ))),
                        None => Ok(None),
                    }
                }
                _ => Ok(None),
            }
        }
        "message_stop" => {
            *message_stop_seen = true;
            Ok(None)
        }
        "message_delta" => {
            let out = ev
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let finish = ev
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(map_stop_reason);
            // One chunk carrying end-of-turn usage + the mapped finish_reason
            // (so truncation — Anthropic's `max_tokens` → `length` — is detected).
            let chunk = ChatChunk {
                choices: finish
                    .map(|fr| {
                        vec![ChunkChoice {
                            delta: Delta::default(),
                            finish_reason: Some(fr),
                        }]
                    })
                    .unwrap_or_default(),
                usage: (out > 0).then_some(Usage {
                    completion_tokens: out,
                    ..Default::default()
                }),
                anthropic_thinking_blocks: vec![],
            };
            Ok((chunk.usage.is_some() || !chunk.choices.is_empty()).then_some(chunk))
        }
        "error" => {
            let err_obj = ev.get("error");
            let err_type = err_obj
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = err_obj
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let kind = match err_type {
                // Anthropic's retryable server-side error types. `api_error` is
                // their internal 500-equivalent and rides alongside
                // `overloaded_error` (529); classifying it terminal aborts the
                // turn on a transient hiccup that a retry would ride out.
                "rate_limit_error" | "overloaded_error" | "api_error" => {
                    crate::client::ChatErrorKind::Transient
                }
                _ => crate::client::ChatErrorKind::Other,
            };
            let err_msg = if err_type.is_empty() {
                format!("anthropic stream error: {msg}")
            } else {
                format!("anthropic stream error ({err_type}): {msg}")
            };
            Err(anyhow::Error::new(crate::client::ChatError {
                status: None,
                retry_after: None,
                kind,
                message: err_msg,
            }))
        }
        _ => Ok(None), // ping, content_block_stop, content_block_start(text), …
    }
}

/// Map an Anthropic `stop_reason` to the OpenAI `finish_reason` vocabulary.
fn map_stop_reason(stop: &str) -> String {
    match stop {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "end_turn" | "stop_sequence" => "stop",
        other => other,
    }
    .to_string()
}

/// Read a `u64` counter from an Anthropic usage object.
fn usage_field(usage: Option<&Value>, key: &str) -> u32 {
    usage
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

/// A prompt-usage chunk from Anthropic's `message_start`: total prompt tokens
/// (`input` + both cache counters) with the cache-read portion surfaced as
/// `cached_tokens`.
fn message_start_usage(usage: Option<&Value>) -> ChatChunk {
    let cache_read = usage_field(usage, "cache_read_input_tokens");
    let prompt = usage_field(usage, "input_tokens")
        + cache_read
        + usage_field(usage, "cache_creation_input_tokens");
    let mut u = Usage {
        prompt_tokens: prompt,
        ..Default::default()
    };
    if cache_read > 0 {
        u.prompt_tokens_details.cached_tokens = Some(cache_read);
    }
    ChatChunk {
        choices: vec![],
        usage: Some(u),
        anthropic_thinking_blocks: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, MessageOrigin, ToolCall};

    fn sys(t: &str) -> ChatMessage {
        ChatMessage::system(t)
    }
    fn user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
    }

    #[test]
    fn system_is_hoisted_and_messages_alternate() {
        let msgs = vec![sys("you are hrdr"), user("hi"), user("still me")];
        let body = build_body(
            "claude",
            1024,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &msgs,
            &[],
        );
        // System hoisted to a top-level block array.
        assert_eq!(body["system"][0]["text"], "you are hrdr");
        // Two consecutive user turns coalesce into one message.
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn tool_calls_and_results_map_to_blocks() {
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: Some("let me check".into()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let result = ChatMessage::tool_result("toolu_1", "file body");
        let body = build_body(
            "claude",
            512,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("go"), assistant, result],
            &[],
        );
        let m = body["messages"].as_array().unwrap();
        // user, assistant(text+tool_use), user(tool_result)
        assert_eq!(m.len(), 3);
        assert_eq!(m[1]["role"], "assistant");
        assert_eq!(m[1]["content"][0]["type"], "text");
        assert_eq!(m[1]["content"][1]["type"], "tool_use");
        assert_eq!(m[1]["content"][1]["id"], "toolu_1");
        assert_eq!(m[1]["content"][1]["input"]["path"], "a.rs");
        assert_eq!(m[2]["role"], "user");
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(m[2]["content"][0]["content"], "file body");
    }

    #[test]
    fn empty_tool_args_serialize_as_an_object_not_a_string() {
        // A zero-argument tool call streams no input_json_delta, so `arguments`
        // is "". Anthropic rejects `"input": ""` (string) — it must be `{}`.
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "list_agents".into(),
                    arguments: String::new(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("go"), assistant],
            &[],
        );
        let input = &body["messages"][1]["content"][0]["input"];
        assert!(
            input.is_object(),
            "empty args must be an object, got {input}"
        );
        assert_eq!(input.as_object().unwrap().len(), 0);
    }

    #[test]
    fn consecutive_tool_results_coalesce_into_one_user_message() {
        let msgs = vec![
            ChatMessage::tool_result("t1", "one"),
            ChatMessage::tool_result("t2", "two"),
        ];
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &msgs,
            &[],
        );
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn tools_carry_input_schema() {
        let tools = vec![ToolDef::function(
            "read",
            "read a file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )];
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("go")],
            &tools,
        );
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn ephemeral_places_breakpoints_on_system_tools_and_last_message() {
        let tools = vec![ToolDef::function("read", "d", json!({ "type": "object" }))];
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Ephemeral,
            false,
            &[sys("s"), user("hi")],
            &tools,
        );
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        let m = body["messages"].as_array().unwrap();
        let last = m.last().unwrap();
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn off_places_no_breakpoints() {
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &[sys("s"), user("hi")],
            &[],
        );
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn effort_enables_thinking_and_suppresses_temperature() {
        // No effort → no thinking; temperature passes through.
        let plain = build_body(
            "claude",
            8192,
            None,
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("hi")],
            &[],
        );
        assert!(plain.get("thinking").is_none());
        let t = plain["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6, "temperature {t}");

        // An effort level → thinking budget within max_tokens, temperature omitted.
        let think = build_body(
            "claude",
            8192,
            Some("high"),
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("hi")],
            &[],
        );
        assert_eq!(think["thinking"]["type"], "enabled");
        let budget = think["thinking"]["budget_tokens"].as_u64().unwrap() as u32;
        assert!((1024..8192).contains(&budget), "budget {budget}");
        assert!(think.get("temperature").is_none());
    }

    #[test]
    fn top_p_and_stop_sequences_map_onto_messages_api() {
        // top_p is sent when thinking is off (temperature path).
        let body = build_body(
            "claude",
            8192,
            None,
            None,
            Some(0.5),
            &["STOP".to_string(), "END".to_string()],
            CacheMode::Off,
            false,
            &[user("hi")],
            &[],
        );
        let p = body["top_p"].as_f64().unwrap();
        assert!((p - 0.5).abs() < 1e-6, "top_p {p}");
        assert_eq!(body["stop_sequences"], json!(["STOP", "END"]));

        // top_p is withheld while extended thinking is enabled (Anthropic
        // forbids setting it alongside `thinking`), same as temperature.
        let thinking_body = build_body(
            "claude",
            8192,
            Some("high"),
            None,
            Some(0.5),
            &[],
            CacheMode::Off,
            false,
            &[user("hi")],
            &[],
        );
        assert!(thinking_body.get("top_p").is_none());

        // No stop sequences configured → key omitted entirely.
        let no_stop = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &[user("hi")],
            &[],
        );
        assert!(no_stop.get("stop_sequences").is_none());
    }

    #[test]
    fn malformed_tool_call_arguments_preserved_as_string_not_emptied() {
        // A non-JSON `arguments` string must not be silently rewritten to `{}`
        // (which would erase the model's original intent from history); it is
        // preserved as a JSON string value instead.
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            origin: MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_bad".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "not valid json".into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let blocks = assistant_blocks(&assistant);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"], json!("not valid json"));
    }

    #[test]
    fn input_json_delta_for_unknown_block_index_is_ignored() {
        // A content_block_delta arriving for an index that never got a
        // content_block_start (so `tool_slot` has no entry) must be dropped,
        // not routed to tool slot 0 (which would corrupt an unrelated tool's
        // arguments with a stray fragment).
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        // No content_block_start recorded for index 5.
        let ev = json!({"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"x\""}});
        let out = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap();
        assert!(
            out.is_none(),
            "unknown block index must be dropped, not routed to slot 0"
        );
    }

    #[test]
    fn thinking_budget_scales_and_guards_small_windows() {
        assert_eq!(thinking_budget(None, 8192), None); // no effort → off
        assert_eq!(thinking_budget(Some("nonsense"), 8192), None); // unknown → off
        // Scales with max_tokens.
        let small = thinking_budget(Some("high"), 8192).unwrap();
        let big = thinking_budget(Some("high"), 32000).unwrap();
        assert!(big > small);
        // Budget always leaves ≥1024 for the answer and is ≥1024 itself.
        assert!((1024..=8192 - 1024).contains(&small));
        // A window too small to fit a budget + answer → thinking off.
        assert_eq!(thinking_budget(Some("high"), 1500), None);
    }

    #[test]
    fn thinking_blocks_captured_and_emitted_first_in_assistant_blocks() {
        // Simulate a streaming sequence: thinking_delta → signature_delta → tool_use
        // The accumulated thinking block must appear first in assistant_blocks.
        use crate::types::{FunctionCall, ToolCall};

        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        // content_block_start: thinking block at index 0
        let ev = json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}});
        assert!(
            map_event(
                &ev,
                &mut slot,
                &mut next,
                &mut thinking,
                &mut redacted,
                &mut stop_seen
            )
            .unwrap()
            .is_none()
        );

        // thinking_delta
        let ev = json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I should call read"}});
        let chunk = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("I should call read")
        );

        // signature_delta
        let ev = json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG123"}});
        assert!(
            map_event(
                &ev,
                &mut slot,
                &mut next,
                &mut thinking,
                &mut redacted,
                &mut stop_seen
            )
            .unwrap()
            .is_none()
        );

        // content_block_start: tool_use at index 1
        let ev = json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_x","name":"read"}});
        map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap();

        // Verify the thinking block accumulated properly
        assert_eq!(thinking.get(&0).unwrap().0, "I should call read");
        assert_eq!(thinking.get(&0).unwrap().1, "SIG123");

        // Simulate assistant_blocks with a ChatMessage that has the thinking blocks stored
        let msg = crate::types::ChatMessage {
            role: crate::types::Role::Assistant,
            content: None,
            reasoning_content: Some("I should call read".into()),
            anthropic_thinking_blocks: vec![
                json!({"type":"thinking","thinking":"I should call read","signature":"SIG123"}),
            ],
            origin: crate::types::MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_x".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"x"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let blocks = assistant_blocks(&msg);
        // Thinking block must be first
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "I should call read");
        assert_eq!(blocks[0]["signature"], "SIG123");
        // tool_use comes after
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "toolu_x");
    }

    #[test]
    fn maps_text_and_tool_stream_events() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;
        // message_start → prompt usage (incl cache counters).
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}});
        let c = map_event(
            &start,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.usage.unwrap().prompt_tokens, 15);
        // text delta.
        let td = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}});
        let c = map_event(
            &td,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hi"));
        // tool_use start at anthropic block index 1 → flat tool index 0.
        let ts = json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"read"}});
        let c = map_event(
            &ts,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        let tc = &c.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("toolu_9"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("read"));
        // input_json_delta on block index 1 routes to flat index 0.
        let jd = json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}});
        let c = map_event(
            &jd,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        let tc = &c.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"path\"")
        );
        // message_delta → completion usage.
        let md = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}});
        let c = map_event(
            &md,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.usage.unwrap().completion_tokens, 42);
        // ping → nothing.
        assert!(
            map_event(
                &json!({"type":"ping"}),
                &mut slot,
                &mut next,
                &mut thinking,
                &mut redacted,
                &mut stop_seen
            )
            .unwrap()
            .is_none()
        );
        // error → Err.
        assert!(
            map_event(
                &json!({"type":"error","error":{"message":"boom"}}),
                &mut slot,
                &mut next,
                &mut thinking,
                &mut redacted,
                &mut stop_seen,
            )
            .is_err()
        );
    }

    #[test]
    fn thinking_block_signature_survives_full_build_body_round_trip() {
        // End-to-end regression for the Anthropic interleaved-thinking protocol.
        //
        // Anthropic requires that when an assistant turn contains both a `thinking`
        // block and a `tool_use` block, the thinking block (with its opaque
        // `signature`) appears **first** in the assistant message's `content`
        // array on the follow-up request. If `assistant_blocks` were to reorder or
        // drop the thinking block, the API would return a 400. This test drives
        // the full `build_body` → `split_system_and_messages` → `assistant_blocks`
        // path and asserts the final wire representation.
        //
        // Approach: construct a `ChatMessage` that already holds the accumulated
        // `anthropic_thinking_blocks` (as the `Accumulator::into_message` would
        // produce after a streaming turn), feed it through `build_body`, and check
        // the serialized JSON body rather than individual helper functions.
        use crate::types::{FunctionCall, ToolCall};

        // Simulate the assistant message that the Accumulator produces after a
        // streaming turn that emitted thinking_delta + signature_delta + tool_use.
        let assistant_msg = crate::types::ChatMessage {
            role: crate::types::Role::Assistant,
            content: None,
            reasoning_content: Some("I should call read".into()),
            anthropic_thinking_blocks: vec![json!({
                "type": "thinking",
                "thinking": "I should call read",
                "signature": "SIG_ROUND_TRIP"
            })],
            origin: crate::types::MessageOrigin::User,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_rt".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"Cargo.toml"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let tool_result = crate::types::ChatMessage::tool_result("toolu_rt", "content");

        let history = vec![user("go"), assistant_msg, tool_result];
        let body = build_body(
            "claude-opus",
            4096,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            &history,
            &[],
        );

        let messages = body["messages"].as_array().expect("messages array");
        // History (no system): user, assistant, user(tool_result) → 3 messages.
        assert_eq!(messages.len(), 3);

        // The assistant message is at index 1.
        let asst = &messages[1];
        assert_eq!(asst["role"], "assistant");
        let blocks = asst["content"].as_array().expect("assistant content array");

        // First block must be the thinking block with the signature intact.
        assert_eq!(
            blocks[0]["type"], "thinking",
            "thinking block must be first; blocks: {blocks:?}"
        );
        assert_eq!(
            blocks[0]["thinking"], "I should call read",
            "thinking text must survive build_body"
        );
        assert_eq!(
            blocks[0]["signature"], "SIG_ROUND_TRIP",
            "signature must survive build_body unchanged"
        );

        // Second block must be the tool_use.
        assert_eq!(
            blocks[1]["type"], "tool_use",
            "tool_use must follow thinking; blocks: {blocks:?}"
        );
        assert_eq!(blocks[1]["id"], "toolu_rt");
        assert_eq!(blocks[1]["name"], "read");
        assert_eq!(blocks[1]["input"]["path"], "Cargo.toml");

        // anthropic_thinking_blocks must NOT appear as a top-level key in the
        // message object (it is an internal hrdr field, not an Anthropic wire key).
        assert!(
            asst.get("anthropic_thinking_blocks").is_none(),
            "anthropic_thinking_blocks must not be a top-level message key"
        );
    }

    #[test]
    fn rate_limit_error_is_transient() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        let ev =
            json!({"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}});
        let err = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(
            chat_err.kind,
            crate::client::ChatErrorKind::Transient,
            "rate_limit_error must be transient"
        );
        assert!(chat_err.message.contains("rate_limit_error"));
        assert!(chat_err.message.contains("Rate limited"));
    }

    #[test]
    fn overloaded_error_is_transient() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        let ev = json!({"type":"error","error":{"type":"overloaded_error","message":"Server overloaded"}});
        let err = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(
            chat_err.kind,
            crate::client::ChatErrorKind::Transient,
            "overloaded_error must be transient"
        );
        assert!(chat_err.message.contains("overloaded_error"));
        assert!(chat_err.message.contains("Server overloaded"));
    }

    #[test]
    fn api_error_is_transient() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        // `api_error` is Anthropic's 500-equivalent — retryable, like overload.
        let ev =
            json!({"type":"error","error":{"type":"api_error","message":"Internal server error"}});
        let err = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(
            chat_err.kind,
            crate::client::ChatErrorKind::Transient,
            "api_error must be transient"
        );
        assert!(chat_err.message.contains("api_error"));
    }

    #[test]
    fn other_anthropic_error_is_terminal() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        // An `invalid_request_error` must be classified as terminal (Other).
        let ev = json!({"type":"error","error":{"type":"invalid_request_error","message":"bad request"}});
        let err = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(
            chat_err.kind,
            crate::client::ChatErrorKind::Other,
            "invalid_request_error must be terminal"
        );
        assert!(chat_err.message.contains("invalid_request_error"));
        assert!(chat_err.message.contains("bad request"));

        // An error with no type field must also be terminal.
        let ev = json!({"type":"error","error":{"message":"generic"}});
        let err = map_event(
            &ev,
            &mut slot,
            &mut next,
            &mut thinking,
            &mut redacted,
            &mut stop_seen,
        )
        .unwrap_err();
        let chat_err = err.downcast_ref::<crate::client::ChatError>().unwrap();
        assert_eq!(
            chat_err.kind,
            crate::client::ChatErrorKind::Other,
            "no-type error must be terminal"
        );
        assert!(chat_err.message.contains("generic"));
    }
}
