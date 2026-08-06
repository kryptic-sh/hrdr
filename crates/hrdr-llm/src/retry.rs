//! Retrying a failed model call: what is worth retrying, how long to wait
//! before the next attempt, and how many attempts one operation gets.
//!
//! This lives beside [`ChatError`](crate::ChatError) because a retry decision is
//! classification plus a clock, and classification is already here — the same
//! reason [`catalog`](crate::catalog) and hrdr-agent's OAuth refresh (neither of
//! which retries anything today) can adopt it without a new dependency.
//!
//! It exists because the same five lines — classify, pick a delay, tell the
//! user, sleep, count — used to be written out three times: the connect loop and
//! the mid-stream drain loop in hrdr-agent's turn loop, and compaction's
//! summarizer call. Each carried its own `const MAX_*_RETRIES`, and two of them
//! *nested*: `connect_and_drain` looped up to 4 times around a `connect_stream`
//! that itself issued up to 5 requests, so one assistant round could fire 20
//! requests at a struggling provider while neither constant said a number
//! larger than 4. A budget is a property of the operation, not of whichever loop
//! happens to be written around it — so it is a value the caller threads through
//! ([`RetryBudget`]), not a `const` re-declared at each site.

use std::time::Duration;

/// Attempts — not retries — one logical operation gets before it gives up.
///
/// Each wait doubles from [`FIRST_BACKOFF`] and is capped at [`MAX_BACKOFF`],
/// so the budget adds up to several minutes of riding out a provider's bad
/// afternoon. The total is not written out here — it is derived from those three
/// constants and would rot the moment one of them moved; it is pinned instead by
/// `default_budget_outlasts_a_rate_limit_window`, which fails if a change to any
/// of the three drags it out of range. The per-loop budgets this replaced gave
/// up after a few seconds of backoff, far shorter than most rate-limit windows.
const MAX_ATTEMPTS: usize = 10;

/// The wait after the first failure; each subsequent wait doubles it.
const FIRST_BACKOFF: Duration = Duration::from_secs(5);

/// Ceiling on the wait before the next attempt.
///
/// It caps the doubling backoff *and* a server's own `Retry-After`
/// ([`retry_after_hint`], [`crate::client::retry_after_from_headers`]) — the
/// same number on purpose, not a coincidence: it is the longest this process is
/// willing to sit idle inside one turn, whoever proposed the delay. Raising one
/// without the other would mean a hostile (or merely optimistic) `Retry-After`
/// could stall a turn longer than our own worst-case backoff ever would.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How many attempts an operation gets and how long it waits between them.
///
/// Data rather than constants so a test can drive the real retry loops without
/// waiting out real backoffs — the schedule below is minutes long by design,
/// and a test that has to sleep through it is a test nobody runs. Production
/// always uses [`RetryPolicy::default`].
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, including the first one. `1` disables retrying.
    pub max_attempts: usize,
    /// The wait after the first failure.
    pub first_backoff: Duration,
    /// Ceiling on any wait, computed or server-requested.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            first_backoff: FIRST_BACKOFF,
            max_backoff: MAX_BACKOFF,
        }
    }
}

impl RetryPolicy {
    /// The wait before retry number `retry` (1-based), before jitter:
    /// `first_backoff` doubled `retry - 1` times, capped at `max_backoff`.
    pub fn backoff(&self, retry: usize) -> Duration {
        let doublings = u32::try_from(retry.saturating_sub(1)).unwrap_or(u32::MAX);
        self.first_backoff
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(self.max_backoff)
    }

    /// [`Self::backoff`] with jitter applied — what the sleep actually is.
    fn jittered_backoff(&self, retry: usize) -> Duration {
        // Every call increments the atomic counter, so concurrent agents receive
        // adjacent jitter slots. The counter cycles evenly through all 1,000.
        let seq = JITTER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Duration::from_secs_f64(self.backoff(retry).as_secs_f64() * retry_jitter(seq))
    }
}

/// Process-wide counter mixed into jitter so concurrent agents (sub-agents
/// especially) don't get identical jitter from same subsec-nanos and retry
/// in lockstep.
static JITTER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Map a sequence number to one of 1,000 evenly spaced jitter slots (±25%).
pub(crate) fn retry_jitter(seq: u64) -> f64 {
    0.75 + f64::from((seq % 1_000) as u32) / 2_000.0
}

/// One retry, as the caller is told about it: everything a message like
/// `network error — retrying in 5s (attempt 2/10)` needs, and nothing about how
/// the caller phrases it. hrdr-agent renders this as an `AgentEvent::Notice`,
/// which is an hrdr-agent type and must not leak down here.
#[derive(Debug, Clone, Copy)]
pub struct RetryAttempt {
    /// How long the driver is about to sleep — jitter and any server
    /// `Retry-After` already applied, so what is reported is what happens.
    pub delay: Duration,
    /// 1-based number of the attempt this retry is about to make: `2` for the
    /// first retry, `max_attempts` for the last one allowed.
    pub attempt: usize,
    /// The whole budget, so a message can say `2/10` rather than just `2`.
    pub max_attempts: usize,
}

