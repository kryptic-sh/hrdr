//! End-to-end TUI tests.
//!
//! These drive a real [`App`] against a **mock OpenAI-compatible server** — no
//! network, no live model — through the same seams the event loop uses
//! (`on_key` for input, `on_turn_msg` for streamed agent events), then render to
//! a ratatui [`TestBackend`] and assert on the visible buffer. It's a child
//! module of `app`, so it reaches `App`'s private methods and fields directly.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::{App, Entry, StatusBarMode, TimestampStyle, TurnMsg};
use crate::ui;
use hrdr_agent::AgentConfig;

// ---------------------------------------------------------------------------
// Mock OpenAI-compatible server
// ---------------------------------------------------------------------------

/// A scripted reply the mock server returns for one `chat/completions` call.
#[derive(Clone)]
enum MockReply {
    /// Plain assistant text; ends the turn (`finish_reason: "stop"`).
    Text(String),
    /// A single tool call (`finish_reason: "tool_calls"`). The agent runs the
    /// tool then requests again, consuming the next queued reply.
    ToolCall { name: String, args: String },
    /// Several tool calls in one turn — `(name, json_args)` each — so a turn
    /// with parallel calls can be exercised.
    ToolCalls(Vec<(String, String)>),
    /// Content split across many SSE frames; tests the streaming accumulator path
    /// end-to-end (each string becomes a separate `data:` frame).
    MultiChunk(Vec<String>),
    /// A reasoning delta arrives first, then a content delta. Exercises the
    /// `AgentEvent::Reasoning` → `Entry::Reasoning` path.
    TextWithReasoning { reasoning: String, text: String },
}

/// A tiny in-process HTTP server speaking just enough of the OpenAI API for the
/// client: `GET …/models` and a streamed (SSE) `POST …/chat/completions`.
/// Replies are popped from a queue per chat request (defaulting to a short text
/// once the queue drains). Runs until dropped.
struct MockServer {
    base_url: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(replies: Vec<MockReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1");
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let queue = queue.clone();
                tokio::spawn(async move {
                    let head = read_request_head(&mut sock).await;
                    let path = head
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    let (ctype, payload) = if path.ends_with("/models") {
                        ("application/json", models_body())
                    } else {
                        let reply = queue
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or(MockReply::Text("ok".to_string()));
                        ("text/event-stream", sse_body(&reply))
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{payload}",
                        payload.len(),
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        Self {
            base_url,
            _handle: handle,
        }
    }
}

/// Read an HTTP request's head (up to and including the blank line), then drain
/// its body per `Content-Length` so the client's write completes cleanly before
/// we respond. Returns the header block (the request line is its first line).
async fn read_request_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = find(&data, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..pos]).to_string();
            let body_start = pos + 4;
            let have = data.len() - body_start;
            let mut remaining = content_length(&headers).saturating_sub(have);
            while remaining > 0 {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                }
            }
            return headers;
        }
    }
    String::from_utf8_lossy(&data).to_string()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn models_body() -> String {
    "{\"object\":\"list\",\"data\":[{\"id\":\"test-model\",\"object\":\"model\",\
     \"owned_by\":\"local\"}]}"
        .to_string()
}

/// Build a full SSE body (role delta → payload → finish → usage → `[DONE]`) for
/// one scripted reply. Sent all at once with `Content-Length`; the client parses
/// it line-by-line regardless of chunking.
fn sse_body(reply: &MockReply) -> String {
    let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
    let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\
                 \"completion_tokens\":5}}\n\n";
    let done = "data: [DONE]\n\n";
    let (payload, finish) = match reply {
        MockReply::Text(t) => (
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                esc(t)
            ),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        MockReply::ToolCall { name, args } => (
            tool_calls_frame(&[(name.clone(), args.clone())]),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        MockReply::ToolCalls(calls) => (
            tool_calls_frame(calls),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        MockReply::MultiChunk(chunks) => {
            // Each string becomes its own `data:` SSE frame; proves the streaming
            // accumulator appends them into one `Entry::Assistant`.
            let payload: String = chunks
                .iter()
                .map(|c| {
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                        esc(c)
                    )
                })
                .collect();
            (
                payload,
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            )
        }
        MockReply::TextWithReasoning { reasoning, text } => {
            // First frame carries `reasoning_content`; second carries `content`.
            let payload = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{}\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                esc(reasoning),
                esc(text),
            );
            (
                payload,
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            )
        }
    };
    format!("{role}{payload}{finish}{usage}{done}")
}

