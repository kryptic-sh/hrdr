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

use super::{App, Entry, EntryKind, StatusBarMode, TimestampStyle, TurnMsg};
use crate::ui;
use hrdr_agent::AgentConfig;

/// Stand-in for the binary's `art.txt` (the TUI takes the art from its caller).
const TEST_LOGO: &str = "██   ██ ██████  ██████  ██████\n██   ██ ██   ██ ██   ██ ██   ██\n███████ ██████  ██   ██ ██████\n██   ██ ██   ██ ██   ██ ██   ██\n██   ██ ██   ██ ██████  ██   ██";

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
    /// `AgentEvent::Reasoning` → `EntryKind::Reasoning` path.
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
            // accumulator appends them into one `EntryKind::Assistant`.
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
        Self::with_max_steps(replies, 50).await
    }

    async fn with_max_steps(replies: Vec<MockReply>, max_steps: usize) -> Self {
        let mock = MockServer::start(replies).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            base_url: mock.base_url.clone(),
            model: "test-model".to_string(),
            cwd: tmp.path().to_path_buf(),
            checkpoints: Some("off".to_string()),
            context_window: Some(1000),
            max_steps,
            ..Default::default()
        };
        let ui = hrdr_app::UiConfig {
            auto_resume: false, // never pick up the developer's real sessions
            ..Default::default()
        };
        let mut app = App::new(config, ui, TEST_LOGO).unwrap();
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
            name: "todo".to_string(),
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
    assert!(screen.contains("todo"), "tool call missing:\n{screen}");
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
                "todo".to_string(),
                r#"{"todos":[{"content":"first task","status":"in_progress"}]}"#.to_string(),
            ),
            ("glob".to_string(), r#"{"pattern":"*"}"#.to_string()),
        ]),
        MockReply::Text("Both ran.".to_string()),
    ])
    .await;
    h.submit("do two things").await;
    let screen = h.render();
    assert!(screen.contains("todo"), "first tool missing:\n{screen}");
    assert!(screen.contains("glob"), "second tool missing:\n{screen}");
    assert!(
        screen.contains("Both ran."),
        "final reply missing:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn read_only_tool_calls_run_concurrently_in_order() {
    // Two read-only calls in one turn exercise the concurrent batch path;
    // results must land (and render) for both, in call order.
    let mut h = Harness::new(vec![
        MockReply::ToolCalls(vec![
            ("glob".to_string(), r#"{"pattern":"*"}"#.to_string()),
            (
                "grep".to_string(),
                r#"{"pattern":"nothing-matches-this"}"#.to_string(),
            ),
        ]),
        MockReply::Text("Both read.".to_string()),
    ])
    .await;
    h.submit("scan the project").await;
    let screen = h.render();
    assert!(
        screen.contains("glob"),
        "glob missing:
{screen}"
    );
    assert!(
        screen.contains("grep"),
        "grep missing:
{screen}"
    );
    assert!(
        screen.contains("Both read."),
        "final reply missing:
{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn step_budget_exhaustion_wraps_up_instead_of_failing() {
    // max_steps = 2: two tool rounds, then the harness must ask the model to
    // wrap up (a final no-tools round) instead of erroring the turn.
    let mut h = Harness::with_max_steps(
        vec![
            MockReply::ToolCalls(vec![("glob".to_string(), r#"{"pattern":"*"}"#.to_string())]),
            MockReply::ToolCalls(vec![("glob".to_string(), r#"{"pattern":"*"}"#.to_string())]),
            MockReply::Text("Ran out of budget; here's where things stand.".to_string()),
        ],
        2,
    )
    .await;
    h.submit("loop forever").await;
    let screen = h.render();
    assert!(
        screen.contains("here's where things stand."),
        "wrap-up text missing:
{screen}"
    );
    assert!(
        screen.contains("tool-round limit reached"),
        "notice missing:
{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn verbatim_failing_retry_is_refused_on_third_attempt() {
    // The model retries the exact same failing call three rounds in a row;
    // the third must be refused without executing, then the turn ends.
    let bad = || {
        MockReply::ToolCalls(vec![(
            "read".to_string(),
            r#"{"path":"no/such/file.txt"}"#.to_string(),
        )])
    };
    let mut h = Harness::new(vec![
        bad(),
        bad(),
        bad(),
        MockReply::Text("Giving up differently.".to_string()),
    ])
    .await;
    h.submit("read that file").await;
    let screen = h.render();
    assert!(
        screen.contains("failed 2 times in a row"),
        "nudge missing:
{screen}"
    );
    assert!(
        screen.contains("refused without running"),
        "refusal missing:
{screen}"
    );
    assert!(
        screen.contains("Giving up differently."),
        "final text missing:
{screen}"
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
        h.app.state.usage.last().is_none(),
        "last_usage must be None before any turn"
    );
    h.submit("ping").await;
    assert!(!h.app.running);
    assert_eq!(
        h.app.state.usage.last(),
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
    let assembled = h.app.state.transcript.iter().find_map(|e| match &e.kind {
        EntryKind::Assistant(s) => Some(s.clone()),
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
    // land as EntryKind::Reasoning alongside the normal EntryKind::Assistant.
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "I am thinking.".to_string(),
        text: "Done.".to_string(),
    }])
    .await;
    assert!(h.app.show_reasoning, "show_reasoning must default to true");
    h.submit("think").await;
    assert!(!h.app.running);
    let has_reasoning = h.app.state.transcript.iter().any(
        |e| matches!(&e.kind, EntryKind::Reasoning { text, .. } if text.contains("I am thinking.")),
    );
    assert!(
        has_reasoning,
        "EntryKind::Reasoning missing from transcript"
    );
    let has_text = h
        .app
        .state
        .transcript
        .iter()
        .any(|e| matches!(&e.kind, EntryKind::Assistant(t) if t.as_str() == "Done."));
    assert!(has_text, "EntryKind::Assistant missing from transcript");
}

#[tokio::test]
async fn reasoning_hidden_in_render_after_toggle() {
    // After /reasoning, reasoning text must not appear in the rendered buffer even
    // though EntryKind::Reasoning is still stored (the entry is skipped at draw time).
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

// ---------------------------------------------------------------------------
// Scroll-offset preservation (Task 27 regression guard)
// ---------------------------------------------------------------------------

/// `TurnMsg::System` (async out-of-band line, e.g. a late `/models` result)
/// must NOT reset `scroll_offset` when the user has scrolled up.  When already
/// following (offset == 0) the value must remain 0 (still following).
#[tokio::test]
async fn system_msg_preserves_scroll_when_scrolled_up() {
    let mut h = Harness::new(vec![]).await;

    // Simulate the user having scrolled up.
    h.app.scroll_offset = 10;
    h.app
        .on_turn_msg(TurnMsg::System("async /models result".to_string()));
    assert_eq!(
        h.app.scroll_offset, 10,
        "TurnMsg::System reset scroll_offset while user was scrolled up"
    );

    // While following (offset == 0) the value must stay 0.
    h.app.scroll_offset = 0;
    h.app
        .on_turn_msg(TurnMsg::System("another notice".to_string()));
    assert_eq!(
        h.app.scroll_offset, 0,
        "TurnMsg::System changed scroll_offset while user was following"
    );
}

/// `TurnMsg::Diff` (async diff block from `/diff`) must not yank the scroll
/// position when the user is scrolled up.
#[tokio::test]
async fn diff_msg_preserves_scroll_when_scrolled_up() {
    let mut h = Harness::new(vec![]).await;

    h.app.scroll_offset = 5;
    h.app.on_turn_msg(TurnMsg::Diff(
        "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new".to_string(),
    ));
    assert_eq!(
        h.app.scroll_offset, 5,
        "TurnMsg::Diff reset scroll_offset while user was scrolled up"
    );

    h.app.scroll_offset = 0;
    h.app.on_turn_msg(TurnMsg::Diff(
        "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new".to_string(),
    ));
    assert_eq!(
        h.app.scroll_offset, 0,
        "TurnMsg::Diff changed scroll_offset while user was following"
    );
}

/// `AgentEvent::Notice` (MCP warning, health alert, step-budget exhaustion,
/// etc.) must not reset the scroll position when the user is scrolled up.
#[tokio::test]
async fn notice_event_preserves_scroll_when_scrolled_up() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![]).await;

    h.app.scroll_offset = 7;
    h.app.on_turn_msg(TurnMsg::Event(AgentEvent::Notice(
        "tool-round limit reached".to_string(),
    )));
    assert_eq!(
        h.app.scroll_offset, 7,
        "AgentEvent::Notice reset scroll_offset while user was scrolled up"
    );

    h.app.scroll_offset = 0;
    h.app.on_turn_msg(TurnMsg::Event(AgentEvent::Notice(
        "health warning".to_string(),
    )));
    assert_eq!(
        h.app.scroll_offset, 0,
        "AgentEvent::Notice changed scroll_offset while user was following"
    );
}

// ---------------------------------------------------------------------------
// Transcript scroll clamp (render-driven)
// ---------------------------------------------------------------------------

/// After a render pass, `scroll_offset` must be clamped to `max_scroll` (the
/// actual content height minus the viewport height).  Setting an absurdly large
/// offset and then rendering must bring it back in range.
#[tokio::test]
async fn scroll_offset_clamped_to_max_scroll_after_render() {
    let mut h = Harness::new(vec![MockReply::Text("hello world".to_string())]).await;
    h.submit("hi").await;
    // An unreachably large scroll offset.
    h.app.scroll_offset = usize::MAX / 2;
    h.render(); // drives draw(), which clamps scroll_offset to max_scroll
    assert!(
        h.app.scroll_offset <= h.app.max_scroll,
        "scroll_offset {} exceeds max_scroll {} after render",
        h.app.scroll_offset,
        h.app.max_scroll
    );
}

/// The rendered scrollback is a stack of blocks: the user prompt sits on its own
/// background, padded one column left/right and one blank row top/bottom, and a
/// tool call renders its name with a status mark plus the tool-specific detail.
///
/// Regression: this catches the padding, background, and separator regressions
/// that unit tests on `render_block` alone can't — it asserts on the actual
/// terminal cells, backgrounds included.
#[tokio::test]
async fn transcript_renders_padded_blocks_with_per_kind_backgrounds() {
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "bash".into(),
            args: r#"{"command":"echo hi"}"#.into(),
        },
        MockReply::Text("done".into()),
    ])
    .await;
    h.submit("run it").await;

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let theme = &h.app.theme;

    // Columns 0..59 — the last column is the scrollbar track, not block content.
    let row_text = |y: u16| -> String {
        (0..59)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let bg_at = |x: u16, y: u16| buf.cell(Position::new(x, y)).unwrap().bg;
    let find_row = |needle: &str| (0..40).find(|&y| row_text(y).contains(needle));

    // Content is inset by the block's horizontal padding, on both sides.
    let pad = " ".repeat(crate::ui::BLOCK_PAD_X);
    let user_y = find_row("run it").expect("user prompt rendered");
    // Padded one column in, on the user background, filled to the block's width.
    assert!(
        row_text(user_y).starts_with(&format!("{pad}run it")),
        "left padding"
    );
    for x in 0..59 {
        assert_eq!(bg_at(x, user_y), theme.user_bg, "user row bg at x={x}");
    }
    // The `#1 you · now` meta row closes the block: it's a block body row (not
    // chrome outside the block), sitting on the block background below the
    // message text, separated from it by a blank row.
    let meta_y = user_y + 2;
    assert!(
        row_text(meta_y).starts_with(&format!("{pad}#1 you · ")),
        "meta row"
    );
    assert_eq!(
        bg_at(0, meta_y),
        theme.user_bg,
        "meta row is inside the block"
    );
    assert_eq!(
        row_text(meta_y - 1).trim(),
        "",
        "blank row before the meta row"
    );
    assert_eq!(
        bg_at(0, meta_y - 1),
        theme.user_bg,
        "that blank row is inside the block too"
    );
    // Blank padded rows above the text and below the meta row.
    assert_eq!(bg_at(0, user_y - 1), theme.user_bg, "top pad row");
    assert_eq!(bg_at(0, meta_y + 1), theme.user_bg, "bottom pad row");
    assert_eq!(row_text(user_y - 1).trim(), "", "top pad row is blank");
    assert_eq!(row_text(meta_y + 1).trim(), "", "bottom pad row is blank");

    // A blank separator row (terminal bg) sits between blocks.
    assert_ne!(bg_at(0, meta_y + 2), theme.user_bg, "separator row");

    // The tool block: status mark + name on the header, command below it, both
    // on the tool background.
    let tool_y = find_row("✓ bash").expect("tool header rendered");
    assert_eq!(bg_at(0, tool_y), theme.tool_bg, "tool block bg");
    assert!(
        row_text(tool_y + 1).starts_with(&format!("{pad}$ echo hi")),
        "command line"
    );
    assert!(
        find_row("hi").is_some(),
        "command output rendered:\n{}",
        buffer_to_string(buf)
    );
}

/// Resuming a session restores the whole transcript verbatim: every entry kind,
/// in order, each with its original timestamp.
///
/// Regression: rebuilding the display from the chat `messages` dropped the
/// model's thoughts, system notices, the per-turn stats line, and `/diff`
/// output — and stamped whatever survived with the current time.
#[tokio::test]
async fn resume_restores_the_full_transcript_with_its_timestamps() {
    let mut h = Harness::new(vec![]).await;

    let t = |secs: i64| hrdr_app::time_from_unix(secs, chrono::Local::now());
    let transcript = vec![
        Entry::at(EntryKind::User("hi".into()), t(1_700_000_000)),
        Entry::at(
            EntryKind::Reasoning {
                text: "thinking".into(),
                took_ms: Some(1_200),
            },
            t(1_700_000_001),
        ),
        Entry::at(EntryKind::Assistant("hello".into()), t(1_700_000_002)),
        Entry::at(
            EntryKind::Tool {
                id: "c1".into(),
                name: "bash".into(),
                args: r#"{"command":"echo hi"}"#.into(),
                result: "hi".into(),
                ok: true,
                done: true,
                expanded: false,
            },
            t(1_700_000_003),
        ),
        Entry::at(EntryKind::Stats("✓ 59 tok".into()), t(1_700_000_004)),
        Entry::at(EntryKind::Diff("+added".into()), t(1_700_000_005)),
    ];
    let state = hrdr_app::SessionState {
        name: "old chat".into(),
        model: "test-model".into(),
        base_url: h.app.state.base_url.clone(),
        cwd: h.app.current_cwd(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: transcript.clone(),
        ..Default::default()
    };

    h.app
        .apply_session("old-chat".to_string(), hrdr_app::Session::new(state));

    // The restored entries are the saved ones, verbatim — kinds, order, times.
    // (Entries after these are the `/resume` notices, stamped now.)
    assert_eq!(&h.app.state.transcript[..transcript.len()], &transcript[..]);
}

/// A tool call still running when the session was saved restores as finished
/// and failed — nothing can complete it now, and a `done: false` block would
/// spin forever on a restored transcript.
#[tokio::test]
async fn resume_settles_a_tool_call_that_was_still_running() {
    let mut h = Harness::new(vec![]).await;
    let state = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: vec![Entry::tool_running("c1", "bash", "{}")],
        ..Default::default()
    };
    h.app
        .apply_session("interrupted".to_string(), hrdr_app::Session::new(state));

    let EntryKind::Tool { ok, done, .. } = &h.app.state.transcript[0].kind else {
        panic!("tool entry lost");
    };
    assert!(*done, "no spinner on a restored block");
    assert!(!*ok);
}

/// An auto-save persists the state the app is already holding — every entry the
/// user saw, not just the ones reconstructible from the chat messages.
#[tokio::test]
async fn autosave_persists_every_transcript_entry() {
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "bash".into(),
            args: r#"{"command":"echo hi"}"#.into(),
        },
        MockReply::Text("all done".into()),
    ])
    .await;
    h.submit("run it").await;

    // The turn's stats line and the tool call are both in the state that a save
    // writes verbatim.
    let kinds = &h.app.state.transcript;
    assert!(
        kinds.iter().any(|e| matches!(e.kind, EntryKind::Stats(_))),
        "the per-turn stats line is part of the state"
    );
    assert!(
        kinds
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Tool { name, .. } if name == "bash")),
        "the tool call is part of the state"
    );
    assert!(
        h.app.state.is_saveable(),
        "a user message makes it saveable"
    );
}

/// Slash-command output renders like assistant output — markdown, undimmed
/// colors — on its own distinct background, so it reads as content rather than
/// chrome. Also pins the per-kind backgrounds against each other.
#[tokio::test]
async fn slash_command_output_renders_as_markdown_on_the_command_background() {
    let mut h = Harness::new(vec![]).await;
    // `/sessions` output is a plain system entry; markdown structure and bold
    // spans both survive the render.
    h.app.push_entry(Entry::system("**bold** output"));

    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let theme = &h.app.theme;

    let row_text = |y: u16| -> String {
        (0..59)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let y = (0..20)
        .find(|&y| row_text(y).contains("bold output"))
        .expect("system entry rendered");

    let cell = buf.cell(Position::new(2, y)).unwrap();
    assert_eq!(cell.bg, theme.command_bg, "own background");
    assert_ne!(theme.command_bg, theme.tool_bg, "distinct from tool blocks");
    assert_ne!(
        theme.command_bg, theme.user_bg,
        "distinct from user prompts"
    );
    // Markdown was parsed: the `**` markers are gone and the text is bold.
    assert!(
        !row_text(y).contains('*'),
        "markdown rendered: {:?}",
        row_text(y)
    );
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::BOLD),
        "bold span survives"
    );
    // Not dimmed: system text uses the assistant's own color.
    assert_eq!(cell.fg, theme.assistant, "undimmed, like assistant output");
}

/// Nothing in the scrollback paints outside a block: every non-empty row starts
/// with the block's one-column left padding. The only bare rows are the blank
/// separators between blocks.
///
/// Regression: meta lines, the thinking spinner, stats lines, and queued-message
/// badges each used to render their own ad-hoc chrome at column 0.
#[tokio::test]
async fn every_transcript_row_is_rendered_through_the_block_path() {
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "bash".into(),
            args: r#"{"command":"echo hi"}"#.into(),
        },
        MockReply::Text("done".into()),
    ])
    .await;
    h.submit("run it").await;
    h.app.state.transcript.push(Entry::diff("+added"));
    h.app.queue.push_back("queued msg".into());

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();

    // Only inspect the transcript viewport (above the input box).
    let transcript_rows = h.app.transcript_height;
    for y in 0..transcript_rows {
        let row: String = (0..59)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect();
        if row.trim().is_empty() {
            continue; // blank separator or a block's pad row
        }
        assert!(
            row.starts_with(' '),
            "row {y} paints at column 0, outside the block: {row:?}"
        );
    }
}

/// The status bar's token counters survive a save/resume: cumulative in/out and
/// the last call's context size come back, rather than restarting at zero.
#[tokio::test]
async fn resume_restores_the_status_bar_token_counters() {
    let mut h = Harness::new(vec![MockReply::Text("hello".into())]).await;
    h.submit("hi").await;

    let usage = h.app.state.usage;
    assert!(
        usage.tokens_in > 0 && usage.tokens_out > 0,
        "turn accumulated tokens"
    );
    assert!(usage.last().is_some(), "turn reported usage");

    let mut h2 = Harness::new(vec![]).await;
    assert_eq!(h2.app.state.usage.tokens_in, 0, "fresh app starts at zero");
    let state = hrdr_app::SessionState {
        cwd: h2.app.current_cwd(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: h.app.state.transcript.clone(),
        usage,
        ..Default::default()
    };
    h2.app
        .apply_session("chat".to_string(), hrdr_app::Session::new(state));

    assert_eq!(h2.app.state.usage.tokens_in, usage.tokens_in);
    assert_eq!(h2.app.state.usage.tokens_out, usage.tokens_out);
    assert_eq!(
        h2.app.state.usage.last(),
        usage.last(),
        "context size restored"
    );
}

/// A session's saved `context_window` fills in the status bar's "of Y" only
/// when the live endpoint hasn't already told us the real one.
#[tokio::test]
async fn a_saved_context_window_never_clobbers_the_probed_one() {
    let mut h = Harness::new(vec![]).await;
    let probed = h.app.state.usage.context_window;
    assert!(probed.is_some(), "the harness config sets a context window");

    let cwd = h.app.current_cwd();
    let session = |window: Option<u32>| {
        hrdr_app::Session::new(hrdr_app::SessionState {
            cwd: cwd.clone(),
            messages: vec![hrdr_agent::Message::system("sys")],
            usage: hrdr_app::SessionUsage {
                context_window: window,
                ..Default::default()
            },
            ..Default::default()
        })
    };

    // A stale saved window loses to the one we already know.
    h.app.apply_session("chat".to_string(), session(Some(999)));
    assert_eq!(
        h.app.state.usage.context_window, probed,
        "probed window wins"
    );

    // With none known, the saved one fills in.
    h.app.state.usage.context_window = None;
    h.app.apply_session("chat".to_string(), session(Some(999)));
    assert_eq!(
        h.app.state.usage.context_window,
        Some(999),
        "saved window fills the gap"
    );
}

/// The app's state is the session file's payload: a turn's autosave writes it
/// to disk, and loading it back yields the same transcript, usage and identity —
/// no conversion layer in between.
#[tokio::test]
async fn autosave_writes_the_state_and_it_loads_back_identically() {
    let data_home = tempfile::tempdir().unwrap();
    // SAFETY: `sessions_dir()` reads this; the value is only used by this test.
    unsafe { std::env::set_var("XDG_DATA_HOME", data_home.path()) };

    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "think".into(),
        text: "**done**".into(),
    }])
    .await;
    h.submit("run it").await;

    // The turn-end autosave assigned an id and wrote the file.
    let id = h
        .app
        .state
        .id
        .clone()
        .expect("autosave assigned a session id");
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id).expect("session file written");

    // The transcript came back verbatim — including the model's reasoning and
    // the per-turn stats line, neither of which exists in `messages`. The only
    // difference is the ephemeral chrome (welcome banner, "session saved as …"),
    // which is never written. Times persist as whole seconds.
    let saved = &loaded.state.transcript;
    let live: Vec<&Entry> = h
        .app
        .state
        .transcript
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::Notice(_)))
        .collect();
    assert_eq!(
        saved.len(),
        live.len(),
        "one saved entry per non-notice entry"
    );
    for (a, b) in saved.iter().zip(&live) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.time.timestamp(), b.time.timestamp());
    }
    assert!(
        !saved.iter().any(|e| matches!(e.kind, EntryKind::Notice(_))),
        "no chrome on disk"
    );
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(e.kind, EntryKind::Reasoning { .. })),
        "the model's thoughts are persisted"
    );
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(e.kind, EntryKind::Stats(_))),
        "the per-turn stats line is persisted"
    );

    // …as did the status bar's counters and the session's identity.
    assert_eq!(loaded.state.usage, h.app.state.usage);
    assert_eq!(loaded.state.model, h.app.state.model);
    assert_eq!(
        loaded.state.id.as_deref(),
        Some(id.as_str()),
        "id from the file name"
    );
    assert_eq!(loaded.state.messages.len(), 3, "system + user + assistant");
}

