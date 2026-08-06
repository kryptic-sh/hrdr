//! Compaction and context management — extracted from [`Agent`] into its own
//! module to keep `lib.rs` manageable.
//!
//! Holds the proactive/auto compaction machinery ([`Agent::compact`],
//! [`Agent::maybe_self_compact`], the summarizer prompt, tail-window selection)
//! and the rough token estimators the context gauge and triggers rely on.
//!
//! Compaction is the ONLY answer to a filling context. Pressure-gated tool-output
//! pruning lived here too and was removed: it invalidated the prompt prefix cache
//! every time it fired, could fire repeatedly across a conversation, still ended
//! in a compaction, and — unlike compaction — dropped information permanently.
//! One cache invalidation that summarizes beats several that only defer.

use anyhow::{Result, bail};
use hrdr_llm::{CompactionReason, ToolDef};

use std::sync::Arc;

use crate::{
    Agent, AgentEvent, AgentRegistry, ChatMessage, MessageOrigin, RetryBudget, Role, drain_stream,
    is_context_overflow,
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

/// Marks an agent as compacting for as long as it is, and clears the flag on every
/// exit — a summarization that fails or is cancelled must not leave its pane
/// spinning "compacting…" forever.
struct CompactingGuard(Option<(AgentRegistry, u64)>);

impl CompactingGuard {
    fn new(home: Option<(AgentRegistry, u64)>) -> Self {
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

/// How many times the model may answer the compaction request with a tool call
/// before compaction gives up. The request carries the session's `tools[]` (see
/// [`Agent::compact`]), so a tool call is possible even though the instruction
/// forbids it; it is never executed, only asked again.
pub(crate) const COMPACT_TOOL_CALL_ATTEMPTS: usize = 2;

/// User-turn instruction that triggers the structured summary.
const COMPACT_TRIGGER: &str = "\
Summarize the conversation so far. Write the summary as your reply: return prose only, \
and do not call a tool — a tool call cannot be a summary and will not be run. \
The summary REPLACES the full history, so it must \
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

/// What one compaction did, and what it cost.
///
/// The cache figures are the whole point of compacting against the live prefix
/// (see [`Agent::compact`]) and a cache hit cannot be unit-tested — it needs a
/// real provider and two sequential requests. So the mechanism reports on
/// itself: every compaction puts its cache-read fraction in the transcript, and
/// a run where that stays near zero says the change stopped working on the first
/// compaction rather than at the end of a billing period.
#[derive(Debug, Clone, Copy)]
pub struct CompactionReport {
    /// What triggered this compaction.
    pub reason: CompactionReason,
    /// History length before and after. Equal means nothing was compacted.
    pub before: usize,
    pub after: usize,
    /// Estimated prompt tokens the next turn would send after this
    /// compaction — the compacted system prompt + summary + preserved tail,
    /// plus the `tools[]` block, on the same ~4 bytes/token basis as every
    /// local figure. This is what the context gauge should show once the
    /// history has been rewritten: the previous reading described the
    /// pre-compaction history and must not be kept, and a cleared gauge
    /// claimed the context was empty. `0` on a no-op compaction, which the
    /// frontend ignores (the existing reading is still accurate).
    pub context_after: u32,
    /// Prompt tokens the summarization request was billed for, and how many of
    /// them the provider served from its cache. `None` means the provider
    /// reported no cache figure at all — which is not the same as zero, and
    /// must not be rendered as one.
    pub prompt_tokens: u32,
    pub cached_prompt_tokens: Option<u32>,
    /// The summary's own output tokens — unchanged by prefix caching, and the
    /// honest denominator for "what did this cost".
    pub output_tokens: u32,
    /// Estimated cost of the summarization call, when the model is priced.
    pub cost_usd: Option<f64>,
    /// Which rung of the shrink ladder produced the summary, and how many
    /// requests it took to get there.
    ///
    /// These are what make [`cached_prompt_tokens`](Self::cached_prompt_tokens)
    /// readable. Below [`ShrinkStage::Full`] a near-zero cache fraction is the
    /// shrink working as designed; after a retry a near-total one may be the
    /// identical previous request warming the cache rather than the live
    /// session's prefix matching. Without them the same number means opposite
    /// things and there is no way to tell which.
    pub stage: ShrinkStage,
    pub attempts: usize,
}

impl CompactionReport {
    /// The report for a compaction that had nothing to summarize.
    fn nothing_to_do(reason: CompactionReason, messages: usize) -> Self {
        Self {
            reason,
            before: messages,
            after: messages,
            context_after: 0,
            prompt_tokens: 0,
            cached_prompt_tokens: None,
            output_tokens: 0,
            cost_usd: None,
            stage: ShrinkStage::Full,
            attempts: 0,
        }
    }

    /// Whether this compaction actually shrank the history.
    pub fn shrank(&self) -> bool {
        self.after < self.before
    }

    /// The transcript line for this compaction — one renderer for every caller,
    /// so a `/compact`, a proactive pass and an overflow rescue cannot describe
    /// the same numbers in three drifting ways.
    ///
    /// The cost figures describe the summarization request ONLY. Compaction
    /// rewrites the history and refreshes the system prompt, so the turn after
    /// it starts cold no matter what this says.
    pub fn notice(&self) -> String {
        if !self.shrank() {
            return "nothing to compact yet".to_string();
        }
        let mut line = format!(
            "{} {} → {} messages",
            self.reason.as_str(),
            self.before,
            self.after
        );
        // What it took to get the summary, when that was not the plain case —
        // and it is not decoration: each of these changes how the cache
        // fraction below should be read.
        let mut how: Vec<String> = Vec::new();
        if self.attempts > 1 {
            how.push(format!("{} attempts", self.attempts));
        }
        if self.stage.rewrites_the_prompt() {
            how.push(format!("{} stage", self.stage.label()));
        }
        if !how.is_empty() {
            line.push_str(&format!(" · {}", how.join(", ")));
        }
        // Everything past here describes the summarization request and nothing
        // else, so it says so. Unscoped, "90% from cache" reads as a claim about
        // the session — while in fact the turn AFTER a compaction starts cold,
        // because compaction rewrites the history and refreshes the system
        // prompt. The figures are joined by commas rather than the `·` used
        // above, so the scope covers all of them rather than only the first.
        // The scoped block goes on its own line: the clause above can already
        // carry " · 2 attempts · full-history stage", and one long line wraps
        // awkwardly in the transcript.
        line.push('\n');
        line.push_str("summary call: ");
        match self.cached_prompt_tokens {
            Some(cached) if self.prompt_tokens > 0 => {
                let percent = f64::from(cached) / f64::from(self.prompt_tokens) * 100.0;
                line.push_str(&format!(
                    "{} prompt tokens, {percent:.0}% from cache",
                    self.prompt_tokens
                ));
            }
            // Absent and zero mean opposite things, and one of them looks like
            // this whole mechanism failed. Say which it is.
            _ => line.push_str(&format!(
                "{} prompt tokens, cache not reported",
                self.prompt_tokens
            )),
        }
        line.push_str(&format!(", {} output", self.output_tokens));
        if let Some(cost) = self.cost_usd {
            line.push_str(&format!(", ${cost:.4}"));
        }
        line
    }
}

/// The prose that introduces a reinjected summary.
///
/// Framing matters more than it looks. The model reading this has no memory of
/// the conversation the summary describes, so without being told what the text
/// IS it treats the contents as a plan rather than as a record — and redoes work
/// that is already finished, which is the characteristic failure of a compacted
/// agent. Two things it therefore has to say: the summary is a record of work
/// ALREADY DONE, and where the summary and the verbatim tail disagree the tail
/// wins.
///
/// That last point is load-bearing since the summarization request covers the
/// whole history rather than just the head: the tail is described by the summary
/// AND present verbatim, so the model sees the same events twice, at different
/// levels of detail and possibly with the summary's own inaccuracies.
///
/// `reason` opens it honestly — "ran out of context" is simply untrue of a
/// `/compact` the user chose to run.
fn continuation_framing(reason: CompactionReason, has_tail: bool) -> String {
    let opening = match reason {
        CompactionReason::UserRequested => "This conversation was compacted at the user's request.",
        CompactionReason::ContextFilling => {
            "This conversation was compacted because it was filling the context window."
        }
        CompactionReason::ContextOverflow => {
            "This conversation was compacted because it exceeded the context window."
        }
    };
    let tail = if has_tail {
        " The most recent messages follow it verbatim: where they and the summary describe the \
         same events, the verbatim messages are authoritative and the summary is the condensed \
         account."
    } else {
        ""
    };
    format!(
        "{opening} The summary below was written by a model reading the full history, and it \
         REPLACES that history — it is a record of what has already happened, not a plan.{tail} \
         Work the summary describes as done IS done: continue from where it leaves off, and do \
         not repeat it. If you need a detail the summary does not carry, read it from the \
         source rather than assuming."
    )
}

/// Max bytes of a tool-result body kept when shrinking a compaction request.
pub(crate) const ELIDE_TOOL_RESULT_BYTES: usize = 400;

/// How much of the history one compaction attempt sends — the ladder
/// [`Agent::compact`] escalates down when the summarization request is itself
/// too big for the context window.
///
/// A type rather than an index because advancing the ladder and running out of
/// it are one decision: [`next`](Self::next) returns `None` at the end, where an
/// increment sitting beside a separate `<` bound could drift from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShrinkStage {
    /// The whole history, verbatim — the only stage whose request still extends
    /// the prefix the live session just cached, and so the only one compacting
    /// in place is cheap at.
    Full,
    /// Bulky tool-result bodies truncated. Tool output is the usual context hog
    /// and the summarizer mostly needs the turns around it.
    Elided,
    /// The most recent half, quarter, eighth of the elided history.
    Half,
    Quarter,
    Eighth,
}

impl ShrinkStage {
    /// The ladder in order, for the sizing pass that picks a starting rung.
    const LADDER: [Self; 5] = [
        Self::Full,
        Self::Elided,
        Self::Half,
        Self::Quarter,
        Self::Eighth,
    ];

    /// The next-smaller stage, or `None` when there is nothing left to shrink.
    fn next(self) -> Option<Self> {
        match self {
            Self::Full => Some(Self::Elided),
            Self::Elided => Some(Self::Half),
            Self::Half => Some(Self::Quarter),
            Self::Quarter => Some(Self::Eighth),
            Self::Eighth => None,
        }
    }

    /// Whether this stage rewrites message bodies — and so gives up the prompt
    /// cache the whole in-place design exists to hit.
    ///
    /// This is what makes a compaction's cache fraction readable: below `Full`
    /// a near-zero reading is the shrink working as designed, not the cache
    /// failing.
    pub fn rewrites_the_prompt(self) -> bool {
        self != Self::Full
    }

    /// How a notice names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full-history",
            Self::Elided => "elided-history",
            Self::Half => "half-history",
            Self::Quarter => "quarter-history",
            Self::Eighth => "eighth-history",
        }
    }
}

