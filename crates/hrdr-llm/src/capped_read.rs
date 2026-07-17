//! Bounded HTTP response-body readers for diagnostic text and structured JSON.
//!
//! These helpers replace unbounded `resp.text().await` / `resp.bytes().await`
//! calls with finite limits, preventing a hostile or misconfigured server from
//! causing unbounded memory allocation in error-diagnostic and metadata paths.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;

/// Maximum bytes for a diagnostic/error response body (**8 KiB**).
///
/// Enough to capture typical API error messages (OpenAI, Anthropic, OpenRouter,
/// etc.) while avoiding unbounded allocation for error display and wire logs.
pub const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

/// Maximum bytes for a structured JSON response body (**1 MiB**).
///
/// Used for model listings and other provider metadata endpoints that may
/// return many entries. Rejects oversized responses rather than growing
/// unbounded.
pub const MAX_STRUCTURED_JSON_BYTES: usize = 1024 * 1024;

/// Truncation marker appended to diagnostic text when the body exceeds
/// the caller's limit.
pub const TRUNCATION_MARKER: &str = "\n… [truncated]";

/// Maximum size for the `HRDR_LOG_REQUESTS` log file in bytes (**10 MiB**).
///
/// Beyond this, new log entries are silently dropped to cap disk growth.
pub const MAX_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Read up to `max_bytes` from a [`reqwest::Response`] body and return as
/// [`String`]. Appends [`TRUNCATION_MARKER`] when the body exceeds the limit.
///
/// Intended for diagnostic/error text — the caller should supply a
/// generous-but-finite bound such as [`MAX_DIAGNOSTIC_BYTES`].
///
/// Non-UTF-8 sequences are replaced with `U+FFFD` (lossy conversion), which is
/// acceptable for diagnostic display.
pub async fn read_capped_text(resp: reqwest::Response, max_bytes: usize) -> String {
    let cap = max_bytes;
    let mut buf = Vec::with_capacity(cap.min(4096));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => break,
        };
        let remaining = cap.saturating_sub(buf.len());
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
        if buf.len() >= cap {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    if buf.len() >= cap {
        format!("{text}{TRUNCATION_MARKER}")
    } else {
        text
    }
}