/// A new session opens with the banner: an animated logo on the left and the
/// session's details (model, provider, cwd) on the right, all inside one block.
#[tokio::test]
async fn a_new_session_opens_with_the_header_banner() {
    let mut h = Harness::new(vec![]).await;
    assert!(
        matches!(h.app.state.transcript[0].kind, EntryKind::Header),
        "the header is the transcript's first entry"
    );

    let mut term = Terminal::new(TestBackend::new(64, 32)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());

    assert!(
        screen.contains("███████ ██████"),
        "logo art rendered:\n{screen}"
    );
    assert!(
        screen.contains("model    test-model"),
        "model shown:\n{screen}"
    );
    assert!(screen.contains("provider"), "provider shown:\n{screen}");
    assert!(screen.contains("cwd"), "cwd shown:\n{screen}");
    assert!(
        screen.contains(concat!("version  ", env!("CARGO_PKG_VERSION"))),
        "version shown:\n{screen}"
    );

    // Every detail value starts at the same screen column — the version's too.
    //
    // Regression: the version rendered as a `hrdr v0.2.8` title rather than a
    // `key value` row, so its value sat several columns left of the others.
    let value_col = |key: &str| -> usize {
        let line = screen
            .lines()
            .find(|l| l.contains(key))
            .unwrap_or_else(|| panic!("no {key} row in:\n{screen}"));
        let after = &line[line.find(key).unwrap() + key.len()..];
        // Screen column of the value's first non-space character.
        line.chars().count() - after.trim_start().chars().count()
    };
    let cols: Vec<usize> = ["version", "model", "provider", "cwd"]
        .iter()
        .map(|k| value_col(k))
        .collect();
    assert!(
        cols.iter().all(|c| *c == cols[0]),
        "detail values are not aligned (value columns {cols:?}):\n{screen}"
    );
}

