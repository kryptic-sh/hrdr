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
use ratatui::style::Color;
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

/// A block's padding row may carry the left bar (`┃`). Strip it before asking
/// whether the row is blank.
fn without_bar(row: &str) -> &str {
    row.trim_start_matches(crate::ui::BORDER_BAR).trim()
}

/// Point `sessions_dir()` at a temp directory for the duration of a test.
///
/// `XDG_DATA_HOME` is process-global, so the tests that write session files must
/// not run concurrently — they'd overwrite each other's `sessions/` root. The
/// returned guard holds a lock for the whole test and keeps the temp dir alive.
fn isolated_data_home() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A previous test's panic must not poison the lock for everyone else.
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the lock above serializes every writer and reader of this var.
    unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };
    (guard, tmp)
}

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
        row_text(user_y).starts_with(&format!(
            "{}{}run it",
            crate::ui::BORDER_BAR,
            " ".repeat(crate::ui::BLOCK_PAD_X - 1)
        )),
        "the bar, then the remaining padding"
    );
    for x in 0..59 {
        assert_eq!(bg_at(x, user_y), theme.user_bg, "user row bg at x={x}");
    }
    // The `#1 you · now` meta row closes the block: it's a block body row (not
    // chrome outside the block), sitting on the block background below the
    // message text, separated from it by a blank row.
    let meta_y = user_y + 2;
    assert!(row_text(meta_y).contains("#1 you · "), "meta row");
    assert_eq!(
        bg_at(0, meta_y),
        theme.user_bg,
        "meta row is inside the block"
    );
    assert_eq!(
        without_bar(&row_text(meta_y - 1)),
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
    assert_eq!(
        without_bar(&row_text(user_y - 1)),
        "",
        "top pad row is blank"
    );
    assert_eq!(
        without_bar(&row_text(meta_y + 1)),
        "",
        "bottom pad row is blank"
    );

    // A blank separator row (terminal bg) sits between blocks.
    assert_ne!(bg_at(0, meta_y + 2), theme.user_bg, "separator row");

    // The tool block: status mark + name on the header, command below it, both
    // on the tool background.
    let tool_y = find_row("✓ bash").expect("tool header rendered");
    assert_eq!(
        bg_at(0, tool_y),
        theme.user_bg,
        "tool blocks share the prompt bg"
    );
    assert!(
        row_text(tool_y + 1).starts_with(&format!("{pad}echo hi")),
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
    assert_ne!(
        theme.command_bg, theme.user_bg,
        "distinct from user prompts (and so from tool blocks, which share it)"
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
            row.starts_with(' ') || row.starts_with(crate::ui::BORDER_BAR),
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
    let _data_home = isolated_data_home();

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
    let _data_home = isolated_data_home();

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
    let _data_home = isolated_data_home();

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

/// A thinking block is just the thought: no `⠋ Thinking` spinner, no
/// `Thought: 1.2s` footer. The dimmer text already says whose voice it is, and
/// the loader above the input says a turn is running.
///
/// The elapsed time is still recorded on the entry — it's the only trace of how
/// long the model thought — it simply isn't drawn.
#[tokio::test]
async fn a_thinking_block_renders_no_label() {
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
    assert_eq!(reasoning.0, "let me think", "the thought, and nothing else");
    assert!(reasoning.1.is_some(), "the elapsed time is still recorded");

    // Neither label is on screen — while streaming, nor once finished.
    let mut term = Terminal::new(TestBackend::new(50, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("let me think"),
        "the thought renders:\n{screen}"
    );
    assert!(!screen.contains("Thought"), "a footer survived:\n{screen}");

    // Streaming shows no spinner label either.
    let mut h2 = Harness::new(vec![]).await;
    h2.app
        .state
        .transcript
        .push(Entry::reasoning("streaming thoughts"));
    h2.app.running = true;
    h2.app.reasoning_start = Some(std::time::Instant::now());
    let mut term = Terminal::new(TestBackend::new(50, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h2.app)).unwrap();
    h2.app.scroll_offset = h2.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h2.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(screen.contains("streaming thoughts"), "{screen}");
    assert!(
        !screen.contains("Thinking"),
        "a spinner label survived:\n{screen}"
    );
}

/// A whitespace-only thinking block renders nothing either — no lone
/// `Thought: …` label over blank padding.
#[tokio::test]
async fn an_empty_thinking_block_renders_nothing() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::reasoning("   \n"));

    let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(!screen.contains("Thought"), "{screen}");
}

/// An empty text delta must not open an assistant entry in the first place.
#[tokio::test]
async fn an_empty_text_delta_opens_no_entry() {
    let mut h = Harness::new(vec![]).await;
    let before = h.app.state.transcript.len();
    h.app
        .on_turn_msg(TurnMsg::Event(hrdr_agent::AgentEvent::Text(String::new())));
    assert_eq!(
        h.app.state.transcript.len(),
        before,
        "an empty delta created a transcript entry"
    );
}

/// The borrowed label is a real jump point: `/goto 2` scrolls to the block that
/// carries it, even though the assistant turn painted no block of its own.
#[tokio::test]
async fn goto_finds_a_text_less_assistant_turn() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    // Enough filler that the target is off-screen before the jump.
    for i in 0..12 {
        h.app.push_entry(Entry::system(format!("filler {i}")));
    }
    h.app
        .push_entry(Entry::reasoning("thought about something"));
    h.app.push_entry(Entry::assistant("")); // message #2
    h.app
        .push_entry(Entry::tool_running("c1", "bash", r#"{"command":"ls"}"#));

    let mut term = Terminal::new(TestBackend::new(40, 14)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    h.type_str("/goto 2");
    h.press(KeyCode::Enter);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());

    // The jump puts the target message's block at the top of the viewport.
    let first_rows: String = screen.lines().take(3).collect::<Vec<_>>().join("\n");
    assert!(
        first_rows.contains("thought about something"),
        "/goto 2 landed elsewhere:\n{screen}"
    );
}

/// Clicking a tool block toggles its expansion. The click rects are derived from
/// where each tool block lands on screen, which the deferred block flush must
/// keep accurate.
#[tokio::test]
async fn clicking_a_tool_block_toggles_its_expansion() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let long_output: String = (0..30).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::reasoning("thinking"));
    h.app.push_entry(Entry::assistant("")); // borrows its label from the thought
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long_output,
        ok: true,
        done: true,
        expanded: false,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    // The recorded hit rect must cover the row the tool header actually renders on.
    let buf = term.backend().buffer();
    let header_y = (0..30)
        .find(|&y| {
            (0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>()
                .contains("✓ bash")
        })
        .expect("tool header rendered");
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    assert!(
        rect.contains(2, header_y),
        "the tool hit rect misses the tool header at row {header_y}"
    );

    // Clicking it expands the block; clicking again collapses it.
    let click = |y: u16| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    let expanded = |app: &App| {
        app.state
            .transcript
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Tool { expanded, .. } if *expanded))
    };
    assert!(!expanded(&h.app), "starts collapsed");
    h.app.on_mouse(click(header_y));
    assert!(expanded(&h.app), "the click expanded it");
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    h.app.on_mouse(click(header_y));
    assert!(!expanded(&h.app), "the second click collapsed it");
}

