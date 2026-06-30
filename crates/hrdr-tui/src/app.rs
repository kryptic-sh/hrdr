//! App state, the async event loop, and agent orchestration.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use hjkl_clipboard::{Clipboard, MimeType, Selection};
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Message, MessageRole, Session, Todo};
use hrdr_editor::{EditorEngine, PlainEngine, VimEngine};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;

use crate::Tui;
use crate::theme::Theme;
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
    /// A unified diff (e.g. `/diff`), rendered with diff coloring.
    Diff(String),
}

/// Messages from the background agent task back to the UI loop.
enum TurnMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
    /// Out-of-band system line (e.g. async `/models` result).
    System(String),
    /// Out-of-band diff block (e.g. async `/diff` result).
    Diff(String),
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) editor: Box<dyn EditorEngine>,
    /// Resolved chat-UI colors (from an hjkl theme).
    pub(crate) theme: Theme,
    pub(crate) transcript: Vec<Entry>,
    pub(crate) running: bool,
    pub(crate) status: String,
    pub(crate) model: String,
    // ---- status bar info ----
    /// Working directory, home-shortened for display.
    pub(crate) dir: String,
    /// Current git branch, if the cwd is in a repo.
    pub(crate) branch: Option<String>,
    /// Model context window in tokens (for "X of Y"), if known.
    pub(crate) context_window: Option<u32>,
    /// Reasoning-effort label to display.
    pub(crate) effort: Option<String>,
    /// Cumulative input/output tokens across the session.
    pub(crate) session_in: usize,
    pub(crate) session_out: usize,
    /// Config kept for mid-session provider resolution (`/provider`).
    cfg: AgentConfig,
    /// OS clipboard for `/copy` (None if unavailable).
    clipboard: Option<Clipboard>,
    /// Selected row in the slash-command completion popup.
    pub(crate) completion_idx: usize,
    /// Whether to render the model's reasoning (`<think>`) blocks (`/reasoning`).
    pub(crate) show_reasoning: bool,
    /// Current endpoint base URL (for `/info`; updated by `/provider`).
    base_url: String,
    /// Active session's file id (stem). Assigned on first auto-save; stable.
    session_id: Option<String>,
    /// Display name override (`/rename`); falls back to the first user message.
    session_label: Option<String>,
    /// Handle to the in-flight turn task; `abort()` cancels it.
    turn_handle: Option<JoinHandle<()>>,
    /// Transcript scroll offset in raw lines from the natural bottom.
    /// 0 = auto-follow (pin to newest content).
    pub(crate) scroll_offset: usize,
    /// Height of the transcript area as measured during the last draw; used
    /// by key handlers to compute half-page scroll amounts.
    pub(crate) transcript_height: u16,
    /// Max scroll offset (rows from bottom to the very top) from the last draw;
    /// lets `Home` jump to the top and bound scrolling.
    pub(crate) max_scroll: usize,
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
        let theme = Theme::load(config.theme.as_deref());
        let dir = display_dir(&config.cwd);
        let branch = git_branch(&config.cwd);
        let context_window = config.context_window;
        let effort = config.effort.clone();
        let base_url = config.base_url.clone();
        let cfg = config.clone();
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
             Ctrl+G opens $EDITOR. /help for commands; /exit (or Ctrl+C twice) to quit."
        } else {
            "hrdr ready. Type a message; Enter sends, Alt+Enter or \\+Enter for a newline \
             (Shift+Enter too on supporting terminals), Ctrl+G opens $EDITOR. /help for commands; \
             /exit (or Ctrl+C twice) to quit. Submit while a reply runs to queue follow-ups."
        };
        Ok(Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor,
            theme,
            transcript: vec![Entry::System(welcome.to_string())],
            running: false,
            status: "ready".to_string(),
            model,
            dir,
            branch,
            context_window,
            effort,
            session_in: 0,
            session_out: 0,
            cfg,
            clipboard: Clipboard::new().ok(),
            completion_idx: 0,
            show_reasoning: true,
            base_url,
            session_id: None,
            session_label: None,
            turn_handle: None,
            scroll_offset: 0,
            transcript_height: 24,
            max_scroll: 0,
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

        // Slash-command completion popup: Tab accepts the selection, Up/Down move
        // it, Enter accepts the selection and submits it.
        let comp = slash_completions(&self.editor.content());
        if !comp.is_empty() && key.modifiers.is_empty() {
            let last = comp.len() - 1;
            match key.code {
                KeyCode::Tab => {
                    let idx = self.completion_idx.min(last);
                    self.editor.set_content(&format!("{} ", comp[idx].0));
                    self.completion_idx = 0;
                    return Action::None;
                }
                KeyCode::Up => {
                    self.completion_idx = self.completion_idx.min(last).saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.completion_idx = (self.completion_idx.min(last) + 1).min(last);
                    return Action::None;
                }
                // Replace the partial input with the selected command, then fall
                // through to the normal submit path below so it runs.
                KeyCode::Enter => {
                    let idx = self.completion_idx.min(last);
                    self.editor.set_content(comp[idx].0);
                    self.completion_idx = 0;
                }
                _ => {}
            }
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
                KeyCode::Home if self.scroll_offset < self.max_scroll => {
                    self.scroll_offset = self.max_scroll; // jump to the top of the session
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
            // Slash commands are handled locally, not sent to the model.
            if self.handle_slash(input.trim()) {
                self.editor.set_content("");
                self.scroll_offset = 0;
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

    fn system(&mut self, msg: impl Into<String>) {
        self.transcript.push(Entry::System(msg.into()));
    }

    /// Dispatch a known slash command. Returns `true` if it was a recognized
    /// command (and thus shouldn't be sent to the model); unknown `/…` input
    /// returns `false` so it goes to the model (e.g. a literal path).
    fn handle_slash(&mut self, input: &str) -> bool {
        let Some(rest) = input.strip_prefix('/') else {
            return false;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match cmd {
            "help" => {
                let mut s = String::from("commands:");
                for (n, d) in SLASH_COMMANDS {
                    s.push_str(&format!("\n  {n} — {d}"));
                }
                self.system(s);
            }
            "clear" => {
                if let Ok(mut a) = self.agent.try_lock() {
                    a.clear();
                }
                self.transcript.clear();
                self.queue.clear();
                self.scroll_offset = 0;
                self.session_in = 0;
                self.session_out = 0;
                self.last_usage = None;
                self.session_id = None; // detach; next message starts a new session
                self.session_label = None;
                self.system("conversation cleared");
            }
            "model" => {
                if arg.is_empty() {
                    self.system(format!("model: {}", self.model));
                } else {
                    let ok = match self.agent.try_lock() {
                        Ok(mut a) => {
                            a.set_model(arg);
                            true
                        }
                        Err(_) => false,
                    };
                    if ok {
                        self.model = arg.to_string();
                        self.system(format!("model → {arg}"));
                    } else {
                        self.system("busy — try again after the current turn");
                    }
                }
            }
            "models" => self.list_models_cmd(),
            "provider" => self.switch_provider(arg),
            "theme" => {
                let path = (!arg.is_empty()).then_some(arg);
                self.theme = Theme::load(path);
                match path {
                    Some(p) => self.system(format!("theme → {p}")),
                    None => self.system("theme reset to default"),
                }
            }
            "cwd" => self.change_cwd(arg),
            "tools" => self.show_tools(),
            "add" => self.add_file(arg),
            "diff" => self.git_diff_cmd(),
            "reasoning" => {
                self.show_reasoning = !self.show_reasoning;
                self.system(if self.show_reasoning {
                    "reasoning shown"
                } else {
                    "reasoning hidden"
                });
            }
            "temp" | "temperature" => self.set_temp_cmd(arg),
            "effort" => {
                if arg.is_empty() {
                    self.system(format!(
                        "effort: {}",
                        self.effort.clone().unwrap_or_else(|| "—".into())
                    ));
                } else {
                    self.effort = Some(arg.to_string());
                    self.system(format!("effort → {arg}"));
                }
            }
            "info" => self.show_info(),
            "copy" => self.copy_last_reply(),
            "retry" => self.retry_last(),
            "undo" => self.undo_last(),
            "resume" | "load" => self.resume_session(arg),
            "rename" => self.rename_session(arg),
            "sessions" => self.list_sessions_cmd(arg),
            _ => return false,
        }
        true
    }

    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, and `/rename`.
    fn autosave(&mut self) {
        let snap = self
            .agent
            .try_lock()
            .ok()
            .map(|a| (a.messages_owned(), a.cwd()));
        let Some((msgs, cwd)) = snap else {
            return;
        };
        // Non-empty == has at least one user message.
        if !msgs.iter().any(|m| m.role == MessageRole::User) {
            return;
        }
        let name = self
            .session_label
            .clone()
            .unwrap_or_else(|| session_name_from(&msgs));
        // Notify once, when the session is first created.
        if self.session_id.is_none() {
            let id = hrdr_agent::unique_session_id(&cwd.display().to_string(), &name);
            self.transcript.push(Entry::System(format!(
                "session saved as '{id}' — /resume {id}"
            )));
            self.session_id = Some(id);
        }
        let id = self.session_id.clone().unwrap_or_else(|| name.clone());
        let s = Session::new(
            name,
            self.model.clone(),
            self.base_url.clone(),
            cwd.display().to_string(),
            msgs,
        );
        let _ = s.save(&id); // best-effort; silent
    }

    fn rename_session(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /rename <name>");
            return;
        }
        self.session_label = Some(arg.to_string());
        self.autosave(); // persist the new name (no-op while still empty)
        self.system(format!("session renamed → {arg}"));
    }

    fn resume_session(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /resume <id-or-name>  (see /sessions)");
            return;
        }
        if self.running {
            self.system("can't resume while a turn is running");
            return;
        }
        // Match by file id first, then by display name (e.g. after /rename).
        let cwd = self.current_cwd();
        let Some((id, session)) = hrdr_agent::resolve_session(&cwd, arg) else {
            self.system(format!("no session matching '{arg}' (see /sessions)"));
            return;
        };
        let count = session.messages.len();
        if let Ok(mut a) = self.agent.try_lock() {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        }
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(id.clone());
        self.session_label = Some(session.name.clone());
        self.scroll_offset = 0;
        self.system(format!("resumed '{}' ({count} messages)", session.name));
        // Switch hrdr's tools to the session's working directory (in-process
        // only — the parent shell is untouched).
        if !session.cwd.is_empty() && session.cwd != cwd {
            let target = std::path::PathBuf::from(&session.cwd);
            if target.is_dir() {
                self.apply_cwd(target.clone());
                self.system(format!("cwd → {}", target.display()));
            } else {
                self.system(format!(
                    "note: session cwd {} no longer exists; staying in {cwd}",
                    session.cwd
                ));
            }
        }
        if session.base_url != self.base_url {
            self.system(format!(
                "note: session endpoint was {} (current: {})",
                session.base_url, self.base_url
            ));
        }
    }

    /// The tools' current working directory (agent's, or the process cwd while
    /// a turn holds the agent lock).
    fn current_cwd(&self) -> String {
        if let Ok(a) = self.agent.try_lock() {
            return a.cwd().display().to_string();
        }
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    fn list_sessions_cmd(&mut self, arg: &str) {
        let all = matches!(arg.trim(), "--all" | "-a" | "all");
        let cur = hrdr_agent::cwd_slug(&self.current_cwd());
        let sessions: Vec<_> = hrdr_agent::list_sessions()
            .into_iter()
            .filter(|m| all || hrdr_agent::cwd_slug(&m.cwd) == cur)
            .collect();
        if sessions.is_empty() {
            self.system(if all {
                format!(
                    "no saved sessions in {}",
                    hrdr_agent::sessions_dir().display()
                )
            } else {
                "no saved sessions for this directory (try /sessions --all)".to_string()
            });
            return;
        }
        let mut s = if all {
            String::from("all sessions (resume by id or name):")
        } else {
            String::from("sessions here (resume by id or name; /sessions --all for every dir):")
        };
        for m in sessions {
            if all {
                s.push_str(&format!("\n  {} — {}  [{}]", m.id, m.name, m.cwd));
            } else {
                s.push_str(&format!("\n  {} — {}", m.id, m.name));
            }
        }
        self.system(s);
    }

    /// Rebuild the display transcript from a restored message history.
    fn rebuild_transcript(&mut self, msgs: &[Message]) {
        self.transcript.clear();
        // Map tool_call_id → (result, ok) from the tool-result messages.
        let mut results: HashMap<String, (String, bool)> = HashMap::new();
        for m in msgs {
            if m.role == MessageRole::Tool
                && let (Some(id), Some(content)) = (&m.tool_call_id, &m.content)
            {
                let ok = !content.starts_with("Error:");
                results.insert(id.clone(), (content.clone(), ok));
            }
        }
        for m in msgs {
            match m.role {
                MessageRole::User => {
                    if let Some(c) = &m.content {
                        self.transcript.push(Entry::User(c.clone()));
                    }
                }
                MessageRole::Assistant => {
                    if let Some(c) = &m.content
                        && !c.is_empty()
                    {
                        self.transcript.push(Entry::Assistant(c.clone()));
                    }
                    for call in m.tool_calls.iter().flatten() {
                        let (result, ok) = results.get(&call.id).cloned().unwrap_or_default();
                        self.transcript.push(Entry::Tool {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            args: call.function.arguments.clone(),
                            result,
                            ok,
                            done: true,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn list_models_cmd(&mut self) {
        let client = self.agent.try_lock().ok().map(|a| a.client());
        let Some(client) = client else {
            self.system("busy — try again after the current turn");
            return;
        };
        let tx = self.tx.clone();
        self.system("fetching models…");
        tokio::spawn(async move {
            let msg = match client.list_models().await {
                Ok(m) if !m.is_empty() => format!("models:\n  {}", m.join("\n  ")),
                Ok(_) => "endpoint reported no models".to_string(),
                Err(e) => format!("models error: {e}"),
            };
            let _ = tx.send(TurnMsg::System(msg));
        });
    }

    fn change_cwd(&mut self, arg: &str) {
        let cur = self.agent.try_lock().ok().map(|a| a.cwd());
        let Some(cur) = cur else {
            self.system("busy — try again after the current turn");
            return;
        };
        if arg.is_empty() {
            self.system(format!("cwd: {}", cur.display()));
            return;
        }
        let p = std::path::Path::new(arg);
        let new = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cur.join(p)
        };
        if !new.is_dir() {
            self.system(format!("not a directory: {}", new.display()));
            return;
        }
        let new = new.canonicalize().unwrap_or(new);
        self.apply_cwd(new.clone());
        self.system(format!("cwd → {}", new.display()));
    }

    /// Switch the tools' working directory: update the agent and the status bar.
    fn apply_cwd(&mut self, new: std::path::PathBuf) {
        if let Ok(mut a) = self.agent.try_lock() {
            a.set_cwd(new.clone());
        }
        self.dir = display_dir(&new);
        self.branch = git_branch(&new);
    }

    fn show_tools(&mut self) {
        match self.agent.try_lock().ok().map(|a| a.tools()) {
            Some(tools) => {
                let mut s = String::from("tools:");
                for (n, d) in tools {
                    s.push_str(&format!("\n  {n} — {d}"));
                }
                self.system(s);
            }
            None => self.system("busy — try again after the current turn"),
        }
    }

    fn add_file(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /add <file>");
            return;
        }
        let cur = self.agent.try_lock().ok().map(|a| a.cwd());
        let Some(cur) = cur else {
            self.system("busy — try again after the current turn");
            return;
        };
        let p = std::path::Path::new(arg);
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cur.join(p)
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let n = content.lines().count();
                let block = format!("`{arg}`:\n```\n{content}\n```\n\n");
                let existing = self.editor.content();
                self.editor.set_content(&format!("{block}{existing}"));
                self.system(format!("added {arg} ({n} lines) to the input"));
            }
            Err(e) => self.system(format!("can't read {arg}: {e}")),
        }
    }

    fn git_diff_cmd(&mut self) {
        let cwd = self.agent.try_lock().ok().map(|a| a.cwd());
        let Some(cwd) = cwd else {
            self.system("busy — try again after the current turn");
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let out = tokio::process::Command::new("git")
                .arg("diff")
                .current_dir(&cwd)
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    if s.trim().is_empty() {
                        let _ = tx.send(TurnMsg::System("git diff: no changes".to_string()));
                    } else {
                        let _ = tx.send(TurnMsg::Diff(s));
                    }
                }
                Ok(o) => {
                    let _ = tx.send(TurnMsg::System(format!(
                        "git diff failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )));
                }
                Err(e) => {
                    let _ = tx.send(TurnMsg::System(format!("git error: {e}")));
                }
            }
        });
    }

    fn set_temp_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            let t = self.agent.try_lock().ok().and_then(|a| a.temperature());
            self.system(format!(
                "temperature: {}",
                t.map(|t| t.to_string()).unwrap_or_else(|| "default".into())
            ));
            return;
        }
        match arg.parse::<f32>() {
            Ok(t) => {
                if let Ok(mut a) = self.agent.try_lock() {
                    a.set_temperature(Some(t));
                }
                self.system(format!("temperature → {t}"));
            }
            Err(_) => self.system("usage: /temp <number>"),
        }
    }

    fn show_info(&mut self) {
        let temp = self.agent.try_lock().ok().and_then(|a| a.temperature());
        let branch = self.branch.clone().unwrap_or_else(|| "—".into());
        let ctx = match (self.last_usage, self.context_window) {
            (Some((p, _)), Some(w)) => format!("{p} / {w}"),
            (Some((p, _)), None) => p.to_string(),
            _ => "—".into(),
        };
        let session = match (&self.session_id, &self.session_label) {
            (Some(id), Some(name)) => format!("{id}  (name: {name})"),
            (Some(id), None) => id.clone(),
            (None, _) => "(unsaved — send a message to start one)".to_string(),
        };
        let info = format!(
            "session: {session}\nmodel: {}\nendpoint: {}\ncwd: {} ({branch})\ncontext: {ctx}\ntokens: ↑{} ↓{}\ntemperature: {}\neffort: {}",
            self.model,
            self.base_url,
            self.dir,
            self.session_in,
            self.session_out,
            temp.map(|t| t.to_string())
                .unwrap_or_else(|| "default".into()),
            self.effort.clone().unwrap_or_else(|| "—".into()),
        );
        self.system(info);
    }

    fn undo_last(&mut self) {
        if self.running {
            self.system("can't undo while a turn is running");
            return;
        }
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user());
        match text {
            Some(t) => {
                if let Some(idx) = self
                    .transcript
                    .iter()
                    .rposition(|e| matches!(e, Entry::User(_)))
                {
                    self.transcript.truncate(idx);
                }
                self.editor.set_content(&t); // restore for editing
                self.scroll_offset = 0;
                self.autosave();
                self.system("undid last turn — edit and resend");
            }
            None => self.system("nothing to undo"),
        }
    }

    fn switch_provider(&mut self, name: &str) {
        if name.is_empty() {
            self.system("usage: /provider <name>");
            return;
        }
        let Some(p) = self.cfg.resolve_provider(name) else {
            self.system(format!("unknown provider '{name}'"));
            return;
        };
        let key = p
            .api_key
            .clone()
            .or_else(|| p.key_env.as_ref().and_then(|e| std::env::var(e).ok()));
        let switched = match self.agent.try_lock() {
            Ok(mut a) => {
                a.set_endpoint(p.base_url.clone(), key);
                if let Some(m) = &p.model {
                    a.set_model(m.clone());
                }
                true
            }
            Err(_) => false,
        };
        if !switched {
            self.system("busy — try again after the current turn");
            return;
        }
        if let Some(m) = &p.model {
            self.model = m.clone();
        }
        if let Some(w) = p.context_window {
            self.context_window = Some(w);
        }
        self.base_url = p.base_url.clone();
        self.system(format!("provider → {name} ({})", p.base_url));
        if !p.remote {
            self.system(
                "note: a running backend isn't restarted; relaunch hrdr for a local backend",
            );
        }
    }

    fn copy_last_reply(&mut self) {
        let last = self.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(s) => Some(s.clone()),
            _ => None,
        });
        match (last, self.clipboard.as_mut()) {
            (Some(text), Some(cb)) => {
                match cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()) {
                    Ok(()) => self.system("copied last reply to clipboard"),
                    Err(_) => self.system("clipboard write failed"),
                }
            }
            (Some(_), None) => self.system("clipboard unavailable"),
            (None, _) => self.system("no assistant reply to copy"),
        }
    }

    fn retry_last(&mut self) {
        if self.running {
            self.system("can't retry while a turn is running");
            return;
        }
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user());
        match text {
            Some(t) => {
                // Drop the old turn's transcript entries back to the last user message.
                if let Some(idx) = self
                    .transcript
                    .iter()
                    .rposition(|e| matches!(e, Entry::User(_)))
                {
                    self.transcript.truncate(idx);
                }
                self.scroll_offset = 0;
                self.spawn_turn(t);
            }
            None => self.system("nothing to retry"),
        }
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
        // Keep last_usage so the status-bar context size persists between turns;
        // it's refreshed when this turn's Usage event arrives.
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let tx_events = tx.clone();
        let handle = tokio::spawn(async move {
            // Release the agent lock before signalling Done, so the UI's
            // auto-save (try_lock) can run immediately afterward.
            let result = {
                let mut a = agent.lock().await;
                a.run(input, |ev| {
                    let _ = tx_events.send(TurnMsg::Event(ev));
                })
                .await
            };
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
            TurnMsg::System(text) => {
                self.transcript.push(Entry::System(text));
                self.scroll_offset = 0;
            }
            TurnMsg::Diff(text) => {
                self.transcript.push(Entry::Diff(text));
                self.scroll_offset = 0;
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
                // Persist the completed turn into the active session, if any.
                self.autosave();
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
                self.session_in += prompt_tokens as usize;
                self.session_out += completion_tokens as usize;
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

/// Display form of `cwd`, with the home directory collapsed to `~`.
fn display_dir(cwd: &std::path::Path) -> String {
    let s = cwd.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s
}

/// Current git branch (or short detached-HEAD sha) by walking up from `cwd` to
/// the repo root and reading `.git/HEAD`. Cheap, no subprocess.
fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let git = d.join(".git");
        if git.is_dir() {
            return std::fs::read_to_string(git.join("HEAD"))
                .ok()
                .and_then(|h| parse_head(&h));
        }
        if git.is_file()
            && let Ok(content) = std::fs::read_to_string(&git)
            && let Some(p) = content.strip_prefix("gitdir:")
            && let Ok(head) = std::fs::read_to_string(std::path::Path::new(p.trim()).join("HEAD"))
        {
            return parse_head(&head);
        }
        dir = d.parent();
    }
    None
}

fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None if !head.is_empty() => Some(head.chars().take(7).collect()),
        None => None,
    }
}