/// The logo animation advances with the wall clock.
///
/// Regression: `hjkl_splash` reads its clock from an anchor, and rebuilding the
/// `Splash` per frame with `Instant::now()` pins the tick at 0 — the art would
/// render, but the highlight would never move.
#[tokio::test]
async fn the_header_logo_animates_across_frames() {
    let mut h = Harness::new(vec![]).await;
    let render = |app: &mut App| {
        let mut term = Terminal::new(TestBackend::new(64, 32)).unwrap();
        term.draw(|f| ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        // The animation shows up as per-cell foreground colors over the art.
        (0..32u16)
            .flat_map(|y| (0..30u16).map(move |x| (x, y)))
            .map(|(x, y)| buf.cell(Position::new(x, y)).unwrap().fg)
            .collect::<Vec<_>>()
    };

    let first = render(&mut h.app);
    // Longer than the splash's default 120ms tick period.
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;
    let later = render(&mut h.app);
    assert_ne!(first, later, "the trail did not move between frames");
}

/// `/clear` starts a new session, so it opens with the banner again.
#[tokio::test]
async fn clearing_reseeds_the_header() {
    let mut h = Harness::new(vec![]).await;
    h.submit("hi").await;
    h.app.clear_all();
    assert!(
        matches!(h.app.state.transcript[0].kind, EntryKind::Header),
        "a cleared transcript opens with the header again"
    );
    assert_eq!(
        h.app
            .state
            .transcript
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Header))
            .count(),
        1,
        "exactly one header"
    );
}

