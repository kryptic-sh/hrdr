use std::sync::Arc;

use hrdr_agent::Agent;
use tokio::sync::Mutex;

use super::helpers::busy_generic;
use super::host::CommandHost;

/// Switch the live agent's model and, in the *same* locked step, re-probe the
/// endpoint for the new model's advertised context window — delivering it to the
/// UI so auto-compaction honors the model's real max. Folding the probe into the
/// model-change future (rather than a separate task) avoids racing a probe of the
/// old model against the switch.
pub(crate) fn switch_model(host: &mut dyn CommandHost, name: String) {
    host.set_model(name.clone());
    // Remember (current provider, new model) as the last-used combo.
    hrdr_agent::record_last_model(&host.provider().unwrap_or_default(), &name);
    let agent = host.agent();
    let post = host.context_window_poster_for(host.provider(), name.clone());
    host.spawn_line(Box::pin(async move {
        let (client, provider) = {
            let mut a = agent.lock().await;
            a.set_model(name);
            (a.client(), a.provider_name().map(str::to_string))
        };
        if let Some(w) = Agent::probe_context_window_for(client, provider).await {
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
    // Probe the new endpoint for its advertised context window unless the
    // provider config already declares one (which wins).
    let probe_after = p.context_window.is_none();
    let post = host.context_window_poster_for(
        Some(name.to_string()),
        p.model.clone().unwrap_or_else(|| host.model()),
    );
    host.spawn_line(Box::pin(async move {
        let (client, provider) = {
            let mut a = agent.lock().await;
            a.set_endpoint(url, key);
            a.set_headers(headers);
            a.set_api_version(api_version);
            a.set_provider(Some(provider.clone()), p.kind);
            if let Some(m) = model {
                a.set_model(m);
            }
            (a.client(), Some(provider))
        };
        if probe_after && let Some(w) = Agent::probe_context_window_for(client, provider).await {
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
    let context_window = choice_context_window.or(p.context_window);
    let probe_after = context_window.is_none();
    let post = host.context_window_poster_for(Some(provider_name.to_string()), model.clone());
    let model_for_agent = model.clone();
    host.spawn_line(Box::pin(async move {
        let (client, provider) = {
            let mut a = agent.lock().await;
            a.set_endpoint(url, key);
            a.set_headers(headers);
            a.set_api_version(api_version);
            a.set_provider(Some(provider.clone()), p.kind);
            a.set_model(model_for_agent);
            (a.client(), Some(provider))
        };
        if probe_after && let Some(w) = Agent::probe_context_window_for(client, provider).await {
            post(w);
        }
        String::new()
    }));
    host.set_model(model.clone());
    if context_window.is_some() {
        host.set_context_window(context_window);
    }
    host.set_base_url(p.base_url.clone());
    host.set_provider(provider_name.to_string());
    // Remember this combo so a later launch with nothing pinned resumes it.
    hrdr_agent::record_last_model(provider_name, &model);
    Ok(())
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
    let client = agent.lock().await.client();
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
