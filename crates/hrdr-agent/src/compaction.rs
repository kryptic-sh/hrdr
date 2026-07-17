//! Compaction and context management — extracted from [`Agent`] into its own
//! module to keep `lib.rs` manageable.
//!
//! Holds the proactive/auto compaction machinery ([`Agent::compact`],
//! [`Agent::maybe_self_compact`], the summarizer prompt, tail-window selection),
//! the pressure-gated tool-output pruning helpers ([`plan_prune`]/[`apply_prune`]),
//! and the rough token estimators the context gauge and triggers rely on.

use anyhow::{Result, bail};

use crate::{
    Agent, AgentEvent, ChatMessage, LiveSubagents, MessageOrigin, Role, drain_stream,
    flatten_tool_protocol, is_context_overflow, is_transient, retry_after_hint, retry_backoff,
};

/// The context-usage token count at which proactive compaction fires:
/// `context_window − reserved`. The reserve is clamped to a quarter of the window
/// so a `reserved` larger than a small model's context still leaves a sane trigger
/// (a trigger of 0 would compact every turn).
///
/// One owner, used by the agent's own [`Agent::maybe_self_compact`] and by a
/// frontend's threshold check, so the two cannot drift apart.
pub fn compaction_trigger(window: u32, reserved: u32) -> u32 {
    window.saturating_sub(reserved.min(window / 4))
}

/// Whether context usage warrants compacting before the next request.
/// `enabled` is the `auto_compact` toggle; `last_prompt_tokens` is the latest
/// model call's prompt size.
pub fn should_auto_compact(
    last_prompt_tokens: Option<u32>,
    context_window: Option<u32>,
    reserved: u32,
    enabled: bool,
) -> bool {
    if !enabled {
        return false;
    }
    let (Some(prompt), Some(window)) = (last_prompt_tokens, context_window) else {
        return false;
    };
    window > 0 && prompt >= compaction_trigger(window, reserved)
}

/// Whether tool-output pruning should even be attempted this round: usage is
/// within `PRUNE_PRESSURE_TOKENS` of the same compaction trigger
/// [`should_auto_compact`] uses ([`compaction_trigger`]). Below that, pruning
/// isn't considered at all — a byte-stable prompt prefix is what keeps the
/// provider cache hitting, and there's no pressure yet to justify spending a
/// prune's one-time invalidation of it.
///
/// Cheap and mutation-free on purpose: the caller gates the (O(n) over the
/// history) [`plan_prune`] scan behind this, so a conversation nowhere near
/// its trigger never pays for the scan either.
pub fn prune_under_pressure(usage: u32, context_window: u32, reserved: u32) -> bool {
    usage >= compaction_trigger(context_window, reserved).saturating_sub(PRUNE_PRESSURE_TOKENS)
}

/// Whether a prune plan is worth applying, given what [`plan_prune`] found.
///
/// A prune is only worth its one-time prompt-cache invalidation if it buys
/// real runway: reclaiming `reclaimable` tokens must land usage at least
/// `PRUNE_ROI_TOKENS` below the compaction trigger — several tool-rounds'
/// worth. A plan that can't clear that bar is skipped entirely (no mutation),
/// and compaction is left to fire naturally at the trigger — pruning first
/// and then compacting two turns later anyway would be the worst of both
/// worlds.
pub fn prune_meets_roi(usage: u32, context_window: u32, reserved: u32, reclaimable: u32) -> bool {
    let target = compaction_trigger(context_window, reserved).saturating_sub(PRUNE_ROI_TOKENS);
    usage.saturating_sub(reclaimable) <= target
}

/// Marks an agent as compacting for as long as it is, and clears the flag on every
/// exit — a summarization that fails or is cancelled must not leave its pane
/// spinning "compacting…" forever.
struct CompactingGuard(Option<(LiveSubagents, u64)>);

impl CompactingGuard {
    fn new(home: Option<(LiveSubagents, u64)>) -> Self {
        if let Some((live, key)) = &home {
            live.update(*key, |e| e.compacting = true);
        }
        Self(home)
    }
}

