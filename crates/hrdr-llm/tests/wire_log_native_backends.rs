//! `HRDR_LOG_REQUESTS` covers the native backends too: a request that never gets
//! a reply is still in the log.
//!
//! The Anthropic and Codex paths used to be logged by `Client::chat_stream`
//! *after* their `chat_stream` returned `Ok` — but the send and the status check
//! happen inside that call, so a 401 (or a refused connection) propagated and the
//! turn left no trace at all: the exact failure the wire log exists to diagnose
//! was invisible on 2 of 3 backends. Both now log the body before the send.
//!
//! Its own integration binary because it sets a process-global env var, which
//! would race the unit tests in the library's test binary. The env var is
//! process-global and the wire log latches on the first `log_wire` call, so
//! every test here shares ONE leaked log path (see `WIRE_LOG` below) and scopes
//! its assertions by markers unique to each test — a record a sibling test wrote
//! never matches, and the latch never sees a second `set_var` to race.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use std::path::PathBuf;
use std::sync::OnceLock;

use futures_util::StreamExt;
use hrdr_llm::{Backend, ChatMessage, Client, serve_response};

/// The one wire-log path every test in this binary shares.
///
/// `HRDR_LOG_REQUESTS` is process-global and the wire log's `OnceLock` latches
/// on whichever test's `set_var` wins the race — so each test cannot point it at
/// its own file. Every test writes to this single leaked path instead, and
/// scopes its assertions by markers that no sibling test's records contain.
static WIRE_LOG: OnceLock<PathBuf> = OnceLock::new();

fn wire_log_path() -> &'static PathBuf {
    WIRE_LOG.get_or_init(|| {
        // Defer the tempdir's removal to process exit so the directory outlives
        // the one-time init: a `TempDir` dropped here would delete the file the
        // wire log keeps appending to.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.log");
        hrdr_test_support::defer_tempdir(dir);
        // SAFETY: this binary runs only its own tests, and the `OnceLock` above
        // guarantees this init runs exactly once, so nothing else sets the
        // variable while it is read and no second `set_var` ever happens.
        unsafe { std::env::set_var("HRDR_LOG_REQUESTS", &path) };
        path
    })
}

/// Every line of the shared wire log, parsed as JSON (one object per line).
fn log_lines() -> Vec<serde_json::Value> {
    std::fs::read_to_string(wire_log_path())
        .expect("the wire log was created")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
        .collect()
}

/// Both native backends write a `request` record for a send that fails, and
/// neither writes the credential.
///
/// The failure is manufactured without any network: the port is out of range, so
/// `reqwest` rejects the URL when the request is built and `send()` returns the
/// stored parse error — no DNS lookup, no socket, no reachable-host assumption in
/// CI. Backend detection keys on the *host*, which is still `api.anthropic.com` /
/// `chatgpt.com`, so the native paths are the ones exercised.
///
/// Records are filtered by the `"marker-prompt"` body marker so the sibling
/// tests writing to the same shared log cannot match.
#[tokio::test]
async fn a_failed_send_is_logged_for_anthropic_and_codex() {
    wire_log_path(); // the shared path exists before any send
    const KEY: &str = "sk-must-never-be-logged";
    const MARKER: &str = "marker-prompt";
    let messages = [ChatMessage::user(MARKER)];

    for base_url in [
        "http://api.anthropic.com:99999/v1",
        "http://chatgpt.com:99999/backend-api/codex",
    ] {
        let client = Client::new(base_url, Some(KEY.to_string()), "claude-sonnet-4-5");
        assert!(
            client.chat_stream(&messages, &[]).await.is_err(),
            "{base_url} must fail to send (that is the point of the test)"
        );
    }

    let lines = log_lines();

    let requests: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|r| r["kind"] == "request" && r["body"].to_string().contains(MARKER))
        .collect();
    assert_eq!(
        requests.len(),
        2,
        "one `request` record per failed send, Anthropic and Codex: {lines:#?}"
    );
    assert_eq!(
        requests[0]["url"],
        "http://api.anthropic.com:99999/v1/messages"
    );
    assert_eq!(
        requests[1]["url"],
        "http://chatgpt.com:99999/backend-api/codex/responses"
    );
    // The body is the whole reason to log: it must be the real one, not a stub.
    for req in &requests {
        assert!(
            req["body"].to_string().contains(MARKER),
            "the logged body must carry the conversation: {req:#?}"
        );
    }

    // The credential is a header on both backends (`x-api-key` / `Bearer`) and
    // never reaches the body builders. Assert it on the file, not the parsed
    // records, so a future field that leaks one fails here too.
    let raw = std::fs::read_to_string(wire_log_path()).unwrap();
    assert!(
        !raw.contains(KEY),
        "the wire log must never contain a credential"
    );
}

