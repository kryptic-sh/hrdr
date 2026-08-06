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
//! last tool, and the last message), and **thinking** (a reasoning `effort`
//! level selects one of the two thinking dialects Anthropic speaks — see
//! [`thinking_config`] — and `thinking_delta`s stream to hrdr's reasoning
//! channel).
//!
//! [`Accumulator`]: crate::Accumulator

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::sse::SseDecoder;

use crate::types::{
    CacheMode, ChatChunk, ChatMessage, ChunkChoice, Delta, Role, TokenDetails, ToolDef, Usage,
    reasoning_chunk, text_chunk, tool_call_chunk,
};

/// Anthropic API version pinned in the `anthropic-version` header.
pub(crate) const API_VERSION: &str = "2023-06-01";

/// Build the native `/v1/messages` request body from hrdr's OpenAI-shaped
/// history. When `cache == Ephemeral`, `cache_control` breakpoints are placed on
/// the last tool, the last system block, the last content block of the last
/// message, and — when `system_cache_split` gives a boundary — the end of the
/// system prompt's stable prefix (Anthropic allows ≤4; with a split we use all 4).
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
    system_cache_split: Option<usize>,
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

    // Thinking. `display: "summarized"` is explicit on the *adaptive* dialect
    // only: there it defaults to `"omitted"`, so thinking blocks come back
    // signed but with an empty `thinking` field and hrdr's reasoning pane stays
    // blank for the whole turn. On the manual dialect — Opus/Sonnet 4.6 and
    // earlier — `"summarized"` is already the default, so sending it buys
    // nothing on models old enough that the field may predate them.
    //
    // Sampling params ride only on the no-thinking path, and only on models that
    // still accept them at all (see `sampling_locked`): Anthropic forbids
    // `temperature`/`top_k` alongside manual thinking, and the current
    // generation rejects them outright.
    match thinking_config(model, effort, max_tokens) {
        ThinkingShape::Adaptive { effort } => {
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            if let Some(e) = effort {
                body["output_config"] = json!({ "effort": e.as_str() });
            }
        }
        ThinkingShape::Manual { budget, effort } => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            if let Some(e) = effort {
                body["output_config"] = json!({ "effort": e.as_str() });
            }
        }
        ThinkingShape::Off if !sampling_locked(model) => {
            if let Some(t) = temperature {
                body["temperature"] = json!(t);
            }
            if let Some(p) = top_p {
                body["top_p"] = json!(p);
            }
        }
        ThinkingShape::Off => {}
    }

    if !stop.is_empty() {
        body["stop_sequences"] = json!(stop);
    }

    if !system.is_empty() {
        let mut blocks = split_system_for_cache(system, system_cache_split);
        if ephemeral {
            // Two breakpoints when the caller gave a boundary: one closing the
            // stable prefix (everything up to the environment block) and the
            // rolling one at the end. Sibling write sub-agents share a persona
            // but a scoped sibling can have a narrower `cwd`, so the tail can differ while
            // everything above it does not — without the first breakpoint that
            // shared prefix is re-sent for every one of them.
            if blocks.len() > 1 {
                blocks[0]["cache_control"] = crate::types::cache_control(ttl_1h);
            }
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

/// Upper bound on a manual thinking budget. The docs warn that budgets above
/// 32k should move to batch processing because the request otherwise runs long
/// enough to hit timeouts — and with `max_tokens` now sized from the model's
/// real output cap (64k–128k), the old `0.75 × max_tokens` alone would ask for
/// a ~96k-token budget on a single streamed turn.
const MAX_THINKING_BUDGET: u32 = 32_768;

/// One of Anthropic's `output_config.effort` levels. Ordered so a model's
/// ceiling can be applied with a comparison; note `Xhigh` is *not* simply below
/// `Max` in support terms — Opus/Sonnet 4.6 took `max` a release before `xhigh`
/// existed, which is why [`EffortSupport`] is an enum rather than a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// Which `output_config.effort` values a model accepts. Sending one it doesn't
/// know is a 400, so an unsupported level is clamped down rather than dropped —
/// the user asked for *more* thinking, and the nearest supported level honours
/// that better than silently reverting to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortSupport {
    /// No `output_config.effort` at all (Sonnet/Haiku 4.5, Opus 4.1, Claude 3).
    None,
    /// `low`/`medium`/`high` only — Opus 4.5, which takes effort *and*
    /// `budget_tokens` but predates the two top levels.
    UpToHigh,
    /// Everything except `xhigh`, which arrived after these shipped
    /// (Opus 4.6, Sonnet 4.6).
    NoXhigh,
    /// The full ladder (Opus 4.7 and later, Sonnet 5, Fable 5, Mythos 5, …).
    All,
}

impl EffortSupport {
    /// `want` clamped to what this model accepts, or `None` when it has no
    /// effort knob at all.
    fn clamp(self, want: Effort) -> Option<Effort> {
        match self {
            EffortSupport::None => None,
            EffortSupport::UpToHigh => Some(want.min(Effort::High)),
            // `max` survives; only `xhigh` has no landing spot here.
            EffortSupport::NoXhigh if want == Effort::Xhigh => Some(Effort::High),
            EffortSupport::NoXhigh | EffortSupport::All => Some(want),
        }
    }
}

/// The thinking configuration a (model, effort, max_tokens) triple calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingShape {
    /// `thinking:{type:"adaptive",display:"summarized"}` plus a separate
    /// top-level `output_config:{effort}` when the level maps to something.
    Adaptive { effort: Option<Effort> },
    /// The pre-4.6 dialect: `thinking:{type:"enabled",budget_tokens}`.
    Manual { budget: u32, effort: Option<Effort> },
    /// No thinking config on the request at all.
    Off,
}

/// Decide how to ask `model` to think.
///
/// Anthropic has two mutually incompatible thinking dialects and the split is by
/// model generation, not by preference:
///
/// * **adaptive** — `thinking:{type:"adaptive"}` with depth steered by a
///   separate top-level `output_config:{effort}`. The only dialect Opus 4.7 and
///   later accept; `type:"enabled"` is a **400** there. On Opus 4.6/4.7/4.8 and
///   Sonnet 4.6 thinking is off until this is sent, so hrdr sends the adaptive
///   object even when no effort level is configured.
/// * **manual** — `thinking:{type:"enabled",budget_tokens}`, the only dialect
///   Sonnet/Opus/Haiku 4.5, Opus 4.1 and the Claude 3 family understand. Opus
///   4.5 alone takes an `output_config.effort` alongside the budget.
///
/// Unknown ids — including provider-prefixed (`anthropic/claude-opus-5`) and
/// not-yet-released ones — default to **adaptive**: the manual-only set is a
/// closed, shrinking list of shipped models, while every new model rejects
/// `enabled`, so guessing adaptive is the forward-compatible guess.
///
/// `effort` is hrdr's own ladder; `none`/an unrecognized label means "no
/// thinking" and yields [`ThinkingShape::Off`] on every model, which is also
/// what a manual-only model gets when no effort is configured (there is no
/// budget to compute).
pub(crate) fn thinking_config(model: &str, effort: Option<&str>, max_tokens: u32) -> ThinkingShape {
    let ModelCaps {
        adaptive, support, ..
    } = classify(model);
    // `None` = the caller configured no effort at all (adaptive stays on, at the
    // model's default depth); `Some(None)` = an explicit "none"/unknown label,
    // which turns thinking off entirely.
    let want: Option<Option<Effort>> = effort.map(map_effort);
    match (adaptive, want) {
        (_, Some(None)) => ThinkingShape::Off,
        (true, None) => ThinkingShape::Adaptive { effort: None },
        (true, Some(Some(want))) => ThinkingShape::Adaptive {
            effort: support.clamp(want),
        },
        (false, None) => ThinkingShape::Off,
        (false, Some(Some(want))) => match thinking_budget(want, max_tokens) {
            Some(budget) => ThinkingShape::Manual {
                budget,
                effort: support.clamp(want),
            },
            // A `max_tokens` window too small to fit a budget plus room for the
            // answer: thinking off rather than a rejected request.
            None => ThinkingShape::Off,
        },
    }
}

