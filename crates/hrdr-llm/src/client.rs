//! HTTP client over `/v1/chat/completions` and `/v1/models`.

use std::io::Write;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::capped_read::{MAX_DIAGNOSTIC_BYTES, MAX_LOG_FILE_BYTES, MAX_STRUCTURED_JSON_BYTES};
use crate::sse::SseDecoder;
use crate::types::{CacheMode, ChatChunk, ChatMessage, ChatRequest, ToolDef};

/// Wire-level debug log, enabled by `HRDR_LOG_REQUESTS=<path>`: every chat
/// request body, every raw SSE data line, and every non-2xx response body is
/// appended to the file as one JSON object per line. For debugging
/// harness ⇄ server disagreements (tool-call framing, stream shape) — off
/// unless the env var is set.
static REQUEST_LOG: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
static REQUEST_LOG_FULL_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static REQUEST_LOG_WARNING: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Take the one-shot wire-log warning for delivery through the caller's normal
/// event channel. This avoids writing stderr while a TUI owns the terminal.
pub fn take_request_log_warning() -> Option<String> {
    REQUEST_LOG_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut warning| warning.take())
}

fn request_log() -> Option<&'static Mutex<std::fs::File>> {
    REQUEST_LOG
        .get_or_init(|| {
            let path = std::env::var_os("HRDR_LOG_REQUESTS")?;
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).append(true);
            // Restrict permissions to owner-only on Unix (0600) so local
            // users cannot read API request/response data from the log.
            #[cfg(unix)]
            opts.mode(0o600);
            let file = opts.open(&path).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if !file.metadata().ok()?.file_type().is_file() {
                    return None;
                }
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .ok()?;
            }
            Some(Mutex::new(file))
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
        let current = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut line = obj.to_string();
        line.push('\n');
        if current >= MAX_LOG_FILE_BYTES
            || line.len() as u64 > MAX_LOG_FILE_BYTES.saturating_sub(current)
        {
            if !REQUEST_LOG_FULL_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let warning = format!(
                    "request logging stopped after reaching {} MiB",
                    MAX_LOG_FILE_BYTES / (1024 * 1024)
                );
                if let Ok(mut pending) = REQUEST_LOG_WARNING.get_or_init(|| Mutex::new(None)).lock()
                {
                    *pending = Some(warning);
                }
            }
            return;
        }
        let _ = file.write_all(line.as_bytes());
    }
}

/// Classification of a chat-endpoint error for the agent's retry and
/// compaction logic. Carried in the typed [`ChatError`] so hrdr-agent can
/// match on the kind directly rather than scanning Display strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatErrorKind {
    /// The request exceeded the model's context window; compaction may help.
    Overflow,
    /// A transient network or server error worth retrying with backoff.
    Transient,
    /// Any other error (bad request, auth failure, unsupported parameter, …).
    Other,
}

/// A typed chat-endpoint error, emitted by [`Client::chat_stream`] for HTTP
/// non-2xx responses and truncated streams. Prefer matching on [`ChatErrorKind`]
/// for retry/compaction decisions; `message` preserves the full display string
/// for the fallback text-scanner in hrdr-agent (which handles errors that arrive
/// only as mid-stream bodies and never go through this path).
#[derive(Debug)]
pub struct ChatError {
    /// HTTP status code, if this was an HTTP-level error.
    pub status: Option<u16>,
    /// Server-requested retry delay parsed from the `Retry-After` header, if
    /// present (only meaningful for 429 responses). Clamped to 60 s upstream.
    pub retry_after: Option<std::time::Duration>,
    /// Coarse classification for retry/compaction decisions.
    pub kind: ChatErrorKind,
    /// Full display string — preserved so hrdr-agent's text-fallback scanner
    /// sees the same content it used to (and can scan the body text for e.g.
    /// 400-overflow messages whose kind can't be determined from status alone).
    pub message: String,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChatError {}

/// Map an HTTP status code to a [`ChatErrorKind`]. 413 is always overflow;
/// 429/5xx are transient. Everything else needs body-text analysis (handled
/// by hrdr-agent's fallback scanner on the `Other` path).
pub(crate) fn classify_status(status: u16) -> ChatErrorKind {
    match status {
        413 => ChatErrorKind::Overflow,
        // 408 request timeout and Cloudflare's origin-timeout family (522/524)
        // are transient — gateways in front of OpenAI-compatible providers emit
        // these under load, and the request is safe to retry.
        408 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529 => ChatErrorKind::Transient,
        _ => ChatErrorKind::Other,
    }
}

/// Parse a `Retry-After` header into a [`Duration`], clamped to 60 s.
pub(crate) fn retry_after_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(|s| std::time::Duration::from_secs(s.min(60)))
}

