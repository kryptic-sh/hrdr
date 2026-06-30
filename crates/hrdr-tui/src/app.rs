//! App state, the async event loop, and agent orchestration.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Todo};
use hrdr_editor::{EditorEngine, PlainEngine, VimEngine};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;

use crate::Tui;
use crate::ui;

/// What a key press asks the run loop to do (for actions needing the terminal).
enum Action {
    None,
    OpenEditor,
}

/// One rendered item in the transcript.
pub(crate) enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
    },
    System(String),
    /// Final per-turn stats line, appended below the last output.
    Stats(String),
}

/// Messages from the background agent task back to the UI loop.
enum TurnMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) editor: Box<dyn EditorEngine>,
    pub(crate) transcript: Vec<Entry>,
    pub(crate) running: bool,
    pub(crate) status: String,
    pub(crate) model: String,
    /// Handle to the in-flight turn task; `abort()` cancels it.
    turn_handle: Option<JoinHandle<()>>,
    /// Transcript scroll offset in raw lines from the natural bottom.
    /// 0 = auto-follow (pin to newest content).
    pub(crate) scroll_offset: usize,
    /// Height of the transcript area as measured during the last draw; used
    /// by key handlers to compute half-page scroll amounts.
    pub(crate) transcript_height: u16,
    /// Shared TODO list updated live by the `todo_write` tool.
    pub(crate) todos: Arc<Mutex<Vec<Todo>>>,
    /// Messages submitted while a turn is running, processed FIFO once it ends.
    pub(crate) queue: VecDeque<String>,
    /// Screen rect of the "follow output" button, set during draw while scrolled
    /// up so mouse clicks can hit-test against it. `None` when following.
    pub(crate) follow_button: Option<Rect>,
    /// Set after one idle Ctrl+C; a second consecutive Ctrl+C quits. Any other
    /// key (or a mouse action) disarms it.
    pub(crate) quit_armed: bool,
    // ---- live inference stats (for the loader above the input) ----
    /// When the current turn started (for elapsed time + spinner).
    pub(crate) turn_started: Option<Instant>,
    /// When the first output token of the turn arrived (for tok/s).
    pub(crate) first_token_at: Option<Instant>,
    /// Streamed output deltas this turn (≈ tokens).
    pub(crate) out_tokens: usize,
    /// `(prompt_tokens, completion_tokens)` from the latest model call.
    pub(crate) last_usage: Option<(u32, u32)>,
    tx: mpsc::UnboundedSender<TurnMsg>,
    rx: Option<mpsc::UnboundedReceiver<TurnMsg>>,
    should_quit: bool,
}

