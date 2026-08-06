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

use super::{App, Entry, EntryKind, StatusBarMode, TurnMsg};
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
    rx: mpsc::Receiver<TurnMsg>,
    _mock: MockServer,
    _tmp: tempfile::TempDir,
}

impl Harness {
    async fn new(replies: Vec<MockReply>) -> Self {
        Self::with_max_steps(replies, 50).await
    }

    async fn with_max_steps(replies: Vec<MockReply>, max_steps: usize) -> Self {
        Self::build(replies, max_steps, hrdr_tools::SandboxMode::None, false).await
    }

    /// A harness whose session is confined for real, with nothing writable to
    /// the agent at all: session mode `read` AND a read-only scope, because
    /// `effective_sandbox` floors a write-capable agent at `write` (see its
    /// decision table).
    ///
    /// Only for tests *about* the sandbox — see the `sandbox: None` reasoning in
    /// [`Harness::build`] for why every other test pins it off. `cfg(unix)` because
    /// its only caller is, and on Windows an uncalled helper is a `-D warnings`
    /// failure the Linux build cannot see.
    #[cfg(unix)]
    async fn read_only_sandbox() -> Self {
        Self::build(vec![], 50, hrdr_tools::SandboxMode::Read, true).await
    }

    async fn build(
        replies: Vec<MockReply>,
        max_steps: usize,
        sandbox: hrdr_tools::SandboxMode,
        read_only: bool,
    ) -> Self {
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
            // A TUI e2e test asserts on what the terminal shows, so its
            // transcript must not depend on what the *host* can confine with.
            // Under the shipped default (`Write`) the first shell command an
            // agent runs on a machine with no OS sandbox — CI's Windows and
            // macOS runners, any Linux without Landlock — queues a degradation
            // notice, which folds into the transcript as an extra entry and
            // scrolls the rows these tests assert on out of the viewport. Worse,
            // that queue is process-global and first-come-first-served, so under
            // a plain `cargo test` (one process, tests in parallel — what the
            // leak-guard job runs) the notice earned by one test lands in
            // whichever *other* test's turn loop drains it first. `None` is the
            // honest setting here: none of these tests is about OS confinement
            // (that lives in hrdr-tools' and hrdr-agent's own tests), and it
            // makes every one of them behave identically on every platform.
            sandbox,
            read_only,
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

    fn ctrl(&mut self, c: char) {
        self.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
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

    /// Hand the app one agent event as a running turn would.
    ///
    /// A turn records the event on the agent's own entry and then wakes the
    /// frontend ([`hrdr_agent::AgentRegistry::start_turn`]) — the transcript, the
    /// counters and the turn clock all come from that record, for every agent. A
    /// test that fabricates an event without a turn behind it has to do both, or it
    /// is exercising a path that does not exist.
    fn inject(&mut self, ev: hrdr_agent::AgentEvent) {
        self.app.registry.record(hrdr_agent::MAIN_KEY, &ev);
        self.app.on_turn_msg(TurnMsg::Event(ev));
    }

    /// Drain the turn channel until the agent is idle **and** nothing it sent is
    /// still queued.
    ///
    /// The flag alone is not the end of the turn: a turn marks its own agent idle
    /// as it finishes (`RunGuard`), and its closing `Done` — plus anything still
    /// behind it — is already in the channel by then. Stopping at the flag would
    /// leave the frontend's end-of-turn work (the stats line, the autosave, a
    /// queued message's relaunch) undone. The relaunch is why this loops: draining
    /// can start the next turn.
    async fn pump(&mut self) {
        loop {
            while self.app.running() {
                match self.rx.recv().await {
                    Some(msg) => self.app.on_turn_msg(msg),
                    None => break,
                }
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.app.on_turn_msg(msg);
            }
            if !self.app.running() {
                return;
            }
        }
    }

    /// Wait for the session save the last turn/tool round enqueued to land.
    ///
    /// Saves serialize + write on a spawned task, so the session file may not be
    /// current (or, before its first write, may not exist) when a turn settles —
    /// a test that reads it must drain until the coalescer's `SaveDone` clears
    /// the in-flight flag. Processes every message it drains, exactly as the
    /// real loop would.
    async fn save_drain(&mut self) {
        while self.app.save_in_flight || self.app.pending_save.is_some() {
            match self.rx.recv().await {
                Some(msg) => self.app.on_turn_msg(msg),
                None => break,
            }
        }
    }

    /// Drain turn messages until one matches `pred`, applying each as it goes
    /// (the real loop's order). Bounded so a watcher event or off-thread walk
    /// that never arrives fails the test instead of hanging it.
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&TurnMsg) -> bool) {
        let wait = async {
            loop {
                let msg = self.rx.recv().await.expect("turn channel closed");
                let done = pred(&msg);
                self.app.on_turn_msg(msg);
                if done {
                    return;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Render the whole UI to a [`TestBackend`] and flatten it to text.
    fn render(&mut self) -> String {
        let mut term = Terminal::new(TestBackend::new(90, 30)).unwrap();
        term.draw(|f| ui::draw(f, &mut self.app)).unwrap();
        buffer_to_string(term.backend().buffer())
    }
}

/// Click a cell: press *and* release, which is what the transcript's own hit
/// targets answer to (a release after movement is a select-to-copy drag).
fn click_at(app: &mut App, column: u16, row: u16) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.on_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        });
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
        tmp: Some(tmp),
    }
}

/// The lifetime of a private data home: holds the env lock and the temp dir, and puts
/// the process-wide roots back when the test ends.
struct DataHomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    tmp: Option<tempfile::TempDir>,
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
        // The root is left on disk rather than deleted, and that is the point.
        //
        // These vars are process-global while the lock is only held by tests that
        // ASK for a private root, so a sibling test running in parallel resolves
        // its session path into this directory without ever taking the lock.
        // `Session::save` creates the directory and then writes into it; delete
        // the root between those two steps and that sibling's autosave fails with
        // ENOENT, it pushes a "conversation is not safely stored" notice into its
        // transcript, and any assertion of that test's on-screen rows shifts by
        // three lines. That was a real, rare failure of
        // `read_only_tool_calls_run_concurrently_in_order`, and nothing about it
        // was a bug in autosave.
        //
        // Restoring the vars above closes the window for paths resolved *after*
        // this point; leaking the directory closes it for the ones already
        // resolved. The cost is one temp dir per isolating test in a test binary,
        // which the OS reclaims from `/tmp`.
        if let Some(tmp) = self.tmp.take() {
            let _ = tmp.keep();
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
    // The tool call and the final text are asserted on the **transcript**, not
    // on the rendered buffer: by the time this turn settles its tool block has
    // already scrolled off the top of a 30-row terminal (the same trap
    // `parallel_tool_calls_in_one_turn_all_run` notes below), so a `contains` on
    // the screen only ever passed by way of the words in the final assistant
    // text — and any extra entry the session picks up (a system notice) pushed
    // that off too, failing the test for a reason that has nothing to do with
    // the tool round-trip it is checking.
    let kinds: Vec<&EntryKind> = h.app.transcript().iter().map(|e| &e.kind).collect();
    let tools: Vec<&str> = kinds
        .iter()
        .filter_map(|k| match k {
            EntryKind::Tool { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tools, ["todo"], "the todo tool ran: {kinds:?}");
    // The follow-up turn's text — proof the round-trip drove a second call.
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EntryKind::Assistant(t) if t == "Added the todo.")),
        "final reply missing: {kinds:?}"
    );
    // The todo panel is fixed chrome at the bottom of the frame, so it shows
    // the item whatever the scrollback is doing.
    let screen = h.render();
    assert!(
        screen.contains("write more tests"),
        "todo item missing:\n{screen}"
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
    // Asserted on the transcript, not the screen — same reason as
    // `two_tool_calls_in_one_turn_both_run`: a tool block sits well above the
    // final reply, and anything that adds rows (a notice, a longer result)
    // scrolls it off the top of a 30-row terminal. What this test is about is
    // that both concurrent calls landed, in call order; the viewport is not the
    // place to ask.
    let tools: Vec<String> = h
        .app
        .transcript()
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Tool { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        ["glob", "grep"],
        "both read-only tools ran, in order"
    );
    // The final reply is the newest entry, so it IS reliably on screen.
    let screen = h.render();
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
    // The three identical reads group behind a summary header; expand the
    // groups so the assertion can see their results.
    h.app.verbose = true;
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
    // The lone tool call collapses behind its summary; fan it out so the
    // result — where the error lives — renders.
    h.app.verbose = true;
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
    let popup = h.app.popup.as_ref().expect("the help popup is open");
    assert!(
        popup.text.contains("/exit") && popup.text.contains("reload AGENTS.md"),
        "help output missing from the popup"
    );
    // The popup renders its top — the command list — and Esc closes it.
    let screen = h.render();
    assert!(screen.contains("/new"), "the help popup renders:\n{screen}");
    h.press(KeyCode::Esc);
    assert!(h.app.popup.is_none(), "Esc closes the help popup");
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
    // A reasoning_content SSE delta lands as EntryKind::Reasoning alongside the
    // normal EntryKind::Assistant — stored regardless of `verbose` (the toggle
    // only gates rendering, see the render test below).
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "I am thinking.".to_string(),
        text: "Done.".to_string(),
    }])
    .await;
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

/// The unified input path folds the `Steered` event into a SINGLE user entry —
/// it does not `push_entry(Entry::user)` one itself. Guards BOTH regressions at
/// once: a dropped push (0 entries) and a double one (push + fold = 2 entries).
#[tokio::test]
async fn a_submitted_message_appears_as_exactly_one_user_entry() {
    let mut h = Harness::new(vec![MockReply::Text("ack".into())]).await;
    h.submit("wire up the parser").await;

    let user_entries = h
        .app
        .transcript()
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::User(t) if t == "wire up the parser"))
        .count();
    assert_eq!(
        user_entries, 1,
        "exactly one user entry — not zero (dropped) and not two (double-pushed)"
    );
}

/// `/init` runs a HIDDEN turn via `launch_hidden`: the init prompt is pushed
/// into the model's history as a note, not shown as something the user typed.
/// The opener-less turn path deliberately emits no `Steered`, so no visible user
/// entry is folded in — yet the turn still runs and the model replies.
#[tokio::test]
async fn init_runs_a_hidden_turn_without_a_visible_user_entry() {
    let mut h = Harness::new(vec![MockReply::Text("wrote AGENTS.md".into())]).await;
    h.submit("/init").await;
    assert!(!h.app.running(), "the hidden turn settled");

    // No user entry at all: neither the `/init` command nor the hidden init
    // prompt appears as something the user said.
    let user_entries = h
        .app
        .transcript()
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::User(_)))
        .count();
    assert_eq!(
        user_entries, 0,
        "the hidden init prompt is never shown as a user entry"
    );

    // But a turn DID run against it: the model's reply is in the transcript.
    let replied = h
        .app
        .transcript()
        .iter()
        .any(|e| matches!(&e.kind, EntryKind::Assistant(t) if t.contains("wrote AGENTS.md")));
    assert!(replied, "the hidden turn ran and the model replied");
}

#[tokio::test]
async fn reasoning_hidden_shows_a_summary_until_verbose() {
    // Reasoning is hidden by default (there is no /thinking toggle any more):
    // the thought is stored as EntryKind::Reasoning but renders as a folded
    // summary line — never the raw text — until /verbose on shows it.
    let mut h = Harness::new(vec![MockReply::TextWithReasoning {
        reasoning: "secret thought".to_string(),
        text: "visible reply".to_string(),
    }])
    .await;
    assert!(!h.app.verbose, "verbose must default to false");
    h.submit("think aloud").await;
    assert!(!h.app.running());
    let screen = h.render();
    assert!(
        !screen.contains("secret thought"),
        "reasoning leaked into render when disabled:\n{screen}"
    );
    assert!(
        screen.contains("Thought for"),
        "the hidden thought leaves a summary entry:\n{screen}"
    );
    assert!(
        screen.contains("visible reply"),
        "text reply missing from render:\n{screen}"
    );
    // /verbose on shows the thinking in full — there is no separate toggle.
    h.submit("/verbose on").await;
    assert!(h.app.verbose, "/verbose on shows reasoning");
    let screen = h.render();
    assert!(
        screen.contains("secret thought"),
        "/verbose on must render the thinking:\n{screen}"
    );
    // /verbose off folds it back behind the summary.
    h.submit("/verbose off").await;
    assert!(!h.app.verbose, "/verbose off hides reasoning");
    assert!(
        !h.render().contains("secret thought"),
        "reasoning leaked after /verbose off"
    );
}

/// A slash command's status line (`/verbose off`) is a toast, not a
/// transcript entry: it never touches the streaming thinking block, so the
/// thought stays one block and closes normally when the reply arrives — no
/// split into two running halves around a notice.
#[tokio::test]
async fn a_slash_commands_status_toasts_instead_of_entering_the_transcript() {
    let mut h = Harness::new(vec![]).await;
    use hrdr_agent::AgentEvent;

    // A turn is in flight, streaming a thought.
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.inject(AgentEvent::Reasoning("secret thought".into()));
    assert!(
        h.app
            .transcript()
            .last()
            .is_some_and(|e| matches!(&e.kind, EntryKind::Reasoning { took_ms: None, .. })),
        "the thought is open and streaming"
    );

    // `/verbose off` mid-thought: its "verbose mode off" line goes to the
    // toast stack — the transcript is untouched.
    h.type_str("/verbose off");
    h.press(KeyCode::Enter);
    assert_eq!(
        h.app.toasts.last_body(),
        Some("verbose mode off"),
        "the status line toasts"
    );
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Notice(s) if s == "verbose mode off")),
        "the status line did not enter the transcript:\n{:?}",
        h.app.transcript()
    );

    // The reply closes the thought normally — one block, never split.
    h.inject(AgentEvent::Text("visible reply".into()));
    h.app
        .registry
        .update(hrdr_agent::MAIN_KEY, |e| e.running = false);
    let t = h.app.transcript();
    let thoughts: Vec<&Entry> = t
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Reasoning { .. }))
        .collect();
    assert_eq!(
        thoughts.len(),
        1,
        "the thought is one block, never split:\n{t:?}"
    );
    assert!(
        matches!(
            &thoughts[0].kind,
            EntryKind::Reasoning {
                took_ms: Some(_),
                ..
            }
        ),
        "the thought closed instead of staying running: {:?}",
        thoughts[0].kind
    );
}

/// A slash command's data output (`/cost`) renders in an Esc-dismissible
/// popup instead of the transcript, and Esc closes it.
#[tokio::test]
async fn a_data_commands_output_shows_in_an_esc_dismissible_popup() {
    let mut h = Harness::new(vec![]).await;

    // `/cost` posts its line synchronously (no spawned task).
    h.type_str("/cost");
    h.press(KeyCode::Enter);
    let popup = h.app.popup.as_ref().expect("the cost popup is open");
    assert!(
        popup.text.contains("session tokens:"),
        "the cost data is in the popup: {:?}",
        popup.text
    );
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Notice(s) if s.contains("session tokens:"))),
        "the cost output did not enter the transcript:\n{:?}",
        h.app.transcript()
    );

    // It renders.
    let screen = h.render();
    assert!(
        screen.contains("session tokens:"),
        "the popup renders:\n{screen}"
    );

    // Esc dismisses it.
    h.press(KeyCode::Esc);
    assert!(h.app.popup.is_none(), "Esc closes the popup");
}

/// A spawned data command (`/status`) posts its output through the async line
/// channel and lands in the same Esc-dismissible popup.
#[tokio::test]
async fn an_async_data_commands_output_lands_in_the_popup() {
    let mut h = Harness::new(vec![]).await;

    h.type_str("/status");
    h.press(KeyCode::Enter);
    // The `/status` body is built in a spawned task; drain until it lands.
    loop {
        let msg = h.rx.recv().await.expect("the status line arrives");
        h.app.on_turn_msg(msg);
        if h.app.popup.is_some() {
            break;
        }
    }
    let popup = h.app.popup.as_ref().expect("the status popup is open");
    assert!(
        popup.text.contains("session:") && popup.text.contains("model:"),
        "the status data is in the popup: {:?}",
        popup.text
    );

    h.press(KeyCode::Esc);
    assert!(h.app.popup.is_none(), "Esc closes the popup");
}

/// `/verbose` is a strict on/off toggle — the rename of `/expand`. A bare
/// `/verbose` flips the mode, `on`/`off` set it. On fans every tool GROUP out
/// in full; off folds them all back behind their summaries.
#[tokio::test]
async fn verbose_toggles_all_tool_blocks_between_on_and_off() {
    let mut h = Harness::new(vec![]).await;
    let t = |secs: i64| hrdr_app::time_from_unix(secs, chrono::Local::now());
    // Two finished calls: a group of two, folded behind its summary by default.
    // The result text appears in no collapsed form, so its presence is proof
    // the calls rendered in full.
    h.app.transcript_mut().push(Entry::at(
        EntryKind::Tool {
            id: "c1".into(),
            name: "shell".into(),
            args: r#"{"command":"echo hi"}"#.into(),
            result: "VERBOSE-MODE-RESULT".into(),
            ok: true,
            done: true,
        },
        t(1_700_000_000),
    ));
    h.app.transcript_mut().push(Entry::at(
        EntryKind::Tool {
            id: "c2".into(),
            name: "read".into(),
            args: r#"{"path":"x"}"#.into(),
            result: "SECOND-RESULT".into(),
            ok: true,
            done: true,
        },
        t(1_700_000_001),
    ));
    assert!(!h.app.verbose, "default is folded behind the summary");
    assert!(
        !h.render().contains("VERBOSE-MODE-RESULT"),
        "folded by default"
    );

    // Bare `/verbose` toggles on: the collapsed block renders in full.
    h.submit("/verbose").await;
    assert!(h.app.verbose, "a bare /verbose turns the mode on");
    assert!(
        h.render().contains("VERBOSE-MODE-RESULT"),
        "on shows every tool's full output"
    );

    // `/verbose off` collapses everything and returns to manual mode.
    h.submit("/verbose off").await;
    assert!(!h.app.verbose, "/verbose off turns the mode off");
    assert!(
        !h.render().contains("VERBOSE-MODE-RESULT"),
        "off collapses again"
    );

    // The explicit setter, then the flip back off.
    h.submit("/verbose on").await;
    assert!(h.app.verbose, "/verbose on sets the mode on");
    h.submit("/verbose").await;
    assert!(!h.app.verbose, "a bare /verbose flips back off");
    // /verbose owns the thinking display too: on shows it, off hides it.
    assert!(
        !h.app.verbose,
        "a bare /verbose flip-off also hides reasoning"
    );
    h.submit("/verbose on").await;
    assert!(
        h.app.verbose,
        "/verbose on shows reasoning as well as tools"
    );
    h.submit("/verbose off").await;
    assert!(!h.app.verbose, "/verbose off hides reasoning again");
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
    h.inject(AgentEvent::Notice("tool-round limit reached".to_string()));
    assert_eq!(
        h.app.scroll_offset, 7,
        "AgentEvent::Notice reset scroll_offset while user was scrolled up"
    );

    h.app.scroll_offset = 0;
    h.inject(AgentEvent::Notice("health warning".to_string()));
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
            name: "shell".into(),
            args: r#"{"command":"echo hi"}"#.into(),
        },
        MockReply::Text("done".into()),
    ])
    .await;
    h.submit("run it").await;
    // The lone tool call collapses behind its summary; fan it out so its box —
    // the surface these assertions check — renders.
    h.app.verbose = true;

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
    // Blank padded rows above and below the text close the block.
    assert_eq!(bg_at(0, user_y - 1), theme.user_bg, "top pad row");
    assert_eq!(bg_at(0, user_y + 1), theme.user_bg, "bottom pad row");
    assert_eq!(
        without_bar(&row_text(user_y - 1)),
        "",
        "top pad row is blank"
    );
    assert_eq!(
        without_bar(&row_text(user_y + 1)),
        "",
        "bottom pad row is blank"
    );

    // A blank separator row (terminal bg) sits between blocks.
    assert_ne!(bg_at(0, user_y + 2), theme.user_bg, "separator row");

    // The tool box: status mark + name on the header, command below it, on the
    // tool background — flush with the transcript's own content column, like
    // every other block.
    let tool_y = find_row("✓ shell").expect("tool header rendered");
    assert_eq!(
        bg_at(0, tool_y),
        theme.user_bg,
        "the call's box carries the tool background"
    );
    assert_eq!(
        bg_at(2, tool_y),
        theme.user_bg,
        "the box itself shares the prompt bg"
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
                name: "shell".into(),
                args: r#"{"command":"echo hi"}"#.into(),
                result: "hi".into(),
                ok: true,
                done: true,
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
        transcript: vec![Entry::tool_running("c1", "shell", "{}")],
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
            name: "shell".into(),
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
            .any(|e| matches!(&e.kind, EntryKind::Tool { name, .. } if name == "shell")),
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
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
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

    // The turn ends: the queued message becomes a turn of its own.
    h.app.on_turn_msg(TurnMsg::Done(None));
    assert!(
        h.app.running(),
        "a fresh turn was spawned for the queued message"
    );
    // Its opener is drained and shown by `run` — via the folded `Steered` event,
    // not a push at submit time — so pump the relaunched turn to completion.
    h.pump().await;
    assert!(h.app.pending().is_empty(), "the queue drained");
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::User(t) if t == "second question")),
        "the queued message was sent after the turn"
    );
}

