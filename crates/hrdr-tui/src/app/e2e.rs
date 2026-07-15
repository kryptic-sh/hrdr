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

/// `max_model_len` is vLLM's non-standard context-window field, which the client
/// reads to fill the status bar's "X of Y".
const MOCK_CONTEXT_WINDOW: u32 = 4096;

fn models_body() -> String {
    format!(
        "{{\"object\":\"list\",\"data\":[{{\"id\":\"test-model\",\"object\":\"model\",\
         \"owned_by\":\"local\",\"max_model_len\":{MOCK_CONTEXT_WINDOW}}}]}}"
    )
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
        // A harnessed app is a REAL app: it autosaves sessions, appends to the input
        // history, persists a `/timestamps` toggle, records a `/model` pick. Every one
        // of those lands under `$HOME` — which, in a test binary, is the throwaway
        // sandbox `hrdr-test-support`'s ctor installed before `main` ever ran. Nothing
        // to call here: the floor is already not the developer's home.
        let mock = MockServer::start(replies).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            base_url: mock.base_url.clone(),
            model: "local://test-model".parse().unwrap(),
            cwd: tmp.path().to_path_buf(),
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

    /// Let a `/model` switch LAND: drain until the switch task posts the identity the
    /// agent actually adopted.
    ///
    /// The chrome is deliberately not written on the keystroke any more. Settling a
    /// switch can need a network round-trip (confirming a ChatGPT entitlement the
    /// cached list cannot vouch for), and a switch that is then refused must leave the
    /// status bar exactly where the agent stayed — so the display only ever follows
    /// the agent, one message later. The real event loop drains that message; a test
    /// that switches has to as well.
    async fn settle_switch(&mut self) {
        while let Some(msg) = self.rx.recv().await {
            let landed = matches!(msg, TurnMsg::Identity(..));
            self.app.on_turn_msg(msg);
            if landed {
                return;
            }
        }
    }

    /// Drain the turn channel until the agent is no longer running.
    async fn pump(&mut self) {
        while self.app.running() {
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

/// A PRIVATE, EMPTY `sessions_dir()` and user config for the duration of one test —
/// for the tests that assert on exactly what they wrote there, and would otherwise read
/// a *sibling test's* files as their own.
///
/// This is no longer about the developer's files, and it is not what stands between a
/// test and `~/.local/share/hrdr`: `hrdr-test-support`'s ctor moved `$HOME` and the XDG
/// roots to a throwaway directory before `main`, for every test in the binary, with
/// nothing to call and nothing to remember. But that sandbox is ONE root shared by every
/// test in the process, and cargo runs them in parallel — a test asserting "the session
/// store holds exactly one session" needs a root no sibling can write. That is this
/// guard's only remaining job.
///
/// It hands the root back on drop: `XDG_DATA_HOME` / `XDG_CONFIG_HOME` are process-
/// global, so a test holding a private root holds the lock while it does, and must not
/// leave the vars pointing at a temp dir that is about to be deleted.
fn isolated_data_home() -> DataHomeGuard {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A previous test's panic must not poison the lock for everyone else.
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the lock above serializes every writer and reader of these vars.
    unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };
    unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
    DataHomeGuard {
        _lock: guard,
        _tmp: tmp,
    }
}

/// The lifetime of a private data home: holds the env lock and the temp dir, and puts
/// the process-wide roots back when the test ends.
struct DataHomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
}

impl Drop for DataHomeGuard {
    fn drop(&mut self) {
        let (data, config, _cache) = hrdr_test_support::user_state_dirs();
        // SAFETY: the lock is still held (it is dropped after this), so no other test
        // is reading or writing these vars.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", data);
            std::env::set_var("XDG_CONFIG_HOME", config);
        }
    }
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    // Both calls landed in the transcript. (Asserted there, not on the screen:
    // the tool blocks scroll off the top of a 30-row terminal.)
    let tools: Vec<String> = h
        .app
        .transcript()
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Tool { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tools, ["todo", "glob"], "both tools ran, in order");

    let screen = h.render();
    assert!(
        screen.contains("Both ran."),
        "final reply missing:\n{screen}"
    );
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
}

#[tokio::test]
async fn usage_captured_after_turn() {
    // The mock always sends prompt_tokens:10 completion_tokens:5 in its usage chunk.
    let mut h = Harness::new(vec![MockReply::Text("pong".to_string())]).await;
    assert!(
        h.app.state().usage.last().is_none(),
        "last_usage must be None before any turn"
    );
    h.submit("ping").await;
    assert!(!h.app.running());
    assert_eq!(
        h.app.state().usage.last(),
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
    assert!(!h.app.running());
    // The accumulator must stitch the deltas into a single entry.
    let assembled = h.app.transcript().iter().find_map(|e| match &e.kind {
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
    assert!(!h.app.running());
    let has_reasoning = h.app.transcript().iter().any(
        |e| matches!(&e.kind, EntryKind::Reasoning { text, .. } if text.contains("I am thinking.")),
    );
    assert!(
        has_reasoning,
        "EntryKind::Reasoning missing from transcript"
    );
    let has_text = h
        .app
        .transcript()
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
    assert!(!h.app.running());
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
        model: "local://test-model".parse().unwrap(),
        base_url: h.app.state().base_url.clone(),
        cwd: h.app.current_cwd(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: transcript.clone(),
        ..Default::default()
    };

    h.app
        .apply_session("old-chat".to_string(), hrdr_app::Session::new(state));

    // The restored entries are the saved ones, verbatim — kinds, order, times.
    // (Entries after these are the `/resume` notices, stamped now.)
    assert_eq!(&h.app.transcript()[..transcript.len()], &transcript[..]);
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

    let EntryKind::Tool { ok, done, .. } = &h.app.transcript()[0].kind else {
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
    let kinds = &h.app.transcript();
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
        h.app.state().is_saveable(),
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
/// A message submitted while a turn runs is never injected mid-stream. When the
/// model ends the turn by answering — no tool call to ride in on — the message
/// waits and is sent as a turn of its own.
///
/// Regression: the text was pushed straight into the transcript at submit time
/// and the agent continued the finished turn to deliver it.
#[tokio::test]
async fn a_mid_turn_submit_waits_when_the_model_just_answers() {
    let mut h = Harness::new(vec![MockReply::Text("first reply".into())]).await;

    // A turn is in flight.
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("second question");
    h.press(KeyCode::Enter);

    assert_eq!(
        h.app.pending(),
        ["second question"],
        "the message is queued"
    );
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "second question")),
        "and not yet in the conversation"
    );
    assert!(h.app.running(), "the running turn was not disturbed");
    assert_eq!(h.app.editor.content(), "", "the draft was taken");

    // The turn ends: the queued message becomes its own turn.
    h.app.on_turn_msg(TurnMsg::Done(None));
    assert!(h.app.pending().is_empty(), "the queue drained");
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "second question")),
        "the queued message was sent after the turn"
    );
    assert!(h.app.running(), "as a turn of its own");
}

/// The steering path: a message queued while the model is working rides in with
/// the next round's tool results, so the model reads its tool output and the
/// correction together and can change course. It enters the transcript at
/// delivery — not at submit — so display order matches the model's view.
#[tokio::test]
async fn a_queued_message_rides_in_with_the_tool_results() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![]).await;
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);

    // Submitted while the model works.
    h.type_str("actually, use ripgrep");
    h.press(KeyCode::Enter);
    let user_entries = |h: &Harness| -> Vec<String> {
        h.app
            .transcript()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::User(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    };
    assert!(
        user_entries(&h).is_empty(),
        "not in the conversation until the model sees it"
    );
    // It is handed to the running turn, which drains it before its next request.
    assert_eq!(h.app.steering_len_for_test(), 1);
    assert_eq!(h.app.pending().len(), 1, "and shown as pending meanwhile");

    // `Agent::run` drains it after the round's tool results and says so — the queue
    // is the agent's, so taking it off is part of what the agent does, not something
    // the frontend does in parallel.
    let taken = h
        .app
        .live_subagents
        .take_pending(hrdr_agent::MAIN_KEY)
        .expect("the agent takes it off its own queue");
    h.app
        .on_turn_msg(TurnMsg::Event(AgentEvent::Steered(taken.display)));
    assert_eq!(
        user_entries(&h),
        ["actually, use ripgrep"],
        "displayed at delivery"
    );
    assert!(h.app.pending().is_empty(), "no longer pending");

    // The turn continues; nothing is re-sent when it ends.
    h.app.on_turn_msg(TurnMsg::Done(None));
    assert_eq!(user_entries(&h), ["actually, use ripgrep"], "sent once");
    assert!(!h.app.running(), "no follow-up turn was spawned");
}

/// A cancelled turn drops the message it was carrying rather than leaking it
/// into the next one.
#[tokio::test]
async fn cancelling_drops_an_undelivered_steer() {
    let mut h = Harness::new(vec![]).await;
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("never mind");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.steering_len_for_test(), 1);

    h.app.cancel_turn();
    assert!(h.app.pending().is_empty());
    assert_eq!(h.app.steering_len_for_test(), 0, "the agent's copy too");
}

/// Several mid-turn submits queue up and are sent one per completed turn, in the
/// order they were typed.
#[tokio::test]
async fn queued_messages_are_sent_fifo_one_turn_at_a_time() {
    let mut h = Harness::new(vec![]).await;
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    for msg in ["one", "two"] {
        h.type_str(msg);
        h.press(KeyCode::Enter);
    }
    assert_eq!(h.app.pending(), ["one", "two"]);

    h.app.on_turn_msg(TurnMsg::Done(None));
    assert_eq!(
        h.app.pending(),
        ["two"],
        "one turn spawns per completion, oldest first"
    );

    // Cancelling drops what is still waiting rather than sending it later.
    h.app.cancel_turn();
    assert!(h.app.pending().is_empty(), "a cancel discards the queue");
}

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
    h.app.transcript_mut().push(Entry::diff("+added"));
    h.app
        .live_subagents
        .enqueue(hrdr_agent::MAIN_KEY, hrdr_agent::Steer::plain("queued msg"));

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

    let usage = h.app.state().usage;
    assert!(
        usage.tokens_in > 0 && usage.tokens_out > 0,
        "turn accumulated tokens"
    );
    assert!(usage.last().is_some(), "turn reported usage");

    let mut h2 = Harness::new(vec![]).await;
    assert_eq!(
        h2.app.state().usage.tokens_in,
        0,
        "fresh app starts at zero"
    );
    let state = hrdr_app::SessionState {
        cwd: h2.app.current_cwd(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: h.app.transcript().clone(),
        usage,
        ..Default::default()
    };
    h2.app
        .apply_session("chat".to_string(), hrdr_app::Session::new(state));

    assert_eq!(h2.app.state().usage.tokens_in, usage.tokens_in);
    assert_eq!(h2.app.state().usage.tokens_out, usage.tokens_out);
    assert_eq!(
        h2.app.state().usage.last(),
        usage.last(),
        "context size restored"
    );
}

/// A session's saved `context_window` fills in the status bar's "of Y" only
/// when the live endpoint hasn't already told us the real one.
#[tokio::test]
async fn a_saved_context_window_never_clobbers_the_probed_one() {
    let mut h = Harness::new(vec![]).await;
    let probed = h.app.state().usage.context_window;
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
        h.app.state().usage.context_window,
        probed,
        "probed window wins"
    );

    // With none known, the saved one fills in.
    h.app.state_mut().usage.context_window = None;
    h.app.apply_session("chat".to_string(), session(Some(999)));
    assert_eq!(
        h.app.state().usage.context_window,
        Some(999),
        "saved window fills the gap"
    );
}

