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
/// Shared by `/provider` and `/login`.
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
    let key = key.or_else(|| hrdr_agent::resolve_api_key(name, &p, None));
    let agent = host.agent();
    let (url, model) = (p.base_url.clone(), p.model.clone());
    let headers: Vec<(String, String)> = p.headers.clone().into_iter().collect();
    let api_version = p.api_version.clone();
    // Probe the new endpoint for its advertised context window unless the
    // provider config already declares one (which wins).
    let probe_after = p.context_window.is_none();
    let post = host.context_window_poster();
    host.spawn_line(Box::pin(async move {
        let mut a = agent.lock().await;
        a.set_endpoint(url, key);
        a.set_headers(headers);
        a.set_api_version(api_version);
        if let Some(m) = model {
            a.set_model(m);
        }
        if probe_after && let Some(w) = a.probe_context_window().await {
            post(w);
        }
        String::new()
    }));
    if let Some(m) = &p.model {
        host.set_model(m.clone());
    }
    if p.context_window.is_some() {
        host.set_context_window(p.context_window);
    }
    host.set_base_url(p.base_url.clone());
    Ok(p)
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
