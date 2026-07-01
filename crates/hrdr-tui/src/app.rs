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
use hrdr_editor::{EditorEngine, PlainEngine, VimEngine};
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
use hrdr_app::{display_dir, git_branch, is_quit_command};
use util::{
    MAX_HISTORY, age_completed_todos, current_config_mtime, load_history, persist_history,
    timestamp_now,
};
// Re-exported so the `tui` driver module (which owns the event loop + terminal)
// can reach these terminal-facing helpers.
pub(crate) use util::{run_editor, setup_config_watcher};

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
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) editor: Box<dyn EditorEngine>,
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
    /// Show every tool result in full (`/expand all`); per-entry `expanded`
    /// overrides this for individual results.
    pub(crate) expand_tools: bool,
    /// True while a compaction (summarization) pass is running.
    pub(crate) compacting: bool,
    /// True while an `/init` turn runs, so its result reloads `AGENTS.md`.
    pending_init: bool,
    /// A file `/edit` requested to open in `$EDITOR`, consumed by the run loop.
    pending_edit: Option<std::path::PathBuf>,
    /// A `/goto` target message number, resolved to a scroll offset at draw.
    pub(crate) pending_goto: Option<usize>,
    /// Last `/find` query (also drives transcript highlighting) and the message
    /// number it last landed on (for cycling).
    pub(crate) find_query: Option<String>,
    find_pos: usize,
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
        let todo_ttl = config.todo_ttl;
        let timestamp_style = TimestampStyle::from_config(config.timestamps.as_deref());
        let statusbar_mode = StatusBarMode::from_config(config.statusbar.as_deref());
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
        if let Err(e) = AgentConfig::load_checked() {
            transcript.push(Entry::System(format!(
                "config file is invalid — using defaults: {e}"
            )));
        }
        if project_docs_loaded {
            transcript.push(Entry::System(
                "loaded project instructions from AGENTS.md".to_string(),
            ));
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
            input_history: load_history(),
            history_pos: None,
            history_draft: String::new(),
            file_index: Vec::new(),
            file_index_cwd: None,
            show_reasoning: true,
            expand_tools: false,
            compacting: false,
            pending_init: false,
            pending_edit: None,
            pending_goto: None,
            find_query: None,
            find_pos: 0,
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
            todo_turn: 0,
            todo_completed_at: HashMap::new(),
            todo_ttl,
            queue: VecDeque::new(),
            follow_button: None,
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
        let Some(client) = self.with_agent(|a| a.client()) else {
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

    /// Age out finished TODO items. Called once per turn (on `TurnDone`).
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

    /// Apply the live-changeable settings from a config. Does NOT touch the
    /// model/provider/endpoint (those are session-scoped).
    fn apply_runtime_config(&mut self, cfg: &AgentConfig) {
        self.theme = Theme::load(cfg.theme.as_deref());
        self.effort = cfg.effort.clone();
        self.auto_compact_ratio = cfg.auto_compact;
        self.bell = cfg.bell;
        self.todo_ttl = cfg.todo_ttl;
        self.timestamp_style = TimestampStyle::from_config(cfg.timestamps.as_deref());
        self.statusbar_mode = StatusBarMode::from_config(cfg.statusbar.as_deref());
        self.icon_mode = cfg
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
                self.apply_runtime_config(&cfg);
                self.cfg = cfg;
                self.system(if manual {
                    "reloaded config (theme, icons, effort, toggles)"
                } else {
                    "config changed on disk — reloaded"
                });
            }
            Err(e) => self.system(format!("config invalid — keeping current settings: {e}")),
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
        let Some(loaded) = self.with_agent(|a| {
            let cwd = a.cwd();
            a.set_cwd(cwd);
            a.project_docs().is_some()
        }) else {
            return;
        };
        if loaded {
            self.system("loaded AGENTS.md into the system prompt");
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
        let msg = if dropped > 0 {
            format!("[cancelled · {dropped} queued message(s) discarded]")
        } else {
            "[cancelled]".to_string()
        };
        self.push_entry(Entry::System(msg));
    }

    fn spawn_turn(&mut self, input: String) {
        // Commit the message into history at send time (a queued message lives
        // as a pending bottom item until this point).
        self.push_entry(Entry::User(input.clone()));
        // Expand `@file` mentions into attached contents for the model only; the
        // transcript still shows the message as the user typed it.
        let cwd = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.cwd())
            .or_else(|| std::env::current_dir().ok());
        let sent = match cwd {
            Some(cwd) => hrdr_app::expand_mentions(&input, &cwd),
            None => input.clone(),
        };
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
        self.turn_started = Some(Instant::now());
        self.turn_started_at = Some(chrono::Local::now());
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
            TurnMsg::Compacted(res) => {
                self.turn_handle = None;
                self.running = false;
                self.compacting = false;
                // Context shrank; drop stale usage so the status bar refreshes
                // on the next turn (and we don't immediately re-trigger).
                self.last_usage = None;
                match res {
                    Ok((before, after)) => {
                        self.push_entry(Entry::System(format!(
                            "compacted: {before} → {after} messages (summary kept; scrollback \
                             above is preserved for you)"
                        )));
                        self.autosave();
                    }
                    Err(e) => {
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
            AgentEvent::TurnDone => {
                self.todo_turn += 1;
                self.prune_completed_todos();
            }
        }
    }
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::Todo;
    use super::util::age_completed_todos;
    use std::collections::HashMap;

    const TTL: u64 = 5;

    fn todo(content: &str, status: &str) -> Todo {
        Todo {
            content: content.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn completed_todos_age_out_after_ttl() {
        let mut stamps = HashMap::new();
        let mut todos = vec![todo("a", "completed"), todo("b", "in_progress")];

        // Turn it completes and the next TTL-1 turns: still shown.
        for turn in 0..TTL {
            age_completed_todos(&mut todos, &mut stamps, turn, TTL);
            assert!(
                todos.iter().any(|t| t.content == "a"),
                "completed item should survive turn {turn}"
            );
        }
        // TTL turns after completion: pruned. The in-progress item stays.
        age_completed_todos(&mut todos, &mut stamps, TTL, TTL);
        assert!(!todos.iter().any(|t| t.content == "a"));
        assert!(todos.iter().any(|t| t.content == "b"));
        assert!(stamps.is_empty(), "stamp forgotten once the item is gone");
    }

    #[test]
    fn pending_todos_are_never_pruned() {
        let mut stamps = HashMap::new();
        let mut todos = vec![todo("keep", "pending")];
        for turn in 0..(TTL * 3) {
            age_completed_todos(&mut todos, &mut stamps, turn, TTL);
        }
        assert_eq!(todos.len(), 1);
        assert!(stamps.is_empty());
    }

    #[test]
    fn recompleted_item_ages_from_scratch() {
        let mut stamps = HashMap::new();
        // Completed at turn 0.
        let mut todos = vec![todo("x", "completed")];
        age_completed_todos(&mut todos, &mut stamps, 0, TTL);
        // Model flips it back to in_progress at turn 2 → stamp forgotten.
        todos[0].status = "in_progress".to_string();
        age_completed_todos(&mut todos, &mut stamps, 2, TTL);
        assert!(stamps.is_empty());
        // Re-completed at turn 3 → stamped at 3, so it survives through turn 7.
        todos[0].status = "completed".to_string();
        age_completed_todos(&mut todos, &mut stamps, 3, TTL);
        age_completed_todos(&mut todos, &mut stamps, 3 + TTL - 1, TTL);
        assert!(todos.iter().any(|t| t.content == "x"));
        age_completed_todos(&mut todos, &mut stamps, 3 + TTL, TTL);
        assert!(!todos.iter().any(|t| t.content == "x"));
    }

    #[test]
    fn ttl_matches_config_default() {
        assert_eq!(TTL, hrdr_agent::DEFAULT_TODO_TTL);
    }
}