impl Drop for CompactingGuard {
    fn drop(&mut self) {
        if let Some((live, key)) = &self.0 {
            live.update(*key, |e| e.compacting = false);
        }
    }
}

/// Tool-output pruning: pressure-gated and ROI-checked, not continuous.
///
/// Pruning rewrites OLD messages — deep in the prompt prefix — so every prune
/// event invalidates the provider's prompt cache for nearly the whole
/// conversation (Anthropic prompt caching, llama.cpp prefix reuse). Pruning
/// continuously (clearing stale tool output every ~20k tokens, the old
/// behavior) re-pays that invalidation over and over and never amortizes —
/// net negative on a cached backend. But pruning still beats *compaction* (a
/// full cache nuke, a summarizer model call, and permanent information loss)
/// when it genuinely averts one. So: prune only when compaction is imminent
/// (`PRUNE_PRESSURE_TOKENS`), and only when the reclaimable amount buys real
/// runway (`PRUNE_ROI_TOKENS`) — pruning and then compacting a couple of
/// turns later anyway would be the worst of both worlds. See
/// [`prune_under_pressure`] and [`prune_meets_roi`] for the gate,
/// [`plan_prune`]/[`apply_prune`] for the mechanism.
///
/// This constant is the size of the protected window itself: the most recent
/// this-many estimated tokens of tool output stay verbatim; older bodies (once
/// a prune is actually triggered) are cleared. Matches opencode's
/// `PRUNE_PROTECT`.
pub(crate) const PRUNE_PROTECT_TOKENS: u32 = 40_000;
/// Pruning is only even *considered* once estimated usage is within this many
/// tokens of the compaction trigger (`context_window − compaction_reserved`,
/// [`compaction_trigger`] — the same trigger [`should_auto_compact`] uses).
/// Below that, never touch history: a byte-stable prefix is what keeps the
/// provider cache hitting.
const PRUNE_PRESSURE_TOKENS: u32 = 16_384;
/// A prune is only *worth* the one-time cache invalidation if it lands usage
/// at least this far below the compaction trigger — several tool-rounds of
/// runway. If the plan can't buy that, skip it and let compaction fire
/// naturally at the trigger instead.
const PRUNE_ROI_TOKENS: u32 = 32_768;
/// The most recent this-many turns (user messages) are never pruned, so the
/// model always keeps the tool output it's actively working with.
pub(crate) const PRUNE_KEEP_TURNS: usize = 2;

/// Stable prefix of a prune placeholder for a cleared `Role::Tool` result body
/// (see [`apply_prune`]): a short pointer at the file the original body was
/// saved to, e.g. `"[old tool output pruned to save context — full output
/// saved to <path>; \`read\` (offset/limit) or \`grep\` it if needed]"`. The
/// path varies per victim, so [`plan_prune`]'s "already pruned" check matches
/// on this prefix (`starts_with`) rather than an exact string.
pub(crate) const PRUNE_TOOL_PLACEHOLDER_PREFIX: &str = "[old tool output pruned";
/// Same idea as [`PRUNE_TOOL_PLACEHOLDER_PREFIX`], for a pruned
/// `Role::User`-with-`MessageOrigin::BackgroundResult` delivery (a detached
/// sub-agent's report — tool product, not the user speaking; see
/// [`plan_prune`]'s Change C victim selection). Worded as a task report, not
/// tool output, so a transcript reader isn't confused about what's missing.
pub(crate) const PRUNE_TASK_PLACEHOLDER_PREFIX: &str = "[old background task report pruned";
/// Fallback placeholder used when [`apply_prune`] can't save a victim's body
/// to a file (disk full, permissions, a read-only overflow dir, ...) — the
/// prune still proceeds (never fail the turn over this): the body still
/// leaves the model-facing history, and the UI transcript still has the
/// original untouched. There's no path to point at, so it's a dead marker
/// rather than a pointer. `plan_prune` recognizes it as already-pruned
/// alongside the two prefixes above — see [`is_prune_placeholder`].
pub(crate) const PRUNE_PLACEHOLDER: &str = "[old tool output cleared to save context]";