/// On startup the endpoint is asked for the model's context window, so the
/// status bar's gauge has an "of Y" side without one being configured.
///
/// Regression: the only context probe ran on a `/model` switch, so a session
/// against an endpoint that advertises its window (vLLM's `max_model_len` here)
/// still opened with a bare token count and no compaction threshold.
#[tokio::test]
async fn the_context_window_is_probed_from_the_endpoint_on_startup() {
    let mut h = Harness::new(vec![]).await;

    // Nothing configured: the probe asks the endpoint and posts what it says.
    h.app.state_mut().usage.context_window = None;
    h.app.spawn_context_probe();
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), h.rx.recv())
        .await
        .expect("the probe posts a context window")
        .expect("the channel is open");
    assert!(
        matches!(msg, TurnMsg::ContextWindow(hrdr_app::PaneId::Main, w) if w == MOCK_CONTEXT_WINDOW),
        "the probe posts the endpoint's advertised window"
    );
    h.app.on_turn_msg(msg);
    assert_eq!(
        h.app.state().usage.context_window,
        Some(MOCK_CONTEXT_WINDOW),
        "the probed window reaches the status bar"
    );

    // Already known (config, provider entry, or restored session): left alone,
    // and no request is made.
    h.app.state_mut().usage.context_window = Some(1000);
    h.app.spawn_context_probe();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), h.rx.recv())
            .await
            .is_err(),
        "a configured window is not re-probed"
    );
    assert_eq!(h.app.state().usage.context_window, Some(1000));
}

/// Mid-turn durability: a `History` snapshot (emitted after each committed
/// tool round) persists the session *while the turn is still running* — the
/// regular autosave can't (the turn holds the agent lock). A crash mid-turn
/// then loses at most the round in flight.
#[tokio::test]
async fn history_snapshot_persists_the_session_mid_turn() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;

    // Simulate a running turn: the turn task would hold the agent lock; here
    // the flag alone shows the regular autosave path is not what saves us.
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.push_entry(Entry::user("do the thing"));
    let snapshot = vec![
        hrdr_agent::Message::user("do the thing"),
        hrdr_agent::Message::assistant("on it"),
    ];
    h.app
        .on_turn_msg(TurnMsg::Event(hrdr_agent::AgentEvent::History(
            snapshot.clone(),
        )));

    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("the mid-turn snapshot assigned a session id");
    let loaded =
        hrdr_app::Session::load(&h.app.current_cwd(), &id).expect("session file written mid-turn");
    assert_eq!(
        loaded.state.messages.len(),
        snapshot.len(),
        "the snapshot's messages were persisted"
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
        .state()
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
        .transcript()
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
    assert_eq!(loaded.state.usage, h.app.state().usage);
    assert_eq!(loaded.state.model, h.app.state().model);
    assert_eq!(
        loaded.state.id.as_deref(),
        Some(id.as_str()),
        "id from the file name"
    );
    assert_eq!(loaded.state.messages.len(), 3, "system + user + assistant");
}

/// The shared sub-agent transcript cell starts empty (no id yet) and is
/// repointed at the session's dir once the first autosave assigns an id.
#[tokio::test]
async fn autosave_populates_the_subagent_transcript_dir() {
    let _data_home = isolated_data_home();

    let mut h = Harness::new(vec![MockReply::Text("done".into())]).await;
    // Before any save there is no id, so the cell stays empty.
    assert!(
        h.app.subagent_dir.lock().unwrap().is_none(),
        "cell empty until an id is assigned"
    );

    h.submit("go").await;

    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("autosave assigned a session id");
    let want = hrdr_app::subagent_transcript_dir(&h.app.current_cwd(), &id);
    assert_eq!(
        *h.app.subagent_dir.lock().unwrap(),
        Some(want),
        "cell points at the session's sub-agent dir after autosave"
    );

    // `/clear` detaches the session, so the cell must be reset too — otherwise
    // the next session's early sub-agents would misfile into this dir.
    h.app.clear_all();
    assert!(
        h.app.subagent_dir.lock().unwrap().is_none(),
        "clear_all resets the sub-agent transcript cell"
    );
}

/// The session id — and so the sub-agent transcript dir — must exist before the
/// turn's first tool batch runs, not after it.
///
/// The id used to be assigned only when the agent emitted its first `History`
/// event, which lands *after* that round's tools have already executed. A
/// brand-new session's first delegated `task` therefore spawned with an empty dir
/// cell and its transcript was silently dropped — exactly the crash the
/// transcript exists to survive.
#[tokio::test]
async fn the_first_turn_reserves_the_session_id_before_any_tool_can_run() {
    let _data_home = isolated_data_home();

    let mut h = Harness::new(vec![MockReply::Text("done".into())]).await;
    assert!(h.app.state().id.is_none(), "a fresh session has no id");
    assert!(h.app.subagent_dir.lock().unwrap().is_none());

    // Send the message but do NOT pump: the turn has been launched and nothing
    // the agent produces has been processed yet — the same instant a first-round
    // `task` tool call would spawn its sub-agent.
    h.type_str("go");
    h.press(KeyCode::Enter);

    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("the id is reserved at turn start, before the agent runs");
    assert_eq!(
        *h.app.subagent_dir.lock().unwrap(),
        Some(hrdr_app::subagent_transcript_dir(&h.app.current_cwd(), &id)),
        "a sub-agent spawned in the first round already has somewhere to write"
    );

    h.pump().await;
    assert_eq!(
        h.app.state().id.as_deref(),
        Some(id.as_str()),
        "id is stable"
    );
}

/// A new session opens with the banner: an animated logo on the left and the
/// session's details (model, provider, cwd) on the right, all inside one block.
#[tokio::test]
async fn a_new_session_opens_with_the_header_banner() {
    let mut h = Harness::new(vec![]).await;
    assert!(
        matches!(h.app.transcript()[0].kind, EntryKind::Header),
        "the header is the transcript's first entry"
    );

    // The harness runs in a temp dir, and macOS hands out long `/var/folders/…`
    // paths that push the cwd's value onto a wrapped row of its own — which the
    // column check below can't read. Pin it short so the row stays one line.
    h.app.dir = "/w".into();

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
        matches!(h.app.transcript()[0].kind, EntryKind::Header),
        "a cleared transcript opens with the header again"
    );
    assert_eq!(
        h.app
            .transcript()
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
        model: "local://restored-model".parse().unwrap(),
        messages: vec![hrdr_agent::Message::system("sys")],
        transcript: vec![Entry::header()],
        ..Default::default()
    };
    h.app
        .apply_session("s".to_string(), hrdr_app::Session::new(state));
    // The chrome follows the agent, never leads it: let the repoint land.
    h.settle_switch().await;

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

/// Model precedence, highest first: **the session > flag > env > config**.
///
/// `--model` / `$HRDR_MODEL` name the identity a **new** session starts on — the
/// default, not a pin. A session that already carries one (it was resumed, or `/model`
/// picked it) keeps it: the provider and the model are part of the conversation, and
/// resuming it brings BOTH back — identity and endpoint together, not one half each.
///
/// Regression (the rule this replaces): a launch flag used to outrank the session, so
/// resuming a `zen://kimi-k2` conversation under `hrdr --model chatgpt://gpt-5.5`
/// carried on the old messages against a different model at a different provider.
#[tokio::test]
async fn a_resumed_session_keeps_its_own_model_over_a_launch_flag() {
    for explicit_resume in [false, true] {
        let mut h = Harness::new(vec![]).await;
        // As if `hrdr --model chatgpt://gpt-5.5` (or `$HRDR_MODEL`).
        h.app.state_mut().model = "chatgpt://gpt-5.5".parse().unwrap();

        let saved = hrdr_app::SessionState {
            cwd: h.app.current_cwd(),
            model: "zen://kimi-k2".parse().unwrap(),
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
        // The chrome follows the agent, never leads it: let the repoint land.
        h.settle_switch().await;
        h.app.sync_panes();

        assert_eq!(
            h.app.state().model.to_string(),
            "zen://kimi-k2",
            "the session's identity wins, whole (explicit_resume={explicit_resume})"
        );
        // …and the agent — the thing doing the talking — went with it.
        let (provider, endpoint) = {
            let a = h.app.agent.lock().await;
            (a.provider_name().to_string(), a.endpoint_base_url())
        };
        assert_eq!(provider, "zen", "the agent is on the session's provider");
        assert!(
            endpoint.contains("opencode.ai"),
            "pointed at that provider's endpoint, not the launch one: {endpoint}"
        );
        assert_eq!(
            h.app.state().base_url,
            endpoint,
            "and the bar names the endpoint the agent is actually talking to"
        );
        // The conversation itself came back too.
        assert!(
            h.app
                .transcript()
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "earlier")),
            "the saved transcript is restored"
        );
    }
}

/// The other half of the same rule: what `/model` picked is what a later resume
/// restores — a pick is the session's identity, and a launch flag is only the default
/// for a session that hasn't got one.
#[tokio::test]
async fn a_model_pick_is_what_a_later_resume_restores() {
    // A pick is REMEMBERED (`apply_choice` → `record_last_model`), so it writes the
    // interactive last-used store — keep it away from the developer's real one.
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app
        .apply_model_choice_for_test("zen", "kimi-k2", Some(200_000));
    h.settle_switch().await;
    h.app.sync_panes();
    assert_eq!(
        h.app.state().model.to_string(),
        "zen://kimi-k2",
        "the pick is the session's identity"
    );

    // What the autosave writes: the identity in force, as picked.
    let saved = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        model: h.app.state().model.clone(),
        messages: vec![hrdr_agent::Message::system("sys")],
        ..Default::default()
    };

    // A LATER process, launched with a different `--model`, resumes it.
    let mut h2 = Harness::new(vec![]).await;
    h2.app.state_mut().model = "chatgpt://gpt-5.5".parse().unwrap();
    h2.app.auto_resume_state(saved, "old".to_string());
    h2.settle_switch().await;
    h2.app.sync_panes();
    assert_eq!(
        h2.app.state().model.to_string(),
        "zen://kimi-k2",
        "the pick came back — the launch flag is a new session's default, not a pin"
    );
}

/// A session whose provider isn't usable HERE (unknown, or its key is gone) is the one
/// case a resume cannot honour: the agent stays where it is — talking to an endpoint
/// that works — and says so. It never silently sends the conversation somewhere it
/// cannot go.
#[tokio::test]
async fn a_session_on_an_unusable_provider_stays_put_and_says_so() {
    let mut h = Harness::new(vec![]).await;
    let saved = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        model: "nowhere://ghost".parse().unwrap(),
        messages: vec![hrdr_agent::Message::system("sys")],
        ..Default::default()
    };
    h.app.auto_resume_state(saved, "old".to_string());

    assert!(
        h.app.transcript().iter().any(|e| matches!(
            &e.kind,
            EntryKind::Notice(t) | EntryKind::System(t)
                if t.contains("this session ran on provider 'nowhere'")
                && t.contains("staying on the current endpoint")
        )),
        "the failure is reported:\n{:?}",
        h.app
            .transcript()
            .iter()
            .map(|e| &e.kind)
            .collect::<Vec<_>>()
    );
    let provider = h.app.agent.lock().await.provider_name().to_string();
    assert_eq!(provider, "local", "the agent did not move");
}

/// A pre-`provider://model` session file names a model and NO provider. "This model"
/// means: on the provider in force — which, at a resume, is still the launch identity's.
#[tokio::test]
async fn a_legacy_session_lands_its_model_on_the_provider_in_force() {
    let mut h = Harness::new(vec![]).await;
    // As if `hrdr --model zen://kimi-k2` — the provider in force at the resume.
    h.app.state_mut().model = "zen://kimi-k2".parse().unwrap();

    let saved = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        model: "local://legacy-model".parse().unwrap(),
        provider_unset: true,
        messages: vec![hrdr_agent::Message::system("sys")],
        ..Default::default()
    };
    h.app.auto_resume_state(saved, "old".to_string());
    h.settle_switch().await;
    h.app.sync_panes();

    assert_eq!(
        h.app.state().model.to_string(),
        "zen://legacy-model",
        "the session's model, on the provider in force"
    );
}

