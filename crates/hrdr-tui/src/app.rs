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
    /// Open a specific file in `$EDITOR` (from `/edit <file>`).
    OpenFile(std::path::PathBuf),
    /// Force a full clear + repaint (Ctrl+L), to fix terminal corruption.
    Redraw,
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
    /// Compaction finished: `Ok((before, after))` message counts, or an error.
    Compacted(Result<(usize, usize), String>),
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
    /// Icon set for the TUI chrome (status bar glyphs).
    pub(crate) icon_mode: hjkl_icons::IconMode,
    /// Cumulative input/output tokens across the session.
    pub(crate) session_in: usize,
    pub(crate) session_out: usize,
    /// Config kept for mid-session provider resolution (`/provider`).
    cfg: AgentConfig,
    /// OS clipboard for `/copy` (None if unavailable).
    clipboard: Option<Clipboard>,
    /// Selected row in the completion popup (slash command or `@file`).
    pub(crate) completion_idx: usize,
    /// Submitted inputs this session, for Up/Down recall (oldest first).
    input_history: Vec<String>,
    /// Current position while browsing `input_history` (None = editing a draft).
    history_pos: Option<usize>,
    /// The in-progress draft stashed when history browsing began.
    history_draft: String,
    /// Cached relative file paths under the cwd, for `@file` completion.
    file_index: Vec<String>,
    /// The cwd `file_index` was built for; rebuilt when the cwd changes.
    file_index_cwd: Option<std::path::PathBuf>,
    /// Whether to render the model's reasoning (`<think>`) blocks (`/reasoning`).
    pub(crate) show_reasoning: bool,
    /// True while a compaction (summarization) pass is running.
    pub(crate) compacting: bool,
    /// True while an `/init` turn runs, so its result reloads `AGENTS.md`.
    pending_init: bool,
    /// A file `/edit` requested to open in `$EDITOR`, consumed by the run loop.
    pending_edit: Option<std::path::PathBuf>,
    /// Auto-compact trigger as a fraction of the context window; 0 disables.
    pub(crate) auto_compact_ratio: f64,
    /// Ring the terminal bell when a turn finishes (after a brief minimum).
    bell: bool,
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
        let auto_compact = config.auto_compact;
        let auto_resume = config.auto_resume;
        let bell = config.bell;
        // No portable terminal-font probe, so an unset/`auto` icons setting
        // resolves to Nerd glyphs.
        let icon_mode = config
            .icons
            .as_deref()
            .and_then(hjkl_icons::IconMode::from_config)
            .unwrap_or(hjkl_icons::IconMode::Nerd);
        let effort = config.effort.clone();
        let base_url = config.base_url.clone();
        let cfg = config.clone();
        let agent = Agent::new(config)?;
        let todos = agent.todos();
        let project_docs_loaded = agent.project_docs().is_some();
        let (tx, rx) = mpsc::unbounded_channel();
        let editor: Box<dyn EditorEngine> = if vim_mode {
            Box::new(VimEngine::new())
        } else {
            Box::new(PlainEngine::new())
        };
        let welcome = if vim_mode {
            "hrdr ready (vim mode). Insert to type, Esc for Normal, Enter in Normal sends, \
             Ctrl+G opens $EDITOR. Type @path to attach a file. /help for commands; \
             /exit (or Ctrl+C twice) to quit."
        } else {
            "hrdr ready. Type a message; Enter sends, Alt+Enter or \\+Enter for a newline \
             (Shift+Enter too on supporting terminals), Ctrl+G opens $EDITOR. Type @path to \
             attach a file. /help for commands; /exit (or Ctrl+C twice) to quit. Submit while a \
             reply runs to queue follow-ups."
        };
        let mut transcript = vec![Entry::System(welcome.to_string())];
        if project_docs_loaded {
            transcript.push(Entry::System(
                "loaded project instructions from AGENTS.md".to_string(),
            ));
        }
        let mut app = Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor,
            theme,
            transcript,
            running: false,
            status: "ready".to_string(),
            model,
            dir,
            branch,
            context_window,
            effort,
            icon_mode,
            session_in: 0,
            session_out: 0,
            cfg,
            clipboard: Clipboard::new().ok(),
            completion_idx: 0,
            input_history: load_history(),
            history_pos: None,
            history_draft: String::new(),
            file_index: Vec::new(),
            file_index_cwd: None,
            show_reasoning: true,
            compacting: false,
            pending_init: false,
            pending_edit: None,
            auto_compact_ratio: auto_compact,
            bell,
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
        };
        if auto_resume {
            app.auto_resume_latest();
        }
        Ok(app)
    }

    /// Probe the endpoint (list its models) on a background task and post a
    /// warning if it's unreachable or doesn't advertise the configured model.
    /// Stays silent on success so it doesn't clutter the transcript.
    fn spawn_health_check(&self) {
        let Some(client) = self.agent.try_lock().ok().map(|a| a.client()) else {
            return;
        };
        let model = self.model.clone();
        let base_url = self.base_url.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.list_models().await {
                Err(e) => {
                    let _ = tx.send(TurnMsg::System(format!(
                        "⚠ endpoint {base_url} looks unreachable: {e}"
                    )));
                }
                Ok(models) => {
                    if model != "default"
                        && !models.is_empty()
                        && !models.iter().any(|m| m == &model)
                    {
                        let sample = models
                            .iter()
                            .take(8)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        let _ = tx.send(TurnMsg::System(format!(
                            "⚠ model '{model}' not found at {base_url}; available: {sample}"
                        )));
                    }
                }
            }
        });
    }

    /// On startup, resume the most recent saved session for the current
    /// directory (if any). No match → leave the fresh session as-is.
    fn auto_resume_latest(&mut self) {
        let cwd = self.current_cwd();
        let cur = hrdr_agent::cwd_slug(&cwd);
        let Some(meta) = hrdr_agent::list_sessions()
            .into_iter()
            .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)
        else {
            return; // nothing saved here yet — start fresh
        };
        let Ok(session) = Session::load_path(&meta.path) else {
            return;
        };
        // Skip empty sessions (system prompt only).
        if session.messages.len() <= 1 {
            return;
        }
        if let Ok(mut a) = self.agent.try_lock() {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        }
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(meta.id);
        self.session_label = Some(session.name.clone());
        self.transcript.push(Entry::System(format!(
            "resumed most recent session '{}' ({} messages) — /clear to start fresh",
            session.name,
            session.messages.len()
        )));
    }

    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        // Probe the endpoint in the background and warn if it's unreachable or
        // doesn't have the configured model — surfaced before the first turn.
        self.spawn_health_check();
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
                    Some(Ok(Event::Key(key))) => match self.on_key(key) {
                        Action::OpenEditor => self.open_in_editor(terminal)?,
                        Action::OpenFile(path) => self.open_file_in_editor(terminal, &path)?,
                        Action::Redraw => terminal.clear()?,
                        Action::None => {}
                    },
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

        // Completion popup (slash command or `@file`): Tab accepts the selection,
        // Up/Down move it, Enter accepts; a slash Enter then submits, an `@file`
        // Enter just inserts the path and keeps editing.
        if key.modifiers.is_empty()
            && let Some(comp) = self.active_completions()
        {
            let last = comp.items.len() - 1;
            match key.code {
                KeyCode::Tab => {
                    self.apply_completion(&comp, self.completion_idx.min(last), true);
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
                KeyCode::Enter => {
                    self.apply_completion(&comp, self.completion_idx.min(last), false);
                    self.completion_idx = 0;
                    // A file mention just inserts; a slash command falls through
                    // to the submit path below so it runs.
                    if matches!(comp.kind, CompletionKind::File { .. }) {
                        return Action::None;
                    }
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
                // Ctrl+L clears + repaints the screen (fix terminal corruption).
                KeyCode::Char('l') => return Action::Redraw,
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
                // Up/Down recall previous submissions (readline-style), but only
                // for single-line input so multi-line editing keeps cursor moves.
                KeyCode::Up if !self.editor.content().contains('\n') => {
                    self.history_prev();
                    return Action::None;
                }
                KeyCode::Down if !self.editor.content().contains('\n') => {
                    self.history_next();
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
            self.record_history(&input);
            // Common quit commands exit the session instead of being sent.
            if is_quit_command(input.trim()) {
                self.should_quit = true;
                return Action::None;
            }
            // Slash commands are handled locally, not sent to the model.
            if self.handle_slash(input.trim()) {
                self.editor.set_content("");
                self.scroll_offset = 0;
                if let Some(path) = self.pending_edit.take() {
                    return Action::OpenFile(path);
                }
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

    /// Open an arbitrary file in `$EDITOR` (from `/edit <file>`), suspending the
    /// TUI for the duration. The file may not exist yet — the editor creates it.
    fn open_file_in_editor(&mut self, terminal: &mut Tui, path: &std::path::Path) -> Result<()> {
        crate::suspend_terminal(terminal)?;
        let status = run_editor(path);
        crate::resume_terminal(terminal)?;
        terminal.clear()?;
        match status {
            Ok(_) => self.system(format!("edited {}", path.display())),
            Err(e) => self.system(format!("editor failed: {e}")),
        }
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
        let cmd = resolve_alias(parts.next().unwrap_or(""));
        let arg = parts.next().unwrap_or("").trim();
        match cmd {
            "help" => self.system(help_text()),
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
            "copy" => self.copy_cmd(arg),
            "paste" => self.paste_cmd(),
            "retry" => self.retry_last(arg),
            "edit" => self.edit_file_cmd(arg),
            "undo" => self.undo_last(),
            "resume" | "load" => self.resume_session(arg),
            "rename" => self.rename_session(arg),
            "sessions" => self.list_sessions_cmd(arg),
            "compact" => self.compact_cmd(arg),
            "init" => self.init_agents_cmd(),
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
        self.file_index_cwd = None; // force a rebuild for the new directory
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

    /// Re-gather `AGENTS.md` for the current cwd and refresh the system prompt
    /// in place (e.g. after `/init` writes one).
    fn reload_project_docs(&mut self) {
        let loaded = match self.agent.try_lock() {
            Ok(mut a) => {
                let cwd = a.cwd();
                a.set_cwd(cwd);
                a.project_docs().is_some()
            }
            Err(_) => return,
        };
        if loaded {
            self.system("loaded AGENTS.md into the system prompt");
        }
    }

    /// `/init` — have the model explore the project and write an `AGENTS.md`
    /// (Claude Code / opencode style): we send it an instruction prompt and it
    /// uses its tools to analyze the repo and create the file.
    fn init_agents_cmd(&mut self) {
        if self.running {
            self.system("can't /init while a turn is running");
            return;
        }
        self.transcript.push(Entry::System(
            "/init — exploring the project to write AGENTS.md…".to_string(),
        ));
        self.scroll_offset = 0;
        self.pending_init = true;
        self.launch_turn(INIT_PROMPT.to_string());
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

    /// `/copy [code|all]` — copy the last reply (default), the last code block,
    /// or the whole transcript to the clipboard.
    fn copy_cmd(&mut self, arg: &str) {
        match arg.trim().to_ascii_lowercase().as_str() {
            "" | "reply" | "last" => match self.last_assistant_text() {
                Some(t) => self.copy_to_clipboard(&t, "last reply"),
                None => self.system("no assistant reply to copy"),
            },
            "code" => match self.last_code_block() {
                Some(t) => self.copy_to_clipboard(&t, "last code block"),
                None => self.system("no code block to copy"),
            },
            "all" | "transcript" => {
                let t = self.transcript_text();
                if t.is_empty() {
                    self.system("nothing to copy");
                } else {
                    self.copy_to_clipboard(&t, "transcript");
                }
            }
            other => self.system(format!(
                "usage: /copy [code|all]  (unknown option: {other})"
            )),
        }
    }

    /// Write `text` to the system clipboard, reporting success/failure.
    fn copy_to_clipboard(&mut self, text: &str, label: &str) {
        let res = self
            .clipboard
            .as_mut()
            .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()));
        match res {
            Some(Ok(())) => self.system(format!("copied {label} to clipboard")),
            Some(Err(_)) => self.system("clipboard write failed"),
            None => self.system("clipboard unavailable"),
        }
    }

    /// The most recent assistant message text.
    fn last_assistant_text(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(s) => Some(s.clone()),
            _ => None,
        })
    }

    /// The most recent fenced code block across assistant messages.
    fn last_code_block(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(s) => last_fenced_block(s),
            _ => None,
        })
    }

    /// A plain-text rendering of the conversation for `/copy all`.
    fn transcript_text(&self) -> String {
        let mut out = String::new();
        for e in &self.transcript {
            match e {
                Entry::User(s) => out.push_str(&format!("## User\n{s}\n\n")),
                Entry::Assistant(s) => out.push_str(&format!("## Assistant\n{s}\n\n")),
                Entry::System(s) => out.push_str(&format!("[{s}]\n\n")),
                Entry::Diff(s) => out.push_str(&format!("{s}\n\n")),
                Entry::Tool { name, .. } => out.push_str(&format!("[tool: {name}]\n\n")),
                Entry::Reasoning(_) | Entry::Stats(_) => {}
            }
        }
        out.trim_end().to_string()
    }

    /// `/paste` — insert the system clipboard into the input. If the clipboard
    /// holds a path to an existing file, attach it as an `@mention` instead.
    fn paste_cmd(&mut self) {
        let data = self
            .clipboard
            .as_ref()
            .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok());
        let Some(bytes) = data else {
            self.system("clipboard unavailable or empty");
            return;
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        if text.is_empty() {
            self.system("clipboard is empty");
            return;
        }
        // A single-line path to an existing file → attach as `@path`.
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && !trimmed.contains('\n')
            && let Some(cwd) = self.agent.try_lock().ok().map(|a| a.cwd())
        {
            let p = std::path::Path::new(trimmed);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            if full.is_file() {
                self.editor.paste(&format!("@{trimmed} "));
                self.system(format!("attached @{trimmed} from clipboard"));
                return;
            }
        }
        self.editor.paste(&text);
    }

    /// `/edit <file>` — open a file (relative to the cwd) in `$EDITOR`.
    fn edit_file_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /edit <file>");
            return;
        }
        if self.running {
            self.system("can't /edit while a turn is running");
            return;
        }
        let Some(cwd) = self.agent.try_lock().ok().map(|a| a.cwd()) else {
            self.system("busy — try again after the current turn");
            return;
        };
        let p = std::path::Path::new(arg);
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        // Consumed by the run loop (it owns the terminal needed to suspend).
        self.pending_edit = Some(path);
    }

    fn retry_last(&mut self, arg: &str) {
        if self.running {
            self.system("can't retry while a turn is running");
            return;
        }
        // Optional model switch for this retry (and subsequent turns).
        if !arg.is_empty() {
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
                return;
            }
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
        self.pending_init = false;
        self.compacting = false;
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
        // Expand `@file` mentions into attached contents for the model only; the
        // transcript still shows the message as the user typed it.
        let sent = self.expand_mentions(&input);
        self.launch_turn(sent);
    }

    /// Run a turn against the model with `input` as the (already-prepared) user
    /// message. The caller is responsible for any transcript display.
    fn launch_turn(&mut self, input: String) {
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

    /// `/compact [instructions]` — summarize the conversation to reclaim context.
    fn compact_cmd(&mut self, arg: &str) {
        if self.running {
            self.system("can't compact while a turn is running");
            return;
        }
        let count = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.message_count())
            .unwrap_or(0);
        if count <= 2 {
            self.system("nothing to compact yet");
            return;
        }
        let instructions = (!arg.trim().is_empty()).then(|| arg.trim().to_string());
        self.system("compacting conversation…");
        self.spawn_compaction(instructions);
    }

    /// Ring the terminal bell when a turn finishes, if enabled and the turn ran
    /// long enough to be worth a nudge (so quick replies stay silent).
    fn maybe_bell(&self) {
        const MIN_SECS: f64 = 5.0;
        if !self.bell {
            return;
        }
        let long_enough = self
            .turn_started
            .map(|t| t.elapsed().as_secs_f64() >= MIN_SECS)
            .unwrap_or(false);
        if long_enough {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07"); // BEL
            let _ = out.flush();
        }
    }

    /// Whether the context has grown enough to auto-compact (with headroom).
    /// A configured ratio of `0` (or outside `0.0..=1.0`) disables it.
    fn should_auto_compact(&self) -> bool {
        if self.compacting {
            return false;
        }
        let ratio = self.auto_compact_ratio;
        if ratio <= 0.0 || ratio > 1.0 {
            return false;
        }
        let (Some((prompt, _)), Some(window)) = (self.last_usage, self.context_window) else {
            return false;
        };
        window > 0 && f64::from(prompt) >= f64::from(window) * ratio
    }

    /// Run a compaction pass on the background task, reporting via `TurnMsg`.
    fn spawn_compaction(&mut self, instructions: Option<String>) {
        self.running = true;
        self.compacting = true;
        self.status = "compacting…".to_string();
        self.turn_started = Some(Instant::now());
        self.first_token_at = None;
        self.out_tokens = 0;
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let res = {
                let mut a = agent.lock().await;
                a.compact(instructions.as_deref()).await
            };
            let _ = tx.send(TurnMsg::Compacted(res.map_err(|e| e.to_string())));
        });
        self.turn_handle = Some(handle);
    }

    /// Record a submitted input for Up/Down recall (skips consecutive dups,
    /// bounds the buffer, persists to disk) and resets browsing state.
    fn record_history(&mut self, input: &str) {
        if self.input_history.last().map(String::as_str) != Some(input) {
            self.input_history.push(input.to_string());
            if self.input_history.len() > MAX_HISTORY {
                let drop = self.input_history.len() - MAX_HISTORY;
                self.input_history.drain(0..drop);
            }
            persist_history(&self.input_history);
        }
        self.history_pos = None;
        self.history_draft.clear();
    }

    /// Recall the previous (older) submission into the input.
    fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = self.editor.content();
                self.input_history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        let text = self.input_history[pos].clone();
        self.editor.set_content(&text);
    }

    /// Move toward newer submissions; past the newest, restore the draft.
    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.input_history.len() {
            self.history_pos = Some(pos + 1);
            let text = self.input_history[pos + 1].clone();
            self.editor.set_content(&text);
        } else {
            self.history_pos = None;
            let draft = self.history_draft.clone();
            self.editor.set_content(&draft);
        }
    }

    /// The active completion popup contents: slash commands when the line starts
    /// with `/`, else `@file` paths when an `@…` token is being typed.
    pub(crate) fn active_completions(&mut self) -> Option<Completions> {
        let content = self.editor.content();
        let slash = slash_completions(&content);
        if !slash.is_empty() {
            return Some(Completions {
                kind: CompletionKind::Slash,
                items: slash
                    .into_iter()
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .collect(),
            });
        }
        if let Some((start, query)) = active_file_token(&content) {
            let items = self.file_completion_items(&query);
            if !items.is_empty() {
                return Some(Completions {
                    kind: CompletionKind::File { token_start: start },
                    items,
                });
            }
        }
        None
    }

    /// Apply the selected completion. `trailing_space` adds a space after the
    /// inserted text (Tab keeps editing; a slash Enter omits it so the bare
    /// command submits).
    fn apply_completion(&mut self, comp: &Completions, idx: usize, trailing_space: bool) {
        let chosen = &comp.items[idx].0;
        match comp.kind {
            CompletionKind::Slash => {
                if trailing_space {
                    self.editor.set_content(&format!("{chosen} "));
                } else {
                    self.editor.set_content(chosen);
                }
            }
            CompletionKind::File { token_start } => {
                let content = self.editor.content();
                // Replace the partial `@…` token with `@<path> ` (always a space
                // so the next mention/word is separate).
                let prefix = content.get(..token_start).unwrap_or("");
                self.editor.set_content(&format!("{prefix}@{chosen} "));
            }
        }
    }

    /// Build (and cache) the list of files under the cwd, then rank by `query`.
    fn file_completion_items(&mut self, query: &str) -> Vec<(String, String)> {
        self.ensure_file_index();
        let q = query.to_ascii_lowercase();
        let mut scored: Vec<(u8, usize, &String)> = self
            .file_index
            .iter()
            .filter_map(|p| {
                if q.is_empty() {
                    return Some((1u8, p.len(), p));
                }
                let lp = p.to_ascii_lowercase();
                let base = lp.rsplit('/').next().unwrap_or(&lp);
                if base.starts_with(&q) {
                    Some((0, p.len(), p))
                } else if lp.contains(&q) {
                    Some((1, p.len(), p))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
        scored
            .into_iter()
            .take(8)
            .map(|(_, _, p)| (p.clone(), String::new()))
            .collect()
    }

    /// Rebuild `file_index` if it's stale for the current cwd.
    fn ensure_file_index(&mut self) {
        let Some(cwd) = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.cwd())
            .or_else(|| std::env::current_dir().ok())
        else {
            return;
        };
        if self.file_index_cwd.as_deref() == Some(cwd.as_path()) && !self.file_index.is_empty() {
            return;
        }
        self.file_index = walk_files(&cwd);
        self.file_index_cwd = Some(cwd);
    }

    /// Expand `@file` mentions in `input` by appending the referenced files'
    /// contents (for the model only). Unreadable or missing references are left
    /// as-is. Returns `input` unchanged when there are no resolvable mentions.
    fn expand_mentions(&self, input: &str) -> String {
        let Some(cwd) = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.cwd())
            .or_else(|| std::env::current_dir().ok())
        else {
            return input.to_string();
        };
        const MAX_BYTES: usize = 100 * 1024;
        let mut attached: Vec<(String, String)> = Vec::new();
        for raw in input.split_whitespace() {
            let Some(rel) = raw.strip_prefix('@') else {
                continue;
            };
            let rel = rel.trim_end_matches([',', '.', ';', ':', ')', ']', '}']);
            if rel.is_empty() || attached.iter().any(|(p, _)| p == rel) {
                continue;
            }
            let path = cwd.join(rel);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let text = if text.len() > MAX_BYTES {
                    format!("{}\n…[truncated]", &text[..MAX_BYTES])
                } else {
                    text
                };
                attached.push((rel.to_string(), text));
            }
        }
        if attached.is_empty() {
            return input.to_string();
        }
        let mut out = String::from(input);
        out.push_str("\n\n--- Referenced files (via @) ---\n");
        for (rel, text) in attached {
            out.push_str(&format!("\n=== {rel} ===\n{text}\n"));
        }
        out
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
                // Notify on completion of a non-trivial turn (if enabled).
                self.maybe_bell();
                // Persist the completed turn into the active session, if any.
                self.autosave();
                // If this was an /init turn, reload AGENTS.md into the prompt.
                if self.pending_init {
                    self.pending_init = false;
                    self.reload_project_docs();
                }
                // Auto-compact near the context limit before doing more work;
                // its Compacted handler resumes the queue afterward.
                if self.should_auto_compact() {
                    self.transcript.push(Entry::System(
                        "context near the limit — auto-compacting…".to_string(),
                    ));
                    self.spawn_compaction(None);
                    return;
                }
                // Start the next queued message, if any (FIFO).
                if let Some(next) = self.queue.pop_front() {
                    self.spawn_turn(next);
                }
            }
            TurnMsg::Compacted(res) => {
                self.turn_handle = None;
                self.running = false;
                self.compacting = false;
                // Context shrank; drop stale usage so the status bar refreshes
                // on the next turn (and we don't immediately re-trigger).
                self.last_usage = None;
                match res {
                    Ok((before, after)) => {
                        self.status = "ready".to_string();
                        self.transcript.push(Entry::System(format!(
                            "compacted: {before} → {after} messages (summary kept; scrollback \
                             above is preserved for you)"
                        )));
                        self.autosave();
                    }
                    Err(e) => {
                        self.status = format!("compact failed: {e}");
                        self.transcript
                            .push(Entry::System(format!("[compact failed] {e}")));
                    }
                }
                self.scroll_offset = 0;
                // Resume any queued work now that the context is compact.
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
    ("/compact", "summarize the conversation to reclaim context"),
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
    ("/init", "analyze the project and write an AGENTS.md"),
    ("/add", "attach a file (or type @path inline)"),
    ("/diff", "show git diff of the working tree"),
    ("/reasoning", "toggle showing model reasoning"),
    ("/temp", "show or set temperature"),
    ("/effort", "show or set effort label"),
    ("/info", "session info"),
    ("/copy", "copy reply (or 'code' / 'all')"),
    ("/paste", "paste clipboard (file path → attach)"),
    ("/edit", "open a file in $EDITOR"),
    ("/retry", "re-run last turn (optional model)"),
    ("/undo", "undo last turn (edit & resend)"),
    ("/help", "list commands"),
    ("/exit", "quit"),
    // Aliases for users switching from other agents (resolved by resolve_alias).
    ("/new", "alias of /clear"),
    ("/reset", "alias of /clear"),
    ("/cd", "alias of /cwd"),
    ("/status", "alias of /info"),
    ("/continue", "alias of /resume"),
    ("/summarize", "alias of /compact"),
];

