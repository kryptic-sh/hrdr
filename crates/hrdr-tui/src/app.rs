//! App state, the async event loop, and agent orchestration.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Todo};
use hrdr_editor::{EditorEngine, PlainEngine, VimEngine};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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
             Ctrl+G opens $EDITOR, Ctrl+C quits."
        } else {
            "hrdr ready. Type a message; Enter sends, Shift+Enter or \\+Enter for a newline, \
             Ctrl+G opens $EDITOR, Ctrl+C quits."
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
            tx,
            rx: Some(rx),
            should_quit: false,
        })
    }

    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = EventStream::new();
        let mut rx = self.rx.take().expect("run called once");

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
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
                Some(msg) = rx.recv() => self.on_turn_msg(msg),
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }

        // Ctrl+C / Ctrl+Q / Ctrl+G, plus vim-mode scroll.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') if self.running => {
                    self.cancel_turn();
                    return Action::None;
                }
                KeyCode::Char('c') | KeyCode::Char('q') => {
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

        // PageUp / PageDown scroll the transcript (any mode).
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
                _ => {}
            }
        }

        // The engine decides whether this key submits (vim: Enter in Normal;
        // plain: Enter without Shift / trailing backslash).
        if !self.running && self.editor.wants_submit(&key) {
            let input = self.editor.content();
            if input.trim().is_empty() {
                return Action::None;
            }
            self.transcript.push(Entry::User(input.clone()));
            self.editor.set_content("");
            self.scroll_offset = 0; // auto-follow on new submission
            self.spawn_turn(input);
            return Action::None;
        }

        self.editor.feed_key(key);
        Action::None
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

    /// Abort the in-flight agent task, update status, push a cancel marker.
    fn cancel_turn(&mut self) {
        if let Some(handle) = self.turn_handle.take() {
            handle.abort();
        }
        self.running = false;
        self.status = "cancelled".to_string();
        self.transcript
            .push(Entry::System("[cancelled]".to_string()));
    }

    fn spawn_turn(&mut self, input: String) {
        self.running = true;
        self.status = "thinking…".to_string();
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
            }
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Text(t) => match self.transcript.last_mut() {
                Some(Entry::Assistant(s)) => s.push_str(&t),
                _ => self.transcript.push(Entry::Assistant(t)),
            },
            AgentEvent::Reasoning(t) => match self.transcript.last_mut() {
                Some(Entry::Reasoning(s)) => s.push_str(&t),
                _ => self.transcript.push(Entry::Reasoning(t)),
            },
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