/// The steering path: a message queued while the model is working rides in with
/// the next round's tool results, so the model reads its tool output and the
/// correction together and can change course. It enters the transcript at
/// delivery — not at submit — so display order matches the model's view.
#[tokio::test]
async fn a_queued_message_rides_in_with_the_tool_results() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);

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
        .registry
        .take_pending(hrdr_agent::MAIN_KEY)
        .expect("the agent takes it off its own queue");
    h.inject(AgentEvent::Steered(taken.display));
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

/// Esc cancels the turn in flight and hands anything queued behind it back to
/// the composer — neither sent nor lost.
#[tokio::test]
async fn cancelling_returns_pending_steering_to_the_composer() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("never mind");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.steering_len_for_test(), 1);

    h.app.cancel_turn();
    // Off the queue...
    assert!(h.app.pending().is_empty(), "nothing is left queued to fire");
    assert_eq!(h.app.steering_len_for_test(), 0, "the agent's copy too");
    // ...and in front of the user, where they can edit, resend or clear it.
    assert_eq!(h.app.editor.content(), "never mind");
    // Cancel means stop: it must not have started anything.
    assert!(!h.app.running(), "cancel does not launch the next turn");
}

/// Esc interrupts only on a second *consecutive* press: the first arms, and any
/// other key in between disarms — a stray Esc must not kill a long turn.
#[tokio::test]
async fn esc_takes_two_presses_to_interrupt_a_turn() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);

    h.press(KeyCode::Esc);
    assert!(h.app.cancel_armed, "the first press only arms");
    assert!(h.app.running(), "the turn is untouched");

    h.type_str("x");
    assert!(!h.app.cancel_armed, "any other key disarms");
    h.press(KeyCode::Esc);
    assert!(
        h.app.running(),
        "so the next Esc arms again, it doesn't cancel"
    );
    h.press(KeyCode::Esc);
    assert!(!h.app.running(), "two consecutive presses interrupt");
}

/// Ctrl+C reads most-local-first: a non-empty box is a draft to clear; an empty
/// one with something in flight is an interrupt; an empty one with nothing
/// running arms, then confirms, the quit.
#[tokio::test]
async fn ctrl_c_clears_the_draft_then_interrupts_then_quits() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("half-typed");

    h.ctrl('c');
    assert_eq!(h.app.editor.content().trim(), "", "the draft is cleared");
    assert!(h.app.running(), "clearing a draft leaves the turn alone");
    assert!(!h.app.quit_armed, "and doesn't arm the quit");

    h.ctrl('c');
    assert!(!h.app.running(), "an empty box makes Ctrl+C an interrupt");
    assert!(
        !h.app.quit_armed,
        "interrupting doesn't arm the quit either"
    );

    h.ctrl('c');
    assert!(h.app.quit_armed, "idle and empty: the first press arms");
    assert!(!h.app.should_quit);
    h.ctrl('c');
    assert!(h.app.should_quit, "the second consecutive press quits");
}

/// Ctrl+S is a draft stack: it puts a non-empty box aside and hands the newest
/// one back when the box is empty, so several drafts can wait at once.
#[tokio::test]
async fn ctrl_s_stashes_and_pops_drafts() {
    let mut h = Harness::new(vec![]).await;

    h.type_str("first thought");
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "", "the box is cleared");

    h.type_str("second thought");
    h.ctrl('s');
    assert_eq!(h.app.stash.len(), 2, "the stash stacks up");

    // Empty box: newest first.
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "second thought");
    // ...and a non-empty box stashes again rather than popping.
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "");
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "second thought");

    h.app.editor.set_content("");
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "first thought");

    // Nothing left to pop: the empty box stays empty.
    h.app.editor.set_content("");
    h.ctrl('s');
    assert_eq!(h.app.editor.content().trim(), "");
    assert!(h.app.stash.is_empty());
}

/// The input pane's top padding line is a status line for the box: blank while
/// there is nothing to report, "N drafts stashed" while Ctrl+S drafts wait, and
/// "history N/M" while Up/Down browse the recall list (N counting from the
/// newest). Both can show at once.
#[tokio::test]
async fn input_indicator_reports_stash_and_history() {
    let mut h = Harness::new(vec![]).await;

    // Nothing to report: the padding line stays blank (its only content is the
    // pane's left bar).
    let screen = h.render();
    let pad_row = h.app.input_rect.y as usize;
    assert!(
        screen
            .lines()
            .nth(pad_row)
            .unwrap()
            .trim_start_matches(crate::ui::BORDER_BAR)
            .trim()
            .is_empty(),
        "the padding line is blank with nothing to report"
    );

    h.type_str("first thought");
    h.ctrl('s');
    let screen = h.render();
    assert!(screen.contains("1 draft stashed"), "one stash: {screen:?}");

    h.type_str("second thought");
    h.ctrl('s');
    let screen = h.render();
    assert!(
        screen.contains("2 drafts stashed"),
        "two stashes: {screen:?}"
    );

    // Browsing history reports position/total, counting from the newest. The
    // TOTAL is whatever the shared sandbox history file holds (other parallel
    // tests record into it), so assert the selected count, which is
    // deterministic: first Up is always 1, second Up 2.
    h.app.history.record("entry-a");
    h.app.history.record("entry-b");
    h.app.editor.set_content("a draft");
    h.press(KeyCode::Up);
    let screen = h.render();
    assert!(
        screen.contains("history 1/"),
        "first Up lands on the newest: {screen:?}"
    );
    h.press(KeyCode::Up);
    let screen = h.render();
    assert!(
        screen.contains("history 2/"),
        "second Up steps one further back: {screen:?}"
    );

    // Down past the newest restores the draft and leaves browsing.
    h.press(KeyCode::Down);
    h.press(KeyCode::Down);
    let screen = h.render();
    assert!(
        !screen.contains("history 1/") && !screen.contains("history 2/"),
        "browsing ended: {screen:?}"
    );
    assert!(
        screen.contains("2 drafts stashed"),
        "the stash indicator stays: {screen:?}"
    );

    // Popping the stash clears the indicator again.
    h.app.editor.set_content("");
    h.ctrl('s'); // pops "second thought"
    h.app.editor.set_content("");
    h.ctrl('s'); // pops "first thought"
    assert!(h.app.stash.is_empty());
    let screen = h.render();
    assert!(
        !screen.contains("draft stashed"),
        "indicator cleared with the stash: {screen:?}"
    );
}

/// Whatever the user is part-way through typing is newer than what was queued,
/// so it stays last — and the restore must not clobber it.
#[tokio::test]
async fn a_restored_steer_lands_above_what_is_being_typed() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("queued thought");
    h.press(KeyCode::Enter);
    h.type_str("half-typed");

    h.app.cancel_turn();
    assert_eq!(h.app.editor.content(), "queued thought\nhalf-typed");
}

/// Up on an empty box takes a queued message back for editing — and TAKES it,
/// rather than copying it.
#[tokio::test]
async fn up_on_an_empty_box_takes_back_the_queued_message() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("half a thought");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.pending(), ["half a thought"], "queued while running");
    assert_eq!(h.app.editor.content(), "", "and the box was cleared");

    h.press(KeyCode::Up);
    assert_eq!(
        h.app.editor.content(),
        "half a thought",
        "Up brings the queued message back to be edited"
    );
    assert!(
        h.app.pending().is_empty(),
        "and takes it OFF the queue — a copy would be delivered as well as the edit"
    );
}

/// The whole point of taking it off the queue: the edited message is sent once.
///
/// If Up merely copied the text, the queue would still drain the original and the
/// user would see their message twice — once as they first wrote it, once as they
/// meant it.
#[tokio::test]
async fn an_edited_message_is_queued_once_not_twice() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("use the blue one");
    h.press(KeyCode::Enter);

    // Take it back, change it, send it again — still mid-turn, so it re-queues.
    h.press(KeyCode::Up);
    h.app.editor.set_content("use the red one");
    h.press(KeyCode::Enter);

    assert_eq!(
        h.app.pending(),
        ["use the red one"],
        "one message on the queue, and it is the edited one"
    );
}

/// With nothing queued, Up on an empty box is history, exactly as before.
#[tokio::test]
async fn up_on_an_empty_box_with_nothing_queued_still_recalls_history() {
    let mut h = Harness::new(vec![MockReply::Text("answer".to_string())]).await;
    h.submit("an older message").await;
    assert!(h.app.pending().is_empty(), "nothing is queued");

    h.press(KeyCode::Up);
    assert_eq!(
        h.app.editor.content(),
        "an older message",
        "history still answers Up when the queue is empty"
    );
}

/// A half-typed draft in the box is not an invitation to raid the queue: Up with
/// text in it browses history, and what is queued stays queued.
#[tokio::test]
async fn up_with_a_draft_in_the_box_leaves_the_queue_alone() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.type_str("queued thought");
    h.press(KeyCode::Enter);
    h.type_str("half-typed");

    h.press(KeyCode::Up);
    assert_eq!(
        h.app.pending(),
        ["queued thought"],
        "the queue is untouched while the box holds a draft"
    );
}

/// A recalled multi-line entry does not trap the arrows: Up/Down keep walking
/// history while a multi-line item is loaded, instead of moving the cursor a
/// line inside it (the old `!contains('\n')` gate stranded the user on the
/// item).
#[tokio::test]
async fn arrows_walk_history_across_multi_line_entries() {
    let mut h = Harness::new(vec![]).await;
    h.app.history.record("entry-a");
    h.app.history.record("multi\nline");

    h.press(KeyCode::Up); // empty box → newest: the multi-line entry
    assert_eq!(h.app.editor.content(), "multi\nline");
    h.press(KeyCode::Up); // ← previously got stuck moving the cursor instead
    assert_eq!(h.app.editor.content(), "entry-a");
    h.press(KeyCode::Down);
    assert_eq!(h.app.editor.content(), "multi\nline");
    h.press(KeyCode::Down); // past the newest → the stashed (empty) draft
    assert_eq!(h.app.editor.content(), "");
}

/// Editing a recalled multi-line entry before stepping on keeps the edit: Down
/// past the newest returns the edited text, not the pre-browsing draft.
#[tokio::test]
async fn editing_a_recalled_multi_line_entry_returns_the_edit() {
    let mut h = Harness::new(vec![]).await;
    h.app.history.record("entry-a");
    h.app.history.record("multi\nline");

    h.press(KeyCode::Up); // → "multi\nline"
    assert_eq!(h.app.editor.content(), "multi\nline");
    h.press(KeyCode::Up); // → "entry-a"
    h.type_str(" + edited");
    h.press(KeyCode::Down); // back to "multi\nline"
    assert_eq!(h.app.editor.content(), "multi\nline");
    h.press(KeyCode::Down); // past the newest → the edited text comes back
    assert_eq!(h.app.editor.content(), "entry-a + edited");
}

/// Several mid-turn submits queue up and are merged into a single message —
/// each line the user types while waiting is one thought, not separate turns.
#[tokio::test]
async fn queued_messages_merge_and_come_back_together() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    for msg in ["one", "two"] {
        h.type_str(msg);
        h.press(KeyCode::Enter);
    }
    // Merged at enqueue time, so only one entry in the queue.
    assert_eq!(h.app.pending(), ["one\ntwo"]);

    // The turn ends: a fresh turn is spawned to drain the queue.
    h.app.on_turn_msg(TurnMsg::Done(None));
    assert!(
        h.app.running(),
        "a fresh turn was spawned to drain the queue"
    );

    // Cancelling hands the merged message back, both lines intact.
    h.app.cancel_turn();
    assert!(h.app.pending().is_empty(), "nothing left on the queue");
    assert_eq!(h.app.editor.content(), "one\ntwo");
}

/// Regression: meta lines, the thinking spinner, stats lines, and queued-message
/// badges each used to render their own ad-hoc chrome at column 0.
#[tokio::test]
async fn every_transcript_row_is_rendered_through_the_block_path() {
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "shell".into(),
            args: r#"{"command":"echo hi"}"#.into(),
        },
        MockReply::Text("done".into()),
    ])
    .await;
    h.submit("run it").await;
    h.app.transcript_mut().push(Entry::diff("+added"));
    h.app
        .registry
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
        matches!(msg, TurnMsg::ContextWindow(hrdr_app::PaneId::MAIN, w) if w == MOCK_CONTEXT_WINDOW),
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
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.push_entry(Entry::user("do the thing"));
    let snapshot = vec![
        hrdr_agent::Message::user("do the thing"),
        hrdr_agent::Message::assistant("on it"),
    ];
    h.inject(hrdr_agent::AgentEvent::History(std::sync::Arc::new(
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

/// A `/resume` of a session another live instance holds open refuses to open it
/// directly, but arms an offer to open a forked copy — and pressing `f` mints
/// the copy, swaps it in as the active session, and leaves the busy original
/// untouched. Esc-style cancel (any other key) just clears the offer.
#[tokio::test]
async fn a_busy_resume_offers_a_fork_that_f_accepts() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let cwd = h.app.current_cwd();

    // A saved session that a *different* live instance holds open.
    let st = hrdr_app::SessionState {
        name: "Other".into(),
        cwd: cwd.clone(),
        model: "local://test-model".parse().unwrap(),
        messages: vec![hrdr_agent::Message::user("hello there")],
        ..Default::default()
    };
    let outcome = hrdr_app::save_session(&st).unwrap().unwrap();
    let busy_id = outcome.id.clone();
    // The other instance's grip on the source — keep it held for the whole test.
    let _other = outcome.open_lock.expect("first save takes the open-lock");
    let path = hrdr_app::session_file_path(&cwd, &busy_id);

    // Explicit /resume of the busy session: refuses to open, arms the fork offer.
    h.app.resume_locked_path(busy_id.clone(), &path);
    assert!(
        h.app.pending_fork.is_some(),
        "a busy /resume arms the fork offer"
    );
    // The active session is unchanged (no id yet — nothing was resumed).
    assert!(
        h.app.state().id.is_none(),
        "the busy session was not swapped in"
    );

    // Press `f`: fork the copy and swap it in.
    h.press(KeyCode::Char('f'));
    assert!(h.app.pending_fork.is_none(), "the offer was consumed");

    let new_id = h
        .app
        .state()
        .id
        .clone()
        .expect("the fork became the active session");
    assert_ne!(
        new_id, busy_id,
        "the active session is the fork, not the busy original"
    );
    assert!(
        h.app.state().name.ends_with(" (fork)"),
        "the fork carries a ' (fork)' name: {}",
        h.app.state().name
    );
    assert_eq!(
        h.app.state().messages.len(),
        1,
        "the fork copied the source's conversation"
    );

    // The busy original is untouched: still its old name, still locked.
    let src = hrdr_app::Session::load(&cwd, &busy_id).expect("source file intact");
    assert_eq!(src.state.name, "Other", "source not renamed by the fork");
    match hrdr_app::Session::open_path(&path) {
        Err(hrdr_app::OpenError::Busy { .. }) => {}
        other => panic!("source's open-lock was disturbed: {other:?}"),
    }
}

/// A non-confirming key cancels the fork offer without forking.
#[tokio::test]
async fn a_busy_resume_offer_is_cancelled_by_any_other_key() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let cwd = h.app.current_cwd();

    let st = hrdr_app::SessionState {
        name: "Other".into(),
        cwd: cwd.clone(),
        model: "local://test-model".parse().unwrap(),
        messages: vec![hrdr_agent::Message::user("hello there")],
        ..Default::default()
    };
    let outcome = hrdr_app::save_session(&st).unwrap().unwrap();
    let busy_id = outcome.id.clone();
    let _other = outcome.open_lock.expect("first save takes the open-lock");
    let path = hrdr_app::session_file_path(&cwd, &busy_id);

    h.app.resume_locked_path(busy_id.clone(), &path);
    assert!(h.app.pending_fork.is_some());

    // Esc cancels: the offer clears and nothing is forked or swapped in.
    h.press(KeyCode::Esc);
    assert!(h.app.pending_fork.is_none(), "Esc cleared the offer");
    assert!(h.app.state().id.is_none(), "nothing was resumed or forked");
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
    // The save is written off-thread; wait for its SaveDone so the load below
    // reads the turn's state, not the first (mint) write.
    h.save_drain().await;

    // The turn-end autosave assigned an id and wrote the file.
    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("autosave assigned a session id");
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id).expect("session file written");

    // The transcript is rebuilt from the sibling jsonl — the fold of the agent's
    // event stream — so what persists is the model's own output: the user turn,
    // its reasoning, its reply (none of which, save the user turn, exists in
    // `messages`). Frontend chrome — the welcome Header, the per-turn Stats line,
    // "session saved as …" notices — is display-only and never written, so it does
    // NOT come back.
    //
    // Per-entry timestamps are NOT preserved: `AgentEvent`s (and so the jsonl
    // records) carry no wall-clock time, and streaming deltas coalesce, so a
    // rebuilt entry is stamped at fold time. Content is what the fold preserves.
    let saved = &loaded.state.transcript;
    let kinds: Vec<&EntryKind> = saved.iter().map(|e| &e.kind).collect();
    assert!(
        matches!(
            kinds.as_slice(),
            [EntryKind::User(u), EntryKind::Reasoning { .. }, EntryKind::Assistant(a)]
                if u == "run it" && a.contains("done")
        ),
        "the event fold — user, reasoning, reply — round-trips in order: {kinds:?}"
    );
    // None of the frontend chrome is on disk — it is not part of the event fold.
    assert!(
        !saved.iter().any(|e| matches!(
            e.kind,
            EntryKind::Notice(_) | EntryKind::Stats(_) | EntryKind::Header
        )),
        "no chrome (notice/stats/header) persisted: {kinds:?}"
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

/// The session save runs off the UI thread: a turn's end-of-turn handling
/// returns BEFORE its save lands, and the file catches up once the save
/// task's `SaveDone` is drained.
///
/// Regression against Slice 3: `persist_mid_turn`/`autosave` used to
/// `serde_json::to_string` + atomically write the whole session on the UI
/// thread, so by the time a turn settled the file was already current. The
/// write now runs on a spawned task behind a latest-wins coalescer — the
/// turn-end code must not wait for it (that is the stall being removed), and
/// the snapshot reaches disk via the `SaveDone` the task posts.
#[tokio::test]
async fn session_save_lands_off_thread_after_the_turn() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text("done".into())]).await;
    h.submit("do the thing").await;

    let id = h.app.state().id.clone().expect("the turn minted an id");
    let cwd = h.app.current_cwd();

    // The turn-end handling (submit's pump) returned without waiting for the
    // save: the last snapshot is still queued on the coalescer, not on disk.
    assert!(
        h.app.save_in_flight || h.app.pending_save.is_some(),
        "the turn settled before its save landed"
    );

    // Drain until the save's SaveDone is processed — the file is written by
    // then.
    h.save_drain().await;
    assert!(
        !h.app.save_in_flight && h.app.pending_save.is_none(),
        "the coalescer drained every queued save"
    );
    let loaded = hrdr_app::Session::load(&cwd, &id).expect("the save landed");
    assert!(
        loaded.state.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("do the thing"))),
        "the user message reached disk"
    );
    assert!(
        loaded
            .state
            .messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains("done"))),
        "the assistant reply reached disk"
    );
}

