//! HTTP client over `/v1/chat/completions` and `/v1/models`.

use std::io::Write;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::types::{ChatChunk, ChatMessage, ChatRequest, ToolDef};

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

/// A configured chat-completions client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Model id sent with each request (a public field; set it directly).
    pub model: String,
    pub temperature: Option<f32>,
}

impl Client {
    /// `base_url` should include the `/v1` suffix, e.g. `http://localhost:8080/v1`.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            model: model.into(),
            temperature: None,
        }
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Repoint the client at a different endpoint (for mid-session provider switch).
    pub fn set_base_url(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
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
        ChatRequest {
            model: self.model.clone(),
            messages,
            tools,
            temperature: self.temperature,
            stream,
            // Ask for token usage on streamed turns (for the live loader stats).
            stream_options: stream.then_some(crate::types::StreamOptions {
                include_usage: true,
            }),
        }
    }

    fn post(&self, body: &ChatRequest) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    /// Streaming completion. Yields decoded chunks as they arrive.
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    ) -> Result<ChatStream> {
        let body = self.request(messages, tools, true);
        log_wire(
            "request",
            serde_json::json!({
                "url": format!("{}/chat/completions", self.base_url),
                "body": serde_json::to_value(&body).unwrap_or_default(),
            }),
        );
        let resp = self
            .post(&body)
            .send()
            .await
            .context("chat stream request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            log_wire(
                "error_response",
                serde_json::json!({"status": status.as_u16(), "body": text}),
            );
            bail!("chat endpoint returned {status}: {text}");
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
        };
        Ok(Box::pin(stream))
    }

    /// List available models from `GET {base_url}/models`.
    /// Returns model ids sorted alphabetically.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let mut req = self.http.get(format!("{}/models", self.base_url));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
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
        let v = self.get_json(&format!("{}/models", self.base_url)).await?;
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

    /// GET `url` with the bearer key (if any) and decode JSON; `None` on any error
    /// (unreachable endpoint, non-2xx, or unparseable body) — detection is
    /// best-effort and never fails the caller.
    async fn get_json(&self, url: &str) -> Option<serde_json::Value> {
        let mut req = self.http.get(url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.ok()?;
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
