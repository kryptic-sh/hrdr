//! App state, the async event loop, and agent orchestration.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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
mod model_selector;
mod session;
mod session_selector;
mod theme_selector;
mod util;

use completion::CompletionKind;
pub(crate) use completion::Completions;
use hrdr_app::config_mtime as current_config_mtime;
use hrdr_app::{
    SubAgentPanel, age_completed_todos, display_dir, git_branch, is_known_command, is_quit_command,
};
pub(crate) use model_selector::ModelSelector;
pub(crate) use session_selector::SessionSelector;
pub(crate) use theme_selector::ThemeSelector;
// Re-exported so the `tui` driver module (which owns the event loop + terminal)
// can reach these terminal-facing helpers.
pub(crate) use util::run_editor;

// The display-mode enums live in the shared `hrdr-app` core so every frontend
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
pub(crate) use hrdr_app::{Entry, EntryKind};

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
    /// A model/provider switch re-probed the endpoint's advertised context window.
    ContextWindow(u32),
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
    /// ASCII art the session header animates, owned by the caller of
    /// [`crate::run`] — the TUI embeds no logo of its own.
    pub(crate) logo: &'static str,
    /// Persistent clock anchor for the header's logo animation. Captured once:
    /// re-anchoring per frame would pin the animation's tick at 0.
    pub(crate) header_anchor: Instant,
    /// The whole persisted session: the display transcript (each entry carries
    /// its own timestamp), the chat history, the TODO snapshot, the token
    /// counters, and the session's identity. Saving serializes this; resuming
    /// assigns it.
    pub(crate) state: hrdr_app::SessionState,
    /// Per-message timestamp style: none / relative / exact (`/timestamps`).
    pub(crate) timestamp_style: TimestampStyle,
    /// Status-bar mode: none / truncate / wrap (`/statusbar`).
    pub(crate) statusbar_mode: StatusBarMode,
    pub(crate) running: bool,
    // ---- status bar info ----
    /// Working directory, home-shortened for display.
    pub(crate) dir: String,
    /// Current git branch, if the cwd is in a repo.
    pub(crate) branch: Option<String>,
    /// Reasoning-effort label to display.
    pub(crate) effort: Option<String>,
    /// Icon set for the TUI chrome (status bar glyphs).
    pub(crate) icon_mode: hjkl_icons::IconMode,
    /// Config kept for mid-session provider resolution (the `/model` picker).
    cfg: AgentConfig,
    /// Last-seen mtime of the config file, for hot-reload polling.
    config_mtime: Option<SystemTime>,
    /// OS clipboard for `/copy` (None if unavailable).
    clipboard: Option<Clipboard>,
    /// Selected row in the completion popup (slash command or `@file`).
    pub(crate) completion_idx: usize,
    /// Submitted-input history + Up/Down browsing (from the shared core).
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
    /// The open `/model` selector modal; while `Some`, it captures every key.
    pub(crate) model_selector: Option<ModelSelector>,
    /// The open `/resume` session picker modal; while `Some`, it captures every key.
    pub(crate) session_selector: Option<SessionSelector>,
    /// The open `/theme` picker modal; while `Some`, it captures every key and
    /// live-previews the highlighted theme.
    pub(crate) theme_selector: Option<ThemeSelector>,
    /// USD already spent when the current session was adopted (a resumed
    /// session's saved spend); the agent's live counter adds on top of it.
    pub(crate) cost_base: f64,
    /// A `/goto` target message number, resolved to a scroll offset at draw.
    pub(crate) pending_goto: Option<usize>,
    /// A transcript index whose block should be pulled to the top of the
    /// viewport at the next draw. Set when a tool block is expanded or
    /// collapsed: the row count changes under the reader, and `scroll_offset` is
    /// measured from the bottom, so the block would otherwise jump.
    pub(crate) pending_scroll_entry: Option<usize>,
    /// A transcript index to pull to the top of the viewport at the next draw,
    /// scrolling there if the reader is following the newest output. Set by a
    /// click on a sub-agent panel row: unlike `pending_scroll_entry`, which only
    /// holds a block still while its height changes, this one *moves* the view.
    pub(crate) pending_focus_entry: Option<usize>,
    /// Last `/find` query (also drives transcript highlighting) and the message
    /// number it last landed on (for cycling).
    pub(crate) find: hrdr_app::FindState,
    /// Whether auto-compaction is enabled (the `auto_compact` toggle).
    pub(crate) auto_compact_enabled: bool,
    /// Tokens reserved below the context window — auto-compaction fires at
    /// `context_window − compaction_reserved` (opencode's model).
    pub(crate) compaction_reserved: u32,
    /// Ring the terminal bell when a turn finishes (after a brief minimum).
    bell: bool,
    /// Handle to the in-flight turn task; `abort()` cancels it.
    turn_handle: Option<JoinHandle<()>>,
    /// A turn task aborted on the quit path, kept so the event loop can `await`
    /// its termination — which drops the task's future and releases the agent
    /// lock — *before* the final autosave. Without this, that save races the
    /// runtime's async teardown of the aborted task: `autosave`'s `try_lock`
    /// can still see the lock held and skip, dropping the in-progress turn.
    quit_reap: Option<JoinHandle<()>>,
    /// Messages submitted while a turn runs, delivered mid-turn ("steering").
    /// Shared with the running `Agent::run`, which drains it between rounds.
    /// Transcript scroll offset in raw lines from the natural bottom.
    /// 0 = auto-follow (pin to newest content).
    pub(crate) scroll_offset: usize,
    /// Height of the transcript area as measured during the last draw; used
    /// by key handlers to compute half-page scroll amounts.
    pub(crate) transcript_height: u16,
    /// Max entries kept in the display transcript before oldest are evicted
    /// from the front (keeping welcome heads). Default 500.
    scrollback: usize,
    /// Max scroll offset (rows from bottom to the very top) from the last draw;
    /// lets `Home` jump to the top and bound scrolling.
    pub(crate) max_scroll: usize,
    /// Shared TODO list updated live by the `todo` tool.
    pub(crate) todos: Arc<Mutex<Vec<Todo>>>,
    /// Count of completed turns, used to age out finished TODO items.
    todo_turn: u64,
    /// Turn (in `todo_turn` units) each completed TODO was first seen finished,
    /// keyed by content. Completed items are pruned `todo_ttl` turns after that
    /// so the list doesn't accrete stale checkmarks.
    todo_completed_at: HashMap<String, u64>,
    /// Turns a completed TODO stays visible before pruning (config `todo_ttl`).
    todo_ttl: u64,
    /// Messages submitted while a turn is running, still waiting to reach the
    /// model. Shown as pending blocks below the transcript. Each also sits in
    /// [`Self::steering`] as its prepared (`@file`-expanded) text; whichever
    /// side consumes it first pops from both.
    pub(crate) queue: VecDeque<String>,
    /// The running turn's steering queue. `Agent::run` drains it before each
    /// request — i.e. right after a round's tool results — so a queued message
    /// rides in with them. Empty when no turn is running.
    steering: hrdr_agent::SteeringQueue,
    /// Screen rect of the "follow output" button, set during draw while scrolled
    /// up so mouse clicks can hit-test against it. `None` when following.
    pub(crate) follow_button: Option<HitRect>,
    /// Clickable screen rects for each visible tool block → its transcript index,
    /// set during draw. A left click toggles that tool's `expanded` (like a
    /// per-entry `/expand`).
    pub(crate) tool_hits: Vec<(HitRect, usize)>,
    /// Live blocking `task` sub-agents in the sub-agent panel, updated by the
    /// event-fold methods as `ToolStart`/`ToolOutput`/`ToolEnd` events arrive.
    pub(crate) subagent_panel: SubAgentPanel,
    /// Shared registry of *detached background* sub-agents (a clone of the
    /// agent's `ctx.background_tasks`), read live for the panel.
    pub(crate) background_tasks: Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>>,
    /// Clickable screen rect for each sub-agent panel row → the id of the `task`
    /// call that spawned it; a left click jumps to that transcript entry. `None`
    /// for a row with no call context, whose click is a no-op.
    pub(crate) subagent_hits: Vec<(HitRect, Option<String>)>,
    /// Set after one idle Ctrl+C; a second consecutive Ctrl+C quits. Any other
    /// key (or a mouse action) disarms it.
    pub(crate) quit_armed: bool,
    // ---- live inference stats (for the loader above the input) ----
    /// When the current turn started (for elapsed time + spinner).
    pub(crate) turn_started: Option<Instant>,
    /// Whether the *model* is working right now: streaming, or awaited before it
    /// starts. `false` while its tool calls run — the model is idle then, so the
    /// loader hides and its clock stops. Distinct from [`Self::running`], which
    /// stays `true` for the whole turn.
    pub(crate) inferring: bool,
    /// Tool calls in flight this round. Inference resumes when it returns to 0:
    /// a turn can issue several tools at once, and only the last one finishing
    /// hands control back to the model.
    tools_running: usize,
    /// Inference time banked from earlier rounds of this turn.
    infer_banked: Duration,
    /// When the current inference stretch began; `None` while paused.
    infer_started: Option<Instant>,
    /// Wall-clock start of the current turn (for the loader's "started …").
    pub(crate) turn_started_at: Option<chrono::DateTime<chrono::Local>>,
    /// When the first output token of the turn arrived (for tok/s).
    pub(crate) first_token_at: Option<Instant>,
    /// When the current thinking block started (for the "Thought:" footer).
    pub(crate) reasoning_start: Option<Instant>,
    /// Streamed output deltas this turn (≈ tokens).
    pub(crate) out_tokens: usize,
    /// Prompt-cache hits + reasoning tokens from the latest call, if reported.
    pub(crate) last_cached_tokens: Option<u32>,
    pub(crate) last_reasoning_tokens: Option<u32>,
    tx: mpsc::UnboundedSender<TurnMsg>,
    pub(crate) rx: Option<mpsc::UnboundedReceiver<TurnMsg>>,
    pub(crate) should_quit: bool,
}