/// The shared sub-agent transcript cell starts empty (no id yet) and is
/// repointed at the session's dir once the first autosave assigns an id.
#[tokio::test]
async fn autosave_populates_the_child_transcript_dir() {
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
    let want = hrdr_app::child_transcript_dir(&h.app.current_cwd(), &id);
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
        Some(hrdr_app::child_transcript_dir(&h.app.current_cwd(), &id)),
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
    // `apply_session` reports the endpoint move as a toast, which renders over
    // the header's details column — drop it so the details are visible.
    h.app.toasts = hjkl_holler::HollerBus::new();

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
        h.app.toasts.history().any(
            |t| t.body.contains("this session ran on provider 'nowhere'")
                && t.body.contains("staying on the current endpoint")
        ),
        "the failure is reported via the toast stack"
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
    // The save is written off-thread; the first load below must see the
    // turn's save (the mint alone filed the session under the pre-sync cwd).
    h.save_drain().await;
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
        // The resume's autosave is written off-thread; wait for it before
        // reading the file back.
        h.save_drain().await;

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

/// A thinking block is just the thought: no `⠹ Thinking` spinner, no
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
    h.app.verbose = true; // this test renders the thought in full
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
    h2.app.verbose = true; // this half renders the thought too
    h2.app
        .transcript_mut()
        .push(Entry::reasoning("streaming thoughts"));
    h2.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
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
    h.inject(hrdr_agent::AgentEvent::Text(String::new()));
    assert_eq!(
        h.app.transcript().len(),
        before,
        "an empty delta created a transcript entry"
    );
}

/// The click rect for a tool block is derived from where it lands on screen,
/// which the group chunk build must keep accurate. A lone tool call is its own
/// group of one: collapsed, the hit rect covers the summary row that heads it;
/// after a click expands it, the same rect covers the tool's own header inside
/// the summary section.
#[tokio::test]
async fn a_lone_tool_block_hit_rect_tracks_its_header() {
    // Long enough that the call's preview differs from its full body — a small
    // call renders in full with nothing to toggle.
    let long_output: String = (0..12).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::reasoning("thinking"));
    h.app.push_entry(Entry::assistant("")); // borrows its label from the thought
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long_output,
        ok: true,
        done: true,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    // Collapsed: the recorded hit rect must cover the row the summary renders on.
    let buf = term.backend().buffer();
    let summary_y = (0..30)
        .find(|&y| {
            (0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>()
                .contains("ran 1 command")
        })
        .expect("tool summary rendered");
    let (rect, _) = h
        .app
        .tool_hits
        .iter()
        .copied()
        .find(|(r, _)| r.contains(2, summary_y))
        .expect("a tool hit rect on the summary");
    assert!(
        rect.contains(2, summary_y),
        "the tool hit rect misses the summary at row {summary_y}"
    );

    // Expand: the call renders below the summary as its own block. Its body is
    // togglable, so a row hit covers the call's own header — a click there
    // lands on the call, not the summary.
    click_at(&mut h.app, 2, summary_y);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let header_y = (0..30)
        .find(|&y| {
            (0..39)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect::<String>()
                .contains("✓ shell")
        })
        .expect("tool header rendered after expansion");
    let (rect, _) = h
        .app
        .row_hits
        .iter()
        .copied()
        .find(|(r, _)| r.contains(2, header_y))
        .expect("a row hit on the call's header");
    assert!(
        rect.contains(2, header_y),
        "the row hit misses the tool header at row {header_y}"
    );
}

/// Copy/paste feedback goes to a toast, not the transcript: it is chrome about
/// the terminal, not part of the conversation, and it dismisses itself.
#[tokio::test]
async fn a_toast_paints_over_the_screen() {
    let mut h = Harness::new(vec![]).await;
    h.app.toasts.info("copied 2 lines");

    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("copied 2 lines"),
        "the toast renders:\n{screen}"
    );
    assert!(
        !h.app
            .transcript()
            .iter()
            .any(|e| format!("{:?}", e.kind).contains("copied 2 lines")),
        "and stays out of the transcript"
    );
}

/// A selection that runs to the right-hand edge copies the TEXT and stops: no
/// scrollbar column, and no run of padding behind it.
///
/// The regression: `draw_transcript` published the selectable rect from `area`
/// while drawing the text into `area.width - 1`, so a drag could reach one column
/// further right than anything was ever painted. Every copied line ended in the
/// scrollbar's `│`, and because a box-drawing character is not whitespace, the
/// trailing-blank trim stopped at it and kept the padding in front of it too.
#[tokio::test]
async fn a_selection_to_the_edge_copies_no_scrollbar_and_no_padding() {
    const WIDTH: u16 = 40;

    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::assistant("PICKME"));

    let mut term = Terminal::new(TestBackend::new(WIDTH, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.transcript_rect;
    assert_eq!(
        rect.x + rect.w,
        WIDTH - 1,
        "the selectable rect stops one column short of the frame — that column is \
         the scrollbar's, and nothing writes text into it"
    );

    // Find the row the reply landed on, then select that whole row edge to edge.
    let row = (rect.y..rect.y + rect.h)
        .find(|&row| {
            (rect.x..rect.x + rect.w).any(|col| {
                term.backend()
                    .buffer()
                    .cell(ratatui::layout::Position::new(col, row))
                    .is_some_and(|c| c.symbol() == "P")
            })
        })
        .expect("the reply is on screen");

    let mut buf = term.backend().buffer().clone();
    let last = rect.x + rect.w - 1;
    let text = ui::paint_selection(&mut buf, rect, ((rect.x, row), (last, row)));

    assert!(
        text.contains("PICKME"),
        "the reply's text is copied: {text:?}"
    );
    assert!(
        !text.contains('│'),
        "no scrollbar column in the copied text: {text:?}"
    );
    assert_eq!(
        text,
        text.trim_end(),
        "and nothing trailing behind it: {text:?}"
    );
}

/// A selection that runs to the left-hand edge copies the TEXT and stops: no
/// `┃` border column, and no block padding in front of it.
///
/// The regression: `transcript_rect` began at the block's left edge, where a
/// user row draws its `┃` and its padding — so a drag across a user prompt (or
/// any multi-row selection, whose continuation rows start at the rect's left)
/// copied the border character into every line. The rect now begins at the
/// first content column, the way its right edge already stops before the
/// scrollbar column.
#[tokio::test]
async fn a_selection_to_the_edge_copies_no_border_bar_and_no_padding() {
    const WIDTH: u16 = 40;

    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // A user prompt wears the `┃` border — the surface whose bar used to land
    // in the copied text.
    h.app.push_entry(Entry::user("PICKME"));

    let mut term = Terminal::new(TestBackend::new(WIDTH, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.transcript_rect;
    let row = (rect.y..rect.y + rect.h)
        .find(|&row| {
            (rect.x..rect.x + rect.w).any(|col| {
                term.backend()
                    .buffer()
                    .cell(ratatui::layout::Position::new(col, row))
                    .is_some_and(|c| c.symbol() == "P")
            })
        })
        .expect("the reply is on screen");

    let mut buf = term.backend().buffer().clone();
    let last = rect.x + rect.w - 1;
    let text = ui::paint_selection(&mut buf, rect, ((rect.x, row), (last, row)));

    assert!(
        text.contains("PICKME"),
        "the reply's text is copied: {text:?}"
    );
    assert!(
        !text.contains('┃'),
        "no border bar in the copied text: {text:?}"
    );
    assert_eq!(
        text,
        text.trim(),
        "and nothing leading or trailing it: {text:?}"
    );
}

/// The scrollbar column is not text, so pressing on it starts no selection.
#[tokio::test]
async fn a_press_on_the_scrollbar_column_starts_no_selection() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    const WIDTH: u16 = 40;

    let mut h = Harness::new(vec![]).await;
    h.app.push_entry(Entry::assistant("something to look at"));
    let mut term = Terminal::new(TestBackend::new(WIDTH, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.transcript_rect;
    let press = |column| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: rect.y + 1,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };

    h.app.on_mouse(press(WIDTH - 1));
    assert!(
        h.app.selection.is_none(),
        "the scrollbar column holds no text, so a press there is not a drag"
    );

    // The last TEXT column still starts one, so the rect was not simply shrunk
    // out of usefulness.
    h.app.on_mouse(press(WIDTH - 2));
    assert!(
        h.app.selection.is_some(),
        "the rightmost text column is still selectable"
    );
}

/// The block chrome at the transcript's left edge is not text, so pressing on
/// it starts no selection — the same rule as the scrollbar column on the right.
/// Only the content band (two columns in, one short of the edge) is selectable.
#[tokio::test]
async fn a_press_on_the_left_padding_column_starts_no_selection() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    const WIDTH: u16 = 40;

    let mut h = Harness::new(vec![]).await;
    // A user row wears the `┃` at the very left — the chrome a press must skip.
    h.app.push_entry(Entry::user("something to look at"));
    let mut term = Terminal::new(TestBackend::new(WIDTH, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.transcript_rect;
    let press = |column| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: rect.y + 1,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };

    h.app.on_mouse(press(0));
    assert!(
        h.app.selection.is_none(),
        "the ┃ column holds no text, so a press there is not a drag"
    );
    h.app.on_mouse(press(1));
    assert!(
        h.app.selection.is_none(),
        "the padding column holds no text either"
    );
    h.app.on_mouse(press(rect.x));
    assert!(
        h.app.selection.is_some(),
        "the content band is where a drag starts"
    );
}

/// Copying from the input box grabs the content only — no `┃` (the prompt's
/// rule is at the pane's left edge), no padding — the same content band every
/// other surface selects.
#[tokio::test]
async fn input_box_copy_contains_no_border_or_padding() {
    const WIDTH: u16 = 40;

    let mut h = Harness::new(vec![]).await;
    h.app.editor.set_content("PICKME from the box");

    let mut term = Terminal::new(TestBackend::new(WIDTH, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.input_rect;
    let row = (rect.y..rect.y + rect.h)
        .find(|&row| {
            (rect.x..rect.x + rect.w).any(|col| {
                term.backend()
                    .buffer()
                    .cell(ratatui::layout::Position::new(col, row))
                    .is_some_and(|c| c.symbol() == "P")
            })
        })
        .expect("the input text is on screen");

    let mut buf = term.backend().buffer().clone();
    let last = rect.x + rect.w - 1;
    let text = ui::paint_selection(&mut buf, rect, ((rect.x, row), (last, row)));

    assert!(
        text.contains("PICKME from the box"),
        "the input text is copied: {text:?}"
    );
    assert!(
        !text.contains('┃'),
        "the prompt's border rule stays out of the copy: {text:?}"
    );
    assert_eq!(
        text,
        text.trim(),
        "nothing leading or trailing the text: {text:?}"
    );
}

/// A todo-panel row copies its content only — the green rule at the panel's
/// left edge stays out of the copy, like the scrollbar does on the right.
#[tokio::test]
async fn todo_panel_copy_contains_no_rule() {
    const WIDTH: u16 = 50;

    let mut h = Harness::new(vec![]).await;
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "SHIP IT NOW".to_string(),
        id: 7,
        status: "in_progress".to_string(),
        evidence: None,
    }];

    let mut term = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let rect = h.app.transcript_rect;
    let row_text = |row: u16| -> String {
        (rect.x..rect.x + rect.w)
            .filter_map(|col| {
                term.backend()
                    .buffer()
                    .cell(ratatui::layout::Position::new(col, row))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let row = (rect.y..rect.y + rect.h)
        .find(|&row| row_text(row).contains("SHIP IT NOW"))
        .expect("the todo renders on screen");

    let mut buf = term.backend().buffer().clone();
    let last = rect.x + rect.w - 1;
    let text = ui::paint_selection(&mut buf, rect, ((rect.x, row), (last, row)));

    assert!(
        text.contains("SHIP IT NOW"),
        "the todo content is copied: {text:?}"
    );
    assert!(
        !text.contains('┃'),
        "the panel's green rule stays out of the copy: {text:?}"
    );
    assert_eq!(
        text,
        text.trim(),
        "nothing leading or trailing the row: {text:?}"
    );
}

/// Dragging across the transcript selects the cells under the pointer and, when
/// the button comes up, copies what they say — the drag never reaches the tool
/// block it started on, so selecting text can't toggle a block open.
#[tokio::test]
async fn dragging_the_transcript_selects_and_copies_instead_of_clicking() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: "SELECTABLE".into(),
        ok: true,
        done: true,
    }));

    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };

    // Press on the tool summary row, drag two cells along it, release. (The
    // summary block's first row is its top pad; the text sits one row down.)
    let summary_row = rect.y + 1;
    h.app.on_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        2,
        summary_row,
    ));
    h.app.on_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        12,
        summary_row,
    ));
    assert!(h.app.selection.is_some(), "the drag started a selection");
    h.app.on_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        12,
        summary_row,
    ));
    assert!(h.app.pending_copy, "releasing a drag queues the copy");
    assert!(
        h.app.tool_groups.is_empty(),
        "a drag is not a click: it leaves the group state untouched"
    );

    // The frame after the release harvests the cells and reports the result.
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(!h.app.pending_copy, "the copy ran on the next frame");
    assert!(
        h.app.toasts.last_body().is_some(),
        "the copy says how it went in a toast"
    );

    // Anything that redraws those cells drops the selection.
    h.press(KeyCode::Char('x'));
    assert!(h.app.selection.is_none(), "a keypress clears the selection");
}

/// Select-to-copy works over the input pane too: a drag that starts and ends
/// inside the input box copies the text it crosses, exactly like a drag over
/// the transcript.
#[tokio::test]
async fn dragging_the_input_box_selects_and_copies() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    h.type_str("hello");
    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let rect = h.app.input_rect;
    assert!(rect.h > 0, "the input pane was drawn");

    // The pane pads one row top and BLOCK_PAD_X columns left; find the `h` the
    // editor rendered — the typed word runs from there.
    let text_row = rect.y + 1;
    let start = (rect.x..rect.x + rect.w)
        .find(|&col| {
            term.backend()
                .buffer()
                .cell(ratatui::layout::Position::new(col, text_row))
                .is_some_and(|c| c.symbol() == "h")
        })
        .expect("the typed text renders in the input pane");
    let end = start + 4; // the five cells of "hello" are start..=end
    let mut buf = term.backend().buffer().clone();
    assert_eq!(
        ui::paint_selection(&mut buf, rect, ((start, text_row), (end, text_row))),
        "hello",
        "the cells under the drag hold the typed text"
    );

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    h.app.on_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        start,
        text_row,
    ));
    h.app.on_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        text_row,
    ));
    assert!(
        h.app.selection.is_some(),
        "a press in the input box starts a selection"
    );
    h.app
        .on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), end, text_row));
    assert!(h.app.pending_copy, "releasing the drag queues the copy");

    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(!h.app.pending_copy, "the copy ran on the next frame");
    assert!(
        h.app.toasts.last_body().is_some(),
        "the copy says how it went in a toast"
    );
}