impl App {
    pub(crate) fn new(config: AgentConfig) -> Result<Self> {
        let model = config.model.clone();
        let vim_mode = config.vim_mode;
        let agent = Agent::new(config)?;
        let todos = agent.todos();
        let (tx, rx) = mpsc::unbounded_channel();
        let editor: Box<dyn EditorEngine> = if vim_mode {
            Box::new(VimEngine::new())
        } else {
            Box::new(PlainEngine::new())
        };
        let welcome = if vim_mode {
            "hrdr ready (vim mode). Insert to type, Esc for Normal, Enter in Normal sends, \
             Ctrl+G opens $EDITOR. Send /exit (or Ctrl+C twice) to quit."
        } else {
            "hrdr ready. Type a message; Enter sends, Alt+Enter or \\+Enter for a newline \
             (Shift+Enter too on supporting terminals), Ctrl+G opens $EDITOR. Send /exit (or \
             Ctrl+C twice) to quit. Submit while a reply is running to queue follow-ups."
        };
        Ok(Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor,
            transcript: vec![Entry::System(welcome.to_string())],
            running: false,
            status: "ready".to_string(),
            model,
            turn_handle: None,
            scroll_offset: 0,
            transcript_height: 24,
            todos,
            queue: VecDeque::new(),
            follow_button: None,
            quit_armed: false,
            turn_started: None,
            first_token_at: None,
            out_tokens: 0,
            last_usage: None,
            tx,
            rx: Some(rx),
            should_quit: false,
        })
    }

    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = EventStream::new();
        let mut rx = self.rx.take().expect("run called once");
        // Periodic wake so the inference spinner animates between tokens.
        let mut ticker = tokio::time::interval(Duration::from_millis(120));

        loop {
            terminal.draw(|f| ui::draw(f, self))?;
            if self.should_quit {
                break;
            }

            tokio::select! {
                maybe_ev = events.next() => match maybe_ev {
                    Some(Ok(Event::Key(key))) => {
                        if let Action::OpenEditor = self.on_key(key) {
                            self.open_in_editor(terminal)?;
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => self.on_mouse(m),
                    Some(Ok(Event::Paste(text))) => {
                        self.quit_armed = false;
                        self.editor.paste(&text);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
                Some(msg) = rx.recv() => self.on_turn_msg(msg),
                _ = ticker.tick() => {}
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }

        // Any key other than a Ctrl+C disarms the quit confirmation.
        let is_ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if !is_ctrl_c {
            self.quit_armed = false;
        }

        // Ctrl+C / Ctrl+Q / Ctrl+G, plus vim-mode scroll.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Ctrl+C interrupts a running turn (doesn't arm quit).
                KeyCode::Char('c') if self.running => {
                    self.cancel_turn();
                    self.quit_armed = false;
                    return Action::None;
                }
                // First idle Ctrl+C arms; a second consecutive one quits.
                KeyCode::Char('c') => {
                    if self.quit_armed {
                        self.should_quit = true;
                    } else {
                        self.quit_armed = true;
                    }
                    return Action::None;
                }
                // Ctrl+Q is an immediate, deliberate quit.
                KeyCode::Char('q') => {
                    self.should_quit = true;
                    return Action::None;
                }
                // Ctrl+G: hand the buffer off to $EDITOR (only when idle).
                KeyCode::Char('g') if !self.running => return Action::OpenEditor,
                // Transcript scroll — Ctrl+U/Ctrl+D in vim Normal mode only
                // (plain mode uses these for line editing; PageUp/Down scroll).
                KeyCode::Char('u') if self.editor.mode_label() == "NORMAL" => {
                    let half = (self.transcript_height / 2).max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_add(half);
                    return Action::None;
                }
                KeyCode::Char('d') if self.editor.mode_label() == "NORMAL" => {
                    let half = (self.transcript_height / 2).max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_sub(half);
                    return Action::None;
                }
                _ => {}
            }
        }

        // Esc while running cancels the in-flight turn (vim: only in Normal, so
        // Esc still exits Insert; plain: always, since Esc is otherwise unused).
        if self.running
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.editor.mode_label() != "INSERT"
        {
            self.cancel_turn();
            return Action::None;
        }

        // Transcript scroll: PageUp/PageDown (any mode); End follows the output
        // when scrolled up (otherwise End falls through to the editor's line-end).
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::PageUp => {
                    let page = self.transcript_height.max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_add(page);
                    return Action::None;
                }
                KeyCode::PageDown => {
                    let page = self.transcript_height.max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_sub(page);
                    return Action::None;
                }
                KeyCode::End if self.scroll_offset > 0 => {
                    self.scroll_offset = 0; // resume following the newest output
                    return Action::None;
                }
                _ => {}
            }
        }

        // The engine decides whether this key submits (vim: Enter in Normal;
        // plain: Enter without a newline modifier / trailing backslash).
        if self.editor.wants_submit(&key) {
            let input = self.editor.content();
            if input.trim().is_empty() {
                return Action::None;
            }
            // Common quit commands exit the session instead of being sent.
            if is_quit_command(input.trim()) {
                self.should_quit = true;
                return Action::None;
            }
            self.editor.set_content("");
            self.scroll_offset = 0; // auto-follow on new submission
            if self.running {
                // A turn is in flight — queue it. It renders as pending at the
                // bottom (following the output) and is committed into history
                // only when it's actually sent (see `spawn_turn`).
                self.queue.push_back(input);
            } else {
                self.spawn_turn(input);
            }
            return Action::None;
        }

        self.editor.feed_key(key);
        Action::None
    }

    /// Mouse: wheel scrolls the transcript; a left click on the follow button
    /// resumes following the newest output.
    fn on_mouse(&mut self, m: MouseEvent) {
        self.quit_armed = false;
        match m.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(rect) = self.follow_button
                    && rect.contains((m.column, m.row).into())
                {
                    self.scroll_offset = 0;
                }
            }
            _ => {}
        }
    }

    /// Hand the input buffer to `$EDITOR`/`$VISUAL`, then read it back.
    fn open_in_editor(&mut self, terminal: &mut Tui) -> Result<()> {
        let path = std::env::temp_dir().join(format!("hrdr-input-{}.md", std::process::id()));
        std::fs::write(&path, self.editor.content())?;

        crate::suspend_terminal(terminal)?;
        let status = run_editor(&path);
        crate::resume_terminal(terminal)?;
        terminal.clear()?;

        if status.is_ok()
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            // Editors append a trailing newline; drop one so it doesn't submit blank.
            let text = text.strip_suffix('\n').unwrap_or(&text);
            self.editor.set_content(text);
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// Abort the in-flight agent task and discard any queued messages.
    fn cancel_turn(&mut self) {
        if let Some(handle) = self.turn_handle.take() {
            handle.abort();
        }
        self.running = false;
        let dropped = self.queue.len();
        self.queue.clear();
        self.status = "cancelled".to_string();
        let msg = if dropped > 0 {
            format!("[cancelled · {dropped} queued message(s) discarded]")
        } else {
            "[cancelled]".to_string()
        };
        self.transcript.push(Entry::System(msg));
    }

    fn spawn_turn(&mut self, input: String) {
        // Commit the message into history at send time (a queued message lives
        // as a pending bottom item until this point).
        self.transcript.push(Entry::User(input.clone()));
        self.running = true;
        self.status = "thinking…".to_string();
        self.turn_started = Some(Instant::now());
        self.first_token_at = None;
        self.out_tokens = 0;
        self.last_usage = None;
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let tx_events = tx.clone();
        let handle = tokio::spawn(async move {
            let mut a = agent.lock().await;
            let result = a
                .run(input, |ev| {
                    let _ = tx_events.send(TurnMsg::Event(ev));
                })
                .await;
            let _ = tx.send(TurnMsg::Done(result.err().map(|e| e.to_string())));
        });
        self.turn_handle = Some(handle);
    }

    fn on_turn_msg(&mut self, msg: TurnMsg) {
        match msg {
            TurnMsg::Event(ev) => {
                // Ignore buffered events after cancellation.
                if self.running {
                    self.apply_event(ev);
                }
            }
            TurnMsg::Done(err) => {
                if !self.running {
                    // Stale Done from an aborted task; discard.
                    return;
                }
                self.turn_handle = None;
                self.running = false;
                match err {
                    Some(e) => {
                        self.status = format!("error: {e}");
                        self.transcript.push(Entry::System(format!("[error] {e}")));
                    }
                    None => self.status = "ready".to_string(),
                }
                // Append the final stats for the turn (before stats are reset by
                // any queued turn that spawns next).
                if let Some(stats) = self.turn_stats() {
                    self.transcript.push(Entry::Stats(stats));
                }
                // Start the next queued message, if any (FIFO).
                if let Some(next) = self.queue.pop_front() {
                    self.spawn_turn(next);
                }
            }
        }
    }

    /// Format the final stats line for the just-finished turn, if it produced
    /// any output.
    fn turn_stats(&self) -> Option<String> {
        let started = self.turn_started?;
        if self.out_tokens == 0 && self.last_usage.is_none() {
            return None;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let speed = match self.first_token_at {
            Some(t0) if self.out_tokens > 0 => {
                let secs = t0.elapsed().as_secs_f64();
                if secs > 0.0 {
                    self.out_tokens as f64 / secs
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let mut s = format!(
            "✓ {} tok · {speed:.1} tok/s · {elapsed:.1}s",
            self.out_tokens
        );
        if let Some((prompt, completion)) = self.last_usage {
            let ratio = if completion > 0 {
                prompt as f64 / completion as f64
            } else {
                0.0
            };
            s.push_str(&format!(
                " · ctx {prompt} (in/out {prompt}/{completion}, {ratio:.1}:1)"
            ));
        }
        Some(s)
    }

    /// Count a streamed delta toward the live tok/s stats.
    fn count_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.out_tokens += 1;
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Text(t) => {
                self.count_token();
                match self.transcript.last_mut() {
                    Some(Entry::Assistant(s)) => s.push_str(&t),
                    _ => self.transcript.push(Entry::Assistant(t)),
                }
            }
            AgentEvent::Reasoning(t) => {
                self.count_token();
                match self.transcript.last_mut() {
                    Some(Entry::Reasoning(s)) => s.push_str(&t),
                    _ => self.transcript.push(Entry::Reasoning(t)),
                }
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.last_usage = Some((prompt_tokens, completion_tokens));
            }
            AgentEvent::ToolStart { id, name, args } => {
                self.status = format!("running {name}…");
                self.transcript.push(Entry::Tool {
                    id,
                    name,
                    args,
                    result: String::new(),
                    ok: true,
                    done: false,
                });
            }
            AgentEvent::ToolEnd {
                id,
                result,
                ok,
                name: _,
            } => {
                for entry in self.transcript.iter_mut().rev() {
                    if let Entry::Tool {
                        id: tid,
                        result: r,
                        ok: o,
                        done,
                        ..
                    } = entry
                        && *tid == id
                        && !*done
                    {
                        *r = result;
                        *o = ok;
                        *done = true;
                        break;
                    }
                }
            }
            AgentEvent::TurnDone => {
                self.status = "ready".to_string();
            }
        }
    }
}

/// Whether a submitted line is a common "quit the session" command, matched
/// across popular CLIs/REPLs/editors so users feel at home: bare `exit`/`quit`,
/// the `/exit` `/quit` `/bye` slash family, and vim's `:q` family.
fn is_quit_command(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "exit"
            | "quit"
            | "q"
            | "bye"
            | "exit()"
            | "quit()"
            | "/exit"
            | "/quit"
            | "/q"
            | "/bye"
            | "/stop"
            | ":q"
            | ":q!"
            | ":qa"
            | ":qa!"
            | ":wq"
            | ":x"
            | ":exit"
            | ":quit"
    )
}

/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry args (e.g. `code -w`), split on whitespace.
fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
}

#[cfg(test)]
mod tests {
    use super::is_quit_command;

    #[test]
    fn recognizes_common_quit_commands() {
        for cmd in [
            "exit",
            "quit",
            "q",
            "bye",
            "/exit",
            "/quit",
            "/bye",
            ":q",
            ":qa",
            ":wq",
            ":x",
            "EXIT",
            "  /quit  ",
        ] {
            assert!(is_quit_command(cmd), "{cmd:?} should quit");
        }
    }

    #[test]
    fn leaves_normal_messages_alone() {
        for msg in [
            "exit the loop early",
            "how do I quit vim?",
            "q1 results",
            "fix bye-bug",
        ] {
            assert!(!is_quit_command(msg), "{msg:?} should NOT quit");
        }
    }
}