/// The retry allowance of **one logical operation**, threaded by the caller
/// through every step that operation takes.
///
/// One assistant round is one operation: connecting the stream and draining it
/// share a single budget, because they are two ways for the same request to
/// fail and the model does not care which one hit. Passing this value into both
/// is what makes the policy's attempt count the truth for a whole round, rather
/// than a separate allowance per loop, multiplied wherever the loops nest.
#[derive(Debug)]
pub struct RetryBudget {
    policy: RetryPolicy,
    /// Retries already spent. Attempts made is always this plus one.
    spent: usize,
}

impl RetryBudget {
    /// A fresh budget. Call this once per logical operation — never per loop.
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy, spent: 0 }
    }

    /// The whole retry cycle for one failure: decide whether `err` is worth
    /// retrying, work out the delay (the server's `Retry-After` if it sent one,
    /// else the computed backoff), report the attempt, sleep, and count it.
    ///
    /// Returns `true` when the caller should try again — by which time the wait
    /// has already happened — and `false` when it must give up and surface
    /// `err`, either because the error is not transient or because the budget is
    /// spent.
    ///
    /// `report` is called *before* the sleep, so a frontend shows "retrying in
    /// 60s" at the start of the wait rather than at the end of it.
    ///
    /// `report` is generic rather than `&mut dyn FnMut` so the returned future
    /// inherits the caller's `Send`-ness: the agent's turn future is spawned
    /// onto tokio, and a trait object here would have forced a `Send` bound
    /// onto every event sink in the turn loop to get there.
    pub async fn retry<R: FnMut(RetryAttempt)>(
        &mut self,
        err: &anyhow::Error,
        report: &mut R,
    ) -> bool {
        if !is_transient(err) {
            return false;
        }
        // `spent + 1` retries means `spent + 2` attempts made; refuse the one
        // that would exceed the budget.
        if self.spent + 2 > self.policy.max_attempts {
            return false;
        }
        self.spent += 1;
        // A server that told us when to come back knows more than our schedule
        // does — it wins, clamped to `MAX_BACKOFF` upstream where it is parsed.
        let delay =
            retry_after_hint(err).unwrap_or_else(|| self.policy.jittered_backoff(self.spent));
        report(RetryAttempt {
            delay,
            attempt: self.spent + 1,
            max_attempts: self.policy.max_attempts,
        });
        tokio::time::sleep(delay).await;
        true
    }
}

/// Case-insensitive substring scan of an error's display string against a set
/// of marker phrases — the shared shape of the classifiers below.
fn err_mentions(e: &anyhow::Error, needles: &[&str]) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    needles.iter().any(|n| msg.contains(n))
}

/// Phrases that mean a USAGE/quota limit rather than a rate limit: retrying
/// cannot help until the billing window resets. Deliberately disjoint from
/// rate-limit wording — a false positive here stops a retry that would have
/// succeeded. A bare "quota" is a rate-limit word, not usage wording: cloud
/// providers use it overwhelmingly for per-minute/per-day REQUEST quotas,
/// which a retry clears. Only the explicit usage forms count:
/// `insufficient_quota`, `billing`, `credit balance`, `spend limit`.
const USAGE_LIMIT_PHRASES: &[&str] = &[
    "insufficient_quota",
    "billing",
    "credit balance",
    "spend limit",
];

/// Whether `text` describes a usage/quota limit rather than a rate limit.
pub(crate) fn is_usage_limit_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    USAGE_LIMIT_PHRASES.iter().any(|n| lower.contains(n))
}

/// Whether an error is a usage/quota limit — the [`ChatErrorKind::UsageLimit`]
/// class, or the phrase scan for errors that never went through the typed
/// classifier. The third classifier alongside [`is_transient`] and
/// [`is_context_overflow`]; codex's `UsageLimitReached`.
pub(crate) fn is_usage_limit(e: &anyhow::Error) -> bool {
    if let Some(ce) = e.downcast_ref::<crate::ChatError>() {
        return ce.kind == crate::ChatErrorKind::UsageLimit;
    }
    is_usage_limit_text(&e.to_string())
}

/// Whether an error looks like a transient network/server failure worth
/// retrying (connection issues `request failed`/`timed out`/…, 429, or 5xx).
///
/// Checks the typed [`ChatError`](crate::ChatError) first. A typed error's
/// `message` carries the server's own response body (or, for a mid-stream error
/// object, the server's own error text) — arbitrary data that happens to
/// contain a word like "connection" or "reset" as part of an unrelated,
/// permanent 400 isn't evidence of a transient failure, so the broad substring
/// scan below is **not** applied to it; `kind` alone decides. Only errors that
/// never went through the typed path at all — raw transport/network failures (a
/// reqwest send failure, a dropped connection mid-read) or a legacy plain-text
/// error — fall back to the substring scan, where those same marker words
/// genuinely describe the transport-level failure itself.
pub fn is_transient(e: &anyhow::Error) -> bool {
    if let Some(ce) = e.downcast_ref::<crate::ChatError>() {
        return ce.kind == crate::ChatErrorKind::Transient;
    }
    // A quota/usage-limit 429 is permanent until the window resets — never
    // a rate limit to ride out. Checked before the transient scan so an
    // untyped "returned 429: quota exceeded" doesn't match `returned 429`.
    // (`is_usage_limit` falls back to the same phrase scan for untyped errors.)
    if is_usage_limit(e) {
        return false;
    }
    err_mentions(
        e,
        &[
            "request failed", // reqwest send() failure (network)
            "timed out",
            "connection",
            "reset",
            "broken pipe",
            "returned 429", // rate limited
            "returned 500",
            "returned 502",
            "returned 503",
            "returned 504",
            "returned 529",      // Anthropic "Overloaded"
            "overloaded",        // Anthropic mid-stream overloaded_error
            "incomplete stream", // stream truncated without terminal marker
        ],
    )
}