/// Select-to-copy works over the status bar too: dragging across its text
/// copies it, like the bottom line of any terminal.
#[tokio::test]
async fn dragging_the_status_bar_selects_and_copies() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    h.app.statusbar_mode = hrdr_app::StatusBarMode::Truncate;
    let mut term = Terminal::new(TestBackend::new(40, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let rect = h.app.status_rect;
    assert!(rect.h > 0, "the status bar was drawn");

    // The block pads one row top; find the first text cell on its content row
    // (skipping the padding blanks and the left border) and select a short run.
    let text_row = rect.y + 1;
    let start = (rect.x..rect.x + rect.w)
        .find(|&col| {
            term.backend()
                .buffer()
                .cell(ratatui::layout::Position::new(col, text_row))
                .is_some_and(|c| !c.symbol().trim().is_empty() && c.symbol() != "│")
        })
        .expect("the status bar has text to select");
    let end = start + 2;
    let mut buf = term.backend().buffer().clone();
    let expected = ui::paint_selection(&mut buf, rect, ((start, text_row), (end, text_row)));
    assert!(
        !expected.trim().is_empty(),
        "the cells under the status drag hold real text"
    );

    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    h.app.on_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        start,
        text_row,
    ));
    h.app.on_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        text_row,
    ));
    assert!(
        h.app.selection.is_some(),
        "a press in the status bar starts a selection"
    );
    h.app
        .on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), end, text_row));
    assert!(h.app.pending_copy, "releasing the drag queues the copy");

    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(!h.app.pending_copy, "the copy ran on the next frame");
    assert!(
        h.app.toasts.last_body().is_some(),
        "the copy says how it went in a toast"
    );
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
    // The first-save toast ("session saved as …") would cover the top rows this
    // test asserts on; it is transient chrome, so drop it before drawing.
    h.app.toasts = hjkl_holler::HollerBus::new();

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

    // Inside the reply's block: same background, no separator between them.
    assert_eq!(
        bg_at(stats_y),
        bg_at(reply_y),
        "stats share the block:\n{screen}"
    );
    assert_ne!(bg_at(stats_y), h.app.theme.stats_bg, "no block of its own");
    // Ordering: the reply, then the stats that close its block.
    assert!(reply_y < stats_y, "stats follow the reply");
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
    // The first-save toast would cover the top rows; drop it before drawing.
    h.app.toasts = hjkl_holler::HollerBus::new();

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
    // The first-save toast would cover the top rows; drop it before drawing.
    h.app.toasts = hjkl_holler::HollerBus::new();

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
/// The "tool" blocks are `edit`/`replace` calls: they always render (they never
/// group), so each is its own tinted block — the fixture is about the separator
/// rows between blocks, not about tool grouping.
///
/// prompt │ tool │ tool │ thought │ tool │ output
///        ↑blank ↑blank ↑         ↑      ↑
#[tokio::test]
async fn separator_rows_appear_only_between_tinted_blocks() {
    let tool = |id: &str, name: &str, args: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: args.into(),
            result: format!("res-{id}"),
            ok: true,
            done: true, // the results are the row anchors below
        })
    };
    let mut h = Harness::new(vec![]).await;
    h.app.verbose = true; // the "thought" row is an anchor below
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("prompt"));
    h.app.push_entry(tool("a", "edit", r#"{"path":"edit-me"}"#));
    h.app
        .push_entry(tool("b", "replace", r#"{"path":"replace-me"}"#));
    h.app.push_entry(Entry::reasoning("thought"));
    h.app.push_entry(tool("c", "edit", r#"{"path":"edit-c"}"#));
    h.app.push_entry(Entry::assistant("output"));
    h.app
        .push_entry(tool("d", "replace", r#"{"path":"replace-d"}"#));

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
    let (prompt_end, a_end, b_end) = (row_of("prompt"), row_of("res-a"), row_of("res-b"));
    let (thought, c_end) = (row_of("thought"), row_of("res-c"));

    // Tinted → tinted: both blocks' pads, plus a separator row between them.
    assert_eq!(
        gap(prompt_end, row_of("edit-me")),
        3,
        "prompt → tool needs a separator:\n{screen}"
    );
    assert_eq!(
        gap(a_end, row_of("replace-me")),
        3,
        "tool → tool needs a separator:\n{screen}"
    );

    // Tinted → untinted and back: just the two pads, no separator.
    assert_eq!(gap(b_end, thought), 2, "tool → thought:\n{screen}");
    assert_eq!(
        gap(thought, row_of("edit-c")),
        2,
        "thought → tool:\n{screen}"
    );
    assert_eq!(gap(c_end, row_of("output")), 2, "tool → output:\n{screen}");
}

/// The model's thought and the output that follows it are separated by a single
/// blank row, not two.
///
/// Regression: each block contributes a blank padded row (below and above), and
/// with neither tinted they stacked into a two-row gap.
#[tokio::test]
async fn a_thought_and_the_output_after_it_share_one_blank_row() {
    let mut h = Harness::new(vec![]).await;
    h.app.verbose = true; // the "thinking here" row is an anchor below
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
    // Untinted → tinted: the summary's first row IS the tool summary now (no
    // pad above it), so only the assistant block's own bottom pad separates it.
    assert_eq!(
        gap(row_of("the output"), row_of("listed 1 directory")),
        1,
        "output → tool:\n{screen}"
    );
}

/// Collapsing an expanded tool group pulls its summary to the top of the
/// viewport rather than letting it slide.
///
/// Regression: `scroll_offset` is measured from the *bottom*, so shrinking the
/// transcript kept the view the same distance from the end — the block the user
/// was reading jumped up by however many rows it lost.
#[tokio::test]
async fn collapsing_a_tool_group_keeps_its_summary_at_the_top_of_the_view() {
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long,
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c2".into(),
        name: "read".into(),
        args: r#"{"path":"x"}"#.into(),
        result: String::new(),
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::assistant("after"));
    // Enough filler that the transcript still overflows the viewport AFTER the
    // collapse — the pin-keeps-top behavior only exists for a scrolled-up view.
    for i in 0..20 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }
    // The group is expanded: the calls fan out and the transcript is tall.
    h.app.tool_groups.insert("c1".to_string());

    let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    // Scroll up until the group summary is on screen, mid-viewport.
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
                .contains("ran 1 command · read 1 file")
        })
    };
    let before = header_row(&term).expect("group summary on screen");
    assert!(h.app.scroll_offset > 0, "the reader is scrolled up");

    // Click the summary: the group folds, and its top comes to the viewport's top.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    assert!(rect.contains(2, before));
    click_at(&mut h.app, 2, before);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let after = header_row(&term).expect("the summary is still on screen");
    let screen = buffer_to_string(term.backend().buffer());
    assert_eq!(
        after, before,
        "collapsing must keep the summary on the same screen row, not jump it:\n{screen}"
    );
}

/// Expanding a collapsed group must not scroll the view: the top of the
/// viewport stays focused on the same line, and the summary the click landed
/// on stays on the same screen row while the calls fan out beneath it.
///
/// Regression: the toggle pinned the entry's top to the TOP of the viewport,
/// so clicking a summary mid-viewport yanked the whole view up to it.
#[tokio::test]
async fn expanding_a_tool_group_keeps_the_viewport_on_the_same_line() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::user("go"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c1".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: "LONG-RESULT".into(),
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "c2".into(),
        name: "read".into(),
        args: r#"{"path":"x"}"#.into(),
        result: String::new(),
        ok: true,
        done: true,
    }));
    // Enough filler that the transcript overflows the viewport.
    for i in 0..20 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    // Scroll up: the summary sits mid-viewport, not at the top.
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let row_of = |term: &Terminal<TestBackend>, needle: &str| -> u16 {
        let buf = term.backend().buffer();
        (0..20)
            .find(|&y| {
                (0..39)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}"))
    };
    let before = row_of(&term, "ran 1 command · read 1 file");
    assert!(
        before > 2,
        "the summary should be mid-viewport for this test, got row {before}"
    );
    assert!(h.app.scroll_offset > 0, "the reader is scrolled up");

    // Click the summary: the group expands, and the summary stays put.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    assert!(rect.contains(2, before));
    click_at(&mut h.app, 2, before);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let after = row_of(&term, "ran 1 command · read 1 file");
    let screen = buffer_to_string(term.backend().buffer());
    assert_eq!(
        after, before,
        "expanding must keep the summary on the same screen row:\n{screen}"
    );
    assert!(
        screen.contains("LONG-RESULT"),
        "the call fanned out below the summary:\n{screen}"
    );
}

/// A running call streams its live preview below the summary — the newest
/// output stays visible while the call runs — and folds behind the summary
/// once it completes. When several calls run at once, only the newest shows.
#[tokio::test]
async fn a_running_call_streams_a_live_preview_and_folds_when_done() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str, done: bool| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: if done {
                format!("result-{id}")
            } else {
                format!("partial-{id}")
            },
            ok: true,
            done,
        })
    };

    // The first call lands and runs: it is its own summary AND a live preview,
    // so the streaming output is visible while it works.
    h.app.push_entry(tool("a", "shell", false));
    let screen = h.render();
    assert!(
        screen.contains("running 1 command"),
        "a running lone call gets a summary:\n{screen}"
    );
    assert!(
        screen.contains("partial-a"),
        "the running call's live output is visible:\n{screen}"
    );

    // A second call arrives while the first still runs: both join the summary,
    // but only the newest shows its preview — the earlier one folds.
    h.app.push_entry(tool("b", "read", false));
    let screen = h.render();
    assert!(
        screen.contains("running 1 command · reading 1 file"),
        "the new call updates the summary, it does not open a new entry:\n{screen}"
    );
    assert!(
        screen.contains("partial-b"),
        "the newest running call shows its live preview:\n{screen}"
    );
    assert!(
        !screen.contains("partial-a"),
        "an earlier running call folds while a newer one previews:\n{screen}"
    );

    // The calls finish; the previews fold behind the settled summary.
    h.app.transcript_mut().iter_mut().for_each(|e| {
        if let EntryKind::Tool { done, result, .. } = &mut e.kind {
            *done = true;
            result.push_str("done");
        }
    });
    let screen = h.render();
    assert!(
        screen.contains("ran 1 command · read 1 file"),
        "the settled summary keeps the merged sections:\n{screen}"
    );
    assert!(
        !screen.contains("done"),
        "finished calls fold behind the summary:\n{screen}"
    );

    // Expanding reveals every call — the newest included — as a child item.
    h.app.tool_groups.insert("a".to_string());
    let screen = h.render();
    assert!(
        screen.contains("✓ shell") && screen.contains("✓ read"),
        "expanding shows the calls inside the summary:\n{screen}"
    );
}

/// A summary update rewrites only the rows it owns: when a new call joins an
/// open group — or the group settles — the summary row and the live preview's
/// own row are the only ones that change; every other row of the buffer is
/// untouched, so the render cannot jump or flicker while the summary counts
/// up. (The group chunk is rebuilt fresh each frame; the guarantee is that
/// the rebuild is layout-stable.)
#[tokio::test]
async fn a_tool_summary_update_rewrites_only_its_own_row() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str, done: bool| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done,
        })
    };
    for i in 0..6 {
        h.app
            .push_entry(Entry::assistant(format!("filler {i}\nline\nline\nline")));
    }

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    h.app.push_entry(tool("a", "shell", false));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = term.backend().buffer().clone();

    // A second call joins the group: the summary counts it in place, and the
    // live preview switches to the newest running call — nothing else moves.
    // The preview box's header sits three rows below the summary (its page
    // blank and top pad between them), so the changed rows are fixed relative
    // to it.
    h.app.push_entry(tool("b", "read", false));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let joined = term.backend().buffer().clone();
    let summary_row = screen_row_of(&term, "running 1 command · reading 1 file")
        .expect("the summary is on screen");
    assert_eq!(
        changed_rows(&before, &joined),
        vec![summary_row, summary_row + 3],
        "a merged running call must touch only the summary and the preview row"
    );

    // A completed call joining folds instead: the summary counts it, and
    // nothing else moves at all — the height is unchanged.
    h.app.push_entry(tool("c", "shell", true));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let joined2 = term.backend().buffer().clone();
    let summary_row = screen_row_of(&term, "running 2 commands · reading 1 file")
        .expect("the summary is on screen");
    assert_eq!(
        changed_rows(&joined, &joined2),
        vec![summary_row],
        "a folded merge must touch only the summary row"
    );

    // Settling: the summary goes past tense and the preview folds — the group
    // shrinks back to its one line, so the view re-anchors to the new bottom.
    // The row-exact guarantee only covers height-preserving updates; assert the
    // summary settled and no running preview remains.
    h.app.transcript_mut().iter_mut().for_each(|e| {
        if let EntryKind::Tool { done, .. } = &mut e.kind {
            *done = true;
        }
    });
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let settled = buffer_to_string(term.backend().buffer());
    assert!(
        settled.contains("ran 2 commands · read 1 file"),
        "the settled summary keeps the merged sections:\n{settled}"
    );
    assert!(
        !settled.contains("⠹"),
        "settling folds the live preview:\n{settled}"
    );
}

/// An expanded group's settled calls render as *previews*, capped at the same
/// size as a running call's live preview: the tail of the result (the newest
/// output) for a normal call, the head for a mutation — the change is at the
/// front. A short result fits the preview whole.
#[tokio::test]
async fn an_expanded_group_previews_calls_until_clicked() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // 20 result lines: the 8-line preview shows only the tail.
    let long: String = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long,
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "b".into(),
        name: "read".into(),
        args: r#"{"path":"x"}"#.into(),
        result: "SHORT".into(),
        ok: true,
        done: true,
    }));
    h.app.tool_groups.insert("a".to_string());

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("line 19"),
        "the preview shows the newest output:\n{screen}"
    );
    assert!(
        !screen.contains("line 0"),
        "the head of a long result is cut:\n{screen}"
    );
    assert!(
        screen.contains("12 earlier line(s)"),
        "the cut is marked:\n{screen}"
    );
    assert!(screen.contains("SHORT"), "a short result fits the preview");
}

/// Clicking one call's preview expands that one call to its full body; clicking
/// the full body folds it back. The other calls stay previews either way.
#[tokio::test]
async fn clicking_a_call_preview_expands_just_that_call() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let long: String = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "shell".into(),
        args: r#"{"command":"ls"}"#.into(),
        result: long,
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "b".into(),
        name: "read".into(),
        args: r#"{"path":"x"}"#.into(),
        result: "SHORT".into(),
        ok: true,
        done: true,
    }));
    h.app.tool_groups.insert("a".to_string());

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    // The preview's tail row is the click target for call "a".
    let preview_row = screen_row_of(&term, "line 19").expect("the preview tail on screen");
    click_at(&mut h.app, 5, preview_row);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("line 0"),
        "clicking the preview expands the full body:\n{screen}"
    );
    assert!(
        h.app.tool_open.contains("a"),
        "the clicked call is marked open"
    );
    assert!(
        screen.contains("SHORT"),
        "the other call stays a preview:\n{screen}"
    );

    // Click a full-body row: the call folds back to its preview.
    let full_row = screen_row_of(&term, "line 0").expect("the full body on screen");
    click_at(&mut h.app, 5, full_row);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("line 0"),
        "clicking the full body folds it back to the preview:\n{screen}"
    );
    assert!(
        h.app.tool_open.is_empty(),
        "the call is no longer marked open"
    );
}

/// Clicking a padding gap between an expanded group's call boxes — not a call,
/// not the summary — folds the whole group back to its summary line.
#[tokio::test]
async fn clicking_a_gap_between_previews_collapses_the_group() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "shell".into(),
        args: "{}".into(),
        result: "RESULT-A".into(),
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "b".into(),
        name: "read".into(),
        args: "{}".into(),
        result: "RESULT-B".into(),
        ok: true,
        done: true,
    }));
    h.app.tool_groups.insert("a".to_string());

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        buffer_to_string(term.backend().buffer()).contains("RESULT-A"),
        "the group is expanded"
    );
    // The summary is the first row of the chunk; the row below it is the
    // first box's top padding — a gap between the summary and the call.
    let summary_row =
        screen_row_of(&term, "ran 1 command · read 1 file").expect("the summary on screen");
    click_at(&mut h.app, 5, summary_row + 1);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(h.app.tool_groups.is_empty(), "a gap click folds the group");
    assert!(
        !screen.contains("RESULT-A"),
        "the calls fold behind the summary:\n{screen}"
    );
}

/// A hidden thought's summary updates in place too: settling from
/// `⠹ Thinking for 1s` to `✓ Thought for 1m 32s` rewrites the summary
/// row and nothing else, so the render cannot jump while the thought finishes.
#[tokio::test]
async fn a_thinking_summary_update_rewrites_only_its_own_row() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    for i in 0..6 {
        h.app
            .push_entry(Entry::assistant(format!("filler {i}\nline\nline\nline")));
    }
    h.app.push_entry(Entry::now(EntryKind::Reasoning {
        text: "a private thought\nsecond line".into(),
        took_ms: None,
    }));

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = term.backend().buffer().clone();
    assert!(
        screen_row_of(&term, "Thinking for").is_some(),
        "the running summary is on screen"
    );

    // Settle the thought: the summary flips to the check form, in place.
    h.app.transcript_mut().iter_mut().for_each(|e| {
        if let EntryKind::Reasoning { took_ms, .. } = &mut e.kind {
            *took_ms = Some(92_000);
        }
    });
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let after = term.backend().buffer().clone();
    let summary_row =
        screen_row_of(&term, "Thought for 1m 32s").expect("the settled summary is on screen");
    assert_eq!(
        changed_rows(&before, &after),
        vec![summary_row],
        "settling a thought must touch only its summary row"
    );
}

/// The rows whose cell contents differ between two rendered buffers.
fn changed_rows(a: &Buffer, b: &Buffer) -> Vec<u16> {
    (0..a.area.height.min(b.area.height))
        .filter(|&y| {
            (0..a.area.width.min(b.area.width)).any(|x| {
                a.cell(Position::new(x, y)).map(|c| c.symbol())
                    != b.cell(Position::new(x, y)).map(|c| c.symbol())
            })
        })
        .collect()
}

/// A hidden thought folds behind a summary entry — `✓ Thought for 1m 32s` —
/// that expands to the full thought on click and folds back behind the
/// summary on a second click, exactly like a tool group's summary.
#[tokio::test]
async fn a_hidden_thought_folds_behind_a_summary_and_expands_on_click() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Reasoning {
        text: "SECRET-THOUGHT".into(),
        took_ms: Some(92_000),
    }));
    h.app.push_entry(Entry::assistant("the answer"));

    // Level 0: one summary line — the thought itself stays hidden.
    let screen = h.render();
    let summary_row = line_index_of(&screen, "Thought for 1m 32s")
        .unwrap_or_else(|| panic!("the summary line:\n{screen}"));
    assert!(
        !screen.contains("SECRET-THOUGHT"),
        "the thought stays folded:\n{screen}"
    );

    // Click the summary: the full thought replaces it (the same block
    // `/verbose on` would render), and a click anywhere on it folds it back.
    click_at(&mut h.app, 2, summary_row);
    let screen = h.render();
    assert!(
        screen.contains("SECRET-THOUGHT"),
        "clicking the summary opens the thought:\n{screen}"
    );
    assert!(
        !screen.contains("Thought for 1m 32s"),
        "the block replaces the summary while open, like /verbose on:\n{screen}"
    );

    // Click it again: folded back behind the summary.
    let thought_row = line_index_of(&screen, "SECRET-THOUGHT")
        .unwrap_or_else(|| panic!("the open thought:\n{screen}"));
    click_at(&mut h.app, 2, thought_row);
    let screen = h.render();
    assert!(
        !screen.contains("SECRET-THOUGHT"),
        "a second click folds the thought back:\n{screen}"
    );
}

/// Clicking a *streaming* thought's summary opens it and keeps it open as more
/// tokens arrive. Its content hash changes with every chunk, so the open state
/// keys on the entry's index — keyed on the hash, the next token would silently
/// fold the open thought back to its summary.
#[tokio::test]
async fn an_open_streaming_thought_stays_open_as_it_streams() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Reasoning {
        text: "the first words".into(),
        took_ms: None,
    }));
    h.app.push_entry(Entry::assistant("the answer"));

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let summary_row =
        screen_row_of(&term, "Thinking for").expect("the streaming summary on screen");
    click_at(&mut h.app, 2, summary_row);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        buffer_to_string(term.backend().buffer()).contains("the first words"),
        "clicking opens the streaming thought"
    );

    // More tokens arrive: the text grows and the content hash changes.
    h.app.transcript_mut().iter_mut().for_each(|e| {
        if let EntryKind::Reasoning { text, .. } = &mut e.kind {
            text.push_str(", and more words keep coming");
            e.refresh_hash();
        }
    });
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("and more words keep coming"),
        "the streamed text renders:\n{screen}"
    );
    assert!(
        !screen.contains("Thinking for"),
        "the thought stays open — no summary, no auto-close:\n{screen}"
    );
}