/// The header survives a save/resume like any other entry, and shows the
/// *current* model rather than the one recorded when it was written — it stores
/// no data of its own.
#[tokio::test]
async fn the_header_persists_and_shows_live_details() {
    let entry = Entry::header();
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"header""#), "{json}");
    // Times persist as whole unix seconds.
    let back: Entry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind, entry.kind, "round-trips");
    assert_eq!(back.time.timestamp(), entry.time.timestamp());

    let mut h = Harness::new(vec![]).await;
    let state = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        model: "restored-model".into(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: vec![Entry::header()],
        ..Default::default()
    };
    h.app
        .apply_session("s".to_string(), hrdr_app::Session::new(state));

    let mut term = Terminal::new(TestBackend::new(64, 32)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("model    restored-model"),
        "details read live session state:\n{screen}"
    );
}

/// A viewport too narrow for both columns drops the details rather than
/// overflowing the block (or panicking).
#[tokio::test]
async fn a_narrow_viewport_drops_the_header_details() {
    let mut h = Harness::new(vec![]).await;
    // Too narrow for both columns. The wrapped welcome text pushes the header
    // off the top, so draw once to measure, scroll to the top, then draw again.
    let mut term = Terminal::new(TestBackend::new(30, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());

    assert!(
        screen.contains("█████"),
        "the logo still renders:\n{screen}"
    );
    assert!(!screen.contains("model  "), "details dropped:\n{screen}");
}

/// Model / provider precedence, highest first: **flag > env > session > config**.
///
/// A flag or env var pins the value: resuming a session — automatically at
/// startup or explicitly via `/resume` — must not switch the model out from
/// under it. The endpoint is never taken from the session either.
///
/// Regression: resuming replaced the configured model with whatever the session
/// last used, so `hrdr --provider go --model deepseek-v4-flash` in a directory
/// with a saved session silently ran a different model, and the header showed
/// the wrong one. `base_url` was clobbered too, contradicting the resume
/// notice's "session endpoint was X (current: Y)".
#[tokio::test]
async fn a_pinned_model_and_provider_survive_a_resume() {
    for explicit_resume in [false, true] {
        let mut h = Harness::new(vec![]).await;
        // As if `--model flash --provider go` (or $HRDR_MODEL / $HRDR_PROVIDER).
        h.app.cfg.model_pinned = true;
        h.app.cfg.provider_pinned = true;
        h.app.state.model = "flash".into();
        h.app.state.provider = Some("go".into());
        let launch_endpoint = h.app.state.base_url.clone();

        let saved = hrdr_app::SessionState {
            cwd: h.app.current_cwd(),
            model: "pro".into(),
            provider: Some("zen".into()),
            base_url: "https://saved.example/v1".into(),
            messages: vec![hrdr_agent::Message::system("sys")],
            transcript: vec![Entry::user("earlier")],
            ..Default::default()
        };
        if explicit_resume {
            h.app
                .apply_session("old".to_string(), hrdr_app::Session::new(saved));
        } else {
            h.app.auto_resume_state(saved, "old".to_string());
        }

        assert_eq!(
            h.app.state.model, "flash",
            "pinned model wins (explicit_resume={explicit_resume})"
        );
        assert_eq!(
            h.app.state.provider.as_deref(),
            Some("go"),
            "pinned provider wins (explicit_resume={explicit_resume})"
        );
        assert_eq!(
            h.app.state.base_url, launch_endpoint,
            "the endpoint belongs to this process, not the session"
        );
        // The conversation itself did come back.
        assert!(
            h.app
                .state
                .transcript
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "earlier")),
            "the saved transcript is restored"
        );
    }
}