/// Whether an error is the server rejecting the request for exceeding the
/// model's context window. The marker phrases are ported from pi's
/// provider-specific overflow patterns (`packages/ai/src/utils/overflow.ts`),
/// covering ~20 OpenAI-compatible backends.
///
/// Checks the typed [`ChatError`](crate::ChatError) first; falls back to a
/// case-insensitive substring scan of the display string for errors that
/// predate the typed form.
pub fn is_context_overflow(e: &anyhow::Error) -> bool {
    if let Some(ce) = e.downcast_ref::<crate::ChatError>() {
        match ce.kind {
            crate::ChatErrorKind::Overflow => return true,
            crate::ChatErrorKind::Transient => return false,
            // A quota limit is not a context overflow: the request itself was
            // fine, so compaction cannot help it.
            crate::ChatErrorKind::UsageLimit => return false,
            // `Other` falls through to the body-text scan: many providers
            // signal context overflow with a 400 + descriptive body, which
            // `classify_status` can't distinguish from an ordinary bad request.
            crate::ChatErrorKind::Other => {}
        }
    }
    // Rate-limit / throttling errors sometimes contain overflow-ish wording
    // (e.g. Bedrock's "Throttling: too many tokens") — exclude them first so
    // they retry (via [`is_transient`]) rather than triggering a compaction.
    if err_mentions(
        e,
        &["rate limit", "too many requests", "throttl", "returned 429"],
    ) {
        return false;
    }
    err_mentions(
        e,
        &[
            // Generic phrasings (cover most backends + our own error text).
            "context length",
            "context_length",
            "maximum context",
            "context window",
            "context size",
            "too many tokens",
            "token limit exceeded",
            "reduce the length",
            // Provider-specific (from pi's overflow.ts).
            "prompt is too long",                     // Anthropic
            "request_too_large",                      // Anthropic 413
            "request too large",                      // Anthropic 413 (spaced)
            "returned 413",                           // our formatting of a 413
            "input is too long",                      // Bedrock
            "exceeds the context window",             // OpenAI
            "input token count",                      // Google Gemini
            "maximum prompt length is",               // xAI Grok
            "maximum allowed input length",           // OpenRouter/Poolside
            "longer than the model's context length", // Together AI
            "exceeds the limit of",                   // GitHub Copilot
            "exceeded model token limit",             // Kimi
            "too large for model with",               // Mistral
            "model_context_window_exceeded",          // z.ai
            "configured context size",                // DS4
        ],
    )
}

/// An optional request parameter a provider can reject outright, identified by
/// the name it carries on the wire.
///
/// Only parameters hrdr sends *when configured* are listed — the ones a strict
/// endpoint can 400 on without the request itself being wrong, and so the ones
/// that can be dropped and the request retried. Required fields (`model`, the
/// messages, `stream`) are deliberately absent: dropping one of those is not a
/// negotiation, it is a broken request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsupportedParam {
    /// The output cap: one [`RequestParams`](crate::RequestParams) field and
    /// *three* wire names — `max_tokens` on chat completions,
    /// `max_completion_tokens` on the OpenAI reasoning models that renamed it
    /// (see `uses_max_completion_tokens`), and `max_output_tokens` on the
    /// Responses shape. All three have to be recognized: the endpoints most
    /// likely to reject a cap are exactly the ones not using the oldest spelling.
    MaxTokens,
    Temperature,
    TopP,
    /// `prompt_cache_key`, the OpenAI-shaped cache routing hint.
    PromptCacheKey,
    /// `reasoning_effort` on the chat-completions shape, `reasoning.effort` on
    /// the Responses shape.
    ReasoningEffort,
}

impl UnsupportedParam {
    /// Every wire name this parameter is known by, **canonical first** — the
    /// name [`Self::as_str`] shows, which should be the one the user recognizes
    /// from their own config rather than whichever spelling this endpoint
    /// happens to use.
    ///
    /// Order does not affect matching. What stops `max_tokens` being found
    /// inside `max_output_tokens` is [`mentions_identifier`]'s boundary check on
    /// both ends, not the sequence here (none of these is a substring of another
    /// anyway).
    fn wire_names(self) -> &'static [&'static str] {
        match self {
            Self::MaxTokens => &["max_tokens", "max_completion_tokens", "max_output_tokens"],
            Self::Temperature => &["temperature"],
            Self::TopP => &["top_p"],
            Self::PromptCacheKey => &["prompt_cache_key"],
            // Never the bare `reasoning`: it is a common English word in these
            // bodies ("… for reasoning models"), and matching it would drop the
            // effort level over a message about some other parameter entirely.
            Self::ReasoningEffort => &["reasoning_effort", "reasoning.effort"],
        }
    }

    /// The name to show a user: the canonical spelling, which for every variant
    /// here is also the configuration key they would edit. Deliberately not the
    /// spelling the server used — telling someone their `max_completion_tokens`
    /// was refused, when what they set is `max_tokens`, sends them looking for a
    /// setting that does not exist.
    pub fn as_str(self) -> &'static str {
        self.wire_names()[0]
    }

    /// Every variant. The order only decides which one wins if a single body
    /// somehow names two, so it is fixed here rather than left to chance.
    const ALL: [Self; 5] = [
        Self::MaxTokens,
        Self::Temperature,
        Self::TopP,
        Self::PromptCacheKey,
        Self::ReasoningEffort,
    ];
}