/// While a thought is still streaming it reads `⠹ Thinking for 1s` with the
/// loader mark; once it settles the mark becomes ✓ and the summary reads
/// `✓ Thought for 1m 32s` — the same verb change and marks a tool
/// group's summary uses.
#[tokio::test]
async fn a_running_thought_reads_thinking_with_the_loader_mark() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Reasoning {
        text: "a thought".into(),
        took_ms: None,
    }));
    let screen = h.render();
    assert!(
        screen.contains("Thinking for"),
        "running reads Thinking, not Thought:\n{screen}"
    );
    assert!(
        !screen.contains("Thought for"),
        "the settled form appears only after the block ends:\n{screen}"
    );
    assert!(
        !screen.contains("✓ Thinking"),
        "the running mark is the loader, not the check:\n{screen}"
    );

    // Settle it: the check appears and the verb flips.
    h.app.transcript_mut().iter_mut().for_each(|e| {
        if let EntryKind::Reasoning { took_ms, .. } = &mut e.kind {
            *took_ms = Some(92_000);
        }
    });
    let screen = h.render();
    assert!(
        screen.contains("✓ Thought for 1m 32s"),
        "settled reads ✓ Thought for …:\n{screen}"
    );
}

/// The tool summary is the group's first row — no pad above it — with exactly
/// one blank row under it, so an expanded group's first call never sits flush
/// against the summary line.
#[tokio::test]
async fn a_tool_summary_has_no_pad_above_and_one_blank_below() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::assistant("context"));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "shell".into(),
        args: "{}".into(),
        result: "RESULT-A".into(),
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "b".into(),
        name: "read".into(),
        args: "{}".into(),
        result: "RESULT-B".into(),
        ok: true,
        done: true,
    }));

    // Collapsed: the summary is the group's first row, one blank under it.
    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let row_of = |needle: &str| -> u16 {
        (0..30)
            .find(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}"))
    };
    let blank = |y: u16| {
        (0..59)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect::<String>()
            .trim()
            .is_empty()
    };
    let summary = row_of("ran 1 command · read 1 file");
    // The row above the summary is the previous block's own bottom pad — and
    // text above that — so the group adds no pad of its own above the line.
    assert!(
        blank(summary - 1),
        "one pad row above the summary:\n{}",
        buffer_to_string(buf)
    );
    assert!(
        !blank(summary - 2),
        "the row above that is the previous block's text — no group pad:\n{}",
        buffer_to_string(buf)
    );
    assert!(!blank(summary), "the summary row itself holds text");
    assert!(
        blank(summary + 1),
        "one blank row under the summary:\n{}",
        buffer_to_string(buf)
    );

    // Expanded: summary, one blank, then the first call's box.
    h.app.tool_groups.insert("a".to_string());
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let row_of = |needle: &str| -> u16 {
        (0..30)
            .find(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}"))
    };
    let blank = |y: u16| {
        (0..59)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect::<String>()
            .trim()
            .is_empty()
    };
    let summary = row_of("ran 1 command · read 1 file");
    let result_a = row_of("RESULT-A");
    assert!(
        blank(summary + 1),
        "one blank row under the summary:\n{}",
        buffer_to_string(buf)
    );
    assert!(
        result_a > summary + 1,
        "the first call starts below the blank, not flush against the summary:\n{}",
        buffer_to_string(buf)
    );
    // The blank before the box is on the page — the box's tint starts below
    // it, on the box's own top pad.
    let bg_at = |y: u16| buf.cell(Position::new(2, y)).unwrap().bg;
    assert_eq!(
        bg_at(summary + 1),
        Color::Reset,
        "the blank before the box is on the page:\n{}",
        buffer_to_string(buf)
    );
    assert_eq!(
        bg_at(summary + 2),
        h.app.theme.user_bg,
        "the box tint starts below the page blank:\n{}",
        buffer_to_string(buf)
    );
}

/// The screen row of the first line containing `needle`, in a `render()`-style
/// newline-joined buffer string.
fn line_index_of(screen: &str, needle: &str) -> Option<u16> {
    screen
        .lines()
        .position(|l| l.contains(needle))
        .map(|i| i as u16)
}

/// The screen row of the first row containing `needle` in the terminal's
/// current buffer, or `None` when it is off-screen.
fn screen_row_of(term: &Terminal<TestBackend>, needle: &str) -> Option<u16> {
    let buf = term.backend().buffer();
    (0..buf.area.height).find(|&y| {
        (0..buf.area.width.saturating_sub(1))
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect::<String>()
            .contains(needle)
    })
}

/// Scrolled up, new content streaming in must not move the viewport: the view
/// stays on the same content rows. The `scroll_offset` compensation in
/// `draw_chunks` adds however many rows `max_scroll` grew so the from-top
/// position is preserved.
///
/// A new tool call that merges into a collapsed group below the marker changes
/// nothing — the folded summary's height is the same whether it holds one call
/// or three — so the marker must not budge.
#[tokio::test]
async fn a_merged_tool_call_keeps_the_scrolled_up_viewport() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done: true,
        })
    };
    h.app.push_entry(Entry::assistant("PIN-MARKER"));
    h.app.push_entry(tool("a", "shell"));
    h.app.push_entry(tool("b", "read"));
    for i in 0..12 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.max_scroll > 0,
        "the transcript overflows the viewport"
    );

    // Scroll to the top: the marker is on screen, the group right below it.
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = screen_row_of(&term, "PIN-MARKER").expect("marker on screen");
    assert!(h.app.scroll_offset > 0, "scrolled up");

    // A new tool call merges into the collapsed group below the marker.
    h.app.push_entry(tool("c", "shell"));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let after = screen_row_of(&term, "PIN-MARKER").expect("marker still on screen");
    assert_eq!(
        after,
        before,
        "the viewport moved:\n{}",
        buffer_to_string(term.backend().buffer())
    );
}

/// The same pin holds when the group is expanded and the merged call GROWS it:
/// the growth sits between the marker and the filler, and the compensation
/// must keep the marker on the same screen row.
#[tokio::test]
async fn an_expanded_group_growing_keeps_the_scrolled_up_viewport() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: format!("result-{id}"),
            ok: true,
            done: true,
        })
    };
    h.app.push_entry(Entry::assistant("PIN-MARKER"));
    h.app.push_entry(tool("a", "shell"));
    h.app.push_entry(tool("b", "read"));
    h.app.tool_groups.insert("a".to_string());
    for i in 0..12 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.max_scroll > 0,
        "the transcript overflows the viewport"
    );

    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = screen_row_of(&term, "PIN-MARKER").expect("marker on screen");
    assert!(h.app.scroll_offset > 0, "scrolled up");

    // A new tool call merges into the expanded group: it grows in place.
    h.app.push_entry(tool("c", "shell"));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let after = screen_row_of(&term, "PIN-MARKER").expect("marker still on screen");
    assert_eq!(
        after,
        before,
        "the viewport moved:\n{}",
        buffer_to_string(term.backend().buffer())
    );
}

/// The same pin holds when a whole NEW group opens below the view (after an
/// `edit`/`replace` boundary): content appended at the bottom grows
/// `max_scroll`, and the compensation keeps the marker put.
#[tokio::test]
async fn a_new_group_streaming_in_keeps_the_scrolled_up_viewport() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done: true,
        })
    };
    h.app.push_entry(Entry::assistant("PIN-MARKER"));
    h.app.push_entry(tool("a", "shell"));
    h.app.push_entry(tool("e1", "edit"));
    for i in 0..12 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.max_scroll > 0,
        "the transcript overflows the viewport"
    );

    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = screen_row_of(&term, "PIN-MARKER").expect("marker on screen");
    assert!(h.app.scroll_offset > 0, "scrolled up");

    // A new tool call lands after the edit boundary: a brand-new group opens
    // at the bottom of the transcript.
    h.app.push_entry(tool("c", "shell"));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let after = screen_row_of(&term, "PIN-MARKER").expect("marker still on screen");
    assert_eq!(
        after,
        before,
        "the viewport moved:\n{}",
        buffer_to_string(term.backend().buffer())
    );
}

/// The real event stream, not just direct pushes: a turn that calls a tool
/// emits `ToolStart` → `ToolEnd` → `Usage` → `TurnDone` → `Done`, each applied
/// to the app and drawn, exactly as the event loop does. Scrolled up the whole
/// way, the viewport must not move at any step — the tool folds into its
/// summary (constant height) and the closing stats line lands at the bottom.
#[tokio::test]
async fn a_real_tool_round_keeps_the_scrolled_up_viewport() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![
        MockReply::ToolCalls(vec![(
            "shell".to_string(),
            r#"{"command":"echo hi"}"#.to_string(),
        )]),
        MockReply::Text("done".to_string()),
    ])
    .await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::assistant("PIN-MARKER"));
    for i in 0..12 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }

    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.max_scroll > 0,
        "the transcript overflows the viewport"
    );
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let before = screen_row_of(&term, "PIN-MARKER").expect("marker on screen");
    assert!(h.app.scroll_offset > 0, "scrolled up");

    // Launch the turn without draining the channel — the test drives it.
    h.type_str("go");
    h.press(KeyCode::Enter);
    let mut saw_tool = false;
    let mut saw_done = false;
    while !saw_done {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), h.rx.recv())
            .await
            .expect("a turn event")
            .expect("channel stays open");
        saw_tool |= matches!(
            msg,
            TurnMsg::Event(AgentEvent::ToolStart { .. })
                | TurnMsg::Event(AgentEvent::ToolEnd { .. })
        );
        saw_done = matches!(msg, TurnMsg::Done(_));
        h.app.on_turn_msg(msg);
        term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
        if let Some(row) = screen_row_of(&term, "PIN-MARKER") {
            assert_eq!(
                row,
                before,
                "the viewport moved:\n{}",
                buffer_to_string(term.backend().buffer())
            );
        }
    }
    assert!(saw_tool, "the turn actually called a tool");
    assert!(!h.app.running(), "the turn finished after Done");
}

/// A run of tool calls groups behind one `{mark} called N tools · ran 2
/// commands · read 1 file` summary line — the only collapsed mode. Clicking
/// the summary renders every call in full; clicking it again folds the group
/// back behind the summary. There is no intermediate one-liner state.
#[tokio::test]
async fn tool_groups_collapse_behind_a_summary_and_expand_on_click() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "a".into(),
        name: "shell".into(),
        args: r#"{"command":"ls -la"}"#.into(),
        result: "RESULT-A".into(),
        ok: true,
        done: true,
    }));
    h.app.push_entry(Entry::now(EntryKind::Tool {
        id: "b".into(),
        name: "read".into(),
        args: r#"{"path":"x.rs"}"#.into(),
        result: "RESULT-B".into(),
        ok: true,
        done: true,
    }));

    // Level 0: one summary line — the calls themselves are hidden.
    let screen = h.render();
    assert!(
        screen.contains("ran 1 command · read 1 file"),
        "the summary leads with the per-call sections:\n{screen}"
    );
    assert!(
        !screen.contains("RESULT-A") && !screen.contains("RESULT-B"),
        "no call bodies while collapsed:\n{screen}"
    );

    // Click the summary → fully expanded: every call renders in full.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    click_at(&mut h.app, rect.x + 2, rect.y + 1);
    let screen = h.render();
    assert!(
        screen.contains("RESULT-A") && screen.contains("RESULT-B"),
        "both calls render in full once the group is expanded:\n{screen}"
    );

    // Click the summary again → folded back behind it.
    click_at(&mut h.app, rect.x + 2, rect.y + 1);
    let screen = h.render();
    assert!(
        screen.contains("ran 1 command · read 1 file"),
        "the group folds back behind the summary:\n{screen}"
    );
    assert!(
        !screen.contains("RESULT-A"),
        "the calls hide again:\n{screen}"
    );
}

/// `edit`/`replace` calls break a tool run — they always render in full — so
/// two tool groups sandwiching one of each stay two groups, with the
/// always-full calls as distinct standalone entries between them:
///
///     summary of 6 calls · edit · replace · summary of 6 calls
///
/// Regression: the groups must not merge across the always-full calls, and the
/// calls must not fold into either group.
#[tokio::test]
async fn edit_and_replace_break_the_group_into_standalone_entries() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done: true,
        })
    };
    for i in 0..6 {
        h.app.push_entry(tool(&format!("a{i}"), "shell"));
    }
    h.app.push_entry(tool("e1", "edit"));
    h.app.push_entry(tool("r1", "replace"));
    for i in 0..6 {
        h.app.push_entry(tool(&format!("b{i}"), "shell"));
    }

    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    let row_of = |needle: &str| -> Vec<u16> {
        (0..24)
            .filter(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .collect()
    };
    let summaries = row_of("ran 6 commands");
    assert_eq!(
        summaries.len(),
        2,
        "one summary per six-call run:\n{screen}"
    );
    let edit = row_of("✓ edit");
    let replace = row_of("✓ replace");
    assert_eq!(
        edit.len(),
        1,
        "the edit renders as its own entry:\n{screen}"
    );
    assert_eq!(
        replace.len(),
        1,
        "the replace renders as its own entry:\n{screen}"
    );
    assert!(
        summaries[0] < edit[0] && edit[0] < replace[0] && replace[0] < summaries[1],
        "summary · edit · replace · summary, in order:\n{screen}"
    );
}

/// Visible entries — a thinking block, the model's output — bound a tool group
/// exactly like an `edit`/`replace` call does; only absorbable (invisible)
/// entries merge a run. So a turn of
///
///     thinking · 6 calls · edit · replace · replace · thinking · output
///
/// renders as thinking, the 6-call summary, the three always-full calls as
/// their own entries, then the closing thinking and the reply.
#[tokio::test]
async fn visible_entries_bound_a_tool_group() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // Open both thoughts so they render as visible entries — without
    // `/verbose`, which would also expand the tool group and push the summary
    // off a 40-row viewport.
    h.app.thinking_open.insert(0);
    h.app.thinking_open.insert(10);
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done: true,
        })
    };
    h.app.push_entry(Entry::reasoning("thinking about it"));
    for i in 0..6 {
        h.app.push_entry(tool(&format!("a{i}"), "shell"));
    }
    h.app.push_entry(tool("e1", "edit"));
    h.app.push_entry(tool("r1", "replace"));
    h.app.push_entry(tool("r2", "replace"));
    h.app.push_entry(Entry::reasoning("thinking again"));
    h.app.push_entry(Entry::assistant("the output"));

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    let row_of = |needle: &str| -> u16 {
        (0..40)
            .find(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"))
    };
    let rows_with = |needle: &str| -> Vec<u16> {
        (0..40)
            .filter(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .collect()
    };
    let replaces = rows_with("✓ replace");
    assert_eq!(
        replaces.len(),
        2,
        "both replace calls render as their own entries:\n{screen}"
    );
    assert_eq!(
        rows_with("ran 6 commands").len(),
        1,
        "the six calls fold into one summary:\n{screen}"
    );
    assert!(
        row_of("thinking about it") < row_of("ran 6 commands")
            && row_of("ran 6 commands") < row_of("✓ edit")
            && row_of("✓ edit") < replaces[0]
            && replaces[0] < replaces[1]
            && replaces[1] < row_of("thinking again")
            && row_of("thinking again") < row_of("the output"),
        "thinking · summary · edit · replace · replace · thinking · output:\n{screen}"
    );
}

/// An opened thought survives scrollback pruning, renumbered to its new index —
/// not folded, and not leaving a stale index that a later Reasoning entry would
/// inherit uninvited. (The 2026-08-06 correctness finding: `thinking_open` was
/// keyed by transcript index while `prune_scrollback` shifted every index.)
#[tokio::test]
async fn opened_thought_survives_scrollback_pruning_renumbered() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // A thought opened mid-transcript, at index 400 (the finding's repro).
    for i in 0..400 {
        h.app.push_entry(Entry::assistant(format!("filler {i}")));
    }
    h.app.push_entry(Entry::reasoning("the opened thought"));
    h.app.thinking_open.insert(400);
    // Grow past the 500-entry scrollback cap: each push past it evicts the
    // oldest entry, so 8 evictions land and every surviving index shifts down.
    for i in 0..107 {
        h.app.push_entry(Entry::assistant(format!("tail {i}")));
    }
    assert!(
        matches!(h.app.transcript()[392].kind, EntryKind::Reasoning { .. }),
        "the thought survived the pruning at its new index"
    );
    assert!(
        h.app.thinking_open.contains(&392),
        "the opened thought stays open, renumbered: {:?}",
        h.app.thinking_open
    );
    assert_eq!(
        h.app.thinking_open.len(),
        1,
        "no stale index left behind to open a different entry"
    );
}

/// A click on one call inside an expanded group pins the GROUP SUMMARY's own
/// top row, not the clicked call's row — the call path used to set
/// `pending_scroll_row` to the click row (which sits below the summary), so
/// every call-toggle while scrolled up shifted the whole view down by the gap.
#[tokio::test]
async fn toggle_tool_call_pins_the_group_summary_row_not_the_click_row() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, result: String| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: "shell".into(),
            args: "{}".into(),
            result,
            ok: true,
            done: true,
        })
    };
    h.app.push_entry(Entry::reasoning("thinking about it"));
    // 9 lines of output: past the 8-line preview cap, so each settled call's
    // body is a togglable preview with its own row hits. A tall terminal keeps
    // the whole group on screen (following), so the summary is a clickable rect.
    let long = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9".to_string();
    h.app.push_entry(tool("a", long.clone()));
    h.app.push_entry(tool("b", long.clone()));
    // Expand the group so every call renders as its own block under the summary.
    h.app.tool_groups.insert("a".to_string());
    let mut term = Terminal::new(TestBackend::new(60, 50)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();

    let head = crate::ui::tool_group_head(h.app.transcript(), 1).unwrap();
    let summary_top = h
        .app
        .tool_hits
        .iter()
        .find(|(_, i)| *i == head)
        .map(|(r, _)| r.y)
        .expect("the group summary has a clickable rect");
    let call_row = h
        .app
        .row_hits
        .iter()
        .find_map(|(r, hit)| match hit {
            crate::ui::RowHit::ToggleToolCall(idx) if *idx == 2 => Some(r.y),
            _ => None,
        })
        .expect("the call body has a toggle row");
    assert!(
        call_row > summary_top,
        "the clicked call sits below the summary — the pin must not use its row"
    );

    h.app.click_transcript(1, call_row);
    assert_eq!(h.app.pending_scroll_entry, Some(head));
    assert_eq!(
        h.app.pending_scroll_row,
        Some(summary_top),
        "the pin row is the summary's top, not the click row below it"
    );
}

