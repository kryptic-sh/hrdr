//! Token/cost counters — *per agent*, not per session.
//!
//! Every agent makes its own model calls, so every agent has its own usage: its
//! cumulative tokens, the size of its last prompt (its live context), the window
//! it is working against, and what it has cost. The main agent's copy is the one
//! a single-agent frontend calls "the session's", but that is a coincidence of
//! there being one agent — a delegated sub-agent on a different provider fills a
//! different window at a different price, and the status bar that claims
//! otherwise is lying about whichever agent you are looking at.
//!
//! Kept here (rather than in a frontend's session state) so the figures exist
//! with no UI attached: `AgentRegistry::record` folds each call's usage into the
//! agent's own entry as its events land, and a frontend reads it off the
//! registry.

use serde::{Deserialize, Serialize};

/// What one model call was billed — the figures
/// [`Agent::account_usage`](crate::Agent::account_usage) extracts from a
/// finished stream, prices, and folds into the session total.
///
/// Named rather than a tuple because three call sites read it and one of them
/// used to keep a hand-rolled copy of the same five values under different
/// field names. One type, so a caller cannot silently pair the wrong figure
/// with the wrong label.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallSpend {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Prompt tokens the provider served from its cache, and wrote into it.
    /// `None` means it reported no figure at all — which is not the same as
    /// zero, and must never be rendered as one.
    pub cached_prompt_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
    /// Estimated USD for this call, and for the session so far. `None` when the
    /// catalog does not price the model.
    pub cost_usd: Option<f64>,
    pub session_cost_usd: Option<f64>,
}

/// One agent's token and cost counters.
///
/// `tokens_in`/`tokens_out` accumulate over every model call the agent makes.
/// `last_prompt_tokens`/`last_completion_tokens` are the most recent call's usage
/// — the prompt half is the live context size ("X of Y"). `context_window` is the
/// model's advertised maximum, kept so the "of Y" is right immediately on resume,
/// before the endpoint has been re-probed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentUsage {
    #[serde(default)]
    pub tokens_in: usize,
    #[serde(default)]
    pub tokens_out: usize,
    /// Estimated USD spent by this agent, priced from the models.dev catalog; 0
    /// when nothing was priceable.
    #[serde(default)]
    pub cost_usd: f64,
    /// `true` when [`cost_usd`](Self::cost_usd) is only a floor: some call ran on
    /// an unpriced model and was excluded from it (only under `allow_unpriced`).
    /// A cost display must then be flagged (`≥ $X`), never shown bare.
    #[serde(default)]
    pub cost_partial: bool,
    /// Prompt tokens this agent's calls were served from the provider's cache,
    /// and wrote into it.
    #[serde(default)]
    pub cache_read_tokens: usize,
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// Prompt tokens from the calls whose cache use the provider actually
    /// reported — the denominator [`cache_hit_rate`](Self::cache_hit_rate)
    /// divides by.
    ///
    /// Separate from `tokens_in` on purpose. A provider that reports no cache
    /// figures at all is not a provider whose cache is missing; folding its
    /// prompt tokens into the denominator would drive the rate toward zero and
    /// read as "prefix caching broke", which is the one conclusion the number
    /// exists to support. So a call counts here only if it said something.
    #[serde(default)]
    pub cache_measured_tokens: usize,
    #[serde(default)]
    pub last_prompt_tokens: Option<u32>,
    #[serde(default)]
    pub last_completion_tokens: Option<u32>,
    #[serde(default)]
    pub context_window: Option<u32>,
}

impl AgentUsage {
    /// The latest call's `(prompt, completion)` usage — the shape the frontends
    /// hold it in — or `None` when no call has reported usage yet.
    pub fn last(&self) -> Option<(u32, u32)> {
        Some((self.last_prompt_tokens?, self.last_completion_tokens?))
    }

    /// Record the latest call's usage (`None` clears it, e.g. after `/clear`).
    pub fn set_last(&mut self, last: Option<(u32, u32)>) {
        self.last_prompt_tokens = last.map(|(p, _)| p);
        self.last_completion_tokens = last.map(|(_, c)| c);
    }

    /// Accumulate one model call: add to the running totals and remember it as
    /// the latest.
    pub fn record_call(&mut self, prompt: u32, completion: u32) {
        self.tokens_in += prompt as usize;
        self.tokens_out += completion as usize;
        self.set_last(Some((prompt, completion)));
    }

    /// Accumulate one call's prompt-cache figures.
    ///
    /// `prompt` is that call's whole prompt — inclusive of the cached and
    /// written halves, which is what the backends normalize it to (see
    /// `hrdr-llm`'s Anthropic usage mapping, where `prompt_tokens` stays the
    /// inclusive total while the two cache fields break it down). Both figures
    /// absent means the provider said nothing, and the call is left out of the
    /// measured denominator entirely.
    pub fn record_cache(&mut self, prompt: u32, read: Option<u32>, written: Option<u32>) {
        if read.is_none() && written.is_none() {
            return;
        }
        self.cache_read_tokens += read.unwrap_or(0) as usize;
        self.cache_write_tokens += written.unwrap_or(0) as usize;
        self.cache_measured_tokens += prompt as usize;
    }