/// The native backends write one `sse` record per raw `data:` line while
/// streaming — the payload before it is parsed, so a line the decoder chokes on
/// is the one worth having in the log.
///
/// Served through a `127.0.0.1` mock with the backend pinned via
/// `set_backend_for_test` (host-keyed detection would call a localhost mock
/// [`Backend::OpenAi`]); the stream is drained fully so every lazy `log_wire`
/// call inside it lands before the log is read.
#[tokio::test]
async fn native_backends_log_each_raw_sse_line() {
    wire_log_path(); // the shared path exists before any send
    for (backend, prompt_marker, key, marker, body) in [
        (
            Backend::Anthropic,
            "ant-sse-prompt",
            "ant-sse-key",
            "ant-sse-marker",
            "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo-ant-sse-marker\"}}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n",
        ),
        (
            Backend::Codex,
            "cod-sse-prompt",
            "cod-sse-key",
            "cod-sse-marker",
            "event: response.output_text.delta\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n\
             event: response.output_text.delta\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo-cod-sse-marker\"}\n\n\
             event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":5}}}\n\n",
        ),
    ] {
        let base_url = serve_response("HTTP/1.1 200 OK", body).await;
        let mut client = Client::new(base_url, Some(key.to_string()), "test-model");
        client.set_backend_for_test(backend);
        let messages = [ChatMessage::user(prompt_marker)];

        let mut stream = client
            .chat_stream(&messages, &[])
            .await
            .expect("the mock server answers 200");
        while let Some(item) = stream.next().await {
            item.expect("clean stream");
        }

        let lines = log_lines();
        let sse: Vec<&serde_json::Value> = lines.iter().filter(|r| r["kind"] == "sse").collect();

        // Every raw `data:` payload from the served body must be in the log.
        for payload in body.lines().filter_map(|l| l.strip_prefix("data: ")) {
            assert!(
                sse.iter().any(|r| r["data"] == payload),
                "{backend:?}: no `sse` record for payload {payload}: {lines:#?}"
            );
        }

        let marker_hits: Vec<&&serde_json::Value> = sse
            .iter()
            .filter(|r| r["data"].to_string().contains(marker))
            .collect();
        assert_eq!(
            marker_hits.len(),
            1,
            "{backend:?}: the marker payload must appear in exactly one `sse` record: {lines:#?}"
        );

        let requests: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|r| r["kind"] == "request" && r["body"].to_string().contains(prompt_marker))
            .collect();
        assert_eq!(
            requests.len(),
            1,
            "{backend:?}: one `request` record must carry the prompt: {lines:#?}"
        );

        let raw = std::fs::read_to_string(wire_log_path()).unwrap();
        assert!(
            !raw.contains(key),
            "{backend:?}: the wire log must never contain a credential"
        );
    }
}

/// The native backends write an `error_response` record for a non-2xx reply,
/// via the shared `client::error_from_response` — status and body, so the
/// provider's actual rejection reason is in the log.
///
/// Served through a `127.0.0.1` mock answering `401 Unauthorized` with a body
/// literal carrying a per-backend marker.
#[tokio::test]
async fn native_backends_log_error_responses() {
    wire_log_path(); // the shared path exists before any send
    for (backend, prompt_marker, key, marker, error_body) in [
        (
            Backend::Anthropic,
            "ant-401-prompt",
            "ant-401-key",
            "ant-401-marker",
            r#"{"error":{"message":"ant-401-marker"}}"#,
        ),
        (
            Backend::Codex,
            "cod-401-prompt",
            "cod-401-key",
            "cod-401-marker",
            r#"{"error":{"message":"cod-401-marker"}}"#,
        ),
    ] {
        let base_url = serve_response("HTTP/1.1 401 Unauthorized", error_body).await;
        let mut client = Client::new(base_url, Some(key.to_string()), "test-model");
        client.set_backend_for_test(backend);
        let messages = [ChatMessage::user(prompt_marker)];
        assert!(
            client.chat_stream(&messages, &[]).await.is_err(),
            "{backend:?}: a 401 must fail the request"
        );

        let lines = log_lines();

        let error_responses: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|r| {
                r["kind"] == "error_response"
                    && r["status"].as_u64() == Some(401)
                    && r["body"].to_string().contains(marker)
            })
            .collect();
        assert_eq!(
            error_responses.len(),
            1,
            "{backend:?}: one `error_response` record must carry status 401 and the body: {lines:#?}"
        );
        assert_eq!(
            error_responses[0]["body"], error_body,
            "{backend:?}: the logged error body must be the served one, verbatim"
        );

        let requests: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|r| r["kind"] == "request" && r["body"].to_string().contains(prompt_marker))
            .collect();
        assert_eq!(
            requests.len(),
            1,
            "{backend:?}: one `request` record must carry the prompt: {lines:#?}"
        );

        let raw = std::fs::read_to_string(wire_log_path()).unwrap();
        assert!(
            !raw.contains(key),
            "{backend:?}: the wire log must never contain a credential"
        );
    }
}