/// A conversation's **provider is part of the conversation**: resuming one repoints
/// the agent to it, so the agent is talking to the provider the status bar names.
///
/// Regression: resume adopted the session's model name and provider label into the
/// display and told the agent only the model, leaving it pointed at the endpoint the
/// process launched with. A session saved on zen, resumed in a process whose config
/// defaults to OpenAI, showed `zen/deepseek-…` on the bar and sent the request to
/// api.openai.com — where that model does not exist and there is no key. The bar
/// said one thing; the socket did another.
#[tokio::test]
async fn resuming_a_session_repoints_the_agent_to_its_provider() {
    let mut h = Harness::new(vec![]).await;

    let saved = hrdr_app::SessionState {
        cwd: h.app.current_cwd(),
        model: "zen://deepseek-v4-flash".parse().unwrap(),
        messages: vec![hrdr_agent::Message::system("sys")],
        ..Default::default()
    };
    h.app.auto_resume_state(saved, "old".to_string());
    // The switch takes the agent lock, so it lands on its own task.
    for _ in 0..20 {
        if h.app
            .agent
            .try_lock()
            .is_ok_and(|a| a.provider_name() == "zen")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The agent itself — not a display copy — is on the session's provider and
    // model. It publishes its own chrome, so what the bar reads is what it is
    // pointed at, and the two cannot disagree.
    let (model, provider, base_url) = {
        let a = h.app.agent.lock().await;
        (
            a.model_name(),
            a.provider_name().to_string(),
            a.endpoint_base_url(),
        )
    };
    assert_eq!(
        model, "deepseek-v4-flash",
        "the agent runs the session's model"
    );
    assert_eq!(provider, "zen", "and is on the session's provider");
    assert!(
        base_url.contains("opencode.ai"),
        "and is pointed at that provider's endpoint, not the one it launched on: \
         {base_url}"
    );

    h.app.sync_panes();
    assert_eq!(
        h.app.state().model.to_string(),
        "zen://deepseek-v4-flash",
        "the bar names the identity the agent is actually talking to"
    );
}

/// The same rule with the launch identity coming from the config file (or a provider
/// preset's default) instead of a flag: a resumed session outranks it too.
#[tokio::test]
async fn a_config_default_yields_to_the_session_as_well() {
    for explicit_resume in [false, true] {
        let mut h = Harness::new(vec![]).await;
        h.app.state_mut().model = "local://from-config".parse().unwrap();

        let saved = hrdr_app::SessionState {
            cwd: h.app.current_cwd(),
            model: "zen://pro".parse().unwrap(),
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
            h.app.state().model.to_string(),
            "zen://pro",
            "session beats config, whole (explicit_resume={explicit_resume})"
        );
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
    let id = h.app.state().id.clone().expect("session saved");
    let cwd = h.app.current_cwd();

    let notices = |app: &App| {
        app.transcript()
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
        .transcript()
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
        h.app.state().name,
        "first message",
        "name derived from the message"
    );

    // Bare `/clear` starts an unnamed session.
    h.type_str("/clear");
    h.press(KeyCode::Enter);
    assert!(h.app.state().name.is_empty(), "no name yet");
    assert!(
        h.app.state().id.is_none(),
        "detached from the old session file"
    );

    // `/new <name>` — the alias — names it up front.
    h.type_str("/new Project X");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.state().name, "Project X");
    assert!(
        h.app.state().id.is_none(),
        "id assigned on first save, not now"
    );

    // The next turn's autosave writes it under that name, slugified.
    h.submit("second message").await;
    assert_eq!(
        h.app.state().name,
        "Project X",
        "the name survives the turn"
    );
    assert_eq!(
        h.app.state().id.as_deref(),
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
        .transcript()
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
        .transcript_mut()
        .push(Entry::reasoning("streaming thoughts"));
    h2.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
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
        .transcript_mut()
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
    let before = h.app.transcript().len();
    h.app
        .on_turn_msg(TurnMsg::Event(hrdr_agent::AgentEvent::Text(String::new())));
    assert_eq!(
        h.app.transcript().len(),
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
        .transcript_mut()
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
        .transcript_mut()
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
        app.transcript()
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
        .transcript_mut()
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
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // The auto-derived session name echoes the first message (so it carries the
    // literal `**`); it now shows in the status bar, but this test is about the
    // transcript, so clear it to keep the `*`-free assertion focused there.
    h.app.state_mut().name.clear();

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
        .transcript_mut()
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
        .transcript_mut()
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
        .transcript_mut()
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
        .transcript_mut()
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
        .transcript_mut()
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
        .transcript_mut()
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

/// Your place in a conversation, and the message you were half-way through typing
/// to it, belong to *that* conversation. Glancing at the main agent and coming
/// back must leave both exactly as they were.
#[tokio::test]
async fn switching_agents_keeps_each_ones_place_and_draft() {
    let mut h = Harness::new(vec![]).await;
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: 1,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "haiku".to_string(),
        provider: None,
        base_url: String::new(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Blocking,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });
    h.app.sync_panes();

    // Half-write a message to main, and scroll back through its transcript.
    h.type_str("a thought for main");
    h.app.scroll_offset = 12;

    // Go to the sub-agent: a different conversation, so a clean box and its own
    // place — not main's leftovers.
    h.app.focus_pane(hrdr_app::PaneId::Sub(1));
    assert_eq!(
        h.app.editor.content(),
        "",
        "the sub-agent's box starts empty"
    );
    assert_eq!(h.app.scroll_offset, 0);

    // Half-write something to the sub-agent, and scroll its transcript.
    h.type_str("wait, check auth");
    h.app.scroll_offset = 5;

    // Back to main: its draft and its place are exactly where we left them.
    h.app.focus_pane(hrdr_app::PaneId::Main);
    assert_eq!(h.app.editor.content(), "a thought for main");
    assert_eq!(h.app.scroll_offset, 12, "main's place is kept");

    // And back to the sub-agent: so are its.
    h.app.focus_pane(hrdr_app::PaneId::Sub(1));
    assert_eq!(
        h.app.editor.content(),
        "wait, check auth",
        "what you were typing to a sub-agent survives a glance at main"
    );
    assert_eq!(h.app.scroll_offset, 5, "and so does your place in it");
}

/// The input box talks to whichever agent is on screen. On a sub-agent's pane a
/// message steers *that* sub-agent — it goes into the very queue its `run` is
/// draining — and is shown in its transcript. The main agent's conversation is not
/// touched: a side-conversation stays on the side.
#[tokio::test]
async fn the_input_box_routes_to_the_focused_agent() {
    let mut h = Harness::new(vec![]).await;

    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    let steering = hrdr_agent::steering_queue();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: 1,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "haiku".to_string(),
        provider: None,
        base_url: String::new(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Blocking,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: steering.clone(),
        // Mid-turn: a message must be delivered as steering, not a new turn.
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });
    h.app.sync_panes();
    h.app.focus_pane(hrdr_app::PaneId::Sub(1));

    let main_before = h.app.transcript().len();
    h.submit("check the auth module too").await;

    // It reached the sub-agent's steering queue — the one its `run` drains.
    let steered: Vec<String> = steering
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.display.clone())
        .collect();
    assert_eq!(
        steered,
        vec!["check the auth module too".to_string()],
        "the message steers the agent being viewed"
    );

    // It shows in that agent's transcript when the agent *takes* it — the same rule
    // the main agent follows (`AgentEvent::Steered` is emitted as the message enters
    // the conversation, so the transcript's order matches the model's view). Here
    // that is the agent's own record; replay it as `run` would on its next round.
    h.app.live_subagents.record(
        1,
        &hrdr_agent::AgentEvent::Steered("check the auth module too".to_string()),
    );
    h.app.sync_panes();
    let sub_pane = h
        .app
        .panes
        .subs()
        .iter()
        .find(|p| p.id == hrdr_app::PaneId::Sub(1))
        .unwrap();
    assert!(
        sub_pane
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "check the auth module too")),
        "the sub-agent's transcript records what you said to it"
    );

    // …and nowhere near the main conversation.
    assert_eq!(
        h.app.transcript().len(),
        main_before,
        "a side-conversation does not enter the main agent's transcript"
    );
    assert!(!h.app.running(), "and it did not start a main-agent turn");
}

/// The agent list switches the view. It lists **main first** (so there is always a
/// way back) and then each live sub-agent; clicking a row makes that agent the one
/// on screen. The sub-agent's transcript is self-contained: it renders only while
/// that agent is active, and never bleeds into the parent's `task` block, which
/// records *what was delegated* rather than replaying the work.
#[tokio::test]
async fn the_agent_list_switches_the_focused_agent() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    h.app.state_mut().name = "my session".to_string();

    // With nothing delegated there is only the main agent, so no list at all.
    assert!(
        !h.app.panes.show_switcher(),
        "a fresh session shows no list"
    );

    // Delegate: the parent's `task` block, and a live sub-agent behind it.
    h.app
        .push_entry(Entry::tool_running("call-1", "task", "{}"));
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: 1,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "haiku".to_string(),
        provider: Some("claude".to_string()),
        base_url: String::new(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Blocking,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });

    // Its output arrives as ToolOutput on the `task` call that spawned it.
    // The sub-agent works. It records what it emits on its own entry — that record
    // is what its pane is built from, so it does not matter whether anyone was
    // watching (or even whether the pane existed) while it ran.
    h.app.live_subagents.record(
        1,
        &hrdr_agent::AgentEvent::Text("reading the codebase".to_string()),
    );
    // The parent also sees the blocking call's flattened output. It must be
    // dropped, not folded in twice.
    h.app.apply_event(hrdr_agent::AgentEvent::ToolOutput {
        id: "call-1".to_string(),
        chunk: "reading the codebase".to_string(),
    });

    let mut term = Terminal::new(TestBackend::new(70, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    let row_of = |screen: &str, needle: &str| -> Option<u16> {
        screen
            .lines()
            .position(|l| l.contains(needle))
            .map(|y| y as u16)
    };

    // The list appeared, main first, and the sub-agent is on it.
    assert!(h.app.panes.show_switcher(), "delegating brings the list up");
    let main_y = row_of(&screen, "· main").expect("main is listed");
    let sub_y = row_of(&screen, "explore").expect("the sub-agent is listed");
    assert!(
        main_y < sub_y,
        "main is first — it is the way back:\n{screen}"
    );

    // We are still on main, and the sub-agent's work is NOT in its transcript.
    assert!(h.app.panes.active().is_main());
    assert!(
        !screen.contains("reading the codebase"),
        "a sub-agent's output does not bleed into the parent's view:\n{screen}"
    );

    // Click the sub-agent's row: the view switches to it, and now its transcript
    // is what renders.
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: sub_y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    assert_eq!(h.app.panes.active(), hrdr_app::PaneId::Sub(1));

    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("reading the codebase"),
        "the sub-agent's own transcript renders when it is the active agent:\n{screen}"
    );
    assert_eq!(
        h.app
            .panes
            .subs()
            .iter()
            .flat_map(|p| p.transcript())
            .filter(|e| matches!(&e.kind, EntryKind::Assistant(s) if s == "reading the codebase"))
            .count(),
        1,
        "its work is folded in once — from its own record, not also from the \
         parent's flattened copy of it"
    );

    // Click main's row to come back.
    let main_y = row_of(&screen, "· main").expect("main is still listed");
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: main_y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    assert!(h.app.panes.active().is_main(), "main is always reachable");
}