/// With nothing pinned, the value came from the config file (or a provider
/// preset's default) — and a resumed session outranks config.
#[tokio::test]
async fn an_unpinned_model_and_provider_yield_to_the_session() {
    for explicit_resume in [false, true] {
        let mut h = Harness::new(vec![]).await;
        assert!(!h.app.cfg.model_pinned, "the harness config isn't pinned");
        h.app.state.model = "from-config".into();
        h.app.state.provider = None;

        let saved = hrdr_app::SessionState {
            cwd: h.app.current_cwd(),
            model: "pro".into(),
            provider: Some("zen".into()),
            messages: vec![hrdr_agent::Message::system("sys")],
            ..Default::default()
        };
        if explicit_resume {
            h.app
                .apply_session("old".to_string(), hrdr_app::Session::new(saved));
        } else {
            h.app.auto_resume_state(saved, "old".to_string());
        }

        assert_eq!(
            h.app.state.model, "pro",
            "session beats config (explicit_resume={explicit_resume})"
        );
        assert_eq!(h.app.state.provider.as_deref(), Some("zen"));
    }
}

/// Session chrome — the welcome banner, "resumed session …", "session saved
/// as …" — is regenerated on every launch and every resume, so it is never
/// persisted.
///
/// Regression: notices were saved with the transcript, so each resume restored
/// the previous run's notices *and* appended a fresh one. Ten resumes, ten
/// stacked "resumed session" lines.
#[tokio::test]
async fn resume_notices_do_not_accumulate() {
    let data_home = tempfile::tempdir().unwrap();
    // SAFETY: `sessions_dir()` reads this; only this test uses the value.
    unsafe { std::env::set_var("XDG_DATA_HOME", data_home.path()) };

    let mut h = Harness::new(vec![MockReply::Text("ok".into())]).await;
    h.submit("hi").await;
    let id = h.app.state.id.clone().expect("session saved");
    let cwd = h.app.current_cwd();

    let notices = |app: &App| {
        app.state
            .transcript
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Notice(_)))
            .count()
    };
    let saved_notices = |id: &str, cwd: &str| {
        hrdr_app::Session::load(cwd, id)
            .unwrap()
            .state
            .transcript
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Notice(_)))
            .count()
    };
    assert_eq!(saved_notices(&id, &cwd), 0, "no chrome written");

    // Resume the session repeatedly, autosaving each time as a real run would.
    for round in 1..=3 {
        let session = hrdr_app::Session::load(&cwd, &id).unwrap();
        h.app.apply_session(id.clone(), session);
        h.app.autosave();

        assert_eq!(
            saved_notices(&id, &cwd),
            0,
            "round {round}: chrome must never reach disk"
        );
        // The live transcript shows this resume's notices, not a pile of them.
        assert!(
            notices(&h.app) <= 3,
            "round {round}: {} notices on screen — they are accumulating",
            notices(&h.app)
        );
    }

    // The conversation itself is untouched by all that resuming.
    let user_msgs = h
        .app
        .state
        .transcript
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::User(t) if t == "hi"))
        .count();
    assert_eq!(user_msgs, 1, "the conversation is not duplicated");
}