/// System prompt for the one-off compaction (summarization) call.
const COMPACT_SYSTEM: &str = "\
You are summarizing a software-engineering conversation between a user and an AI \
coding agent so it can continue in a fresh context with nothing important lost. \
Be precise, technical, and exhaustive about concrete details — vague summaries are \
useless here.";

/// User-turn instruction that triggers the structured summary.
const COMPACT_TRIGGER: &str = "\
Summarize the conversation so far. The summary REPLACES the full history, so it must \
let the agent continue seamlessly. Use these sections:

1. **Intent & requirements** — what the user asked for, in their own terms, including \
   explicit constraints and preferences.
2. **Technical context** — languages, frameworks, key APIs, architecture decisions.
3. **Files & code** — every file created or modified (with paths) and the gist of the \
   changes; include important snippets, signatures, and config values verbatim.
4. **Commands & results** — notable commands run and their outcomes (builds, tests, \
   commits, pushes).
5. **Errors & fixes** — problems hit and how they were resolved.
6. **Current state** — what is done and verified vs. in progress.
7. **Pending tasks & next step** — what remains, and the single most immediate next \
   action.

Be specific: prefer exact names, paths, and values over paraphrase. Output only the \
summary.";

/// Max bytes of a tool-result body kept when shrinking a compaction request.
pub(crate) const ELIDE_TOOL_RESULT_BYTES: usize = 400;

/// Index where the kept-verbatim tail begins for compaction. Keeps the last
/// `tail_turns` turns (a turn begins at a `role:"user"` message), but no more
/// than `preserve_tokens` estimated tokens — walking newest → oldest, adding
/// whole turns until the budget is hit, always keeping at least the newest
/// turn. Never returns 0 (the system prompt stays); the tail always begins on a
/// user message, so no tool result is orphaned. Everything in `1..start` gets
/// summarized. Mirrors opencode's compaction tail selection.
pub(crate) fn compaction_tail_start(
    msgs: &[ChatMessage],
    tail_turns: usize,
    preserve_tokens: u32,
) -> usize {
    if tail_turns == 0 {
        return msgs.len();
    }
    // Turn boundaries: user messages after the system prompt.
    let starts: Vec<usize> = (1..msgs.len())
        .filter(|&i| msgs[i].role == Role::User)
        .collect();
    let Some(&newest) = starts.last() else {
        return msgs.len().max(1);
    };
    let candidates = &starts[starts.len().saturating_sub(tail_turns)..];
    let mut tail_start = msgs.len();
    let mut tokens = 0u32;
    for &start in candidates.iter().rev() {
        let turn_tokens = estimate_tokens_in_messages(&msgs[start..tail_start]);
        // Always keep the newest turn; stop before an older turn that busts the
        // budget.
        if start != newest && tokens + turn_tokens > preserve_tokens {
            break;
        }
        tokens += turn_tokens;
        tail_start = start;
    }
    tail_start.max(1)
}

/// Advance `start` forward past any leading `role:"tool"` messages, so a window
/// beginning at the returned index never starts on a tool result orphaned from
/// its assistant `tool_calls` message (strict servers reject that). Returns
/// `msgs.len()` when everything from `start` on is tool results.
fn align_past_tool_results(msgs: &[ChatMessage], mut start: usize) -> usize {
    while start < msgs.len() && msgs[start].role == Role::Tool {
        start += 1;
    }
    start
}