/// The status bar describes **the agent you are looking at**: its model, its
/// provider, its context gauge, its tokens. A sub-agent runs on its own model
/// against its own window and bills its own tokens, so a bar that always reported
/// the main agent's figures was describing a conversation that wasn't on screen.
///
/// And because the bar reads the same state `/model` writes, `/model` on a
/// sub-agent's view switches *that* agent and the bar shows it — one piece of
/// state, not a display copy.
#[tokio::test]
async fn the_status_bar_and_model_command_follow_the_agent_on_screen() {
    // It picks a model below (`apply_model_choice_for_test` → `apply_choice`), which
    // records the last-used identity — never into the developer's real store.
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.state_mut().usage = hrdr_app::SessionUsage {
        tokens_in: 5_000,
        last_prompt_tokens: Some(5_000),
        last_completion_tokens: Some(10),
        context_window: Some(200_000),
        ..Default::default()
    };
    // The main agent is an entry in the registry like any other, and its pane is
    // rebuilt from that entry every frame. The *counters* are seeded here; what the
    // agent is running on is published by the agent itself — so the bar cannot show
    // a model the agent is not on.
    h.app.publish_main_agent();
    {
        let mut a = h.app.agent.lock().await;
        // One call: the model and the provider it is served by arrive together.
        a.set_model_ref("claude://opus".parse().unwrap()).unwrap();
        a.set_context_window(Some(200_000));
    }

    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: 1,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "qwen3".to_string(),
        provider: Some("local".to_string()),
        base_url: "http://127.0.0.1:8080/v1".to_string(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        // A small local window, most of it already used — nothing like the
        // parent's.
        usage: hrdr_agent::AgentUsage {
            tokens_in: 40_000,
            tokens_out: 2_000,
            last_prompt_tokens: Some(40_000),
            last_completion_tokens: Some(120),
            context_window: Some(64_000),
            cost_usd: 0.0,
        },
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Blocking,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: false,
        compacting: false,
        done: true,
        delivered: false,
        pinned: false,
    });

    let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("claude/opus") && screen.contains("200.0k"),
        "on main, the bar shows the main agent's model and window:\n{screen}"
    );

    // Switch to the sub-agent: the bar switches with it.
    h.app.focus_pane(hrdr_app::PaneId::Sub(1));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("local/qwen3"),
        "the bar shows the sub-agent's provider/model:\n{screen}"
    );
    assert!(
        screen.contains("64.0k") && !screen.contains("200.0k"),
        "and *its* context window, not the parent's:\n{screen}"
    );
    assert!(
        screen.contains("↑40.0k") && screen.contains("↓2.0k"),
        "and its own token counters:\n{screen}"
    );

    // Picking a model in `/model` now means "switch the model of *this* agent" —
    // the same path the picker's confirm takes.
    h.app
        .apply_model_choice_for_test("openai", "gpt-5", Some(400_000));
    h.settle_switch().await;
    assert_eq!(
        h.app.active_model_ref(),
        "openai://gpt-5".parse().unwrap(),
        "/model switched the agent on screen — provider and model together"
    );
    assert_eq!(
        h.app.live_subagents.with(|v| v
            .iter()
            .find(|e| e.key == 1)
            .map(|e| e.model.clone())
            .unwrap()),
        "gpt-5",
        "the switch lands on the registry — the pane is rebuilt from it every \
         frame, so a pane-only write would be silently undone"
    );
    assert_eq!(
        h.app.state().model.model(),
        "opus",
        "and the main agent is left alone"
    );

    // The bar follows it immediately, window and all.
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("openai/gpt-5") && screen.contains("400.0k"),
        "the bar shows the newly-chosen model and its window:\n{screen}"
    );

    // Coming back to main restores main's chrome.
    h.app.focus_pane(hrdr_app::PaneId::Main);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("claude/opus"),
        "back on main, the bar is main's again:\n{screen}"
    );
}

/// An **empty** turn — the kind a `!command`'s output or a finished background task
/// rides in on — carries no message of its own, so it must not mint a session named
/// after one.
///
/// Regression: `launch_turn("")` reserved a session id, which seeds the saved mirror
/// with `Message::user("")`. `is_saveable()` sees a user message and writes the file;
/// the name derives from that first message, which is blank — so running `!ls` as the
/// first thing in a fresh project left a `session.json` whose opening turn is empty.
#[tokio::test]
async fn an_empty_turn_does_not_mint_a_blank_session() {
    let mut h = Harness::new(vec![]).await;
    assert!(h.app.state().id.is_none(), "nothing saved yet");

    // What `finish_user_shell` does once a `!command` ends: the note is already in the
    // agent's history, and an empty turn hands it to the model.
    h.app.reserve_session_id("");
    assert!(
        h.app.state().id.is_none(),
        "an empty turn reserves no session"
    );
    assert!(
        h.app.state().messages.is_empty(),
        "and seeds no blank user message into the saved conversation"
    );

    // A real message still does, exactly as before.
    h.app.reserve_session_id("read the config");
    assert!(h.app.state().id.is_some(), "a real turn reserves one");
}

/// A detached sub-agent that finishes while nothing is running wakes the model:
/// an empty turn spawns, and `Agent::run` folds the result into the conversation
/// before its first request. The user never has to type to collect it.
#[tokio::test]
async fn a_finished_background_task_wakes_an_idle_model() {
    let mut h = Harness::new(vec![]).await;
    let task = |done: bool, delivered: bool| hrdr_tools::BackgroundTask {
        id: 1,
        tool_id: Some("call-1".into()),
        label: "explore".into(),
        log: "↳ task#1".into(),
        done,
        result: done.then(|| "found it".to_string()),
        delivered,
        ..Default::default()
    };

    // Still running: nothing to deliver.
    *h.app.background_tasks.lock().unwrap() = vec![task(false, false)];
    h.app.maybe_deliver_background();
    assert!(!h.app.running(), "an unfinished task doesn't wake anything");

    // Finished, but a turn is already in flight — it will drain at its next
    // request, so don't spawn a second turn on top of it.
    *h.app.background_tasks.lock().unwrap() = vec![task(true, false)];
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.maybe_deliver_background();
    h.app.live_subagents.end_turn(hrdr_agent::MAIN_KEY);

    // Already delivered: nothing to do (and no wake-up loop).
    *h.app.background_tasks.lock().unwrap() = vec![task(true, true)];
    h.app.maybe_deliver_background();
    assert!(!h.app.running(), "a delivered result doesn't wake anything");

    // Finished, undelivered, idle: the model is woken with an empty turn — no
    // user message of its own is added to the transcript.
    *h.app.background_tasks.lock().unwrap() = vec![task(true, false)];
    let before = h.app.transcript().len();
    h.app.maybe_deliver_background();
    assert!(h.app.running(), "the model was woken");
    assert_eq!(
        h.app.transcript().len(),
        before,
        "the wake-up turn adds no user message"
    );
}

/// A pending message renders as a tinted block, with a blank row between its
/// text and the `Queued` badge that closes it.
#[tokio::test]
async fn the_queued_badge_sits_below_a_blank_row() {
    let mut h = Harness::new(vec![]).await;
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("hold this thought");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.pending().len(), 1, "the message is pending");

    let mut term = Terminal::new(TestBackend::new(50, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    // Columns 0..49: the last one is the transcript's scrollbar, not content.
    let row = |y: u16| -> String {
        (0..49)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };

    let text_y = (0..24)
        .find(|&y| row(y).contains("hold this thought"))
        .expect("the pending message renders");
    let badge_y = (0..24)
        .find(|&y| row(y).contains("Queued"))
        .expect("the badge renders");

    assert_eq!(
        badge_y,
        text_y + 2,
        "one row between the text and the badge"
    );
    let gap = row(text_y + 1);
    assert_eq!(without_bar(&gap), "", "and it is blank:\n{screen}");
    // Inside the block, so it carries the block's own background.
    assert_eq!(
        buf.cell(Position::new(2, text_y + 1)).unwrap().bg,
        h.app.theme.user_bg,
        "the blank row is inside the block:\n{screen}"
    );
}

/// The todo panel wears the input pane's chrome — no border, the prompt's
/// background, two columns of padding either side and a blank row above and
/// below — differing only in the color of its left rule, which is green.
#[tokio::test]
async fn the_todo_panel_matches_the_input_pane_but_for_a_green_rule() {
    let mut h = Harness::new(vec![]).await;
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "ship it".to_string(),
        status: "in_progress".to_string(),
    }];

    let mut term = Terminal::new(TestBackend::new(50, 24)).unwrap();
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
    let cell = |x: u16, y: u16| buf.cell(Position::new(x, y)).unwrap().clone();

    let text_y = (0..24)
        .find(|&y| row(y).contains("ship it"))
        .expect("the todo renders");

    // No border glyphs, and no title, anywhere on the panel.
    for y in text_y - 1..=text_y + 1 {
        let r = row(y);
        for ch in ['┌', '┐', '└', '┘', '│', '─'] {
            assert!(!r.contains(ch), "border glyph {ch:?} on row {y}:\n{screen}");
        }
    }
    assert!(!screen.contains("todos"), "no title:\n{screen}");

    // The prompt's background across the full width, on the padding rows too.
    for x in [0, 2, 49] {
        for y in [text_y - 1, text_y, text_y + 1] {
            assert_eq!(cell(x, y).bg, h.app.theme.user_bg, "({x},{y}):\n{screen}");
        }
    }
    // One blank row above and below the content.
    assert_eq!(without_bar(&row(text_y - 1)), "", "top padding:\n{screen}");
    assert_eq!(
        without_bar(&row(text_y + 1)),
        "",
        "bottom padding:\n{screen}"
    );

    // The rule, then the rest of the left padding, then the content.
    assert!(
        row(text_y).starts_with(&format!("{} [~] ship it", crate::ui::BORDER_BAR)),
        "{screen}"
    );
    // Green, where the input pane's is the prompt's mauve.
    for y in text_y - 1..=text_y + 1 {
        assert_eq!(cell(0, y).symbol(), crate::ui::BORDER_BAR, "{screen}");
        assert_eq!(cell(0, y).fg, h.app.theme.success, "green rule:\n{screen}");
    }
    assert_ne!(
        h.app.theme.success, h.app.theme.prompt_border,
        "the two rules are told apart by color"
    );
}

/// Every panel above the input carries a blank row above itself, so it never
/// butts up against the scrollback (whose last block no longer trails one) — and
/// that row costs nothing when the panel isn't rendered.
#[tokio::test]
async fn each_panel_above_the_input_owns_a_blank_row_above_it() {
    let mut h = Harness::new(vec![]).await;
    // Overflow the transcript so its last block runs up to whatever is below.
    for i in 0..40 {
        h.app.push_entry(Entry::system(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(50, 24)).unwrap();
    let draw = |h: &mut Harness, term: &mut Terminal<TestBackend>| -> Vec<String> {
        term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
        let buf = term.backend().buffer();
        (0..24)
            .map(|y| {
                (0..50)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect()
            })
            .collect()
    };
    let tinted_at = |h: &Harness, term: &Terminal<TestBackend>, y: u16| -> bool {
        term.backend()
            .buffer()
            .cell(Position::new(2, y))
            .unwrap()
            .bg
            == h.app.theme.user_bg
    };

    // No panel: the transcript's filler runs right down to the input's own gap.
    let rows = draw(&mut h, &mut term);
    assert!(
        rows.iter().all(|r| !r.contains("ship it")),
        "no todo panel yet"
    );

    // With todos, the panel's first row is preceded by a blank, untinted row.
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "ship it".to_string(),
        status: "in_progress".to_string(),
    }];
    let rows = draw(&mut h, &mut term);
    let text_y = rows
        .iter()
        .position(|r| r.contains("ship it"))
        .expect("the todo renders") as u16;
    // text_y − 1 is the panel's tinted top pad; the row above it is the spacer.
    let spacer_y = text_y - 2;
    assert!(
        tinted_at(&h, &term, text_y - 1),
        "the panel's top pad:\n{}",
        rows.join("\n")
    );
    assert_eq!(
        rows[spacer_y as usize].trim(),
        "",
        "blank spacer above the panel:\n{}",
        rows.join("\n")
    );
    assert!(
        !tinted_at(&h, &term, spacer_y),
        "the spacer is not the panel's own padding:\n{}",
        rows.join("\n")
    );
    // The spacer is the layout's, not the panel's: dropping it would put the
    // panel's tinted pad directly under the transcript's last row.
    assert!(
        rows[spacer_y as usize - 1].contains("filler") || !tinted_at(&h, &term, spacer_y - 1),
        "the transcript, or its block's own bottom pad, sits above the spacer:\n{}",
        rows.join("\n")
    );
}