/// `/clear` (and its aliases `/new`, `/reset`) take an optional name for the
/// fresh session, so it saves under that name instead of one derived from its
/// first message.
#[tokio::test]
async fn clear_and_new_take_a_session_name() {
    let data_home = tempfile::tempdir().unwrap();
    // SAFETY: `sessions_dir()` reads this; only this test uses the value.
    unsafe { std::env::set_var("XDG_DATA_HOME", data_home.path()) };

    let mut h = Harness::new(vec![
        MockReply::Text("ok".into()),
        MockReply::Text("ok".into()),
    ])
    .await;
    h.submit("first message").await;
    assert_eq!(
        h.app.state.name, "first message",
        "name derived from the message"
    );

    // Bare `/clear` starts an unnamed session.
    h.type_str("/clear");
    h.press(KeyCode::Enter);
    assert!(h.app.state.name.is_empty(), "no name yet");
    assert!(
        h.app.state.id.is_none(),
        "detached from the old session file"
    );

    // `/new <name>` — the alias — names it up front.
    h.type_str("/new Project X");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.state.name, "Project X");
    assert!(
        h.app.state.id.is_none(),
        "id assigned on first save, not now"
    );

    // The next turn's autosave writes it under that name, slugified.
    h.submit("second message").await;
    assert_eq!(h.app.state.name, "Project X", "the name survives the turn");
    assert_eq!(
        h.app.state.id.as_deref(),
        Some("project-x"),
        "file id from the name"
    );
    let cwd = h.app.current_cwd();
    assert_eq!(
        hrdr_app::Session::load(&cwd, "project-x")
            .unwrap()
            .state
            .name,
        "Project X",
        "the named session is on disk"
    );
}

