use std::sync::Arc;

use hrdr_agent::{Agent, ModelRef, ModelSpec, ProviderName};
use tokio::sync::Mutex;

use super::helpers::busy_generic;
use super::host::CommandHost;

/// Switch **the active agent's** model — `/model <spec>`, where the spec is either
/// a bare model id (same provider) or a whole `provider://model` identity — and,
/// in the *same* locked step, re-probe the endpoint for the new model's advertised
/// context window, delivering it to the UI so auto-compaction honors the model's
/// real max. Folding the probe into the switch future (rather than a separate
/// task) avoids racing a probe of the old model against the switch.
///
/// `/model` means "change the model of the conversation I am looking at", exactly
/// as `/compact` means "compact the conversation I am looking at".
/// [`set_model_ref`] writes to that same agent's chrome, so the status bar follows
/// it — the two are the same piece of state, not a display copy of one.
///
/// [`set_model_ref`]: CommandHost::set_model_ref
pub(crate) fn switch_model(host: &mut dyn CommandHost, name: String) {
    // A bare id keeps the provider in force; `provider://model` replaces it whole.
    // Either way what reaches the agent is a COMPLETE identity.
    let spec: ModelSpec = match name.parse() {
        Ok(s) => s,
        Err(e) => {
            host.info(format!("/model {name}: {e}"));
            return;
        }
    };
    let Some(reference) = spec.apply(&host.model_ref()) else {
        // `/model openai://` names a provider and no model — the one shape a spec
        // cannot resolve by itself. This is an INTERACTIVE switch, so it gets the
        // interactive policy: the model you last used on that provider, else the one
        // it declares, else a picker filtered to it ([`apply_provider_or_pick`]).
        let provider = spec.provider().expect("ProviderOnly names a provider");
        let _ = apply_provider_or_pick(host, provider.as_str());
        return;
    };
    apply_reference(host, reference, None, true);
}

/// Why switching to a named provider didn't happen.
///
/// [`NeedsModel`](Self::NeedsModel) is the one that matters: a provider names an
/// endpoint, not a model, and the model you were using belongs to the provider you
/// are leaving. It is a *question for the user*, not a failure — a frontend with a
/// picker answers it by opening one ([`CommandHost::begin_model_selector_for`]),
/// and only a frontend without one surfaces the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSwitchError {
    /// No built-in and no `[providers.<name>]` by that name.
    Unknown(String),
    /// A turn is running.
    Busy(String),
    /// The provider declares no model, and none was ever used on it.
    NeedsModel { provider: String, message: String },
}

impl std::fmt::Display for ProviderSwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(m) | Self::Busy(m) => f.write_str(m),
            Self::NeedsModel { message, .. } => f.write_str(message),
        }
    }
}

/// Switch the live session to provider `name` — the `/login` path, and the only
/// one that names a provider WITHOUT a model.
///
/// Which model? **Not the one you were using**: that one belongs to the provider
/// you are leaving, and following you onto this one is the bug this refactor
/// exists to kill. [`hrdr_agent::model_for_provider`] answers it — the model you
/// last used on THIS provider, else one the provider itself declares — and when it
/// has no answer, this says so ([`ProviderSwitchError::NeedsModel`]) rather than
/// inventing one.
pub fn apply_provider(
    host: &mut dyn CommandHost,
    name: &str,
) -> Result<hrdr_agent::ResolvedProvider, ProviderSwitchError> {
    let Some(p) = host.resolve_provider(name) else {
        return Err(ProviderSwitchError::Unknown(format!(
            "unknown provider '{name}'"
        )));
    };
    if host.is_busy() {
        return Err(ProviderSwitchError::Busy(busy_generic()));
    }
    let provider = ProviderName::new(name);
    let reference = hrdr_agent::model_for_resolved_provider(&provider, &p).map_err(|e| {
        ProviderSwitchError::NeedsModel {
            provider: name.to_string(),
            message: format!("{e:#}"),
        }
    })?;
    apply_reference(host, reference, p.context_window, true);
    Ok(p)
}

/// Switch to provider `name`, and when it cannot name a model, **ask** — open the
/// model picker filtered to that provider instead of reporting an error the user
/// can do nothing with. The `/login` completion path.
///
/// Returns the resolved provider when the switch happened.
pub fn apply_provider_or_pick(
    host: &mut dyn CommandHost,
    name: &str,
) -> Result<hrdr_agent::ResolvedProvider, ProviderSwitchError> {
    let outcome = apply_provider(host, name);
    if let Err(ProviderSwitchError::NeedsModel { provider, message }) = &outcome {
        host.info(format!("{message} — pick one:"));
        host.begin_model_selector_for(provider);
    }
    outcome
}