/// Index where a verbatim tail can safely begin *inside a single mega-turn* —
/// used when [`compaction_tail_start`] found no earlier turn boundary to fall
/// back to (the whole history beyond the system prompt is one `role:"user"`
/// turn that grew huge through many tool round-trips; every delegated
/// sub-agent's history is exactly this shape). Same walk as
/// `compaction_tail_start` — newest → oldest, budgeted by `preserve_tokens`,
/// always keeping at least the newest message — but at MESSAGE granularity
/// rather than whole-turn granularity, since the turn itself is the only unit
/// left to split.
///
/// Never lands on a `Role::Tool` message: walks forward past one so a tool
/// result is never torn from its assistant `tool_calls` message (mirrors
/// [`tail_window`]'s alignment). That forward walk can consume the entire
/// budgeted window (e.g. the newest message is a lone tool result whose
/// call is now the oldest thing that fits) — in which case this returns
/// `msgs.len()`, meaning: keep nothing verbatim, summarize the whole turn.
/// That is still a valid, useful result (`compact` ends up with just
/// `[system, continuation]`), and never invalid: an empty tail can't orphan
/// anything. Returns `turn_start` when nothing is worth summarizing (the
/// turn already fits the budget, or `turn_start >= msgs.len()`).
pub(crate) fn mega_turn_tail_start(
    msgs: &[ChatMessage],
    turn_start: usize,
    preserve_tokens: u32,
) -> usize {
    if turn_start >= msgs.len() {
        return turn_start;
    }
    let mut tail_start = msgs.len();
    let mut tokens = 0u32;
    for i in (turn_start..msgs.len()).rev() {
        let msg_tokens = estimate_tokens_in_messages(&msgs[i..=i]);
        // Always keep the newest message; stop before an older one that busts
        // the budget.
        if tail_start != msgs.len() && tokens + msg_tokens > preserve_tokens {
            break;
        }
        tokens += msg_tokens;
        tail_start = i;
    }
    align_past_tool_results(msgs, tail_start).max(turn_start)
}

/// Copy of `msgs` with bulky tool-result bodies truncated — tool output is the
/// usual context hog, and the summarizer mostly needs the surrounding turns.
pub(crate) fn elide_tool_results(msgs: &[ChatMessage]) -> Vec<ChatMessage> {
    msgs.iter()
        .map(|m| {
            let Some(c) = &m.content else {
                return m.clone();
            };
            if m.role != Role::Tool || c.len() <= ELIDE_TOOL_RESULT_BYTES {
                return m.clone();
            }
            let cut = hrdr_tools::floor_char_boundary(c, ELIDE_TOOL_RESULT_BYTES);
            let mut m = m.clone();
            m.content = Some(format!(
                "{}\n…[tool output elided for compaction]",
                &c[..cut]
            ));
            m
        })
        .collect()
}

/// Whether `body` is already some variant of an applied prune placeholder —
/// a file-linked pointer (either [`PRUNE_TOOL_PLACEHOLDER_PREFIX`] or
/// [`PRUNE_TASK_PLACEHOLDER_PREFIX`]) or the constant [`PRUNE_PLACEHOLDER`]
/// fallback used when saving the body failed. [`plan_prune`] uses this so
/// re-planning never re-targets — and so never re-saves or double-counts — a
/// body an earlier prune already cleared.
fn is_prune_placeholder(body: &str) -> bool {
    body.starts_with(PRUNE_TOOL_PLACEHOLDER_PREFIX)
        || body.starts_with(PRUNE_TASK_PLACEHOLDER_PREFIX)
        || body == PRUNE_PLACEHOLDER
}

/// Whether `m` is prunable *content* — bulky non-conversation material that
/// isn't the real user↔agent exchange: a tool-call result, a detached
/// background sub-agent's delivery report, or a harness turn-end nudge
/// (`Role::User` on the wire, since that's how each is folded into history,
/// but `MessageOrigin::BackgroundResult`/`MessageOrigin::Nudge` mark them as
/// harness/tool product rather than the user speaking). Never a genuine user
/// message (`origin` `User`/`Steering`), an assistant message (its
/// `tool_calls` metadata must stay so the tool-call ↔ result pairing strict
/// servers require stays intact), or a system message.
fn is_prunable(m: &ChatMessage) -> bool {
    m.role == Role::Tool
        || (m.role == Role::User
            && matches!(
                m.origin,
                MessageOrigin::BackgroundResult | MessageOrigin::Nudge
            ))
}