/// Commands grouped by theme for a readable `/help`.
const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Session",
        &[
            "/clear",
            "/sessions",
            "/resume",
            "/rename",
            "/compact",
            "/info",
        ],
    ),
    (
        "Model & sampling",
        &[
            "/model",
            "/models",
            "/provider",
            "/temp",
            "/effort",
            "/reasoning",
        ],
    ),
    (
        "Files & context",
        &[
            "/init", "/add", "/edit", "/diff", "/cwd", "/tools", "/paste",
        ],
    ),
    ("Reply", &["/copy", "/retry", "/undo"]),
    ("Appearance", &["/theme"]),
    ("Other", &["/help", "/exit"]),
];

/// Render the grouped, aligned `/help` text.
fn help_text() -> String {
    let desc = |name: &str| {
        SLASH_COMMANDS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| *d)
            .unwrap_or("")
    };
    let mut s = String::from("Commands");
    for (group, names) in HELP_GROUPS {
        s.push_str(&format!("\n\n{group}"));
        for name in *names {
            s.push_str(&format!("\n  {name:<11}{}", desc(name)));
        }
    }
    s.push_str(
        "\n\nTips: @path attaches a file · Up/Down recalls history · Ctrl+L redraws · \
         Ctrl+C twice quits",
    );
    s
}