/// A short session name derived from the first user message.
fn session_name_from(msgs: &[Message]) -> String {
    msgs.iter()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| m.content.as_deref())
        .map(|c| {
            c.lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(60)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "untitled".to_string())
}

/// Slash commands offered by the completion popup.
pub(crate) const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "reset the conversation"),
    (
        "/sessions",
        "list this dir's saved sessions (--all for every dir)",
    ),
    ("/resume", "resume a saved session by id or name"),
    ("/rename", "rename the current session"),
    ("/model", "show or switch model"),
    ("/models", "list models from the endpoint"),
    ("/provider", "switch provider preset"),
    ("/theme", "switch theme (path, or reset)"),
    ("/cwd", "show or change working directory"),
    ("/tools", "list available tools"),
    ("/add", "attach a file to the next message"),
    ("/diff", "show git diff of the working tree"),
    ("/reasoning", "toggle showing model reasoning"),
    ("/temp", "show or set temperature"),
    ("/effort", "show or set effort label"),
    ("/info", "session info"),
    ("/copy", "copy last reply"),
    ("/retry", "re-run last turn"),
    ("/undo", "undo last turn (edit & resend)"),
    ("/help", "list commands"),
    ("/exit", "quit"),
];

/// Commands matching the in-progress `/…` input (empty once a space is typed).
///
/// Matches the query (the text after `/`) against both the command name and its
/// description (case-insensitive substring), so e.g. `/list` surfaces `/help`
/// ("list commands"). Ranked: name-prefix, then name-substring, then
/// description-substring.
pub(crate) fn slash_completions(input: &str) -> Vec<(&'static str, &'static str)> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    if query.is_empty() {
        return SLASH_COMMANDS.to_vec();
    }
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, (&'static str, &'static str))> = Vec::new();
    for &(name, desc) in SLASH_COMMANDS {
        let nl = name.trim_start_matches('/').to_ascii_lowercase();
        let rank = if nl.starts_with(&q) {
            0
        } else if nl.contains(&q) {
            1
        } else if desc.to_ascii_lowercase().contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, (name, desc)));
    }
    scored.sort_by_key(|(r, _)| *r); // stable: preserves list order within a rank
    scored.into_iter().map(|(_, c)| c).collect()
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
    use super::{is_quit_command, slash_completions};

    #[test]
    fn slash_completions_filter() {
        let names = |i: &str| {
            slash_completions(i)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
        };
        assert!(names("/").len() >= 6); // all commands for a bare slash
        // Name-prefix matches rank first (/clear, /cwd, /copy all start with c).
        assert_eq!(names("/c")[0], "/clear");
        assert!(names("/c").contains(&"/copy") && names("/c").contains(&"/cwd"));
        // Description match: "/list" surfaces "/help" ("list commands").
        assert!(names("/list").contains(&"/help"));
        assert!(!names("/list").contains(&"/clear"));
        assert!(names("/model gpt").is_empty()); // a space ends completion
        assert!(names("hello").is_empty()); // not a slash command
    }

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
