//! HTTP client over `/v1/chat/completions` and `/v1/models`.

use std::io::Write;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::types::{CacheMode, ChatChunk, ChatMessage, ChatRequest, ToolDef};

/// Wire-level debug log, enabled by `HRDR_LOG_REQUESTS=<path>`: every chat
/// request body, every raw SSE data line, and every non-2xx response body is
/// appended to the file as one JSON object per line. For debugging
/// harness ⇄ server disagreements (tool-call framing, stream shape) — off
/// unless the env var is set.
static REQUEST_LOG: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn request_log() -> Option<&'static Mutex<std::fs::File>> {
    REQUEST_LOG
        .get_or_init(|| {
            let path = std::env::var_os("HRDR_LOG_REQUESTS")?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

/// Append one `{"ts":…,"kind":…,…}` line to the wire log (no-op when off).
fn log_wire(kind: &str, fields: serde_json::Value) {
    let Some(file) = request_log() else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut obj = serde_json::json!({"ts": ts, "kind": kind});
    if let (Some(o), Some(f)) = (obj.as_object_mut(), fields.as_object()) {
        for (k, v) in f {
            o.insert(k.clone(), v.clone());
        }
    }
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{obj}");
    }
}

/// Boxed stream of decoded streaming chunks.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// Which wire protocol the endpoint speaks. Auto-detected from `base_url`
/// (Anthropic's own host → native Messages API), else the OpenAI chat-completions
/// shape every other server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    OpenAi,
    Anthropic,
}

/// Default `max_tokens` for the native Anthropic backend (required by the API;
/// well under every current Claude model's output cap).
const ANTHROPIC_MAX_TOKENS: u32 = 8192;

/// A configured chat-completions client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Model id sent with each request (a public field; set it directly).
    pub model: String,
    pub temperature: Option<f32>,
    /// Prompt-caching strategy (default [`CacheMode::Off`]).
    cache: CacheMode,
    /// Use the extended 1-hour cache TTL instead of the ~5-minute default.
    cache_1h: bool,
    /// Reasoning-effort label; sent as `reasoning_effort` when it names a known
    /// level (see [`crate::normalize_effort`]).
    effort: Option<String>,
    /// Opt-in request parameters (`max_tokens`, `top_p`, `seed`, `stop`,
    /// `include_usage`) applied to each request.
    params: crate::RequestParams,
    /// Extra HTTP headers (provider-configured) sent with every request.
    extra_headers: Vec<(String, String)>,
    /// Azure OpenAI API version. When set, requests append `?api-version=<v>` and
    /// authenticate with an `api-key` header instead of `Bearer` (Azure is still
    /// the OpenAI chat-completions wire, just a different URL + auth).
    api_version: Option<String>,
    /// Wire protocol, derived from `base_url`.
    backend: Backend,
}

/// Format a `Retry-After` response header as ` (retry-after: Ns)` so the agent's
/// retry loop can honor the server's requested wait (integer-seconds form only;
/// empty string when the header is absent or not a plain number).
pub(crate) fn retry_after_suffix(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| format!(" (retry-after: {secs}s)"))
        .unwrap_or_default()
}

/// Whether `model` is an OpenAI reasoning model that wants `max_completion_tokens`
/// instead of `max_tokens` (o-series, gpt-5). Handles a provider prefix like
/// `openai/o3-mini`. Non-OpenAI models are unaffected (they use `max_tokens`).
fn uses_max_completion_tokens(model: &str) -> bool {
    let m = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("o5")
        || m.starts_with("gpt-5")
}

/// Native Anthropic Messages API iff the endpoint host is `api.anthropic.com`.
/// Anthropic also exposes an OpenAI-compat endpoint, but it can't cache; the
/// native path is what unlocks prompt caching.
fn detect_backend(base_url: &str) -> Backend {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if host == "api.anthropic.com" || host.ends_with(".anthropic.com") {
        Backend::Anthropic
    } else {
        Backend::OpenAi
    }
}