/// One SSE delta carrying a `tool_calls` array with `(name, args)` per call.
fn tool_calls_frame(calls: &[(String, String)]) -> String {
    let items: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            format!(
                "{{\"index\":{i},\"id\":\"call_{i}\",\"function\":{{\"name\":\"{}\",\
                 \"arguments\":\"{}\"}}}}",
                esc(name),
                esc(args)
            )
        })
        .collect();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{}]}}}}]}}\n\n",
        items.join(",")
    )
}

/// Minimal JSON string escaping for values embedded in the canned SSE frames.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            _ => o.push(c),
        }
    }
    o
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// Drives an [`App`] against a [`MockServer`] without the crossterm event loop.
struct Harness {
    app: App,
    rx: mpsc::UnboundedReceiver<TurnMsg>,
    _mock: MockServer,
    _tmp: tempfile::TempDir,
}

impl Harness {
    async fn new(replies: Vec<MockReply>) -> Self {
        let mock = MockServer::start(replies).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            base_url: mock.base_url.clone(),
            model: "test-model".to_string(),
            cwd: tmp.path().to_path_buf(),
            checkpoints: Some("off".to_string()),
            context_window: Some(1000),
            ..Default::default()
        };
        let ui = hrdr_app::UiConfig {
            auto_resume: false, // never pick up the developer's real sessions
            ..Default::default()
        };
        let mut app = App::new(config, ui).unwrap();
        let rx = app.rx.take().expect("fresh app has its receiver");
        Self {
            app,
            rx,
            _mock: mock,
            _tmp: tmp,
        }
    }

    fn press(&mut self, code: KeyCode) {
        self.app.on_key(KeyEvent::new(code, KeyModifiers::empty()));
    }

    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Type `msg`, press Enter, then pump agent events until the turn settles.
    async fn submit(&mut self, msg: &str) {
        self.type_str(msg);
        self.press(KeyCode::Enter);
        self.pump().await;
    }

    /// Drain the turn channel until the agent is no longer running.
    async fn pump(&mut self) {
        while self.app.running {
            match self.rx.recv().await {
                Some(msg) => self.app.on_turn_msg(msg),
                None => break,
            }
        }
    }

    /// Render the whole UI to a [`TestBackend`] and flatten it to text.
    fn render(&mut self) -> String {
        let mut term = Terminal::new(TestBackend::new(90, 30)).unwrap();
        term.draw(|f| ui::draw(f, &mut self.app)).unwrap();
        buffer_to_string(term.backend().buffer())
    }
}

