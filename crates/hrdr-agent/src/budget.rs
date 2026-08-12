//! Session cost tracking and budget enforcement — extracted from [`Agent`] into
//! its own module to keep `lib.rs` manageable.
//!
//! Every method here is `impl super::Agent` — they live on the agent directly
//! because they access agent-private state (the shared cost counter, the
//! price-card memo, the max-cost cap, and the message buffer).

use anyhow::{Result, bail};
use hrdr_llm::Accumulator;

use crate::model_ref::catalog_provider_key;
use crate::{Agent, estimate_tokens, estimate_tokens_in_messages};

impl Agent {
    /// The current `(provider, model)` price card from the models.dev
    /// catalog, memoized per pair — the inner `None` remembers an unpriced
    /// model (a local server) so the catalog isn't re-read every call.
    async fn current_cost_rates(&mut self) -> Option<hrdr_llm::catalog::ModelCost> {
        let key = self.resolved.reference().clone();
        if self.cost_rates.as_ref().map(|(k, _)| k) != Some(&key) {
            // The catalog's namespace, not the app's — see `catalog_provider_key`.
            let rates = hrdr_llm::catalog::model_cost(
                catalog_provider_key(Some(key.provider().as_str())).as_deref(),
                key.model(),
            )
            .await;
            self.cost_rates = Some((key, rates));
        }
        self.cost_rates.as_ref().and_then(|(_, r)| *r)
    }