/// Tool-only turns leave an empty assistant marker in the transcript; the
/// marker renders nothing, so the tool runs on either side of it merge into
/// one group — 3 + 2 + 6 calls become a single `ran 7 commands · read 4
/// files` summary rather than three. Only an `edit`/`replace` (or a visible
/// entry) breaks a run, and the absorbed turn markers keep their `#N
/// assistant` `/goto` labels at their transcript positions when the group is
/// expanded.
#[tokio::test]
async fn tool_runs_merge_across_invisible_turn_markers() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    let tool = |id: &str, name: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            result: format!("result-{id}"),
            ok: true,
            done: true,
        })
    };
    // Round 1: 1 shell + 2 reads; round 2: 1 shell + 1 read; round 3:
    // 5 shells + 1 read — 11 calls, 7 commands, 4 files, in the user's
    // exact mix, with an empty assistant turn between the rounds.
    for (id, name) in [("a0", "shell"), ("a1", "read"), ("a2", "read")] {
        h.app.push_entry(tool(id, name));
    }
    h.app.push_entry(Entry::assistant(""));
    for (id, name) in [("b0", "shell"), ("b1", "read")] {
        h.app.push_entry(tool(id, name));
    }
    h.app.push_entry(Entry::assistant(""));
    for (id, name) in [
        ("c0", "shell"),
        ("c1", "shell"),
        ("c2", "shell"),
        ("c3", "shell"),
        ("c4", "shell"),
        ("c5", "read"),
    ] {
        h.app.push_entry(tool(id, name));
    }

    // Collapsed: one merged summary, with the runs' own counts gone.
    let screen = h.render();
    assert!(
        screen.contains("ran 7 commands · read 4 files"),
        "the three runs merge into one summary:\n{screen}"
    );
    for split in ["ran 3 commands", "ran 2 commands", "ran 6 commands"] {
        assert!(
            !screen.contains(split),
            "no per-run summary {split:?}:\n{screen}"
        );
    }

    // Expanded: every call renders inside the group, in order — the turn
    // markers between the rounds render nothing of their own now.
    h.app.verbose = true;
    let mut term = Terminal::new(TestBackend::new(60, 120)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);
    let row_of = |needle: &str| -> u16 {
        (0..120)
            .find(|&y| {
                (0..59)
                    .filter_map(|x| {
                        buf.cell(Position::new(x, y))
                            .map(|c| c.symbol().to_string())
                    })
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("no row containing {needle:?}:\n{screen}"))
    };
    assert!(
        screen.contains("result-a0") && screen.contains("result-c5"),
        "every call renders:\n{screen}"
    );
    // The runs' calls stay in order across the invisible turn markers.
    assert!(
        row_of("result-a2") < row_of("result-b0") && row_of("result-b1") < row_of("result-c0"),
        "each round's calls stay in order:\n{screen}"
    );
    assert!(
        !screen.contains("assistant"),
        "the turn markers render nothing:\n{screen}"
    );
}

/// Expanding or collapsing a tool group while following the newest output must
/// not scroll away from the bottom: the view is already pinned there, and
/// there's nothing to keep in place. (The summary is the only group toggle —
/// clicking a call toggles just that call — so the group must stay in view for
/// the collapse half.)
#[tokio::test]
async fn collapsing_while_following_stays_at_the_bottom() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .transcript_mut()
        .retain(|e| !matches!(e.kind, EntryKind::Notice(_) | EntryKind::Header));
    // Filler so the transcript overflows the viewport — "following" is real.
    for i in 0..8 {
        h.app
            .push_entry(Entry::assistant(format!("filler {i}\nline\nline\nline")));
    }
    let tool = |id: &str| {
        Entry::now(EntryKind::Tool {
            id: id.into(),
            name: "shell".into(),
            args: "{}".into(),
            result: String::new(),
            ok: true,
            done: true,
        })
    };
    // The group sits at the bottom; its expanded calls fit, so the summary
    // stays on screen while following.
    h.app.push_entry(tool("a"));
    h.app.push_entry(tool("b"));

    let mut term = Terminal::new(TestBackend::new(40, 16)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert_eq!(h.app.scroll_offset, 0, "following the newest output");

    // Expand, then fold it back — either way the view stays pinned to the
    // bottom.
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    click_at(&mut h.app, 2, rect.y);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert_eq!(h.app.scroll_offset, 0, "still following after expanding");
    let (rect, _) = h.app.tool_hits.first().copied().expect("a tool hit rect");
    click_at(&mut h.app, 2, rect.y);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert_eq!(h.app.scroll_offset, 0, "still following after collapsing");
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
        name: "shell".into(),
        args: "{}".into(),
        result: "res".into(),
        ok: true,
        done: true, // the result row is the layout anchor below
    }));
    // The lone call collapses behind its summary; fan it out so its box — the
    // surface these background assertions check — renders.
    h.app.tool_groups.insert("c1".to_string());

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

    // The box carries its own bottom padding — one blank row on the tool
    // background — and the section's page-background pad closes the scrollback
    // after it, so the tinted surface never butts up against the input.
    assert_eq!(
        bg_at(last_content + 1),
        h.app.theme.user_bg,
        "the box's own bottom pad:\n{screen}"
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
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
    });
    h.app.sync_panes();

    // Half-write a message to main, and scroll back through its transcript.
    h.type_str("a thought for main");
    h.app.scroll_offset = 12;

    // Go to the sub-agent: a different conversation, so a clean box and its own
    // place — not main's leftovers.
    h.app.focus_pane(hrdr_app::PaneId(1));
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
    h.app.focus_pane(hrdr_app::PaneId::MAIN);
    assert_eq!(h.app.editor.content(), "a thought for main");
    assert_eq!(h.app.scroll_offset, 12, "main's place is kept");

    // And back to the sub-agent: so are its.
    h.app.focus_pane(hrdr_app::PaneId(1));
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

    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    let steering = hrdr_agent::steering_queue();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: steering.clone(),
        // Mid-turn: a message must be delivered as steering, not a new turn.
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
    });
    h.app.sync_panes();
    h.app.focus_pane(hrdr_app::PaneId(1));

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
    h.app.registry.record(
        1,
        &hrdr_agent::AgentEvent::Steered("check the auth module too".to_string()),
    );
    h.app.sync_panes();
    let sub_pane = h
        .app
        .panes
        .subs()
        .iter()
        .find(|p| p.id == hrdr_app::PaneId(1))
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
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
    });

    // Its output arrives as ToolOutput on the `task` call that spawned it.
    // The sub-agent works. It records what it emits on its own entry — that record
    // is what its pane is built from, so it does not matter whether anyone was
    // watching (or even whether the pane existed) while it ran.
    h.app.registry.record(
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
    // is what renders. The list rides in the transcript, so its rows answer to a
    // click — press and release — not to the press alone.
    click_at(&mut h.app, 3, sub_y);
    assert_eq!(h.app.panes.active(), hrdr_app::PaneId(1));

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
    click_at(&mut h.app, 3, main_y);
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

    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
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
            cost_partial: false,
            ..Default::default()
        },
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: false,
        compacting: false,
        done: true,
        delivered: false,
        pinned: false,
        transcript: None,
    });

    let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("claude://opus") && screen.contains("200.0k"),
        "on main, the bar shows the main agent's model and window:\n{screen}"
    );

    // Switch to the sub-agent: the bar switches with it.
    h.app.focus_pane(hrdr_app::PaneId(1));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("local://qwen3"),
        "the bar shows the sub-agent's provider://model:\n{screen}"
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
        h.app.registry.with(|v| v
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
        screen.contains("openai://gpt-5") && screen.contains("400.0k"),
        "the bar shows the newly-chosen model and its window:\n{screen}"
    );

    // Coming back to main restores main's chrome.
    h.app.focus_pane(hrdr_app::PaneId::MAIN);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("claude://opus"),
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
    let _data_home = isolated_data_home();
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

/// `reserve_session_id` claims the session id on the caller's thread — so the
/// sub-agent transcript dir is named before the turn runs — but writes the
/// file off-thread, so the first Enter no longer blocks the UI on the disk
/// (the mint is synchronous; the serialize + two-fsync write is not). The
/// write still lands, and under the CURRENT cwd's slug, not the empty one a
/// brand-new state carries.
#[tokio::test]
async fn reserve_session_id_defers_the_first_write_off_thread() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;

    h.app.reserve_session_id("first message");
    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("the id is claimed synchronously");
    // The write is off-thread; wait for it to land before reading the file.
    h.app.await_saves().await;
    let loaded = hrdr_app::Session::load(&h.app.current_cwd(), &id)
        .expect("session file written after the deferred save");
    assert_eq!(
        loaded
            .state
            .messages
            .first()
            .and_then(|m| m.content.as_deref()),
        Some("first message"),
        "the seeded mirror round-trips"
    );
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
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.maybe_deliver_background();
    h.app.registry.end_turn(hrdr_agent::MAIN_KEY);

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
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
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
    *h.app.todos.lock().unwrap() = vec![
        hrdr_agent::Todo {
            content: "ship it".to_string(),
            id: 0,
            status: "in_progress".to_string(),
            evidence: None,
        },
        hrdr_agent::Todo {
            content: "wait here".to_string(),
            id: 0,
            status: "pending".to_string(),
            evidence: None,
        },
        hrdr_agent::Todo {
            content: "landed".to_string(),
            id: 0,
            status: "completed".to_string(),
            evidence: None,
        },
        hrdr_agent::Todo {
            content: "skip it".to_string(),
            id: 0,
            status: "cancelled".to_string(),
            evidence: None,
        },
    ];

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

    // No border glyphs, and no title, anywhere on the panel. (The last column
    // is the transcript's scrollbar, which is not the panel's chrome.)
    for y in text_y - 1..=text_y + 1 {
        let r: String = row(y).chars().take(49).collect();
        for ch in ['┌', '┐', '└', '┘', '│', '─'] {
            assert!(!r.contains(ch), "border glyph {ch:?} on row {y}:\n{screen}");
        }
    }
    assert!(!screen.contains("todos"), "no title:\n{screen}");

    // The prompt's background across the block's width, on the padding rows too.
    // The last column is the transcript's scrollbar gutter — the panel is a
    // block in the scrollback now, so it stops where every other block does.
    for x in [0, 2, 48] {
        for y in [text_y - 1, text_y, text_y + 1] {
            assert_eq!(cell(x, y).bg, h.app.theme.user_bg, "({x},{y}):\n{screen}");
        }
    }
    // One blank row above and below the panel content. (Dropping the last
    // column drops the transcript's scrollbar, which paints over every row.)
    let panel_row = |y: u16| -> String { row(y).chars().take(49).collect() };
    let last_text_y = text_y + 2;
    assert_eq!(
        without_bar(&panel_row(text_y - 1)),
        "",
        "top padding:\n{screen}"
    );
    assert_eq!(
        without_bar(&panel_row(last_text_y + 1)),
        "",
        "bottom padding:\n{screen}"
    );

    // The rule, then the rest of the left padding, then the content. The
    // status mark leads — the in_progress marker is a braille SPINNER frame
    // (first frame at t≈0) — before the item's stable `#N` reference.
    let first_frame = "⠹";
    assert!(
        row(text_y).starts_with(&format!(
            "{} {first_frame} #0 ship it",
            crate::ui::BORDER_BAR
        )),
        "{screen}"
    );
    assert!(
        screen.contains("  #0 wait here"),
        "pending marker: {screen}"
    );
    // The two finished tasks are folded away behind one row.
    assert!(
        !screen.contains("landed"),
        "completed tasks are hidden: {screen}"
    );
    assert!(!screen.contains("skip it"), "cancelled ones too: {screen}");
    assert!(
        screen.contains("▸ 2 finished — click to show"),
        "the panel offers them: {screen}"
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

/// Every live panel carries a blank row above itself, so it never butts up
/// Finished tasks are folded away behind the panel's last row, and clicking
/// that row unfolds them — the panel is about what is left, but what was done is
/// still in the list until it ages out.
#[tokio::test]
async fn clicking_the_finished_row_unfolds_the_done_todos() {
    let mut h = Harness::new(vec![]).await;
    *h.app.todos.lock().unwrap() = vec![
        hrdr_agent::Todo {
            content: "ship it".to_string(),
            id: 0,
            status: "in_progress".to_string(),
            evidence: None,
        },
        hrdr_agent::Todo {
            content: "landed".to_string(),
            id: 0,
            status: "completed".to_string(),
            evidence: None,
        },
    ];

    let mut term = Terminal::new(TestBackend::new(50, 24)).unwrap();
    let draw = |h: &mut Harness, term: &mut Terminal<TestBackend>| -> String {
        term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
        buffer_to_string(term.backend().buffer())
    };

    let screen = draw(&mut h, &mut term);
    assert!(!screen.contains("landed"), "folded away:\n{screen}");
    let toggle_y = (0..24)
        .find(|&y| {
            screen
                .lines()
                .nth(y as usize)
                .is_some_and(|l| l.contains("1 finished"))
        })
        .expect("the panel offers the finished task");

    // The row is a click target that rides in the transcript, like a tool block.
    click_at(&mut h.app, 4, toggle_y);
    assert!(h.app.show_done_todos, "the click unfolded them");
    let screen = draw(&mut h, &mut term);
    assert!(
        screen.contains("✓ #") && screen.contains("landed"),
        "now shown:\n{screen}"
    );
    assert!(
        screen.contains("▾ 1 finished — click to hide"),
        "and the row folds them back:\n{screen}"
    );

    click_at(&mut h.app, 4, toggle_y);
    let screen = draw(&mut h, &mut term);
    assert!(!h.app.show_done_todos, "clicking again folds them");
    assert!(!screen.contains("landed"), "hidden again:\n{screen}");
}

/// Every live panel carries a blank row above itself, so it never butts up
/// against the block before it (the scrollback's last block no longer trails
/// one) — and that row costs nothing when the panel isn't rendered.
#[tokio::test]
async fn each_panel_owns_a_blank_row_above_it() {
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
                // Without the last column: the transcript's scrollbar paints
                // there, and it is not part of any block.
                (0..49)
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
        id: 0,
        status: "in_progress".to_string(),
        evidence: None,
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

/// Two scroll buttons — "↓ Press END ↓" and "↑ Press HOME ↑" — float side by
/// side two rows above the input pane when scrolled up, and click to the newest
/// output / the top of the session respectively.
#[tokio::test]
async fn the_scroll_buttons_float_above_the_input_and_are_clickable() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut h = Harness::new(vec![]).await;
    for i in 0..30 {
        h.app.push_entry(Entry::system(format!("filler {i}")));
    }
    h.type_str("draft");

    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.end_button.is_none() && h.app.home_button.is_none(),
        "no buttons while following"
    );

    // Scroll up: both buttons appear.
    h.app.scroll_offset = 5;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let end = h.app.end_button.expect("the END button is drawn");
    let home = h.app.home_button.expect("the HOME button is drawn");
    let buf = term.backend().buffer();
    let screen = buffer_to_string(buf);

    // Read only a button's own columns — the rest of the row is transcript.
    let label = |rect: crate::app::HitRect| -> String {
        (rect.x..rect.x + rect.w)
            .filter_map(|x| {
                buf.cell(Position::new(x, rect.y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let end_label = label(end);
    let end_trimmed = end_label.trim();
    assert!(
        end_trimmed.starts_with('↓') && end_trimmed.ends_with('↓'),
        "END arrows: {end_label:?}\n{screen}"
    );
    assert!(end_trimmed.contains("Press END"), "{screen}");
    let home_label = label(home);
    let home_trimmed = home_label.trim();
    assert!(
        home_trimmed.starts_with('↑') && home_trimmed.ends_with('↑'),
        "HOME arrows: {home_label:?}\n{screen}"
    );
    assert!(home_trimmed.contains("Press HOME"), "{screen}");
    // Side by side: HOME sits to the right of END, on the same row.
    assert_eq!(end.y, home.y, "same row:\n{screen}");
    assert!(home.x >= end.x + end.w, "HOME right of END:\n{screen}");

    // Directly above the input pane, on the layout's spacer row — close enough
    // to read as part of the input, without covering the pane itself.
    let pane_top = (0..20)
        .find(|&y| buf.cell(Position::new(2, y)).unwrap().bg == h.app.theme.user_bg)
        .expect("the input pane renders");
    assert_eq!(end.y + 1, pane_top, "one row above the pane:\n{screen}");

    // Clicking HOME jumps to the top of the session.
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: home.x + 1,
        row: home.y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    assert_eq!(
        h.app.scroll_offset, h.app.max_scroll,
        "the HOME click jumped to the top"
    );

    // Clicking END resumes following the newest output.
    h.app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: end.x + 1,
        row: end.y,
        modifiers: crossterm::event::KeyModifiers::empty(),
    });
    assert_eq!(h.app.scroll_offset, 0, "the END click resumed following");
}

/// Compaction is the one loader that shows in normal mode: nothing else on
/// screen says the conversation is being summarized, so its indicator is not
/// gated behind `/verbose` the way the inference loader is.
#[tokio::test]
async fn the_compacting_indicator_shows_even_in_normal_mode() {
    let mut h = Harness::new(vec![]).await;
    h.app
        .registry
        .update(hrdr_agent::MAIN_KEY, |e| e.compacting = true);

    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("compacting context — summarizing the conversation"),
        "the compacting indicator shows without /verbose:\n{screen}"
    );

    // With compaction over, the inference loader is still verbose-only.
    h.app
        .registry
        .update(hrdr_agent::MAIN_KEY, |e| e.compacting = false);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("inferring") && !screen.contains("generating"),
        "the inference loader stays verbose-only:\n{screen}"
    );
}

/// The loader sits exactly one blank row off the surface above it, whether
/// that surface is tinted (a user prompt) or untinted (an assistant reply).
/// Its separator is conditional on the block above — an untinted block's own
/// bottom pad already is the blank row, so the loader never stacks a second
/// one under it.
#[tokio::test]
async fn the_loader_is_one_blank_row_off_the_block_above() {
    // Draw and count the blank rows between the last transcript block's content
    // and the loader's first non-empty line (columns skip the scrollbar).
    let blanks_above = |h: &mut Harness| -> (u16, u16) {
        let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
        term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
        let buf = term.backend().buffer();
        let row_at = |y: u16| -> String {
            (0..59)
                .filter_map(|x| {
                    buf.cell(Position::new(x, y))
                        .map(|c| c.symbol().to_string())
                })
                .collect()
        };
        let y = (0..30u16)
            .find(|&y| row_at(y).contains("inferring"))
            .expect("the loader renders");
        let blanks = (0..y)
            .rev()
            .take_while(|&by| row_at(by).trim().is_empty())
            .count() as u16;
        (y, blanks)
    };

    // Tinted above: the user prompt's solid pad, then the loader's separator.
    let mut h = Harness::new(vec![]).await;
    h.app.push_entry(Entry::user("the prompt"));
    h.app.verbose = true;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    let (y_tinted, blanks_tinted) = blanks_above(&mut h);

    // Untinted above: the assistant reply's own blank bottom pad.
    let mut h = Harness::new(vec![]).await;
    h.app.push_entry(Entry::assistant("the reply"));
    h.app.verbose = true;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    let (y_untinted, blanks_untinted) = blanks_above(&mut h);

    assert_eq!(
        blanks_tinted, 1,
        "one blank under a tinted block (loader at row {y_tinted})"
    );
    assert_eq!(
        blanks_untinted, 1,
        "one blank under an untinted block (loader at row {y_untinted})"
    );
}

/// The loader tracks the *model*, not the turn: it hides while the model's tool
/// calls run, because the model is idle then — and its clock stops with it, so a
/// slow tool doesn't inflate the turn's reported inference time.
#[tokio::test]
async fn the_loader_stops_while_the_models_tools_run() {
    use hrdr_agent::AgentEvent;

    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.resume_inference_for_test();
    // The clock is the *agent's*, kept on its registry entry — the main agent's is
    // read exactly the way a sub-agent's is.
    let turn = |h: &Harness| {
        h.app
            .registry
            .turn(hrdr_agent::MAIN_KEY)
            .expect("the session's agent is in the registry")
    };
    assert!(turn(&h).inferring(), "the model works as the turn opens");

    // A tool round opens: the model handed off and is now idle.
    h.inject(AgentEvent::ToolStart {
        id: "a".into(),
        name: "shell".into(),
        args: "{}".into(),
    });
    h.inject(AgentEvent::ToolStart {
        id: "b".into(),
        name: "shell".into(),
        args: "{}".into(),
    });
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
    let end = |id: &str| AgentEvent::ToolEnd {
        id: id.into(),
        name: "shell".into(),
        result: "ok".into(),
        ok: true,
    };
    h.inject(end("a"));
    assert!(
        !turn(&h).inferring(),
        "one tool of two is still outstanding"
    );

    // The last one hands control back: the model works again, and the clock runs.
    h.inject(end("b"));
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
    h.app.verbose = true; // the loader is verbose-only chrome; this test is about it
    let sub = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: Default::default(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
    });

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // The sub-agent is working; the main agent is idle. On main: no loader — it is
    // not the main agent that is busy.
    h.app.registry.begin_turn(1);
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("inferring") && !screen.contains("generating"),
        "the main agent is idle, so its view shows no loader:\n{screen}"
    );

    // Switch to the sub-agent: the loader is there, running *its* clock.
    h.app.focus_pane(hrdr_app::PaneId(1));
    h.app
        .registry
        .record(1, &hrdr_agent::AgentEvent::Text("looking".into()));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("generating"),
        "the agent on screen is working, so its loader shows:\n{screen}"
    );

    // Its tool runs: the model is idle, so the loader hides — its own pane, its own
    // clock.
    h.app.registry.record(
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

/// The loader closes the transcript while a turn runs: it is the last thing in
/// the scrollback — below the todo panel, above the input — with a blank row
/// above it, and it scrolls with everything else instead of holding a row of
/// every frame for itself.
#[tokio::test]
async fn the_generating_line_closes_the_transcript() {
    let mut h = Harness::new(vec![]).await;
    h.app.verbose = true; // the loader is verbose-only chrome; this test is about it
    h.type_str("draft");
    // A panel between the loader and the input, so "top-most" is a real claim.
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "ship it".to_string(),
        id: 0,
        status: "in_progress".to_string(),
        evidence: None,
    }];
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
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
    // It heads the panels: above the todo list, and above the input pane.
    assert!(loader_y < find("ship it"), "above the todos:\n{screen}");
    assert!(loader_y < find("draft"), "above the input:\n{screen}");

    // A blank, untinted row above it (the panel before it ends with its own
    // tinted pad, so this one is the spacer), and the loader row itself is
    // untinted — it is a status line, not a block.
    assert_eq!(
        without_bar(&row(loader_y - 1).chars().take(55).collect::<String>()),
        "",
        "blank row above the loader:\n{screen}"
    );
    assert_eq!(
        cell(loader_y - 1).bg,
        Color::Reset,
        "untinted spacer:\n{screen}"
    );
    assert_eq!(
        cell(loader_y).bg,
        Color::Reset,
        "untinted loader:\n{screen}"
    );

    // Scrolling up takes it away with the rest of the scrollback — that is the
    // point of moving it in: nothing outside the transcript owns a row.
    h.app.scroll_offset = h.app.max_scroll;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let scrolled = buffer_to_string(term.backend().buffer());
    assert!(
        !scrolled.contains("inferring"),
        "the loader scrolls with the transcript:\n{scrolled}"
    );
}