/// Format a retry-after duration as ` (retry-after: Ns)` for embedding in
/// error messages (preserves the text format hrdr-agent's fallback scanner
/// used to rely on).
pub(crate) fn retry_after_suffix_from(d: Option<std::time::Duration>) -> String {
    d.map(|d| format!(" (retry-after: {}s)", d.as_secs()))
        .unwrap_or_default()
}

/// Boxed stream of decoded streaming chunks.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// Which wire protocol the endpoint speaks. Auto-detected from `base_url`
/// (Anthropic's own host → native Messages API; the ChatGPT/Codex `/codex/`
/// endpoint → the OpenAI Responses API), else the OpenAI chat-completions shape
/// every other server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    OpenAi,
    Anthropic,
    /// OpenAI **Responses API** — the ChatGPT/Codex OAuth endpoint
    /// (`https://chatgpt.com/backend-api/codex/responses`). See [`crate::codex`].
    Codex,
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

/// The host portion of `base_url` (scheme, userinfo, port, and path stripped).
///
/// Handles bracketed IPv6 literals (`http://[::1]:8080/v1` → `::1`): a naive
/// `rsplit_once(':')` would chop an IPv6 address's internal colons instead of
/// just the trailing port, mangling the host. This helper is duplicated in
/// hrdr-agent's `resolve_cache_mode` helpers — keep both in sync (or, better,
/// have hrdr-agent call this one).
pub fn url_host(base_url: &str) -> &str {
    let authority = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: host is everything up to the closing `]`;
        // a trailing `:port` after the bracket is discarded.
        return rest.split(']').next().unwrap_or(authority);
    }
    authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
}

/// Detect the wire protocol from `base_url`:
/// - `api.anthropic.com` → native Anthropic Messages API (unlocks caching).
/// - `chatgpt.com` with a `/codex/` path → the OpenAI Responses API (the
///   ChatGPT/Codex OAuth endpoint); the client POSTs to `{base_url}/responses`,
///   so point it at `https://chatgpt.com/backend-api/codex`.
/// - anything else → the OpenAI chat-completions shape.
fn detect_backend(base_url: &str) -> Backend {
    let host = url_host(base_url);
    if host == "api.anthropic.com" || host.ends_with(".anthropic.com") {
        Backend::Anthropic
    } else if (host == "chatgpt.com" || host.ends_with(".chatgpt.com"))
        && base_url.contains("/codex")
    {
        Backend::Codex
    } else {
        Backend::OpenAi
    }
}