/// Exactly one untinted row separates the scrollback from the input pane, even
/// when a tinted block (a user prompt) runs right up to the bottom of it.
///
/// The blank belongs to the layout, not to the block: `flush` no longer trails a
/// separator after the last block, so two tinted surfaces can't merge into one
/// slab and an untinted one can't leave a two-row hole.
#[tokio::test]
async fn one_blank_row_separates_the_scrollback_from_the_input() {
    for last_is_tinted in [true, false] {
        let mut h = Harness::new(vec![]).await;
        // Overflow the transcript so its final block reaches the input pane.
        for i in 0..40 {
            h.app.push_entry(Entry::system(format!("filler {i}")));
        }
        if last_is_tinted {
            h.app.push_entry(Entry::user("prompt"));
        } else {
            h.app.push_entry(Entry::assistant("output"));
        }
        h.type_str("draft");

        let mut term = Terminal::new(TestBackend::new(50, 20)).unwrap();
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
        // Column 2 is inside every block's padding, past the `┃` bar at column 0.
        let bg_at = |y: u16| buf.cell(Position::new(2, y)).unwrap().bg;

        let draft_y = (0..20)
            .find(|&y| row(y).contains("draft"))
            .expect("the draft renders");
        // The pane's own top pad is tinted; above it must sit exactly one blank,
        // untinted row, and above that the transcript's last row.
        let gap_y = draft_y - 2;
        assert_eq!(
            bg_at(draft_y - 1),
            h.app.theme.user_bg,
            "the input's top pad ({last_is_tinted}):\n{screen}"
        );
        assert_eq!(
            row(gap_y).trim(),
            "",
            "the gap row is blank ({last_is_tinted}):\n{screen}"
        );
        assert_eq!(
            bg_at(gap_y),
            Color::Reset,
            "the gap row is untinted ({last_is_tinted}):\n{screen}"
        );
        // And it is the *only* one: the transcript's last row is the block's own
        // bottom pad, tinted when the block is.
        let want = if last_is_tinted {
            h.app.theme.user_bg
        } else {
            Color::Reset
        };
        assert_eq!(
            bg_at(gap_y - 1),
            want,
            "the transcript's last row is the block's bottom pad ({last_is_tinted}):\n{screen}"
        );
    }
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

/// The loader tracks the *model*, not the turn: it hides while the model's tool
/// calls run, because the model is idle then — and its clock stops with it, so a
/// slow tool doesn't inflate the turn's reported inference time.
#[tokio::test]
async fn the_loader_stops_while_the_models_tools_run() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![]).await;
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.resume_inference_for_test();
    // The clock is the *agent's*, kept on its registry entry — the main agent's is
    // read exactly the way a sub-agent's is.
    let turn = |h: &Harness| {
        h.app
            .live_subagents
            .turn(hrdr_agent::MAIN_KEY)
            .expect("the session's agent is in the registry")
    };
    assert!(turn(&h).inferring(), "the model works as the turn opens");

    // A tool round opens: the model handed off and is now idle.
    h.app.on_turn_msg(TurnMsg::Event(AgentEvent::ToolStart {
        id: "a".into(),
        name: "bash".into(),
        args: "{}".into(),
    }));
    h.app.on_turn_msg(TurnMsg::Event(AgentEvent::ToolStart {
        id: "b".into(),
        name: "bash".into(),
        args: "{}".into(),
    }));
    assert!(!turn(&h).inferring(), "idle while its tools run");
    let frozen = turn(&h).infer_elapsed();

    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("inferring") && !screen.contains("generating"),
        "no loader while tools run:\n{screen}"
    );
    // The clock is frozen: the banked time doesn't advance across a pause.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(turn(&h).infer_elapsed(), frozen, "the clock stopped");

    // One of two tools returning is not enough — the model is still waiting.
    let end = |id: &str| {
        TurnMsg::Event(AgentEvent::ToolEnd {
            id: id.into(),
            name: "bash".into(),
            result: "ok".into(),
            ok: true,
        })
    };
    h.app.on_turn_msg(end("a"));
    assert!(
        !turn(&h).inferring(),
        "one tool of two is still outstanding"
    );

    // The last one hands control back: the model works again, and the clock runs.
    h.app.on_turn_msg(end("b"));
    assert!(turn(&h).inferring(), "the model resumed");
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(turn(&h).infer_elapsed() > frozen, "the clock restarted");

    // The turn ends: the model stops, whatever was in flight.
    h.app.on_turn_msg(TurnMsg::Done(None));
    assert!(!turn(&h).inferring(), "the turn is over");
}

/// The loader belongs to **the agent on screen**. A turn is per agent, so its
/// clock is per agent: a sub-agent working shows *its* loader, and the main agent
/// working while you read a sub-agent shows none.
///
/// Regression: the loader was driven by the main agent's clock whichever agent was
/// being viewed — so a sub-agent's pane claimed to be "generating" the main agent's
/// tokens, and a sub-agent grinding away under an idle main agent showed nothing.
#[tokio::test]
async fn the_loader_belongs_to_the_agent_on_screen() {
    let mut h = Harness::new(vec![]).await;
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: 1,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "haiku".to_string(),
        provider: None,
        base_url: String::new(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Background,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // The sub-agent is working; the main agent is idle. On main: no loader — it is
    // not the main agent that is busy.
    h.app.live_subagents.begin_turn(1);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("inferring") && !screen.contains("generating"),
        "the main agent is idle, so its view shows no loader:\n{screen}"
    );

    // Switch to the sub-agent: the loader is there, running *its* clock.
    h.app.focus_pane(hrdr_app::PaneId::Sub(1));
    h.app
        .live_subagents
        .record(1, &hrdr_agent::AgentEvent::Text("looking".into()));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("generating"),
        "the agent on screen is working, so its loader shows:\n{screen}"
    );

    // Its tool runs: the model is idle, so the loader hides — its own pane, its own
    // clock.
    h.app.live_subagents.record(
        1,
        &hrdr_agent::AgentEvent::ToolStart {
            id: "t1".into(),
            name: "grep".into(),
            args: "{}".into(),
        },
    );
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("inferring") && !screen.contains("generating"),
        "no loader while the agent's own tool runs:\n{screen}"
    );
}

/// The loader heads the input area while a turn runs: above every panel, with a
/// blank row on each side. Those blanks are the shared per-section spacer — the
/// loader's own above it, the next section's below it.
#[tokio::test]
async fn the_generating_line_heads_the_input_area_with_a_blank_row_each_side() {
    let mut h = Harness::new(vec![]).await;
    h.type_str("draft");
    // A panel between the loader and the input, so "top-most" is a real claim.
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "ship it".to_string(),
        status: "in_progress".to_string(),
    }];
    h.app.live_subagents.begin_turn(hrdr_agent::MAIN_KEY);
    // The loader tracks the *model* working, not merely a turn being in flight.
    h.app.resume_inference_for_test();

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
    let cell = |y: u16| buf.cell(Position::new(2, y)).unwrap().clone();
    let find = |needle: &str| {
        (0..24)
            .find(|&y| row(y).contains(needle))
            .unwrap_or_else(|| panic!("no {needle} row:\n{screen}"))
    };

    let loader_y = find("inferring");
    // Top-most: the todo panel and the input pane both sit below it.
    assert!(loader_y < find("ship it"), "above the todos:\n{screen}");
    assert!(loader_y < find("draft"), "above the input:\n{screen}");

    // A blank, untinted row on each side of it.
    for y in [loader_y - 1, loader_y + 1] {
        assert_eq!(row(y).trim(), "", "blank row at {y}:\n{screen}");
        assert_eq!(cell(y).bg, Color::Reset, "untinted row at {y}:\n{screen}");
    }
    // And exactly one below: the todo panel's tinted top pad follows it.
    assert_eq!(
        cell(loader_y + 2).bg,
        h.app.theme.user_bg,
        "the next section starts one row below:\n{screen}"
    );
}

/// The user's own surfaces — the prompt block and the input pane — wear a bar
/// down their left edge, running their whole height. A tool call shares the
/// prompt's background but not its bar; it isn't the user speaking.
#[tokio::test]
async fn the_prompt_and_input_wear_a_left_bar() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
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
    // One row, so the gauge label this test keys off can't be split across a
    // wrap. Where the bar wraps depends on the section widths, which vary with
    // the platform's temp paths — the padding under test does not.
    h.app.statusbar_mode = hrdr_app::StatusBarMode::Truncate;
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
        .transcript()
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

// ---------------------------------------------------------------------------
// Autosave on quit/cancel mid-turn
// ---------------------------------------------------------------------------

/// Pump the turn channel until a streamed chunk of assistant text has landed
/// in the transcript, then stop — proof that the agent already pushed the
/// user message into its own history (that happens synchronously, before any
/// network I/O) and that a partial reply is now visible, without waiting for
/// the turn to actually finish.
async fn pump_until_partial_reply(h: &mut Harness) {
    loop {
        let msg = h.rx.recv().await.expect("the mock server always replies");
        h.app.on_turn_msg(msg);
        if h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Assistant(t) if t.contains("partial")))
        {
            return;
        }
    }
}

/// Cancelling a running turn (Ctrl+C, Esc) autosaves immediately: the user's
/// message and whatever partial reply had streamed in before the cancel must
/// reach disk, since no `Done` will ever arrive to trigger the usual
/// end-of-turn autosave.
///
/// Regression: `cancel_turn` cleared the turn state and dropped the queue but
/// never called `autosave` — a turn cancelled mid-stream vanished from the
/// session file entirely.
#[tokio::test]
async fn cancelling_a_turn_autosaves_the_in_progress_transcript() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::MultiChunk(vec![
        "partial ".into(),
        "reply".into(),
    ])])
    .await;

    h.type_str("investigate the bug");
    h.press(KeyCode::Enter);
    assert!(h.app.running(), "the turn is in flight");
    pump_until_partial_reply(&mut h).await;

    h.app.cancel_turn();
    assert!(!h.app.running(), "cancelled");
    // `cancel_turn`'s save is best-effort (it `try_lock`s and skips while the
    // just-aborted turn task still holds the agent lock). Await the reap to
    // release it, then save — the deterministic equivalent of the catch-up save
    // a later checkpoint performs.
    h.app.reap_cancelled_turn().await;
    h.app.autosave();

    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("cancel_turn autosaved and assigned a session id");
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id).expect("session file written");
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "investigate the bug")),
        "the user's message survives the cancel"
    );
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Assistant(t) if t.contains("partial"))),
        "the partial reply survives the cancel"
    );
}