impl Client {
    /// `base_url` should include the `/v1` suffix, e.g. `http://localhost:8080/v1`.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let backend = detect_backend(&base_url);
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            model: model.into(),
            temperature: None,
            cache: CacheMode::Off,
            cache_1h: false,
            effort: None,
            params: crate::RequestParams::default(),
            extra_headers: Vec::new(),
            api_version: None,
            backend,
        }
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Set the prompt-caching strategy (builder form).
    pub fn with_cache(mut self, cache: CacheMode) -> Self {
        self.cache = cache;
        self
    }

    /// Set the prompt-caching strategy (e.g. after a mid-session provider switch).
    pub fn set_cache(&mut self, cache: CacheMode) {
        self.cache = cache;
    }

    /// Use the extended 1-hour cache TTL (`true`) or the default ~5-minute
    /// ephemeral (`false`).
    pub fn set_cache_ttl_1h(&mut self, one_hour: bool) {
        self.cache_1h = one_hour;
    }

    /// Set the reasoning-effort label; only recognized levels
    /// ([`crate::normalize_effort`]) are actually sent.
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
    }

    /// Set the opt-in request parameters (`max_tokens`, `top_p`, `seed`, `stop`,
    /// `include_usage`).
    pub fn set_params(&mut self, params: crate::RequestParams) {
        self.params = params;
    }

    /// Rebuild the HTTP client with a connect + idle-read timeout (so a hung or
    /// stalled provider fails the request instead of blocking forever). `None`
    /// restores the default (no timeout). The read timeout is per-chunk, so a
    /// slow-but-progressing stream isn't killed. A build error keeps the current
    /// client.
    pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) {
        let mut builder = reqwest::Client::builder();
        if let Some(t) = timeout {
            builder = builder.connect_timeout(t).read_timeout(t);
        }
        if let Ok(http) = builder.build() {
            self.http = http;
        }
    }

    /// Set the provider-configured extra headers sent with every request.
    pub fn set_headers(&mut self, headers: Vec<(String, String)>) {
        self.extra_headers = headers;
    }

    /// Set the Azure OpenAI API version (enables the Azure URL + `api-key` auth
    /// quirks); `None` for a standard OpenAI-compatible endpoint.
    pub fn set_api_version(&mut self, api_version: Option<String>) {
        self.api_version = api_version;
    }

    /// Build a request URL for `path` (e.g. `chat/completions`), appending the
    /// Azure `?api-version=` query when configured.
    fn url(&self, path: &str) -> String {
        match &self.api_version {
            Some(v) => format!("{}/{path}?api-version={v}", self.base_url),
            None => format!("{}/{path}", self.base_url),
        }
    }

    /// The current endpoint base URL (including the `/v1` suffix).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Repoint the client at a different endpoint (for mid-session provider switch).
    pub fn set_base_url(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self.backend = detect_backend(&self.base_url);
    }

    /// Replace the API key (or clear it with `None`).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.api_key = api_key;
    }

    fn request(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        stream: bool,
    ) -> ChatRequest {
        // OpenAI reasoning models want `max_completion_tokens`, not `max_tokens`.
        let (max_tokens, max_completion_tokens) = match self.params.max_tokens {
            Some(n) if uses_max_completion_tokens(&self.model) => (None, Some(n)),
            other => (other, None),
        };
        ChatRequest {
            model: self.model.clone(),
            messages,
            tools,
            temperature: self.temperature,
            reasoning_effort: self.effort.as_deref().and_then(crate::normalize_effort),
            max_tokens,
            max_completion_tokens,
            top_p: self.params.top_p,
            seed: self.params.seed,
            stop: self.params.stop.clone(),
            stream,
            // Ask for token usage on streamed turns (for the live loader stats),
            // unless a strict server rejects `stream_options`.
            stream_options: (stream && self.params.include_usage).then_some(
                crate::types::StreamOptions {
                    include_usage: true,
                },
            ),
        }
    }

    fn post(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.auth(self.http.post(self.url("chat/completions")).json(body))
    }

    /// Apply the backend's auth + any provider-configured extra headers to a
    /// request builder: `x-api-key` + `anthropic-version` for the native
    /// Anthropic backend, else `Bearer`.
    fn auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req = match self.backend {
                Backend::Anthropic => req
                    .header("x-api-key", key)
                    .header("anthropic-version", crate::anthropic::API_VERSION),
                // Azure OpenAI authenticates with an `api-key` header, not Bearer.
                Backend::OpenAi if self.api_version.is_some() => req.header("api-key", key),
                Backend::OpenAi => req.bearer_auth(key),
            };
        }
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        req
    }

    /// Serialize a request and apply cache breakpoints per the active [`CacheMode`].
    fn body_json(&self, body: &ChatRequest) -> serde_json::Value {
        let mut json = serde_json::to_value(body).unwrap_or_default();
        if self.cache == CacheMode::Ephemeral {
            crate::types::apply_cache_breakpoints(&mut json, self.cache_1h);
        }
        json
    }

    /// Streaming completion. Yields decoded chunks as they arrive. Dispatches to
    /// the native Anthropic Messages API or the OpenAI chat-completions shape
    /// based on the detected [`Backend`].
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    ) -> Result<ChatStream> {
        if self.backend == Backend::Anthropic {
            let (body, stream) = crate::anthropic::chat_stream(
                &self.http,
                &self.base_url,
                self.api_key.as_deref(),
                &self.model,
                self.params.max_tokens.unwrap_or(ANTHROPIC_MAX_TOKENS),
                self.effort.as_deref(),
                self.temperature,
                self.cache,
                self.cache_1h,
                &self.extra_headers,
                messages,
                tools,
            )
            .await?;
            log_wire(
                "request",
                serde_json::json!({
                    "url": format!("{}/messages", self.base_url),
                    "body": body,
                }),
            );
            return Ok(stream);
        }
        let body = self.body_json(&self.request(messages, tools, true));
        log_wire(
            "request",
            serde_json::json!({
                "url": self.url("chat/completions"),
                "body": body,
            }),
        );
        let resp = self
            .post(&body)
            .send()
            .await
            .context("chat stream request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let retry = retry_after_suffix(resp.headers());
            let text = resp.text().await.unwrap_or_default();
            log_wire(
                "error_response",
                serde_json::json!({"status": status.as_u16(), "body": text}),
            );
            bail!("chat endpoint returned {status}: {text}{retry}");
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            // Buffer raw bytes and only decode complete lines: a multibyte
            // codepoint split across network chunks must not be decoded lossily
            // mid-sequence (0x0A never occurs inside a UTF-8 sequence, so
            // splitting on it is safe).
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.context("reading stream chunk")?;
                buf.extend_from_slice(&chunk);
                // SSE frames are separated by a blank line; events are `data: ...`.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line[..nl]);
                    let line = line.trim_end_matches('\r');
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    log_wire("sse", serde_json::json!({"data": data}));
                    if data == "[DONE]" {
                        return;
                    }
                    let parsed: ChatChunk = serde_json::from_str(data)
                        .with_context(|| format!("decoding stream event: {data}"))?;
                    yield parsed;
                }
            }
            // Reaching here means the byte stream closed without the [DONE]
            // sentinel — truncated response or network drop. Classify as
            // transient so the agent retry loop can re-request.
            Err(anyhow::anyhow!(
                "incomplete stream: OpenAI stream ended without [DONE] \
                 (partial response, safe to retry)"
            ))?;
        };
        Ok(Box::pin(stream))
    }

    /// List available models from `GET {base_url}/models`.
    /// Returns model ids sorted alphabetically.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let req = self.auth(self.http.get(self.url("models")));
        let resp = req.send().await.context("models request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading models response")?;
        if !status.is_success() {
            bail!("models endpoint returned {status}: {text}");
        }
        let parsed: ModelsResponse = serde_json::from_str(&text)
            .with_context(|| format!("decoding models response: {text}"))?;
        let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Best-effort probe of the server's context window in tokens (for the status
    /// bar's "X of Y" + the auto-compaction threshold). This is **not** part of
    /// the OpenAI spec, but many OpenAI-compatible servers advertise it:
    ///
    /// - on the `/v1/models` entry as a non-standard field — vLLM's
    ///   `max_model_len`, LM Studio's `max_context_length`, and similar;
    /// - or, for llama.cpp, via `GET /props`
    ///   (`default_generation_settings.n_ctx`).
    ///
    /// Returns `None` when nothing exposes it (e.g. OpenAI itself, or infr
    /// today), so the caller can fall back to a configured/default value.
    pub async fn context_window(&self) -> Option<u32> {
        if let Some(n) = self.context_from_models().await {
            return Some(n);
        }
        self.context_from_props().await
    }

    /// Look for a context-length field on this client's model in `/v1/models`
    /// (falling back to the first entry if the id doesn't match).
    async fn context_from_models(&self) -> Option<u32> {
        let v = self.get_json(&self.url("models")).await?;
        let data = v.get("data")?.as_array()?;
        let entry = data
            .iter()
            .find(|e| e.get("id").and_then(|i| i.as_str()) == Some(self.model.as_str()))
            .or_else(|| data.first())?;
        context_field(entry)
    }

    /// llama.cpp exposes the loaded context via `GET /props` (served at the root,
    /// not under `/v1`), either top-level or under `default_generation_settings`.
    async fn context_from_props(&self) -> Option<u32> {
        let root = self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url);
        let v = self.get_json(&format!("{root}/props")).await?;
        context_field(&v).or_else(|| v.get("default_generation_settings").and_then(context_field))
    }

    /// GET `url` with the backend's auth and decode JSON; `None` on any error
    /// (unreachable endpoint, non-2xx, or unparseable body) — detection is
    /// best-effort and never fails the caller.
    async fn get_json(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self.auth(self.http.get(url)).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<serde_json::Value>().await.ok()
    }
}

