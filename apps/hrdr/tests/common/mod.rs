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

use portable_pty::{PtySize, native_pty_system};

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
    /// Reply with an HTTP error status **and** a JSON error body. The body is
    /// what the agent classifies on — a rejected parameter and a context
    /// overflow are both 400s, and only the body tells them apart — so a test
    /// about either needs this rather than [`Chat::Status`].
    StatusBody(u16, String),
    /// Open the SSE stream (200 + these initial `data:` lines), then hold the
    /// connection open without finishing — the turn stays "running" so a caller
    /// can cancel it (Esc) mid-flight. The socket is closed after a long sleep.
    Hang(Vec<String>),
}

/// A running mock endpoint. Dropping it stops the listener thread.
pub struct MockServer {
    port: u16,
    stop: Arc<Mutex<bool>>,
    /// Every `/chat/completions` request body, in the order they arrived — so a
    /// test can assert what actually went on the wire, not merely what came
    /// back. Chat requests only: the `/models` probe would otherwise interleave
    /// with them unpredictably.
    chat_bodies: Arc<Mutex<Vec<String>>>,
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
        let chat_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_thread = Arc::clone(&chat_bodies);
        thread::spawn(move || {
            for conn in listener.incoming() {
                if *stop_thread.lock().unwrap() {
                    break;
                }
                let Ok(stream) = conn else { break };
                let queue = Arc::clone(&queue);
                let bodies = Arc::clone(&bodies_thread);
                thread::spawn(move || handle(stream, &queue, &bodies));
            }
        });
        MockServer {
            port,
            stop,
            chat_bodies,
        }
    }

    /// The base URL to configure a provider with (`…/v1`).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The `/chat/completions` request bodies received so far, parsed as JSON.
    /// Panics on a body that is not JSON — the binary only ever sends JSON here,
    /// so that is a bug worth failing on rather than skipping past.
    pub fn chat_bodies(&self) -> Vec<serde_json::Value> {
        self.chat_bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| serde_json::from_str(body).expect("chat request body is JSON"))
            .collect()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
    }
}

/// Serve one connection: read the request head + body, then route on the path.
fn handle(mut stream: TcpStream, queue: &Mutex<VecDeque<Chat>>, chat_bodies: &Mutex<Vec<String>>) {
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
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                remaining = remaining.saturating_sub(n);
            }
        }
    }

    if path.contains("/chat/completions") {
        let end = (headers_end + content_len).min(buf.len());
        chat_bodies
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&buf[headers_end..end]).into_owned());
        let next = queue.lock().unwrap().pop_front();
        match next {
            Some(Chat::Sse(lines)) => write_sse(&mut stream, &lines),
            Some(Chat::Drop) => { /* write nothing: connection resets */ }
            Some(Chat::Status(code)) => write_status(&mut stream, code),
            Some(Chat::StatusBody(code, body)) => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
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
    write_config_with(config_home, base_url, "");
}

/// [`write_config`] with `extra` top-level keys spliced in ahead of the provider
/// table — for a test that needs hrdr to actually *send* an optional parameter
/// (`temperature`, `max_tokens`, …), which it only does when one is configured.
pub fn write_config_with(config_home: &std::path::Path, base_url: &str, extra: &str) {
    let dir = config_home.join("hrdr");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "model = \"mock://mock-model\"\n\
             {extra}\n\
             [providers.mock]\n\
             base_url = \"{base_url}\"\n\
             context_window = 200000\n"
        ),
    )
    .expect("write config.toml");
}

// ── pty plumbing ─────────────────────────────────────────────────────────────

/// Drain a pty master into a shared buffer, answering the terminal handshake.
///
/// **Two Windows traps live here, and both cost a red CI run before this was
/// shared rather than re-written per test file.**
///
/// 1. A ConPTY opens by asking the terminal where the cursor is (`ESC[6n`) and
///    *waits for the reply* before flushing anything the child wrote. A real
///    terminal answers; a harness has to as well. Without it Windows produces
///    exactly four bytes — the query — and hangs, so every assertion fails on a
///    timeout with an empty screen and nothing to say why.
/// 2. A ConPTY master returns `WouldBlock`/`Interrupted` before the child has
///    written anything. A loop that treats the first `Err` as the end (a plain
///    `while let Ok(n) = read(..)`) reads zero bytes forever.
///
/// `writer` is shared so a test can type into the same pty afterwards.
pub fn drain_pty(
    mut reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
) -> Arc<Mutex<String>> {
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&seen);
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                // EOF: the child closed the pty. Nothing more is coming.
                Ok(0) => break,
                Ok(n) => {
                    if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = w.write_all(b"\x1b[1;1R");
                        let _ = w.flush();
                    }
                    let mut s = sink.lock().unwrap_or_else(|e| e.into_inner());
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    seen
}

/// Read the shared buffer, ignoring poisoning. A test that panics mid-assertion
/// should report *its* failure, not have a poisoned mutex bury it.
pub fn pty_text(seen: &Arc<Mutex<String>>) -> String {
    seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Check whether a pty can be allocated. Returns `false` when the Landlock
/// sandbox inherited from the parent hrdr process blocks `/dev/ptmx`.
pub fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

/// Whether to skip a pty test for want of a pty — **never in CI**.
///
/// Locally this is the common case: hrdr running these tests under its own
/// Landlock sandbox cannot open `/dev/ptmx`, and reporting that as a failure
/// says nothing about the code. On a runner there is no such sandbox, so a
/// missing pty is a broken environment and the only useful thing a test can do
/// is fail. A skip that cannot tell those apart converts an infrastructure
/// failure into a green tick, which is the one outcome worse than either.
pub fn skip_for_want_of_a_pty() -> bool {
    if pty_available() || std::env::var_os("CI").is_some() {
        return false;
    }
    eprintln!("skipping: no pty available (a Landlock sandbox blocks /dev/ptmx)");
    true
}

/// Strip ANSI escape sequences, so assertions read the *text* on the screen
/// rather than the control codes that positioned it.
pub fn visible(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ST`) are the two hrdr emits.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\x07' || c == '\x1b' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