    /// Estimated USD spent this session: every model call, including delegated
    /// sub-agents'. Estimates come from the models.dev catalog; unpriced
    /// models (local servers) count as $0.
    pub fn session_cost(&self) -> f64 {
        *self.cost_total.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether [`session_cost`](Self::session_cost) is only a floor: some call
    /// this session (this agent's or a sub-agent's) ran on an unpriced model
    /// and was excluded from the total. Only ever true under
    /// [`AgentConfig::allow_unpriced`](crate::AgentConfig::allow_unpriced); a
    /// frontend that shows the total must flag it (`≥ $X`) when this is set.
    pub fn session_cost_partial(&self) -> bool {
        self.cost_partial.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Zero the session cost counter (session reset — the counter tracks the
    /// *session*, not the process).
    ///
    /// Clears the partial flag with it: the unpriced call that set it belonged
    /// to the conversation being thrown away, and a latch that outlives its
    /// total renders a brand-new fully-priced conversation as `≥ $X` forever
    /// (the frontend tally latches on the flag the agent reports). Only the
    /// *reset* clears it — [`set_session_cost`](Self::set_session_cost) seeds a
    /// resumed conversation, whose restored total is still as partial as it was
    /// when it was saved. Clearing the shared `Arc` here is safe because the one
    /// caller, [`Agent::clear`](crate::Agent::clear), aborts background
    /// sub-agents before it.
    pub fn reset_session_cost(&self) {
        self.set_session_cost(0.0);
        self.cost_partial
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Seed the cost counter — a resumed conversation has already spent something,
    /// so the agent counts on from there.
    ///
    /// The agent reports this total with every `Usage` event, and that is what the
    /// counters show. A frontend adding a saved base on top of the agent's figure
    /// would be keeping a second, divergent tally of the same number.
    pub fn set_session_cost(&self, usd: f64) {
        *self.cost_total.lock().unwrap_or_else(|p| p.into_inner()) = usd;
    }

    /// Check the per-session cost budget before issuing a model call.
    ///
    /// Returns an error when:
    /// - The cap has been reached (`cost_total ≥ max_cost`).
    /// - The cap is set but the current model has no price in the catalog
    ///   (the budget cannot be enforced for an unpriced model) — UNLESS
    ///   [`AgentConfig::allow_unpriced`](crate::AgentConfig::allow_unpriced) is
    ///   set, in which case the unpriced call proceeds uncounted.
    ///
    /// The cap-exhausted check runs first and is model-agnostic: once priced
    /// usage reaches the cap, the run stops whatever model is next in force.
    pub(crate) async fn budget_preflight(&mut self) -> Result<()> {
        let Some(cap) = self.max_cost else {
            return Ok(());
        };
        let spent = *self.cost_total.lock().unwrap_or_else(|p| p.into_inner());
        if spent >= cap {
            bail!("cost budget exhausted: est. ${spent:.2} ≥ cap ${cap:.2}");
        }
        if !self.allow_unpriced && self.current_cost_rates().await.is_none() {
            let model = self.resolved.reference();
            bail!(
                "cost budget cannot be enforced for unpriced model {model}; \
                 remove max_cost, pass --allow-unpriced, or choose a priced model"
            );
        }
        Ok(())
    }

    /// Account for one model call: extract token counts from the stream
    /// accumulator, price the call via the catalog, and accumulate into the
    /// session total.
    ///
    ///
    /// `tool_tokens` is the caller's estimate of the `tools[]` block it sent with
    /// this request (see [`crate::estimate_tokens_in_tools`]) — the tool surface
    /// is part of the prompt on every call, so the no-usage fallback below is
    /// short by thousands of tokens without it. Callers compute it once per turn
    /// from the defs they already hold, and pass `0` for a round that sends no
    /// tools at all (the wrap-up round). It is ignored entirely when the server
    /// reports usage: that number already counts the tools.
    pub(crate) async fn account_usage(
        &mut self,
        acc: &Accumulator,
        tool_tokens: u32,
    ) -> crate::usage::CallSpend {
        let (prompt_tokens, completion_tokens) = match &acc.usage {
            Some(usage) => (usage.prompt_tokens, usage.completion_tokens),
            // A server that reports nothing: estimate every channel the model
            // was billed for, not just the visible text. Reasoning and tool-call
            // arguments are completion tokens too, and a round that writes a
            // file is almost entirely the latter — counting content alone put
            // this figure near zero for exactly the busiest rounds, which then
            // understated throughput and the compaction trigger alike. The
            // prompt side adds the tool schemas for the same reason: they are
            // sent with every request and are the single largest fixed block in
            // it, so omitting them made the gauge and the compaction trigger
            // read low by a constant several thousand tokens.
            None => (
                estimate_tokens_in_messages(&self.messages, self.client.token_target())
                    .saturating_add(tool_tokens),
                estimate_tokens(&acc.content)
                    .saturating_add(estimate_tokens(&acc.reasoning))
                    .saturating_add(acc.tool_call_tokens()),
            ),
        };
        let cached_prompt_tokens = acc.usage.as_ref().and_then(|usage| usage.cached_tokens());
        // Prompt tokens *written* into the cache on this call. Priced at a
        // premium over plain input (1.25x / 2x by TTL), and hrdr's rolling
        // breakpoint writes the cache on nearly every turn, so leaving these in
        // the plain-input bucket under-billed the session — and with it the
        // `max_cost` cap that `budget_preflight` enforces.
        let cache_creation_tokens = acc
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_creation_tokens());
        let cost_usd = self.current_cost_rates().await.map(|rates| {
            rates.call_cost(
                prompt_tokens,
                completion_tokens,
                cached_prompt_tokens,
                cache_creation_tokens,
                // Which write multiplier the fallback uses — 1.25x on the
                // 5-minute TTL, 2x on the 1-hour one. Read off the client that
                // is about to make (or just made) the call rather than off the
                // config, because the TTL travels with the identity and a
                // `/model` switch can change it mid-session; asking the client
                // is what keeps the estimate describing the same call the
                // request did. Unused entirely for a model whose catalog entry
                // publishes a real `cache_write` rate.
                self.client.cache_ttl_1h(),
            )
        });
        // An unpriced call just happened and its cost is unknown, so it is not in
        // the running total: mark the total a floor. `session_cost_usd` stays
        // `None` until a priced call gives a figure to floor, so a purely local
        // session shows no cost at all (unchanged) rather than "≥ $0.00".
        if cost_usd.is_none() {
            self.cost_partial
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let session_cost_usd = {
            let mut total = self.cost_total.lock().unwrap_or_else(|p| p.into_inner());
            *total += cost_usd.unwrap_or(0.0);
            (*total > 0.0).then_some(*total)
        };
        crate::usage::CallSpend {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            cache_creation_tokens,
            cost_usd,
            session_cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use hrdr_llm::{Accumulator, ToolDef, Usage};

    use std::sync::Arc;

    use crate::{Agent, AgentConfig, ChatMessage};

    /// One tool with a schema roughly the size of a real hrdr tool's.
    fn fat_tool() -> ToolDef {
        let mut props = serde_json::Map::new();
        for i in 0..30 {
            props.insert(
                format!("field_{i}"),
                serde_json::json!({"type": "string", "description": "d".repeat(300)}),
            );
        }
        ToolDef::function(
            "write",
            "writes a file",
            serde_json::json!({"type": "object", "properties": props}),
        )
    }

    fn agent_with_history() -> Agent {
        let mut agent = Agent::new(AgentConfig::default()).unwrap();
        Arc::make_mut(&mut agent.messages).push(ChatMessage::user("hello"));
        agent
    }

    /// The bug this guards: a server that reports no usage left the tool surface
    /// out of the prompt estimate, so `last_prompt_tokens` — and with it the
    /// compaction trigger and the context gauge — read low by however many
    /// thousand tokens the schemas take.
    #[tokio::test]
    async fn no_usage_fallback_counts_the_tool_surface() {
        let tools = [fat_tool()];
        let expected = crate::estimate_tokens_in_tools(&tools);
        assert!(
            expected > 500,
            "the fixture is a big schema, not a rounding error"
        );

        let acc = Accumulator::new();
        let without = agent_with_history()
            .account_usage(&acc, 0)
            .await
            .prompt_tokens;
        let with = agent_with_history()
            .account_usage(&acc, expected)
            .await
            .prompt_tokens;
        assert_eq!(
            with,
            without + expected,
            "the tools sent with the request must land in the prompt estimate"
        );
    }

    /// The wiring, end to end: a history whose **text** sits comfortably under
    /// the compaction trigger, but which carries images, is over it once the
    /// images are counted — so the agent compacts instead of walking into a
    /// context-overflow 400.
    ///
    /// This is the path a real turn takes on an endpoint that reports no usage:
    /// `account_usage` estimates the prompt, the turn loop stores it as
    /// `last_prompt_tokens`, and `should_auto_compact` — the one predicate the
    /// agent and every frontend gauge share — reads it. An attachment used to
    /// contribute nothing to that number at all.
    #[tokio::test]
    async fn attachments_push_a_history_over_the_compaction_trigger() {
        const WINDOW: u32 = 131_072;
        const RESERVED: u32 = 8_192;
        let trigger = crate::compaction::compaction_trigger(WINDOW, RESERVED);
        let acc = Accumulator::new();

        // What the agent's own system prompt already costs, so the turn below
        // can be sized to land just under the trigger rather than against a
        // number this test would have to guess.
        let baseline = Agent::new(AgentConfig::default())
            .unwrap()
            .account_usage(&acc, 0)
            .await
            .prompt_tokens;

        // Text alone: 1,000 tokens clear of the trigger.
        let text_tokens = trigger - baseline - 1_000;
        let mut msg = ChatMessage::user("x".repeat(text_tokens as usize * 4));
        let mut agent = Agent::new(AgentConfig::default()).unwrap();
        Arc::make_mut(&mut agent.messages).push(msg.clone());
        let text_only = agent.account_usage(&acc, 0).await.prompt_tokens;
        assert!(
            !crate::compaction::should_auto_compact(Some(text_only), Some(WINDOW), RESERVED, true),
            "the text alone must be under the trigger, or this test proves nothing \
             ({text_only} vs {trigger})"
        );

        // The same turn with a screenshot on it, and no text changed. The
        // default endpoint is a local OpenAI-compatible server, so the image is
        // priced at OpenAI's 32×32 patches: ⌈1000/32⌉² = 1,024. The same bytes
        // bound for Anthropic cost 1,296 (⌈1000/28⌉²) — asserted below, because
        // the endpoint reaching the estimator is the whole point of it being a
        // parameter.
        msg.attachments = vec![
            hrdr_llm::media::Attachment::new(
                crate::compaction::tests::png_sized(1_000, 1_000),
                hrdr_llm::media::MediaType::Png,
                "shot.png",
            )
            .expect("a valid png"),
        ];
        let mut agent = Agent::new(AgentConfig::default()).unwrap();
        Arc::make_mut(&mut agent.messages).push(msg);
        let with_image = agent.account_usage(&acc, 0).await.prompt_tokens;

        assert!(
            crate::compaction::should_auto_compact(Some(with_image), Some(WINDOW), RESERVED, true),
            "the image is what takes this history over the trigger \
             ({with_image} vs {trigger})"
        );
        assert_eq!(
            with_image,
            text_only + 1_024,
            "and it is charged its visual tokens, nothing else having changed"
        );

        // Repointed at Anthropic, the identical history estimates higher — the
        // agent's own client is what decides, not a constant compiled in here.
        agent.client.set_base_url("https://api.anthropic.com/v1");
        assert_eq!(
            agent.account_usage(&acc, 0).await.prompt_tokens,
            text_only + 1_296,
            "the same screenshot costs Anthropic's 28px patches there"
        );
    }

    /// The server-reported path is untouched: its number already counts the
    /// tools, so adding an estimate on top would double-count them.
    #[tokio::test]
    async fn server_reported_usage_ignores_the_tool_estimate() {
        let mut acc = Accumulator::new();
        acc.usage = Some(Usage {
            prompt_tokens: 4321,
            completion_tokens: 77,
            ..Default::default()
        });

        let spend = agent_with_history()
            .account_usage(&acc, crate::estimate_tokens_in_tools(&[fat_tool()]))
            .await;
        assert_eq!(
            spend.prompt_tokens, 4321,
            "the server's prompt count is used verbatim"
        );
        assert_eq!(spend.completion_tokens, 77);
    }

    /// A session reset takes the partial latch with the total it qualifies.
    ///
    /// The bug: `/clear` (`Agent::clear`) zeroed `cost_total` and left the flag
    /// set, so the next `Usage` event still shipped `cost_partial: true` and the
    /// frontend tally re-latched on it — a brand-new, fully-priced conversation
    /// rendering as `≥ $X` for the rest of the process. Seeding a *resumed*
    /// total is the other case and must keep the flag: that total really is
    /// partial.
    #[test]
    fn resetting_the_session_cost_clears_the_partial_latch() {
        use std::sync::atomic::Ordering::Relaxed;

        let agent = Agent::new(AgentConfig::default()).unwrap();
        agent.cost_partial.store(true, Relaxed);
        agent.set_session_cost(0.42);

        agent.reset_session_cost();
        assert_eq!(agent.session_cost(), 0.0);
        assert!(
            !agent.session_cost_partial(),
            "the unpriced call that set this belonged to the cleared conversation"
        );

        // Resume seeding is not a reset: a restored partial total stays partial.
        agent.cost_partial.store(true, Relaxed);
        agent.set_session_cost(0.42);
        assert!(
            agent.session_cost_partial(),
            "seeding a resumed total must not clear the latch"
        );
    }

    /// The cache-write premium is priced, not swallowed. `account_usage` has no
    /// price card in a test (no catalog), so this checks the arithmetic on the
    /// card directly — the seam `account_usage` feeds — with the numbers a
    /// real turn produces: hrdr's rolling breakpoint writes the cache on nearly
    /// every turn, and pricing those tokens as plain input under-reports the
    /// session and loosens the `max_cost` cap that `budget_preflight` enforces.
    #[test]
    fn cache_writes_cost_more_than_plain_input() {
        // Claude Opus 5's published card: $5/MTok in, $25 out, $0.50 read.
        let card = hrdr_llm::catalog::ModelCost {
            input: 5.0,
            output: 25.0,
            cache_read: Some(0.5),
            cache_write: None,
        };
        // 100k prompt: 60k read, 30k written, 10k plain.
        let with_write = card.call_cost(100_000, 0, Some(60_000), Some(30_000), false);
        // What the old code charged, folding the 30k write into plain input.
        let as_plain = card.call_cost(100_000, 0, Some(60_000), None, false);
        assert!(
            with_write > as_plain,
            "a cache write must cost more than plain input: {with_write} vs {as_plain}"
        );
        // Exactly the 1.25x premium on the written tokens.
        let premium = 30_000.0 / 1e6 * 5.0 * 0.25;
        assert!((with_write - as_plain - premium).abs() < 1e-9);
    }
}