/// The ONE place an identity is applied to the agent *and* to the chrome that
/// describes it — so the two cannot drift apart.
///
/// `window` is a window already known for the target (a picker row's, or the
/// provider's configured one), which wins over a probe; `remember` records the
/// identity as last-used (a deliberate choice — a session resume does not).
fn apply_reference(
    host: &mut dyn CommandHost,
    reference: ModelRef,
    window: Option<u32>,
    remember: bool,
) {
    // Is this identity even REAL? Ask before anything moves, so that a refusal — a
    // ChatGPT model this account is not entitled to — leaves the agent *and* the
    // chrome that describes it on the identity in force, together. The check is
    // network-free (cached catalogs only), so it is affordable on the UI thread.
    //
    // `try_lock` fails only while a turn holds the agent. The switch future below
    // then runs the same check under the real lock (`validated` says whether it
    // still has to), so a refusal is never skipped — it merely arrives with the
    // future rather than with the keystroke, and the agent still never moves.
    let agent = host.agent();
    let mut warnings = Vec::new();
    let mut validated = false;
    if let Ok(a) = agent.try_lock() {
        match a.validate_ref(&reference) {
            Ok(w) => {
                warnings = w;
                validated = true;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                drop(a);
                host.info(msg);
                return;
            }
        }
    }
    // What merely LOOKS wrong (models.dev has never heard of this id) is said, not
    // enforced: the catalog lags every release, and a model shipped today must run.
    for w in warnings {
        host.info(w);
    }

    // A change of PROVIDER moves the endpoint; a change of model on the provider
    // you are already on does not — that would undo a `--base-url` relocation,
    // which is where this provider lives for this session. Same rule as the agent's.
    let moving_provider = host.model_ref().provider() != reference.provider();
    let endpoint = moving_provider
        .then(|| host.resolve_provider(reference.provider().as_str()))
        .flatten()
        .map(|p| p.base_url);

    host.set_model_ref(reference.clone());
    if let Some(url) = endpoint {
        host.set_base_url(url);
    }
    if let Some(w) = window {
        host.set_context_window(Some(w));
    }
    if remember {
        // Remembered per provider, so a later `/login <provider>` (which names no
        // model) can come back to the model you were actually using there.
        hrdr_agent::record_last_model(&reference);
    }

    let post = host.context_window_poster();
    let probe_after = window.is_none();
    host.spawn_line(Box::pin(async move {
        let mut a = agent.lock().await;
        // The busy-agent path: validate under the real lock. A refusal returns before
        // `set_model_ref`, so the agent stays exactly where it is.
        let mut deferred = String::new();
        if !validated {
            match a.validate_ref(&reference) {
                Ok(w) => deferred = w.join("\n"),
                Err(e) => return format!("{e:#}"),
            }
        }
        // ONE call: endpoint, key, api-version, headers, trust kind and model move
        // together, under the same lock, so a probe can never see a half-switch.
        if let Err(e) = a.set_model_ref(reference) {
            return format!("{e:#}");
        }
        if probe_after && let Some(w) = a.probe_context_window().await {
            post(w);
        }
        deferred
    }));
}

/// Repoint the agent to the `(provider, model)` a **resumed session** was on.
///
/// A conversation's model and provider are part of it: resuming one and then
/// talking to a different provider's endpoint is not the same conversation. The
/// chrome already adopts them, and the agent has to be repointed with them —
/// otherwise the two disagree, and it is the agent that is doing the talking.
///
/// Regression: resume adopted the session's model *name* and provider *label*
/// into the display, told the agent only the model, and left it pointing at
/// whatever endpoint the process launched on. A session saved on one provider,
/// resumed in a process configured for another, showed the right thing in the
/// status bar and sent the request somewhere else — where the model does not exist
/// and the key is not valid.
///
/// Nothing is remembered as a "last used" combo (unlike [`apply_choice`]): a
/// resume is restoring a conversation, not choosing a model.
pub fn restore_session_provider(
    host: &mut dyn CommandHost,
    provider_name: &str,
    model: String,
    saved_window: Option<u32>,
) -> Result<(), String> {
    repoint(host, provider_name, model, saved_window, false)
}

