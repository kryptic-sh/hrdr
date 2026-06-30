//! HTTP client over `/v1/chat/completions` and `/v1/models`.

use std::pin::Pin;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::types::{ChatChunk, ChatMessage, ChatRequest, ChatResponse, ToolDef};

/// Boxed stream of decoded streaming chunks.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// A configured chat-completions client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Default model id; overridable per request via [`Client::with_model`].
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

    /// Non-streaming completion. Returns the full response.
    pub async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    ) -> Result<ChatResponse> {
        let body = self.request(messages, tools, false);
        let resp = self
            .post(&body)
            .send()
            .await
            .context("chat request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading chat response body")?;
        if !status.is_success() {
            bail!("chat endpoint returned {status}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("decoding chat response: {text}"))
    }

    /// Streaming completion. Yields decoded chunks as they arrive.
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
    ) -> Result<ChatStream> {
        let body = self.request(messages, tools, true);
        let resp = self
            .post(&body)
            .send()
            .await
            .context("chat stream request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("chat endpoint returned {status}: {text}");
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut buf = String::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.context("reading stream chunk")?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // SSE frames are separated by a blank line; events are `data: ...`.
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim_end_matches('\r').to_string();
                    buf.drain(..=nl);
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
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