/// hrdr's effort ladder → Anthropic's. `minimal` has no Anthropic equivalent and
/// folds into `low`; `high` is Anthropic's own default. `none` (and any label
/// [`crate::normalize_effort`] doesn't know) means no thinking at all.
fn map_effort(label: &str) -> Option<Effort> {
    match crate::normalize_effort(label)?.as_str() {
        "minimal" | "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        // "none" — and nothing else reaches here.
        _ => None,
    }
}

/// What one model id supports, as far as this module cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelCaps {
    /// Speaks the adaptive thinking dialect rather than `budget_tokens`.
    adaptive: bool,
    /// Which `output_config.effort` levels it accepts.
    support: EffortSupport,
    /// Rejects any non-default `temperature`/`top_p`/`top_k` outright — see
    /// [`sampling_locked`].
    sampling_locked: bool,
}

/// Whether `model` refuses non-default sampling parameters **unconditionally**.
///
/// Two different rules are in play. On Fable 5, Mythos 5/Preview, Opus 5, Opus
/// 4.8, Opus 4.7 and Sonnet 5 a non-default `temperature`, `top_p` or `top_k` is
/// a **400 on every request**, thinking or not — so those parameters can never
/// go out, and hrdr drops them rather than failing the turn. On everything older
/// (Opus 4.6 and Sonnet 4.6 included) the restriction is only "while thinking is
/// on", which [`build_body`]'s existing rule — sampling params only when no
/// thinking config is sent — already covers.
///
/// Unknown and future ids count as **locked**, for the same reason unknown ids
/// default to adaptive: the locked set only grows, and a silently dropped
/// sampling parameter is a quality nudge while a 400 is a dead turn.
fn sampling_locked(model: &str) -> bool {
    classify(model).sampling_locked
}

/// Classify a model id.
///
/// Matching is on the id's own shape — `claude-<family>-<major>[-<minor>]` —
/// after stripping any `provider/` prefix and lowercasing, so both
/// `claude-opus-4-5` and `anthropic/claude-opus-4-5-20251101` land in the same
/// bucket. A trailing date (`claude-opus-4-20250514`) is not a minor version:
/// numeric segments of three digits or more are treated as snapshot dates, which
/// is what keeps Claude 4.0 out of the "4.6 or later" branch.
fn classify(model: &str) -> ModelCaps {
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();

    // Claude 3 is versioned `claude-3[-<minor>]-<family>`, the other way round,
    // and is uniformly manual-only with no effort knob and no sampling lock.
    if id.starts_with("claude-3") {
        return ModelCaps {
            adaptive: false,
            support: EffortSupport::None,
            sampling_locked: false,
        };
    }

    let Some((major, minor)) = model_version(&id) else {
        // Not a shape we recognize (an alias, a gateway's renaming, a model that
        // does not exist yet — `claude-mythos-preview` lands here too): assume
        // the current generation's rules on both axes.
        return ModelCaps {
            adaptive: true,
            support: EffortSupport::All,
            sampling_locked: true,
        };
    };

    let (adaptive, support) = match (major, minor) {
        // Claude 4.6: adaptive, and the first with `output_config.effort` up to
        // `max` — but `xhigh` only landed with 4.7.
        (4, 6) => (true, EffortSupport::NoXhigh),
        // Opus 4.5 takes effort (low/medium/high) but only manual thinking.
        (4, 5) if id.contains("opus") => (false, EffortSupport::UpToHigh),
        // The rest of Claude 4 below 4.6 — Sonnet/Haiku 4.5, 4.1, 4.0, and any
        // other 4.x snapshot — is manual-only with no effort control.
        (4, m) if m < 6 => (false, EffortSupport::None),
        // 4.7+ and everything from Claude 5 on.
        _ => (true, EffortSupport::All),
    };
    ModelCaps {
        adaptive,
        support,
        // The sampling lock arrived one release *after* adaptive thinking did:
        // 4.6 still takes sampling params when thinking is off, 4.7 and later
        // never do.
        sampling_locked: major > 4 || (major == 4 && minor >= 7),
    }
}

/// `(major, minor)` from a `claude-<family>-<major>[-<minor>]` id, with an
/// absent minor reading as `0` (`claude-opus-5` is 5.0). `None` when the id
/// doesn't carry a recognizable family + version pair.
fn model_version(id: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = id.split('-').collect();
    let family = parts
        .iter()
        .position(|p| matches!(*p, "opus" | "sonnet" | "haiku" | "fable" | "mythos"))?;
    // A version segment is one or two digits; three or more is a snapshot date
    // (`20250514`), which must not be mistaken for a minor version.
    let version = |i: usize| -> Option<u32> {
        let s = *parts.get(i)?;
        (s.len() <= 2).then(|| s.parse().ok()).flatten()
    };
    Some((version(family + 1)?, version(family + 2).unwrap_or(0)))
}

/// Manual-mode thinking budget (tokens) for an effort level, or `None` when the
/// `max_tokens` window is too small to fit a budget plus room for the answer.
/// The budget scales with `max_tokens` so raising the output cap gives Claude
/// more room to think, bounded by [`MAX_THINKING_BUDGET`]; Anthropic requires
/// `budget ≥ 1024` and `budget < max_tokens`.
fn thinking_budget(effort: Effort, max_tokens: u32) -> Option<u32> {
    let frac = match effort {
        Effort::Low => 0.40,
        Effort::Medium => 0.60,
        Effort::High => 0.75,
        Effort::Xhigh => 0.85,
        Effort::Max => 0.95,
    };
    // Reserve at least 1024 tokens below `max_tokens` for the actual answer.
    let ceiling = max_tokens
        .checked_sub(1024)
        .filter(|c| *c >= 1024)?
        .min(MAX_THINKING_BUDGET);
    Some(((max_tokens as f64 * frac) as u32).clamp(1024, ceiling))
}

