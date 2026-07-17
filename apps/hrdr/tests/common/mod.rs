//! A self-contained, in-process mock of an OpenAI-compatible endpoint, used by
//! the process-level integration tests to drive the *real* `hrdr` binary
//! through a whole model turn without touching the network.
//!
//! Pure std: a blocking `TcpListener` on a background thread speaking
//! hand-rolled HTTP/1.1 + SSE. No new dependencies (this mirrors the tokio
//! `MockServer` the unit tests use in `hrdr-agent`, rebuilt on `std` so an
//! integration test can own it end to end).
//!
//! Routing, not a strict per-connection queue, because the binary makes probe
//! requests we don't control the timing of:
//!
//! * `GET …/models` — a canned model list. The startup health probe and
//!   `context_window` detection hit this; answering it keeps them off the chat
//!   queue.
//! * `POST …/chat/completions` — the next scripted [`Chat`] response, popped in
//!   order. One per model call, so a tool-round turn scripts two.
//! * anything else — `200 OK`, empty.

#![allow(dead_code)] // Not every test uses every helper.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// What the server does for one `POST …/chat/completions` request.
pub enum Chat {
    /// Stream these payloads as SSE `data:` events (each already a JSON string,
    /// or the `[DONE]` sentinel). This is a normal, successful turn.
    Sse(Vec<String>),
    /// Accept and read the request, then drop the connection without writing a
    /// valid HTTP response — a mid-stream network failure (connection reset).
    Drop,
    /// Reply with a bare HTTP error status and no body (e.g. 400, 500).
    Status(u16),
    /// Open the SSE stream (200 + these initial `data:` lines), then hold the
    /// connection open without finishing — the turn stays "running" so a caller
    /// can cancel it (Esc) mid-flight. The socket is closed after a long sleep.
    Hang(Vec<String>),
}

/// A running mock endpoint. Dropping it stops the listener thread.
pub struct MockServer {
    port: u16,
    stop: Arc<Mutex<bool>>,
}

impl MockServer {
    /// Bind an ephemeral port and start serving. `chats` are consumed one per
    /// `/chat/completions` request, in order; once exhausted, further chat
    /// requests get a minimal one-line text turn so nothing ever hangs.
    pub fn start(chats: Vec<Chat>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        let queue: Arc<Mutex<VecDeque<Chat>>> = Arc::new(Mutex::new(chats.into_iter().collect()));
        let stop = Arc::new(Mutex::new(false));
        let stop_thread = Arc::clone(&stop);
        thread::spawn(move || {
            for conn in listener.incoming() {
                if *stop_thread.lock().unwrap() {
                    break;
                }
                let Ok(stream) = conn else { break };
                let queue = Arc::clone(&queue);
                thread::spawn(move || handle(stream, &queue));
            }
        });
        MockServer { port, stop }
    }

    /// The base URL to configure a provider with (`…/v1`).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
    }
}

/// Serve one connection: read the request head + body, then route on the path.
fn handle(mut stream: TcpStream, queue: &Mutex<VecDeque<Chat>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until the end of the headers.
    let headers_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&buf[..headers_end]).to_string();
    let request_line = head.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("");

    // Drain the request body (Content-Length bytes) so the client's write
    // finishes cleanly before we reply.
    let content_len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    let have = buf.len().saturating_sub(headers_end);
    let mut remaining = content_len.saturating_sub(have);
    while remaining > 0 {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => remaining = remaining.saturating_sub(n),
        }
    }

    if path.contains("/chat/completions") {
        let next = queue.lock().unwrap().pop_front();
        match next {
            Some(Chat::Sse(lines)) => write_sse(&mut stream, &lines),
            Some(Chat::Drop) => { /* write nothing: connection resets */ }
            Some(Chat::Status(code)) => write_status(&mut stream, code),
            Some(Chat::Hang(lines)) => {
                // Open the stream and flush the initial chunks, then hold the
                // connection so the turn stays in-flight to be cancelled.
                let mut body = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for line in &lines {
                    body.push_str("data: ");
                    body.push_str(line);
                    body.push_str("\n\n");
                }
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                thread::sleep(std::time::Duration::from_secs(30));
            }
            // Queue exhausted: a trivial, valid turn so nothing hangs.
            None => write_sse(
                &mut stream,
                &[text_chunk("x", ""), stop_chunk("x"), "[DONE]".to_string()],
            ),
        }
    } else if path.contains("/models") {
        // A model list the startup health probe accepts. Includes the id the
        // tests run on so the probe raises no "model not found" warning.
        let body = r#"{"object":"list","data":[{"id":"mock-model","object":"model"},{"id":"other","object":"model"}]}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
    let _ = stream.flush();
}

fn write_sse(stream: &mut TcpStream, lines: &[String]) {
    let mut body = String::new();
    for line in lines {
        body.push_str("data: ");
        body.push_str(line);
        body.push_str("\n\n");
    }
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
    );
}

fn write_status(stream: &mut TcpStream, code: u16) {
    let _ = write!(
        stream,
        "HTTP/1.1 {code} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

// ── SSE chunk builders (OpenAI ChatCompletionChunk shape) ────────────────────

/// An assistant text delta chunk.
pub fn text_chunk(id: &str, text: &str) -> String {
    serde_json::json!({
        "id": id,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    })
    .to_string()
}

/// A `finish_reason: "stop"` chunk (the model answered without tools).
pub fn stop_chunk(id: &str) -> String {
    serde_json::json!({
        "id": id,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    })
    .to_string()
}

/// The opening chunk of a tool call: names the tool and opens the slot.
pub fn tool_start_chunk(id: &str, call_id: &str, name: &str) -> String {
    serde_json::json!({
        "id": id,
        "choices": [{"index": 0, "delta": {
            "role": "assistant",
            "content": null,
            "tool_calls": [{"index": 0, "id": call_id, "type": "function",
                            "function": {"name": name, "arguments": ""}}]
        }, "finish_reason": null}]
    })
    .to_string()
}

/// A tool-call arguments delta (`arguments` is a JSON-encoded string).
pub fn tool_args_chunk(id: &str, args_json: &str) -> String {
    serde_json::json!({
        "id": id,
        "choices": [{"index": 0, "delta": {
            "tool_calls": [{"index": 0, "function": {"arguments": args_json}}]
        }, "finish_reason": null}]
    })
    .to_string()
}

/// The `finish_reason: "tool_calls"` chunk closing a tool-call round.
pub fn tool_calls_stop_chunk(id: &str) -> String {
    serde_json::json!({
        "id": id,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    })
    .to_string()
}

// ── config.toml wiring ───────────────────────────────────────────────────────

/// Write a `config.toml` under `config_home/hrdr/` that points the `mock`
/// provider at `base_url` and pins `mock-model` as the launch identity.
///
/// `context_window` is pinned so startup does not need to probe for it (the
/// probe would otherwise add a request and a 3s worst-case wait); the model id
/// is deliberately not the `default` placeholder, so the placeholder-model
/// network check is skipped too.
pub fn write_config(config_home: &std::path::Path, base_url: &str) {
    let dir = config_home.join("hrdr");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "model = \"mock://mock-model\"\n\n\
             [providers.mock]\n\
             base_url = \"{base_url}\"\n\
             context_window = 200000\n"
        ),
    )
    .expect("write config.toml");
}