impl App {
    pub(crate) fn new(
        config: AgentConfig,
        ui: hrdr_app::UiConfig,
        logo: &'static str,
    ) -> Result<Self> {
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
        let scrollback = ui.scrollback;
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
        let provider = config.provider.clone();
        let cfg = config.clone();
        let agent = Agent::new(config)?;
        let todos = agent.todos();
        let background_tasks = agent.background_tasks();
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
        // The banner opens every new session; the welcome text follows it.
        // Both are chrome: a resumed session gets a fresh pair, not the saved one.
        let mut transcript = vec![Entry::header(), Entry::notice(welcome)];
        // Warn (but don't fail) if the config file exists but is invalid — the
        // running config has already fallen back to defaults + env in that case.
        if let Some(warning) = hrdr_app::startup_config_warning() {
            transcript.push(Entry::notice(warning));
        }
        if project_docs_loaded {
            transcript.push(Entry::notice(hrdr_app::PROJECT_DOCS_LOADED_MSG));
        }
        let state = hrdr_app::SessionState {
            model,
            provider,
            base_url,
            transcript,
            usage: hrdr_app::SessionUsage {
                context_window,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut app = Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor,
            theme,
            logo,
            header_anchor: Instant::now(),
            state,
            timestamp_style,
            statusbar_mode,
            running: false,
            dir,
            branch,
            effort,
            icon_mode,
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
            model_selector: None,
            session_selector: None,
            theme_selector: None,
            cost_base: 0.0,
            pending_goto: None,
            pending_scroll_entry: None,
            pending_focus_entry: None,
            find: hrdr_app::FindState::default(),
            auto_compact_enabled: auto_compact,
            compaction_reserved,
            bell,
            turn_handle: None,
            quit_reap: None,
            scroll_offset: 0,
            transcript_height: 24,
            scrollback,
            max_scroll: 0,
            todos,
            todo_turn: 0,
            todo_completed_at: HashMap::new(),
            todo_ttl,
            queue: VecDeque::new(),
            steering: hrdr_agent::steering_queue(),
            inferring: false,
            tools_running: 0,
            infer_banked: Duration::ZERO,
            infer_started: None,
            follow_button: None,
            tool_hits: Vec::new(),
            subagent_panel: SubAgentPanel::default(),
            background_tasks,
            subagent_hits: Vec::new(),
            quit_armed: false,
            turn_started: None,
            turn_started_at: None,
            first_token_at: None,
            reasoning_start: None,
            out_tokens: 0,
            last_cached_tokens: None,
            last_reasoning_tokens: None,
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
        let model = self.state.model.clone();
        let base_url = self.state.base_url.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(warning) = hrdr_app::endpoint_health_warning(agent, model, base_url).await {
                let _ = tx.send(TurnMsg::System(warning));
            }
        });
    }