// --- /v1/models response types (local to this module) ---

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Pull a context-window value from a JSON object, trying the field names the
/// various OpenAI-compatible servers use. Accepts a number or a numeric string;
/// ignores non-positive values.
fn context_field(v: &serde_json::Value) -> Option<u32> {
    const KEYS: &[&str] = &[
        "max_model_len",      // vLLM
        "max_context_length", // LM Studio et al.
        "context_length",     // Ollama-style model_info
        "context_window",     // generic
        "n_ctx",              // llama.cpp
        "context_size",
        "max_context",
    ];
    KEYS.iter()
        .find_map(|k| v.get(k).and_then(json_u32).filter(|n| *n > 0))
}

/// Read a `u32` from a JSON number or numeric string.
fn json_u32(v: &serde_json::Value) -> Option<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn max_tokens_routes_to_completion_field_for_reasoning_models() {
        assert!(uses_max_completion_tokens("o3-mini"));
        assert!(uses_max_completion_tokens("openai/gpt-5"));
        assert!(uses_max_completion_tokens("o1"));
        assert!(!uses_max_completion_tokens("gpt-4o"));
        assert!(!uses_max_completion_tokens("claude-opus-4-8"));

        // A reasoning model routes the cap to `max_completion_tokens`.
        let mut c = Client::new("https://api.openai.com/v1", None, "o3-mini");
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(vec![], vec![], false);
        assert_eq!(r.max_tokens, None);
        assert_eq!(r.max_completion_tokens, Some(1000));

        // A normal model uses `max_tokens`.
        let mut c = Client::new("https://api.openai.com/v1", None, "gpt-4o");
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(vec![], vec![], false);
        assert_eq!(r.max_tokens, Some(1000));
        assert_eq!(r.max_completion_tokens, None);
    }

    #[test]
    fn backend_detected_from_host() {
        assert_eq!(
            detect_backend("https://api.anthropic.com/v1"),
            Backend::Anthropic
        );
        assert_eq!(detect_backend("https://api.openai.com/v1"), Backend::OpenAi);
        assert_eq!(detect_backend("http://localhost:8080/v1"), Backend::OpenAi);
    }

    #[test]
    fn url_appends_azure_api_version_when_set() {
        let mut c = Client::new(
            "https://r.openai.azure.com/openai/deployments/gpt4o",
            None,
            "gpt4o",
        );
        // Standard endpoint: plain path.
        assert_eq!(
            Client::new("https://api.openai.com/v1", None, "m").url("chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        // Azure: api-version query appended.
        c.set_api_version(Some("2024-10-21".to_string()));
        assert_eq!(
            c.url("chat/completions"),
            "https://r.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn reads_common_context_fields() {
        // vLLM
        assert_eq!(context_field(&json!({"max_model_len": 32768})), Some(32768));
        // LM Studio
        assert_eq!(
            context_field(&json!({"max_context_length": 8192})),
            Some(8192)
        );
        // llama.cpp /props
        assert_eq!(context_field(&json!({"n_ctx": 4096})), Some(4096));
        // numeric string
        assert_eq!(
            context_field(&json!({"context_window": "16384"})),
            Some(16384)
        );
        // nothing recognizable (e.g. OpenAI/infr)
        assert_eq!(context_field(&json!({"id": "m", "object": "model"})), None);
        // non-positive is ignored
        assert_eq!(context_field(&json!({"n_ctx": 0})), None);
    }

    #[test]
    fn json_u32_parses_numeric_string() {
        assert_eq!(json_u32(&json!("1234")), Some(1234u32));
        assert_eq!(json_u32(&json!("0")), Some(0u32));
    }

    #[test]
    fn json_u32_negative_string_is_none() {
        // A negative numeric string must not parse to a valid u32.
        assert_eq!(json_u32(&json!("-1")), None);
    }

    #[test]
    fn json_u32_u64_overflow_is_none() {
        // A JSON number > u32::MAX cannot be represented; must return None.
        let big = serde_json::Value::Number(serde_json::Number::from(u64::from(u32::MAX) + 1));
        assert_eq!(json_u32(&big), None);
    }

    #[test]
    fn context_field_string_zero_is_filtered() {
        // "0" parses as u32 0 but is filtered out by the `> 0` guard.
        assert_eq!(context_field(&json!({"n_ctx": "0"})), None);
    }

    #[test]
    fn context_field_empty_object_is_none() {
        assert_eq!(context_field(&json!({})), None);
    }
}
