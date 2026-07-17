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

    /// Zero the session cost counter (session reset — the counter tracks the
    /// *session*, not the process).
    pub fn reset_session_cost(&self) {
        self.set_session_cost(0.0);
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
    ///   (the budget cannot be enforced for an unpriced model).
    pub(crate) async fn budget_preflight(&mut self) -> Result<()> {
        let Some(cap) = self.max_cost else {
            return Ok(());
        };
        let spent = *self.cost_total.lock().unwrap_or_else(|p| p.into_inner());
        if spent >= cap {
            bail!("cost budget exhausted: est. ${spent:.2} ≥ cap ${cap:.2}");
        }
        if self.current_cost_rates().await.is_none() {
            let model = self.resolved.reference();
            bail!(
                "cost budget cannot be enforced for unpriced model {model}; \
                 remove max_cost or choose a priced model"
            );
        }
        Ok(())
    }

    /// Account for one model call: extract token counts from the stream
    /// accumulator, price the call via the catalog, and accumulate into the
    /// session total.
    ///
    /// Returns `(prompt_tokens, completion_tokens, cached_prompt_tokens,
    /// cost_usd, session_cost_usd)`.
    pub(crate) async fn account_usage(
        &mut self,
        acc: &Accumulator,
    ) -> (u32, u32, Option<u32>, Option<f64>, Option<f64>) {
        let (prompt_tokens, completion_tokens) = match &acc.usage {
            Some(usage) => (usage.prompt_tokens, usage.completion_tokens),
            None => (
                estimate_tokens_in_messages(&self.messages),
                estimate_tokens(&acc.content),
            ),
        };
        let cached_prompt_tokens = acc.usage.as_ref().and_then(|usage| usage.cached_tokens());
        let cost_usd = self
            .current_cost_rates()
            .await
            .map(|rates| rates.call_cost(prompt_tokens, completion_tokens, cached_prompt_tokens));
        let session_cost_usd = {
            let mut total = self.cost_total.lock().unwrap_or_else(|p| p.into_inner());
            *total += cost_usd.unwrap_or(0.0);
            (*total > 0.0).then_some(*total)
        };
        (
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            cost_usd,
            session_cost_usd,
        )
    }
}
