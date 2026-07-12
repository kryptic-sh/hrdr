use std::sync::Arc;

use hrdr_agent::Agent;
use tokio::sync::Mutex;

use super::helpers::busy_generic;
use super::host::CommandHost;

/// Switch **the active agent's** model and, in the *same* locked step, re-probe
/// the endpoint for the new model's advertised context window — delivering it to
/// the UI so auto-compaction honors the model's real max. Folding the probe into
/// the model-change future (rather than a separate task) avoids racing a probe of
/// the old model against the switch.
///
/// `/model` means "change the model of the conversation I am looking at", exactly
/// as `/compact` means "compact the conversation I am looking at". [`set_model`]
/// writes to that same agent's chrome, so the status bar follows it — the two are
/// the same piece of state, not a display copy of one.
///
/// [`set_model`]: CommandHost::set_model
pub(crate) fn switch_model(host: &mut dyn CommandHost, name: String) {
    host.set_model(name.clone());
    // Remember (current provider, new model) as the last-used combo.
    hrdr_agent::record_last_model(&host.provider().unwrap_or_default(), &name);
    let agent = host.agent();
    let post = host.context_window_poster();
    host.spawn_line(Box::pin(async move {
        let mut a = agent.lock().await;
        a.set_model(name);
        if let Some(w) = a.probe_context_window().await {
            post(w);
        }
        String::new()
    }));
}

/// Switch the live session to provider `name`: repoint the endpoint (with the
/// resolved or supplied API key), model, and context window, and update the
/// displayed chrome. `key` overrides the resolved credential — the `/login`
/// wizard passes the freshly-entered key. Returns the resolved provider on
/// success; `Err(message)` when the name is unknown or a turn is running.
/// Used by the `/login` wizard.
pub fn apply_provider(
    host: &mut dyn CommandHost,
    name: &str,
    key: Option<String>,
) -> Result<hrdr_agent::ResolvedProvider, String> {
    let Some(p) = host.resolve_provider(name) else {
        return Err(format!("unknown provider '{name}'"));
    };
    if host.is_busy() {
        return Err(busy_generic());
    }
    let key = key.or_else(|| hrdr_agent::resolve_api_key(name, &p, None, None));
    let agent = host.agent();
    let (url, model) = (p.base_url.clone(), p.model.clone());
    // The catalog fallback in `probe_context_window` is keyed `provider/model`.
    let provider = name.to_string();
    let headers: Vec<(String, String)> = p.headers.clone().into_iter().collect();
    let api_version = p.api_version.clone();
    // Trust identity travels with the endpoint: a stale kind would misgate OAuth
    // injection on the next request.
    let kind = p.kind;
    // Probe the new endpoint for its advertised context window unless the
    // provider config already declares one (which wins).
    let probe_after = p.context_window.is_none();
    let post = host.context_window_poster();
    host.spawn_line(Box::pin(async move {
        let mut a = agent.lock().await;
        a.apply_provider_switch(hrdr_agent::ProviderSwitch {
            name: provider,
            base_url: url,
            api_key: key,
            api_version,
            headers,
            kind,
            model,
        });
        if probe_after && let Some(w) = a.probe_context_window().await {
            post(w);
        }
        String::new()
    }));
    if let Some(m) = &p.model {
        host.set_model(m.clone());
        // Remember (provider, its model) as the last-used combo.
        hrdr_agent::record_last_model(name, m);
    }
    if p.context_window.is_some() {
        host.set_context_window(p.context_window);
    }
    host.set_base_url(p.base_url.clone());
    host.set_provider(name.to_string());
    Ok(p)
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

/// The one place a `(provider, model)` pair is applied to an agent *and* to the
/// chrome that describes it — so the two cannot drift apart. `remember` records it
/// as the last-used combo (a deliberate choice), which a resume does not.
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
    let key = hrdr_agent::resolve_api_key(provider_name, &p, None, None);
    let agent = host.agent();
    let url = p.base_url.clone();
    let provider = provider_name.to_string();
    let headers: Vec<(String, String)> = p.headers.clone().into_iter().collect();
    let api_version = p.api_version.clone();
    let kind = p.kind;
    // Prefer the chosen model's own context window (e.g. an entitled ChatGPT
    // row) over the provider's; probe the endpoint only when neither is known.
    let effective_window = choice_context_window.or(p.context_window);
    let probe_after = effective_window.is_none();
    let post = host.context_window_poster();
    let model_for_agent = model.clone();
    host.spawn_line(Box::pin(async move {
        let mut a = agent.lock().await;
        a.apply_provider_switch(hrdr_agent::ProviderSwitch {
            name: provider,
            base_url: url,
            api_key: key,
            api_version,
            headers,
            kind,
            model: Some(model_for_agent),
        });
        if probe_after && let Some(w) = a.probe_context_window().await {
            post(w);
        }
        String::new()
    }));
    host.set_model(model.clone());
    if effective_window.is_some() {
        host.set_context_window(effective_window);
    }
    host.set_base_url(p.base_url.clone());
    host.set_provider(provider_name.to_string());
    if remember {
        // Remember this combo so a later launch with nothing pinned resumes it.
        hrdr_agent::record_last_model(provider_name, &model);
    }
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
/// looks unreachable or doesn't advertise `model`; `None` when healthy. The
/// startup health-check core — both frontends spawn it and surface the
/// warning as a system line before the first turn.
pub async fn endpoint_health_warning(
    agent: Arc<Mutex<Agent>>,
    model: String,
    base_url: String,
) -> Option<String> {
    let (client, kind, has_credential, provider) = {
        let a = agent.lock().await;
        (
            a.client(),
            a.provider_kind(),
            a.has_credential(),
            a.provider_name(),
        )
    };
    // Never call a provider that requires auth with no auth. The 401 that comes
    // back is a fact about the missing key, not about the endpoint — reporting it
    // as "unreachable" sends the user off to debug a server that is fine. A local
    // endpoint legitimately needs no key, so it is still probed.
    if !has_credential && !hrdr_agent::is_local_endpoint(&base_url) {
        return Some(missing_credential_guidance(provider.as_deref(), &base_url));
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
            if model != "default" && !models.is_empty() && !models.iter().any(|m| m == &model) {
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
            provider: Some("openai".to_string()),
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
            provider: Some("chatgpt".to_string()),
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