/// The active completion popup's contents and kind.
pub(crate) struct Completions {
    pub(crate) kind: CompletionKind,
    /// `(label, description)` rows; the label is the text inserted on accept.
    pub(crate) items: Vec<(String, String)>,
}

/// Which completion is active, and how to apply the selection.
pub(crate) enum CompletionKind {
    /// Replace the whole input with the chosen command.
    Slash,
    /// Replace the `@…` token starting at this byte offset with `@<path> `.
    File { token_start: usize },
}

impl Completions {
    /// Popup title shown on the border.
    pub(crate) fn title(&self) -> &'static str {
        match self.kind {
            CompletionKind::Slash => " commands · Tab ",
            CompletionKind::File { .. } => " files · Tab ",
        }
    }
}

/// If an `@…` file mention is being typed at the end of `input`, return the byte
/// offset of the `@` and the partial query after it. Requires the `@` to start a
/// token (preceded by start-of-input or whitespace) with no whitespace after it.
fn active_file_token(input: &str) -> Option<(usize, String)> {
    let at = input.rfind('@')?;
    // Must start a token.
    if at > 0 {
        let prev = input[..at].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let query = &input[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at, query.to_string()))
}

/// Max files indexed and max directory depth walked for `@file` completion.
const WALK_MAX_FILES: usize = 20_000;
const WALK_MAX_DEPTH: usize = 12;