/// The per-turn stats line closes the turn's block instead of opening one of its
/// own: same background as the reply, above the `#N assistant` label.
#[tokio::test]
async fn the_stats_line_rides_on_the_turns_block() {
    let mut h = Harness::new(vec![MockReply::Text("all done".into())]).await;
    h.submit("run it").await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));

    let mut term = Terminal::new(TestBackend::new(46, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row_of = |needle: &str| -> u16 {
        (0..30)
            .find(|&y| {
                (0..45)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"))
    };
    let bg_at = |y: u16| buf.cell(Position::new(2, y)).unwrap().bg;

    let reply_y = row_of("all done");
    let stats_y = row_of("tok/s");
    let label_y = row_of("#2 assistant");

    // Inside the reply's block: same background, no separator between them.
    assert_eq!(
        bg_at(stats_y),
        bg_at(reply_y),
        "stats share the block:\n{screen}"
    );
    assert_ne!(bg_at(stats_y), h.app.theme.stats_bg, "no block of its own");
    // Ordering: reply, stats, then the label that closes the block.
    assert!(reply_y < stats_y, "stats follow the reply");
    assert!(stats_y < label_y, "the label still closes the block");
    // A user prompt block sits above, on its own background.
    assert_ne!(bg_at(reply_y), bg_at(row_of("run it")));
}

/// A user prompt renders through the same path as the model's output: markdown
/// is parsed, and the text uses the same foreground color. Only the block's
/// background differs.
///
/// Regression: prompts were emitted as raw styled lines in a bespoke `user`
/// color, so `**bold**` showed its asterisks and the two spoke in different
/// colors.
#[tokio::test]
async fn user_prompts_render_like_the_models_output() {
    let mut h = Harness::new(vec![MockReply::Text("**reply** text".into())]).await;
    h.submit("**prompt** text").await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));

    let mut term = Terminal::new(TestBackend::new(44, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    // Markdown is parsed on both sides: no literal `**` survives.
    assert!(!screen.contains('*'), "markdown not rendered:\n{screen}");
    assert!(screen.contains("prompt text"), "{screen}");
    assert!(screen.contains("reply text"), "{screen}");

    let cell_of = |needle: &str| {
        let y = (0..30)
            .find(|&y| {
                (0..43)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"));
        // Column 2 is the first content column; `**bold**` starts there.
        let c = buf.cell(Position::new(2, y)).unwrap();
        (c.fg, c.bg, c.modifier)
    };
    let (prompt_fg, prompt_bg, prompt_mod) = cell_of("prompt text");
    let (reply_fg, reply_bg, reply_mod) = cell_of("reply text");

    // Same foreground, and the bold span survived on both.
    assert_eq!(prompt_fg, reply_fg, "prompt and reply share a foreground");
    assert!(prompt_mod.contains(ratatui::style::Modifier::BOLD));
    assert!(reply_mod.contains(ratatui::style::Modifier::BOLD));

    // Only the background differs.
    assert_eq!(prompt_bg, h.app.theme.user_bg);
    assert_eq!(reply_bg, Color::Reset);
}

/// Fenced code renders at the block's own indentation, with no language tag row
/// above it — it is the file's text, not a framed widget.
///
/// Regression: code blocks were padded into a solid rectangle (an extra leading
/// column) and prefixed with a dim `rs` tag line.
#[tokio::test]
async fn fenced_code_has_no_extra_indent_or_language_row() {
    let mut h = Harness::new(vec![MockReply::Text(
        "text\n\n```rs\nfn main() {\n    let x = 1;\n}\n```\n".into(),
    )])
    .await;
    h.submit("go").await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));

    let mut term = Terminal::new(TestBackend::new(44, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    let rows: Vec<&str> = screen.lines().collect();

    let prose_y = rows
        .iter()
        .position(|l| l.contains("text"))
        .expect("prose rendered");
    let indent = |l: &str| l.len() - l.trim_start().len();

    // No `rs` tag row between the prose and the code.
    assert!(
        rows[prose_y + 1].contains("fn main()"),
        "a language row was inserted:\n{screen}"
    );
    // The code starts in the same column as the prose around it…
    assert_eq!(
        indent(rows[prose_y + 1]),
        indent(rows[prose_y]),
        "code is indented past the prose:\n{screen}"
    );
    // …and the file's own indentation is preserved exactly.
    assert_eq!(
        indent(rows[prose_y + 2]) - indent(rows[prose_y]),
        4,
        "the file's 4-space indent changed:\n{screen}"
    );
}

/// A blank separator row appears only between two *tinted* blocks. Their padded
/// rows carry their backgrounds, so a prompt and the tool call it triggered — or
/// two tool calls — would otherwise merge into one slab. A block on the terminal
/// background already begins and ends in a blank row, so it needs no separator
/// on either side.
///
/// Two untinted blocks are the other way round: each contributes a plain blank
/// pad row, and two is one too many between the model's thought and its output —
/// so one is dropped.
///
/// prompt │ tool │ tool │ thought │ tool │ output
///        ↑blank ↑blank ↑         ↑      ↑
#[tokio::test]
async fn separator_rows_appear_only_between_tinted_blocks() {
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: format!("res-{id}"),
            ok: true,
            done: true,
            expanded: false,
        })
    };
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("prompt"));
    h.app.push_entry(tool("a", "ls"));
    h.app.push_entry(tool("b", "cat"));
    h.app.push_entry(Entry::reasoning("thought"));
    h.app.push_entry(tool("c", "grep"));
    h.app.push_entry(Entry::assistant("output"));
    h.app.push_entry(tool("d", "wc"));

    let mut term = Terminal::new(TestBackend::new(40, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row_of = |needle: &str| -> u16 {
        (0..40)
            .find(|&y| {
                (0..39)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"))
    };
    // Blank rows strictly between two blocks' content rows. Two blocks always
    // contribute their own bottom + top pad; a separator makes it three.
    let blank = |y: u16| {
        without_bar(
            &(0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>(),
        )
        .is_empty()
    };
    let gap = |from: u16, to: u16| (from + 1..to).filter(|&y| blank(y)).count();

    // Anchor on each block's *last* content row and the next block's first.
    let (prompt_end, ls_end, cat_end) = (row_of("#1 you"), row_of("res-a"), row_of("res-b"));
    let (thought, grep_end) = (row_of("thought"), row_of("res-c"));

    // Tinted → tinted: both blocks' pads, plus a separator row between them.
    assert_eq!(
        gap(prompt_end, row_of("ls")),
        3,
        "prompt → tool needs a separator:\n{screen}"
    );
    assert_eq!(
        gap(ls_end, row_of("cat")),
        3,
        "tool → tool needs a separator:\n{screen}"
    );

    // Tinted → untinted and back: just the two pads, no separator.
    assert_eq!(gap(cat_end, thought), 2, "tool → thought:\n{screen}");
    assert_eq!(gap(thought, row_of("grep")), 2, "thought → tool:\n{screen}");
    assert_eq!(
        gap(grep_end, row_of("output")),
        2,
        "tool → output:\n{screen}"
    );
}

/// The model's thought and the output that follows it are separated by a single
/// blank row, not two.
///
/// Regression: each block contributes a blank padded row (below and above), and
/// with neither tinted they stacked into a two-row gap.
#[tokio::test]
async fn a_thought_and_the_output_after_it_share_one_blank_row() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Reasoning {
        text: "thinking here".into(),
        took_ms: Some(1_100),
    }));
    h.app.push_entry(Entry::assistant("the output"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "ls".into(),
        args: "{}".into(),
        result: "res".into(),
        ok: true,
        done: true,
        expanded: false,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row_of = |needle: &str| -> u16 {
        (0..30)
            .find(|&y| {
                (0..39)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"))
    };
    let blank = |y: u16| {
        without_bar(
            &(0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>(),
        )
        .is_empty()
    };
    let gap = |from: u16, to: u16| (from + 1..to).filter(|&y| blank(y)).count();

    // Untinted → untinted: exactly one blank row.
    assert_eq!(
        gap(row_of("thinking here"), row_of("the output")),
        1,
        "thought → output:\n{screen}"
    );
    // Untinted → tinted is unchanged: the two blocks' own pads.
    assert_eq!(
        gap(row_of("#1 assistant"), row_of("ls")),
        2,
        "output → tool:\n{screen}"
    );
}

/// Collapsing a long tool block pulls its top to the top of the viewport rather
/// than letting it slide.
///
/// Regression: `scroll_offset` is measured from the *bottom*, so shrinking the
/// transcript kept the view the same distance from the end — the block the user
/// was reading jumped up by however many rows it lost.
#[tokio::test]
async fn collapsing_a_tool_block_keeps_it_at_the_top_of_the_view() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long,
        ok: true,
        done: true,
        expanded: true, // long and open
    }));
    h.app.push_entry(Entry::assistant("after"));

    let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    // Scroll up until the tool header is on screen, mid-viewport.
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let header_row = |term: &Terminal<TestBackend>| -> Option<u16> {
        let buf = term.backend().buffer();
        (0..20).find(|&y| {
            (0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>()
                .contains("✓ bash")
        })
    };
    let before = header_row(&term).expect("tool header on screen");
    assert!(h.app.scroll_offset > 0, "the reader is scrolled up");

    // Click it: the block collapses, and its top comes to the viewport's top.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    assert!(rect.contains(2, before));
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: before,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let after = header_row(&term).expect("tool header still on screen");
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        after <= 1,
        "the collapsed block should sit at the top of the viewport, got row {after}:\n{screen}"
    );
}

/// Collapsing while following the newest output must not scroll away from the
/// bottom: the view is already pinned there, and there's nothing to keep in
/// place.
#[tokio::test]
async fn collapsing_while_following_stays_at_the_bottom() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "bash".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long,
        ok: true,
        done: true,
        expanded: true,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 16)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert_eq!(h.app.scroll_offset, 0, "following the newest output");

    // The header is off the top of a long expanded block; click its last row.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row: rect.y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    assert_eq!(h.app.scroll_offset, 0, "still following the newest output");
}

/// A tinted block at the end of the scrollback gets the same blank row it would
/// get before another tinted block, so it doesn't butt up against the input.
#[tokio::test]
async fn a_trailing_tinted_block_ends_with_a_blank_row() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "bash".into(),
        args: "{}".into(),
        result: "res".into(),
        ok: true,
        done: true,
        expanded: false,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let last_content = (0..h.app.transcript_height)
        .rev()
        .find(|&y| {
            (0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>()
                .contains("res")
        })
        .expect("tool output rendered");
    let bg_at = |y: u16| buf.cell(Position::new(2, y)).unwrap().bg;

    // Its own bottom pad (tinted), then a blank row on the terminal background.
    assert_eq!(
        bg_at(last_content + 1),
        h.app.theme.user_bg,
        "bottom pad:\n{screen}"
    );
    assert_eq!(
        bg_at(last_content + 2),
        Color::Reset,
        "a blank row closes the scrollback:\n{screen}"
    );
}

/// The input pane is borderless, on the user prompt's background, with one blank
/// row above and below and two columns either side — the same chrome a
/// transcript block wears.
#[tokio::test]
async fn the_input_pane_matches_the_user_prompt_block() {
    let mut h = Harness::new(vec![]).await;
    h.type_str("hello world");

    let mut term = Terminal::new(TestBackend::new(50, 26)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row = |y: u16| -> String {
        (0..50)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let text_y = (0..26)
        .find(|&y| row(y).contains("hello world"))
        .expect("the draft renders");
    let bg_at = |x: u16, y: u16| buf.cell(Position::new(x, y)).unwrap().bg;

    // No border glyphs anywhere on the pane.
    for y in text_y - 1..=text_y + 1 {
        let r = row(y);
        for ch in ['┌', '┐', '└', '┘', '│', '─'] {
            assert!(!r.contains(ch), "border glyph {ch:?} on row {y}:\n{screen}");
        }
    }

    // The prompt's background, across the full width and the padding rows.
    for x in [0, 2, 49] {
        for y in [text_y - 1, text_y, text_y + 1] {
            assert_eq!(bg_at(x, y), h.app.theme.user_bg, "({x},{y}):\n{screen}");
        }
    }
    // One blank row above and below the text.
    assert_eq!(without_bar(&row(text_y - 1)), "", "top padding:\n{screen}");
    assert_eq!(
        without_bar(&row(text_y + 1)),
        "",
        "bottom padding:\n{screen}"
    );
    // The bar, then the remaining padding column, then the text.
    assert!(
        row(text_y).starts_with(&format!("{}{}hello world", crate::ui::BORDER_BAR, " ")),
        "{screen}"
    );

    // A blank row separates the tinted pane from the chrome below it.
    let below = row(text_y + 2);
    assert_eq!(below.trim(), "", "blank row below the input:\n{screen}");
    assert_eq!(
        bg_at(2, text_y + 2),
        Color::Reset,
        "and it is not tinted:\n{screen}"
    );

    // Nothing below the pane but the status bar: the footer row is gone, so the
    // editor's mode and the draft size no longer render anywhere.
    assert!(!screen.contains("[TEXT]"), "no mode footer:\n{screen}");
    assert!(!screen.contains("11 ch"), "no draft-size footer:\n{screen}");
}

/// The "follow output" button floats two rows above the input pane, with an
/// arrow at each end, and clicking it returns to following the newest output.
#[tokio::test]
async fn the_follow_button_floats_above_the_input_and_is_clickable() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    for i in 0..30 {
        h.app.push_entry(Entry::system(format!("filler {i}")));
    }
    h.type_str("draft");

    let mut term = Terminal::new(TestBackend::new(50, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(h.app.follow_button.is_none(), "no button while following");

    // Scroll up: the button appears.
    h.app.scroll_offset = 5;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let rect = h.app.follow_button.expect("the follow button is drawn");
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    // An arrow at each end of the label. Read only the button's own columns —
    // the rest of the row is still transcript.
    let label: String = (rect.x..rect.x + rect.w)
        .filter_map(|x| {
            buf.cell(Position::new(x, rect.y))
                .map(|c| c.symbol().to_string())
        })
        .collect();
    let trimmed = label.trim();
    assert!(trimmed.starts_with('↓'), "left arrow: {label:?}\n{screen}");
    assert!(trimmed.ends_with('↓'), "right arrow: {label:?}\n{screen}");
    assert!(trimmed.contains("Press END to follow output"), "{screen}");

    // Two rows above the input pane, so it doesn't sit on the pane itself.
    let pane_top = (0..20)
        .find(|&y| buf.cell(Position::new(2, y)).unwrap().bg == h.app.theme.user_bg)
        .expect("the input pane renders");
    assert_eq!(rect.y + 2, pane_top, "two rows above the pane:\n{screen}");
    assert_ne!(
        buf.cell(Position::new(2, rect.y)).unwrap().bg,
        h.app.theme.user_bg,
        "the button is clear of the pane:\n{screen}"
    );

    // Clicking it resumes following.
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 1,
        row: rect.y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    assert_eq!(h.app.scroll_offset, 0, "the click resumed following");
}

/// A blank row sits between the inference loader and the input pane below it.
#[tokio::test]
async fn a_blank_row_follows_the_generating_line() {
    let mut h = Harness::new(vec![]).await;
    h.app.running = true;
    h.app.turn_started = Some(std::time::Instant::now());

    let mut term = Terminal::new(TestBackend::new(56, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row = |y: u16| -> String {
        (0..56)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let loader_y = (0..24)
        .find(|&y| row(y).contains("inferring"))
        .expect("the loader renders while a turn runs");

    // The row below it is blank, and on the terminal background — the input
    // pane's own (tinted) top padding comes after that.
    assert_eq!(row(loader_y + 1).trim(), "", "blank row below:\n{screen}");
    assert_eq!(
        buf.cell(Position::new(2, loader_y + 1)).unwrap().bg,
        Color::Reset,
        "the blank row is not the input pane's padding:\n{screen}"
    );
    assert_eq!(
        buf.cell(Position::new(2, loader_y + 2)).unwrap().bg,
        h.app.theme.user_bg,
        "the input pane starts after it:\n{screen}"
    );
}

/// The user's own surfaces — the prompt block and the input pane — wear a bar
/// down their left edge, running their whole height. A tool call shares the
/// prompt's background but not its bar; it isn't the user speaking.
#[tokio::test]
async fn the_prompt_and_input_wear_a_left_bar() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .state
        .transcript
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("prompt here"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "bash".into(),
        args: r#"{"command":"echo hi"}"#.into(),
        result: "hi".into(),
        ok: true,
        done: true,
        expanded: false,
    }));
    h.type_str("typing");

    let mut term = Terminal::new(TestBackend::new(54, 26)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    let cell = |y: u16| buf.cell(Position::new(0, y)).unwrap();
    let row = |y: u16| -> String {
        (0..54)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let row_of = |needle: &str| (0..26).find(|&y| row(y).contains(needle)).unwrap();

    // The prompt block: the bar spans its padding rows too.
    let prompt_y = row_of("prompt here");
    for y in prompt_y - 1..=prompt_y + 1 {
        assert_eq!(cell(y).symbol(), "┃", "bar on row {y}:\n{screen}");
        assert_eq!(cell(y).fg, h.app.theme.prompt_border);
        assert_eq!(cell(y).bg, h.app.theme.user_bg);
    }

    // The tool block shares the background but wears no bar.
    let tool_y = row_of("✓ bash");
    assert_eq!(
        cell(tool_y).symbol(),
        " ",
        "no bar on a tool block:\n{screen}"
    );
    assert_eq!(
        buf.cell(Position::new(2, tool_y)).unwrap().bg,
        h.app.theme.user_bg
    );

    // The input pane: bar down its whole height, padding rows included.
    let input_y = row_of("typing");
    for y in input_y - 1..=input_y + 1 {
        assert_eq!(cell(y).symbol(), "┃", "bar on input row {y}:\n{screen}");
        assert_eq!(cell(y).fg, h.app.theme.prompt_border);
    }
}

/// The status bar renders through the block renderer: two columns of padding
/// either side, and a blank row above and below it.
#[tokio::test]
async fn the_status_bar_is_a_padded_block() {
    let mut h = Harness::new(vec![]).await;
    let mut term = Terminal::new(TestBackend::new(54, 26)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    let row = |y: u16| -> String {
        (0..54)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let status_y = (0..26)
        .find(|&y| row(y).contains("of 1.0k"))
        .expect("the status bar renders");

    assert!(
        row(status_y).starts_with("  "),
        "two columns of padding:\n{screen}"
    );
    // The content is laid out at the inner width, so the last two columns are
    // always padding.
    for x in [52u16, 53] {
        assert_eq!(
            buf.cell(Position::new(x, status_y)).unwrap().symbol(),
            " ",
            "column {x} is padding:\n{screen}"
        );
    }
    assert_eq!(row(status_y - 1).trim(), "", "blank row above:\n{screen}");
    // Its own trailing pad row is the last row on the screen — the status bar is
    // the bottom-most chrome now that the footer is gone.
    assert_eq!(row(status_y + 1).trim(), "", "blank row below:\n{screen}");
    assert_eq!(
        status_y + 1,
        25,
        "the status bar sits at the bottom:\n{screen}"
    );
}

/// There is no footer: the row that used to carry the editor's mode, the draft
/// size and the keybindings is gone entirely. The keys live in `/help`, and the
/// mode is signalled by the cursor's shape (see `tui::sync_cursor`).
#[tokio::test]
async fn the_footer_is_gone_and_the_keys_live_in_help() {
    let mut h = Harness::new(vec![]).await;
    h.type_str("draft");

    let mut term = Terminal::new(TestBackend::new(70, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("[TEXT]"),
        "the mode left the screen:\n{screen}"
    );
    assert!(
        !screen.contains("~2 tok · 5 ch"),
        "the draft size left the screen:\n{screen}"
    );
    assert!(
        !screen.contains("Enter=send"),
        "the keybindings left the screen:\n{screen}"
    );

    // `/help` lists them, including the plain engine's own hint. (A fresh
    // harness: the draft above would otherwise prefix the command.)
    let mut h = Harness::new(vec![]).await;
    h.type_str("/help");
    h.press(KeyCode::Enter);
    let help = h
        .app
        .state
        .transcript
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Notice(t) | EntryKind::System(t) if t.contains("Keys:") => Some(t.clone()),
            _ => None,
        })
        .expect("/help output");
    assert!(help.contains("Enter=send"), "the engine's keys:\n{help}");
    assert!(help.contains("Ctrl+G=$EDITOR"), "{help}");
    assert!(help.contains("@path attaches a file"), "{help}");
    assert!(help.contains("click a tool block"), "{help}");
}

/// The "follow output" and quit-confirm banners share one render path: same row
/// above the input pane, same bold centering — only their text and colors
/// differ. The quit confirmation takes the row when both would apply.
#[tokio::test]
async fn both_banners_render_through_the_same_path() {
    let mut h = Harness::new(vec![]).await;
    for i in 0..30 {
        h.app.push_entry(Entry::system(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(50, 20)).unwrap();
    let cell = |term: &Terminal<TestBackend>, x: u16, y: u16| {
        let c = term.backend().buffer().cell(Position::new(x, y)).unwrap();
        (c.fg, c.bg, c.modifier)
    };
    let label = |term: &Terminal<TestBackend>, rect: crate::app::HitRect| -> String {
        let buf = term.backend().buffer();
        (rect.x..rect.x + rect.w)
            .filter_map(|x| {
                buf.cell(Position::new(x, rect.y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };

    // Scrolled up: the follow banner, in the warn colors.
    h.app.scroll_offset = 5;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let follow = h.app.follow_button.expect("the follow banner is drawn");
    assert!(label(&term, follow).contains("follow output"));
    let (fg, bg, m) = cell(&term, follow.x + 1, follow.y);
    assert_eq!((fg, bg), (Color::Black, h.app.theme.warn));
    assert!(m.contains(ratatui::style::Modifier::BOLD), "bold");

    // Arming the quit takes the same row, in the error colors, flanked by its
    // icon, and is not clickable.
    h.app.quit_armed = true;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.follow_button.is_none(),
        "the quit banner isn't clickable"
    );
    let screen = buffer_to_string(term.backend().buffer());
    let quit_row: String = (0..50)
        .filter_map(|x| {
            term.backend()
                .buffer()
                .cell(Position::new(x, follow.y))
                .map(|c| c.symbol().to_string())
        })
        .collect();
    let at = quit_row
        .find("Press Ctrl+C again to quit")
        .unwrap_or_else(|| panic!("the quit banner takes the follow banner's row:\n{screen}"));
    // The icon flanks the label on both sides.
    assert!(
        quit_row.contains("● Press Ctrl+C again to quit ●"),
        "the icon flanks the label: {quit_row:?}"
    );
    // Sample the quit banner's own cells — it is a different width, so it is
    // centered on different columns.
    let (fg, bg, m) = cell(&term, at as u16, follow.y);
    assert_eq!((fg, bg), (Color::White, h.app.theme.error));
    assert!(m.contains(ratatui::style::Modifier::BOLD), "bold");
}