/// Work out which *old* non-conversation messages a prune would clear —
/// tool-call results and background-task delivery reports — keeping the most
/// recent [`PRUNE_PROTECT_TOKENS`] of that content — plus the last
/// [`PRUNE_KEEP_TURNS`] turns — verbatim. Pure: does not touch `messages`, so
/// the caller can weigh the reclaim against the cost of pruning (see
/// [`prune_meets_roi`]) before committing to [`apply_prune`].
///
/// Returns the victim indices (oldest prunable messages past the protected
/// window) and their total estimated token size. `protect_tokens` is the
/// recent window (tool output + background-task reports combined) kept
/// verbatim; `keep_turns` the recent turns never touched.
pub(crate) fn plan_prune(
    messages: &[ChatMessage],
    protect_tokens: u32,
    keep_turns: usize,
) -> (Vec<usize>, u32) {
    let mut turns = 0usize;
    // Cumulative prunable-content tokens seen scanning newest → oldest (both
    // tool output and background-task reports count toward the same window).
    let mut seen_tokens = 0u32;
    let mut reclaimable = 0u32;
    let mut victims: Vec<usize> = Vec::new();
    for i in (0..messages.len()).rev() {
        let m = &messages[i];
        // Only a genuine user turn (typed input or a mid-turn steering
        // correction) is a turn boundary. A `BackgroundResult` delivery is
        // `Role::User` on the wire but isn't the user speaking — counting it
        // here would let a burst of task deliveries either shield old
        // content from ever being pruned (each one pushes `keep_turns`
        // further back) or burn through the protected-turns budget on
        // messages that were never protected content to begin with. So turn
        // counting and prunability both key off role *and* origin, not role
        // alone.
        if m.role == Role::User && matches!(m.origin, MessageOrigin::User | MessageOrigin::Steering)
        {
            turns += 1;
        }
        // The last few turns are always kept whole — the model is still working
        // with that output.
        if turns < keep_turns {
            continue;
        }
        if !is_prunable(m) {
            continue;
        }
        let body = m.content.as_deref().unwrap_or_default();
        if is_prune_placeholder(body) {
            continue; // already pruned
        }
        let est = estimate_tokens(body);
        seen_tokens += est;
        // Keep the newest window verbatim; everything older is a prune target.
        if seen_tokens <= protect_tokens {
            continue;
        }
        reclaimable += est;
        victims.push(i);
    }
    (victims, reclaimable)
}

/// Apply a plan from [`plan_prune`]: replace each victim's body with a short
/// pointer at a file holding the original content, saved via the same
/// overflow mechanism tool outputs already use
/// ([`hrdr_tools::save_overflow`] into [`hrdr_tools::tool_output_dir`]) — one
/// file per victim, so the model can still `read` (offset/limit) or `grep`
/// it back if it turns out to matter after all. The assistant `tool_calls`
/// metadata and every message stays, so the tool-call ↔ result pairing
/// strict servers require is intact. Split from planning so the caller only
/// pays for this — and the prompt-cache invalidation it causes — once the
/// plan is known to be worth it.
pub(crate) fn apply_prune(messages: &mut [ChatMessage], victims: &[usize]) {
    apply_prune_in(messages, victims, &hrdr_tools::tool_output_dir());
}

