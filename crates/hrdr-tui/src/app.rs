//! App state, the async event loop, and agent orchestration.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use hjkl_clipboard::Clipboard;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Todo};
use hrdr_editor::{PlainEngine, TuiEditorEngine, VimEngine};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;

use crate::theme::Theme;

mod commands;
mod completion;
mod session;
mod util;

use completion::CompletionKind;
pub(crate) use completion::Completions;
use hrdr_app::config_mtime as current_config_mtime;
use hrdr_app::{age_completed_todos, display_dir, git_branch, is_quit_command};
use util::timestamp_now;
// Re-exported so the `tui` driver module (which owns the event loop + terminal)
// can reach these terminal-facing helpers.
pub(crate) use util::run_editor;

// The display-mode enums live in the shared `hrdr-app` core so the TUI and GUI
// resolve/persist these settings identically.
pub(crate) use hrdr_app::{StatusBarMode, TimestampStyle};

/// What a key press asks the driver to do (for actions needing the terminal).
/// Returned by [`App::on_key`] so the render/terminal layer stays outside `App`.
pub(crate) enum Action {
    None,
    OpenEditor,
    /// Open a specific file in `$EDITOR` (from `/edit <file>`).
    OpenFile(std::path::PathBuf),
    /// Force a full clear + repaint (Ctrl+L), to fix terminal corruption.
    Redraw,
}