/// Read up to `max_bytes` from a [`reqwest::Response`] body and deserialize
/// as JSON. Returns an error if the body exceeds `max_bytes`, ensuring
/// oversized payloads are rejected rather than buffered into memory.
///
/// Intended for structured endpoints such as `/v1/models` or `/v1/props`.
/// For diagnostic display use [`read_capped_text`] instead.
pub async fn read_capped_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<T> {
    let cap = max_bytes;
    let mut buf = Vec::with_capacity(cap.min(4096));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let remaining = cap.saturating_sub(buf.len());
        if chunk.len() > remaining {
            bail!(
                "response body exceeds {} byte limit (already buffered {}, chunk was {})",
                cap,
                buf.len(),
                chunk.len(),
            );
        }
        buf.extend_from_slice(&chunk);
        if buf.len() > cap {
            // The `remaining` check above prevents writing past cap, but a
            // zero-length chunk at exactly `cap` could land here.
            bail!(
                "response body exceeds {} byte limit (buffered {})",
                cap,
                buf.len(),
            );
        }
    }
    serde_json::from_slice(&buf)
        .with_context(|| format!("decoding JSON body ({} bytes)", buf.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helper: in-process HTTP server that returns a fixed body ──────────
    //
    // Mirrors `serve_once` in client.rs's test module but parameterised so
    // the body, status, and content-type are controllable.

    async fn serve(body: &'static [u8], status: u16, content_type: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match stream.read(&mut tmp).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status} {}\r\n\
                 Content-Type: {content_type}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                if status == 200 { "OK" } else { "Error" },
                body.len(),
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(body).await;
        });
        format!("http://127.0.0.1:{port}")
    }

    /// Start a server and return the `Response` object so the test can
    /// exercise the capped-reader directly.
    async fn serve_response(body: &'static [u8], status: u16) -> reqwest::Response {
        let base = serve(body, status, "application/json").await;
        reqwest::get(&base).await.unwrap()
    }

    // ── read_capped_text ─────────────────────────────────────────────────

    #[tokio::test]
    async fn capped_text_short_body_passes_through() {
        let body = b"hello world";
        let resp = serve_response(body, 200).await;
        let text = read_capped_text(resp, 1024).await;
        assert_eq!(text, "hello world");
        assert!(!text.contains("truncated"), "no truncation marker");
    }

    #[tokio::test]
    async fn capped_text_at_limit_truncates() {
        // Reading exactly cap bytes stops and appends the truncation marker
        // because the reader cannot distinguish "body was exactly cap" from
        // "body exceeds cap" — the marker is the safe overapproximation.
        let body = vec![b'a'; MAX_DIAGNOSTIC_BYTES];
        let body_leak: &'static [u8] = Box::leak(body.into_boxed_slice());
        let resp = serve_response(body_leak, 200).await;
        let text = read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
        assert_eq!(text.len(), MAX_DIAGNOSTIC_BYTES + TRUNCATION_MARKER.len());
        assert!(text.ends_with(TRUNCATION_MARKER));
    }

    #[tokio::test]
    async fn capped_text_oversized_body_truncates() {
        let body = vec![b'B'; 16 * 1024];
        let body_leak: &'static [u8] = Box::leak(body.into_boxed_slice());
        let resp = serve_response(body_leak, 200).await;
        let text = read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
        assert_eq!(text.len(), MAX_DIAGNOSTIC_BYTES + TRUNCATION_MARKER.len());
        assert!(text.ends_with(TRUNCATION_MARKER));
    }

    #[tokio::test]
    async fn capped_text_empty_body() {
        let resp = serve_response(b"", 200).await;
        let text = read_capped_text(resp, 1024).await;
        assert_eq!(text, "");
    }

    #[tokio::test]
    async fn capped_text_non_utf8_body() {
        // Invalid UTF-8 sequence: 0xFF is never valid in UTF-8.
        let resp = serve_response(b"\xff\xfe\x00\x01", 200).await;
        let text = read_capped_text(resp, 1024).await;
        // Lossy replacement should turn invalid bytes into replacement chars.
        assert!(!text.is_empty(), "non-utf8 should produce lossy output");
        // The text should contain replacement characters (U+FFFD = �)
        assert!(text.contains('\u{FFFD}'), "lossy replacement expected");
    }

    // ── read_capped_json ─────────────────────────────────────────────────

    #[tokio::test]
    async fn capped_json_valid_body() {
        let resp = serve_response(br#"{"key":"value"}"#, 200).await;
        let v: serde_json::Value = read_capped_json(resp, 1024).await.unwrap();
        assert_eq!(v, json!({"key": "value"}));
    }

    #[tokio::test]
    async fn capped_json_oversized_body_rejected() {
        // A body that exceeds the limit must be rejected.
        let body = vec![b'x'; 2 * 1024];
        let body_leak: &'static [u8] = Box::leak(body.into_boxed_slice());
        let resp = serve_response(body_leak, 200).await;
        let err = read_capped_json::<serde_json::Value>(resp, 1024)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("1024 byte limit"),
            "oversize rejection message: {msg}"
        );
    }

    #[tokio::test]
    async fn capped_json_malformed_body() {
        let resp = serve_response(b"not json", 200).await;
        let err = read_capped_json::<serde_json::Value>(resp, 1024)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("decoding JSON"), "malformed JSON error: {msg}");
    }

    // ── read_capped_json on the models listing endpoint pattern ──────────

    #[tokio::test]
    async fn capped_json_models_listing_fits_within_structured_limit() {
        // Build a plausible models payload right at 1 MiB.
        let model = br#"{"id":"gpt-4o","object":"model"}"#;
        let mut payload = br#"{"data":["#.to_vec();
        // Repeat the model entry until we hit ~1 MiB.
        let target = MAX_STRUCTURED_JSON_BYTES - 100; // a bit under
        while payload.len() < target {
            payload.extend_from_slice(model);
            payload.extend_from_slice(b",");
        }
        payload.extend_from_slice(model);
        payload.extend_from_slice(b"]}");
        let body_leak: &'static [u8] = Box::leak(payload.into_boxed_slice());
        let resp = serve_response(body_leak, 200).await;
        let _parsed: serde_json::Value = read_capped_json(resp, MAX_STRUCTURED_JSON_BYTES)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn capped_json_oversized_models_listing_rejected() {
        let model = br#"{"id":"gpt-4o","object":"model"},"#;
        let mut payload = br#"{"data":["#.to_vec();
        while payload.len() < MAX_STRUCTURED_JSON_BYTES {
            payload.extend_from_slice(model);
        }
        payload.extend_from_slice(b"{}]");
        let body_leak: &'static [u8] = Box::leak(payload.into_boxed_slice());
        let resp = serve_response(body_leak, 200).await;
        let err = read_capped_json::<serde_json::Value>(resp, MAX_STRUCTURED_JSON_BYTES)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("byte limit"), "oversize rejection: {msg}");
    }

    // ── Constants ────────────────────────────────────────────────────────

    #[test]
    fn diagnostic_limit_is_8_kib() {
        assert_eq!(MAX_DIAGNOSTIC_BYTES, 8 * 1024);
    }

    #[test]
    fn structured_json_limit_is_1_mib() {
        assert_eq!(MAX_STRUCTURED_JSON_BYTES, 1024 * 1024);
    }

    #[test]
    fn log_file_limit_is_10_mib() {
        assert_eq!(MAX_LOG_FILE_BYTES, 10 * 1024 * 1024);
    }

    #[test]
    fn truncation_marker_is_not_empty() {
        assert!(!TRUNCATION_MARKER.is_empty());
    }
}