/// Quitting mid-turn (Ctrl+Q, double Ctrl+C, Ctrl+D on empty input, `/exit`)
/// must not lose the in-progress transcript either: `App::request_quit`
/// cancels the running turn first (which autosaves) before arming
/// `should_quit`.
///
/// Regression: every quit path set `should_quit` directly, so the running
/// turn's background task — and the visible message + partial reply it
/// carried — was simply abandoned, and nothing ever autosaved it.
#[tokio::test]
async fn quitting_mid_turn_autosaves_the_in_progress_transcript() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::MultiChunk(vec![
        "partial ".into(),
        "reply".into(),
    ])])
    .await;

    h.type_str("investigate the bug");
    h.press(KeyCode::Enter);
    pump_until_partial_reply(&mut h).await;

    // Ctrl+Q: an immediate, deliberate quit while a turn is running.
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(h.app.should_quit, "Ctrl+Q arms the quit");
    assert!(!h.app.running(), "the in-flight turn was cancelled first");

    // Finish the quit the way the run loop does: await the aborted turn (which
    // releases the agent lock) then run the final autosave. Without this, the
    // reap-then-save the loop performs on `should_quit` never happens, and the
    // best-effort save in `cancel_turn` skips while the lock is still held.
    h.app.reap_cancelled_turn().await;
    h.app.autosave();

    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("quitting mid-turn autosaved and assigned a session id");
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id).expect("session file written");
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "investigate the bug")),
        "the user's message survives the quit"
    );
    assert!(
        loaded
            .state
            .transcript
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Assistant(t) if t.contains("partial"))),
        "the partial reply survives the quit"
    );
}

/// The scrollback cap evicts the oldest *conversation* entries but must never
/// touch the intro block (`Entry::header()` + the welcome/config `Notice`s
/// pushed in `App::new`) — that banner should survive no matter how long the
/// session runs.
///
/// Regression: `prune_scrollback` counted leading `EntryKind::System`
/// entries as the protected head, but the intro is `Header` + `Notice`, so
/// `head` was always 0 and the welcome banner was the very first thing
/// evicted once the transcript grew past the cap.
#[tokio::test]
async fn pruning_keeps_the_header_banner_not_a_leading_system_entry() {
    let mut h = Harness::new(vec![]).await;
    h.app.scrollback = 5;
    assert!(
        matches!(h.app.transcript()[0].kind, EntryKind::Header),
        "the header opens every session"
    );

    for i in 0..20 {
        h.app.push_entry(Entry::system(format!("entry {i}")));
    }

    assert!(
        matches!(h.app.transcript()[0].kind, EntryKind::Header),
        "the header banner must survive pruning: {:?}",
        h.app.transcript()[0].kind
    );
    assert!(
        h.app.transcript().len() <= 5,
        "the scrollback cap is enforced"
    );
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::System(s) if s == "entry 0")),
        "the oldest conversation entry was evicted"
    );
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::System(s) if s == "entry 19")),
        "the newest conversation entry is kept"
    );
}

/// In vim mode, Ctrl+D on an empty input line quits — as the welcome banner
/// advertises ("Ctrl+D on an empty line") — even in Normal mode, where Ctrl+D
/// would otherwise scroll the transcript half a page. With a non-empty draft,
/// Normal-mode Ctrl+D still scrolls as before.
///
/// Regression: the Normal-mode scroll arm for Ctrl+D was checked before the
/// empty-input quit arm, so Normal mode always won and the advertised
/// "Ctrl+D on an empty line" quit never fired there.
#[tokio::test]
async fn vim_normal_mode_ctrl_d_quits_only_on_empty_input() {
    let mut h = Harness::new(vec![]).await;
    h.app.editor = Box::new(hrdr_editor::VimEngine::new());
    assert_eq!(
        h.app.editor.mode_label(),
        "NORMAL",
        "vim starts in Normal mode"
    );

    // Non-empty draft: Normal-mode Ctrl+D scrolls (down — it *decreases* the
    // from-bottom offset), same as always. Start scrolled up so the decrease
    // is observable.
    h.app.editor.set_content("a draft in progress");
    h.app.transcript_height = 20;
    h.app.scroll_offset = 10;
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(!h.app.should_quit, "a non-empty draft must not quit");
    assert!(h.app.scroll_offset < 10, "Normal-mode Ctrl+D still scrolls");

    // Empty input: Ctrl+D quits, matching the welcome banner.
    h.app.editor.set_content("");
    h.app.scroll_offset = 0;
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(
        h.app.should_quit,
        "Ctrl+D on an empty line must quit even in Normal mode"
    );
}

/// A mistyped slash command (`/exprot`) is caught and reported instead of
/// silently becoming a full model turn — it's shaped like an attempted
/// command (a single leading `/word` token, letters/digits/hyphens only),
/// just not a registered one.
///
/// Regression: `handle_slash` returning `false` for an unrecognized command
/// fell straight through to `spawn_turn`, so a typo silently became a chat
/// message sent to the model.
#[tokio::test]
async fn an_unrecognized_slash_command_is_reported_not_sent_to_the_model() {
    let mut h = Harness::new(vec![]).await;

    h.type_str("/exprot");
    h.press(KeyCode::Enter);

    assert!(!h.app.running(), "no turn should have been spawned");
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(_))),
        "the typo must not enter the conversation as a user message"
    );
    let screen = h.render();
    assert!(
        screen.contains("unknown command"),
        "should report the typo:\n{screen}"
    );
}

/// A message that merely starts with `/` but isn't command-shaped (a real
/// path, with a further `/` in it) still goes to the model as usual — the
/// unknown-command guard must not swallow legitimate messages.
#[tokio::test]
async fn a_path_like_message_starting_with_slash_still_sends() {
    let mut h = Harness::new(vec![MockReply::Text("looks fine to me".into())]).await;
    h.submit("/etc/hosts looks wrong, can you check?").await;

    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t.starts_with("/etc/hosts"))),
        "a path-shaped message should be sent as a normal chat message"
    );
}

/// The `/login` modal drives the whole flow: a provider picker first (same
/// chrome as the other pickers), then a masked key field for a key-based
/// provider. The raw key never renders and never touches the input editor,
/// and Esc cancels without saving.
#[tokio::test]
async fn login_modal_flow_masks_the_key_entry() {
    let mut h = Harness::new(vec![]).await;
    h.submit("/login").await;
    assert!(
        matches!(
            h.app.login_modal,
            Some(crate::app::LoginModal::Providers(_))
        ),
        "/login opens the provider picker"
    );
    let screen = h.render();
    assert!(screen.contains("OpenAI"), "providers listed:\n{screen}");
    assert!(
        screen.contains("browser login"),
        "auth-method column:\n{screen}"
    );

    // Narrow to OpenAI (remote, key-based) and continue → the key phase.
    h.type_str("openai");
    h.press(KeyCode::Enter);
    assert!(
        matches!(h.app.login_modal, Some(crate::app::LoginModal::Key { .. })),
        "a key-based provider advances to the key field"
    );
    let screen = h.render();
    assert!(
        screen.contains("PLAINTEXT"),
        "the storage warning shows:\n{screen}"
    );

    h.type_str("sk-super-secret-value");
    let screen = h.render();
    assert!(
        !screen.contains("sk-super-secret"),
        "the raw key must never render:\n{screen}"
    );
    assert!(
        screen.contains('•'),
        "masked bullets render in its place:\n{screen}"
    );
    assert!(
        h.app.editor.content().is_empty(),
        "the key bypasses the input editor entirely"
    );

    // Esc cancels without saving anything.
    h.press(KeyCode::Esc);
    assert!(h.app.login_modal.is_none(), "Esc closes the modal");
}

/// A browser login's late result is applied only when its `login_id` matches the
/// current `Authorizing` pending state — a stale/duplicate login is ignored.
#[tokio::test]
async fn browser_login_ignores_a_stale_login_id() {
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 2,
        provider: "chatgpt".to_string(),
        label: "ChatGPT".to_string(),
    });
    // A late result from an older login (id 1) must not disturb id 2.
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 1,
        provider: "chatgpt".to_string(),
        token_saved: true,
        error: None,
    });
    assert!(
        matches!(
            h.app.login_modal,
            Some(crate::app::LoginModal::Authorizing { login_id: 2, .. })
        ),
        "a stale login result must leave the current pending login intact"
    );
}

/// Esc abandons an in-flight browser login; a later result for it is then
/// dropped (no matching `Authorizing`).
#[tokio::test]
async fn browser_login_esc_cancels_then_late_result_is_dropped() {
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 1,
        provider: "chatgpt".to_string(),
        label: "ChatGPT".to_string(),
    });
    // A long-lived task stands in for the real callback/exchange future.
    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    h.app.browser_login_task = Some(handle);
    h.press(KeyCode::Esc);
    assert!(
        h.app.login_modal.is_none(),
        "Esc abandons the pending login"
    );
    assert!(
        h.app.browser_login_task.is_none(),
        "Esc aborts + drops the in-flight login task (freeing the callback port)"
    );
    // The in-flight task's late result now matches nothing → no-op.
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 1,
        provider: "chatgpt".to_string(),
        token_saved: true,
        error: None,
    });
    assert!(
        h.app.login_modal.is_none(),
        "a cancelled login's late result does nothing"
    );
}

/// A matching, successful browser login runs the switch transaction: the modal
/// closes and the live provider is switched.
#[tokio::test]
async fn browser_login_success_switches_provider() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 7,
        provider: "chatgpt".to_string(),
        label: "ChatGPT".to_string(),
    });
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 7,
        provider: "chatgpt".to_string(),
        token_saved: true,
        error: None,
    });
    h.settle_switch().await;
    assert!(
        h.app.login_modal.is_none(),
        "the switch transaction closed the modal"
    );
    assert_eq!(
        h.app.state().model.provider().as_str(),
        "chatgpt",
        "the live provider switched to ChatGPT"
    );
}

/// A failed (matching) browser login reports the error and closes the modal
/// without switching.
#[tokio::test]
async fn browser_login_failure_reports_and_closes() {
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 3,
        provider: "chatgpt".to_string(),
        label: "ChatGPT".to_string(),
    });
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 3,
        provider: "chatgpt".to_string(),
        token_saved: false,
        error: Some("authorization was rejected".to_string()),
    });
    assert!(
        h.app.login_modal.is_none(),
        "a failed login closes the modal"
    );
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Notice(t) if t.contains("login failed"))),
        "the failure is reported to the user"
    );
}

/// A catalog load from a superseded generation (picker closed/reopened or
/// provider changed since it began) must not touch the current picker.
#[tokio::test]
async fn model_catalog_stale_generation_is_dropped() {
    let mut h = Harness::new(vec![]).await;
    h.app.model_gen = 5;
    h.app.model_selector = Some(crate::app::model_selector(vec![]));
    h.app.model_loading = true;
    h.app.apply_catalog_result(
        4, // an older generation
        vec![hrdr_agent::ChatGptModel {
            slug: "gpt-5.5".to_string(),
            label: "GPT-5.5".to_string(),
            context_window: Some(400_000),
        }],
        hrdr_agent::CatalogSource::Fresh,
        None,
    );
    assert!(
        h.app.model_loading,
        "a stale result leaves loading untouched"
    );
    assert!(
        h.app.model_source.is_none(),
        "a stale result sets no source"
    );
}

/// A matching-generation catalog load merges the entitled rows into the open
/// picker and records the source.
#[tokio::test]
async fn model_catalog_matching_generation_merges_rows() {
    let mut h = Harness::new(vec![]).await;
    h.app.model_gen = 7;
    h.app.model_selector = Some(crate::app::model_selector(vec![]));
    h.app.model_loading = true;
    h.app.apply_catalog_result(
        7,
        vec![hrdr_agent::ChatGptModel {
            slug: "gpt-5.5".to_string(),
            label: "GPT-5.5".to_string(),
            context_window: Some(400_000),
        }],
        hrdr_agent::CatalogSource::Fresh,
        None,
    );
    assert!(!h.app.model_loading, "loading cleared on a matching result");
    assert_eq!(h.app.model_source, Some(hrdr_agent::CatalogSource::Fresh));
    let sel = h.app.model_selector.as_ref().unwrap();
    assert!(
        sel.rows()
            .any(|c| c.provider == "chatgpt" && c.model == "gpt-5.5"),
        "the entitled ChatGPT row is merged into the picker"
    );
}