/// [`apply_prune`] with an explicit overflow directory — mirrors
/// `hrdr_tools::truncate_saved`'s `_in` test seam, so tests can point pruned
/// bodies at a scratch dir (or an unwritable one, to exercise the
/// save-failure fallback) instead of the real `tool_output_dir()`.
pub(crate) fn apply_prune_in(
    messages: &mut [ChatMessage],
    victims: &[usize],
    dir: &std::path::Path,
) {
    for &i in victims {
        let m = &mut messages[i];
        let body = m.content.clone().unwrap_or_default();
        // Tool results and background-task reports get distinct labels and
        // wording (see the placeholder-prefix docs) so a transcript reader —
        // and `is_prune_placeholder` — can tell which kind of content is
        // missing.
        let (label, prefix, kind) = if m.role == Role::Tool {
            ("pruned-tool", PRUNE_TOOL_PLACEHOLDER_PREFIX, "output")
        } else {
            ("pruned-task", PRUNE_TASK_PLACEHOLDER_PREFIX, "report")
        };
        m.content = Some(match hrdr_tools::save_overflow(dir, label, &body) {
            Ok(path) => format!(
                "{prefix} to save context — full {kind} saved to {}; `read` (offset/limit) or \
                 `grep` it if needed]",
                path.display()
            ),
            // Never fail the turn over a prune: the body still leaves the
            // model-facing history (the point of pruning at all), and the UI
            // transcript still has the original untouched — there's just no
            // file to point at, so it degrades to the dead-marker fallback.
            Err(_) => PRUNE_PLACEHOLDER.to_string(),
        });
    }
}

/// The most recent `1/div` of `msgs` (at least two messages), aligned forward
/// past any leading `role:"tool"` results so no result is orphaned from its
/// assistant `tool_calls` message (strict servers reject that).
pub(crate) fn tail_window(msgs: &[ChatMessage], div: usize) -> Vec<ChatMessage> {
    let keep = (msgs.len() / div.max(1)).clamp(2, msgs.len());
    let start = align_past_tool_results(msgs, msgs.len() - keep);
    msgs[start..].to_vec()
}

/// Very rough token estimate (~4 characters per token) for `text`. Used only as
/// a fallback when the server reports no usage — good enough for the context bar
/// + auto-compaction, not for billing.
pub(crate) fn estimate_tokens(text: &str) -> u32 {
    (text.len() / 4) as u32
}