/// The `⠋ Thinking` and `Thought: 1.2s` labels render the same way: one label
/// row, then exactly one blank row, then the thought text.
///
/// Regression: the streaming spinner was a header row, while the finished label
/// was spliced into the entry's *text* as `"Thought: 1.2s\n\n"` — so it went
/// through markdown, was persisted into the transcript, and the two states had
/// different spacing.
#[tokio::test]
async fn thinking_and_thought_labels_render_identically() {
    // The rightmost column is the scrollbar, not block content.
    let content = |l: &str| -> String {
        let mut c: Vec<char> = l.chars().collect();
        c.pop();
        c.into_iter().collect::<String>().trim().to_string()
    };
    let rows_after_label = |screen: &str, label: &str| -> (String, String) {
        let mut it = screen.lines().skip_while(|l| !l.contains(label)).skip(1);
        (
            content(it.next().unwrap_or_default()),
            content(it.next().unwrap_or_default()),
        )
    };
    let render = |app: &mut App| {
        let mut term = Terminal::new(TestBackend::new(50, 40)).unwrap();
        term.draw(|f| ui::draw(f, app)).unwrap();
        // Scroll to the top so the block is on screen, then redraw.
        app.scroll_offset = app.max_scroll;
        term.draw(|f| ui::draw(f, app)).unwrap();
        buffer_to_string(term.backend().buffer())
    };

    // Finished: the duration is data on the entry, not text inside it.
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "let me think".into(),
        text: "done".into(),
    }])
    .await;
    h.submit("go").await;
    let reasoning = h
        .app
        .state
        .transcript
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::Reasoning { text, took_ms } => Some((text.clone(), *took_ms)),
            _ => None,
        })
        .expect("a reasoning entry");
    assert_eq!(
        reasoning.0, "let me think",
        "the label is not spliced into the text"
    );
    assert!(
        reasoning.1.is_some(),
        "the elapsed time is recorded as data"
    );

    let screen = render(&mut h.app);
    let (blank, text) = rows_after_label(&screen, "Thought:");
    assert_eq!(blank, "", "exactly one blank row after the label");
    assert_eq!(text, "let me think", "then the thought");

    // Streaming: same shape, spinner label.
    let mut h2 = Harness::new(vec![]).await;
    h2.app
        .state
        .transcript
        .push(Entry::reasoning("streaming thoughts"));
    h2.app.running = true;
    h2.app.reasoning_start = Some(std::time::Instant::now());

    let screen = render(&mut h2.app);
    let (blank, text) = rows_after_label(&screen, "Thinking");
    assert_eq!(blank, "", "exactly one blank row after the label");
    assert_eq!(text, "streaming thoughts", "then the thought");
}