/// The loader — the "inferring"/"generating" line — is verbose-only chrome:
/// normal mode hides it (the status bar carries the turn state), `/verbose on`
/// brings it back.
#[tokio::test]
async fn the_loader_shows_only_in_verbose_mode() {
    let mut h = Harness::new(vec![]).await;
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.resume_inference_for_test();
    assert!(!h.app.verbose, "normal mode is the default");
    assert!(
        !h.render().contains("inferring"),
        "the loader is hidden in normal mode"
    );

    h.app.verbose = true;
    assert!(
        h.render().contains("inferring"),
        "/verbose on brings the loader back"
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
        name: "shell".into(),
        args: r#"{"command":"echo hi"}"#.into(),
        result: "hi".into(),
        ok: true,
        done: true,
    }));
    // The lone tool call collapses behind its summary; fan it out so its box —
    // the surface the bar assertions check — renders.
    h.app.verbose = true;
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
    let tool_y = row_of("✓ shell");
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
    // `/help` is a data command: its output lives in the Esc-dismissible popup.
    let help = h
        .app
        .popup
        .as_ref()
        .map(|p| p.text.clone())
        .expect("/help popup");
    assert!(help.contains("Keys:"), "the engine's keys:\n{help}");
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

    // Scrolled up: the END banner, in the warn colors.
    h.app.scroll_offset = 5;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let follow = h.app.end_button.expect("the END banner is drawn");
    assert!(label(&term, follow).contains("Press END"));
    let (fg, bg, m) = cell(&term, follow.x + 1, follow.y);
    assert_eq!((fg, bg), (Color::Black, h.app.theme.warn));
    assert!(m.contains(ratatui::style::Modifier::BOLD), "bold");

    // Arming the quit takes the same row, in the error colors, flanked by its
    // icon, and is not clickable.
    h.app.quit_armed = true;
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    assert!(
        h.app.end_button.is_none(),
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
    // The save is written off-thread; wait for its SaveDone before reading
    // the file.
    h.save_drain().await;

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
    // releases the agent lock), run the final autosave, then flush the
    // off-thread save before the process exits. Without the reap-then-save the
    // loop performs on `should_quit`, the best-effort save in `cancel_turn`
    // skips while the lock is still held.
    h.app.reap_cancelled_turn().await;
    h.app.autosave();
    h.app.await_saves().await;

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
    assert!(screen.contains("OAuth"), "auth-method column:\n{screen}");

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
        provider: "openai".to_string(),
        label: "ChatGPT".to_string(),
    });
    // A late result from an older login (id 1) must not disturb id 2.
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 1,
        provider: "openai".to_string(),
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
        provider: "openai".to_string(),
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
        provider: "openai".to_string(),
        token_saved: true,
        error: None,
    });
    assert!(
        h.app.login_modal.is_none(),
        "a cancelled login's late result does nothing"
    );
}

/// A matching, successful ChatGPT browser login runs the switch transaction: the
/// modal closes, a usable default model is seeded, and the model picker opens so
/// the user can switch to another entitled model.
///
/// The login targets the merged built-in `openai` (the OAuth credential lives in
/// the `openai` slot), which declares no default model — so the login slice seeds
/// the ChatGPT subscription default (`gpt-5.5`) as the model last used on `openai`.
/// The switch then lands on a talkable model instead of stalling on `NeedsModel`,
/// and the picker still opens for a deliberate choice.
#[tokio::test]
async fn browser_login_success_seeds_default_and_opens_the_model_picker() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 7,
        provider: "openai".to_string(),
        label: "ChatGPT subscription".to_string(),
    });
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 7,
        provider: "openai".to_string(),
        token_saved: true,
        error: None,
    });
    assert!(
        h.app.login_modal.is_none(),
        "the switch transaction closed the modal"
    );
    assert!(
        h.app.model_selector.is_some(),
        "the post-login model picker opens to choose an entitled model"
    );
    // The subscription default was seeded as the model last used on `openai`, so
    // the session lands on a talkable model even without a picker choice.
    let seeded = hrdr_agent::model_for_provider(
        &hrdr_agent::ProviderName::new("openai"),
        &hrdr_agent::AgentConfig::default(),
    )
    .expect("a default model is recorded for openai after ChatGPT login");
    assert_eq!(seeded.model(), hrdr_agent::CHATGPT_DEFAULT_MODEL);
}

/// A failed (matching) browser login reports the error and closes the modal
/// without switching.
#[tokio::test]
async fn browser_login_failure_reports_and_closes() {
    let mut h = Harness::new(vec![]).await;
    h.app.login_modal = Some(crate::app::LoginModal::Authorizing {
        login_id: 3,
        provider: "openai".to_string(),
        label: "ChatGPT".to_string(),
    });
    h.app.on_browser_login(hrdr_app::BrowserLoginOutcome {
        login_id: 3,
        provider: "openai".to_string(),
        token_saved: false,
        error: Some("authorization was rejected".to_string()),
    });
    assert!(
        h.app.login_modal.is_none(),
        "a failed login closes the modal"
    );
    assert!(
        h.app
            .toasts
            .last_body()
            .is_some_and(|t| t.contains("login failed")),
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
            .any(|c| c.provider == "openai" && c.model == "gpt-5.5"),
        "the entitled ChatGPT row is merged into the picker, under the CANONICAL \
         provider name every other row carries"
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
    // The lone tool call collapses behind its summary; fan it out so the
    // shell output renders.
    h.app.verbose = true;
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
    // closed tool block, not "whenever the next turn saves". The write is
    // off-thread; wait for its SaveDone first.
    h.save_drain().await;
    // The ToolEnd autosave filed the session under the session's own cwd (the
    // state mirror it wrote); `current_cwd()` would fall back to the process
    // cwd while the follow-up turn still holds the agent lock, so load where
    // the save actually went.
    let cwd = h.app.state().cwd.clone();
    let id = h
        .app
        .state()
        .id
        .clone()
        .expect("the !command's autosave assigned a session id");
    let loaded = hrdr_app::Session::load(&cwd, &id).expect("session file written on ToolEnd");
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

/// `:!command` is the `!` shell escape under the ex-style prefix — vim
/// muscle memory types `:!git status` and means the shell, not a skill
/// named `!`. It takes the exact `!` path: no model turn spawns, the
/// output streams into a tool block, and ToolEnd commits the note.
#[cfg(unix)]
#[tokio::test]
async fn colon_bang_runs_the_same_user_shell_command() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.type_str(":!echo hello-from-colon-bang");
    h.press(KeyCode::Enter);
    assert!(!h.app.running(), "no model turn spawns for a :!command");
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Tool { .. })),
        "the tool block opened synchronously"
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
    // The lone tool call collapses behind its summary; fan it out so the
    // shell output renders.
    h.app.verbose = true;
    let screen = h.render();
    assert!(
        screen.contains("hello-from-colon-bang"),
        "output in the transcript:\n{screen}"
    );
    let noted = h.app.agent.try_lock().is_ok_and(|a| {
        a.messages_owned().iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("hello-from-colon-bang") && c.contains("I ran"))
        })
    });
    assert!(noted, "the history note landed with ToolEnd");
}

/// A second `!command` typed while one is already running is refused before
/// anything is minted or recorded: no tool id, no session reservation, no
/// ToolStart. On the pre-fix code the refused submission opened a second
/// `done: false` block that nothing ever closed, and it resurfaced as a
/// settled-failed block on resume.
#[cfg(unix)]
#[tokio::test]
async fn a_second_bang_command_while_one_runs_leaves_no_phantom_block() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    // A command that stays alive long enough for a second submission.
    h.type_str("!sleep 2");
    h.press(KeyCode::Enter);
    assert!(
        h.app
            .transcript()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Tool { done: false, .. })),
        "the first !command's block opened"
    );
    // The second !command is refused, with no transcript artifact.
    h.type_str("!echo hi");
    h.press(KeyCode::Enter);
    let open = h
        .app
        .transcript()
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::Tool { done: false, .. }))
        .count();
    assert_eq!(open, 1, "the rejected command must not open a second block");
    assert!(
        h.app
            .toasts
            .last_body()
            .is_some_and(|t| t.contains("already running")),
        "the refusal message is shown"
    );
    // Drain until the first block closes; there must still be exactly one
    // tool block total — the rejected command never appears.
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
    let total = h
        .app
        .transcript()
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::Tool { .. }))
        .count();
    assert_eq!(
        total,
        1,
        "only the first command's block exists: {:?}",
        h.app
            .transcript()
            .iter()
            .map(|e| &e.kind)
            .collect::<Vec<_>>()
    );
}

/// **`!command` runs as the user, with no OS confinement — and nothing else does.**
///
/// The relief valve the whole design now leans on. With escalation removed this is
/// the only way to run something the sandbox would refuse, so a refactor that
/// routed the bang path through `sandboxed_shell_command` "for consistency" would
/// delete the last way out and no other test would notice.
///
/// Proved as a property of the real backend rather than by inspecting a flag: the
/// session runs in `read` mode, where an *agent's* shell may write nowhere at all,
/// and the probe is a write into the working directory. Unconfined, it lands.
/// Confined — under Landlock or Seatbelt alike, since `read` grants no
/// writable root on any of them — it would die on EROFS. Read mode rather than a
/// path outside the roots on purpose: in `write` mode `env::temp_dir()` is
/// writable, and every path a test can write to lives under it.
#[cfg(unix)]
#[tokio::test]
async fn a_bang_command_runs_unsandboxed() {
    let _data_home = isolated_data_home();
    let mut h = Harness::read_only_sandbox().await;
    // The confinement is real for the agent: nothing is writable to it.
    assert!(
        h.app
            .agent
            .try_lock()
            .expect("idle")
            .sandbox_policy()
            .writable_roots
            .is_empty(),
        "read mode must grant the agent no writable root, or this proves nothing"
    );

    let probe = h._tmp.path().join("bang-ran-as-the-user");
    h.type_str(&format!("!touch {}", probe.display()));
    h.press(KeyCode::Enter);

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

    assert!(
        probe.exists(),
        "the user's own command must not be confined — it wrote nothing:\n{}",
        h.render()
    );
}

/// A `!command` that dumps far more than the streaming cap (256 KiB) must not
/// grow the in-memory buffer to match: the bytes actually forwarded over the
/// channel for display stay bounded well below what the command wrote, and
/// the process still runs to completion — the pipes are drained the whole
/// time regardless of the cap, so nothing backs up and deadlocks. Unix-only,
/// like the other `!command` tests.
#[cfg(unix)]
#[tokio::test]
async fn bang_command_output_is_capped_while_streaming_not_just_at_the_end() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    // ~2 MB of output — comfortably past the 256 KiB streaming cap and the
    // 50_000-char final display cap alike.
    h.type_str("!yes 0123456789abcdef0123456789abcdef0123456789abcdef | head -c 2000000");
    h.press(KeyCode::Enter);
    assert!(h.app.user_shell.is_some(), "the shell task is tracked");

    let mut forwarded_bytes = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !h
        .app
        .transcript()
        .iter()
        .any(|e| matches!(&e.kind, EntryKind::Tool { done: true, .. }))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "shell events never arrived — the pipes may have backed up and deadlocked"
        );
        match tokio::time::timeout(std::time::Duration::from_secs(10), h.rx.recv()).await {
            Ok(Some(msg)) => {
                if let TurnMsg::UserShell(hrdr_agent::AgentEvent::ToolOutput { chunk, .. }, _) =
                    &msg
                {
                    forwarded_bytes += chunk.len();
                }
                h.app.on_turn_msg(msg);
            }
            Ok(None) => panic!("channel closed before the shell finished"),
            Err(_) => panic!("timed out waiting for shell events"),
        }
    }

    assert!(
        forwarded_bytes < 512 * 1024,
        "streaming forwarded {forwarded_bytes} bytes for a 2MB command — the \
         in-memory cap did not stop growth while the process was running"
    );
    let done = h.app.transcript().iter().any(|e| {
        matches!(&e.kind, EntryKind::Tool { done: true, ok: true, result, .. }
            if result.len() < 60_000)
    });
    assert!(done, "the command finished cleanly with a bounded result");
}

/// A `!command`'s output settles *inside* its block, and a person's own shell
/// output is not cut to the model's line budget.
///
/// Two regressions from routing `!` through the model's shell path. The live
/// stream is forwarded by a second task pushing onto the same channel as the
/// settle, so `ToolEnd` could overtake output still in flight and leave it
/// landing in a closed block. And `max_output` was raised to 50_000 bytes while
/// `max_output_lines` kept its default of 50, so `!seq 1 500` — or any `!git
/// log` — settled to 50 lines and a spool pointer.
#[cfg(unix)]
#[tokio::test]
async fn bang_command_output_lands_before_the_block_closes_and_is_not_line_capped() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.type_str("!seq 1 500");
    h.press(KeyCode::Enter);

    let mut ended = false;
    let mut output_after_end = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
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
        match tokio::time::timeout(std::time::Duration::from_secs(10), h.rx.recv()).await {
            Ok(Some(msg)) => {
                match &msg {
                    TurnMsg::UserShell(hrdr_agent::AgentEvent::ToolOutput { .. }, _) if ended => {
                        output_after_end += 1;
                    }
                    TurnMsg::UserShell(hrdr_agent::AgentEvent::ToolEnd { .. }, _) => ended = true,
                    _ => {}
                }
                h.app.on_turn_msg(msg);
            }
            Ok(None) => panic!("channel closed before the shell finished"),
            Err(_) => panic!("timed out waiting for shell events"),
        }
    }

    assert_eq!(
        output_after_end, 0,
        "output arrived after the block had already settled"
    );
    let result = h
        .app
        .transcript()
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::Tool {
                done: true, result, ..
            } => Some(result.clone()),
            _ => None,
        })
        .expect("a settled tool block");
    // Not `contains("500")`: over the cap the head/tail view keeps the *last*
    // lines, so the final line is present either way. The count is what tells
    // truncation from the whole thing.
    assert!(
        result.lines().count() >= 500,
        "all 500 lines survived, not just the model's default 50 plus a spool \
         pointer — got {} lines:\n{result}",
        result.lines().count()
    );
}

/// A double Esc cancels a running `!command`: the child is killed, the tool
/// block closes as "(cancelled)", the cancellation note commits to history +
/// disk like any other transcript entry, and the slot frees for the next
/// command.
#[cfg(unix)]
#[tokio::test]
async fn esc_cancels_a_running_user_shell_command() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.type_str("!sleep 30");
    h.press(KeyCode::Enter);
    assert!(h.app.user_shell.is_some(), "the shell task is tracked");

    h.press(KeyCode::Esc);
    assert!(h.app.user_shell.is_some(), "the first Esc only arms");
    h.press(KeyCode::Esc);
    assert!(
        h.app.user_shell.is_none(),
        "the second Esc cleared the slot"
    );
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