/// Switch to a specific `(provider, model)` pair chosen in the `/model`
/// selector — repoint the endpoint/key/headers to `provider` *and* set the
/// exact `model` in one locked step (so a probe can't race a half-applied
/// switch), then update the displayed chrome. Like [`apply_provider`] but with
/// an explicit model instead of the provider's default. `Err(message)` when the
/// provider is unknown or a turn is running.
pub fn apply_choice(
    host: &mut dyn CommandHost,
    provider_name: &str,
    model: String,
    choice_context_window: Option<u32>,
) -> Result<(), String> {
    repoint(host, provider_name, model, choice_context_window, true)
}

/// Apply a `(provider, model)` pair from a two-key source (a picker row, a saved
/// session) — collapsed into one identity here, at the edge, and applied through
/// [`apply_reference`]. `remember` records it as the last-used identity (a
/// deliberate choice), which a resume does not.
fn repoint(
    host: &mut dyn CommandHost,
    provider_name: &str,
    model: String,
    choice_context_window: Option<u32>,
    remember: bool,
) -> Result<(), String> {
    let Some(p) = host.resolve_provider(provider_name) else {
        return Err(format!("unknown provider '{provider_name}'"));
    };
    if host.is_busy() {
        return Err(busy_generic());
    }
    let reference = ModelRef::new(ProviderName::new(provider_name), &model)
        .map_err(|e| format!("{provider_name}://{model}: {e}"))?;
    // Prefer the chosen model's own context window (e.g. an entitled ChatGPT
    // row) over the provider's; probe the endpoint only when neither is known.
    let window = choice_context_window.or(p.context_window);
    apply_reference(host, reference, window, remember);
    Ok(())
}

/// Guidance for a remote endpoint we hold no credential for. This is a *config*
/// problem, not a reachability one — so it must not be reported as one.
///
/// Regression: the health probe called `/models` unauthenticated, got the 401 it
/// was always going to get, and rendered it through [`unreachable_guidance`] —
/// telling a user whose only mistake was not running `/login` that api.openai.com
/// "looks unreachable" and suggesting they start a local llama-server on it.
pub(crate) fn missing_credential_guidance(provider: Option<&str>, base_url: &str) -> String {
    let who = provider.unwrap_or(base_url);
    format!(
        "⚠ no API key configured for {who} — hrdr won't call it until there is one.\n\
         Run `/login` to set one up, or set the provider's key env var. To use a local \
         model instead, point hrdr at a server that needs no key (e.g. `infr serve <model> \
         --addr 127.0.0.1:8080`)."
    )
}

/// First-run guidance for an unreachable endpoint: what `base_url` failed and
/// how to get hrdr talking to a model. Pure (no I/O) so it's unit-testable and
/// identical across frontends. Now that hrdr never spawns a server, this is the
/// nudge a fresh user sees when nothing is listening yet.
pub(crate) fn unreachable_guidance(base_url: &str, err: &str) -> String {
    format!(
        "⚠ endpoint {base_url} looks unreachable: {err}\n\
         hrdr talks to a running OpenAI-compatible server. Start one listening at \
         {base_url} — e.g. `infr serve <model> --addr 127.0.0.1:8080` or `llama-server \
         -hf <ref> --jinja --port 8080` — and it connects on your next message. Or run \
         `/login` to set up a hosted provider (zen/openai/openrouter/claude) and its API key."
    )
}