/// A render-agnostic clickable rectangle (screen cells), for mouse hit-testing
/// without depending on the renderer's geometry types.
#[derive(Clone, Copy)]
pub(crate) struct HitRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl HitRect {
    /// Whether the cell at `(col, row)` is inside this rectangle.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}

// The transcript item model + its representation-independent queries
// (search/count/export) live in the shared `hrdr-app` core.
pub(crate) use hrdr_app::Entry;

/// Messages from the background agent task back to the UI loop.
pub(crate) enum TurnMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
    /// Out-of-band system line (e.g. async `/models` result).
    System(String),
    /// Out-of-band diff block (e.g. async `/diff` result).
    Diff(String),
    /// Compaction finished: `Ok((before, after))` message counts, or an error.
    Compacted(Result<(usize, usize), String>),
    /// `@file` completion index built off-thread for `cwd`.
    FileIndex(std::path::PathBuf, Vec<String>),
    /// The config file changed on disk (from the shared watcher).
    ConfigChanged,
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) editor: Box<dyn TuiEditorEngine>,
    /// Resolved chat-UI colors (from an hjkl theme).
    pub(crate) theme: Theme,
    pub(crate) transcript: Vec<Entry>,
    /// Local timestamp per transcript entry (parallel to `transcript`), rendered
    /// as relative or absolute time at draw.
    pub(crate) entry_times: Vec<chrono::DateTime<chrono::Local>>,
    /// Per-message timestamp style: none / relative / exact (`/timestamps`).
    pub(crate) timestamp_style: TimestampStyle,
    /// Status-bar mode: none / truncate / wrap (`/statusbar`).
    pub(crate) statusbar_mode: StatusBarMode,
    pub(crate) running: bool,
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
    /// Last-seen mtime of the config file, for hot-reload polling.
    config_mtime: Option<SystemTime>,
    /// OS clipboard for `/copy` (None if unavailable).
    clipboard: Option<Clipboard>,
    /// Selected row in the completion popup (slash command or `@file`).
    pub(crate) completion_idx: usize,
    /// Submitted-input history + Up/Down browsing (shared with the GUI).
    history: hrdr_app::HistoryBrowser,
    /// Cached relative file paths under the cwd, for `@file` completion.
    file_index: Vec<String>,
    /// The cwd `file_index` was built for; rebuilt when the cwd changes.
    file_index_cwd: Option<std::path::PathBuf>,
    /// An off-thread index build is in flight (don't spawn another).
    file_index_building: bool,
    /// Whether to render the model's reasoning (`<think>`) blocks (`/reasoning`).
    pub(crate) show_reasoning: bool,
    /// Show every tool result in full (`/expand all`); per-entry `expanded`
    /// overrides this for individual results.
    pub(crate) expand_tools: bool,
    /// True while a compaction (summarization) pass is running.
    pub(crate) compacting: bool,
    /// True while an `/init` turn runs, so its result reloads `AGENTS.md`.
    pending_init: bool,
    /// A file `/edit` requested to open in `$EDITOR`, consumed by the run loop.
    pending_edit: Option<std::path::PathBuf>,
    /// An in-progress `/login` wizard; while `Some`, submitted lines feed it
    /// instead of the model or the slash dispatcher.
    login: Option<hrdr_app::LoginWizard>,
    /// A `/goto` target message number, resolved to a scroll offset at draw.
    pub(crate) pending_goto: Option<usize>,
    /// Last `/find` query (also drives transcript highlighting) and the message
    /// number it last landed on (for cycling).
    pub(crate) find: hrdr_app::FindState,
    /// Auto-compact enable carrier: `0` (or out of range) disables it.
    pub(crate) auto_compact_ratio: f64,
    /// Tokens reserved below the context window — auto-compaction fires at
    /// `context_window − compaction_reserved` (opencode's model).
    pub(crate) compaction_reserved: u32,
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
    /// Count of completed turns, used to age out finished TODO items.
    todo_turn: u64,
    /// Turn (in `todo_turn` units) each completed TODO was first seen finished,
    /// keyed by content. Completed items are pruned `todo_ttl` turns after that
    /// so the list doesn't accrete stale checkmarks.
    todo_completed_at: HashMap<String, u64>,
    /// Turns a completed TODO stays visible before pruning (config `todo_ttl`).
    todo_ttl: u64,
    /// Messages submitted while a turn is running, processed FIFO once it ends.
    pub(crate) queue: VecDeque<String>,
    /// Screen rect of the "follow output" button, set during draw while scrolled
    /// up so mouse clicks can hit-test against it. `None` when following.
    pub(crate) follow_button: Option<HitRect>,
    /// Clickable screen rects for each visible tool block → its transcript index,
    /// set during draw. A left click toggles that tool's `expanded` (like a
    /// per-entry `/expand`).
    pub(crate) tool_hits: Vec<(HitRect, usize)>,
    /// Set after one idle Ctrl+C; a second consecutive Ctrl+C quits. Any other
    /// key (or a mouse action) disarms it.
    pub(crate) quit_armed: bool,
    // ---- live inference stats (for the loader above the input) ----
    /// When the current turn started (for elapsed time + spinner).
    pub(crate) turn_started: Option<Instant>,
    /// Wall-clock start of the current turn (for the loader's "started …").
    pub(crate) turn_started_at: Option<chrono::DateTime<chrono::Local>>,
    /// When the first output token of the turn arrived (for tok/s).
    pub(crate) first_token_at: Option<Instant>,
    /// Streamed output deltas this turn (≈ tokens).
    pub(crate) out_tokens: usize,
    /// `(prompt_tokens, completion_tokens)` from the latest model call.
    pub(crate) last_usage: Option<(u32, u32)>,
    tx: mpsc::UnboundedSender<TurnMsg>,
    pub(crate) rx: Option<mpsc::UnboundedReceiver<TurnMsg>>,
    pub(crate) should_quit: bool,
}