/// Directory names skipped by the fallback walk (non-git projects).
const WALK_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".cache",
    "dist",
    "build",
    ".next",
    "vendor",
    ".venv",
    "__pycache__",
];

/// The last fenced (```…```) code block in markdown `md`, without the fences.
fn last_fenced_block(md: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_block = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            if in_block {
                blocks.push(std::mem::take(&mut cur));
                in_block = false;
            } else {
                in_block = true;
                cur.clear();
            }
        } else if in_block {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    blocks
        .into_iter()
        .next_back()
        .map(|b| b.trim_end().to_string())
        .filter(|b| !b.is_empty())
}

/// Max input-history entries kept (in memory and on disk).
const MAX_HISTORY: usize = 200;

/// Path to the persisted input history (`$XDG_DATA_HOME/hrdr/history`).
fn history_path() -> Option<std::path::PathBuf> {
    hjkl_xdg::data_dir("hrdr").ok().map(|d| d.join("history"))
}

/// Load persisted single-line input history (most recent `MAX_HISTORY`).
fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if v.len() > MAX_HISTORY {
        let drop = v.len() - MAX_HISTORY;
        v.drain(0..drop);
    }
    v
}

/// Persist input history (one entry per line; multi-line entries are skipped to
/// keep the line-based file well-formed).
fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = history
        .iter()
        .filter(|s| !s.contains('\n'))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, body);
}