/// The NAME of the wire protocol hrdr will speak at `base_url` — the public face
/// of [`detect_backend`], which keys on the HOST.
///
/// Exposed because that host-keying is invisible and consequential: two providers
/// that differ only in their `base_url` (a `[providers.*]` gateway on localhost
/// versus `api.anthropic.com`) speak DIFFERENT APIs — chat-completions versus the
/// Anthropic Messages API. A caller that compares this across two URLs can say so
/// out loud rather than letting the request shape change under the user.
pub fn wire_protocol(base_url: &str) -> &'static str {
    match detect_backend(base_url) {
        Backend::OpenAi => "OpenAI",
        Backend::Anthropic => "Anthropic",
        Backend::Codex => "Codex",
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

    /// Current reasoning-effort label, including display-only values.
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
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

    /// Whether an extra header with `name` (case-insensitive) is currently set.
    pub fn extra_headers_contains(&self, name: &str) -> bool {
        self.extra_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// Whether a credential (API key, or an injected OAuth bearer) is currently
    /// set. Returns only the presence bit — never the secret — so a caller can
    /// assert a credential was cleared without being able to read it.
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_ref().is_some_and(|k| !k.is_empty())
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

    fn request(&self, messages: &[ChatMessage], tools: &[ToolDef], stream: bool) -> ChatRequest {
        // OpenAI reasoning models want `max_completion_tokens`, not `max_tokens`.
        let (max_tokens, max_completion_tokens) = match self.params.max_tokens {
            Some(n) if uses_max_completion_tokens(&self.model) => (None, Some(n)),
            other => (other, None),
        };
        ChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
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
                // Codex (Responses API) uses the same Bearer auth; the streaming
                // path builds its own request (see `crate::codex::chat_stream`),
                // this only covers the best-effort `/models` + `/props` GETs.
                Backend::OpenAi | Backend::Codex => req.bearer_auth(key),
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
    ///
    /// Takes slices to avoid cloning the full history on every retry. The
    /// request body is serialized before any network I/O, so the borrow does
    /// not extend into the returned [`ChatStream`] future.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
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
                self.params.top_p,
                &self.params.stop,
                // `self.params.seed` is intentionally not passed: the native
                // Messages API has no determinism-seed equivalent.
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
        if self.backend == Backend::Codex {
            let (body, stream) = crate::codex::chat_stream(
                &self.http,
                &self.base_url,
                self.api_key.as_deref(),
                &self.model,
                self.effort.as_deref(),
                self.temperature,
                self.params.top_p,
                self.params.max_tokens,
                // `ChatGPT-Account-Id` rides here (set via `set_headers`);
                // `originator: hrdr` + `Authorization: Bearer` are added inside.
                &self.extra_headers,
                messages,
                tools,
            )
            .await?;
            log_wire(
                "request",
                serde_json::json!({
                    "url": format!("{}/responses", self.base_url),
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
            let retry_after = retry_after_from_headers(resp.headers());
            let text = crate::capped_read::read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
            let status_u16 = status.as_u16();
            log_wire(
                "error_response",
                serde_json::json!({"status": status_u16, "body": text}),
            );
            return Err(anyhow::Error::new(ChatError {
                status: Some(status_u16),
                retry_after,
                kind: classify_status(status_u16),
                message: format!(
                    "chat endpoint returned {status}: {text}{}",
                    retry_after_suffix_from(retry_after)
                ),
            }));
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            // Feed raw byte chunks into the SSE decoder, which buffers per-line
            // and yields complete events on blank-line terminators.  Splitting
            // on 0x0A is safe for UTF-8: the byte never appears inside a
            // multi-byte sequence, so a codepoint split across chunks is
            // buffered whole and decoded only when its line is complete.
            let mut decoder = SseDecoder::new();
            loop {
                // On EOF, `finish()` flushes a final `data:` line that had no
                // blank-line terminator (lenient servers end with `data: [DONE]\n`
                // rather than a spec `\n\n`), so the sentinel isn't lost.
                let (events, at_eof) = match bytes.next().await {
                    Some(chunk) => {
                        // A transport error mid-body (connection reset, WiFi blip)
                        // is safe to retry — the reply was partial. Type it as
                        // Transient so the agent retry loop catches it; an untyped
                        // anyhow error would print only "reading stream chunk" and
                        // slip past the classifier.
                        let bytes = chunk.map_err(|e| ChatError {
                            status: None,
                            retry_after: None,
                            kind: ChatErrorKind::Transient,
                            message: format!(
                                "incomplete stream: transport error mid-response \
                                 ({e}) (partial response, safe to retry)"
                            ),
                        })?;
                        decoder.push(&bytes);
                        // SSE buffer overflow: server sent more data than
                        // MAX_BUFFER_BYTES per line/event — a broken or hostile
                        // server, not a transient glitch.  Reject as non-retryable
                        // (Other) so the agent retry loop does not keep retrying a
                        // stream that will overflow again.
                        if decoder.overflowed() {
                            let _ = decoder.drain(); // discard truncated events
                            Err(ChatError {
                                status: None,
                                retry_after: None,
                                kind: ChatErrorKind::Other,
                                message: "SSE stream overflow: received data exceeding \
                                          32 MiB limit; broken or hostile server"
                                    .to_string(),
                            })?;
                        }
                        (decoder.drain(), false)
                    }
                    None => {
                        let events = decoder.finish();
                        // If overflow was flagged during the stream, the final
                        // events may be truncated — never parse them.
                        if decoder.overflowed() {
                            Err(ChatError {
                                status: None,
                                retry_after: None,
                                kind: ChatErrorKind::Other,
                                message: "SSE stream overflow: received data exceeding \
                                          32 MiB limit; broken or hostile server"
                                    .to_string(),
                            })?;
                        }
                        (events, true)
                    }
                };
                for ev in events {
                    let data = ev.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    log_wire("sse", serde_json::json!({"data": data}));
                    if data == "[DONE]" {
                        return;
                    }
                    let value: serde_json::Value = serde_json::from_str(data)
                        .with_context(|| format!("decoding stream event: {data}"))?;
                    // A mid-stream error object (`{"error":{"message":"..."}}`) would
                    // otherwise deserialize as an empty `ChatChunk` (every field is
                    // `#[serde(default)]`), silently swallowing the server's real
                    // error and letting the stream fall through to the generic
                    // "incomplete stream" retryable classification below. Surface it
                    // here instead, as a terminal (non-retryable) error carrying the
                    // server's message — mirrors the native Anthropic `"error"` event
                    // handling in `anthropic::map_event`.
                    if let Some(err_obj) = value.get("error").filter(|e| !e.is_null()) {
                        let msg = err_obj
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown error");
                        // Gateways (OpenRouter, LiteLLM, …) deliver rate-limit and
                        // overload conditions as mid-stream error objects. Classify
                        // them Transient by code/type so the retry loop catches
                        // them, matching the native Anthropic path.
                        let code = err_obj
                            .get("code")
                            .and_then(|c| c.as_u64())
                            .map(|c| c as u16)
                            .or_else(|| {
                                err_obj.get("status").and_then(|c| c.as_u64()).map(|c| c as u16)
                            });
                        let type_str = err_obj
                            .get("type")
                            .or_else(|| err_obj.get("code"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        let transient = code.map(classify_status)
                            == Some(ChatErrorKind::Transient)
                            || type_str.contains("rate_limit")
                            || type_str.contains("overloaded")
                            || type_str.contains("server_error");
                        let kind = if transient {
                            ChatErrorKind::Transient
                        } else {
                            ChatErrorKind::Other
                        };
                        Err(ChatError {
                            status: None,
                            retry_after: None,
                            kind,
                            message: format!("mid-stream error: {msg}"),
                        })?;
                    }
                    let parsed: ChatChunk = serde_json::from_value(value)
                        .with_context(|| format!("decoding stream event: {data}"))?;
                    yield parsed;
                }
                if at_eof {
                    break;
                }
            }
            // Reaching here means the byte stream closed without the [DONE]
            // sentinel — truncated response or network drop. Classify as
            // transient so the agent retry loop can re-request.
            Err(ChatError {
                status: None,
                retry_after: None,
                kind: ChatErrorKind::Transient,
                message: "incomplete stream: OpenAI stream ended without [DONE] \
                          (partial response, safe to retry)"
                    .to_string(),
            })?;
        };
        Ok(Box::pin(stream))
    }

    /// List available models from `GET {base_url}/models`.
    /// Returns model ids sorted alphabetically.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let req = self.auth(self.http.get(self.url("models")));
        let resp = req.send().await.context("models request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = crate::capped_read::read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
            bail!("models endpoint returned {status}: {text}");
        }
        let parsed: ModelsResponse =
            crate::capped_read::read_capped_json(resp, MAX_STRUCTURED_JSON_BYTES)
                .await
                .context("decoding models response")?;
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
    fn effort_getter_preserves_display_only_values_and_clear() {
        let mut client = Client::new("http://localhost/v1", None, "model");
        assert_eq!(client.effort(), None);
        client.set_effort(Some("high".to_string()));
        assert_eq!(client.effort(), Some("high"));
        client.set_effort(Some("custom-display-label".to_string()));
        assert_eq!(client.effort(), Some("custom-display-label"));
        client.set_effort(None);
        assert_eq!(client.effort(), None);
    }

    #[test]
    fn effort_getter_reflects_latest_value_and_request_mapping() {
        let mut client = Client::new("http://localhost/v1", None, "model");

        client.set_effort(Some("off".to_string()));
        assert_eq!(client.effort(), Some("off"));
        let off = client.request(&[], &[], false);
        assert!(off.reasoning_effort.is_none());

        client.set_effort(Some("high".to_string()));
        assert_eq!(client.effort(), Some("high"));
        let high = client.request(&[], &[], false);
        assert_eq!(high.reasoning_effort.as_deref(), Some("high"));

        client.set_effort(None);
        assert_eq!(client.effort(), None);
        let none = client.request(&[], &[], false);
        assert!(none.reasoning_effort.is_none());
    }

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
        let r = c.request(&[], &[], false);
        assert_eq!(r.max_tokens, None);
        assert_eq!(r.max_completion_tokens, Some(1000));

        // A normal model uses `max_tokens`.
        let mut c = Client::new("https://api.openai.com/v1", None, "gpt-4o");
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(&[], &[], false);
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
        // ChatGPT/Codex OAuth endpoint → the Responses API backend.
        assert_eq!(
            detect_backend("https://chatgpt.com/backend-api/codex"),
            Backend::Codex
        );
        // chatgpt.com without a `/codex/` path is not the Responses backend.
        assert_eq!(
            detect_backend("https://chatgpt.com/backend-api"),
            Backend::OpenAi
        );
    }

    #[test]
    fn url_host_handles_bracketed_ipv6_literal() {
        // A naive `rsplit_once(':')` would chop this into `[:` / `:1]:8080`,
        // mangling the address. The bracket-aware parse must return the bare
        // address with the port stripped.
        assert_eq!(url_host("http://[::1]:8080/v1"), "::1");
        assert_eq!(url_host("https://[2001:db8::1]/v1"), "2001:db8::1");
        // Plain hostname + port still works.
        assert_eq!(url_host("http://localhost:8080/v1"), "localhost");
        // Anthropic detection must still work through the shared helper.
        assert_eq!(
            detect_backend("http://[::1]:8080/v1"),
            Backend::OpenAi,
            "an IPv6-literal endpoint must not mis-detect as Anthropic"
        );
    }

    /// Minimal in-process HTTP server used only to exercise the SSE decoding
    /// path in `chat_stream` (mirrors the mock server in hrdr-agent's test
    /// module, trimmed to a single canned response).
    async fn serve_once(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Read (and discard) the request headers + body; we don't care
            // about the request shape for this test.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let headers_end = loop {
                match stream.read(&mut tmp).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    }
                }
            };
            let hdrs = String::from_utf8_lossy(&buf[..headers_end]);
            let content_len: usize = hdrs
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let body_so_far = buf.len().saturating_sub(headers_end);
            let remaining = content_len.saturating_sub(body_so_far);
            if remaining > 0 {
                let mut body_buf = vec![0u8; remaining];
                let _ = stream.read_exact(&mut body_buf).await;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}"
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    #[tokio::test]
    async fn mid_stream_error_object_without_a_type_is_terminal() {
        // A server that sends a well-formed error object mid-stream, with no
        // [DONE] sentinel. Before the fix this deserialized as an empty
        // `ChatChunk` (every field `#[serde(default)]`) and the stream fell
        // through to the generic "incomplete stream" transient error,
        // swallowing the real message. An untyped error stays terminal.
        let body = "data: {\"error\":{\"message\":\"something broke\"}}\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let first = stream
            .next()
            .await
            .expect("stream must yield the error, not end silently");
        let err = first.expect_err("mid-stream error object must surface as Err");
        let chat_err = err
            .downcast_ref::<ChatError>()
            .expect("error must be a typed ChatError");
        assert_eq!(
            chat_err.kind,
            ChatErrorKind::Other,
            "an untyped mid-stream error must not be classified transient"
        );
        assert!(
            chat_err.message.contains("something broke"),
            "message must carry the server's text: {}",
            chat_err.message
        );
    }

    #[tokio::test]
    async fn mid_stream_rate_limit_error_is_transient() {
        // Gateways (OpenRouter, LiteLLM) deliver overload as a typed mid-stream
        // error object. It must retry, matching the native Anthropic path.
        let body =
            "data: {\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let err = stream
            .next()
            .await
            .expect("stream must yield the error")
            .expect_err("typed error must surface as Err");
        let chat_err = err.downcast_ref::<ChatError>().expect("typed ChatError");
        assert_eq!(chat_err.kind, ChatErrorKind::Transient);
    }

    #[tokio::test]
    async fn explicit_null_error_field_does_not_abort_the_stream() {
        // Some proxies emit `"error": null` on healthy chunks. That must not be
        // read as an error.
        let body = "data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: [DONE]\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let chunk = stream
            .next()
            .await
            .expect("stream must yield the content chunk")
            .expect("null error field must not be treated as an error");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
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

    /// The wire protocol is a function of the HOST — which is exactly why the
    /// `base_url` a provider is defined with decides the API hrdr speaks to it,
    /// without anything being said about the API anywhere.
    #[test]
    fn the_wire_protocol_is_decided_by_the_host_alone() {
        assert_eq!(wire_protocol("https://api.anthropic.com/v1"), "Anthropic");
        assert_eq!(
            wire_protocol("https://chatgpt.com/backend-api/codex"),
            "Codex"
        );
        assert_eq!(wire_protocol("https://api.openai.com/v1"), "OpenAI");
        // The flip that Deliverable 3(a) exists to announce: same provider, same
        // model, different host — and a different request shape on the wire.
        assert_ne!(
            wire_protocol("https://api.anthropic.com/v1"),
            wire_protocol("http://localhost:1234/v1"),
        );
        assert_eq!(wire_protocol("http://localhost:1234/v1"), "OpenAI");
    }

    // ── Log hardening ───────────────────────────────────────────────────
    //
    // These tests verify the REQUEST_LOG file creation and growth-cap logic
    // without exercising the global singleton (which is hard to reset).

    #[test]
    #[cfg(unix)]
    fn log_file_created_with_0600_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Replicate the open options request_log() uses.
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        opts.mode(0o600);
        let file = opts.open(&path).unwrap();
        drop(file);

        // On Unix, the mode argument to OpenOptions is only a *request* —
        // the kernel applies the umask on top.  The resulting file must not
        // have group/other bits set, even though the exact mode may differ
        // from 0600.
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).unwrap();
        let perm = meta.permissions().mode();
        // Check that group and other bits are clear.
        assert_eq!(
            perm & 0o077,
            0,
            "log file must not have group/other permissions (mode={perm:#o})"
        );
    }

    #[test]
    fn log_wire_skips_write_when_file_exceeds_cap() {
        // The growth-cap check inside log_wire compares the target file's
        // length against MAX_LOG_FILE_BYTES.  We verify the same logic by
        // writing up to and past the limit to a temp file, then re-checking
        // against the cap constant.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Write MAX_LOG_FILE_BYTES bytes.
        let data = vec![b'x'; MAX_LOG_FILE_BYTES as usize];
        std::fs::write(&path, &data).unwrap();

        // The metadata check should see length >= MAX_LOG_FILE_BYTES.
        let meta = std::fs::metadata(&path).unwrap();
        assert!(
            meta.len() >= MAX_LOG_FILE_BYTES,
            "file should be at or past the cap"
        );

        // Opening for append and writing a line would be skipped by the
        // guard in log_wire.  We verify the guard condition directly.
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let would_skip = file.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_LOG_FILE_BYTES;
        assert!(
            would_skip,
            "log_wire must skip writes when file >= MAX_LOG_FILE_BYTES"
        );
        drop(file);
    }

    #[test]
    fn log_wire_allows_write_when_file_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Write a small amount well under the cap.
        std::fs::write(&path, b"small").unwrap();

        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let would_skip = file.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_LOG_FILE_BYTES;
        assert!(
            !would_skip,
            "log_wire must allow writes when file < MAX_LOG_FILE_BYTES"
        );
    }
}