impl App {
    pub(crate) fn new(config: AgentConfig, ui: hrdr_app::UiConfig) -> Result<Self> {
        let model = config.model.clone();
        let vim_mode = ui.vim_mode;
        let theme = Theme::load(ui.theme.as_deref());
        let dir = display_dir(&config.cwd);
        let branch = git_branch(&config.cwd);
        let context_window = config.context_window;
        let auto_compact = config.auto_compact;
        let compaction_reserved = config.compaction_reserved;
        let auto_resume = ui.auto_resume;
        let bell = ui.bell;
        let todo_ttl = ui.todo_ttl;
        let show_thinking = ui.show_thinking;
        let timestamp_style = TimestampStyle::from_config(ui.timestamps.as_deref());
        let statusbar_mode = StatusBarMode::from_config(ui.statusbar.as_deref());
        // No portable terminal-font probe, so an unset/`auto` icons setting
        // resolves to Nerd glyphs.
        let icon_mode = ui
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
        let editor: Box<dyn TuiEditorEngine> = if vim_mode {
            Box::new(VimEngine::new())
        } else {
            Box::new(PlainEngine::new())
        };
        let welcome = if vim_mode {
            "hrdr ready (vim mode). Insert to type, Esc for Normal, Enter in Normal sends, \
             Ctrl+G opens $EDITOR. Type @path to attach a file. /help for commands; \
             /exit (Ctrl+C twice, or Ctrl+D on an empty line) to quit."
        } else {
            "hrdr ready. Type a message; Enter sends, Alt+Enter or \\+Enter for a newline \
             (Shift+Enter too on supporting terminals), Ctrl+G opens $EDITOR. Type @path to \
             attach a file. /help for commands; /exit (Ctrl+C twice, or Ctrl+D on an empty line) \
             to quit. Submit while a reply runs to queue follow-ups."
        };
        let mut transcript = vec![Entry::System(welcome.to_string())];
        // Warn (but don't fail) if the config file exists but is invalid — the
        // running config has already fallen back to defaults + env in that case.
        if let Some(warning) = hrdr_app::startup_config_warning() {
            transcript.push(Entry::System(warning));
        }
        if project_docs_loaded {
            transcript.push(Entry::System(hrdr_app::PROJECT_DOCS_LOADED_MSG.to_string()));
        }
        let entry_times = vec![timestamp_now(); transcript.len()];
        let mut app = Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor,
            theme,
            transcript,
            entry_times,
            timestamp_style,
            statusbar_mode,
            running: false,
            model,
            dir,
            branch,
            context_window,
            effort,
            icon_mode,
            session_in: 0,
            session_out: 0,
            cfg,
            config_mtime: current_config_mtime(),
            clipboard: Clipboard::new().ok(),
            completion_idx: 0,
            history: hrdr_app::HistoryBrowser::load(),
            file_index: Vec::new(),
            file_index_cwd: None,
            file_index_building: false,
            show_reasoning: show_thinking,
            expand_tools: false,
            compacting: false,
            pending_init: false,
            pending_edit: None,
            login: None,
            pending_goto: None,
            find: hrdr_app::FindState::default(),
            auto_compact_ratio: auto_compact,
            compaction_reserved,
            bell,
            base_url,
            session_id: None,
            session_label: None,
            turn_handle: None,
            scroll_offset: 0,
            transcript_height: 24,
            max_scroll: 0,
            todos,
            todo_turn: 0,
            todo_completed_at: HashMap::new(),
            todo_ttl,
            queue: VecDeque::new(),
            follow_button: None,
            tool_hits: Vec::new(),
            quit_armed: false,
            turn_started: None,
            turn_started_at: None,
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
    pub(crate) fn spawn_health_check(&self) {
        let agent = self.agent.clone();
        let model = self.model.clone();
        let base_url = self.base_url.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(warning) = hrdr_app::endpoint_health_warning(agent, model, base_url).await {
                let _ = tx.send(TurnMsg::System(warning));
            }
        });
    }

    /// Start the shared config-file watch, piping change pings into the UI
    /// loop (dedup happens in [`Self::maybe_reload_config`]'s mtime guard).
    /// The returned guard must be kept alive for the watch to stay active.
    pub(crate) fn start_config_watch(&self) -> hrdr_app::ConfigWatcherGuard {
        let tx = self.tx.clone();
        hrdr_app::watch_config(move || {
            let _ = tx.send(TurnMsg::ConfigChanged);
        })
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Action {
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
                // Ctrl+D on an empty input quits (shell-style EOF).
                KeyCode::Char('d') if self.editor.content().is_empty() => {
                    self.should_quit = true;
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

        // Convert to the seam's renderer-agnostic key (None = release event,
        // which must not reach the engines).
        let Some(ekey) = hrdr_editor::key_from_crossterm(&key) else {
            return Action::None;
        };
        // The engine decides whether this key submits (vim: Enter in Normal;
        // plain: Enter without a newline modifier / trailing backslash).
        if self.editor.wants_submit(&ekey) {
            let input = self.editor.content();
            if input.trim().is_empty() {
                return Action::None;
            }
            // A running `/login` wizard captures the line (an API key must never
            // reach input history or the transcript), before anything else.
            if self.login.is_some() {
                self.login_feed(input.trim());
                self.editor.set_content("");
                self.scroll_offset = 0;
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

        self.editor.feed_key(ekey);
        Action::None
    }

    /// Mouse: wheel scrolls the transcript; a left click on the follow button
    /// resumes following the newest output.
    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
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
                    && rect.contains(m.column, m.row)
                {
                    self.scroll_offset = 0;
                    return;
                }
                // Click a tool block to toggle its full output (per-entry /expand).
                let hit = self
                    .tool_hits
                    .iter()
                    .find(|(r, _)| r.contains(m.column, m.row))
                    .map(|(_, i)| *i);
                if let Some(idx) = hit
                    && let Some(Entry::Tool { expanded, .. }) = self.transcript.get_mut(idx)
                {
                    *expanded = !*expanded;
                }
            }
            _ => {}
        }
    }

    pub(crate) fn system(&mut self, msg: impl Into<String>) {
        self.push_entry(Entry::System(msg.into()));
    }

    /// Run `f` with the locked agent, returning its result — or `None` if a turn
    /// currently holds the lock. For fire-and-forget mutations (ignore the
    /// `None`) or optional reads.
    fn with_agent<T>(&self, f: impl FnOnce(&mut Agent) -> T) -> Option<T> {
        self.agent.try_lock().ok().map(|mut a| f(&mut a))
    }

    /// Like [`Self::with_agent`], but emits the standard "busy" system line when
    /// the agent is locked, so callers can `let Some(x) = …_or_busy(…) else {
    /// return; }`.
    fn with_agent_or_busy<T>(&mut self, f: impl FnOnce(&mut Agent) -> T) -> Option<T> {
        let result = self.with_agent(f);
        if result.is_none() {
            self.system("busy — try again after the current turn");
        }
        result
    }

    /// Append a transcript entry, stamping it with the current local time so the
    /// `entry_times` vector stays parallel to `transcript`.
    fn push_entry(&mut self, e: Entry) {
        self.transcript.push(e);
        self.entry_times.push(timestamp_now());
    }

    /// Clear the transcript (and its parallel timestamps).
    fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.entry_times.clear();
    }

    /// Truncate the transcript (and its parallel timestamps) to `len`.
    fn truncate_transcript(&mut self, len: usize) {
        self.transcript.truncate(len);
        self.entry_times.truncate(len);
    }

    /// Age out finished TODO items. Called once per turn (on `Done`, so it also
    /// runs when a turn errors — same trigger as the GUI).
    fn prune_completed_todos(&mut self) {
        if let Ok(mut todos) = self.todos.lock() {
            age_completed_todos(
                &mut todos,
                &mut self.todo_completed_at,
                self.todo_turn,
                self.todo_ttl,
            );
        }
    }

    /// The tools' current working directory (agent's, or the process cwd while
    /// a turn holds the agent lock).
    fn current_cwd(&self) -> String {
        if let Some(cwd) = self.with_agent(|a| a.cwd()) {
            return cwd.display().to_string();
        }
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// Switch the tools' working directory: update the agent and the status bar.
    fn apply_cwd(&mut self, new: std::path::PathBuf) {
        self.with_agent(|a| a.set_cwd(new.clone()));
        self.dir = display_dir(&new);
        self.branch = git_branch(&new);
        self.file_index_cwd = None; // force a rebuild for the new directory
    }

    /// Apply the live-changeable settings from a (config, ui-config) pair. Does
    /// NOT touch the model/provider/endpoint (those are session-scoped).
    fn apply_runtime_config(&mut self, cfg: &AgentConfig, ui: &hrdr_app::UiConfig) {
        self.theme = Theme::load(ui.theme.as_deref());
        self.effort = cfg.effort.clone();
        self.auto_compact_ratio = cfg.auto_compact;
        self.compaction_reserved = cfg.compaction_reserved;
        self.bell = ui.bell;
        self.todo_ttl = ui.todo_ttl;
        self.timestamp_style = TimestampStyle::from_config(ui.timestamps.as_deref());
        self.statusbar_mode = StatusBarMode::from_config(ui.statusbar.as_deref());
        self.show_reasoning = ui.show_thinking;
        self.icon_mode = ui
            .icons
            .as_deref()
            .and_then(hjkl_icons::IconMode::from_config)
            .unwrap_or(hjkl_icons::IconMode::Nerd);
        if let Some(t) = cfg.temperature {
            self.with_agent(|a| a.set_temperature(Some(t)));
        }
    }

    /// Re-load config and apply it. On an invalid file, keep the current
    /// settings and warn instead of resetting.
    fn apply_config_reload(&mut self, manual: bool) {
        match AgentConfig::load_checked() {
            Ok(cfg) => {
                self.apply_runtime_config(&cfg, &hrdr_app::UiConfig::load());
                self.cfg = cfg;
                self.system(if manual {
                    hrdr_app::RELOAD_MANUAL_MSG
                } else {
                    hrdr_app::RELOAD_HOT_MSG
                });
            }
            Err(e) => self.system(hrdr_app::reload_invalid_message(&e)),
        }
        // Either way, stop re-triggering for this version of the file.
        self.config_mtime = current_config_mtime();
    }

    /// Hot-reload: poll the config file's mtime and apply changes when it's
    /// edited (manually or by another session).
    pub(crate) fn maybe_reload_config(&mut self) {
        let mtime = current_config_mtime();
        if mtime != self.config_mtime {
            self.apply_config_reload(false);
        }
    }

    /// Persist a single setting to the user config file, suppressing the
    /// resulting hot-reload (we already applied it in memory).
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        match hrdr_agent::persist_setting(key, value) {
            Ok(_) => self.config_mtime = current_config_mtime(),
            Err(e) => self.system(format!("couldn't save '{key}' to config: {e}")),
        }
    }

    /// Remove a setting from the user config file (e.g. resetting the theme).
    fn unpersist_setting(&mut self, key: &str) {
        match hrdr_agent::remove_setting(key) {
            Ok(_) => self.config_mtime = current_config_mtime(),
            Err(e) => self.system(format!("couldn't update config: {e}")),
        }
    }

    /// Re-gather `AGENTS.md` for the current cwd and refresh the system prompt
    /// in place (e.g. after `/init` writes one).
    fn reload_project_docs(&mut self) {
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(line) = hrdr_app::reload_project_docs(agent).await {
                let _ = tx.send(TurnMsg::System(line));
            }
        });
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
        self.push_entry(Entry::System(hrdr_app::cancel_message(dropped)));
    }

    fn spawn_turn(&mut self, input: String) {
        // Commit the message into history at send time (a queued message lives
        // as a pending bottom item until this point).
        self.push_entry(Entry::User(input.clone()));
        // Expand `@file` mentions into attached contents for the model only; the
        // transcript still shows the message as the user typed it.
        let sent = hrdr_app::expand_mentions(&input, &hrdr_app::agent_cwd(&self.agent));
        self.launch_turn(sent);
    }

    /// Run a turn against the model with `input` as the (already-prepared) user
    /// message. The caller is responsible for any transcript display.
    fn launch_turn(&mut self, input: String) {
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.turn_started_at = Some(chrono::Local::now());
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

    /// Ring the terminal bell when a turn finishes (shared gate: enabled +
    /// ran at least [`hrdr_app::BELL_MIN_SECS`], so quick replies stay silent).
    fn maybe_bell(&self) {
        let elapsed = self.turn_started.map(|t| t.elapsed().as_secs_f64());
        if hrdr_app::should_bell(self.bell, elapsed) {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07"); // BEL
            let _ = out.flush();
        }
    }

    /// Whether the context has grown enough to auto-compact (with headroom).
    /// A configured ratio of `0` (or outside `0.0..=1.0`) disables it.
    fn should_auto_compact(&self) -> bool {
        !self.compacting
            && hrdr_app::should_auto_compact(
                self.last_usage.map(|(p, _)| p),
                self.context_window,
                self.compaction_reserved,
                self.auto_compact_ratio > 0.0 && self.auto_compact_ratio <= 1.0,
            )
    }

    /// Run a compaction pass on the background task, reporting via `TurnMsg`.
    fn spawn_compaction(&mut self, instructions: Option<String>) {
        self.running = true;
        self.compacting = true;
        self.turn_started = Some(Instant::now());
        self.turn_started_at = Some(chrono::Local::now());
        self.first_token_at = None;
        self.out_tokens = 0;
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let res = hrdr_app::run_compaction(agent, instructions).await;
            let _ = tx.send(TurnMsg::Compacted(res));
        });
        self.turn_handle = Some(handle);
    }

    /// Record a submitted input for Up/Down recall (shared browser).
    fn record_history(&mut self, input: &str) {
        self.history.record(input);
    }

    /// Recall the previous (older) submission into the input.
    fn history_prev(&mut self) {
        let current = self.editor.content();
        if let Some(text) = self.history.recall_prev(&current) {
            self.editor.set_content(&text);
        }
    }

    /// Move toward newer submissions; past the newest, restore the draft.
    fn history_next(&mut self) {
        if let Some(text) = self.history.recall_next() {
            self.editor.set_content(&text);
        }
    }

    pub(crate) fn on_turn_msg(&mut self, msg: TurnMsg) {
        match msg {
            TurnMsg::Event(ev) => {
                // Ignore buffered events after cancellation.
                if self.running {
                    self.apply_event(ev);
                }
            }
            TurnMsg::System(text) => {
                self.push_entry(Entry::System(text));
                self.scroll_offset = 0;
            }
            TurnMsg::Diff(text) => {
                self.push_entry(Entry::Diff(text));
                self.scroll_offset = 0;
            }
            TurnMsg::Done(err) => {
                if !self.running {
                    // Stale Done from an aborted task; discard.
                    return;
                }
                self.turn_handle = None;
                self.running = false;
                if let Some(e) = err {
                    self.push_entry(Entry::System(format!("[error] {e}")));
                }
                // Append the final stats for the turn (before stats are reset by
                // any queued turn that spawns next).
                if let Some(stats) = self.turn_stats() {
                    self.push_entry(Entry::Stats(stats));
                }
                // Age out completed TODOs once per turn.
                self.todo_turn += 1;
                self.prune_completed_todos();
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
                    self.push_entry(Entry::System(
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
            TurnMsg::FileIndex(cwd, files) => {
                self.file_index = files;
                self.file_index_cwd = Some(cwd);
                self.file_index_building = false;
            }
            TurnMsg::ConfigChanged => self.maybe_reload_config(),
            TurnMsg::Compacted(res) => {
                self.turn_handle = None;
                self.running = false;
                self.compacting = false;
                // Context shrank; drop stale usage so the status bar refreshes
                // on the next turn (and we don't immediately re-trigger).
                self.last_usage = None;
                self.push_entry(Entry::System(hrdr_app::compaction_message(&res)));
                if res.is_ok() {
                    self.autosave();
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
        hrdr_app::turn_stats_line(
            started.elapsed().as_secs_f64(),
            self.first_token_at
                .map(|t0| t0.duration_since(started).as_secs_f64()),
            self.out_tokens,
            self.last_usage,
        )
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
                    _ => self.push_entry(Entry::Assistant(t)),
                }
            }
            AgentEvent::Reasoning(t) => {
                self.count_token();
                match self.transcript.last_mut() {
                    Some(Entry::Reasoning(s)) => s.push_str(&t),
                    _ => self.push_entry(Entry::Reasoning(t)),
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
                self.push_entry(Entry::Tool {
                    id,
                    name,
                    args,
                    result: String::new(),
                    ok: true,
                    done: false,
                    expanded: false,
                });
            }
            AgentEvent::ToolOutput { id, chunk } => {
                // Append live output to the running tool's entry.
                for entry in self.transcript.iter_mut().rev() {
                    if let Entry::Tool {
                        id: tid,
                        result: r,
                        done,
                        ..
                    } = entry
                        && *tid == id
                        && !*done
                    {
                        r.push_str(&chunk);
                        break;
                    }
                }
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
            AgentEvent::Notice(text) => {
                self.push_entry(Entry::System(text));
                self.scroll_offset = 0;
            }
            AgentEvent::TurnDone => {}
        }
    }
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    /// The TUI's TODO-panel default lifetime must track the shared UI-config
    /// default (the aging logic itself is tested in `hrdr-app`).
    #[test]
    fn ttl_matches_config_default() {
        assert_eq!(5, hrdr_app::DEFAULT_TODO_TTL);
    }
}