/// Instruction sent to the model by `/init` to author an `AGENTS.md`.
const INIT_PROMPT: &str = "\
Analyze this codebase and create an AGENTS.md file at the repository root to guide \
AI coding agents working here (the open standard at https://agents.md).

Do this:
1. Explore the project with your tools — read the README(s), the build/manifest \
   files (Cargo.toml, package.json, pyproject.toml, go.mod, Makefile, etc.), CI \
   config, and skim the source layout with glob/grep/read_file to understand how \
   it's organized.
2. If an AGENTS.md (or CLAUDE.md / .cursorrules / similar) already exists, read it \
   and improve it instead of discarding useful content.
3. Write AGENTS.md (use the write_file tool) with concise, repo-specific sections:
   - Project overview: what it is and does.
   - Setup / build / run: the actual commands for THIS repo.
   - Testing: how to run the test suite and a single test.
   - Code style & conventions: formatting, linting, naming — inferred from config \
     and existing code.
   - Architecture / layout: key directories and how they fit together.
   - Gotchas or rules an agent must follow.

Prefer real commands, paths, and specifics over generic advice. Keep it tight. \
When finished, give a one-line summary of what you wrote.";

/// Collect relative file paths under `root` for `@file` completion. In a git
/// repo, honors `.gitignore`/`.ignore` (and parents/global) + `.git/info/exclude`
/// via the `ignore` crate; outside one, falls back to a manual walk that skips
/// known VCS/build and hidden directories.
fn walk_files(root: &std::path::Path) -> Vec<String> {
    if in_git_repo(root) {
        walk_files_gitignore(root)
    } else {
        walk_files_fallback(root)
    }
}

