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
//! **silently drops** `cache_control`, so prompt caching (and extended thinking)
//! are only reachable here. Scope of this backend: system + messages + tools +
//! streaming + prompt caching. Extended thinking is a follow-up.
//!
//! [`Accumulator`]: crate::Accumulator

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::types::{
    CacheMode, ChatChunk, ChatMessage, ChunkChoice, Delta, FunctionDelta, Role, ToolCallDelta,
    ToolDef, Usage,
};

/// Anthropic API version pinned in the `anthropic-version` header.
const API_VERSION: &str = "2023-06-01";

/// Build the native `/v1/messages` request body from hrdr's OpenAI-shaped
/// history. When `cache == Ephemeral`, `cache_control` breakpoints are placed on
/// the last system block, the last tool, and the last content block of the last
/// message (Anthropic allows ≤4; we use ≤3).
pub(crate) fn build_body(
    model: &str,
    max_tokens: u32,
    messages: &[ChatMessage],
    tools: &[ToolDef],
    cache: CacheMode,
) -> Value {
    let ephemeral = cache == CacheMode::Ephemeral;
    let (system, msgs) = split_system_and_messages(messages);

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": msgs,
        "stream": true,
    });

    if !system.is_empty() {
        let mut blocks = system;
        if ephemeral {
            mark_last_block(&mut blocks);
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
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body["tools"] = Value::Array(defs);
    }

    // Rolling cache breakpoint on the last content block of the last message.
    if ephemeral
        && let Some(last) = body["messages"].as_array_mut().and_then(|m| m.last_mut())
        && let Some(blocks) = last.get_mut("content").and_then(|c| c.as_array_mut())
    {
        mark_last_block(blocks);
    }

    body
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

/// Assistant turn → optional leading `text` block + one `tool_use` block per
/// call (arguments parsed from the JSON string; `{}` if unparseable).
fn assistant_blocks(m: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(t) = &m.content
        && !t.is_empty()
    {
        blocks.push(json!({ "type": "text", "text": t }));
    }
    for call in m.tool_calls.iter().flatten() {
        let input: Value =
            serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.function.name,
            "input": input,
        }));
    }
    blocks
}

/// Tag the last block in a block array with an ephemeral cache breakpoint.
fn mark_last_block(blocks: &mut [Value]) {
    if let Some(last) = blocks.last_mut()
        && let Some(obj) = last.as_object_mut()
    {
        obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
}

/// Stream a completion from the native Messages API, yielding OpenAI-shaped
/// [`ChatChunk`]s.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_stream(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    cache: CacheMode,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDef>,
) -> Result<(Value, crate::ChatStream)> {
    let body = build_body(model, max_tokens, &messages, &tools, cache);
    let mut req = http
        .post(format!("{base_url}/messages"))
        .header("anthropic-version", API_VERSION)
        .json(&body);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    let resp = req.send().await.context("chat stream request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("chat endpoint returned {status}: {text}");
    }

    let stream = async_stream::try_stream! {
        let mut bytes = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        // Anthropic content-block index → our flat tool-call index.
        let mut tool_slot: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        let mut next_tool: usize = 0;
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.context("reading stream chunk")?;
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line[..nl]);
                let line = line.trim_end_matches('\r');
                // Anthropic SSE carries `event:` and `data:` lines; every `data:`
                // payload is a complete JSON object with its own `type`, so the
                // `event:` line is redundant and ignored.
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data.is_empty() { continue }
                let ev: Value = serde_json::from_str(data)
                    .with_context(|| format!("decoding stream event: {data}"))?;
                if let Some(out) = map_event(&ev, &mut tool_slot, &mut next_tool)? {
                    yield out;
                }
            }
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
) -> Result<Option<ChatChunk>> {
    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "message_start" => {
            let u = ev.get("message").and_then(|m| m.get("usage"));
            Ok(Some(usage_chunk(prompt_tokens(u), 0)))
        }
        "content_block_start" => {
            let block = ev.get("content_block");
            if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
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
                        .and_then(Value::as_str);
                    Ok(t.map(|t| reasoning_chunk(t.to_string())))
                }
                Some("input_json_delta") => {
                    let frag = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let slot = tool_slot.get(&idx).copied().unwrap_or(0);
                    Ok(Some(tool_call_chunk(
                        slot,
                        None,
                        None,
                        Some(frag.to_string()),
                    )))
                }
                _ => Ok(None),
            }
        }
        "message_delta" => {
            let out = ev
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            Ok((out > 0).then(|| usage_chunk(0, out)))
        }
        "error" => {
            let msg = ev
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("anthropic stream error: {msg}")
        }
        _ => Ok(None), // ping, content_block_stop, message_stop
    }
}

/// Anthropic `input_tokens` plus the two cache counters (so the status bar's
/// prompt-token figure reflects what was actually billed/cached).
fn prompt_tokens(usage: Option<&Value>) -> u32 {
    let get = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    (get("input_tokens") + get("cache_read_input_tokens") + get("cache_creation_input_tokens"))
        as u32
}

fn usage_chunk(prompt_tokens: u32, completion_tokens: u32) -> ChatChunk {
    ChatChunk {
        choices: vec![],
        usage: Some(Usage {
            prompt_tokens,
            completion_tokens,
        }),
    }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    fn sys(t: &str) -> ChatMessage {
        ChatMessage::system(t)
    }
    fn user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
    }

    #[test]
    fn system_is_hoisted_and_messages_alternate() {
        let msgs = vec![sys("you are hrdr"), user("hi"), user("still me")];
        let body = build_body("claude", 1024, &msgs, &[], CacheMode::Off);
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
            &[user("go"), assistant, result],
            &[],
            CacheMode::Off,
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
    fn consecutive_tool_results_coalesce_into_one_user_message() {
        let msgs = vec![
            ChatMessage::tool_result("t1", "one"),
            ChatMessage::tool_result("t2", "two"),
        ];
        let body = build_body("claude", 256, &msgs, &[], CacheMode::Off);
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
        let body = build_body("claude", 256, &[user("go")], &tools, CacheMode::Off);
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
            &[sys("s"), user("hi")],
            &tools,
            CacheMode::Ephemeral,
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
        let body = build_body("claude", 256, &[sys("s"), user("hi")], &[], CacheMode::Off);
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn maps_text_and_tool_stream_events() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        // message_start → prompt usage (incl cache counters).
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}});
        let c = map_event(&start, &mut slot, &mut next).unwrap().unwrap();
        assert_eq!(c.usage.unwrap().prompt_tokens, 15);
        // text delta.
        let td = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}});
        let c = map_event(&td, &mut slot, &mut next).unwrap().unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hi"));
        // tool_use start at anthropic block index 1 → flat tool index 0.
        let ts = json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"read"}});
        let c = map_event(&ts, &mut slot, &mut next).unwrap().unwrap();
        let tc = &c.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("toolu_9"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("read"));
        // input_json_delta on block index 1 routes to flat index 0.
        let jd = json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}});
        let c = map_event(&jd, &mut slot, &mut next).unwrap().unwrap();
        let tc = &c.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"path\"")
        );
        // message_delta → completion usage.
        let md = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}});
        let c = map_event(&md, &mut slot, &mut next).unwrap().unwrap();
        assert_eq!(c.usage.unwrap().completion_tokens, 42);
        // ping → nothing.
        assert!(
            map_event(&json!({"type":"ping"}), &mut slot, &mut next)
                .unwrap()
                .is_none()
        );
        // error → Err.
        assert!(
            map_event(
                &json!({"type":"error","error":{"message":"boom"}}),
                &mut slot,
                &mut next
            )
            .is_err()
        );
    }
}