/// The output allowance assumed when the endpoint reports no `max_tokens` at
/// all, so the shrink ladder still has a figure to reserve against. Only ever a
/// sizing estimate — nothing caps the response at this.
const COMPACT_ASSUMED_OUTPUT_TOKENS: u32 = 32_768;

/// Slack held back on top of the output allowance when sizing the summarization
/// request: the trigger message, and the estimator's own error. The system
/// prompt and `tools[]` are charged separately — see
/// [`Agent::first_viable_compact_stage`]'s `prefix_tokens`.
const COMPACT_INPUT_SLACK: u32 = 8_192;

/// The history a given shrink stage sends: the whole head, the head with bulky
/// tool results elided, then the most recent 1/2, 1/4, 1/8 of the elided head.
///
/// `elided` memoizes the (O(n), allocating) elision across stages, since every
/// stage above 0 starts from it.
fn compact_stage_history(
    stage: ShrinkStage,
    full: &[ChatMessage],
    elided: &mut Option<Vec<ChatMessage>>,
) -> Vec<ChatMessage> {
    if stage == ShrinkStage::Full {
        return full.to_vec();
    }
    let elided = elided.get_or_insert_with(|| elide_tool_results(full));
    let div = match stage {
        // `Full` returned above; naming it (rather than a catch-all) is what
        // makes a new stage a compile error here instead of silently taking
        // whichever arm was written last.
        ShrinkStage::Full | ShrinkStage::Elided => return elided.clone(),
        ShrinkStage::Half => 2,
        ShrinkStage::Quarter => 4,
        ShrinkStage::Eighth => 8,
    };
    carry_prior_summary(elided, tail_window(elided, div))
}