/// Esc-Esc cancels a turn whose *model-driven* tool is mid-flight: the batch
/// guard aborts the spawned tool task (a dropped `JoinHandle` alone would just
/// detach it), the shell tool's future drops, and `kill_on_drop` kills the
/// child — so the `touch` behind a `sleep 3` never fires. Without the abort
/// the spawned task would keep running to its own five-minute timeout and the
/// marker would appear ~3s after the tool started; the assert below waits past
/// that deadline, so a detached task fails it.
#[cfg(unix)]
#[tokio::test]
async fn esc_esc_cancels_a_turn_mid_tool_call_and_aborts_the_tool_task() {
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "shell".to_string(),
            args: r#"{"command":"sleep 3; touch batch-cancel-marker"}"#.to_string(),
        },
        // Consumed only if the turn survived the cancel: the follow-up round's
        // request for another completion.
        MockReply::Text("the cancel should have prevented this".to_string()),
    ])
    .await;
    let marker = h._tmp.path().join("batch-cancel-marker");

    // Start the turn, but do NOT pump to idle — the tool runs for 3s.
    h.type_str("run the sleep");
    h.press(KeyCode::Enter);

    // Wait until the shell tool is actually in flight: its `ToolStart` has
    // landed and been applied to the app's transcript (open, not done).
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            while let Ok(msg) = h.rx.try_recv() {
                h.app.on_turn_msg(msg);
            }
            let started = h.app.transcript().iter().any(|e| {
                matches!(&e.kind, EntryKind::Tool { name, done, .. }
                    if name == "shell" && !done)
            });
            if started {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the shell tool started within 10s");
    // Let the child actually spawn before cancelling.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    assert!(!h.app.running(), "the turn was cancelled");
    // Await the aborted turn task so its future — and with it the batch guard
    // — is dropped before we check the marker.
    h.app.reap_cancelled_turn().await;

    // Well past the `sleep 3` deadline: if the tool task had been detached
    // instead of aborted, its `touch` would have landed by now.
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(
        !marker.exists(),
        "the tool task was aborted, not detached — the child must not survive \
         the cancel: {} exists",
        marker.display()
    );

    // The open tool block settles as a failed call (the same repair a resumed
    // session gets), not a live spinner.
    let settled = h.app.transcript().iter().any(|e| {
        matches!(&e.kind, EntryKind::Tool { name, done, ok, .. }
            if name == "shell" && *done && !*ok)
    });
    assert!(settled, "the cancelled tool block settled as failed");
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

/// A `/theme` switch invalidates every cached block and the next render
/// rebuilds them with the new theme's colors — the bug that made old colors
/// persist in cached rows.
#[tokio::test]
async fn a_theme_switch_invalidates_transcript_cache() {
    let mut h = Harness::new(vec![]).await;
    h.app.push_entry(Entry::user("hello"));
    h.app.push_entry(Entry::assistant("hi there"));

    // First render populates the block cache. Indices 2=user, 3=assistant
    // (0=header, 1=notice are the initial entries; the header is never cached).
    h.render();
    let before: Vec<Option<usize>> = (2..=3).map(crate::ui::block_cache_ptr).collect();
    assert!(
        before.iter().all(Option::is_some),
        "every entry should be cached after one render"
    );

    // Change theme via `/theme <name>` — goes through TuiHost::set_theme,
    // which now calls clear_transcript_cache().
    h.app.submit_input("/theme catppuccin-mocha".to_string());

    // Cache is now empty — old pointers should be gone.
    let after_cmd: Vec<Option<usize>> = (2..=3).map(crate::ui::block_cache_ptr).collect();
    assert!(
        after_cmd.iter().all(Option::is_none),
        "theme switch must clear the render cache: {after_cmd:?}"
    );

    // Re-render builds rows with the new theme.
    h.render();
    let after_render: Vec<Option<usize>> = (2..=3).map(crate::ui::block_cache_ptr).collect();
    assert!(
        after_render.iter().all(Option::is_some),
        "re-render after theme switch must repopulate the cache"
    );

    // Verify the rendered output actually shows the new theme's colors.
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    let theme = &h.app.theme;
    let row_text = |y: u16| -> String {
        (0..59)
            .filter_map(|x| buf.cell(Position::new(x, y)).map(|c| c.symbol()))
            .collect()
    };
    let reply_y = (0..20)
        .find(|&y| row_text(y).contains("hi there"))
        .expect("assistant text visible after theme switch");
    // The assistant block has no background (Reset), but its text foreground
    // must be the new theme's assistant color.
    let cell = buf.cell(Position::new(2, reply_y)).unwrap();
    assert_eq!(
        cell.fg, theme.assistant,
        "assistant text should wear the new theme's assistant color after re-render"
    );
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
    let printed = h
        .app
        .popup
        .as_ref()
        .map(|p| p.text.clone())
        .expect("the /help popup");
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
        error: None,
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
    h.app.skills = hrdr_app::discover_skills(
        std::path::Path::new(&h.app.current_cwd()),
        hrdr_agent::ProjectInstructions::Load,
    );
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
    // The user turn carries an immutable timestamp prefix; strip it to compare
    // the expanded template the model received.
    assert_eq!(
        hrdr_agent::strip_user_timestamp(&user),
        "Run the release checklist for v0.3"
    );
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
    h.type_str("/statusbar tr");
    let screen = h.render();
    assert!(screen.contains("truncate"), "candidate offered:\n{screen}");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.content(), "/statusbar truncate ");

    // Theme names complete too (built-ins are always registered).
    h.app.editor.set_content("");
    h.type_str("/theme dra");
    assert!(h.render().contains("dracula"), "theme name offered");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.editor.content(), "/theme dracula ");
}

/// Enter on the popup ACCEPTS the highlighted suggestion into the input — it
/// does not submit. Accepting a completion and sending are two distinct presses:
/// the first Enter fills the box (and suppresses the popup), the second sends it.
#[tokio::test]
async fn enter_accepts_a_partial_completion_then_a_second_enter_submits() {
    let mut h = Harness::new(vec![]).await;
    // A partial slash command: the popup completes to /statusbar.
    h.type_str("/statusba");
    assert!(h.render().contains("/statusbar"), "popup offers /statusbar");

    // First Enter accepts the suggestion into the input; it does NOT submit
    // (submitting would clear the box).
    h.press(KeyCode::Enter);
    assert_eq!(
        h.app.editor.content(),
        "/statusbar ",
        "Enter filled the command into the box without sending it"
    );

    // The popup is suppressed, so the second Enter submits — the input clears.
    h.press(KeyCode::Enter);
    assert!(
        h.app.editor.content().is_empty(),
        "the second Enter sent the message"
    );
}

/// A command the user has already typed in full still submits on the FIRST
/// Enter — there is nothing left to accept. This holds even when the popup
/// surfaces the command's canonical alias (`/clear` → suggestion `/new`): the
/// typed command dispatches as-is rather than being replaced by the suggestion.
#[tokio::test]
async fn enter_submits_a_fully_typed_command_in_one_press_despite_an_alias_suggestion() {
    let mut h = Harness::new(vec![MockReply::Text("answer".to_string())]).await;
    h.submit("remember this").await;
    assert!(h.render().contains("answer"));

    // `/clear` is complete (its popup row is the canonical `/new`); one Enter runs it.
    h.type_str("/clear");
    h.press(KeyCode::Enter);
    h.pump().await;
    let screen = h.render();
    assert!(
        screen.contains("conversation cleared"),
        "a fully-typed command submits in one Enter:\n{screen}"
    );
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

/// Up/Down browse HISTORY, even once a recalled entry is itself a slash command.
///
/// The regression (`6ff0172`): recalling `/help` opened the completion popup on
/// the recalled text, and the popup then swallowed the next Up — so history
/// browsing was stuck on that entry and everything older than it was
/// unreachable. `suppress_completions` keeps the popup dormant for the duration
/// of the browse, and typing clears it, so completions still work.
#[tokio::test]
async fn up_after_recalling_a_slash_command_keeps_walking_history() {
    let mut h = Harness::new(vec![MockReply::Text("answer".to_string())]).await;
    h.submit("the older message").await;
    // A bare slash command: recalled into the box, its own text matches.
    h.submit("/help").await;
    // `/help` is a data command now — its popup captures keys until Esc.
    h.press(KeyCode::Esc);

    // The first Up recalls the newest entry — the command.
    h.press(KeyCode::Up);
    assert_eq!(h.app.editor.content(), "/help", "Up recalls the command");
    assert!(
        h.app.active_completions().is_none(),
        "no popup over a recalled entry — it would take the next Up"
    );

    // The second Up keeps walking history rather than moving a popup selection.
    h.press(KeyCode::Up);
    assert_eq!(
        h.app.editor.content(),
        "the older message",
        "Up walked past the slash command to the earlier entry"
    );

    // Typing again lifts the suppression: the same text completes normally.
    h.app.editor.set_content("");
    h.type_str("/help");
    let comp = h
        .app
        .active_completions()
        .expect("a freshly typed `/` opens completions again");
    assert!(
        comp.items.iter().any(|(name, _)| name == "/help"),
        "the popup offers the command: {:?}",
        comp.items
    );
}

/// `@file` completion sees files that appear *after* its index was built. A
/// recursive watcher on the cwd invalidates the cache on create/rename/remove,
/// so a file added by a `git pull`, another shell, or the agent's own write
/// tool shows up on the next `@` keystroke. (Regression: the index was a
/// one-shot snapshot per cwd — new files were invisible until the cwd changed
/// or hrdr restarted.)
#[tokio::test]
async fn at_mention_completion_picks_up_files_created_after_the_index() {
    let mut h = Harness::new(vec![]).await;
    let cwd = std::path::PathBuf::from(h.app.current_cwd());
    std::fs::write(cwd.join("alpha.txt"), "x").unwrap();

    // The first `@` builds the index off-thread. The keystroke itself sees the
    // empty content (the char is inserted *after* the completion branch), so
    // the build is triggered by the next draw — do here what the frame's draw
    // does — then the popup appears once the FileIndex result lands.
    h.type_str("@");
    let _ = h.app.active_completions();
    h.wait_for("file index", |m| matches!(m, TurnMsg::FileIndex(..)))
        .await;
    let items = h
        .app
        .active_completions()
        .map(|c| c.items)
        .unwrap_or_default();
    assert!(
        items.iter().any(|(name, _)| name == "alpha.txt"),
        "indexed file offered: {items:?}"
    );

    // A new file appears in the watched tree — a `git pull` or another shell.
    std::fs::write(cwd.join("beta.rs"), "fn main() {}").unwrap();

    // The next keystroke starts the rebuild once the watcher's dirty ping has
    // been applied (it may already have been drained — the alpha.txt write
    // pinged too). Wait until the rebuilt index actually contains the file
    // rather than betting on which ping is still in the channel.
    h.type_str("b");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        while let Ok(msg) = h.rx.try_recv() {
            h.app.on_turn_msg(msg);
        }
        // A keystroke or draw would call this; do it here so the rebuild starts
        // once the dirty ping has been applied.
        let _ = h.app.active_completions();
        if h.app.file_index.iter().any(|p| p == "beta.rs") {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "rebuilt index never included beta.rs: {:?}",
                h.app.file_index
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let items = h
        .app
        .active_completions()
        .map(|c| c.items)
        .unwrap_or_default();
    assert!(
        items.iter().any(|(name, _)| name == "beta.rs"),
        "the newly created file is offered: {items:?}"
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
        id: 0,
        status: "in_progress".to_string(),
        evidence: None,
    }];

    // Register a sub-agent with its own TODO.
    let sub_key = 1u64;
    let sub_todos = std::sync::Arc::new(std::sync::Mutex::new(vec![hrdr_agent::Todo {
        content: "sub task".to_string(),
        id: 0,
        status: "pending".to_string(),
        evidence: None,
    }]));
    let sub_agent = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: sub_todos.clone(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub_agent)),
        steering: hrdr_agent::steering_queue(),
        running: false,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
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
    h.app.focus_pane(hrdr_app::PaneId(sub_key));
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        !screen.contains("main task"),
        "main's todos hidden on sub:\n{screen}"
    );
    assert!(screen.contains("sub task"), "sub-agent's todos:\n{screen}");
}

/// The TODO list stays up while sub-agents run. It used to be suppressed to
/// save the rows the layout charged for it; the panels ride in the scrollback
/// now, so there are no rows to save — and hiding the plan exactly while the
/// work is being done is the wrong half of the trade.
#[tokio::test]
async fn the_todo_panel_stays_up_while_a_sub_agent_runs() {
    let mut h = Harness::new(vec![]).await;

    // Give the main agent a TODO.
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "main task".to_string(),
        id: 0,
        status: "in_progress".to_string(),
        evidence: None,
    }];

    // Register a running sub-agent with its own TODO.
    let sub_key = 1u64;
    let sub_todos = std::sync::Arc::new(std::sync::Mutex::new(vec![hrdr_agent::Todo {
        content: "sub task".to_string(),
        id: 0,
        status: "in_progress".to_string(),
        evidence: None,
    }]));
    let sub_agent = hrdr_agent::Agent::new(hrdr_agent::AgentConfig::default()).unwrap();
    h.app.registry.register(hrdr_agent::AgentEntry {
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
        sandbox: hrdr_tools::SandboxMode::None,
        todos: sub_todos.clone(),
        usage: hrdr_agent::AgentUsage::default(),
        events: hrdr_agent::event_log(),
        reasoning_open: false,
        pending_notices: Vec::new(),
        turn: hrdr_agent::TurnStats::default(),
        agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub_agent)),
        steering: hrdr_agent::steering_queue(),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        transcript: None,
    });
    h.app.sync_panes();

    // Both lists are up: the plan on the active agent, and the agents working it.
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(
        screen.contains("main task"),
        "the active agent's todos stay while a sub-agent runs:\n{screen}"
    );
    assert!(
        screen.contains("explore"),
        "and so does the list:\n{screen}"
    );

    // And they are still there once the sub-agent is done.
    h.app.registry.update(sub_key, |s| {
        s.running = false;
        s.done = true;
    });
    h.app.sync_panes();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let screen = buffer_to_string(term.backend().buffer());
    assert!(screen.contains("main task"), "still listed:\n{screen}");
}

/// The todo panel sits off the block above it with exactly one blank row
/// between — the block's own bottom pad — and no extra separator of its own,
/// so the list spaces exactly like a transcript entry. Regression: the panel
/// used to emit a separator of its own, leaving two stacked blanks above the
/// todo list.
#[tokio::test]
async fn the_todo_panel_sits_one_blank_row_off_the_block_above() {
    const WIDTH: u16 = 60;
    let mut h = Harness::new(vec![]).await;
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "SHIP IT NOW".to_string(),
        id: 7,
        status: "in_progress".to_string(),
        evidence: None,
    }];
    h.app.push_entry(Entry::assistant("hello there"));

    let mut term = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    // The content band only — the scrollbar column is not block content.
    let rect = h.app.transcript_rect;
    let row_text = |y: u16| -> String {
        (rect.x..rect.x + rect.w)
            .filter_map(|x| {
                buf.cell(ratatui::layout::Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let content_y = (0..24)
        .find(|&y| row_text(y).contains("hello there"))
        .expect("the block content renders");
    let todo_y = (0..24)
        .find(|&y| row_text(y).contains("SHIP IT NOW"))
        .expect("the todo renders");
    // Between the block's content and the todo's content there is exactly ONE
    // entirely-blank row — the block's own bottom pad — then the todo's ┃ pad
    // row. The panel no longer emits a separator of its own (two stacked
    // blanks above the todo list was the reported extra line).
    let blanks: Vec<u16> = (content_y + 1..todo_y)
        .filter(|&y| row_text(y).trim().is_empty())
        .collect();
    assert_eq!(
        blanks,
        vec![content_y + 1, content_y + 2],
        "exactly two blank rows in the band — the block's bottom pad and the \
         todo's own top pad, no separator of its own: rows {content_y}..{todo_y}"
    );
    let full_row = |y: u16| -> String {
        (0..WIDTH)
            .filter_map(|x| {
                buf.cell(ratatui::layout::Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    assert!(
        full_row(todo_y - 1).starts_with(crate::ui::BORDER_BAR),
        "the row above the todo content is its ┃ pad: {:?}",
        full_row(todo_y - 1)
    );
}

/// A `/compact` typed while the agent is mid-turn is queued, not refused and
/// not steered: it runs after the turn ends. The queued request keeps its
/// summary-steering message, and the queue drains only once the agent is idle.
#[tokio::test]
async fn a_compact_queued_while_busy_runs_after_the_turn_ends() {
    // Reply 1: the turn. Reply 2: the compaction's summary call (a tiny
    // history means the pass reports "nothing to compact yet", still proving
    // it RAN — a model call is only made when there is a head to summarize).
    let mut h = Harness::new(vec![
        MockReply::Text("first turn done".to_string()),
        MockReply::Text("summary".to_string()),
    ])
    .await;

    // Start the turn without pumping: the agent is busy.
    h.type_str("hello");
    h.press(KeyCode::Enter);
    assert!(h.app.running(), "the turn is in flight");

    // The model is busy, so `/compact {msg}` queues instead of running.
    h.type_str("/compact keep the file paths");
    h.press(KeyCode::Enter);
    assert!(
        h.app.pending_compaction.is_some(),
        "the request is queued, not run"
    );
    let screen = h.render();
    assert!(
        screen.contains("compact queued"),
        "the queue notice is in the transcript:\n{screen}"
    );

    // The turn ends; the queued compaction runs and reports.
    h.pump().await;
    assert!(!h.app.running(), "the turn is over");
    assert!(
        h.app.pending_compaction.is_none(),
        "the queue drained once the agent was idle"
    );
    let screen = h.render();
    assert!(
        screen.contains("nothing to compact yet"),
        "the compaction ran and reported:\n{screen}"
    );
}

/// A tinted surface above the todo panel (a user prompt) gets the same
/// separator `flush` puts between two tinted transcript blocks: the block's
/// `┃` pad and the panel's must not stack with no blank between. The untinted
/// case already supplies its blank (the block's own bottom pad) — the panel's
/// bg section is preceded by exactly one blank line either way.
#[tokio::test]
async fn a_tinted_block_above_the_todo_panel_gets_the_separator() {
    const WIDTH: u16 = 60;
    let mut h = Harness::new(vec![]).await;
    *h.app.todos.lock().unwrap() = vec![hrdr_agent::Todo {
        content: "SHIP IT NOW".to_string(),
        id: 7,
        status: "in_progress".to_string(),
        evidence: None,
    }];
    h.app.push_entry(Entry::user("a user prompt"));

    let mut term = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
    term.draw(|f| ui::draw(f, &mut h.app)).unwrap();
    let buf = term.backend().buffer();
    // Full rows, minus the scrollbar column (the transcript's scrollbar paints
    // there and is not part of any block).
    let row = |y: u16| -> String {
        (0..WIDTH - 1)
            .filter_map(|x| {
                buf.cell(Position::new(x, y))
                    .map(|c| c.symbol().to_string())
            })
            .collect()
    };
    let content_y = (0..24)
        .find(|&y| row(y).contains("a user prompt"))
        .expect("the block content renders");
    let todo_y = (0..24)
        .find(|&y| row(y).contains("SHIP IT NOW"))
        .expect("the todo renders");
    // content → the block's ┃ bottom pad → the separator (an entirely blank
    // row) → the panel's ┃ top pad → todo content. Without the separator the
    // two pads would stack with no blank between (the reported "missing line").
    assert_eq!(todo_y, content_y + 4, "rows {content_y}..{todo_y}");
    assert!(
        row(content_y + 1).starts_with(crate::ui::BORDER_BAR),
        "row above the separator is the block's ┃ pad: {:?}",
        row(content_y + 1)
    );
    assert!(
        row(content_y + 2).trim().is_empty(),
        "the separator is a fully blank row: {:?}",
        row(content_y + 2)
    );
    assert!(
        row(content_y + 3).starts_with(crate::ui::BORDER_BAR),
        "row below the separator is the panel's ┃ pad: {:?}",
        row(content_y + 3)
    );
}