fn buffer_to_string(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell(Position::new(x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_message_gets_a_streamed_reply() {
    let mut h = Harness::new(vec![MockReply::Text(
        "Hello from the mock model.".to_string(),
    )])
    .await;
    h.submit("hi there").await;
    let screen = h.render();
    // The user's message and the assistant's streamed reply both render.
    assert!(
        screen.contains("hi there"),
        "user message missing:\n{screen}"
    );
    assert!(
        screen.contains("Hello from the mock model."),
        "assistant reply missing:\n{screen}"
    );
    // The turn finished — not stuck "running".
    assert!(!h.app.running);
}

#[tokio::test]
async fn tool_call_runs_the_tool_then_finishes() {
    // First reply asks to write a todo; the follow-up turn ends with text.
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "todo_write".to_string(),
            args: r#"{"todos":[{"content":"write more tests","status":"in_progress"}]}"#
                .to_string(),
        },
        MockReply::Text("Added the todo.".to_string()),
    ])
    .await;
    h.submit("make a plan").await;
    let screen = h.render();
    // The tool call is surfaced, the todo panel shows the item, and the final
    // assistant text lands — proving the full tool round-trip drove two calls.
    assert!(
        screen.contains("todo_write"),
        "tool call missing:\n{screen}"
    );
    assert!(
        screen.contains("write more tests"),
        "todo item missing:\n{screen}"
    );
    assert!(
        screen.contains("Added the todo."),
        "final reply missing:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn parallel_tool_calls_in_one_turn_all_run() {
    // One turn requests two tools; the follow-up request ends with text.
    let mut h = Harness::new(vec![
        MockReply::ToolCalls(vec![
            (
                "todo_write".to_string(),
                r#"{"todos":[{"content":"first task","status":"in_progress"}]}"#.to_string(),
            ),
            ("glob".to_string(), r#"{"pattern":"*"}"#.to_string()),
        ]),
        MockReply::Text("Both ran.".to_string()),
    ])
    .await;
    h.submit("do two things").await;
    let screen = h.render();
    assert!(
        screen.contains("todo_write"),
        "first tool missing:\n{screen}"
    );
    assert!(screen.contains("glob"), "second tool missing:\n{screen}");
    assert!(
        screen.contains("Both ran."),
        "final reply missing:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn a_failing_tool_call_is_surfaced_but_not_fatal() {
    // The model hallucinates a tool that doesn't exist; the turn must recover.
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "nonexistent_tool".to_string(),
            args: "{}".to_string(),
        },
        MockReply::Text("Recovered fine.".to_string()),
    ])
    .await;
    h.submit("use a bad tool").await;
    let screen = h.render();
    // The error is shown to the user (and was fed back to the model)…
    assert!(
        screen.contains("unknown tool") || screen.contains("Error"),
        "tool error not surfaced:\n{screen}"
    );
    // …and the turn continued to a normal reply instead of dying.
    assert!(
        screen.contains("Recovered fine."),
        "did not recover after tool error:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn clear_wipes_the_transcript() {
    let mut h = Harness::new(vec![MockReply::Text("first answer".to_string())]).await;
    h.submit("remember this").await;
    assert!(h.render().contains("first answer"));

    // `/clear` resets to a fresh session — prior turns must be gone.
    h.submit("/clear").await;
    let screen = h.render();
    assert!(
        screen.contains("conversation cleared"),
        "clear notice missing:\n{screen}"
    );
    assert!(
        !screen.contains("first answer") && !screen.contains("remember this"),
        "old transcript survived /clear:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn slash_help_renders_locally_without_a_turn() {
    let mut h = Harness::new(vec![]).await;
    // `/help` is handled locally — no model turn, so nothing is consumed.
    h.submit("/help").await;
    let screen = h.render();
    // The help text is long and the transcript follows to the bottom, so assert
    // on lines that stay visible there rather than the "Commands" header up top.
    assert!(
        screen.contains("/exit") && screen.contains("reload AGENTS.md"),
        "help output missing:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn usage_captured_after_turn() {
    // The mock always sends prompt_tokens:10 completion_tokens:5 in its usage chunk.
    let mut h = Harness::new(vec![MockReply::Text("pong".to_string())]).await;
    assert!(
        h.app.last_usage.is_none(),
        "last_usage must be None before any turn"
    );
    h.submit("ping").await;
    assert!(!h.app.running);
    assert_eq!(
        h.app.last_usage,
        Some((10, 5)),
        "last_usage should be populated from the mock's usage SSE chunk"
    );
}

#[tokio::test]
async fn multi_chunk_text_assembles_correctly() {
    // Three separate SSE content frames should be concatenated into one Assistant entry.
    let mut h = Harness::new(vec![MockReply::MultiChunk(vec![
        "Hel".to_string(),
        "lo, ".to_string(),
        "world!".to_string(),
    ])])
    .await;
    h.submit("say hello").await;
    assert!(!h.app.running);
    // The accumulator must stitch the deltas into a single entry.
    let assembled = h.app.transcript.iter().find_map(|e| match e {
        Entry::Assistant(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        assembled.as_deref(),
        Some("Hello, world!"),
        "streamed chunks not assembled correctly: {assembled:?}"
    );
}

#[tokio::test]
async fn reasoning_entry_appended_to_transcript() {
    // show_reasoning is true by default; a reasoning_content SSE delta should
    // land as Entry::Reasoning alongside the normal Entry::Assistant.
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "I am thinking.".to_string(),
        text: "Done.".to_string(),
    }])
    .await;
    assert!(h.app.show_reasoning, "show_reasoning must default to true");
    h.submit("think").await;
    assert!(!h.app.running);
    let has_reasoning = h
        .app
        .transcript
        .iter()
        .any(|e| matches!(e, Entry::Reasoning(t) if t.as_str() == "I am thinking."));
    assert!(has_reasoning, "Entry::Reasoning missing from transcript");
    let has_text = h
        .app
        .transcript
        .iter()
        .any(|e| matches!(e, Entry::Assistant(t) if t.as_str() == "Done."));
    assert!(has_text, "Entry::Assistant missing from transcript");
}

#[tokio::test]
async fn reasoning_hidden_in_render_after_toggle() {
    // After /reasoning, reasoning text must not appear in the rendered buffer even
    // though Entry::Reasoning is still stored (the entry is skipped at draw time).
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "secret thought".to_string(),
        text: "visible reply".to_string(),
    }])
    .await;
    h.submit("/reasoning").await;
    assert!(
        !h.app.show_reasoning,
        "show_reasoning should be false after first /reasoning"
    );
    h.submit("think aloud").await;
    assert!(!h.app.running);
    let screen = h.render();
    assert!(
        !screen.contains("secret thought"),
        "reasoning leaked into render when disabled:\n{screen}"
    );
    assert!(
        screen.contains("visible reply"),
        "text reply missing from render:\n{screen}"
    );
    // Toggling again re-enables display.
    h.submit("/reasoning").await;
    assert!(
        h.app.show_reasoning,
        "show_reasoning should be true after second /reasoning"
    );
}