/// Put the previous compaction's summary back at the front of a `window` that
/// dropped it.
///
/// The shrink stages keep the most recent slice of the history, so they drop
/// its front — and that is exactly where the last compaction's summary sits (it
/// is always the message straight after the system prompt, so always `full[0]`
/// here). Dropping it does not shrink anything, because [`Agent::compact`]
/// replaces that message unconditionally: the summarizer would write a new
/// summary having never seen the old one, and everything before the previous
/// compaction's tail would be gone from the session for good. That is a silent
/// loss — nothing errors, the history simply starts later than it did.
///
/// Carrying it costs one message on a request that is being shrunk precisely
/// because it is too big; the summary is bounded by the summarizer's own output
/// and is the only record of the older half of the session, which makes it the
/// last thing worth dropping rather than the first.
fn carry_prior_summary(full: &[ChatMessage], window: Vec<ChatMessage>) -> Vec<ChatMessage> {
    // `tail_window` returns a suffix, so an equal length means nothing was cut
    // and the summary is already in there.
    if window.len() >= full.len() {
        return window;
    }
    let Some(summary) = full
        .first()
        .filter(|m| matches!(m.origin, MessageOrigin::Summary(_)))
    else {
        return window;
    };
    let mut carried = Vec::with_capacity(window.len() + 1);
    carried.push(summary.clone());
    carried.extend(window);
    carried
}

/// Whether `msg` begins a real user turn.
///
/// `Role::User` alone is not the question. hrdr pushes FOUR kinds of user-role
/// message through the one `push_user_message` chokepoint, and only one of them
/// is the user speaking: a [`MessageOrigin::Nudge`] is the harness prompting
/// itself about unfinished TODOs, a [`MessageOrigin::Tool`] is a detached
/// sub-agent's report arriving after its round closed, and a
/// [`MessageOrigin::Summary`] is compaction's own output. Counting those as
/// turns spends the verbatim tail budget on messages the user never sent —
/// worst on the busiest sessions, which are the ones compacting.
///
/// A steer is [`MessageOrigin::User`] and so does count: it is the user
/// speaking, only mid-turn. A steer relayed from a main agent to a sub-agent
/// is the same thing, because the main agent acts on the user's behalf.
pub(crate) fn is_user_turn(msg: &ChatMessage) -> bool {
    msg.role == Role::User && msg.origin == MessageOrigin::User
}