/// `!command` runs the shell directly: the output streams into a transcript
/// tool block, and on ToolEnd the command + output are committed through the
/// same plumbing as a finished turn — the user note enters the agent's
/// history synchronously and an autosave writes the session, so nothing rides
/// a later turn's save. No model turn is spawned. Unix-only: the Windows
/// runners' `bash`/`pwsh` mix isn't predictable enough to assert output
/// verbatim.
#[cfg(unix)]
#[tokio::test]
async fn bang_runs_a_user_shell_command_and_records_it() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.type_str("!echo hello-from-shell");
    h.press(KeyCode::Enter);
    assert!(!h.app.running(), "no model turn spawns for a !command");
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Tool { .. })),
        "the tool block opened synchronously: {:?}",
        h.app
            .transcript()
            .iter()
            .map(|e| &e.kind)
            .collect::<Vec<_>>()
    );

    // Drain the events the spawned shell task sends until the block closes.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !h
        .app
        .transcript()
        .iter()
        .any(|e| matches!(&e.kind, EntryKind::Tool { done: true, .. }))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "shell events never arrived"
        );
        match tokio::time::timeout(std::time::Duration::from_secs(5), h.rx.recv()).await {
            Ok(Some(msg)) => h.app.on_turn_msg(msg),
            Ok(None) => panic!("channel closed before the shell finished"),
            Err(_) => panic!("timed out waiting for shell events"),
        }
    }
    let screen = h.render();
    assert!(
        screen.contains("hello-from-shell"),
        "output in the transcript:\n{screen}"
    );

    // ToolEnd committed the note synchronously — same plumbing as a turn.
    let noted = h.app.agent.try_lock().is_ok_and(|a| {
        a.messages_owned().iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("hello-from-shell") && c.contains("I ran"))
        })
    });
    assert!(noted, "the history note landed with ToolEnd");

    // …and autosaved: the session file already carries the note and the
    // closed tool block, not "whenever the next turn saves".
    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("the !command's autosave assigned a session id");
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id)
        .expect("session file written on ToolEnd");
    assert!(
        loaded.state.messages.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("hello-from-shell") && c.contains("I ran"))
        }),
        "the note persisted"
    );
    assert!(
        loaded.state.transcript.iter().any(|e| {
            matches!(&e.kind, EntryKind::Tool { done: true, result, .. }
                if result.contains("hello-from-shell"))
        }),
        "the tool block persisted"
    );
}

/// Esc cancels a running `!command`: the child is killed, the tool block
/// closes as "(cancelled)", the cancellation note commits to history + disk
/// like any other transcript entry, and the slot frees for the next command.
#[cfg(unix)]
#[tokio::test]
async fn esc_cancels_a_running_user_shell_command() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.type_str("!sleep 30");
    h.press(KeyCode::Enter);
    assert!(h.app.user_shell.is_some(), "the shell task is tracked");

    h.press(KeyCode::Esc);
    assert!(h.app.user_shell.is_none(), "Esc cleared the slot");
    let noted = h.app.agent.try_lock().is_ok_and(|a| {
        a.messages_owned().iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("cancelled"))
        })
    });
    assert!(noted, "the cancellation note landed with the cancel");
    assert!(
        h.app.state().id.is_some(),
        "the cancel autosaved the session"
    );
    let cancelled = h.app.transcript().iter().any(|e| {
        matches!(&e.kind, EntryKind::Tool { done: true, ok: false, result, .. }
            if result.contains("cancelled"))
    });
    assert!(cancelled, "the tool block closed as cancelled");

    // The slot is free: a new command runs fine.
    h.type_str("!echo after-cancel");
    h.press(KeyCode::Enter);
    assert!(h.app.user_shell.is_some(), "a new command is accepted");
}

/// `/skills` opens a picker of the discovered skills; Enter inserts the
/// `:name ` invocation into the input and hands the cursor back.
#[tokio::test]
async fn skills_picker_inserts_the_invocation() {
    let mut h = Harness::new(vec![]).await;
    let dir = std::path::PathBuf::from(h.app.current_cwd()).join(".hrdr/skills");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ship.md"),
        "---\ndescription: release checklist\n---\nGo.",
    )
    .unwrap();

    h.submit("/skills").await;
    assert!(h.app.skill_selector.is_some(), "/skills opens the picker");
    let screen = h.render();
    assert!(screen.contains(":ship"), "skill listed:\n{screen}");
    assert!(
        screen.contains("release checklist"),
        "description column:\n{screen}"
    );

    h.press(KeyCode::Enter);
    assert!(h.app.skill_selector.is_none(), "Enter closes the picker");
    assert_eq!(
        h.app.editor.content(),
        ":ship ",
        "the invocation lands in the input, ready for arguments"
    );
}

/// A transcript whose cumulative wrapped-row count exceeds `u16::MAX` must not
/// have its scroll math wrap around: `draw_transcript` keeps that accumulator
/// in `usize` to avoid overflow, but the cast down to ratatui's `u16`-only
/// scroll type has to saturate, not truncate.
///
/// Regression: `let total = *cum.last().unwrap_or(&0) as u16;` (and the other
/// cast sites) truncated instead of clamping, so a transcript taller than
/// 65535 rows wrapped `max_scroll` back down to a small, unrelated number —
/// snapping the scrollbar near the top of a long session instead of pinning
/// it near the bottom.
#[tokio::test]
async fn a_transcript_taller_than_u16_max_rows_does_not_wrap_the_scroll_math() {
    let mut h = Harness::new(vec![]).await;
    // A high cap so the transcript really does grow past 65535 rows instead
    // of being pruned back down.
    h.app.scrollback = 1_000_000;
    for i in 0..40_000 {
        h.app.push_entry(Entry::system(format!("line {i}")));
    }

    h.render(); // drives draw_transcript, which recomputes app.max_scroll

    assert!(
        h.app.max_scroll > 60_000,
        "max_scroll should saturate near u16::MAX for a transcript this tall, got {}",
        h.app.max_scroll
    );
}

/// A block whose entry has not changed is **reused**, not rebuilt — and a block
/// whose entry *has* changed is rebuilt.
///
/// This is the whole reason a long session stays responsive. A frame used to cost
/// the entire transcript: every entry's rows were re-cloned, re-measured, and then
/// handed to a `Paragraph` that re-wrapped the lot from the top and threw away
/// everything above the scroll. At a thousand entries that was ~26ms per frame —
/// and a frame is drawn on every keystroke — while past the old cache's 1024-entry
/// cap it collapsed to ~120ms, because each frame evicted what the next one needed.
///
/// Now each block is laid out once and shared by `Rc`, so a frame that changes
/// nothing hands the same rows back. Pointer identity is the proof: same pointer,
/// no re-render.
#[tokio::test]
async fn an_unchanged_block_is_reused_not_rerendered() {
    let mut h = Harness::new(vec![]).await;
    for i in 0..50 {
        h.app.push_entry(Entry::user(format!("message {i}")));
    }
    h.render();
    let first: Vec<Option<usize>> = (1..=50).map(crate::ui::block_cache_ptr).collect();
    assert!(
        first.iter().all(Option::is_some),
        "every entry should have been laid out once"
    );

    // A frame that changes nothing must not lay anything out again.
    h.render();
    let second: Vec<Option<usize>> = (1..=50).map(crate::ui::block_cache_ptr).collect();
    assert_eq!(first, second, "an idle frame must reuse every block");

    // Growing one entry — what streaming does, a token at a time — rebuilds that
    // block and leaves every other one alone.
    if let Some(EntryKind::User(text)) = h
        .app
        .panes
        .main_mut()
        .transcript_mut()
        .get_mut(10)
        .map(|e| &mut e.kind)
    {
        text.push_str(" and more");
    }
    h.app
        .panes
        .main_mut()
        .transcript_mut()
        .get_mut(10)
        .unwrap()
        .refresh_hash();
    h.render();
    let third: Vec<Option<usize>> = (1..=50).map(crate::ui::block_cache_ptr).collect();
    assert_ne!(
        second[9], third[9],
        "the entry that changed must be laid out again"
    );
    for (i, (before, after)) in second.iter().zip(&third).enumerate() {
        if i != 9 {
            assert_eq!(before, after, "entry {} was rebuilt for nothing", i + 1);
        }
    }
}

/// A command handed to hrdr on the command line does exactly what typing it does.
///
/// `hrdr /new`, `hrdr /model`, `hrdr '!git status'`, `hrdr ':skill …'` — all of it
/// goes through `submit_input`, the same function `Enter` calls, so the two can't
/// drift: a command the input box learns, the command line gets for free. What is
/// checked here is that each *kind* of input is still told apart when it arrives
/// this way — a slash command runs locally instead of being sent to the model, a
/// plain message starts a turn.
#[tokio::test]
async fn a_command_line_command_runs_the_same_path_as_typing_it() {
    // A slash command runs locally: it does its work in the session, and nothing
    // is sent to the model.
    let mut h = Harness::new(vec![]).await;
    h.app.submit_input("/help".to_string());
    let printed: String = h
        .app
        .transcript()
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::System(t) | EntryKind::Notice(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        printed.contains("/model"),
        "`hrdr /help` should print the command list, as typing it does: {printed}"
    );
    assert!(!h.app.running(), "a slash command must not start a turn");

    // A plain message opens the session with a turn to the model.
    let mut h = Harness::new(vec![MockReply::Text("on it".to_string())]).await;
    h.app.submit_input("fix the failing test".to_string());
    h.pump().await;
    let out = h.render();
    assert!(out.contains("fix the failing test"), "the message is shown");
    assert!(out.contains("on it"), "and the model answered it: {out}");

    // The input box is left empty either way — the command was consumed, not
    // dropped into the draft for the user to press Enter on themselves.
    assert_eq!(h.app.editor.content(), "");
}

/// The `/model` selector renders both columns (friendly model · provider),
/// narrows as you type into its fuzzy filter, and closes on Esc.
#[tokio::test]
async fn model_selector_renders_columns_filters_and_closes() {
    let mut h = Harness::new(vec![]).await;
    let choices = vec![
        hrdr_agent::ModelChoice {
            provider: "zen".into(),
            model: "claude-fable-5".into(),
            provider_label: "OpenCode Zen".into(),
            model_label: "Claude Fable 5.0".into(),
            context_window: None,
        },
        hrdr_agent::ModelChoice {
            provider: "go".into(),
            model: "deepseek-v4-pro".into(),
            provider_label: "OpenCode Go".into(),
            model_label: "DeepSeek V4 Pro".into(),
            context_window: None,
        },
    ];
    h.app.model_selector = Some(crate::app::model_selector(choices));

    let screen = h.render();
    assert!(screen.contains("Search"), "search line missing: {screen}");
    assert!(
        screen.contains("Claude Fable 5.0"),
        "model column: {screen}"
    );
    assert!(screen.contains("OpenCode Zen"), "provider column: {screen}");
    assert!(screen.contains("DeepSeek V4 Pro"), "second row: {screen}");

    // Typing filters to just the DeepSeek row (matches the model name).
    h.type_str("deepseek");
    let screen = h.render();
    assert!(
        screen.contains("DeepSeek V4 Pro"),
        "kept the match: {screen}"
    );
    assert!(
        !screen.contains("Claude Fable 5.0"),
        "filtered the non-match out: {screen}"
    );
    // Model left, provider right: on the row, the model precedes the provider.
    let row = screen
        .lines()
        .find(|l| l.contains("DeepSeek V4 Pro"))
        .expect("the DeepSeek row");
    let model_at = row.find("DeepSeek V4 Pro").expect("model on the row");
    let prov_at = row.find("OpenCode Go").expect("provider on the row");
    assert!(model_at < prov_at, "model is left of the provider: {row:?}");

    // Esc closes the modal.
    h.press(KeyCode::Esc);
    assert!(h.app.model_selector.is_none(), "Esc closes the selector");
    assert!(!h.render().contains("Search"), "modal is gone after Esc");
}