    /// Fraction of measured prompt tokens this agent had served from the
    /// prompt cache, in `0.0..=1.0`.
    ///
    /// `None` when no call this session reported any cache figure — an endpoint
    /// that does not publish them, which must not render as a rate of zero.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        (self.cache_measured_tokens > 0)
            .then(|| self.cache_read_tokens as f64 / self.cache_measured_tokens as f64)
    }

    /// Fold one [`crate::AgentEvent::Usage`] into these counters. The single
    /// place an event becomes a number, so an agent's counters read the same
    /// whoever is watching it — or when nobody is.
    pub fn record_event(&mut self, ev: &crate::AgentEvent) {
        if let crate::AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            cache_creation_tokens,
            session_cost_usd,
            cost_partial,
            ..
        } = ev
        {
            self.record_call(*prompt_tokens, *completion_tokens);
            self.record_cache(
                *prompt_tokens,
                *cached_prompt_tokens,
                *cache_creation_tokens,
            );
            if let Some(total) = session_cost_usd {
                self.cost_usd = *total;
            }
            // Latches: a session that ever excluded an unpriced call stays
            // partial even when later events carry a fresh priced total.
            self.cost_partial |= *cost_partial;
        }
    }

    /// The live context size — the last call's prompt tokens.
    pub fn ctx_used(&self) -> usize {
        self.last_prompt_tokens.unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_accumulates_totals_and_becomes_the_latest() {
        let mut u = AgentUsage::default();
        assert_eq!(u.last(), None);
        u.record_call(100, 20);
        u.record_call(300, 5);
        assert_eq!(u.tokens_in, 400);
        assert_eq!(u.tokens_out, 25);
        assert_eq!(u.last(), Some((300, 5)), "the latest call, not the sum");
        assert_eq!(u.ctx_used(), 300, "context in use is the last prompt");
        u.set_last(None);
        assert_eq!(u.ctx_used(), 0, "cleared after a /clear or a compaction");
    }

    /// The session cache rate divides by what was actually MEASURED, not by
    /// every prompt token the agent sent.
    ///
    /// An endpoint that publishes no cache figures is not an endpoint whose
    /// cache stopped working, and those are the two things the rate has to tell
    /// apart — it exists to answer "did prefix caching keep working this
    /// session", and a silent provider dragging it toward zero would answer
    /// "no" for every session that touched one.
    #[test]
    fn the_cache_rate_divides_by_what_was_measured() {
        let mut u = AgentUsage::default();
        // Nothing reported yet: absent, not zero.
        assert_eq!(u.cache_hit_rate(), None);
        u.record_cache(1_000, None, None);
        assert_eq!(
            u.cache_hit_rate(),
            None,
            "a provider that says nothing must not read as a 0% hit rate"
        );
        assert_eq!(u.cache_measured_tokens, 0);

        // A reporting call: 900 of its 1000 prompt tokens came from cache.
        u.record_cache(1_000, Some(900), Some(50));
        assert_eq!(u.cache_read_tokens, 900);
        assert_eq!(u.cache_write_tokens, 50);
        assert_eq!(u.cache_hit_rate(), Some(0.9));

        // A second silent call must not move it — that is the whole point of
        // the separate denominator.
        u.record_cache(9_000, None, None);
        assert_eq!(
            u.cache_hit_rate(),
            Some(0.9),
            "an unreported call must not dilute a measured rate"
        );

        // A first turn writes the cache and reads nothing. That IS a 0% rate,
        // and it is measured, so it counts.
        let mut fresh = AgentUsage::default();
        fresh.record_cache(2_000, None, Some(2_000));
        assert_eq!(fresh.cache_hit_rate(), Some(0.0));
    }

    /// The event is folded into the counters here, so an agent's usage is the
    /// same whether a UI is watching it or not.
    #[test]
    fn a_usage_event_folds_into_the_counters() {
        let mut u = AgentUsage::default();
        u.record_event(&crate::AgentEvent::Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            decode_ms: 0,
            cached_prompt_tokens: Some(6),
            cache_creation_tokens: Some(2),
            reasoning_tokens: None,
            cost_usd: None,
            session_cost_usd: Some(0.5),
            cost_partial: false,
        });
        assert_eq!(u.tokens_in, 10);
        assert_eq!(u.tokens_out, 4);
        assert_eq!(u.cost_usd, 0.5);
        // The cache halves ride the same event and land in the same fold —
        // they used to be carried past these counters and dropped.
        assert_eq!(u.cache_read_tokens, 6);
        assert_eq!(u.cache_write_tokens, 2);
        assert_eq!(u.cache_hit_rate(), Some(0.6));
        assert!(!u.cost_partial, "a fully-priced total is complete");
        // Anything else leaves them alone.
        u.record_event(&crate::AgentEvent::TurnDone);
        assert_eq!(u.tokens_in, 10);
    }

    /// A mixed run — priced usage plus an excluded unpriced call — folds into a
    /// total marked partial, and the mark latches even if a later priced event
    /// carries `cost_partial: false`.
    #[test]
    fn an_excluded_unpriced_call_marks_the_total_partial() {
        let mut u = AgentUsage::default();
        u.record_event(&crate::AgentEvent::Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            decode_ms: 0,
            cached_prompt_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            cost_usd: Some(0.25),
            session_cost_usd: Some(0.25),
            cost_partial: true,
        });
        assert_eq!(u.cost_usd, 0.25, "priced usage still counts");
        assert!(u.cost_partial, "the excluded unpriced call is admitted");
        // A later purely-priced event must not clear the mark.
        u.record_event(&crate::AgentEvent::Usage {
            prompt_tokens: 5,
            completion_tokens: 2,
            decode_ms: 0,
            cached_prompt_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            cost_usd: Some(0.1),
            session_cost_usd: Some(0.35),
            cost_partial: false,
        });
        assert!(u.cost_partial, "partial latches for the whole session");
    }
}