/// Whether `root` (or an ancestor) is inside a git repo. `.git` may be a
/// directory (normal) or a file (worktrees/submodules).
fn in_git_repo(root: &std::path::Path) -> bool {
    root.ancestors().any(|d| d.join(".git").exists())
}

/// Gitignore-aware walk (ripgrep's walker).
fn walk_files_gitignore(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(WALK_MAX_DEPTH))
        .hidden(true) // skip dotfiles/dotdirs
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if out.len() >= WALK_MAX_FILES {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    out
}

/// Fallback walk for non-git directories: skip hidden + known build/VCS dirs.
fn walk_files_fallback(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > WALK_MAX_DEPTH || out.len() >= WALK_MAX_FILES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if name.starts_with('.') || WALK_SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if ft.is_file()
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= WALK_MAX_FILES {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}

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
/// Resolve a slash-command alias to its canonical name (case-insensitive), so
/// users coming from other agents can use familiar names. Unknown names pass
/// through unchanged.
fn resolve_alias(cmd: &str) -> &str {
    match cmd.to_ascii_lowercase().as_str() {
        // Claude Code / opencode / aider new-session & reset names.
        "new" | "reset" => "clear",
        // aider/shell-style directory change.
        "cd" => "cwd",
        // Claude Code status line.
        "status" => "info",
        // opencode/Claude Code resume.
        "continue" => "resume",
        // descriptive name for compaction.
        "summarize" | "summary" => "compact",
        // help variants.
        "commands" | "?" => "help",
        _ => cmd,
    }
}

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
    use super::{
        active_file_token, is_quit_command, last_fenced_block, resolve_alias, slash_completions,
    };

    #[test]
    fn last_fenced_block_extraction() {
        let md = "intro\n```rust\nfn a() {}\n```\nmid\n```\nlast block\nline2\n```\nend";
        assert_eq!(last_fenced_block(md).as_deref(), Some("last block\nline2"));
        assert_eq!(last_fenced_block("no code here"), None);
        assert_eq!(last_fenced_block("```\n\n```"), None); // empty block
    }

    #[test]
    fn aliases_resolve_to_canonical() {
        assert_eq!(resolve_alias("new"), "clear");
        assert_eq!(resolve_alias("RESET"), "clear"); // case-insensitive
        assert_eq!(resolve_alias("cd"), "cwd");
        assert_eq!(resolve_alias("status"), "info");
        assert_eq!(resolve_alias("continue"), "resume");
        assert_eq!(resolve_alias("summarize"), "compact");
        assert_eq!(resolve_alias("?"), "help");
        assert_eq!(resolve_alias("model"), "model"); // unknown passes through
    }

    #[test]
    fn active_file_token_detection() {
        // Bare @ at start, or after whitespace, with a partial query.
        assert_eq!(active_file_token("@"), Some((0, String::new())));
        assert_eq!(
            active_file_token("look at @src/ma"),
            Some((8, "src/ma".into()))
        );
        // Not a token boundary (email-ish) — the @ is preceded by a non-space.
        assert_eq!(active_file_token("me@host"), None);
        // Already-completed mention followed by a space is not active.
        assert_eq!(active_file_token("@src/main.rs and"), None);
        // No @ at all.
        assert_eq!(active_file_token("hello world"), None);
    }

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