/// Probe the endpoint (list its models) and return a warning line when it
/// looks unreachable, doesn't advertise `model`, or is being addressed with the
/// `default` placeholder despite serving a model list; `None` when healthy. The
/// startup health-check core — both frontends spawn it and surface the
/// warning as a system line before the first turn.
///
/// The `default` rule rides on THIS probe rather than one of its own: `/v1/models`
/// is already on the wire here, and its answer is exactly what decides whether
/// `default` names anything (see [`hrdr_agent::validate_placeholder_model`]).
pub async fn endpoint_health_warning(
    agent: Arc<Mutex<Agent>>,
    model: String,
    base_url: String,
) -> Option<String> {
    let (client, kind, has_credential, provider, reference) = {
        let a = agent.lock().await;
        (
            a.client(),
            a.provider_kind(),
            a.has_credential(),
            a.provider_name().to_string(),
            a.model_ref().clone(),
        )
    };
    // Never call a provider that requires auth with no auth. The 401 that comes
    // back is a fact about the missing key, not about the endpoint — reporting it
    // as "unreachable" sends the user off to debug a server that is fine. A local
    // endpoint legitimately needs no key, so it is still probed.
    if !has_credential && !hrdr_agent::is_local_endpoint(&base_url) {
        return Some(missing_credential_guidance(Some(&provider), &base_url));
    }
    // Trusted ChatGPT OAuth: the Codex backend does not expose the generic
    // unauthenticated `/models` shape, so this probe only yields a false 401
    // warning. Its authenticated catalog (Task 3) has its own health/fallback
    // path and still surfaces a genuine 401/403 as an auth warning, so a real
    // revoked credential is not masked. Discovered + fixed in v1
    // (fix-chatgpt-oauth-model-discovery, endpoint_health_warning). Custom
    // shadows keep probing.
    if kind == hrdr_agent::ResolvedProviderKind::ChatGptOAuth {
        return None;
    }
    match client.list_models().await {
        Err(e) => Some(unreachable_guidance(&base_url, &e.to_string())),
        Ok(models) => {
            // `default` is a PLACEHOLDER, and an endpoint that advertises a model
            // namespace has something to name — so it names nothing there.
            if let Err(e) = hrdr_agent::validate_placeholder_model(&reference, Some(&models)) {
                return Some(format!("⚠ {e}"));
            }
            if model != hrdr_agent::PLACEHOLDER_MODEL
                && !models.is_empty()
                && !models.iter().any(|m| m == &model)
            {
                let sample = models
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "⚠ model '{model}' not found at {base_url}; available: {sample}"
                ))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote provider with no credential is not probed at all: the request
    /// would 401 by construction, and a 401 is a fact about the missing key, not
    /// about the endpoint.
    ///
    /// Regression: hrdr called `https://api.openai.com/v1/models` with no bearer,
    /// got "Missing bearer authentication in header", and reported the endpoint as
    /// "unreachable" — advising the user to start a local llama-server on
    /// api.openai.com. The only thing actually wrong was that they hadn't run
    /// `/login`.
    ///
    /// The endpoint here is real, but no call is made — the guard returns before
    /// `list_models`.
    #[tokio::test]
    async fn a_remote_provider_with_no_key_is_told_to_log_in_not_that_it_is_down() {
        let config = hrdr_agent::AgentConfig {
            model: "openai://gpt-5".parse().unwrap(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            ..Default::default()
        };
        let agent = Arc::new(Mutex::new(hrdr_agent::Agent::new(config).unwrap()));
        let warning = endpoint_health_warning(
            agent,
            "gpt-5".to_string(),
            "https://api.openai.com/v1".to_string(),
        )
        .await
        .expect("an unconfigured provider is still worth a word");

        assert!(
            warning.contains("no API key configured for openai") && warning.contains("/login"),
            "it names the real problem and the fix: {warning}"
        );
        assert!(
            !warning.contains("unreachable") && !warning.contains("llama-server"),
            "and does not blame the endpoint: {warning}"
        );
    }

    /// A *local* endpoint with no key is the normal case — llama-server, vLLM and
    /// `infr serve` need none — so it is still probed, and a genuinely dead one
    /// still gets the "start a server" guidance.
    #[tokio::test]
    async fn a_local_endpoint_with_no_key_is_still_probed() {
        let base_url = "http://127.0.0.1:1/v1".to_string(); // nothing listens on port 1
        let config = hrdr_agent::AgentConfig {
            base_url: base_url.clone(),
            api_key: None,
            ..Default::default()
        };
        let agent = Arc::new(Mutex::new(hrdr_agent::Agent::new(config).unwrap()));
        let warning = endpoint_health_warning(agent, "qwen3".to_string(), base_url)
            .await
            .expect("a dead local endpoint is reported");
        assert!(
            warning.contains("looks unreachable"),
            "the no-key guard must not swallow a real local failure: {warning}"
        );
    }

    /// Trusted ChatGPT OAuth skips the generic `/models` probe — the Codex
    /// backend returns a false 401 to it (v1 provenance). The skip happens before
    /// `list_models`, so this returns `None` without any network call.
    #[tokio::test]
    async fn health_probe_skipped_for_trusted_chatgpt_oauth() {
        let config = hrdr_agent::AgentConfig {
            model: "chatgpt://gpt-5.5".parse().unwrap(),
            base_url: hrdr_agent::CHATGPT_CODEX_BASE_URL.to_string(),
            ..Default::default()
        };
        let agent = Arc::new(Mutex::new(hrdr_agent::Agent::new(config).unwrap()));
        let warning = endpoint_health_warning(
            agent,
            "gpt-5.5".to_string(),
            hrdr_agent::CHATGPT_CODEX_BASE_URL.to_string(),
        )
        .await;
        assert!(
            warning.is_none(),
            "trusted ChatGPT OAuth must skip the false-401 probe"
        );
    }
}