#[tokio::test]
async fn statusbar_slash_command_updates_state() {
    // /statusbar is a local slash command — no model turn consumed.
    let mut h = Harness::new(vec![]).await;
    assert!(
        h.app.statusbar_mode == StatusBarMode::Truncate,
        "statusbar_mode should default to Truncate"
    );
    h.submit("/statusbar none").await;
    assert!(
        h.app.statusbar_mode == StatusBarMode::None,
        "/statusbar none did not set None mode"
    );
    h.submit("/statusbar wrap").await;
    assert!(
        h.app.statusbar_mode == StatusBarMode::Wrap,
        "/statusbar wrap did not set Wrap mode"
    );
    h.submit("/statusbar truncate").await;
    assert!(
        h.app.statusbar_mode == StatusBarMode::Truncate,
        "/statusbar truncate did not set Truncate mode"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn timestamps_slash_command_updates_state() {
    // /timestamps is a local slash command — no model turn consumed.
    let mut h = Harness::new(vec![]).await;
    assert!(
        h.app.timestamp_style == TimestampStyle::Relative,
        "timestamp_style should default to Relative"
    );
    h.submit("/timestamps exact").await;
    assert!(
        h.app.timestamp_style == TimestampStyle::Exact,
        "/timestamps exact did not set Exact style"
    );
    h.submit("/timestamps none").await;
    assert!(
        h.app.timestamp_style == TimestampStyle::None,
        "/timestamps none did not set None style"
    );
    h.submit("/timestamps relative").await;
    assert!(
        h.app.timestamp_style == TimestampStyle::Relative,
        "/timestamps relative did not set Relative style"
    );
    assert!(!h.app.running);
}