    /// Ask the endpoint what the model's context window is, on a background task.
    ///
    /// Only when nothing has already supplied one — a `context_window` in the
    /// config, on the provider entry, or restored from the session all pin it,
    /// and the user chose those deliberately. Without this the status bar's
    /// gauge had no "of Y" side on any endpoint that doesn't declare a window
    /// up front, because the only other probe ran on a `/model` switch.
    pub(crate) fn spawn_context_probe(&self) {
        if self.state.usage.context_window.is_some() {
            return;
        }
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let window = agent.lock().await.probe_context_window().await;
            if let Some(w) = window {
                let _ = tx.send(TurnMsg::ContextWindow(w));
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

        // The `/model` selector modal captures every key while it is open.
        if self.model_selector.is_some() {
            self.model_selector_key(key);
            return Action::None;
        }

        // Likewise the `/resume` session picker and the `/theme` picker.
        if self.session_selector.is_some() {
            self.session_selector_key(key);
            return Action::None;
        }
        if self.theme_selector.is_some() {
            self.theme_selector_key(key);
            return Action::None;
        }

        // Completion popup (slash command or `@` mention): Tab accepts the
        // selection, Up/Down move it, Enter accepts; a slash Enter then
        // submits, an `@` mention Enter just inserts and keeps editing.
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
                    // A mention just inserts; a slash command falls through
                    // to the submit path below so it runs.
                    if matches!(comp.kind, CompletionKind::Mention { .. }) {
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
                        self.request_quit();
                    } else {
                        self.quit_armed = true;
                    }
                    return Action::None;
                }
                // Ctrl+Q is an immediate, deliberate quit.
                KeyCode::Char('q') => {
                    self.request_quit();
                    return Action::None;
                }
                // Ctrl+L clears + repaints the screen (fix terminal corruption).
                KeyCode::Char('l') => return Action::Redraw,
                // Ctrl+G: hand the buffer off to $EDITOR (only when idle).
                KeyCode::Char('g') if !self.running => return Action::OpenEditor,
                // Ctrl+D on an empty input quits (shell-style EOF) — checked
                // before the vim Normal-mode scroll arm below so it fires even
                // in Normal mode, matching the welcome banner's advertised
                // "Ctrl+D on an empty line" behavior. `.trim()` (not just
                // `.is_empty()`) because the vim engine's `content()` always
                // carries a trailing newline, even on a freshly-opened,
                // never-typed-in buffer.
                KeyCode::Char('d') if self.editor.content().trim().is_empty() => {
                    self.request_quit();
                    return Action::None;
                }
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
                self.request_quit();
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
            // `handle_slash` returned false: not a recognized command. If the
            // input still *looks* like an attempted slash command — a single
            // leading `/word` token, command-name-shaped (letters/digits/
            // hyphens only, no further `/` or `.`) — a typo (`/exprot`) would
            // otherwise become a full model turn instead of an error. A real
            // path-like message (`/etc/hosts looks wrong`) falls outside this
            // shape (it has another `/` or a `.`) and is sent as usual.
            if let Some(first) = input.split_whitespace().next()
                && first.len() > 1
                && first.starts_with('/')
                && first[1..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !is_known_command(first)
            {
                self.editor.set_content("");
                self.scroll_offset = 0;
                self.system(format!(
                    "unknown command: {first} (see /help — or drop the leading '/' to send it as a message)"
                ));
                return Action::None;
            }
            self.editor.set_content("");
            self.scroll_offset = 0; // auto-follow on new submission
            if self.running {
                // A turn is in flight. The message is never injected mid-stream:
                // it waits as a pending block, and `Agent::run` picks it up
                // before its next request — which only happens after a round's
                // tool results, so the model reads them together. If the model
                // instead ends the turn, nothing drains it and `Done` re-sends it
                // as a turn of its own.
                self.queue.push_back(input.clone());
                let sent = hrdr_app::prepare_outgoing_via(&self.agent, &input);
                if let Ok(mut q) = self.steering.lock() {
                    q.push_back(sent);
                }
            } else if self.compacting {
                // Summarizing, not in `run()` — nothing is draining steering.
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
        // The `/model` selector owns the mouse while open: the wheel scrolls its
        // list (moving the highlight, which the view follows); other events are
        // swallowed so they don't reach the transcript beneath the modal.
        if let Some(sel) = &mut self.model_selector {
            match m.kind {
                MouseEventKind::ScrollUp => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.up()),
                MouseEventKind::ScrollDown => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.down()),
                _ => {}
            }
            return;
        }
        // The `/resume` and `/theme` pickers get the same treatment (the theme
        // picker also live-previews the newly-highlighted row).
        if let Some(sel) = &mut self.session_selector {
            match m.kind {
                MouseEventKind::ScrollUp => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.up()),
                MouseEventKind::ScrollDown => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.down()),
                _ => {}
            }
            return;
        }
        if let Some(sel) = &mut self.theme_selector {
            match m.kind {
                MouseEventKind::ScrollUp => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.up()),
                MouseEventKind::ScrollDown => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.down()),
                _ => {}
            }
            self.preview_selected_theme();
            return;
        }
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
                // Click a sub-agent panel row to jump to the `task` tool call it
                // came from — the panel lists agents; the transcript holds their
                // output. A row without a call context has nothing to jump to.
                if let Some(hit) = self
                    .subagent_hits
                    .iter()
                    .find(|(r, _)| r.contains(m.column, m.row))
                    .map(|(_, id)| id.clone())
                {
                    if let Some(idx) = hit.and_then(|id| self.tool_entry_index(&id)) {
                        self.pending_focus_entry = Some(idx);
                    }
                    return;
                }
                // Click a tool block to toggle its full output (per-entry /expand).
                let hit = self
                    .tool_hits
                    .iter()
                    .find(|(r, _)| r.contains(m.column, m.row))
                    .map(|(_, i)| *i);
                if let Some(idx) = hit
                    && let Some(EntryKind::Tool { expanded, .. }) =
                        self.state.transcript.get_mut(idx).map(|e| &mut e.kind)
                {
                    *expanded = !*expanded;
                    // The block's height just changed; keep its top where the
                    // reader is looking instead of letting it slide.
                    self.pending_scroll_entry = Some(idx);
                }
            }
            _ => {}
        }
    }

    /// Index of the transcript entry for tool call `id`, if it is still there.
    /// A sub-agent panel row jumps to it; the call may have been cleared by
    /// `/clear` or scrolled out of a compacted transcript, hence the `Option`.
    pub(crate) fn tool_entry_index(&self, id: &str) -> Option<usize> {
        self.state
            .transcript
            .iter()
            .position(|e| matches!(&e.kind, EntryKind::Tool { id: tid, .. } if tid == id))
    }

    /// Whether the input pane should render masked (every char hidden) —
    /// while the `/login` wizard is waiting for the actual API key. The real
    /// value stays in the editor buffer untouched (`/login` reads it via
    /// `self.editor.content()` as usual); only the on-screen rendering
    /// changes, so the key isn't fully visible on screen as it's typed.
    pub(crate) fn masks_input(&self) -> bool {
        self.login.as_ref().is_some_and(|w| w.wants_secret_input())
    }

    /// Show a transient status line: a command's output, a usage hint, a busy
    /// guard, a reload notice. These are chrome — regenerated on demand and
    /// never persisted (see [`hrdr_app::EntryKind::Notice`]).
    ///
    /// Content that belongs to the conversation's history — a turn's error, a
    /// cancel, a compaction result, an agent warning — pushes `Entry::system`
    /// directly instead.
    pub(crate) fn system(&mut self, msg: impl Into<String>) {
        self.push_entry(Entry::notice(msg.into()));
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

    /// Append a transcript entry. Each entry carries its own timestamp, set when
    /// it was constructed.
    fn push_entry(&mut self, e: Entry) {
        self.state.transcript.push(e);
        self.prune_scrollback();
    }

    /// Evict oldest entries from the transcript front when the scrollback cap
    /// is exceeded. The window of intro entries (the header banner + the
    /// welcome/config/project-docs notices — see `App::new`) is always kept
    /// so the user never loses the intro banner.
    fn prune_scrollback(&mut self) {
        if self.state.transcript.len() <= self.scrollback {
            return;
        }
        // Count leading `Header`/`Notice` entries: they form the intro block
        // (`Entry::header()` + one or more `Entry::notice(...)`s) and should
        // never be evicted. Everything else past them is fair game.
        //
        // Regression: this counted leading `EntryKind::System` entries, but
        // the intro is Header + Notice — so `head` was always 0 and the
        // welcome banner was the very first thing evicted.
        let head = self
            .state
            .transcript
            .iter()
            .take_while(|e| matches!(e.kind, EntryKind::Header | EntryKind::Notice(_)))
            .count();
        let excess = self.state.transcript.len().saturating_sub(self.scrollback);
        // Ensure we always keep at least `head` entries.
        let remove = excess.min(self.state.transcript.len().saturating_sub(head));
        if remove == 0 {
            return;
        }
        // Drop the oldest non-head entries.
        let keep_start = head.saturating_add(remove).min(self.state.transcript.len());
        self.state.transcript.drain(head..keep_start);
        // Prune the render cache: any key with an entry_idx that has shifted
        // is stale.  Easiest way: clear the whole thread-local transcript cache
        // once (cheap — it rebuilds lazily on the next frame).
        crate::ui::clear_transcript_cache();
    }

    /// Clear the transcript.
    fn clear_transcript(&mut self) {
        self.state.transcript.clear();
        crate::ui::clear_transcript_cache();
    }

    /// Truncate the transcript to `len`.
    fn truncate_transcript(&mut self, len: usize) {
        self.state.transcript.truncate(len);
        crate::ui::clear_transcript_cache();
    }

    /// Age out finished TODO items. Called once per turn (on `Done`, so it also
    /// runs when a turn errors).
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
        self.auto_compact_enabled = cfg.auto_compact;
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
                    hrdr_app::RELOAD_MANUAL_MSG.to_string()
                } else {
                    hrdr_app::reload_hot_message()
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
            // Keep the aborted handle so the quit path can await it (releasing
            // the agent lock) before the final save. In the stay-in-app cancel
            // case it's simply reaped by the next quit or overwritten by the
            // next turn — harmless either way.
            self.quit_reap = Some(handle);
        }
        self.running = false;
        self.pending_init = false;
        self.compacting = false;
        self.pause_inference();
        self.tools_running = 0;
        let dropped = self.queue.len();
        self.queue.clear();
        // Undelivered steering would otherwise leak into the next turn.
        if let Ok(mut q) = self.steering.lock() {
            q.clear();
        }
        self.push_entry(Entry::system(hrdr_app::cancel_message(dropped)));
        // The turn never reached `Done`, so nothing has autosaved the visible
        // user message + whatever partial reply streamed in before the
        // cancel. Persist it now — the same best-effort save every other
        // checkpoint uses (skips if the agent lock is still busy; a later
        // save, or the one on quit, catches up).
        self.autosave();
    }

    /// Quit the session. If a turn is running, cancel it first — which
    /// autosaves the in-progress transcript — so quitting mid-turn (Ctrl+Q,
    /// double Ctrl+C, Ctrl+D on empty input, `/exit`) never drops the visible
    /// user message or a partial reply.
    fn request_quit(&mut self) {
        if self.running {
            self.cancel_turn();
        }
        self.should_quit = true;
    }

    /// Await a turn task aborted on the quit path so its future is dropped and
    /// the agent lock released, making the subsequent final autosave's
    /// `try_lock` reliably succeed. A no-op when nothing was cancelled; awaiting
    /// an already-terminated handle returns immediately.
    pub(crate) async fn reap_cancelled_turn(&mut self) {
        if let Some(handle) = self.quit_reap.take() {
            let _ = handle.await;
        }
    }

    fn spawn_turn(&mut self, input: String) {
        // Commit the message into history at send time (a queued message lives
        // as a pending bottom item until this point).
        self.push_entry(Entry::user(input.clone()));
        // Prepare the outgoing message: expand `@file` mentions and route any
        // `@agent` mention to the matching sub-agent via a delegation directive.
        let sent = hrdr_app::prepare_outgoing_via(&self.agent, &input);
        self.launch_turn(sent);
    }

    /// Run a turn against the model with `input` as the (already-prepared) user
    /// message. The caller is responsible for any transcript display.
    fn launch_turn(&mut self, input: String) {
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.turn_started_at = Some(chrono::Local::now());
        self.first_token_at = None;
        self.reasoning_start = None;
        self.out_tokens = 0;
        self.tools_running = 0;
        self.infer_banked = Duration::ZERO;
        self.infer_started = None;
        self.resume_inference();
        // Keep last_usage so the status-bar context size persists between turns;
        // it's refreshed when this turn's Usage event arrives.
        let agent = self.agent.clone();
        let steering = self.steering.clone();
        let tx = self.tx.clone();
        let tx_events = tx.clone();
        let handle = tokio::spawn(async move {
            // Release the agent lock before signalling Done, so the UI's
            // auto-save (try_lock) can run immediately afterward.
            let result = {
                let mut a = agent.lock().await;
                a.run(input, steering, |ev| {
                    let _ = tx_events.send(TurnMsg::Event(ev));
                })
                .await
            };
            let _ = tx.send(TurnMsg::Done(result.err().map(|e| e.to_string())));
        });
        self.turn_handle = Some(handle);
    }

    /// Connect the configured MCP servers (once, at startup), showing a status
    /// line per server. Their tools join the set the model is offered.
    pub(crate) async fn connect_mcp(&mut self) {
        let notices = self.agent.lock().await.connect_mcp().await;
        for n in notices {
            self.push_entry(Entry::system(n));
        }
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
                self.state.usage.last().map(|(p, _)| p),
                self.state.usage.context_window,
                self.compaction_reserved,
                self.auto_compact_enabled,
            )
    }

    /// Run a compaction pass on the background task, reporting via `TurnMsg`.
    fn spawn_compaction(&mut self, instructions: Option<String>) {
        self.running = true;
        self.compacting = true;
        self.turn_started = Some(Instant::now());
        self.turn_started_at = Some(chrono::Local::now());
        self.first_token_at = None;
        self.reasoning_start = None;
        self.out_tokens = 0;
        // Summarizing is the model working: its own clock, no tools.
        self.tools_running = 0;
        self.infer_banked = Duration::ZERO;
        self.infer_started = None;
        self.resume_inference();
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
                self.push_entry(Entry::notice(text));
                // Do NOT reset scroll_offset here: this is an async/passive line
                // (e.g. a late `/models` result). Resetting would yank the user's
                // view when they are scrolled up reading back-scroll. When the
                // user is already following (offset == 0), it stays 0 unchanged.
            }
            TurnMsg::Diff(text) => {
                self.push_entry(Entry::diff(text));
                // Same rationale as TurnMsg::System above: passive async output.
            }
            TurnMsg::Done(err) => {
                if !self.running {
                    // Stale Done from an aborted task; discard.
                    return;
                }
                self.turn_handle = None;
                self.running = false;
                self.pause_inference();
                self.tools_running = 0;
                // The turn is over — clear any sub-agents still in the live panel
                // (an interrupted turn may not have delivered their ToolEnd).
                self.subagent_panel.clear();
                if let Some(e) = err {
                    self.push_entry(Entry::system(format!("[error] {e}")));
                }
                // Append the final stats for the turn (before stats are reset by
                // any queued turn that spawns next).
                if let Some(stats) = self.turn_stats() {
                    self.push_entry(Entry::stats(stats));
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
                    self.push_entry(Entry::notice(
                        "context near the limit — auto-compacting…".to_string(),
                    ));
                    self.spawn_compaction(None);
                    return;
                }
                // The turn ended without draining what was queued (the model
                // answered instead of calling a tool). Drop the agent's prepared
                // copies — `spawn_turn` re-prepares — and send the oldest as a
                // turn of its own. The rest wait for that turn to finish.
                if let Ok(mut q) = self.steering.lock() {
                    q.clear();
                }
                if let Some(next) = self.queue.pop_front() {
                    self.spawn_turn(next);
                }
            }
            TurnMsg::FileIndex(cwd, files) => {
                self.file_index = files;
                self.file_index_cwd = Some(cwd);
                self.file_index_building = false;
            }
            TurnMsg::ContextWindow(tokens) => {
                // A model/provider switch re-probed the endpoint; honor the new
                // advertised max (drives "X of Y" + the auto-compaction trigger).
                self.state.usage.context_window = Some(tokens);
            }
            TurnMsg::ConfigChanged => self.maybe_reload_config(),
            TurnMsg::Compacted(res) => {
                self.turn_handle = None;
                self.running = false;
                self.compacting = false;
                self.pause_inference();
                // Context shrank; drop stale usage so the status bar refreshes
                // on the next turn (and we don't immediately re-trigger).
                self.state.usage.set_last(None);
                self.last_cached_tokens = None;
                self.last_reasoning_tokens = None;
                self.push_entry(Entry::system(hrdr_app::compaction_message(&res)));
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
            // The model's working time, excluding the tool calls it waited on.
            self.infer_elapsed().as_secs_f64(),
            self.first_token_at
                .map(|t0| t0.duration_since(started).as_secs_f64()),
            self.out_tokens,
            self.state.usage.last(),
            self.last_cached_tokens,
            self.last_reasoning_tokens,
        )
    }

    /// The model went idle: bank the inference time and hide the loader. Called
    /// when the first tool of a round starts, and when a turn ends.
    fn pause_inference(&mut self) {
        if let Some(t) = self.infer_started.take() {
            self.infer_banked += t.elapsed();
        }
        self.inferring = false;
    }

    /// The model is working again: the turn just began, or its last tool call
    /// returned and the agent is about to request the next response.
    fn resume_inference(&mut self) {
        self.infer_started.get_or_insert_with(Instant::now);
        self.inferring = true;
    }

    /// How long the model has actually worked this turn: banked stretches plus
    /// the one in progress. Excludes time spent waiting on tool calls.
    pub(crate) fn infer_elapsed(&self) -> Duration {
        self.infer_banked
            + self
                .infer_started
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO)
    }

    /// A detached sub-agent finished while nothing was running: wake the model so
    /// it reacts to the result instead of sitting on it until the user's next
    /// message.
    ///
    /// `Agent::run` folds finished background tasks into the conversation before
    /// each request, so an empty turn is enough to deliver them — it pushes no
    /// user message of its own. Only fires when idle: a running turn already
    /// drains them at its next request, and a compaction is about to.
    pub(crate) fn maybe_deliver_background(&mut self) {
        if self.running || self.compacting {
            return;
        }
        let ready = self
            .background_tasks
            .lock()
            .map(|v| v.iter().any(|t| t.done && !t.delivered))
            .unwrap_or(false);
        if ready {
            self.launch_turn(String::new());
        }
    }

    /// Messages handed to the running turn but not yet delivered.
    #[cfg(test)]
    pub(crate) fn steering_len_for_test(&self) -> usize {
        self.steering.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Start the inference clock from a test, without spawning a real turn.
    #[cfg(test)]
    pub(crate) fn resume_inference_for_test(&mut self) {
        self.resume_inference();
    }

    /// Count a streamed delta toward the live tok/s stats.
    fn count_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.out_tokens += 1;
    }

    /// Record how long the last reasoning block took, when thinking ends. The
    /// renderer turns it into the block's `Thought: 1.2s` label — it is never
    /// spliced into the entry's text.
    fn finish_reasoning(&mut self) {
        let Some(start) = self.reasoning_start.take() else {
            return;
        };
        let elapsed = start.elapsed().as_millis() as u64;
        if let Some(EntryKind::Reasoning { took_ms, .. }) =
            self.state.transcript.last_mut().map(|e| &mut e.kind)
        {
            *took_ms = Some(elapsed);
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        // Stamp the elapsed thinking time on the last reasoning block when
        // thinking ends (the next event after Reasoning is something else).
        let end_reasoning = !matches!(ev, AgentEvent::Reasoning(_));
        if end_reasoning {
            self.finish_reasoning();
        }
        match ev {
            AgentEvent::Text(t) => {
                self.count_token();
                match self.state.transcript.last_mut().map(|e| &mut e.kind) {
                    Some(EntryKind::Assistant(s)) => s.push_str(&t),
                    // Don't open an assistant entry for an empty delta — a turn
                    // that only calls tools would leave an empty one behind.
                    _ if t.is_empty() => {}
                    _ => self.push_entry(Entry::assistant(t)),
                }
            }
            AgentEvent::Reasoning(t) => {
                self.count_token();
                if self.reasoning_start.is_none() {
                    self.reasoning_start = Some(Instant::now());
                }
                match self.state.transcript.last_mut().map(|e| &mut e.kind) {
                    Some(EntryKind::Reasoning {
                        text,
                        took_ms: None,
                    }) => text.push_str(&t),
                    _ => self.push_entry(Entry::reasoning(t)),
                }
            }
            AgentEvent::History(messages) => self.persist_mid_turn(messages),
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                cached_prompt_tokens,
                reasoning_tokens,
                session_cost_usd,
                ..
            } => {
                self.state
                    .usage
                    .record_call(prompt_tokens, completion_tokens);
                if let Some(total) = session_cost_usd {
                    // The agent's counter covers this process (incl. its
                    // sub-agents); a resumed session's saved spend is the base.
                    self.state.usage.cost_usd = self.cost_base + total;
                }
                self.last_cached_tokens = cached_prompt_tokens;
                self.last_reasoning_tokens = reasoning_tokens;
            }
            AgentEvent::ToolStart { id, name, args } => {
                // The model has handed off: it is idle until every tool of this
                // round returns. Stop its clock and hide the loader.
                self.tools_running += 1;
                if self.tools_running == 1 {
                    self.pause_inference();
                }
                // A `task` call opens a live entry in the sub-agent panel.
                if name == "task" {
                    self.subagent_panel.on_tool_start(id.clone());
                }
                self.push_entry(Entry::tool_running(id.clone(), name.clone(), args));
            }
            AgentEvent::ToolOutput { id, chunk } => {
                // Append live output to the running tool's entry.
                for entry in self.state.transcript.iter_mut().rev() {
                    if let EntryKind::Tool {
                        id: tid,
                        result: r,
                        done,
                        ..
                    } = &mut entry.kind
                        && *tid == id
                        && !*done
                    {
                        r.push_str(&chunk);
                        break;
                    }
                }
                // Mirror into the sub-agent panel's live log (the full stream,
                // which the transcript discards at ToolEnd).
                self.subagent_panel.on_tool_output(&id, &chunk);
            }
            AgentEvent::ToolEnd {
                id,
                result,
                ok,
                name: _,
            } => {
                for entry in self.state.transcript.iter_mut().rev() {
                    if let EntryKind::Tool {
                        id: tid,
                        result: r,
                        ok: o,
                        done,
                        ..
                    } = &mut entry.kind
                        && *tid == id
                        && !*done
                    {
                        *r = result;
                        *o = ok;
                        *done = true;
                        break;
                    }
                }
                // The sub-agent finished — its result is now in the transcript;
                // drop it from the live panel.
                self.subagent_panel.on_tool_end(&id);
                // The last tool of the round returned: the agent is about to ask
                // the model again, so it is working from here.
                self.tools_running = self.tools_running.saturating_sub(1);
                if self.tools_running == 0 && self.running {
                    self.resume_inference();
                }
            }
            AgentEvent::Notice(text) => {
                self.push_entry(Entry::system(text));
                // Do NOT reset scroll_offset: notices (MCP warnings, health
                // alerts, step-budget exhaustion) are passive async lines. When
                // the user is scrolled up, the new line appends without moving
                // their view. When already following (offset == 0) it stays 0.
            }
            AgentEvent::Steered(sent) => {
                // It just entered the conversation, after the tool results of the
                // round that carried it. Display it at *this* point so the
                // transcript's order matches the model's view — and show what the
                // user typed, not its `@file`-expanded form.
                let shown = self.queue.pop_front().unwrap_or(sent);
                self.push_entry(Entry::user(shown));
            }
            AgentEvent::TurnDone => {}
        }
    }
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::HitRect;

    /// The TUI's TODO-panel default lifetime must track the shared UI-config
    /// default (the aging logic itself is tested in `hrdr-app`).
    #[test]
    fn ttl_matches_config_default() {
        assert_eq!(5, hrdr_app::DEFAULT_TODO_TTL);
    }

    // ---- HitRect hit-test (transcript tool-block click targeting) ----

    /// `HitRect::contains` is the sole gate for all mouse hit-testing in the
    /// TUI (tool-block expansion, sub-agent panel rows, the follow button).
    /// Verify the boundary arithmetic is correct in all four directions.
    #[test]
    fn hitrect_contains_boundary() {
        // Rectangle occupying columns 10–29, rows 5–7 (w=20, h=3).
        let r = HitRect {
            x: 10,
            y: 5,
            w: 20,
            h: 3,
        };

        // Corners and a centre cell must be inside.
        assert!(r.contains(10, 5), "top-left corner should be inside");
        assert!(
            r.contains(29, 7),
            "bottom-right corner (x+w-1, y+h-1) should be inside"
        );
        assert!(r.contains(20, 6), "centre cell should be inside");

        // Each boundary's immediate outside must be rejected.
        assert!(!r.contains(9, 5), "one col left of rect should be outside");
        assert!(!r.contains(30, 5), "x+w (exclusive) should be outside");
        assert!(!r.contains(10, 4), "one row above rect should be outside");
        assert!(!r.contains(10, 8), "y+h (exclusive) should be outside");
    }

    /// A zero-size HitRect never contains anything.
    #[test]
    fn hitrect_zero_size_never_contains() {
        let r = HitRect {
            x: 5,
            y: 5,
            w: 0,
            h: 0,
        };
        assert!(
            !r.contains(5, 5),
            "zero-size rect must never contain any cell"
        );
        assert!(!r.contains(0, 0));
    }
}