/// Split hrdr history into Anthropic `system` blocks + `messages`. Consecutive
/// same-role messages (e.g. a run of tool results) are coalesced into one
/// message, since Anthropic requires alternating user/assistant turns and tool
/// results to ride in a `user` message.
/// Split the assembled system prompt into `[stable prefix, volatile tail]` at
/// `at` bytes, so a cache breakpoint can close the prefix.
///
/// A no-op — one block, as before — when there is no boundary, when it lands
/// outside the text, or when it is not a char boundary (it always is: it is a
/// sum of section lengths, but slicing on a bad index would panic and a
/// mis-cached prompt is not worth that).
fn split_system_for_cache(system: Vec<Value>, at: Option<usize>) -> Vec<Value> {
    let Some(at) = at else { return system };
    // Only meaningful for the single assembled system message; anything else is
    // left alone rather than guessed at.
    if system.len() != 1 {
        return system;
    }
    let Some(text) = system[0].get("text").and_then(|t| t.as_str()) else {
        return system;
    };
    if at == 0 || at >= text.len() || !text.is_char_boundary(at) {
        return system;
    }
    vec![
        json!({ "type": "text", "text": &text[..at] }),
        json!({ "type": "text", "text": &text[at..] }),
    ]
}

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
/// into the returned [`crate::ChatStream`] future. Writes its own `request` /
/// `error_response` / `sse` wire-log records (see [`crate::client::log_wire`]).
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
    system_cache_split: Option<usize>,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<crate::ChatStream> {
    let body = build_body(
        model,
        max_tokens,
        effort,
        temperature,
        top_p,
        stop,
        cache,
        ttl_1h,
        system_cache_split,
        messages,
        tools,
    );
    let url = format!("{base_url}/messages");
    let mut req = http
        .post(&url)
        .header("anthropic-version", API_VERSION)
        .json(&body);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    // Auth-type names are filtered out here, so `x-api-key` above stays the only
    // credential on the request (see `crate::client::apply_extra_headers`).
    req = crate::client::apply_extra_headers(req, extra_headers);
    let betas = beta_headers(&body, !tools.is_empty(), cache, ttl_1h);
    if !betas.is_empty() {
        req = req.header("anthropic-beta", betas.join(","));
    }
    // Log before the send, not after: the round-trip and the status check below
    // both happen here, so logging afterwards would miss exactly the requests
    // the wire log exists to explain (a 401, a 400 on a malformed tool block).
    // Only the body goes in — the credential is a header (`x-api-key`), and
    // `build_body` never sees it.
    crate::client::log_wire("request", || json!({"url": url, "body": body}));
    let resp = req.send().await.context("chat stream request failed")?;
    if !resp.status().is_success() {
        return Err(crate::client::error_from_response(resp).await);
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
                let data = &sse_ev.data;
                if data.is_empty() { continue; }
                // Raw line, before parsing: a payload we fail to decode is the
                // one worth having in the log.
                crate::client::log_wire("sse", || json!({"data": data}));
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
                responses_reasoning_items: vec![],
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
    Ok(Box::pin(stream))
}

/// `anthropic-beta` values for a request, given the body [`build_body`] produced.
///
/// * interleaved thinking — reasoning *between* tool calls — is a beta only on
///   the **manual** dialect. Adaptive thinking interleaves on its own and the
///   header is unneeded (and ignored) there, so it rides only on `enabled`.
/// * the 1-hour cache TTL no longer requires its beta, but the header remains
///   harmless; it is kept so this fix doesn't quietly change caching too.
fn beta_headers(
    body: &Value,
    has_tools: bool,
    cache: CacheMode,
    ttl_1h: bool,
) -> Vec<&'static str> {
    let mut betas = Vec::new();
    let manual = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("enabled");
    if manual && has_tools {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if ttl_1h && cache == CacheMode::Ephemeral {
        betas.push("extended-cache-ttl-2025-04-11");
    }
    betas
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
            let usage = ev.get("usage");
            let out = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                .unwrap_or(0);
            // Anthropic reports thinking spend only here, on the final
            // `message_delta`, nested a level deeper than the totals. It is part
            // of `output_tokens`, so it maps onto the OpenAI-shaped
            // `completion_tokens_details.reasoning_tokens` the UI already reads
            // for the OpenAI/Codex backends.
            let reasoning = usage
                .and_then(|u| u.get("output_tokens_details"))
                .and_then(|d| d.get("thinking_tokens"))
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
            let finish = ev
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(|stop| match map_stop_reason(stop) {
                    Some(mapped) => mapped.to_string(),
                    None => {
                        // hrdr cannot know what a reason it has never seen means,
                        // and must not pretend it did: the value rides through
                        // verbatim (it carries the most information and invents
                        // no semantics), and the user is told by name that the
                        // reply may be incomplete. One-shot, through the same
                        // slot the turn loop already drains into a Notice.
                        crate::client::set_client_warning(format!(
                            "hrdr does not recognize Anthropic stop_reason \
                             `{stop}`, and the reply may be incomplete"
                        ));
                        stop.to_string()
                    }
                });
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
                usage: (out > 0 || reasoning.is_some()).then(|| Usage {
                    completion_tokens: out,
                    completion_tokens_details: TokenDetails {
                        reasoning_tokens: reasoning,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                anthropic_thinking_blocks: vec![],
                responses_reasoning_items: vec![],
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
                // Anthropic's retryable server-side error types (`api_error` is
                // their 500-equivalent; `rate_limit_error` is 429). A
                // rate_limit_error carrying a credit/quota message is a spent
                // billing cap, not a rate limit — permanent until the user tops
                // up, so it must not be retried for six minutes.
                "rate_limit_error" | "overloaded_error" | "api_error"
                    if crate::retry::is_usage_limit_text(msg) =>
                {
                    crate::client::ChatErrorKind::UsageLimit
                }
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

/// Map an Anthropic `stop_reason` to the OpenAI `finish_reason` vocabulary —
/// `None` when hrdr does not recognize the reason.
///
/// The `None` is the point. An unrecognized reason could mean anything,
/// including "the output was cut short", and [`crate::Accumulator::truncated`]
/// matches only `"length" | "max_tokens"` — so folding an unknown into `stop`
/// reports half an answer as a whole one, and folding it into `length` reports
/// a *finished* reply (a refusal, say) as truncated. Both are silent and wrong,
/// so the choice is handed back to the caller, which passes the value through
/// verbatim and warns the user by name (see [`map_event`]). Keeping the mapping
/// itself total and side-effect-free is what lets the table test cover it.
fn map_stop_reason(stop: &str) -> Option<&'static str> {
    match stop {
        // Both of these are the reply ending early — one at the requested
        // output cap, one at the model's context window — so both have to reach
        // `truncated()`, which is what makes the turn loop say so.
        "max_tokens" | "model_context_window_exceeded" => Some("length"),
        "tool_use" => Some("tool_calls"),
        "end_turn" | "stop_sequence" => Some("stop"),
        // A refusal is a *finished* response the safety classifiers declined,
        // not a cut-off one, so mapping it to `length` would be its own silent
        // lie. `content_filter` is the OpenAI-shaped word, and the Codex backend
        // already maps its own filter stop onto that same string — see
        // [`crate::codex`]'s `map_finish_reason`.
        "refusal" => Some("content_filter"),
        // Deliberately not an arm: `pause_turn`, which Anthropic emits when its
        // *server-side* tool loop pauses a turn a follow-up request resumes.
        // Every tool hrdr sends is a plain `{name, description, input_schema}`
        // definition it executes itself, so no server-side loop exists to pause,
        // and there is no OpenAI-shaped word for "resume me" to map it onto. If
        // one ever does arrive, the warning below is the honest answer.
        _ => None,
    }
}

/// Read a `u64` counter from an Anthropic usage object.
fn usage_field(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// A prompt-usage chunk from Anthropic's `message_start`: total prompt tokens
/// (`input` + both cache counters), with **both** cache portions surfaced —
/// reads as `cached_tokens`, writes as `cache_creation_input_tokens`.
///
/// `prompt_tokens` stays the inclusive total it has always been; the two cache
/// counters are a breakdown of it, not an addition to it. Both are needed
/// because the three parts are priced differently: a read is ~0.1x the input
/// rate, a write is a *premium* over it (1.25x at the 5-minute TTL, 2x at the
/// 1-hour one). Dropping the write counter — as this did before — priced every
/// cache write as plain input, and hrdr writes the cache on nearly every turn.
/// See [`crate::catalog::ModelCost::call_cost`].
fn message_start_usage(usage: Option<&Value>) -> ChatChunk {
    let cache_read = usage_field(usage, "cache_read_input_tokens");
    let cache_write = usage_field(usage, "cache_creation_input_tokens");
    let prompt = usage_field(usage, "input_tokens")
        .saturating_add(cache_read)
        .saturating_add(cache_write);
    let mut u = Usage {
        prompt_tokens: u32::try_from(prompt).unwrap_or(u32::MAX),
        ..Default::default()
    };
    if cache_read > 0 {
        u.prompt_tokens_details.cached_tokens = Some(u32::try_from(cache_read).unwrap_or(u32::MAX));
    }
    // Left `None` at zero, matching `cached_tokens` above: "the provider said
    // nothing" and "the provider said zero" must stay distinguishable, and a
    // request that wrote nothing has nothing to price.
    if cache_write > 0 {
        u.cache_creation_input_tokens = Some(u32::try_from(cache_write).unwrap_or(u32::MAX));
    }
    ChatChunk {
        choices: vec![],
        usage: Some(u),
        anthropic_thinking_blocks: vec![],
        responses_reasoning_items: vec![],
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
            None,
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
            responses_reasoning_items: vec![],
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
            None,
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
            responses_reasoning_items: vec![],
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
            None,
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
            None,
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
            None,
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
            None,
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

    /// A caller-supplied boundary splits the system prompt into a cached stable
    /// prefix and a volatile tail, and marks BOTH — the extra breakpoint is what
    /// lets sibling write sub-agents (same persona, possibly different `cwd`)
    /// stop re-sending everything above their environment block.
    #[test]
    fn a_system_cache_split_yields_two_marked_blocks() {
        let body = build_body(
            "claude",
            256,
            None,
            None,
            None,
            &[],
            CacheMode::Ephemeral,
            false,
            Some(6), // "STABLE" | "VOLATILE"
            &[sys("STABLEVOLATILE"), user("hi")],
            &[],
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2, "prefix and tail are separate blocks");
        assert_eq!(system[0]["text"], "STABLE");
        assert_eq!(system[1]["text"], "VOLATILE");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
    }

    /// Degenerate boundaries leave the prompt as one block rather than risking a
    /// mis-placed breakpoint — or a panic on a non-char boundary.
    #[test]
    fn a_bad_system_cache_split_is_ignored() {
        let one_block = |at: Option<usize>| {
            let body = build_body(
                "claude",
                256,
                None,
                None,
                None,
                &[],
                CacheMode::Ephemeral,
                false,
                at,
                &[sys("héllo"), user("hi")],
                &[],
            );
            body["system"].as_array().unwrap().len()
        };
        assert_eq!(one_block(None), 1, "no boundary given");
        assert_eq!(one_block(Some(0)), 1, "empty prefix");
        assert_eq!(one_block(Some(999)), 1, "past the end");
        assert_eq!(
            one_block(Some(2)),
            1,
            "mid-codepoint: would panic if sliced"
        );
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
            None,
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

    /// The adaptive body shape: the thinking object is sent even with no effort
    /// configured (thinking is off by default on Opus 4.6–4.8 / Sonnet 4.6, and
    /// this is what turns it on), `output_config` appears only when an effort
    /// level maps to something, and `budget_tokens` never appears — it is a 400
    /// on every model that speaks this dialect.
    #[test]
    fn adaptive_models_send_adaptive_thinking_and_output_config() {
        let plain = build_body(
            "claude-opus-5",
            8192,
            None,
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert_eq!(plain["thinking"]["type"], "adaptive");
        assert_eq!(plain["thinking"]["display"], "summarized");
        assert!(plain["thinking"].get("budget_tokens").is_none());
        assert!(
            plain.get("output_config").is_none(),
            "no effort configured → no output_config"
        );
        // A thinking config is being sent, so sampling params are withheld.
        assert!(plain.get("temperature").is_none());

        let think = build_body(
            "claude-opus-5",
            8192,
            Some("high"),
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert_eq!(think["thinking"]["type"], "adaptive");
        assert_eq!(think["output_config"]["effort"], "high");
        assert!(think["thinking"].get("budget_tokens").is_none());
        assert!(think.get("temperature").is_none());
    }

    /// The manual body shape, still the only one Claude 4.5-and-earlier accept:
    /// a budget that fits inside `max_tokens`, no `output_config` (Sonnet 4.5
    /// has no effort knob), no `display` (`"summarized"` is already the default
    /// on every model that speaks this dialect), and temperature withheld.
    #[test]
    fn manual_models_send_a_thinking_budget() {
        let think = build_body(
            "claude-sonnet-4-5",
            8192,
            Some("high"),
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert_eq!(think["thinking"]["type"], "enabled");
        assert!(
            think["thinking"].get("display").is_none(),
            "manual thinking already defaults to summarized"
        );
        let budget = think["thinking"]["budget_tokens"].as_u64().unwrap() as u32;
        assert!((1024..8192).contains(&budget), "budget {budget}");
        assert!(think.get("output_config").is_none());
        assert!(think.get("temperature").is_none());

        // Opus 4.5 is the one model that takes both dialect and effort.
        let opus45 = build_body(
            "claude-opus-4-5",
            8192,
            Some("medium"),
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert_eq!(opus45["thinking"]["type"], "enabled");
        assert_eq!(opus45["output_config"]["effort"], "medium");

        // No effort at all on a manual-only model → no thinking config, and
        // sampling params flow as before.
        let off = build_body(
            "claude-sonnet-4-5",
            8192,
            None,
            Some(0.3),
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert!(off.get("thinking").is_none());
        let t = off["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6, "temperature {t}");
    }

    /// An explicit "none" (or an unrecognized label) turns thinking off on every
    /// model, including the ones that would otherwise think by default. Whether
    /// sampling params then ride along is a separate, per-model question — see
    /// `sampling_params_are_withheld_from_models_that_reject_them`.
    #[test]
    fn effort_none_sends_no_thinking_config_on_any_model() {
        for model in ["claude-opus-5", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
            for effort in ["none", "off", "banana"] {
                let body = build_body(
                    model,
                    8192,
                    Some(effort),
                    Some(0.3),
                    Some(0.5),
                    &[],
                    CacheMode::Off,
                    false,
                    None,
                    &[user("hi")],
                    &[],
                );
                assert!(
                    body.get("thinking").is_none(),
                    "{model} / {effort} must send no thinking config"
                );
                assert!(body.get("output_config").is_none());
            }
        }
    }

    /// `temperature`/`top_p` are a **400 on every request** — thinking or not —
    /// on Fable/Mythos 5, Opus 5, Opus 4.8/4.7 and Sonnet 5, so they are dropped
    /// there rather than failing the turn. On Opus 4.6 / Sonnet 4.6 and earlier
    /// the restriction is only "while thinking is on", which the thinking-config
    /// check already covers, so they still ride on the no-thinking path.
    #[test]
    fn sampling_params_are_withheld_from_models_that_reject_them() {
        // `effort: "none"` is the only way to reach the sampling path on a model
        // that would otherwise think by default.
        let body = |model: &str| {
            build_body(
                model,
                8192,
                Some("none"),
                Some(0.3),
                Some(0.5),
                &[],
                CacheMode::Off,
                false,
                None,
                &[user("hi")],
                &[],
            )
        };
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "anthropic/claude-opus-5",
            // Unknown / future ids are assumed locked: a dropped sampling param
            // is a nudge, a 400 is a dead turn.
            "claude-opus-9",
            "some-gateway-alias",
        ] {
            let b = body(model);
            assert!(
                b.get("temperature").is_none(),
                "{model} rejects temperature"
            );
            assert!(b.get("top_p").is_none(), "{model} rejects top_p");
        }
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-sonnet-4-5",
            "claude-opus-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-1",
            "claude-3-5-haiku-20241022",
        ] {
            let b = body(model);
            let t = b["temperature"].as_f64().unwrap_or_default();
            assert!((t - 0.3).abs() < 1e-6, "{model} takes temperature: {t}");
            assert!(b.get("top_p").is_some(), "{model} takes top_p");
        }
        // Sonnet 4.6 with thinking on still withholds them — the lock is
        // per-model, the thinking rule is per-request, and both apply.
        let thinking = build_body(
            "claude-sonnet-4-6",
            8192,
            None,
            Some(0.3),
            Some(0.5),
            &[],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        assert!(thinking.get("temperature").is_none());
        assert!(thinking.get("top_p").is_none());
    }

    /// Model classification. The manual-only set is a closed list of shipped
    /// models; anything else — including ids that do not exist yet and
    /// provider-prefixed forms — must default to adaptive, because the new
    /// models are the ones that 400 on `type:"enabled"`.
    #[test]
    fn model_classification_defaults_to_adaptive() {
        let adaptive = |m: &str| {
            matches!(
                thinking_config(m, Some("medium"), 8192),
                ThinkingShape::Adaptive { .. }
            )
        };
        for m in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            // Provider-prefixed and dated forms of the same models.
            "anthropic/claude-opus-5",
            "openrouter/anthropic/claude-opus-4-8",
            "Claude-Opus-4-7-20260101",
            // Unknown / future ids fall to the forward-compatible default.
            "claude-opus-9",
            "claude-quartz-5",
            "some-gateway-alias",
        ] {
            assert!(adaptive(m), "{m} must be adaptive");
        }
        for m in [
            "claude-3-opus-20240229",
            "claude-3-5-haiku-20241022",
            "claude-3-7-sonnet-latest",
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-1",
            "claude-opus-4-0",
            "claude-sonnet-4-0",
            "claude-sonnet-4-2",
            // A dated Claude 4.0 snapshot: the trailing date must not read as a
            // minor version and promote it to adaptive.
            "claude-opus-4-20250514",
            "anthropic/claude-sonnet-4-5",
        ] {
            assert!(!adaptive(m), "{m} must use manual thinking");
        }
    }

    /// hrdr's effort ladder maps onto Anthropic's, and levels a model doesn't
    /// know are clamped down instead of being sent (and 400'd).
    #[test]
    fn effort_maps_and_clamps_per_model() {
        let level = |model: &str, effort: &str| match thinking_config(model, Some(effort), 200_000)
        {
            ThinkingShape::Adaptive { effort } | ThinkingShape::Manual { effort, .. } => {
                effort.map(Effort::as_str)
            }
            ThinkingShape::Off => None,
        };
        // `minimal` has no Anthropic equivalent; the rest are one-to-one.
        assert_eq!(level("claude-opus-5", "minimal"), Some("low"));
        assert_eq!(level("claude-opus-5", "low"), Some("low"));
        assert_eq!(level("claude-opus-5", "medium"), Some("medium"));
        assert_eq!(level("claude-opus-5", "high"), Some("high"));
        assert_eq!(level("claude-opus-5", "xhigh"), Some("xhigh"));
        assert_eq!(level("claude-opus-5", "max"), Some("max"));
        // Opus/Sonnet 4.6 took `max` before `xhigh` existed.
        assert_eq!(level("claude-opus-4-6", "xhigh"), Some("high"));
        assert_eq!(level("claude-opus-4-6", "max"), Some("max"));
        assert_eq!(level("claude-sonnet-4-6", "xhigh"), Some("high"));
        // Opus 4.5's effort knob stops at `high`.
        assert_eq!(level("claude-opus-4-5", "xhigh"), Some("high"));
        assert_eq!(level("claude-opus-4-5", "max"), Some("high"));
        // Manual-only models with no effort knob send none at all.
        assert_eq!(level("claude-sonnet-4-5", "max"), None);
        assert_eq!(level("claude-haiku-4-5", "high"), None);
    }

    #[test]
    fn top_p_and_stop_sequences_map_onto_messages_api() {
        // top_p is sent when no thinking config is (here: effort explicitly off)
        // on a model that still accepts sampling params at all.
        let body = build_body(
            "claude-sonnet-4-6",
            8192,
            Some("none"),
            None,
            Some(0.5),
            &["STOP".to_string(), "END".to_string()],
            CacheMode::Off,
            false,
            None,
            &[user("hi")],
            &[],
        );
        let p = body["top_p"].as_f64().unwrap();
        assert!((p - 0.5).abs() < 1e-6, "top_p {p}");
        assert_eq!(body["stop_sequences"], json!(["STOP", "END"]));

        // top_p is withheld whenever a thinking config is sent, on either
        // dialect (Anthropic forbids it alongside manual thinking, and hrdr
        // keeps the same conservative rule under adaptive).
        for model in ["claude-opus-5", "claude-sonnet-4-5"] {
            let thinking_body = build_body(
                model,
                8192,
                Some("high"),
                None,
                Some(0.5),
                &[],
                CacheMode::Off,
                false,
                None,
                &[user("hi")],
                &[],
            );
            assert!(thinking_body.get("top_p").is_none(), "{model}");
        }

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
            None,
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
            responses_reasoning_items: vec![],
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
        // Scales with max_tokens.
        let small = thinking_budget(Effort::High, 8192).unwrap();
        let big = thinking_budget(Effort::High, 32000).unwrap();
        assert!(big > small);
        // Budget always leaves ≥1024 for the answer and is ≥1024 itself.
        assert!((1024..=8192 - 1024).contains(&small));
        // A window too small to fit a budget + answer → thinking off.
        assert_eq!(thinking_budget(Effort::High, 1500), None);
        assert_eq!(
            thinking_config("claude-sonnet-4-5", Some("high"), 1500),
            ThinkingShape::Off
        );
    }

    /// With `max_tokens` now sized from the model's real output cap, the raw
    /// fraction would ask for a ~120k-token budget on a single turn; the cap
    /// keeps it inside what the docs consider a non-batch request, while the
    /// `< max_tokens` and `≥ 1024` invariants still hold.
    #[test]
    fn manual_thinking_budget_is_capped() {
        for &max_tokens in &[64_000, 128_000] {
            for effort in [Effort::Low, Effort::High, Effort::Max] {
                let b = thinking_budget(effort, max_tokens).unwrap();
                assert!(b <= MAX_THINKING_BUDGET, "{effort:?}/{max_tokens}: {b}");
                assert!(
                    (1024..max_tokens).contains(&b),
                    "{effort:?}/{max_tokens}: {b}"
                );
            }
        }
        // Below the cap the fraction still governs, so effort still matters.
        assert!(thinking_budget(Effort::Low, 32_000).unwrap() < MAX_THINKING_BUDGET);
    }

    /// The interleaved-thinking beta rides on the manual dialect only.
    #[test]
    fn interleaved_thinking_beta_is_manual_only() {
        let tools = vec![ToolDef::function("read", "d", json!({ "type": "object" }))];
        let body = |model: &str| {
            build_body(
                model,
                8192,
                Some("high"),
                None,
                None,
                &[],
                CacheMode::Off,
                false,
                None,
                &[user("hi")],
                &tools,
            )
        };
        assert_eq!(
            beta_headers(&body("claude-sonnet-4-5"), true, CacheMode::Off, false),
            vec!["interleaved-thinking-2025-05-14"],
        );
        assert!(
            beta_headers(&body("claude-opus-5"), true, CacheMode::Off, false).is_empty(),
            "adaptive models interleave without the beta"
        );
        // Without tools there is nothing to interleave between.
        assert!(beta_headers(&body("claude-sonnet-4-5"), false, CacheMode::Off, false).is_empty());
        // The 1h-TTL beta is independent of the thinking dialect.
        assert_eq!(
            beta_headers(&body("claude-opus-5"), true, CacheMode::Ephemeral, true),
            vec!["extended-cache-ttl-2025-04-11"],
        );
    }

    /// Anthropic reports thinking spend as `usage.output_tokens_details
    /// .thinking_tokens` on the final `message_delta`; it must land on the same
    /// `completion_tokens_details.reasoning_tokens` the OpenAI/Codex paths fill,
    /// which is what the UI's reasoning readout reads.
    #[test]
    fn message_delta_reports_thinking_tokens_as_reasoning_tokens() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;
        let mut map = |ev: &Value| {
            map_event(
                ev,
                &mut slot,
                &mut next,
                &mut thinking,
                &mut redacted,
                &mut stop_seen,
            )
            .unwrap()
        };

        let ev = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 500, "output_tokens_details": {"thinking_tokens": 320}},
        });
        let usage = map(&ev).unwrap().usage.unwrap();
        assert_eq!(usage.completion_tokens, 500);
        assert_eq!(usage.reasoning_tokens(), Some(320));

        // A turn without thinking omits the nested object entirely.
        let plain = json!({"type": "message_delta", "usage": {"output_tokens": 7}});
        let usage = map(&plain).unwrap().usage.unwrap();
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.reasoning_tokens(), None);
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
            responses_reasoning_items: vec![],
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

    /// `message_start` surfaces the cache-**write** counter as its own number
    /// **without** changing `prompt_tokens`, which stays the inclusive total
    /// (input + cache read + cache write). The breakdown exists because the
    /// three parts are priced differently — a write is 1.25x/2x plain input, a
    /// read 0.1x — and folding writes into the plain bucket under-billed every
    /// turn hrdr's rolling breakpoint wrote the cache on (i.e. nearly all).
    #[test]
    fn message_start_reports_cache_writes_without_changing_the_prompt_total() {
        let u = message_start_usage(Some(&json!({
            "input_tokens": 10,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 300,
        })))
        .usage
        .unwrap();
        // Inclusive total, exactly as before this breakdown existed.
        assert_eq!(u.prompt_tokens, 315);
        assert_eq!(u.cached_tokens(), Some(5));
        assert_eq!(u.cache_creation_tokens(), Some(300));
        // The three parts partition the total with nothing left over.
        assert_eq!(
            u.prompt_tokens,
            10 + u.cached_tokens().unwrap() + u.cache_creation_tokens().unwrap()
        );

        // No cache activity: both counters stay `None` (not `Some(0)`), so
        // "wrote nothing" can't be confused with "reported nothing".
        let plain = message_start_usage(Some(&json!({ "input_tokens": 42 })))
            .usage
            .unwrap();
        assert_eq!(plain.prompt_tokens, 42);
        assert_eq!(plain.cached_tokens(), None);
        assert_eq!(plain.cache_creation_tokens(), None);
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
            responses_reasoning_items: vec![],
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
            None,
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

    /// A `rate_limit_error` whose message describes a spent billing cap is
    /// terminal — the user is out of credit, not rate limited, so retrying for
    /// six minutes is pointless.
    #[test]
    fn rate_limit_error_with_a_credit_message_is_terminal() {
        let mut slot = std::collections::HashMap::new();
        let mut next = 0usize;
        let mut thinking: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        let mut redacted: Vec<(u64, Value)> = vec![];
        let mut stop_seen = false;

        let ev = json!({"type":"error","error":{"type":"rate_limit_error","message":"Your credit balance is too low to access the API"}});
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
            crate::client::ChatErrorKind::UsageLimit,
            "a rate_limit_error with a credit/quota message is a spent cap"
        );
        assert!(!crate::retry::is_transient(&err));
        assert!(chat_err.message.contains("rate_limit_error"));
        assert!(chat_err.message.contains("credit balance is too low"));
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

    /// Drive a canned SSE body through the real [`chat_stream`] and collect what
    /// came out: the chunks, then the error that terminated the stream (if any —
    /// `try_stream` stops at the first `Err`, so there is at most one).
    ///
    /// The forced backend is the whole point. [`crate::client::detect_backend`]
    /// keys on the HOST, so a mock bound to `127.0.0.1` is `Backend::OpenAi` and
    /// `Client::chat_stream` would dispatch to the chat-completions path — none
    /// of this module would run. Everything after the byte loop above (the
    /// thinking-block flush, the missing-`message_stop` truncation error) is
    /// reachable from a test only this way.
    async fn anthropic_stream(body: &'static str) -> (Vec<ChatChunk>, Option<anyhow::Error>) {
        let base_url = crate::client::serve_once(body).await;
        let mut client = crate::Client::new(base_url, Some("test-key".to_string()), "claude-test");
        client.set_backend_for_test(crate::client::Backend::Anthropic);
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

    /// The typed error a stream ended with, or a panic naming what arrived
    /// instead — an untyped `anyhow` here would sail past hrdr-agent's retry
    /// classifier, so the type is part of the assertion.
    fn chat_error(err: Option<anyhow::Error>) -> crate::client::ChatError {
        let err = err.expect("the stream must have terminated with an error");
        let typed = err
            .downcast_ref::<crate::client::ChatError>()
            .unwrap_or_else(|| panic!("error must be a typed ChatError, got: {err:#}"));
        crate::client::ChatError {
            status: typed.status,
            retry_after: typed.retry_after,
            kind: typed.kind,
            message: typed.message.clone(),
        }
    }

    /// The control for the truncation test below: a stream that DOES terminate
    /// must come back clean, so a green truncation test cannot be explained by
    /// "everything through this path errors".
    #[tokio::test]
    async fn a_complete_stream_yields_text_and_the_mapped_finish_reason() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
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
        assert_eq!(finish, ["stop"], "end_turn maps to the OpenAI `stop`");

        // `message_start` carries the prompt total, `message_delta` the output —
        // both have to survive the assembly, not just the text.
        let prompt: Vec<u32> = chunks
            .iter()
            .filter_map(|c| c.usage.as_ref())
            .map(|u| u.prompt_tokens)
            .collect();
        assert_eq!(prompt, [7, 0]);
        let completion: Vec<u32> = chunks
            .iter()
            .filter_map(|c| c.usage.as_ref())
            .map(|u| u.completion_tokens)
            .collect();
        assert_eq!(completion, [0, 5]);

        assert!(
            chunks
                .iter()
                .all(|c| c.anthropic_thinking_blocks.is_empty()),
            "no thinking blocks in the stream → no synthetic chunk"
        );
    }

    /// A stream cut before `message_stop` must be **Transient**, which is what
    /// makes the agent re-request instead of accepting half a reply as final.
    /// The OpenAI equivalent is covered end-to-end by hrdr-agent's
    /// `agent_run_incomplete_stream_then_retry`.
    #[tokio::test]
    async fn a_stream_without_message_stop_is_a_transient_error() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half a rep\"}}\n\n";
        let (chunks, err) = anthropic_stream(body).await;

        // The partial text still arrives — the error is about the *ending*, not
        // about the chunks that got through.
        let text: String = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.content.clone())
            .collect();
        assert_eq!(text, "half a rep");

        let err = chat_error(err);
        assert_eq!(
            err.kind,
            crate::client::ChatErrorKind::Transient,
            "a cut stream must be retryable, not terminal"
        );
        assert!(
            err.message.contains("message_stop"),
            "message must name the missing terminator: {}",
            err.message
        );
    }

    /// The post-loop flush: a thinking block streamed alongside a tool call is
    /// re-emitted as one synthetic chunk so the [`crate::Accumulator`] can hang it
    /// off the assistant message. Anthropic 400s the follow-up request if the
    /// signed block does not come back with the `tool_use` turn, so the block's
    /// CONTENT is the assertion, not merely that a chunk appeared.
    #[tokio::test]
    async fn a_thinking_block_is_flushed_after_the_loop_with_its_signature() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"weigh \"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"the options\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}\n\n\
                    event: content_block_stop\n\
                    data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
        assert!(err.is_none(), "stream terminated cleanly");

        // The thinking text also streams live to the reasoning pane, which is a
        // different path from the flush and must not be traded for it.
        let reasoning: String = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.reasoning_content.clone())
            .collect();
        assert_eq!(reasoning, "weigh the options");

        // The tool call is what makes replaying the signed block mandatory.
        let tool_ids: Vec<String> = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .flat_map(|c| c.delta.tool_calls.iter().flatten())
            .filter_map(|t| t.id.clone())
            .collect();
        assert_eq!(tool_ids, ["toolu_1"]);

        let blocks: Vec<&Vec<Value>> = chunks
            .iter()
            .map(|c| &c.anthropic_thinking_blocks)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(blocks.len(), 1, "exactly one synthetic flush chunk");
        assert_eq!(
            *blocks[0],
            vec![json!({
                "type": "thinking",
                "thinking": "weigh the options",
                "signature": "sig-abc",
            })],
            "the deltas must be reassembled verbatim, signature included"
        );
    }

    /// The flush filter keeps a block that carries EITHER text or a signature.
    /// A signed block with empty text is the normal shape on the adaptive
    /// dialect when `display` is omitted, and dropping it makes Anthropic 400 the
    /// follow-up request — so both halves of the filter are asserted here: the
    /// signed-but-empty block survives, the block that got neither is dropped.
    #[tokio::test]
    async fn a_signed_thinking_block_with_empty_text_survives_the_flush_filter() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-only\"}}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
        assert!(err.is_none(), "stream terminated cleanly");

        let blocks: Vec<&Vec<Value>> = chunks
            .iter()
            .map(|c| &c.anthropic_thinking_blocks)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(blocks.len(), 1, "exactly one synthetic flush chunk");
        assert_eq!(
            *blocks[0],
            vec![json!({"type": "thinking", "thinking": "", "signature": "sig-only"})],
            "the signed block is kept; the one with neither text nor signature is not"
        );
    }

    /// Every arm of [`map_stop_reason`], fallthrough included.
    ///
    /// The two `length` arms are the load-bearing ones:
    /// [`crate::Accumulator::truncated`] matches only `"length" | "max_tokens"`,
    /// so if either regressed, a reply cut off — at the output cap or at the
    /// context window — would report a clean finish: no truncation notice, no
    /// continuation, silently half an answer. The stream-path tests below pin
    /// the plumbing; this pins the map.
    #[test]
    fn map_stop_reason_maps_every_arm_onto_the_openai_vocabulary() {
        for (stop, expected) in [
            ("max_tokens", Some("length")),
            ("model_context_window_exceeded", Some("length")),
            ("tool_use", Some("tool_calls")),
            ("end_turn", Some("stop")),
            ("stop_sequence", Some("stop")),
            // A refusal finished; it was not cut short. `content_filter` says
            // that in the OpenAI vocabulary, and leaves `truncated()` false.
            ("refusal", Some("content_filter")),
            // Unrecognized: no arm claims to know what these mean, so the caller
            // passes them through verbatim and warns rather than reporting a
            // clean finish for a reply that may be half an answer.
            ("pause_turn", None),
            ("nova_flare", None),
            ("", None),
        ] {
            assert_eq!(map_stop_reason(stop), expected, "stop_reason {stop:?}");
        }
    }

    /// The process-global one-shot warning slot ([`crate::take_client_warning`])
    /// is shared state, so the tests that read it take this first rather than
    /// racing each other's writes. It does not shut out the rest of the crate:
    /// the only other writer reachable from this test binary is the one-shot
    /// auth-header strip in `client::apply_extra_headers` (the wire-log warnings
    /// need `HRDR_LOG_REQUESTS`, which only the separate integration-test binary
    /// sets). That leaves one possible foreign write per process, which would
    /// fail an assertion below rather than pass one.
    ///
    /// Async-aware because the guard is held across the stream `await` that
    /// produces the warning — the whole window that needs protecting.
    static WARNING_SLOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Empty the warning slot, and first burn the one foreign writer that can
    /// still reach it from this test binary: `client::apply_extra_headers`
    /// warns **once per process** when it drops an auth header, and that write
    /// lands whenever those tests happen to run. Unburned it can sit in the slot
    /// (it did — `a_recognized_stop_reason_raises_no_client_warning` failed on
    /// exactly that) or clobber the warning under test mid-stream. Burning the
    /// one-shot here means the only writer left during the stream below is the
    /// code under test; the wire-log warnings need `HRDR_LOG_REQUESTS`, which
    /// only the separate integration-test binary sets.
    fn drain_warning_slot() {
        let _burn = crate::client::apply_extra_headers(
            reqwest::Client::new().get("http://127.0.0.1/"),
            &[("x-api-key".to_string(), "burn".to_string())],
        );
        let _ = crate::client::take_client_warning();
    }

    /// A one-event stream whose `message_delta` carries `stop_reason`.
    ///
    /// `chat_stream`'s mock server takes a `&'static str`, so the body is leaked
    /// — a few hundred bytes per case, in a test binary that is about to exit.
    fn stop_reason_stream(stop_reason: &str) -> &'static str {
        Box::leak(
            format!(
                "event: message_start\n\
                 data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":7}}}}}}\n\n\
                 event: content_block_delta\n\
                 data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"half an ans\"}}}}\n\n\
                 event: message_delta\n\
                 data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
                 event: message_stop\n\
                 data: {{\"type\":\"message_stop\"}}\n\n"
            )
            .into_boxed_str(),
        )
    }

    /// Fold a stream the way the agent's turn loop does.
    async fn accumulate(body: &'static str) -> crate::Accumulator {
        let (chunks, err) = anthropic_stream(body).await;
        assert!(err.is_none(), "clean stream: {:#}", err.unwrap());
        let mut acc = crate::Accumulator::new();
        for chunk in &chunks {
            acc.push(chunk).unwrap();
        }
        acc
    }

    /// Anthropic's context-window stop has to land as truncation for the same
    /// reason `max_tokens` does — the reply stopped early — and the whole way
    /// down the real stream path, not just in the map.
    #[tokio::test]
    async fn a_context_window_stop_reason_reaches_the_accumulator_as_truncated() {
        let acc = accumulate(stop_reason_stream("model_context_window_exceeded")).await;
        assert_eq!(
            acc.finish_reason.as_deref(),
            Some("length"),
            "running out of context window must arrive as the OpenAI `length`"
        );
        assert!(
            acc.truncated(),
            "a reply cut off at the context window must report truncated"
        );
    }

    /// The loud half of the unknown case: the value rides through untranslated
    /// *and* the user is told, by name, that hrdr did not recognize it. Without
    /// the warning this is the original bug — an unknown reason is neither
    /// `length` nor `tool_calls`, so the turn reads as a clean, complete finish
    /// and a half answer is presented as whole.
    #[tokio::test]
    async fn an_unrecognized_stop_reason_rides_through_verbatim_and_warns_by_name() {
        let _slot = WARNING_SLOT.lock().await;
        drain_warning_slot();

        let acc = accumulate(stop_reason_stream("nova_flare")).await;
        assert_eq!(
            acc.finish_reason.as_deref(),
            Some("nova_flare"),
            "an unrecognized reason is passed through verbatim, not folded"
        );

        let warning = crate::client::take_client_warning()
            .expect("an unrecognized stop_reason must raise a client warning");
        assert!(
            warning.contains("nova_flare"),
            "the warning must name the reason hrdr did not recognize: {warning}"
        );
        assert!(
            warning.contains("may be incomplete"),
            "the warning must say what is at stake for the reply: {warning}"
        );
    }

    /// The negative half. Without it, a mapping that warned on *every* stop
    /// reason — including the four that finish normally — would pass the test
    /// above and bury the real signal under a warning on every single turn.
    #[tokio::test]
    async fn a_recognized_stop_reason_raises_no_client_warning() {
        let _slot = WARNING_SLOT.lock().await;
        drain_warning_slot();

        for stop in [
            "end_turn",
            "stop_sequence",
            "max_tokens",
            "tool_use",
            "refusal",
            "model_context_window_exceeded",
        ] {
            let acc = accumulate(stop_reason_stream(stop)).await;
            assert!(
                acc.finish_reason.is_some(),
                "stop_reason {stop:?} reached the accumulator"
            );
            // Scoped to *our* warning rather than `== None`: the burn above
            // closes the ordinary path for a foreign write, but a second belt
            // costs nothing here, and a mapping that warned on everything still
            // fails — its warning names the stop reason.
            if let Some(warning) = crate::client::take_client_warning() {
                assert!(
                    !warning.contains("stop_reason"),
                    "stop_reason {stop:?} is recognized and must not warn: {warning}"
                );
            }
        }
    }

    /// The user-visible half of the mapping above: a `message_delta` carrying
    /// `stop_reason: "max_tokens"` has to travel the real stream path and land
    /// in the [`crate::Accumulator`] as truncation. The table test alone would
    /// stay green if the plumbing between `map_stop_reason` and the chunk's
    /// `finish_reason` broke. The Codex equivalent is
    /// `incomplete_max_output_tokens_maps_to_length`; on OpenAI `length` is
    /// native, which leaves Anthropic the untested one of the three.
    #[tokio::test]
    async fn a_max_tokens_stop_reason_reaches_the_accumulator_as_truncated() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half an ans\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":5}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
        assert!(
            err.is_none(),
            "hitting the output cap is not a stream error: {:#}",
            err.unwrap()
        );

        // Fold exactly as the agent's turn loop does, then ask the question the
        // agent asks.
        let mut acc = crate::Accumulator::new();
        for chunk in &chunks {
            acc.push(chunk).unwrap();
        }
        assert_eq!(
            acc.finish_reason.as_deref(),
            Some("length"),
            "Anthropic's `max_tokens` must arrive as the OpenAI `length`"
        );
        assert!(
            acc.truncated(),
            "a reply cut off at the output cap must report truncated"
        );
    }

    /// A `redacted_thinking` block — what Anthropic emits in place of a thinking
    /// block when its safety classifier trips — carries all of its payload in
    /// the `data` field of `content_block_start` and receives no deltas at all.
    /// It still has to reach the post-loop flush intact: the follow-up request
    /// carrying `tool_use` is rejected if a thinking block from the turn is
    /// missing.
    #[tokio::test]
    async fn a_redacted_thinking_block_is_flushed_with_its_data_intact() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EroBCkYIAxgCIkD\"}}\n\n\
                    event: content_block_stop\n\
                    data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
        assert!(err.is_none(), "stream terminated cleanly");

        let blocks: Vec<&Vec<Value>> = chunks
            .iter()
            .map(|c| &c.anthropic_thinking_blocks)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(blocks.len(), 1, "exactly one synthetic flush chunk");
        assert_eq!(
            *blocks[0],
            vec![json!({"type": "redacted_thinking", "data": "EroBCkYIAxgCIkD"})],
            "the opaque `data` must be replayed byte-for-byte"
        );

        // The block is opaque, so nothing of it is shown to the user — only the
        // ordinary text block streams to the reasoning/content channels.
        let reasoning: String = chunks
            .iter()
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.reasoning_content.clone())
            .collect();
        assert_eq!(reasoning, "", "a redacted block has no deltas to stream");
    }

    /// Redacted and normal thinking blocks are collected in two separate places
    /// — a `Vec` and a `HashMap` — and merged before the flush, so only the sort
    /// by stream index keeps them in the order Anthropic sent them. Replaying
    /// them out of order makes Anthropic reject the follow-up `tool_use` turn.
    ///
    /// The redacted block sits at the LOWER index on purpose: the merge appends
    /// the redacted ones after the normal ones, so an implementation that
    /// skipped the sort would emit thinking-then-redacted and fail here.
    #[tokio::test]
    async fn thinking_blocks_flush_in_stream_index_order_not_collection_order() {
        let body = "event: message_start\n\
                    data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"enc-first\"}}\n\n\
                    event: content_block_stop\n\
                    data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"then plain\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-second\"}}\n\n\
                    event: content_block_stop\n\
                    data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                    event: content_block_start\n\
                    data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}\n\n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
                    event: message_delta\n\
                    data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let (chunks, err) = anthropic_stream(body).await;
        assert!(err.is_none(), "stream terminated cleanly");

        let blocks: Vec<&Vec<Value>> = chunks
            .iter()
            .map(|c| &c.anthropic_thinking_blocks)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(blocks.len(), 1, "exactly one synthetic flush chunk");
        assert_eq!(
            *blocks[0],
            vec![
                json!({"type": "redacted_thinking", "data": "enc-first"}),
                json!({
                    "type": "thinking",
                    "thinking": "then plain",
                    "signature": "sig-second",
                }),
            ],
            "index 0 (redacted) must precede index 1 (plain), as Anthropic sent them"
        );
    }
}