/// Phrases with which a provider says "I do not know this parameter".
///
/// Deliberately short. A false positive here silently stops sending something
/// the user configured, so this matches only wordings observed in the wild —
/// OpenAI's two (`Unsupported parameter: …`, `Unrecognized request argument
/// supplied: …`) and the generic `unknown parameter`. Everything else is left
/// to fail loudly, which is the safe direction.
const REJECTION_PHRASES: [&str; 4] = [
    "unsupported parameter",
    "unsupported_parameter",
    "unrecognized request argument",
    "unknown parameter",
];

/// Whether `haystack` contains `needle` as a whole identifier — not as a piece
/// of a longer one. `max_output_tokens_v2` must not read as `max_output_tokens`,
/// or a model with a *differently* named cap would have hrdr drop the cap it was
/// actually sending and retry into the same rejection.
fn mentions_identifier(haystack: &str, needle: &str) -> bool {
    let boundary = |c: u8| !c.is_ascii_alphanumeric() && c != b'_';
    haystack.match_indices(needle).any(|(start, _)| {
        let before_ok = start == 0 || boundary(haystack.as_bytes()[start - 1]);
        let after_ok = haystack
            .as_bytes()
            .get(start + needle.len())
            .is_none_or(|&c| boundary(c));
        before_ok && after_ok
    })
}

/// Which optional parameter, if any, the provider rejected as unsupported —
/// the one error class a caller can recover from by sending *less*.
///
/// Requires the typed [`ChatError`](crate::ChatError) with status 400: a rejected
/// parameter is a request-shape complaint, and nothing else (a 422, a 5xx, an
/// untyped transport failure) is evidence of one. The body must both carry a
/// rejection phrase and name a parameter hrdr actually sends optionally; either
/// half alone is not enough, because the whole point is to drop exactly the
/// field the server named.
pub fn unsupported_param(e: &anyhow::Error) -> Option<UnsupportedParam> {
    let ce = e.downcast_ref::<crate::ChatError>()?;
    if ce.status != Some(400) {
        return None;
    }
    let message = ce.message.to_ascii_lowercase();
    if !REJECTION_PHRASES.iter().any(|p| message.contains(p)) {
        return None;
    }
    UnsupportedParam::ALL.into_iter().find(|param| {
        param
            .wire_names()
            .iter()
            .any(|name| mentions_identifier(&message, name))
    })
}