/// Estimate the prompt tokens of a whole request: each message's content and any
/// tool-call names/arguments, plus a small per-message overhead for the role and
/// structural tokens the chat template adds.
pub(crate) fn estimate_tokens_in_messages(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .map(|m| {
            let content = m.content.as_deref().map(str::len).unwrap_or(0);
            let calls = m
                .tool_calls
                .as_ref()
                .map(|tcs| {
                    tcs.iter()
                        .map(|c| c.function.name.len() + c.function.arguments.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            (content + calls) as u32 / 4 + 4
        })
        .sum()
}

impl Agent {
    /// Whether this agent compacts itself when its context fills, and the buffer it
    /// keeps below its window — which is also the threshold its context gauge turns
    /// red at.
    ///
    /// Live-changeable (`/reload`). Before this the frontend kept its own copies and
    /// a reload updated only those: the gauge moved, while the agent went on
    /// compacting (or not) exactly as it had at launch.
    pub fn set_auto_compact(&mut self, on: bool) {
        self.auto_compact = on;
        self.publish_chrome();
    }

    pub fn set_compaction_reserved(&mut self, tokens: u32) {
        self.compaction_reserved = tokens;
        self.publish_chrome();
    }

    /// Compact the conversation: ask the model for a structured summary and
    /// replace the history with `[system prompt, summary]`, so the context
    /// shrinks while continuity is preserved (Claude Code / opencode style).
    ///
    /// `instructions` optionally steers the summary's focus. Returns
    /// `(messages_before, messages_after)`; a no-op when there's nothing beyond
    /// the system prompt and one message.
    pub async fn compact(&mut self, instructions: Option<&str>) -> Result<(usize, usize)> {
        // Whatever the outcome, the last prompt reading describes the history as it
        // was *before* this call. Clearing it here (rather than in one caller) stops
        // a frontend-driven `/compact` from leaving a stale, over-the-trigger figure
        // that makes the agent immediately compact the history it just compacted.
        self.last_prompt_tokens = None;
        let before = self.messages.len();
        if before <= 2 {
            return Ok((before, before));
        }
        // The agent is the one that knows it is summarizing — including when it
        // decided to on its own, which no frontend is told about. The guard clears
        // the flag on every exit, error and cancellation included.
        let _compacting = CompactingGuard::new(self.live_home.clone());
        // Keep the most recent messages verbatim — compaction usually fires
        // mid-task, and the summary alone loses exactly the detail the model
        // is working with. Only the head (everything older) is summarized.
        let mut tail_start = compaction_tail_start(
            &self.messages,
            self.compaction_tail_turns,
            self.preserve_recent_tokens,
        );
        if tail_start <= 2 {
            // No earlier turn boundary exists before the tail: the newest (and
            // only) turn *is* the whole history beyond the system prompt. That
            // still may be worth shrinking — a single turn balloons through many
            // tool round-trips without ever adding a second `role:"user"`
            // message (every delegated sub-agent's history is exactly this
            // shape), and can itself bust the context window. Simply no-op'ing
            // here used to let context-overflow recovery retry an identical,
            // still-too-big request until the retry budget was exhausted. Split
            // *inside* the turn instead: walk it the same way
            // `compaction_tail_start` walks whole turns, but at message
            // granularity (the turn itself is the only unit available), landing
            // only on a non-`Role::Tool` boundary so no tool_use/tool_result
            // pair is torn apart.
            tail_start = mega_turn_tail_start(&self.messages, 1, self.preserve_recent_tokens);
            if tail_start <= 1 {
                // Splitting bought nothing — the lone turn already fits the
                // tail budget, or there's truly nothing beyond the system
                // prompt to summarize.
                return Ok((before, before));
            }
        }

        // Build a one-off summarization request: a dedicated summarizer system
        // prompt + the conversation so far (minus its own system prompt) + the
        // trigger instruction. No tools — we only want prose back.
        let mut trigger = COMPACT_TRIGGER.to_string();
        if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
            trigger.push_str("\n\nAdditional instructions for the summary, follow them closely:\n");
            trigger.push_str(extra);
        }
        // When compaction is overflow-triggered, the summarization request is
        // itself near the limit (versus the failed request it only drops the
        // `tools[]` block). If it overflows too, shrink what the summarizer
        // sees and retry: first elide bulky tool results, then keep only the
        // most recent half/quarter/eighth of the conversation.
        let full: Vec<ChatMessage> = self.messages[1..tail_start].to_vec();
        let mut stage = 0usize;
        // Bounded retry (with the same backoff the main turn loop uses) for a
        // transient 429/503 hitting the summarization request itself — without
        // this, compaction (often triggered *because* the model is under
        // pressure) aborts the whole turn on a hiccup that a plain retry would
        // have ridden out. Separate from `stage`, which is about shrinking the
        // request on overflow, not retrying it unchanged.
        const MAX_COMPACT_RETRIES: usize = 3;
        let mut transient_attempt = 0usize;
        let summary = loop {
            let history = match stage {
                0 => full.clone(),
                1 => elide_tool_results(&full),
                n => tail_window(&elide_tool_results(&full), 1 << (n - 1)),
            };
            // The summarizer is sent no `tools` (it must answer in prose), but
            // `history` still carries tool_use/tool_result blocks from the
            // conversation being summarized — including, if a `/compact` lands
            // right after an Esc-cancelled tool round, a dangling `tool_calls`
            // message with no matching result. The native Anthropic backend
            // 400s on either shape unless `tools` is defined, so flatten the
            // protocol out of the request entirely rather than repairing it.
            let history = flatten_tool_protocol(&history);
            let mut req = Vec::with_capacity(history.len() + 2);
            req.push(ChatMessage::system(COMPACT_SYSTEM.to_string()));
            req.extend(history);
            req.push(ChatMessage::user(trigger.clone()));
            self.budget_preflight().await?;
            match self.plain_completion(req).await {
                Ok(s) => break s,
                Err(e) if is_context_overflow(&e) && stage < 4 => stage += 1,
                Err(e) if is_transient(&e) && transient_attempt < MAX_COMPACT_RETRIES => {
                    transient_attempt += 1;
                    let delay =
                        retry_after_hint(&e).unwrap_or_else(|| retry_backoff(transient_attempt));
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        };
        if summary.trim().is_empty() {
            bail!("compaction produced an empty summary");
        }

        // Replace history: the original (coding) system prompt, a user
        // message carrying the summary as the continuation seed, then the
        // recent tail verbatim.
        let system = self.messages[0].clone();
        let tail: Vec<ChatMessage> = self.messages[tail_start..].to_vec();
        let continuation = format!(
            "This session is being continued from an earlier conversation that ran out of \
             context. The summary below captures the older part of the conversation; the most \
             recent messages follow it verbatim. Continue from where they leave off without \
             losing any detail.\n\n{summary}"
        );
        let mut messages = Vec::with_capacity(2 + tail.len());
        messages.push(system);
        messages.push(ChatMessage::user(continuation));
        messages.extend(tail);
        self.messages = messages;
        // Most file contents the model had read live only in the summary now;
        // require fresh reads before further edits.
        self.reset_read_files();
        // A summarization call just succeeded — whatever previously made
        // `maybe_self_compact` latch itself off no longer holds, so let
        // proactive compaction try again instead of staying silently disabled
        // for the rest of the session (only `invalidate_context_window`, on a
        // model switch, used to clear this).
        self.self_compact_failed = false;
        Ok((before, self.messages.len()))
    }

    /// Run one no-tools request to completion, returning the streamed text.
    /// Silent: the shared [`drain_stream`] gets a no-op event sink.
    async fn plain_completion(&mut self, req: Vec<ChatMessage>) -> Result<String> {
        let mut stream = self.client.chat_stream(&req, &[]).await?;
        let acc = drain_stream(&mut stream, &mut |_| {}).await?;
        self.account_usage(&acc).await;
        Ok(acc.into_message().content.unwrap_or_default())
    }

    /// Compact this agent's own history when it is close to filling its context
    /// window — *before* the next request, rather than after one has failed.
    ///
    /// Every agent does this for itself — main, sub-agent, headless. Context
    /// management is the agent's own business, not a feature of whatever is
    /// watching it: an agent driven by no UI at all (the CLI, a delegated task)
    /// fills its window exactly like one with a frontend, and a 64k local model
    /// reading its way through a codebase gets there fast. Without this, such an
    /// agent's only safety net is overflow recovery, which fires *after* a request
    /// has already been rejected — paying for the round trip and risking the task.
    ///
    /// Failure is non-fatal: if the summarising call fails, the turn proceeds and
    /// overflow recovery is still there to catch it.
    pub(crate) async fn maybe_self_compact<F: FnMut(AgentEvent)>(&mut self, on_event: &mut F) {
        if !self.auto_compact || self.self_compact_failed || self.last_prompt_tokens.is_none() {
            return;
        }
        // Learn our own window before judging how full it is. Without this the
        // trigger is `None` for most agents and this whole path is dead code —
        // exactly for the small-context models it exists to protect.
        self.ensure_context_window();
        if !should_auto_compact(
            self.last_prompt_tokens,
            self.context_window,
            self.compaction_reserved,
            self.auto_compact,
        ) {
            return;
        }
        match self.compact(None).await {
            // `compact` clears `last_prompt_tokens` itself: the reading described
            // the history we just replaced, and leaving it set would re-trigger on
            // the next round against a history that is already small.
            Ok((before, after)) if before != after => {
                on_event(AgentEvent::Notice(format!(
                    "context was filling up — compacted {before} → {after} messages"
                )));
            }
            Ok(_) => {}
            Err(e) => {
                // Don't retry a summariser that failed for a reason a retry won't
                // fix: it would burn a model call and a notice on every round.
                // Overflow recovery is still there if the context really is too big.
                self.self_compact_failed = true;
                on_event(AgentEvent::Notice(format!(
                    "could not compact a filling context ({e}) — continuing"
                )));
            }
        }
    }
}