/// Index where the kept-verbatim tail begins for compaction. Keeps the last
/// `tail_turns` turns (a turn begins at a real user message — see
/// [`is_user_turn`]), but no more than `preserve_tokens` estimated tokens —
/// walking newest → oldest, adding whole turns until the budget is hit, always
/// keeping at least the newest turn. Never returns 0 (the system prompt stays);
/// the tail always begins on a user message, so no tool result is orphaned.
/// Everything in `1..start` gets summarized. Mirrors opencode's compaction tail
/// selection.
pub(crate) fn compaction_tail_start(
    msgs: &[ChatMessage],
    tail_turns: usize,
    preserve_tokens: u32,
) -> usize {
    if tail_turns == 0 {
        return msgs.len();
    }
    // Turn boundaries: real user messages after the system prompt.
    let starts: Vec<usize> = (1..msgs.len())
        .filter(|&i| is_user_turn(&msgs[i]))
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

/// The most recent `1/div` of `msgs` (at least two messages, or all of them
/// when there are fewer than two), aligned forward past any leading
/// `role:"tool"` results so no result is orphaned from its assistant
/// `tool_calls` message (strict servers reject that).
pub(crate) fn tail_window(msgs: &[ChatMessage], div: usize) -> Vec<ChatMessage> {
    let keep = (msgs.len() / div.max(1))
        .min(msgs.len())
        .max(2.min(msgs.len()));
    let start = align_past_tool_results(msgs, msgs.len() - keep);
    msgs[start..].to_vec()
}

/// Very rough token estimate (~4 characters per token) for `text`. Used only as
/// a fallback when the server reports no usage — good enough for the context bar
/// + auto-compaction, not for billing.
pub(crate) fn estimate_tokens(text: &str) -> u32 {
    (text.len() / 4) as u32
}

/// Estimate the prompt tokens of the request's `tools[]` block, on the same
/// ~4-bytes-per-token basis as [`estimate_tokens`]: each tool's name,
/// description and JSON-serialized parameter schema, plus a small per-tool
/// structural overhead for the wrapper object's keys.
///
/// These are counted because they *ride on every request*. hrdr advertises a
/// large tool surface whose schemas run to several thousand tokens, and the
/// server charges for them on every single call — so leaving them out of the
/// no-usage fallback understated `last_prompt_tokens` by a constant multi-thousand
/// token offset, exactly on the endpoints that have no counter of their own. That
/// figure drives auto-compaction, pruning and the frontends' context gauge, so the
/// omission made compaction fire late (a hard context-overflow 400, recoverable
/// only by burning a request) while the gauge quietly lied.
///
/// Serializing the schemas is not free, so callers estimate once per turn from
/// the `defs` the turn already built rather than per round.
pub(crate) fn estimate_tokens_in_tools(tools: &[ToolDef]) -> u32 {
    tools
        .iter()
        .map(|t| {
            let schema = serde_json::to_string(&t.function.parameters)
                .map(|s| s.len())
                .unwrap_or(0);
            (t.function.name.len() + t.function.description.len() + schema) as u32 / 4 + 4
        })
        .sum()
}

/// Estimate the prompt tokens of a whole request: each message's content and any
/// tool-call names/arguments, plus a small per-message overhead for the role and
/// structural tokens the chat template adds.
///
/// Messages only — the `tools[]` block is [`estimate_tokens_in_tools`]'s job,
/// because it is per-turn state rather than per-message and callers cache it.
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
    /// replace the history with `[system prompt, summary, …recent tail]`, so the
    /// context shrinks while continuity is preserved.
    ///
    /// `reason` is what triggered this; it rides on the summary message and on
    /// the returned [`CompactionReport`], so a caller's notice and a resumed
    /// session both know whether this was a `/compact` or a rescue.
    /// `instructions` optionally steers the summary's focus. A no-op report
    /// (`before == after`) means there was nothing beyond the system prompt and
    /// one message to summarize.
    ///
    /// `on_event` receives an [`AgentEvent::Usage`] per summarization attempt
    /// and nothing else — compaction stays silent about the summary's content,
    /// which is not the user's to read, but its model calls are accounted like
    /// any other. They were not: the tokens went into the session's cost total
    /// while its token counters never saw them, so `/cost` under-reported by
    /// exactly the largest calls a session makes.
    pub async fn compact<F: FnMut(AgentEvent)>(
        &mut self,
        reason: CompactionReason,
        instructions: Option<&str>,
        on_event: &mut F,
    ) -> Result<CompactionReport> {
        // Whatever the outcome, the last prompt reading describes the history as it
        // was *before* this call. Clearing it here (rather than in one caller) stops
        // a frontend-driven `/compact` from leaving a stale, over-the-trigger figure
        // that makes the agent immediately compact the history it just compacted.
        self.last_prompt_tokens = None;
        let before = self.messages.len();
        if before <= 2 {
            return Ok(CompactionReport::nothing_to_do(reason, before));
        }
        // The agent is the one that knows it is summarizing — including when it
        // decided to on its own, which no frontend is told about. The guard clears
        // the flag on every exit, error and cancellation included.
        let _compacting = CompactingGuard::new(self.live_home.clone());
        // A `/compact` landing right after an Esc-cancelled tool round leaves an
        // assistant `tool_calls` message with no results, which strict servers
        // reject. Backfill the stubs the same way the turn loop does at turn
        // start, rather than flattening the tool protocol out of the request:
        // flattening rewrote every message and so guaranteed a cache miss, and
        // with the session's `tools[]` now in the request there is nothing to
        // flatten it for.
        //
        // BEFORE the tail is chosen, not after: the repair inserts messages, so
        // an index taken first would slide backwards underneath it and could
        // land the tail on an orphaned tool result.
        crate::turn_loop::repair_dangling_tool_calls(Arc::make_mut(&mut self.messages));
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
                return Ok(CompactionReport::nothing_to_do(reason, before));
            }
        }

        // The request is an ORDINARY turn: the session's own system prompt, its
        // own `tools[]`, its own history, with the instruction appended as one
        // more user message. Nothing about its shape differs from the request
        // sent a moment ago, so the provider's prefix cache still matches and
        // compaction pays cached rates on the history instead of full rate on
        // all of it — at the most expensive moment in a session, and again on
        // each shrink stage.
        //
        // It used to be a one-off: a dedicated summarizer system prompt, the
        // head only, and no `tools[]` at all. Four independent reasons the
        // cached prefix could not match.
        //
        // `tool_choice` is deliberately NOT used to forbid a tool call.
        // Anthropic's cache hierarchy is tools → system → messages, and their
        // documentation has `tool_choice` invalidating message blocks — it
        // would keep the cheap half valid and throw away the expensive half,
        // exactly backwards for a long conversation. The instruction plus the
        // guard on the response below cover it, and cost nothing anywhere.
        let mut trigger = COMPACT_TRIGGER.to_string();
        if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
            trigger.push_str("\n\nAdditional instructions for the summary, follow them closely:\n");
            trigger.push_str(extra);
        }
        // The whole history, not just the head. Truncating at `tail_start`
        // would end the request in the middle of the conversation, where no
        // earlier request ended and so no cached prefix reaches — the point of
        // this call is that its prefix is the one that was just cached. The
        // tail being summarized as well as kept verbatim is redundant, not
        // wrong: the summary covers the session and the tail follows it in
        // full.
        let defs = self.tools.defs();
        // When compaction is overflow-triggered, the summarization request is
        // itself over the limit. If it overflows, shrink what the summarizer
        // sees and retry: first elide bulky tool results, then keep only the
        // most recent half/quarter/eighth of the conversation. Every stage
        // above 0 rewrites message bodies and so gives the cache up — which is
        // the cost of a rescue, and the reason proactive compaction (which
        // fires while stage 0 still fits) is where the saving lands.
        let full: Vec<ChatMessage> = self.messages[1..].to_vec();
        // Elision is O(history) and allocates a copy of it; the stages above 0
        // all start from the same elided copy, so build it at most once instead
        // of once per attempt (stage 3 used to rebuild it, then window it).
        let mut elided: Option<Vec<ChatMessage>> = None;
        // Start at the first stage that stands a chance rather than always at
        // stage 0. Compaction is usually triggered *by* a context overflow, so
        // the summarization request is near the limit too — and each doomed
        // attempt is a full upload of the whole history to be told, one round
        // trip later, what could have been computed locally. The estimator
        // under-counts (~4 bytes/token, and code is denser than that), so a
        // wrong guess errs toward starting too early, which the escalation
        // below still handles: this only skips stages that plainly cannot fit.
        // The prefix rides on every stage and cannot be shrunk by any of them,
        // so it comes off the budget rather than being sized against it.
        let prefix_tokens = estimate_tokens_in_messages(&self.messages[..1])
            .saturating_add(estimate_tokens_in_tools(&defs));
        let mut stage = self.first_viable_compact_stage(&full, &mut elided, prefix_tokens);
        // Bounded retry (with the same backoff the main turn loop uses) for a
        // transient 429/503 hitting the summarization request itself — without
        // this, compaction (often triggered *because* the model is under
        // pressure) aborts the whole turn on a hiccup that a plain retry would
        // have ridden out. Separate from `stage`, which is about shrinking the
        // request on overflow, not retrying it unchanged.
        //
        // Its own budget, deliberately: summarizing is a *different* model call
        // from the round that may have triggered it, so a round that already
        // burned nine retries has not thereby decided that compaction gets
        // none. The budgets that must not stack are the ones retrying the SAME
        // request (connect and drain) — see `connect_and_drain`.
        let mut budget = RetryBudget::new(self.retry_policy);
        // Attempts spent on a model that answered with a tool call instead of
        // the summary. Counted apart from `budget`, which only retries
        // transient network and server failures — this is neither.
        let mut tool_call_attempts = 0usize;
        // Every request this compaction issued, whatever became of it. Reported
        // so a reader can tell a cache fraction warmed by an identical retry
        // from one the live session's prefix actually produced.
        let mut attempts = 0usize;
        // The summary and what the attempt that produced it was billed, bound
        // together: a spend exists only where a request succeeded, so there is
        // no zeroed placeholder to mistake for a real reading.
        let (summary, spend) = loop {
            attempts += 1;
            let history = compact_stage_history(stage, &full, &mut elided);
            let mut req = Vec::with_capacity(history.len() + 2);
            req.push(self.messages[0].clone());
            req.extend(history);
            // The instruction exists only in this request. The rebuilt history
            // below is assembled from the system prompt, the summary and the
            // tail — so a fake "summarize the conversation so far" user turn
            // cannot survive into the session, which the model would otherwise
            // read as something the user asked for.
            req.push(ChatMessage::user(trigger.clone()));
            self.budget_preflight().await?;
            match self.plain_completion(req, &defs, on_event).await {
                Ok((msg, spend)) => {
                    // A tool call returned during compaction is NEVER executed.
                    // The tools are in the request for the prefix cache, not
                    // for use: running one here would be a side effect the user
                    // never asked for, at the worst possible moment in a
                    // session. Treat it as a failed attempt and ask again.
                    if msg.tool_calls.is_some_and(|calls| !calls.is_empty()) {
                        tool_call_attempts += 1;
                        if tool_call_attempts > COMPACT_TOOL_CALL_ATTEMPTS {
                            bail!(
                                "the model answered the compaction request with a tool call \
                                 {tool_call_attempts} times instead of writing the summary — \
                                 refusing to replace the conversation"
                            );
                        }
                        continue;
                    }
                    break (msg.content.unwrap_or_default(), spend);
                }
                Err(e) if is_context_overflow(&e) => match stage.next() {
                    Some(smaller) => stage = smaller,
                    // Out of ladder: the request cannot be made to fit, and the
                    // retry budget below would refuse a non-transient error
                    // anyway. Say so here rather than routing it through a
                    // helper that only looks like it might retry.
                    None => return Err(e),
                },
                Err(e) => {
                    // Silent: `compact` has no event sink (it is called from
                    // overflow recovery and from `/compact` alike), so the retry
                    // is reported to nobody — the caller's own notice covers the
                    // outcome. The driver still sleeps and counts.
                    if !budget.retry(&e, &mut |_| {}).await {
                        return Err(e);
                    }
                }
            }
        };
        if summary.trim().is_empty() {
            bail!("compaction produced an empty summary");
        }

        // Replace history: the system prompt, a user message carrying the
        // summary as the continuation seed, then the recent tail verbatim.
        //
        // The system prompt is rebuilt rather than cloned, so the memory index
        // reflects notes saved during THIS session. It has to happen here
        // specifically: the agent's own `memory` write is visible to it only as a
        // tool exchange in the history, and compaction is the moment that exchange
        // is summarized away. Cloning the old prompt forward would leave the note
        // on disk, absent from the index, and no longer in the conversation —
        // saved and then invisible. Refreshing costs nothing here because
        // compaction rewrites the history anyway, so the prefix cache is already
        // gone. (A *running* session is otherwise never re-seeded — see
        // `refresh_system`'s callers.)
        self.refresh_system_prompt_in_place();
        let system = self.messages[0].clone();
        let tail: Vec<ChatMessage> = self.messages[tail_start..].to_vec();
        let continuation = format!(
            "{}\n\n{summary}",
            continuation_framing(reason, !tail.is_empty())
        );
        let mut messages = Vec::with_capacity(2 + tail.len());
        messages.push(system);
        // Tagged, not a plain user message. Nothing but the prose opening used
        // to mark it apart from something the user typed, and the code that
        // asks "is this a turn?" asked `role == User` — so once the summary was
        // old enough to fall in the head, the next compaction summarized it
        // again, degrading a summary of a summary with nothing erroring.
        //
        // The tag also makes the one-summary invariant hold by construction:
        // the tail always begins at a real user turn, which is always after
        // index 1, so an existing summary is always in the head. The summarizer
        // therefore sees it as context and folds it into the new summary, and
        // this line replaces it. Exactly one summary is in history at any time,
        // always covering the session start through the tail.
        messages.push(ChatMessage {
            origin: MessageOrigin::Summary(reason),
            ..ChatMessage::user(continuation)
        });
        messages.extend(tail);
        self.messages = Arc::new(messages);
        // Most file contents the model had read live only in the summary now;
        // require fresh reads before further edits.
        self.reset_read_files();
        // A summarization call just succeeded — whatever previously made
        // `maybe_self_compact` latch itself off no longer holds, so let
        // proactive compaction try again instead of staying silently disabled
        // for the rest of the session (only `invalidate_context_window`, on a
        // model switch, used to clear this).
        self.self_compact_failed_at = None;
        // What the next turn would actually send: the compacted system +
        // summary + tail, plus the tools block. The pre-compaction provider
        // reading described the history that was just replaced, so this
        // estimate is the figure a frontend gauge should show once it lands.
        let context_after = estimate_tokens_in_messages(&self.messages)
            .saturating_add(estimate_tokens_in_tools(&self.tools.defs()));
        Ok(CompactionReport {
            reason,
            before,
            after: self.messages.len(),
            context_after,
            prompt_tokens: spend.prompt_tokens,
            cached_prompt_tokens: spend.cached_prompt_tokens,
            output_tokens: spend.completion_tokens,
            cost_usd: spend.cost_usd,
            stage,
            attempts,
        })
    }

    /// The first shrink stage whose request plausibly fits the context window,
    /// so the doomed attempts before it are never uploaded.
    ///
    /// `None` window (an endpoint that never said, and no configured value)
    /// means there is nothing to size against: start at 0 and let the escalation
    /// discover the limit the slow way, exactly as before.
    ///
    /// `prefix_tokens` is what the request carries no matter which stage it
    /// sends — the session's system prompt and its `tools[]`. No stage can
    /// shrink either, so they come off the budget rather than being sized
    /// against it.
    ///
    /// The output allowance comes off it too, and is the session's own
    /// `max_tokens` because [`Agent::plain_completion`] no longer overrides it.
    /// A model whose endpoint reports none is sized against
    /// [`COMPACT_ASSUMED_OUTPUT_TOKENS`] instead — a guess for the ladder, not a
    /// limit on the response.
    fn first_viable_compact_stage(
        &self,
        full: &[ChatMessage],
        elided: &mut Option<Vec<ChatMessage>>,
        prefix_tokens: u32,
    ) -> ShrinkStage {
        let Some(window) = self.context_window else {
            return ShrinkStage::Full;
        };
        let headroom = self
            .client
            .params()
            .max_tokens
            .unwrap_or(COMPACT_ASSUMED_OUTPUT_TOKENS)
            .saturating_add(COMPACT_INPUT_SLACK);
        let budget = window
            .saturating_sub(headroom)
            .saturating_sub(prefix_tokens);
        if budget == 0 {
            // A window smaller than the headroom: nothing can be sized, and
            // guessing the smallest stage would throw away the history for a
            // model that may still have room. Let the escalation decide.
            return ShrinkStage::Full;
        }
        ShrinkStage::LADDER
            .into_iter()
            .find(|&stage| {
                estimate_tokens_in_messages(&compact_stage_history(stage, full, elided)) <= budget
            })
            .unwrap_or(ShrinkStage::Eighth)
    }

    /// Run one request to completion, returning the assistant message the model
    /// streamed back and what the call was billed. Silent: the shared
    /// [`drain_stream`] gets a no-op event sink.
    ///
    /// The whole message, not just its text, because the caller has to see
    /// whether the model called a tool — [`Agent::compact`] sends the session's
    /// `tools[]` for the prefix cache and must never execute what comes back.
    ///
    /// A parameter this endpoint rejects is dropped and the request retried,
    /// through the same [`Agent::drop_unsupported_param`] the turn path uses.
    /// The loop terminates because each rejection can be honoured once: a
    /// parameter already dropped makes the helper return `false` and the error
    /// propagate. No other 400 is retried here.
    async fn plain_completion<F: FnMut(AgentEvent)>(
        &mut self,
        req: Vec<ChatMessage>,
        defs: &[ToolDef],
        on_event: &mut F,
    ) -> Result<(ChatMessage, crate::usage::CallSpend)> {
        // Compaction can run before any normal turn (notably `/compact` after
        // resuming a process), so it cannot rely on `connect_stream` having
        // refreshed and injected ChatGPT OAuth. Do it at the actual request
        // boundary, after every no-op compaction exit and before cloning the
        // client. The non-OAuth branch also retains the shared helper's stale
        // account-header cleanup semantics.
        self.refresh_oauth_if_needed().await;
        loop {
            // NOTHING about the request parameters is overridden, and that is
            // the point: the compaction call has to be byte-identical to an
            // ordinary turn or the prefix cache [`Agent::compact`] exists to hit
            // is gone. Anthropic's invalidation table puts `output_config.effort`
            // and the `thinking` config in the same class as `tool_choice` —
            // each "always invalidates message blocks", and on models that
            // render the config ahead of them, `tools` and `system` too.
            //
            // `max_tokens` is not on that table itself, and this call used to cap
            // it as a runaway-summarizer guard. That guard was removed because on
            // the MANUAL thinking dialect the cap was not neutral: `thinking_budget`
            // in `hrdr-llm`'s Anthropic backend derives `thinking.budget_tokens`
            // from `max_tokens`, so capping the output rewrote the thinking block
            // and cost the cache on every one of those models. The summarizer now
            // runs with the session's own allowance, which is what makes the
            // request identical; a summary that runs to that limit is still
            // refused rather than accepted truncated, below.
            let client = self.client.clone();
            let output_cap = client.params().max_tokens;
            match self
                .plain_completion_inner(&client, &req, defs, output_cap, on_event)
                .await
            {
                Err(error) if self.drop_unsupported_param(&error) => continue,
                result => return result,
            }
        }
    }

    async fn plain_completion_inner<F: FnMut(AgentEvent)>(
        &mut self,
        client: &hrdr_llm::Client,
        req: &[ChatMessage],
        defs: &[ToolDef],
        output_cap: Option<u32>,
        on_event: &mut F,
    ) -> Result<(ChatMessage, crate::usage::CallSpend)> {
        let mut stream = client.chat_stream(req, defs).await?;
        // The stream's own sink is a no-op — the summary's text is not the
        // user's to read — but the round is TIMED like any other, because its
        // tokens are reported below and throughput measured over tokens with
        // no time against them is not throughput.
        let drained = drain_stream(&mut stream, &mut |_| {}).await?;
        let acc = drained.acc;
        // The compaction request carries the session's own `tools[]` — the same
        // surface a normal round pays for, and the same estimate when the server
        // reports no usage.
        let spend = self
            .account_usage(&acc, estimate_tokens_in_tools(defs))
            .await;
        // Emitted per ATTEMPT, not once per compaction. A retry after the model
        // answered with a tool call is a billed call like any other, and the
        // counters have to see it or they under-report exactly when compaction
        // cost most.
        on_event(AgentEvent::Usage {
            prompt_tokens: spend.prompt_tokens,
            completion_tokens: spend.completion_tokens,
            decode_ms: drained.decode.as_millis().min(u32::MAX as u128) as u32,
            cached_prompt_tokens: spend.cached_prompt_tokens,
            cache_creation_tokens: spend.cache_creation_tokens,
            reasoning_tokens: acc.usage.as_ref().and_then(|u| u.reasoning_tokens()),
            cost_usd: spend.cost_usd,
            session_cost_usd: spend.session_cost_usd,
            cost_partial: self.session_cost_partial(),
        });
        // A summary cut off at the output cap is the one truncation that must
        // never be accepted quietly: this text REPLACES the conversation, so
        // half of it is not a degraded answer but a permanently missing half of
        // the session — and it would read as complete to every later turn.
        // Refusing leaves the real history in place, which is recoverable.
        if acc.truncated() {
            if let Some(output_cap) = output_cap {
                bail!(
                    "the summary was cut off at the output limit ({output_cap} tokens) — \
                     refusing to replace the conversation with a partial summary"
                );
            }
            bail!(
                "the uncapped summary ended at the model's output limit — refusing to replace \
                 the conversation with a partial summary"
            );
        }
        Ok((acc.into_message(), spend))
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
        if !self.auto_compact || self.last_prompt_tokens.is_none() {
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
        if self.self_compact_suppressed() {
            return;
        }
        // `compact` clears `last_prompt_tokens` on entry whatever the outcome, so
        // the reading the failure happened at has to be captured before the call
        // — read it afterwards and it is always `None`, the suppression never
        // engages, and a failing summarizer is retried on every single round.
        let attempted_at = self.last_prompt_tokens;
        match self
            .compact(CompactionReason::ContextFilling, None, on_event)
            .await
        {
            // `compact` clears `last_prompt_tokens` itself: the reading described
            // the history we just replaced, and leaving it set would re-trigger on
            // the next round against a history that is already small.
            Ok(report) if report.shrank() => {
                on_event(AgentEvent::Notice(report.notice()));
            }
            Ok(_) => {}
            Err(e) => {
                // Don't retry a summariser that failed for a reason a retry won't
                // fix: it would burn a model call and a notice on every round.
                // Overflow recovery is still there if the context really is too big.
                self.self_compact_failed_at = attempted_at;
                on_event(AgentEvent::Notice(format!(
                    "could not compact a filling context ({e}) — continuing"
                )));
            }
        }
    }

    /// Whether an earlier proactive-compaction failure still suppresses this
    /// attempt.
    ///
    /// Bounded, unlike the flag it replaces. A failure used to disable proactive
    /// compaction for the entire session, and the only thing that could re-enable
    /// it was a *successful* `compact` — which nothing else would call, because
    /// the caller that would have was the one just disabled. The suppression
    /// therefore lifted only after the context had actually overflowed and
    /// reactive recovery had cleaned up, which is the event proactive compaction
    /// exists to prevent.
    ///
    /// Now the failure is remembered against the context reading it happened at,
    /// and a re-probe is allowed once usage has grown by
    /// [`Self::self_compact_retry_growth`] since — so a summarizer that failed
    /// for a passing reason gets another chance while the window is still
    /// filling, and one that is genuinely broken costs a bounded handful of
    /// calls rather than one per round.
    fn self_compact_suppressed(&self) -> bool {
        let (Some(failed_at), Some(now)) = (self.self_compact_failed_at, self.last_prompt_tokens)
        else {
            return false;
        };
        now < failed_at.saturating_add(self.self_compact_retry_growth())
    }

    /// How much the context must grow after a failed proactive compaction before
    /// another is attempted.
    ///
    /// A sixteenth of the window, against a headroom above the trigger that
    /// [`compaction_trigger`] caps at a quarter of it — so at most four further
    /// attempts before the request overflows and reactive recovery takes the
    /// wheel. An unknown window suppresses indefinitely, which is unreachable in
    /// practice: `should_auto_compact` has already required one.
    fn self_compact_retry_growth(&self) -> u32 {
        self.context_window.map_or(u32::MAX, |window| window / 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_llm::ChatMessage;

    /// A message of roughly `tokens` estimated size (~4 bytes per token).
    fn sized(role: Role, tokens: usize) -> ChatMessage {
        let body = "x".repeat(tokens * 4);
        match role {
            Role::Tool => ChatMessage::tool_result("call-1", body),
            Role::User => ChatMessage::user(body),
            _ => ChatMessage::assistant(body),
        }
    }

    /// The stage ladder: full history, elided tool results, then successively
    /// smaller tails — and the elision is computed once and reused, not rebuilt
    /// per stage.
    #[test]
    fn compact_stage_history_shrinks_and_memoizes_the_elision() {
        let full: Vec<ChatMessage> = (0..8)
            .map(|i| {
                if i % 2 == 0 {
                    sized(Role::User, 100)
                } else {
                    sized(Role::Tool, 1_000)
                }
            })
            .collect();
        let mut elided = None;

        let whole = compact_stage_history(ShrinkStage::Full, &full, &mut elided);
        assert_eq!(whole.len(), full.len(), "the first stage is the whole head");
        assert!(
            elided.is_none(),
            "the first stage must not pay for the elision"
        );

        let elided_stage = compact_stage_history(ShrinkStage::Elided, &full, &mut elided);
        assert_eq!(
            elided_stage.len(),
            full.len(),
            "elision keeps every message"
        );
        assert!(elided.is_some(), "the elided copy is memoized");
        assert!(
            estimate_tokens_in_messages(&elided_stage) < estimate_tokens_in_messages(&whole),
            "eliding tool results must shrink the request"
        );

        // Each later stage keeps a smaller tail, and none of them rebuilds the
        // elided copy (the memo is already populated and is reused as-is).
        let half = compact_stage_history(ShrinkStage::Half, &full, &mut elided);
        let quarter = compact_stage_history(ShrinkStage::Quarter, &full, &mut elided);
        assert!(half.len() <= elided_stage.len());
        assert!(quarter.len() <= half.len());

        // The ladder ends, and that is how the escalation knows to stop rather
        // than by comparing against a bound written down somewhere else.
        assert_eq!(ShrinkStage::Eighth.next(), None);
        assert_eq!(ShrinkStage::LADDER.last(), Some(&ShrinkStage::Eighth));
    }

    /// A shrink stage drops the front of the history — but never the previous
    /// compaction's summary.
    ///
    /// That summary is the only record of everything before the last
    /// compaction, and `compact` replaces it whatever the summarizer saw. Let a
    /// shrink window drop it and the older half of the session is gone with
    /// nothing erroring: the history simply starts later than it used to.
    #[test]
    fn a_shrink_stage_carries_the_previous_summary_it_would_otherwise_drop() {
        let mut full = vec![ChatMessage {
            origin: MessageOrigin::Summary(CompactionReason::ContextOverflow),
            ..ChatMessage::user("EVERYTHING BEFORE THE LAST COMPACTION")
        }];
        full.extend((0..16).map(|i| {
            if i % 2 == 0 {
                sized(Role::User, 100)
            } else {
                sized(Role::Tool, 1_000)
            }
        }));

        let carried = |stage: ShrinkStage| -> Vec<ChatMessage> {
            let mut elided = None;
            compact_stage_history(stage, &full, &mut elided)
        };
        // The first two stages send every message, so the summary is already
        // there.
        for stage in [ShrinkStage::Full, ShrinkStage::Elided] {
            assert_eq!(carried(stage).len(), full.len(), "{}", stage.label());
        }
        // The windowing stages cut the front. Each must still open with the
        // summary, and must not have grown a second copy of it.
        for stage in [ShrinkStage::Half, ShrinkStage::Quarter, ShrinkStage::Eighth] {
            let label = stage.label();
            let history = carried(stage);
            assert!(
                history.len() < full.len(),
                "precondition: the {label} stage shrinks the history"
            );
            assert_eq!(
                history[0].origin,
                MessageOrigin::Summary(CompactionReason::ContextOverflow),
                "the {label} stage dropped the only record of the older session"
            );
            assert_eq!(
                history
                    .iter()
                    .filter(|m| matches!(m.origin, MessageOrigin::Summary(_)))
                    .count(),
                1,
                "the {label} stage carried it twice"
            );
        }

        // A history with no prior summary is untouched: nothing is prepended to
        // the window just because it was cut.
        let plain: Vec<ChatMessage> = full[1..].to_vec();
        let mut elided = None;
        let history = compact_stage_history(ShrinkStage::Eighth, &plain, &mut elided);
        assert!(
            history
                .iter()
                .all(|m| !matches!(m.origin, MessageOrigin::Summary(_))),
            "nothing to carry, nothing carried"
        );
    }

    /// The notice says what it took to get the summary, because that is what
    /// decides how to read its cache fraction.
    ///
    /// The same percentage means opposite things depending on two facts the
    /// line used to omit. Below `Full` the request rewrites message bodies, so
    /// a near-zero reading is the shrink ladder working, not the cache failing.
    /// And after a retry the winning attempt's prefix was warmed by the
    /// identical attempt before it, so a near-total reading may say nothing
    /// about the live session's prefix at all. Neither is decoration: without
    /// them a reader cannot tell a healthy compaction from a broken one, which
    /// is the only reason the fraction is printed.
    #[test]
    fn the_notice_says_what_it_took_to_produce_the_summary() {
        let report = |stage, attempts| CompactionReport {
            reason: CompactionReason::ContextOverflow,
            before: 40,
            after: 6,
            context_after: 0,
            prompt_tokens: 1_000,
            cached_prompt_tokens: Some(30),
            output_tokens: 900,
            cost_usd: None,
            stage,
            attempts,
        };

        // The plain case says neither — one attempt at the full history is what
        // every healthy compaction does, and naming it would be noise.
        let plain = report(ShrinkStage::Full, 1).notice();
        assert!(!plain.contains("attempts"), "{plain}");
        assert!(!plain.contains("stage"), "{plain}");
        assert!(plain.contains("3% from cache"), "{plain}");

        // A rescue that escalated: the stage explains the 3%.
        let escalated = report(ShrinkStage::Quarter, 1).notice();
        assert!(
            escalated.contains("quarter-history stage"),
            "a low cache fraction below the full stage is expected, and the line \
             has to say so: {escalated}"
        );
        assert!(!escalated.contains("attempts"), "{escalated}");

        // A retried one: the attempt count explains a fraction that may have
        // been warmed by the previous attempt rather than by the session.
        let retried = report(ShrinkStage::Full, 2).notice();
        assert!(retried.contains("2 attempts"), "{retried}");
        assert!(!retried.contains("stage"), "{retried}");

        // Both, and they read as one clause rather than two stray fields.
        let both = report(ShrinkStage::Eighth, 3).notice();
        assert!(both.contains("3 attempts, eighth-history stage"), "{both}");
    }

    /// The reinjected summary is framed as a RECORD, and says which of the two
    /// accounts of the overlapping messages wins.
    ///
    /// Without framing, a model with no memory of the conversation reads the
    /// summary as a plan and redoes finished work — the characteristic failure
    /// of a compacted agent. And since the summarization request now covers the
    /// whole history rather than just the head, the tail is described by the
    /// summary AND present verbatim; the model has to be told which to trust.
    #[test]
    fn the_reinjected_summary_is_framed_for_the_model_that_reads_it() {
        let framed = continuation_framing(CompactionReason::UserRequested, true);
        assert!(framed.contains("at the user's request"), "{framed}");
        assert!(
            framed.contains("record of what has already happened, not a plan"),
            "{framed}"
        );
        assert!(framed.contains("do not repeat it"), "{framed}");
        assert!(
            framed.contains("verbatim messages are authoritative"),
            "the tail wins where the two overlap: {framed}"
        );

        // No tail kept: there is nothing for the summary to disagree with, so
        // the precedence sentence would be describing messages that aren't there.
        let no_tail = continuation_framing(CompactionReason::ContextOverflow, false);
        assert!(no_tail.contains("exceeded the context window"), "{no_tail}");
        assert!(!no_tail.contains("verbatim"), "{no_tail}");

        // The opening states the real trigger — "ran out of context" is simply
        // untrue of a compaction the user chose to run.
        assert!(
            continuation_framing(CompactionReason::ContextFilling, true)
                .contains("filling the context window")
        );
    }

    /// Pre-sizing skips the stages that cannot fit, so a doomed request is never
    /// uploaded — but only when a window is actually known, and it never
    /// over-shrinks past the last stage.
    #[test]
    fn first_viable_stage_skips_what_cannot_fit() {
        let mut agent = crate::Agent::new(crate::AgentConfig::default()).unwrap();
        // ~40k estimated tokens of tool output in the history.
        let full: Vec<ChatMessage> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    sized(Role::User, 50)
                } else {
                    sized(Role::Tool, 8_000)
                }
            })
            .collect();

        // No window: nothing to size against, so behaviour is unchanged.
        agent.set_context_window(None);
        let mut elided = None;
        assert_eq!(
            agent.first_viable_compact_stage(&full, &mut elided, 0),
            ShrinkStage::Full
        );

        // A window with ample room for the whole history still starts at 0.
        agent.set_context_window(Some(400_000));
        let mut elided = None;
        assert_eq!(
            agent.first_viable_compact_stage(&full, &mut elided, 0),
            ShrinkStage::Full
        );

        // Headroom tracks the session's own output allowance rather than a
        // fixed cap, so read it off the agent instead of restating it here.
        let headroom = |agent: &crate::Agent| {
            agent
                .client
                .params()
                .max_tokens
                .unwrap_or(COMPACT_ASSUMED_OUTPUT_TOKENS)
                .saturating_add(COMPACT_INPUT_SLACK)
        };

        // A window the whole history cannot fit skips straight past the first
        // stage.
        let window = headroom(&agent) + 20_000;
        agent.set_context_window(Some(window));
        let mut elided = None;
        let stage = agent.first_viable_compact_stage(&full, &mut elided, 0);
        assert!(
            stage.rewrites_the_prompt(),
            "a doomed first stage must be skipped, got {stage:?}"
        );

        // The prefix — system prompt and tools — rides on every stage, so it
        // comes off the budget: a window that fits the history alone does not
        // fit it once the prefix is charged.
        let window = headroom(&agent) + 60_000;
        agent.set_context_window(Some(window));
        let mut elided = None;
        assert_eq!(
            agent.first_viable_compact_stage(&full, &mut elided, 0),
            ShrinkStage::Full
        );
        let mut elided = None;
        assert!(
            agent
                .first_viable_compact_stage(&full, &mut elided, 40_000)
                .rewrites_the_prompt(),
            "the prefix must be charged against the same budget"
        );

        // The summarizer runs with the session's own `max_tokens` (nothing caps
        // it any more), so the allowance it reserves has to move with it: the
        // window that just fit at the smaller allowance stops fitting once the
        // session asks for more output.
        let params = hrdr_llm::RequestParams {
            max_tokens: Some(headroom(&agent) + 40_000),
            ..agent.client.params().clone()
        };
        agent.client.set_params(params);
        let mut elided = None;
        assert!(
            agent
                .first_viable_compact_stage(&full, &mut elided, 0)
                .rewrites_the_prompt(),
            "the output allowance must be charged against the same budget"
        );

        // A window smaller than the headroom has nothing to size against and
        // must not throw the history away on a guess.
        agent.set_context_window(Some(1_000));
        let mut elided = None;
        assert_eq!(
            agent.first_viable_compact_stage(&full, &mut elided, 0),
            ShrinkStage::Full
        );
    }
}