/// The server-requested wait from a `Retry-After` header, if the client embedded
/// one in the error as `retry-after: <seconds>s` (see the client's rate-limit
/// error formatting). Clamped to [`MAX_BACKOFF`] so a hostile/oversized value
/// can't stall the turn. Only the integer-seconds form is parsed (the HTTP-date
/// form is ignored).
///
/// Checks the typed [`ChatError`](crate::ChatError) first; falls back to a text
/// scan of the display string for errors that predate the typed form.
pub fn retry_after_hint(e: &anyhow::Error) -> Option<Duration> {
    if let Some(ce) = e.downcast_ref::<crate::ChatError>() {
        return ce.retry_after;
    }
    let msg = e.to_string().to_ascii_lowercase();
    let after = msg.split("retry-after:").nth(1)?;
    let secs: u64 = after
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    (secs > 0).then(|| Duration::from_secs(secs.min(MAX_BACKOFF.as_secs())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatError, ChatErrorKind};

    /// The default budget exists to outlast a provider's rate-limit window, and
    /// how long it actually lasts is derived from [`MAX_ATTEMPTS`],
    /// [`FIRST_BACKOFF`] and [`MAX_BACKOFF`]. That total used to be spelled out
    /// in a comment, where nothing updated it when one of the three moved. It
    /// lives here instead: change any of them enough to matter and this goes
    /// red, which a comment can never do.
    #[test]
    fn default_budget_outlasts_a_rate_limit_window() {
        let p = RetryPolicy::default();
        // `max_attempts` attempts means one fewer wait between them.
        let total: Duration = (1..p.max_attempts).map(|retry| p.backoff(retry)).sum();
        assert!(
            total >= Duration::from_secs(5 * 60),
            "the default retry budget is {total:?} — too short to sit out a \
             typical rate-limit window"
        );
        assert!(
            total <= Duration::from_secs(10 * 60),
            "the default retry budget is {total:?} — long enough that a turn \
             looks hung rather than patient"
        );
    }

    fn chat_err(kind: ChatErrorKind, retry_after: Option<Duration>) -> anyhow::Error {
        anyhow::Error::new(ChatError {
            status: None,
            retry_after,
            kind,
            message: "server said no".to_string(),
        })
    }

    /// A policy whose waits are zero, so a test can spend a whole budget in
    /// microseconds. Only the *schedule* tests below use the real one.
    fn instant(max_attempts: usize) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            first_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    /// Collect every [`RetryAttempt`] a budget reports while it is spent down
    /// against an endlessly-failing transient error.
    async fn spend(budget: &mut RetryBudget, err: &anyhow::Error) -> Vec<RetryAttempt> {
        let mut seen = Vec::new();
        while budget.retry(err, &mut |a| seen.push(a)).await {}
        seen
    }

    /// The shipped schedule, spelled out: nine waits, doubling from 5s, capped
    /// at 60s. Written as literals rather than recomputed from the constants —
    /// a test that reimplements the formula agrees with any formula.
    #[test]
    fn the_backoff_schedule_is_5_10_20_40_then_60() {
        let policy = RetryPolicy::default();
        let secs: Vec<u64> = (1..=9).map(|n| policy.backoff(n).as_secs()).collect();
        assert_eq!(secs, vec![5, 10, 20, 40, 60, 60, 60, 60, 60]);
        // Nine waits is exactly one fewer than the attempt budget.
        assert_eq!(policy.max_attempts, 10);
        // 6¼ minutes of riding out an outage, as the doc comment claims.
        assert_eq!(secs.iter().sum::<u64>(), 375);
    }

    /// Jitter stays inside ±25% of the schedule and never escapes the cap by
    /// enough to matter — the reason it exists is desynchronising sub-agents,
    /// not changing the schedule.
    #[test]
    fn jitter_stays_within_25_percent_of_the_schedule() {
        let policy = RetryPolicy::default();
        for retry in 1..=9 {
            let base = policy.backoff(retry).as_secs_f64();
            for _ in 0..200 {
                let d = policy.jittered_backoff(retry).as_secs_f64();
                assert!(
                    d >= base * 0.75 - f64::EPSILON && d <= base * 1.25 + f64::EPSILON,
                    "retry {retry}: {d}s is outside ±25% of {base}s"
                );
            }
        }
    }

    /// Every jitter slot is reachable and they span the full ±25% band — a
    /// generator stuck on one slot would retry every sub-agent in lockstep,
    /// which is the failure this exists to prevent.
    #[test]
    fn retry_jitter_uses_every_slot() {
        let mut seen: Vec<f64> = (0..1000).map(retry_jitter).collect();
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap());
        seen.dedup();
        assert_eq!(seen.len(), 1000);
        assert!((seen[0] - 0.75).abs() < 1e-9);
        assert!((seen[999] - 1.2495).abs() < 1e-9);
    }

    /// Ten attempts, then it gives up: nine retries reported, numbered 2..=10.
    #[tokio::test]
    async fn a_budget_allows_ten_attempts_and_then_gives_up() {
        let mut budget = RetryBudget::new(instant(10));
        let reported = spend(&mut budget, &chat_err(ChatErrorKind::Transient, None)).await;

        assert_eq!(reported.len(), 9, "ten attempts means nine retries");
        let numbers: Vec<usize> = reported.iter().map(|a| a.attempt).collect();
        assert_eq!(numbers, (2..=10).collect::<Vec<_>>());
        assert!(reported.iter().all(|a| a.max_attempts == 10));
        // Spent is spent: asking again after the budget is gone stays `false`.
        assert!(
            !budget
                .retry(&chat_err(ChatErrorKind::Transient, None), &mut |_| {})
                .await
        );
    }

    /// A non-transient error is not retried at all — no wait, no report, and
    /// nothing taken out of the budget.
    #[tokio::test]
    async fn a_non_transient_error_is_not_retried() {
        for kind in [
            ChatErrorKind::Other,
            ChatErrorKind::Overflow,
            ChatErrorKind::UsageLimit,
        ] {
            let mut budget = RetryBudget::new(instant(10));
            let mut reports = 0usize;
            assert!(
                !budget
                    .retry(&chat_err(kind, None), &mut |_| reports += 1)
                    .await
            );
            assert_eq!(reports, 0);
            // The budget is untouched: a transient failure afterwards still gets
            // all nine retries.
            let left = spend(&mut budget, &chat_err(ChatErrorKind::Transient, None)).await;
            assert_eq!(left.len(), 9, "{kind:?} must not consume the budget");
        }
    }

    /// A usage/quota limit is its own class: permanent until the billing window
    /// resets, not a rate limit to ride out — so it must never be transient, and
    /// it is not a context overflow either (the request itself was fine).
    #[test]
    fn usage_limit_is_distinct_from_rate_limit() {
        for msg in [
            "you exceeded your current quota, please check your plan and billing details",
            "insufficient_quota",
            "Your credit balance is too low",
            "you have reached your spend limit",
        ] {
            assert!(
                is_usage_limit_text(msg),
                "expected a usage limit for: {msg}"
            );
        }
        for msg in [
            "rate limit exceeded, retry after 20s",
            "too many requests",
            "throttled: slow down",
            "chat endpoint returned 429: slow down",
            // Bare "quota" is a rate-limit word (per-minute/per-day REQUEST
            // quota), not usage wording — none of these carry a billing /
            // credit / spend / insufficient_quota marker.
            "quota exceeded for metric requests per minute: limit 60.0",
            "too many requests for the per-day quota",
        ] {
            assert!(
                !is_usage_limit_text(msg),
                "rate-limit wording must not read as a usage limit: {msg}"
            );
        }

        // Typed: the kind decides, whatever the status says.
        let quota = anyhow::Error::new(ChatError {
            status: Some(429),
            kind: ChatErrorKind::UsageLimit,
            retry_after: None,
            message: "quota exceeded".to_string(),
        });
        assert!(!is_transient(&quota));
        assert!(!is_context_overflow(&quota));
        assert!(is_usage_limit(&quota));

        let rate = anyhow::Error::new(ChatError {
            status: Some(429),
            kind: ChatErrorKind::Transient,
            retry_after: None,
            message: "rate limited".to_string(),
        });
        assert!(is_transient(&rate));

        // Untyped: bare "quota" wording is a rate limit, so the `returned 429`
        // needle decides and the error stays retryable.
        let quota_untyped = anyhow::anyhow!("chat endpoint returned 429: quota exceeded");
        assert!(is_transient(&quota_untyped));
        assert!(!is_usage_limit(&quota_untyped));

        let rate_untyped = anyhow::anyhow!("chat endpoint returned 429: rate limited");
        assert!(is_transient(&rate_untyped));
    }

    /// A server that says when to come back beats the computed backoff — the
    /// whole point of reading `Retry-After`.
    #[tokio::test]
    async fn a_server_hint_beats_the_computed_backoff() {
        let mut budget = RetryBudget::new(RetryPolicy::default());
        let hinted = chat_err(ChatErrorKind::Transient, Some(Duration::from_secs(1)));
        let mut seen = None;
        // Real policy, but the hint is what is slept — 1s, not the 5s schedule.
        assert!(budget.retry(&hinted, &mut |a| seen = Some(a)).await);
        assert_eq!(
            seen.expect("a retry was reported").delay,
            Duration::from_secs(1)
        );
    }

    /// A hostile `Retry-After` is clamped to the same ceiling as the backoff, so
    /// no server can park a turn for an hour.
    #[test]
    fn a_hostile_retry_after_is_clamped() {
        let hostile =
            anyhow::anyhow!("chat endpoint returned 429: slow down (retry-after: 86400s)");
        assert_eq!(retry_after_hint(&hostile), Some(MAX_BACKOFF));
        assert_eq!(MAX_BACKOFF, RetryPolicy::default().max_backoff);
    }

    // ── Classification ───────────────────────────────────────────────────
    //
    // Moved wholesale from hrdr-agent when the classifiers did: same asserts,
    // same phrases, now next to the code they pin.

    #[test]
    fn classifies_transient_and_overflow_errors() {
        let overflow = anyhow::anyhow!(
            "chat endpoint returned 400 Bad Request: This model's maximum context length is 8192 tokens"
        );
        assert!(is_context_overflow(&overflow));
        assert!(!is_transient(&overflow));

        let rate = anyhow::anyhow!("chat endpoint returned 429 Too Many Requests: slow down");
        assert!(is_transient(&rate));
        assert!(!is_context_overflow(&rate));

        let net = anyhow::anyhow!("chat stream request failed: connection refused");
        assert!(is_transient(&net));

        let plain = anyhow::anyhow!("chat endpoint returned 400 Bad Request: invalid tool schema");
        assert!(!is_transient(&plain));
        assert!(!is_context_overflow(&plain));

        // Incomplete stream errors are transient (the server dropped the connection).
        assert!(is_transient(&anyhow::anyhow!(
            "incomplete stream: something"
        )));
    }

    #[test]
    fn typed_chat_error_classified_correctly() {
        use std::time::Duration;

        // Overflow typed error.
        let overflow = anyhow::Error::new(ChatError {
            status: Some(413),
            kind: ChatErrorKind::Overflow,
            retry_after: None,
            message: "request too large".to_string(),
        });
        assert!(is_context_overflow(&overflow));
        assert!(!is_transient(&overflow));
        assert_eq!(retry_after_hint(&overflow), None);

        // Transient typed error with Retry-After.
        let delay = Duration::from_secs(30);
        let rate = anyhow::Error::new(ChatError {
            status: Some(429),
            kind: ChatErrorKind::Transient,
            retry_after: Some(delay),
            message: "rate limited".to_string(),
        });
        assert!(is_transient(&rate));
        assert!(!is_context_overflow(&rate));
        assert_eq!(retry_after_hint(&rate), Some(delay));

        // Other typed error: neither transient nor overflow.
        let other = anyhow::Error::new(ChatError {
            status: Some(400),
            kind: ChatErrorKind::Other,
            retry_after: None,
            message: "bad request".to_string(),
        });
        assert!(!is_transient(&other));
        assert!(!is_context_overflow(&other));

        // A 400 whose body describes a context overflow classifies as Other by
        // status, but must still fall through to the body-text scan and be
        // treated as overflow (many OpenAI-compatible providers do this instead
        // of 413) — otherwise auto-compaction silently stops firing for them.
        let overflow_400 = anyhow::Error::new(ChatError {
            status: Some(400),
            kind: ChatErrorKind::Other,
            retry_after: None,
            message: "chat endpoint returned 400: maximum context length exceeded".to_string(),
        });
        assert!(is_context_overflow(&overflow_400));
        assert!(!is_transient(&overflow_400));
    }

    #[test]
    fn typed_other_error_is_not_retried_on_incidental_substring_match() {
        // Regression: a permanent, server-provided error body that merely
        // *contains* a transport-sounding word ("connection", "reset") must not
        // be retried as if it were a real network failure. Only the typed
        // `kind` decides for a `ChatError`; the broad substring scan is reserved
        // for errors that never went through the typed classifier (raw
        // transport/network failures).
        let bad_request = anyhow::Error::new(ChatError {
            status: Some(400),
            kind: ChatErrorKind::Other,
            retry_after: None,
            message: "chat endpoint returned 400: invalid 'reset_token' — connection profile \
                      is malformed"
                .to_string(),
        });
        assert!(
            !is_transient(&bad_request),
            "a typed Other error must not be retried just because its body mentions \
             'reset'/'connection'"
        );

        // A raw (non-typed) transport failure with the same words must still be
        // treated as transient — the scan isn't disabled entirely, just scoped
        // away from typed server-error bodies.
        let raw_transport = anyhow::anyhow!("chat stream request failed: connection reset by peer");
        assert!(is_transient(&raw_transport));
    }

    #[test]
    fn retry_after_hint_parses_and_clamps() {
        // Parsed from the client's error suffix.
        let e = anyhow::anyhow!("chat endpoint returned 429 : rate limited (retry-after: 5s)");
        assert_eq!(retry_after_hint(&e).map(|d| d.as_secs()), Some(5));
        // Clamped to 60s.
        let big = anyhow::anyhow!("returned 429 (retry-after: 9999s)");
        assert_eq!(retry_after_hint(&big).map(|d| d.as_secs()), Some(60));
        // Absent → None (falls back to exponential backoff).
        assert_eq!(retry_after_hint(&anyhow::anyhow!("returned 500")), None);
    }

    #[test]
    fn is_transient_more_variants() {
        for msg in [
            "chat stream request failed: connection timed out",
            "broken pipe",
            "chat endpoint returned 502 Bad Gateway: upstream down",
            "chat endpoint returned 503 Service Unavailable",
            "chat endpoint returned 504 Gateway Timeout",
            "connection reset by peer",
            "chat endpoint returned 529 : {\"type\":\"overloaded_error\"}", // Anthropic
            "anthropic stream error: Overloaded",
        ] {
            assert!(
                is_transient(&anyhow::anyhow!("{msg}")),
                "expected transient for: {msg}"
            );
        }
    }

    /// Every status [`crate::client::classify_status`] calls transient, paired
    /// with whether the substring scan in [`is_transient`] would **also** catch
    /// it once the typed error is out of the picture.
    ///
    /// The two covers do not line up. The scan's needles include
    /// `returned 429`/`500`/`502`/`503`/`504`/`529` and `overloaded`, but
    /// nothing for 408, 522 or 524 — so for those three the `classify_status`
    /// arm is the *only* thing making them retryable. They are exactly what a
    /// Cloudflare-fronted gateway emits when its origin stalls, so losing one of
    /// those arms turns a retryable stall into a terminal error that kills the
    /// turn, with no other code path to notice.
    const TRANSIENT_STATUS_HAS_TEXT_FALLBACK: &[(u16, bool)] = &[
        (408, false),
        (429, true),
        (500, true),
        (502, true),
        (503, true),
        (504, true),
        (522, false),
        (524, false),
        (529, true),
    ];

    /// The message shape `error_from_response` builds, as an *untyped*
    /// `anyhow::Error` — what [`is_transient`]'s fallback alone would see. The
    /// body deliberately carries none of the scan's marker words, so only the
    /// `returned <code>` needles can match.
    fn untyped_status_error(status: u16) -> anyhow::Error {
        let code = reqwest::StatusCode::from_u16(status).expect("a valid status code");
        anyhow::anyhow!("chat endpoint returned {code}: upstream said no")
    }

    #[test]
    fn three_transient_statuses_have_no_text_fallback_safety_net() {
        use crate::client::classify_status;

        for &(status, has_net) in TRANSIENT_STATUS_HAS_TEXT_FALLBACK {
            assert_eq!(
                classify_status(status),
                ChatErrorKind::Transient,
                "{status} must stay in the transient table"
            );
            let untyped = untyped_status_error(status);
            assert_eq!(
                is_transient(&untyped),
                has_net,
                "text-fallback coverage for {status} changed: {untyped}"
            );
        }

        // Derived from the scan's actual behaviour, not re-read off the table
        // above, and compared with the three spelled out by name. Adding a
        // `returned 408`-style needle should fail here so the claim gets
        // updated; deleting a `classify_status` arm fails the loop above.
        let unprotected: Vec<u16> = TRANSIENT_STATUS_HAS_TEXT_FALLBACK
            .iter()
            .filter(|&&(status, _)| !is_transient(&untyped_status_error(status)))
            .map(|&(status, _)| status)
            .collect();
        assert_eq!(
            unprotected,
            vec![408, 522, 524],
            "these three are retryable only because `classify_status` says so"
        );
    }

    #[test]
    fn is_context_overflow_more_variants() {
        for msg in [
            "context window exceeded",
            "too many tokens in the prompt",
            "please reduce the length of the messages",
            "context size limit reached",
            "context_length exceeded",
            // Provider-specific patterns ported from pi.
            "prompt is too long: 213462 tokens > 200000 maximum", // Anthropic
            "request_too_large",                                  // Anthropic 413
            "your input exceeds the context window of this model", // OpenAI
            "the input token count (1196265) exceeds the maximum", // Gemini
            "this model's maximum prompt length is 131072",       // xAI
            "exceeds the maximum allowed input length of 8000 tokens", // OpenRouter
            "is longer than the model's context length (4096 tokens)", // Together
            "prompt token count of 5 exceeds the limit of 4",     // Copilot
            "your request exceeded model token limit",            // Kimi
            "too large for model with 8192 maximum context length", // Mistral
            "model_context_window_exceeded",                      // z.ai
        ] {
            assert!(
                is_context_overflow(&anyhow::anyhow!("{msg}")),
                "expected context overflow for: {msg}"
            );
        }
        // Rate-limit / throttling is NOT overflow, even when it mentions tokens.
        for msg in [
            "chat endpoint returned 429 Too Many Requests: slow down",
            "ThrottlingException: too many tokens, please wait",
            "rate limit exceeded, retry after 20s",
        ] {
            assert!(
                !is_context_overflow(&anyhow::anyhow!("{msg}")),
                "throttling must not be treated as overflow: {msg}"
            );
        }
    }

    fn rejection(status: Option<u16>, message: &str) -> anyhow::Error {
        anyhow::Error::new(crate::ChatError {
            status,
            retry_after: None,
            kind: crate::ChatErrorKind::Other,
            message: message.to_string(),
        })
    }

    /// Both wire spellings of every parameter are recognized, and the variant
    /// returned is the one the caller has to clear — not merely "something was
    /// rejected".
    #[test]
    fn unsupported_param_names_the_parameter_to_drop() {
        for (message, expected) in [
            (
                r#"HTTP 400: {"detail":"Unsupported parameter: max_output_tokens"}"#,
                UnsupportedParam::MaxTokens,
            ),
            (
                r#"{"error":{"message":"UNSUPPORTED PARAMETER: MAX_TOKENS."}}"#,
                UnsupportedParam::MaxTokens,
            ),
            // The spelling OpenAI's reasoning models use on chat completions.
            (
                "Unsupported parameter: max_completion_tokens",
                UnsupportedParam::MaxTokens,
            ),
            (
                "Unrecognized request argument supplied: temperature",
                UnsupportedParam::Temperature,
            ),
            (
                "Unsupported parameter: 'top_p' is not supported with this model.",
                UnsupportedParam::TopP,
            ),
            (
                "unknown parameter: prompt_cache_key",
                UnsupportedParam::PromptCacheKey,
            ),
            (
                "Unsupported parameter: reasoning.effort",
                UnsupportedParam::ReasoningEffort,
            ),
            (
                "Unsupported parameter: reasoning_effort",
                UnsupportedParam::ReasoningEffort,
            ),
        ] {
            assert_eq!(
                unsupported_param(&rejection(Some(400), message)),
                Some(expected),
                "{message}"
            );
        }
    }

    /// Every variant is reachable through every name it is known by. Driven off
    /// `ALL` and `wire_names` rather than a hand-written list, so a sixth variant
    /// — or a sixth spelling of an existing one — fails here until it is
    /// recognized. A `match` arm returning an empty slice would otherwise be
    /// indistinguishable from one that worked.
    #[test]
    fn every_variant_round_trips_through_every_wire_name_it_answers_to() {
        for param in UnsupportedParam::ALL {
            assert!(
                !param.wire_names().is_empty(),
                "{param:?} answers to no wire name at all"
            );
            for name in param.wire_names() {
                let body = format!(r#"{{"error":{{"message":"Unsupported parameter: {name}"}}}}"#);
                assert_eq!(
                    unsupported_param(&rejection(Some(400), &body)),
                    Some(param),
                    "`{name}` must resolve to {param:?}"
                );
            }
            // `as_str` is what the user is shown, so it has to be a name the
            // matcher itself accepts, not a prettified label.
            assert!(
                param.wire_names().contains(&param.as_str()),
                "{param:?} displays a name it does not answer to"
            );
        }
    }

    /// The negative half, and the one that matters: a false positive here stops
    /// hrdr sending something the user configured, silently and for the rest of
    /// the session.
    #[test]
    fn unsupported_param_refuses_everything_it_is_not_sure_about() {
        for (status, message) in [
            // A cap by another name is not our cap — dropping ours would retry
            // into the identical rejection.
            (Some(400), "Unsupported parameter: max_output_tokens_v2"),
            (Some(400), "Unsupported parameter: xmax_tokens"),
            // The bare word `reasoning` is prose in these bodies, not a field.
            (Some(400), "Unsupported parameter: foo for reasoning models"),
            // A rejection phrase naming nothing we send optionally.
            (Some(400), "Unsupported parameter: logit_bias"),
            // A parameter name with no rejection phrase around it.
            (Some(400), "temperature must be between 0 and 2"),
            // Right words, wrong status: a rejected parameter is a 400.
            (Some(422), "Unsupported parameter: max_output_tokens"),
            (Some(500), "Unsupported parameter: temperature"),
        ] {
            assert_eq!(
                unsupported_param(&rejection(status, message)),
                None,
                "status={status:?} message={message}"
            );
        }
        // Untyped errors carry no status, so they are never a parameter
        // rejection however they read.
        assert_eq!(
            unsupported_param(&anyhow::anyhow!("Unsupported parameter: temperature")),
            None
        );
    }
}