/// The `/resume` session picker mirrors the `/model` selector: columns
/// (id · name · age · cwd), fuzzy filter across all three text columns, and
/// Esc to close.
#[tokio::test]
async fn session_selector_renders_columns_filters_and_closes() {
    let mut h = Harness::new(vec![]).await;
    // A recent timestamp so the age cell reads "2m ago" (epoch 0 would render
    // a "20644d…" age too wide for the column and get truncated).
    let two_min_ago = (chrono::Local::now().timestamp() - 120) as u64;
    let meta = |id: &str, name: &str, cwd: &str| hrdr_app::SessionMeta {
        id: id.to_string(),
        name: name.to_string(),
        cwd: cwd.to_string(),
        updated: two_min_ago,
        path: std::path::PathBuf::from(format!("/tmp/{id}.json")),
    };
    h.app.session_selector = Some(crate::app::session_selector(vec![
        meta("fix-auth", "Fix the auth bug", "/home/u/api"),
        meta("tui-polish", "TUI polish pass", "/home/u/hrdr"),
    ]));

    let screen = h.render();
    assert!(screen.contains("Search"), "search line missing: {screen}");
    assert!(screen.contains("Enter resume"), "hint line: {screen}");
    assert!(screen.contains("fix-auth"), "id column: {screen}");
    assert!(screen.contains("Fix the auth bug"), "name column: {screen}");
    assert!(screen.contains("ago"), "age column: {screen}");
    assert!(screen.contains("/home/u/api"), "cwd column: {screen}");

    // Column order on a row: id, name, age, cwd.
    let row = screen
        .lines()
        .find(|l| l.contains("fix-auth"))
        .expect("the fix-auth row");
    let id_at = row.find("fix-auth").unwrap();
    let name_at = row.find("Fix the auth bug").unwrap();
    let ts_at = row.find("ago").unwrap();
    let cwd_at = row.find("/home/u/api").unwrap();
    assert!(
        id_at < name_at && name_at < ts_at && ts_at < cwd_at,
        "columns ordered id·name·age·cwd: {row:?}"
    );

    // Typing filters (matches the cwd of the second session only).
    h.type_str("hrdr");
    let screen = h.render();
    assert!(screen.contains("tui-polish"), "kept the match: {screen}");
    assert!(
        !screen.contains("fix-auth"),
        "filtered the non-match out: {screen}"
    );

    // Esc closes the modal.
    h.press(KeyCode::Esc);
    assert!(h.app.session_selector.is_none(), "Esc closes the picker");
    assert!(!h.render().contains("Search"), "modal is gone after Esc");
}

/// The `/theme` picker lists the baked-in themes, live-previews the highlight
/// (moving it swaps the app theme), and Esc restores the original theme.
#[tokio::test]
async fn theme_selector_previews_and_esc_restores() {
    let mut h = Harness::new(vec![]).await;
    let original_user = h.app.theme.user;

    h.submit("/theme").await;
    assert!(h.app.theme_selector.is_some(), "/theme opens the picker");
    let screen = h.render();
    for name in [
        "tokyonight",
        "catppuccin-mocha",
        "dracula",
        "gruvbox-dark",
        "nord",
    ] {
        assert!(screen.contains(name), "{name} listed: {screen}");
    }
    assert!(screen.contains("built-in"), "source column: {screen}");

    // Filter down to dracula: the preview applies it immediately.
    h.type_str("dracula");
    assert_eq!(
        h.app.theme.user,
        ratatui::style::Color::Rgb(0x8b, 0xe9, 0xfd),
        "highlighted theme (dracula cyan) is live-previewed"
    );

    // Esc restores the theme that was in force when the picker opened.
    h.press(KeyCode::Esc);
    assert!(h.app.theme_selector.is_none(), "Esc closes the picker");
    assert_eq!(h.app.theme.user, original_user, "original theme restored");
}

/// A `:skill` invocation sends the expanded template to the model while the
/// transcript shows the raw `:name args` the user typed; the `:` prefix also
/// drives the shared completion popup.
#[tokio::test]
async fn skill_invocation_expands_for_the_model_and_completes() {
    let mut h = Harness::new(vec![MockReply::Text("shipped".to_string())]).await;
    let skills_dir = std::path::PathBuf::from(h.app.current_cwd()).join(".hrdr/skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("ship.md"),
        "---
description: release checklist
---
Run the release checklist for $ARGUMENTS",
    )
    .unwrap();
    // The popup lists the skill (the App cache was built before the file
    // existed — refresh the way /reload and a cwd change do).
    h.app.skills = hrdr_app::discover_skills(std::path::Path::new(&h.app.current_cwd()));
    h.type_str(":sh");
    let screen = h.render();
    assert!(screen.contains(":ship"), "popup lists the skill:\n{screen}");
    assert!(
        screen.contains("release checklist"),
        "popup shows the description:\n{screen}"
    );
    for _ in 0..3 {
        h.press(KeyCode::Backspace);
    }

    h.submit(":ship v0.3").await;
    let screen = h.render();
    assert!(
        screen.contains(":ship v0.3"),
        "the transcript shows the raw invocation:\n{screen}"
    );
    // The model got the expanded template (synced into the session state by
    // the turn-end autosave).
    let user = h
        .app
        .state()
        .messages
        .iter()
        .find(|m| m.role == hrdr_agent::MessageRole::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert_eq!(user, "Run the release checklist for v0.3");
}

/// `/effort` opens a picker of the model's own levels, "Default" on top,
/// highest effort first; picking a level sets + persists it, and picking
/// Default clears the override.
#[tokio::test]
async fn effort_picker_lists_levels_default_first_and_applies() {
    // Enter persists the pick — keep it away from the developer's real config.
    let _env = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.submit("/effort").await;
    assert!(h.app.effort_selector.is_some(), "/effort opens the picker");
    let screen = h.render();
    assert!(screen.contains("Default"), "Default row:\n{screen}");
    assert!(screen.contains("High"), "levels listed:\n{screen}");
    // Default is on top; "test-model" isn't in the catalog, so the fallback
    // ladder applies and High is the first real level.
    let d = screen.find("Default").unwrap();
    let hi = screen.find("High").unwrap();
    assert!(d < hi, "Default sorts above the levels");

    // Fuzzy filter + Enter applies the level. ("medium", not "med": the
    // subsequence filter would also keep Default via "the ModEl/proviDer".)
    h.type_str("medium");
    h.press(KeyCode::Enter);
    assert!(h.app.effort_selector.is_none(), "Enter closes the picker");
    // Effort is the agent's; it publishes it into the pane the frontend renders.
    let effort_of = |h: &Harness| h.app.panes.active_pane().effort.clone();
    for _ in 0..20 {
        if effort_of(&h).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        h.app.sync_panes();
    }
    assert_eq!(effort_of(&h).as_deref(), Some("medium"));
    let screen = h.render();
    assert!(
        screen.contains("effort → Medium (medium)"),
        "confirmation line:\n{screen}"
    );

    // Reopen and pick Default: the override clears.
    h.submit("/effort").await;
    h.press(KeyCode::Enter); // Default is the first row
    for _ in 0..20 {
        if effort_of(&h).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        h.app.sync_panes();
    }
    assert_eq!(effort_of(&h), None, "Default clears the override");
}

/// Argument completion: after a command name + space, the popup offers the
/// argument's candidate values, anchored at the argument column, and Tab
/// completes just the argument.
#[tokio::test]
async fn argument_completion_offers_values_and_tab_fills_the_argument() {
    let mut h = Harness::new(vec![]).await;
    h.type_str("/timestamps rel");
    let screen = h.render();
    assert!(screen.contains("relative"), "candidate offered:\n{screen}");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.content(), "/timestamps relative ");

    // Theme names complete too (built-ins are always registered).
    h.app.editor.set_content("");
    h.type_str("/theme dra");
    assert!(h.render().contains("dracula"), "theme name offered");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.content(), "/theme dracula ");
}

/// The completion popup shows at most 5 rows plus a "… N more" hint, and
/// slides its window to keep the selection visible.
#[tokio::test]
async fn completion_popup_caps_at_five_rows_and_scrolls() {
    let mut h = Harness::new(vec![]).await;
    h.type_str("/");
    let screen = h.render();
    // The first five registry commands render; the sixth doesn't (cap = 5).
    // (Counting screen lines that start with '/' is a trap: the banner's cwd
    // path wraps onto its own line on runners with long temp paths.)
    let names: Vec<&str> = hrdr_app::slash_completions("/")
        .iter()
        .map(|(n, _)| *n)
        .collect();
    for n in &names[..5] {
        assert!(screen.contains(n), "{n} visible in the popup:\n{screen}");
    }
    assert!(screen.contains("more"), "overflow hint:\n{screen}");
    assert!(screen.contains("/new"), "canonical /new listed:\n{screen}");

    // Moving the selection past the window slides it: the sixth command shows
    // only after stepping the selection down to it.
    let sixth = hrdr_app::slash_completions("/")[5].0;
    assert!(
        !h.render().contains(sixth),
        "sixth command hidden initially"
    );
    for _ in 0..6 {
        h.press(KeyCode::Down);
    }
    assert!(
        h.render().contains(sixth),
        "window slid to keep the selection visible"
    );
}

/// The TODO panel shows the current agent's list — not a global one. Each
/// agent keeps its own TODO list in its live entry; switching panes switches
/// which TODOs are rendered below the sub-agent panel. The existing tests
/// exercise the main agent; this one verifies the sub-agent's own list
/// appears when its pane is active.
#[tokio::test]
async fn the_todo_panel_shows_the_active_agents_list() {
    let mut h = Harness::new(vec![]).await;

    // Give the main agent a TODO.
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "main task".to_string(),
        status: "in_progress".to_string(),
    }];

    // Register a sub-agent with its own TODO.
    let sub_key = 1u64;
    let sub_todos = std::sync::Arc::new(std::sync::Mutex::new(vec![hrdr_agent::Todo {
        content: "sub task".to_string(),
        status: "pending".to_string(),
    }]));
    let sub_agent = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
        ..Default::default()
    })
    .unwrap();
    h.app.live_subagents.register(hrdr_agent::LiveSubagent {
        key: sub_key,
        bg_id: None,
        tool_id: Some("call-1".to_string()),
        label: "explore".to_string(),
        model: "haiku".to_string(),
        provider: None,
        base_url: String::new(),
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: sub_todos.clone(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        turn: hrdr_agent::TurnStats::default(),
        kind: hrdr_agent::SubagentKind::Blocking,
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub_agent)),
        steering: hrdr_agent::steering_queue(),
        running: false,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
    });
    h.app.sync_panes();

    // On the main agent: only the main agent's TODO shows.
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("main task"),
        "main agent's todos:\n{screen}"
    );
    assert!(
        !screen.contains("sub task"),
        "sub-agent's todos not on main:\n{screen}"
    );

    // Switch to the sub-agent: now only its TODO shows.
    h.app.focus_pane(hrdr_app::PaneId::Sub(sub_key));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("main task"),
        "main's todos hidden on sub:\n{screen}"
    );
    assert!(screen.contains("sub task"), "sub-agent's todos:\n{screen}");
}
