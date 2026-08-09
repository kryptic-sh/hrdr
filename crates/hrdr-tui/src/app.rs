//! App state, the async event loop, and agent orchestration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use hjkl_clipboard::Clipboard;
use hjkl_holler::HollerBus;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Todo};
use hrdr_editor::{PlainEngine, TuiEditorEngine, VimEngine};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;

/// How long a `!command` may run: a day, which is to say "until the user stops
/// it".
///
/// The model's tools take `DEFAULT_TOOL_TIMEOUT_SECS` because a stuck tool call
/// hangs a turn. This is the user's own shell — the one thing in the app that
/// answers to nobody — so the only bound here is the one that stops a forgotten
/// process outliving the session.
const USER_SHELL_TIMEOUT_SECS: u64 = 60 * 60 * 24;

use crate::theme::Theme;

mod commands;
mod completion;
use completion::CompletionKind;
mod selector;
mod session;
mod util;

pub(crate) use completion::Completions;
use hrdr_app::config_mtime as current_config_mtime;
use hrdr_app::{display_dir, git_branch, is_known_command, is_quit_command};
pub(crate) use selector::{
    CommandSelector, EffortSelector, LoginProviderSelector, ModelSelector, Selector,
    SessionSelector, ThemeSelector, command_selector, effort_selector, login_provider_selector,
    model_selector, session_selector, theme_selector,
};
// Re-exported so the `tui` driver module (which owns the event loop + terminal)
// can reach these terminal-facing helpers.
pub(crate) use util::run_editor;

/// A running user `!command`: enough to cancel it (abort the task — the
/// child is `kill_on_drop`) and close its transcript block coherently.
pub(crate) struct UserShell {
    /// Tool-block id, to mark the entry cancelled.
    id: String,
    /// Tool name shown on the block (always "shell").
    name: String,
    /// The command, for the model's history note on cancel.
    command: String,
    /// The streaming task; aborting it kills the child process.
    handle: tokio::task::JoinHandle<()>,
}

/// A slash command's data output (`/status`, `/cost`, `/help`, …), shown in
/// an Esc-dismissible popup rather than the transcript. Scrolls with Up/Down
/// when the text is taller than the popup.
pub(crate) struct NoticePopup {
    /// The command's output, as-is (the popup renders it as plain text).
    pub(crate) text: String,
    /// Scroll offset from the top, in rows.
    pub(crate) scroll: u16,
}

impl NoticePopup {
    pub(crate) fn new(text: String) -> Self {
        Self { text, scroll: 0 }
    }
}

/// The `/login` modal's two phases: pick a provider from a fuzzy list, then —
/// for a remote key-based provider — enter the API key in a masked field.
/// OAuth and keyless providers finish straight from the first phase.
pub(crate) enum LoginModal {
    Providers(LoginProviderSelector),
    Key {
        /// Provider name the key belongs to.
        name: String,
        /// Friendly label for the modal title.
        label: String,
        /// The plaintext-storage warning shown above the field.
        warning: String,
        /// The key as typed/pasted (rendered masked).
        input: String,
    },
    /// A browser OAuth login is in flight. Esc / `/cancel` abandons it (a late
    /// result is ignored by `login_id` mismatch); `Switching` cannot be
    /// interrupted.
    Authorizing {
        /// Rejects a stale/duplicate login's late [`TurnMsg::BrowserLogin`].
        login_id: u64,
        /// The provider being authorized (`chatgpt` / `openrouter`).
        provider: String,
        /// Friendly label for the modal title.
        label: String,
    },
    /// The credential is saved and the live provider switch is running — the
    /// final transaction, deliberately NOT cancellable.
    Switching {
        label: String,
    },
}

// The display-mode enums live in the shared `hrdr-app` core so every frontend
// resolve/persist these settings identically.
pub(crate) use hrdr_app::StatusBarMode;

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

impl From<ratatui::layout::Rect> for HitRect {
    fn from(r: ratatui::layout::Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            w: r.width,
            h: r.height,
        }
    }
}

impl HitRect {
    /// Whether the cell at `(col, row)` is inside this rectangle.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }

    /// `(col, row)` pulled inside this rectangle — a drag that leaves the
    /// transcript still selects up to its edge.
    pub fn clamp(&self, col: u16, row: u16) -> (u16, u16) {
        (
            col.clamp(self.x, (self.x + self.w).saturating_sub(1)),
            row.clamp(self.y, (self.y + self.h).saturating_sub(1)),
        )
    }
}

/// An ordered selection: its `(start, end)` screen cells, top-left first.
pub(crate) type SelectionSpan = ((u16, u16), (u16, u16));

/// A mouse drag across the transcript, in screen cells. The text under it is
/// harvested from the rendered frame — what is on screen is what is copied — so
/// the selection is held as screen coordinates, not transcript offsets, and dies
/// the moment the frame beneath it can no longer be trusted (a key, a scroll).
#[derive(Clone, Copy)]
pub(crate) struct MouseSelection {
    /// Where the button went down, and where the pointer is now. Either may be
    /// the earlier of the two on screen — [`App::selection_span`] orders them.
    anchor: (u16, u16),
    head: (u16, u16),
    /// The pane the drag started in: its rect decides where the head clamps and
    /// which horizontal band [`paint_selection`](crate::ui::paint_selection)
    /// reads interior rows over.
    area: SelectionArea,
    /// The button is still down: the head still follows the pointer.
    dragging: bool,
    /// The pointer left the anchor cell, which is what tells a selection from a
    /// plain click (the click the transcript's blocks answer to).
    moved: bool,
}

impl MouseSelection {
    /// The pane this selection is anchored in.
    pub(crate) fn area(&self) -> SelectionArea {
        self.area
    }
}

/// Which pane a mouse selection is anchored in. Select-to-copy works over the
/// transcript, the input pane and the status bar alike; each area's rect bounds
/// the drag and is the band its copied rows are read over.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionArea {
    Transcript,
    Input,
    Status,
}

// The transcript item model + its representation-independent queries
// (search/count/export) live in the shared `hrdr-app` core.
pub(crate) use hrdr_app::{Entry, EntryKind};

/// Messages from the background agent task back to the UI loop.
pub(crate) enum TurnMsg {
    Event(AgentEvent),
    /// A user-initiated `!command` shell event. Separate from [`TurnMsg::Event`]
    /// so it bypasses the "ignore buffered events after cancellation" guard —
    /// these aren't turn events and arrive while no turn is running. The
    /// `ToolEnd` carries the history note (command + bounded output) so the
    /// UI loop can commit it through the same plumbing as a finished turn.
    UserShell(AgentEvent, Option<String>),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
    /// An off-thread session save finished: `Ok(path)` once the snapshot is
    /// atomically on disk, or the error. Sent by the save task spawned by
    /// [`App::enqueue_save`]; the coalescer's in-flight flag clears here.
    SaveDone(Result<String, String>),
    /// An event from a sub-agent being driven directly from its pane. Carried per
    /// pane key so it lands in that agent's transcript and nowhere else.
    SubAgent(u64, AgentEvent),
    /// Out-of-band system line (e.g. async `/models` result).
    System(String),
    /// A slash command's data output (`/status`, `/cost`, `/help`, …) from a
    /// spawned line task — shown in an Esc-dismissible popup.
    Popup(String),
    /// Out-of-band diff block (e.g. async `/diff` result).
    Diff(String),
    /// Compaction finished: `Ok((before, after))` message counts, or an error.
    /// Carries the pane whose agent was summarized — `/compact` acts on the
    /// conversation being viewed, so its clock and its stale context reading are
    /// that agent's, not the session's.
    Compacted(
        hrdr_app::PaneId,
        Result<hrdr_agent::CompactionReport, String>,
    ),
    /// A model/provider switch re-probed the endpoint's advertised context window.
    /// Carries the pane whose agent was switched: `/model` acts on the agent being
    /// viewed, so its probe result belongs to that agent and not to the session's.
    ContextWindow(hrdr_app::PaneId, u32),
    /// A `/model` switch was ACCEPTED by the agent: adopt the identity it actually
    /// took — and the endpoint/window that moved with it — onto that pane's chrome.
    /// Sent by the switch task, never by the keystroke: settling a switch can need a
    /// network round-trip (confirming a ChatGPT entitlement), and a switch that is
    /// then refused must leave the status bar where the agent stayed.
    Identity(
        hrdr_app::PaneId,
        hrdr_agent::ModelRef,
        Option<String>,
        Option<u32>,
    ),
    /// A browser OAuth login's exchange/save step finished. Carries the typed
    /// outcome (with its originating `login_id`) so the loop can reject a stale
    /// login and, on a match, run the live provider switch.
    BrowserLogin(hrdr_app::BrowserLoginOutcome),
    /// An async ChatGPT catalog load finished. Carries the generation it was
    /// spawned at (a stale generation is dropped), the entitled rows, the source,
    /// and an optional warning.
    ModelCatalog {
        generation: u64,
        models: Vec<hrdr_agent::ChatGptModel>,
        source: hrdr_agent::CatalogSource,
        warning: Option<String>,
    },
    /// `@file` completion index built off-thread for `cwd` — each entry is
    /// `(path, lowercase_path)`, the lowercase half precomputed by the builder.
    FileIndex(std::path::PathBuf, Vec<(String, String)>),
    /// The watched cwd changed on disk (entries created/renamed/removed —
    /// external edits, a `git pull`, the agent's own writes). The `@file`
    /// index is stale; the frontend invalidates it.
    FileIndexDirty,
    /// The config file changed on disk (from the shared watcher).
    ConfigChanged,
}

/// Capacity of the UI event channel ([`App::tx`] → the render loop).
///
/// The render loop drains the *entire* channel on every wake (a `recv` then a
/// `try_recv` loop, see `tui::run_loop`), so in steady state it sits near
/// empty. A bound is still needed for the window where the consumer is *not*
/// draining — a slow `terminal.draw`, or the seconds/minutes the loop is
/// suspended inside `$EDITOR` — while a fast local model streams tokens. The
/// old unbounded channel let that window queue without limit. The cap leaves
/// generous headroom for the control events (tool start/end, notices, usage)
/// that accrue during such a stall — the token deltas themselves are coalesced
/// by [`EventSender`] into O(1) messages — while capping worst-case memory.
/// Mirrors the agent's own `UI_STREAM_CAP` house value.
const TUI_EVENT_CAP: usize = 1024;

/// Coalescing, bounded sink for a turn's [`AgentEvent`] stream.
///
/// The agent turn drives events through a **synchronous** `FnMut` callback, so
/// it cannot `await` backpressure inline. Feeding a fast token stream straight
/// into a bounded channel with `try_send` would force a choice between blocking
/// (impossible here) and dropping — and dropping a `ToolEnd`/error/state event
/// is not acceptable. This sink resolves that by keeping a FIFO backlog outside
/// the channel and draining it opportunistically:
///
/// * Adjacent streaming deltas of the same kind (`Text`, `Reasoning`, or
///   `ToolOutput` for one call id) **coalesce** into the backlog's tail message
///   before they ever reach the channel. This is exactly what the consumer
///   would render anyway — every delta is a `push_str` (see
///   `hrdr_app::apply_event`) — so it holds the token stream at O(1) queued
///   messages regardless of arrival rate.
/// * Every other event is **never dropped or merged**: it waits in the backlog,
///   in order, until the channel accepts it.
///
/// [`EventSender::drain`] flushes any backlog still outstanding when a turn ends
/// during a stall, applying real (async) backpressure at the turn boundary. A
/// closed receiver (the app shutting down) turns every send into a no-op error
/// rather than a block, so a producer awaiting capacity can never deadlock.
struct EventSender {
    tx: mpsc::Sender<TurnMsg>,
    backlog: std::collections::VecDeque<TurnMsg>,
}

impl EventSender {
    fn new(tx: mpsc::Sender<TurnMsg>) -> Self {
        Self {
            tx,
            backlog: std::collections::VecDeque::new(),
        }
    }

    /// Enqueue one event — coalescing streaming deltas into the backlog tail —
    /// then push as much of the backlog into the channel as it will accept.
    fn send(&mut self, ev: AgentEvent) {
        if !self.coalesce_into_tail(&ev) {
            self.backlog.push_back(TurnMsg::Event(ev));
        }
        self.pump();
    }

    /// If `ev` is a streaming delta whose kind (and, for tool output, call id)
    /// matches the current backlog tail, append its text there and report
    /// `true`. Only the tail is eligible: any earlier message may have an
    /// order-sensitive event queued behind it.
    fn coalesce_into_tail(&mut self, ev: &AgentEvent) -> bool {
        let Some(TurnMsg::Event(tail)) = self.backlog.back_mut() else {
            return false;
        };
        match (tail, ev) {
            (AgentEvent::Text(acc), AgentEvent::Text(delta)) => {
                acc.push_str(delta);
                true
            }
            (AgentEvent::Reasoning(acc), AgentEvent::Reasoning(delta)) => {
                acc.push_str(delta);
                true
            }
            (
                AgentEvent::ToolOutput {
                    id: acc_id,
                    chunk: acc,
                },
                AgentEvent::ToolOutput {
                    id: delta_id,
                    chunk: delta,
                },
            ) if *acc_id == *delta_id => {
                acc.push_str(delta);
                true
            }
            _ => false,
        }
    }

    /// Push backlog messages into the channel until it is full or the backlog is
    /// empty. Non-blocking: a full channel leaves the remainder queued for the
    /// next call; a closed channel discards it (the receiver is gone).
    fn pump(&mut self) {
        while let Some(msg) = self.backlog.pop_front() {
            if let Err(e) = self.tx.try_send(msg) {
                match e {
                    mpsc::error::TrySendError::Full(msg) => {
                        self.backlog.push_front(msg);
                        break;
                    }
                    mpsc::error::TrySendError::Closed(_) => {
                        self.backlog.clear();
                        break;
                    }
                }
            }
        }
    }

    /// Flush any remaining backlog, awaiting channel capacity. Called once a turn
    /// ends: if the stall that backed the queue up outlived the turn, this is
    /// where the tail events finally land. A closed receiver ends the drain at
    /// once instead of blocking.
    #[cfg(test)]
    async fn drain(mut self) {
        while let Some(msg) = self.backlog.pop_front() {
            if self.tx.send(msg).await.is_err() {
                break;
            }
        }
    }

    /// Take everything still queued, so a caller can await those sends without
    /// holding the lock this sink is shared behind — the turn's event hook and its
    /// completion hook are separate closures over the same sink.
    fn take_backlog(&mut self) -> Vec<TurnMsg> {
        self.backlog.drain(..).collect()
    }
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    /// Every agent this session can show: the main one, plus each retained
    /// sub-agent. The main agent's transcript lives in its pane — `state`'s copy
    /// is a serialization shape, refreshed from the pane at save time.
    pub(crate) panes: hrdr_app::PaneSet,
    /// The agent's live sub-agent registry — the source the pane list is
    /// reconciled against, and where a pane's steering queue and `Agent` come from.
    pub(crate) registry: hrdr_agent::AgentRegistry,
    /// Shared cell for the sub-agent transcript dir, handed to the agent config
    /// and refreshed whenever the session id is assigned (see
    /// [`Self::refresh_subagent_dir`]).
    subagent_dir: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    /// A session was created by [`Self::reserve_session_id`] at turn start, so
    /// the first autosave still owes the user the "session saved" notice. The
    /// reservation stays silent on purpose: announcing it there would print the
    /// notice ahead of the reply rather than after the turn, where it belongs.
    session_notice_pending: bool,
    /// Last autosave error shown in the transcript. Identical failures stay
    /// silent until a save succeeds, preventing every checkpoint from spamming.
    session_save_error: Option<String>,
    /// The coalescer's waiting snapshot: a save is in flight, and this newer
    /// state is what the next save task writes (latest-wins — a later save
    /// always supersedes an in-flight one). Captured on the UI thread at
    /// enqueue time, so a `/rename` or `/clear` after that point cannot
    /// interleave with a stale write. `Some` implies [`Self::save_in_flight`].
    pending_save: Option<hrdr_app::SessionState>,
    /// Whether a session-save task is currently writing (serialize + atomic
    /// fs write, both off the UI thread).
    save_in_flight: bool,
    /// Wakes the quit flush ([`Self::await_saves`]) when a save task posts its
    /// `SaveDone`. The save writes the file BEFORE notifying, so waking is the
    /// durability signal.
    save_done: Arc<tokio::sync::Notify>,
    /// The open-lock for the session currently loaded, if any. Held for the whole
    /// time this session is active so a second hrdr instance can't resume the same
    /// session and silently clobber our turns (last-writer-wins). Set when a
    /// session is minted (first save), resumed (picker / `/resume`), or
    /// auto-resumed; dropped — releasing the lock — on `/new` and on exit.
    active_lock: Option<hrdr_app::SessionLock>,
    /// The id-reservation of a session minted by [`Self::reserve_session_id`]
    /// whose first write has not landed yet. Held here (not in the save task)
    /// so a `/clear`/`/resume` that discards the pending write drops it too —
    /// its drop removes the `.id.lock` a failed first write would otherwise
    /// leave behind. Taken into the save task by [`Self::enqueue_save`].
    pending_reservation: Option<hrdr_agent::Reservation>,
    pub(crate) editor: Box<dyn TuiEditorEngine>,
    /// Resolved chat-UI colors (from an hjkl theme).
    pub(crate) theme: Theme,
    /// ASCII art the session header animates, owned by the caller of
    /// [`crate::run`] — the TUI embeds no logo of its own.
    pub(crate) logo: &'static str,
    /// Persistent clock anchor for the header's logo animation. Captured once:
    /// re-anchoring per frame would pin the animation's tick at 0.
    pub(crate) header_anchor: Instant,
    /// Per-message timestamp style: none / relative / exact (`/timestamps`).
    /// Status-bar mode: none / truncate / wrap (`/statusbar`).
    pub(crate) statusbar_mode: StatusBarMode,
    // ---- status bar info ----
    /// Working directory, home-shortened for display.
    pub(crate) dir: String,
    /// Current git branch, if the cwd is in a repo.
    pub(crate) branch: Option<String>,
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
    /// When true, `active_completions()` returns `None` so Up/Down keys
    /// fall through to history navigation instead of being intercepted
    /// by the completion popup. Set by history recall, cleared on typing.
    suppress_completions: bool,
    /// Submitted-input history + Up/Down browsing (from the shared core).
    pub(crate) history: hrdr_app::HistoryBrowser,
    /// Cached relative file paths under the cwd, for `@file` completion. Each
    /// entry is `(path, lowercase_path)` — the lowercase half is built once by
    /// `hrdr_app::spawn_file_index`, so ranking never re-lowercases the index.
    file_index: Vec<(String, String)>,
    /// The cwd `file_index` was built for; rebuilt when the cwd changes or a
    /// filesystem change lands in the watched tree.
    file_index_cwd: Option<std::path::PathBuf>,
    /// An off-thread index build is in flight (don't spawn another).
    file_index_building: bool,
    /// A filesystem change landed while a build was in flight, so the build
    /// result about to land is already stale. Keeps the cache invalidated
    /// until the next build starts (which clears it).
    file_index_dirty: bool,
    /// Recursive watcher on the cwd; its events (create/rename/remove —
    /// anything but a read) arrive as [`TurnMsg::FileIndexDirty`] and
    /// invalidate the `@file` index. `None` when no watcher could be
    /// established — completion then refreshes only on cwd changes.
    file_watcher: Option<notify::RecommendedWatcher>,
    /// Show every tool result in full (`/verbose on`); per-entry `expanded`
    /// overrides this for individual results. `verbose` also renders the
    /// model's reasoning blocks in full — there is no separate thinking
    /// toggle.
    pub(crate) verbose: bool,
    /// A file `/edit` requested to open in `$EDITOR`, consumed by the run loop.
    pending_edit: Option<std::path::PathBuf>,
    /// The open `/model` selector modal; while `Some`, it captures every key.
    pub(crate) model_selector: Option<ModelSelector>,
    /// Authoritative monotonic generation for async model-catalog loads. Owned
    /// by `App` (not the selector) so it survives the picker's open/close: a
    /// catalog result is applied only when its captured snapshot still equals
    /// this. Bumped on every selector close/cancel and provider/session change.
    pub(crate) model_gen: u64,
    /// Whether an async ChatGPT catalog load is in flight for the open picker.
    pub(crate) model_loading: bool,
    /// Provenance of the rows currently shown (fresh / stale / built-in
    /// fallback), rendered separately from the startup guidance.
    pub(crate) model_source: Option<hrdr_agent::CatalogSource>,
    /// The open `/resume` session picker modal; while `Some`, it captures every key.
    pub(crate) session_selector: Option<SessionSelector>,
    /// Memoized rendered rows for the open `/resume` picker, keyed by its
    /// filter + modal width — the picker repaints every frame and the rows only
    /// move when the filter does. Cleared when the picker (re)opens, so a fresh
    /// `list_sessions` is never hidden behind the previous open's rows.
    pub(crate) session_rows: Option<crate::ui::SessionRows>,
    /// The open `/theme` picker modal; while `Some`, it captures every key and
    /// live-previews the highlighted theme.
    pub(crate) theme_selector: Option<ThemeSelector>,
    /// The theme in force when the `/theme` picker opened — restored on Esc
    /// (and while no row matches its filter).
    pub(crate) theme_original: Option<Theme>,
    /// The open `/effort` picker modal; while `Some`, it captures every key.
    pub(crate) effort_selector: Option<EffortSelector>,
    /// The open `/commands` picker modal; while `Some`, it captures every key.
    pub(crate) command_selector: Option<CommandSelector>,
    /// The open `/login` modal (provider list, then masked key entry); while
    /// `Some`, it captures every key (and pasted text, for the key field).
    pub(crate) login_modal: Option<LoginModal>,
    /// Monotonic id for browser logins — bumped per launch so a stale/duplicate
    /// login's late result is rejected by [`LoginModal::Authorizing`].
    pub(crate) next_login_id: u64,
    /// The in-flight browser-login task, so cancelling authorization can
    /// `abort()` it — which drops its callback listener (freeing the localhost
    /// port for a retry) and prevents an abandoned flow from still saving tokens.
    pub(crate) browser_login_task: Option<tokio::task::JoinHandle<()>>,
    /// The running user `!command`, if any — Esc cancels it.
    pub(crate) user_shell: Option<UserShell>,
    /// Discovered `:command` prompt templates for the current cwd, for the
    /// completion popup (refreshed on cwd change and `/reload`; the send path
    /// re-discovers on its own, so a stale list only affects completion).
    pub(crate) commands: Vec<hrdr_app::Command>,
    /// Discovered `SKILL.md` bundles for the current cwd — the other half of the
    /// `:name` namespace, refreshed alongside `commands`. The invalid ones ride
    /// along because the `/commands` picker shows them with their reason.
    pub(crate) skills: hrdr_app::DiscoveredSkills,
    /// Whether this session may read project-scoped commands and skills at all
    /// — the session agent's own [`hrdr_agent::Agent::project_instructions`],
    /// which every discovery this frontend runs has to be given (completion
    /// popup, `/commands` picker, and the `:name` expansion on submit).
    ///
    /// Read once at construction and kept: the agent derives it from its
    /// sandbox mode and never changes it, and the send path cannot take the
    /// agent's lock — a running turn holds it, which is exactly when a steer is
    /// typed.
    pub(crate) project_instructions: hrdr_agent::ProjectInstructions,
    /// A `/goto` target message number, resolved to a scroll offset at draw.
    pub(crate) pending_goto: Option<usize>,
    /// A transcript index whose block should be pulled to the top of the
    /// viewport at the next draw. Set when a tool block is expanded or
    /// collapsed: the row count changes under the reader, and `scroll_offset` is
    /// measured from the bottom, so the block would otherwise jump.
    pub(crate) pending_scroll_entry: Option<usize>,
    /// The screen row `pending_scroll_entry` was on when clicked. The next draw
    /// keeps that chunk's top on this row (see `draw_chunks`), so expanding or
    /// collapsing a section holds the viewport steady on the line that was
    /// under the cursor instead of yanking the entry to the top.
    pub(crate) pending_scroll_row: Option<u16>,
    /// A transcript index to pull to the top of the viewport at the next draw,
    /// scrolling there if the reader is following the newest output. Set by a
    /// click on a sub-agent panel row: unlike `pending_scroll_entry`, which only
    /// holds a block still while its height changes, this one *moves* the view.
    /// Last `/find` query (also drives transcript highlighting) and the message
    /// number it last landed on (for cycling).
    pub(crate) find: hrdr_app::FindState,
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
    /// The running turn's steering queue. `Agent::run` drains it before each
    /// request — i.e. right after a round's tool results — so a queued message
    /// rides in with them. Empty when no turn is running.
    steering: hrdr_agent::SteeringQueue,
    /// A `/compact` typed while the agent was busy: the pane it was requested
    /// for and the summary-steering message, run once the turn ends. Deliberately
    /// NOT a steer — a steer reaches the model mid-turn; a compaction must wait
    /// for the turn to finish so the summary sees the whole conversation.
    pending_compaction: Option<(hrdr_app::PaneId, Option<String>)>,
    /// A slash command's data output (`/status`, `/cost`, `/help`, …) shown
    /// in an Esc-dismissible popup. `None` when nothing is open.
    pub(crate) popup: Option<NoticePopup>,
    /// Screen rect of the "↓ Press END ↓" button (jump to newest output), set
    /// during draw while scrolled up so mouse clicks can hit-test against it.
    /// `None` when following.
    pub(crate) end_button: Option<HitRect>,
    /// Screen rect of the "↑ Press HOME ↑" button (jump to the top of the
    /// session), the sibling of [`end_button`](Self::end_button). `None`
    /// when following.
    pub(crate) home_button: Option<HitRect>,
    /// Clickable screen rects for each visible tool block → the transcript
    /// entry it toggles, set during draw. A click on a tool GROUP's summary
    /// toggles the whole group; a click on a standalone block (or a hidden
    /// thought's summary) toggles that entry. The group chunk's own calls and
    /// gaps are row-level targets, handled via [`Self::row_hits`].
    pub(crate) tool_hits: Vec<(HitRect, usize)>,
    /// Tool groups the reader opened (keyed by the head tool-call id). While a
    /// group is open its calls render as child items inside its summary; a
    /// click on the summary toggles it. Session-lifetime view state — never
    /// persisted.
    pub(crate) tool_groups: std::collections::HashSet<String>,
    /// Individual calls inside an expanded group that the reader opened in
    /// full (keyed by the tool-call id): every other call shows its preview
    /// (tail/head) until clicked. `verbose` shows every call in full at once.
    pub(crate) tool_open: std::collections::HashSet<String>,
    /// Thinking entries the reader opened while reasoning is hidden: a
    /// collapsed `✓ Thought for 1m 32s · 2m ago` summary shows its full block
    /// instead. Keyed by the entry's transcript index — not its content hash,
    /// which changes as a streaming thought's text grows and would silently
    /// fold the open thought back on the next token; indices are stable for a
    /// session (nothing truncates the transcript). `verbose` (via `/verbose on`)
    /// shows every thought at once.
    pub(crate) thinking_open: std::collections::HashSet<usize>,
    /// Live blocking `task` sub-agents in the sub-agent panel, updated by the
    /// event-fold methods as `ToolStart`/`ToolOutput`/`ToolEnd` events arrive.
    /// Shared registry of *detached background* sub-agents (a clone of the
    /// agent's `ctx.background_tasks`), read live for the panel.
    pub(crate) background_tasks: Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>>,
    /// Clickable screen rects for the live panels that close the transcript (the
    /// agent switcher's rows, the TODO list's "finished" row) → what a click on
    /// each one does. They scroll with the transcript, so the frame recomputes
    /// them every draw, exactly like [`Self::tool_hits`].
    pub(crate) row_hits: Vec<(HitRect, crate::ui::RowHit)>,
    /// Screen rect of the transcript, set during draw: the region a drag may
    /// select from, and the frame the selected text is read back out of.
    pub(crate) transcript_rect: HitRect,
    /// Screen rect of the input pane, set during draw: mouse select-to-copy
    /// works there too, not just over the transcript.
    pub(crate) input_rect: HitRect,
    /// Screen rect of the status bar block, set during draw: likewise selectable.
    pub(crate) status_rect: HitRect,
    /// The live mouse selection, if any.
    pub(crate) selection: Option<MouseSelection>,
    /// Set when the button comes up on a real drag: the next frame — the one
    /// that has the rendered cells — harvests the selected text and copies it.
    pub(crate) pending_copy: bool,
    /// Toast notifications (copy/paste feedback), drawn over the top-right of
    /// the screen and dismissed by their own TTLs.
    pub(crate) toasts: HollerBus,
    /// Whether the TODO panel is showing the tasks that are over (completed,
    /// cancelled) as well as the ones still to do. Off by default — the panel is
    /// about what is left — and toggled by clicking its "finished" row.
    pub(crate) show_done_todos: bool,
    /// Set after one idle Ctrl+C; a second consecutive Ctrl+C quits. Any other
    /// key (or a mouse action) disarms it.
    pub(crate) quit_armed: bool,
    /// Drafts put aside with Ctrl+S, newest last — an empty-box Ctrl+S pops the
    /// last one back into the editor. Session-lifetime only, like the editor
    /// buffer itself.
    pub(crate) stash: Vec<String>,
    /// Set after one Esc pressed against something in flight; a second
    /// consecutive Esc interrupts it. Any other key (or a mouse action) disarms
    /// it, so a stray Esc can't kill a long turn.
    pub(crate) cancel_armed: bool,
    /// A `/resume` that hit a session held open by another live instance armed an
    /// offer to open a forked copy instead; carries the busy session's `(id,
    /// path)`. The next key answers: `f`/`y` forks, any other key cancels. Like
    /// [`Self::quit_armed`], a lightweight armed flag consumed by the next key.
    pub(crate) pending_fork: Option<(String, std::path::PathBuf)>,
    // ---- live inference stats (for the loader above the input) ----
    /// When the current thinking block started (for the "Thought:" footer).
    tx: mpsc::Sender<TurnMsg>,
    pub(crate) rx: Option<mpsc::Receiver<TurnMsg>>,
    pub(crate) should_quit: bool,
    /// Set by a turn task that *caught* a tool panic: the process-global panic
    /// hook already tore the terminal down (left the alt screen, dropped raw
    /// mode) before `catch_unwind` recovered, so the driver must re-enter it
    /// before the next frame. The driver clears the flag once it has restored.
    terminal_lost: Arc<AtomicBool>,
}

impl App {
    pub(crate) fn new(
        mut config: AgentConfig,
        ui: hrdr_app::UiConfig,
        logo: &'static str,
    ) -> Result<Self> {
        let identity = config.model.clone();
        // Compute the env-sourced-key warning while `config` is still whole
        // (it is consumed later building the agent); pushed into the transcript
        // below alongside the other startup notices.
        let env_auth_warning = {
            let prov = config.model.provider().as_str().to_string();
            config.resolve_provider(&prov).and_then(|p| {
                hrdr_agent::api_key_env_source(&p).map(|var| {
                    format!(
                        "⚠ using the API key from ${var} (environment) for '{prov}' — \
                         this overrides any /login credential"
                    )
                })
            })
        };
        let vim_mode = ui.vim_mode;
        let theme = Theme::load(ui.theme.as_deref());
        let dir = display_dir(&config.cwd);
        let branch = git_branch(&config.cwd);
        let cwd_for_commands = config.cwd.clone();
        let context_window = config.context_window;
        let auto_resume = ui.auto_resume;
        let bell = ui.bell;
        let todo_ttl = ui.todo_ttl;
        let scrollback = ui.scrollback;
        let statusbar_mode = StatusBarMode::from_config(ui.statusbar.as_deref());
        // No portable terminal-font probe, so an unset/`auto` icons setting
        // resolves to Nerd glyphs.
        let icon_mode = ui
            .icons
            .as_deref()
            .and_then(hjkl_icons::IconMode::from_config)
            .unwrap_or(hjkl_icons::IconMode::Nerd);
        let base_url = config.base_url.clone();
        // Shared transcript-dir cell: handed to the agent (so the `task` tool
        // can persist sub-agent runs) and kept here to repoint at the session's
        // dir once an id is assigned (`refresh_subagent_dir`).
        let subagent_dir = Arc::new(std::sync::Mutex::new(None));
        config.child_transcript_dir = Some(subagent_dir.clone());
        // The user's TODO-lifetime preference lives in the UI config, but the
        // ageing itself is the agent's — hand the preference over.
        config.todo_ttl = todo_ttl;
        let cfg = config.clone();
        let agent = Agent::new(config)?;
        let todos = agent.todos();
        let registry = agent.registry();
        let background_tasks = agent.background_tasks();
        let project_docs_loaded = agent.project_docs().is_some();
        // The agent has already answered "may this directory's files steer this
        // session" (it is jailed, or it is not). Take that answer rather than
        // asking the trust store a second time: two derivations of one rule are
        // two answers waiting to disagree, and the frontend's is the one the
        // user types `:name` into.
        let project_instructions = agent.project_instructions();
        let (tx, rx) = mpsc::channel(TUI_EVENT_CAP);
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
             to quit. Submit while a reply runs to queue follow-ups; Up on an empty box \
             takes the last one back to edit."
        };
        // The banner opens every new session; the welcome text follows it.
        // Both are chrome: a resumed session gets a fresh pair, not the saved one.
        let mut transcript = vec![Entry::header(), Entry::notice(welcome)];
        // Warn (but don't fail) if the config file exists but is invalid — the
        // running config has already fallen back to defaults + env in that case.
        if let Some(warning) = hrdr_app::startup_config_warning() {
            transcript.push(Entry::notice(warning));
        }
        // Surface when the API key hrdr will use is coming from an environment
        // variable (rather than `/login` or config): a stray `OPENAI_API_KEY`
        // silently overriding a stored credential should be visible. Computed
        // early (see `env_auth_warning` above) so `config` is still un-moved.
        if let Some(warning) = env_auth_warning {
            transcript.push(Entry::notice(warning));
        }
        if project_docs_loaded {
            transcript.push(Entry::notice(hrdr_app::PROJECT_DOCS_LOADED_MSG));
        }
        // The main agent's state *is* its pane's state — its model, its endpoint,
        // its counters and its transcript, held exactly the way a sub-agent's are.
        // The opening chrome (banner + welcome) is seeded straight into it.
        let state = hrdr_app::SessionState {
            model: identity,
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
            subagent_dir,
            registry,
            panes: {
                let mut panes = hrdr_app::PaneSet::new();
                panes.main_mut().state = state;
                panes
            },
            session_notice_pending: false,
            session_save_error: None,
            pending_save: None,
            save_in_flight: false,
            save_done: Arc::new(tokio::sync::Notify::new()),
            active_lock: None,
            pending_reservation: None,
            editor,
            theme,
            logo,
            header_anchor: Instant::now(),
            statusbar_mode,
            dir,
            branch,
            icon_mode,
            cfg,
            config_mtime: current_config_mtime(),
            clipboard: Clipboard::new().ok(),
            completion_idx: 0,
            suppress_completions: false,
            history: hrdr_app::HistoryBrowser::load(),
            file_index: Vec::new(),
            file_index_cwd: None,
            file_index_building: false,
            file_index_dirty: false,
            file_watcher: None,
            verbose: false,
            pending_edit: None,
            model_selector: None,
            model_gen: 0,
            model_loading: false,
            model_source: None,
            session_selector: None,
            session_rows: None,
            theme_selector: None,
            theme_original: None,
            effort_selector: None,
            command_selector: None,
            login_modal: None,
            next_login_id: 0,
            browser_login_task: None,
            user_shell: None,
            commands: hrdr_app::discover_commands(&cwd_for_commands, project_instructions),
            skills: hrdr_app::discover_skills(&cwd_for_commands, project_instructions),
            project_instructions,
            pending_goto: None,
            pending_scroll_entry: None,
            pending_scroll_row: None,
            find: hrdr_app::FindState::default(),
            bell,
            turn_handle: None,
            quit_reap: None,
            scroll_offset: 0,
            home_button: None,
            transcript_height: 24,
            scrollback,
            max_scroll: 0,
            todos,
            todo_turn: 0,
            todo_completed_at: HashMap::new(),
            todo_ttl,
            steering: hrdr_agent::steering_queue(),
            pending_compaction: None,
            popup: None,
            end_button: None,
            tool_hits: Vec::new(),
            tool_groups: std::collections::HashSet::new(),
            tool_open: std::collections::HashSet::new(),
            thinking_open: std::collections::HashSet::new(),
            transcript_rect: HitRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            input_rect: HitRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            status_rect: HitRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            selection: None,
            pending_copy: false,
            toasts: HollerBus::new(),
            background_tasks,
            row_hits: Vec::new(),
            show_done_todos: false,
            quit_armed: false,
            cancel_armed: false,
            stash: Vec::new(),
            pending_fork: None,
            tx,
            rx: Some(rx),
            should_quit: false,
            terminal_lost: Arc::new(AtomicBool::new(false)),
        };
        // The session's agent joins the registry alongside every delegated one, so
        // the frontend can build its view the same way for all of them.
        app.publish_main_agent();
        // Watch the working tree so a change to it (a new file, a `git pull`)
        // invalidates the `@file` completion index instead of leaving the cached
        // snapshot stale forever. Re-armed on every cwd change.
        app.arm_file_watcher(&cwd_for_commands);
        if auto_resume {
            app.auto_resume_latest();
        }
        Ok(app)
    }

    /// Put the session's agent in the registry, and keep its chrome there in step
    /// with the pane's state.
    ///
    /// The registry is what a pane is built from — for the main agent as much as a
    /// delegated one — so a `/model` switch, a resume, or a `/clear` has to land
    /// there, or the next frame would quietly restore the old values.
    pub(crate) fn publish_main_agent(&mut self) {
        let (reference, base_url, usage) = {
            let s = self.state();
            (s.model.clone(), s.base_url.clone(), s.usage)
        };
        // The live registry still carries the identity as two values (it is shared
        // with the agent side, which has its own reasons); it is taken apart here, at
        // the edge, and nowhere else.
        self.registry.register_session(
            self.agent.clone(),
            self.steering.clone(),
            reference.model().to_string(),
            Some(reference.provider().to_string()),
            base_url,
            usage,
        );
        // The *counters* are the frontend's to seed (a resumed session carries them,
        // a `/clear` resets them). What the agent is **running on** — model,
        // provider, endpoint — is not: the agent publishes that itself, from what it
        // is actually pointed at (`Agent::attach_live`). A copy kept here is a copy
        // that can be wrong, and one that was: a resumed session's provider label
        // reached the status bar while the agent kept talking to the endpoint it
        // launched with.
        self.registry
            .update(hrdr_agent::MAIN_KEY, |e| e.usage = usage);
        // Adopt the entry (idempotent) so every later change republishes into it.
        let agent = self.agent.clone();
        let live = self.registry.clone();
        if let Ok(mut a) = agent.try_lock() {
            a.attach_live(live, hrdr_agent::MAIN_KEY);
        } else {
            tokio::spawn(async move {
                agent.lock().await.attach_live(live, hrdr_agent::MAIN_KEY);
            });
        }
    }

    /// Probe the endpoint (list its models) on a background task and post a
    /// warning if it's unreachable or doesn't advertise the configured model.
    /// Stays silent on success so it doesn't clutter the transcript.
    pub(crate) fn spawn_health_check(&self) {
        let agent = self.agent.clone();
        let model = self.state().model.model().to_string();
        let base_url = self.state().base_url.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(warning) = hrdr_app::endpoint_health_warning(agent, model, base_url).await {
                let _ = tx.send(TurnMsg::System(warning)).await;
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
        if self.state().usage.context_window.is_some() {
            return;
        }
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let window = agent.lock().await.probe_context_window().await;
            if let Some(w) = window {
                // The startup probe is the *session* agent's, whatever is on screen.
                let _ = tx
                    .send(TurnMsg::ContextWindow(hrdr_app::PaneId::MAIN, w))
                    .await;
            }
        });
    }

    /// Fire the `session_start` lifecycle hooks on a background task; any
    /// failures surface as system lines. A no-op without configured hooks.
    pub(crate) fn spawn_session_start_hooks(&self) {
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let notes = agent
                .lock()
                .await
                .run_session_hooks(hrdr_tools::HookEvent::SessionStart)
                .await;
            for note in notes {
                let _ = tx.send(TurnMsg::System(note)).await;
            }
        });
    }

    /// Run the `session_end` lifecycle hooks on the quit path. Awaited — the
    /// process is about to exit, so a spawned task would be killed mid-hook;
    /// each hook's own timeout bounds the wait. Their output has nowhere to
    /// go (the terminal is being restored), so notes are dropped.
    pub(crate) async fn run_session_end_hooks(&self) {
        // The quit path reaped any turn first, so the lock should be free; if
        // something still holds it, skipping beats hanging the exit.
        if let Ok(a) = self.agent.try_lock() {
            let _ = a.run_session_hooks(hrdr_tools::HookEvent::SessionEnd).await;
        }
    }

    /// Start the shared config-file watch, piping change pings into the UI
    /// loop (dedup happens in [`Self::maybe_reload_config`]'s mtime guard).
    /// The returned guard must be kept alive for the watch to stay active.
    pub(crate) fn start_config_watch(&self) -> hrdr_app::ConfigWatcherGuard {
        let tx = self.tx.clone();
        hrdr_app::watch_config(move || {
            // Sync watcher callback — can't await. A dropped ping (channel
            // momentarily full) is harmless: the mtime guard in
            // `maybe_reload_config` re-checks on the next wake anyway.
            let _ = tx.try_send(TurnMsg::ConfigChanged);
        })
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }

        // Each double-press confirmation survives only its own key: any other
        // key breaks the sequence and disarms it.
        let is_ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if !is_ctrl_c {
            self.quit_armed = false;
        }
        if key.code != KeyCode::Esc {
            self.cancel_armed = false;
        }
        // A mouse selection is anchored to cells of the frame it was drawn on;
        // typing is about to redraw them.
        self.selection = None;

        // A `/resume` refused a session held open elsewhere and armed an offer to
        // open a forked copy: `f`/`y` forks, any other key cancels (and is then
        // handled normally — we don't swallow unrelated input). Mirrors the
        // `quit_armed` armed-flag pattern.
        if self.pending_fork.is_some() {
            let confirm = key.modifiers.is_empty()
                && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('y'));
            if confirm {
                let (id, path) = self.pending_fork.take().expect("just checked is_some");
                self.fork_session(id, &path);
                return Action::None;
            }
            self.pending_fork = None;
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
        if self.effort_selector.is_some() {
            self.effort_selector_key(key);
            return Action::None;
        }
        if self.command_selector.is_some() {
            self.command_selector_key(key);
            return Action::None;
        }
        if self.login_modal.is_some() {
            self.login_modal_key(key);
            return Action::None;
        }

        // The notice popup (a slash command's data output) captures keys while
        // it is open: Esc (or Ctrl+C) dismisses, Up/Down scroll.
        if self.popup.is_some() {
            self.popup_key(key);
            return Action::None;
        }

        // Completion popup (slash command, `@` mention, argument): Tab and Enter
        // both accept the selection into the input; Up/Down move it. Neither
        // submits — accepting the completion and sending the message are two
        // distinct presses, so Enter here just fills the box and the *next* Enter
        // (with nothing left to accept) sends it. Tab keeps the popup live so a
        // command's argument completions can follow; Enter suppresses it so the
        // next Enter submits — typing anything clears that and completions return.
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
                    let idx = self.completion_idx.min(last);
                    // A mention is part of composing a message, and a suggestion
                    // the user has not yet typed in full is something to accept —
                    // in both cases Enter fills the input and does NOT submit; the
                    // next Enter (popup now suppressed) sends it. Only a command or
                    // argument the user already typed in full has nothing left to
                    // accept, so Enter there submits in one press, as before.
                    let accept_only = matches!(comp.kind, CompletionKind::Mention { .. })
                        || !self.completion_is_exact(&comp, idx);
                    if accept_only {
                        self.apply_completion(&comp, idx, true);
                        self.completion_idx = 0;
                        self.suppress_completions = true;
                        return Action::None;
                    }
                }
                _ => {}
            }
        }

        // Ctrl+C / Ctrl+Q / Ctrl+G, plus vim-mode scroll.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Ctrl+C, most-local-first: clear a non-empty input box, else
                // interrupt what is in flight, else arm quit — and a second
                // consecutive Ctrl+C on that armed, idle, empty state quits.
                KeyCode::Char('c') => {
                    if !self.editor.content().trim().is_empty() {
                        self.editor.set_content("");
                        self.quit_armed = false;
                    } else if self.cancel_in_flight() {
                        self.quit_armed = false;
                    } else if self.quit_armed {
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
                // Ctrl+S puts the draft aside and takes it back: a non-empty box
                // is pushed onto the stash and cleared, an empty one pops the
                // newest stash back. A stack, so several drafts can wait at once
                // (last stashed is first back). Same `trim()` reasoning as the
                // Ctrl+D arm below.
                KeyCode::Char('s') => {
                    let draft = self.editor.content();
                    if draft.trim().is_empty() {
                        if let Some(stashed) = self.stash.pop() {
                            self.editor.set_content(&stashed);
                        }
                    } else {
                        self.stash.push(draft);
                        self.editor.set_content("");
                    }
                    return Action::None;
                }
                // Ctrl+] pastes the clipboard into the input, for terminals
                // (and remote sessions) where the terminal's own paste doesn't
                // reach the app as a bracketed paste.
                KeyCode::Char(']') => {
                    match hrdr_app::clipboard_read_text(&self.clipboard) {
                        Some(text) if !text.is_empty() => {
                            let chars = text.chars().count();
                            self.on_paste(&text);
                            self.toasts.info(format!("pasted {chars} chars"));
                        }
                        Some(_) => {
                            self.toasts.warn("clipboard is empty");
                        }
                        None => {
                            self.toasts.warn("clipboard unavailable");
                        }
                    }
                    return Action::None;
                }
                // Ctrl+G: hand the buffer off to $EDITOR (only when idle).
                KeyCode::Char('g') if !self.running() => return Action::OpenEditor,
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

        // Esc interrupts what is in flight — a turn or a user `!command` — but
        // only on a second consecutive press, so a stray Esc can't kill a long
        // turn (vim: only in Normal, so Esc still exits Insert; plain: always,
        // since Esc is otherwise unused).
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.editor.mode_label() != "INSERT"
            && self.in_flight()
        {
            if std::mem::take(&mut self.cancel_armed) {
                self.cancel_in_flight();
            } else {
                self.cancel_armed = true;
            }
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
                // Up on an EMPTY box takes back a message still waiting to be
                // said, before it falls through to history — see
                // [`Self::take_queued_into_input`].
                KeyCode::Up if self.editor.content().trim().is_empty() => {
                    if !self.take_queued_into_input() {
                        self.history_prev();
                    }
                    return Action::None;
                }
                // Up/Down recall previous submissions (readline-style), always —
                // even for multi-line entries, so the arrows never get stuck
                // moving the cursor inside a recalled multi-line item.
                KeyCode::Up => {
                    self.history_prev();
                    return Action::None;
                }
                KeyCode::Down => {
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
            return self.submit_input(input);
        }

        self.suppress_completions = false;
        self.editor.feed_key(ekey);
        Action::None
    }

    /// Act on one line of input — the single path everything the user can *say* to
    /// hrdr goes down, whichever way they said it.
    ///
    /// A `/name` slash command, a `:name` prompt command, a `!shell` escape, a quit
    /// word, or a message for the model: the rules for telling them apart, and the
    /// routing that follows,
    /// live here and nowhere else. `Enter` in the input box is one caller; a
    /// command handed to hrdr on the command line (`hrdr /new`) is another, and it
    /// gets exactly the behaviour typing it would.
    pub(crate) fn submit_input(&mut self, input: String) -> Action {
        {
            if input.trim().is_empty() {
                return Action::None;
            }
            self.record_history(&input);
            // Common quit commands exit the session instead of being sent.
            if is_quit_command(input.trim()) {
                self.request_quit();
                return Action::None;
            }
            // `!command` — the user-initiated shell escape: run it directly
            // (bash/PowerShell), stream the output into a transcript tool
            // block, and record command + output into the model's history.
            // `:!command` is the same escape under the ex-style prefix —
            // vim muscle memory means the shell, never a command named `!`.
            let trimmed = input.trim();
            if let Some(cmd) = trimmed
                .strip_prefix('!')
                .or_else(|| trimmed.strip_prefix(":!"))
            {
                let cmd = cmd.trim().to_string();
                self.editor.set_content("");
                self.scroll_offset = 0;
                if cmd.is_empty() {
                    self.system("usage: !<shell command>  (e.g. !git status)".to_string());
                } else {
                    self.user_shell_command(cmd);
                }
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
            // The input box talks to whichever agent you are looking at. On a
            // sub-agent pane that means *that* sub-agent — steered if a turn is in
            // flight, a fresh turn on the retained agent if it is idle.
            if !self.panes.active().is_main() {
                self.send_to_subagent(self.panes.active().key(), input);
                return Action::None;
            }
            if self.running() || self.compacting() {
                // Busy. The message is never injected mid-stream: it waits on the
                // agent's own queue, and `Agent::run` picks it up before its next
                // request — which only happens after a round's tool results, so the
                // model reads them together. If the model ends the turn instead,
                // nothing drains it and `Done` re-sends it as a turn of its own.
                // (While compacting, nothing is in `run()` to drain it at all.)
                let sent =
                    hrdr_app::prepare_outgoing_via(&self.agent, &input, self.project_instructions);
                self.registry
                    .enqueue(hrdr_agent::MAIN_KEY, hrdr_agent::Steer::new(sent, input));
            } else {
                self.spawn_turn(input);
            }
            Action::None
        }
    }

    /// Run a user-typed `!command`: spawn the shell in the agent's cwd,
    /// stream its output through the normal tool-event pipeline (so it renders
    /// as a live tool block), and, when it finishes, commit the command +
    /// (bounded) output to the model's history and autosave — the same
    /// end-of-work plumbing a turn gets (see [`Self::finish_user_shell`]).
    /// User-initiated, so hrdr's shell guardrails don't apply — this is the
    /// user's own shell. Rejected while a turn is running: its tool blocks
    /// would interleave with the model's.
    pub(crate) fn user_shell_command(&mut self, command: String) {
        if self.running() {
            self.system(
                "a turn is running — wait for it (or interrupt with Esc Esc) before running                  !commands"
                    .to_string(),
            );
            return;
        }
        // Reject a second `!command` before anything is minted or recorded: a
        // refused command must leave no id, no session reservation, and no
        // tool block in the transcript.
        if self
            .user_shell
            .as_ref()
            .is_some_and(|u| !u.handle.is_finished())
        {
            self.system(
                "a !command is already running — wait for it (or cancel with Esc Esc)".to_string(),
            );
            return;
        }
        let Some(shell) = hrdr_tools::Shell::detect() else {
            self.system(
                "no shell found — !commands need bash or a POSIX shell on PATH (on Windows, \
                 use WSL or Git Bash)"
                    .to_string(),
            );
            return;
        };
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "user-shell-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // Mint the session id — and with it attach the main transcript writer —
        // before the tool block opens, so a `!command` run as the very first
        // action in a brand-new session still records to the durable jsonl. The
        // same reservation a normal turn makes in `spawn_turn`; a no-op once the
        // session already has an id. Without it, the ToolStart/ToolEnd below fire
        // through `record(MAIN_KEY)` while no writer is attached, and the block is
        // absent from the transcript on resume.
        self.reserve_session_id(&format!("! {command}"));
        // Open the tool block immediately (synchronously, so it lands before
        // any streamed output). It renders as the `shell` tool.
        self.record_local(AgentEvent::ToolStart {
            id: id.clone(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": command}).to_string(),
        });
        let task_id = id.clone();
        let cwd = hrdr_app::agent_cwd(&self.agent);
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<String>(256);
        let mut ctx = hrdr_tools::ToolContext::new(&cwd);
        ctx.stream = Some(stream_tx);
        ctx.max_output = 50_000;
        // Raised in step with `max_output`. The default is 50 lines, which is a
        // sensible read for the *model* and absurd for a person who just typed
        // `!git log`: the block would settle to 50 lines and a spool pointer.
        ctx.max_output_lines = 5_000;
        ctx.guardrails = Arc::new(Vec::new()); // user's own shell — no guardrails
        let tx = self.tx.clone();
        let task_command = command.clone();
        // The user's own shell is not on a leash. The model's tools time out
        // because a stuck tool call hangs a turn; `!command` is the user sitting
        // at their own prompt, and `!tail -f` or `!npm run dev` must not be
        // killed at five minutes. Ctrl+C is how this one ends.
        let timeout = std::time::Duration::from_secs(USER_SHELL_TIMEOUT_SECS);
        let handle = tokio::spawn(async move {
            // Forward live output to the TUI as ToolOutput events.
            let fwd_tx = tx.clone();
            let fwd_id = task_id.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(chunk) = stream_rx.recv().await {
                    let _ = fwd_tx
                        .send(TurnMsg::UserShell(
                            AgentEvent::ToolOutput {
                                id: fwd_id.clone(),
                                chunk,
                            },
                            None,
                        ))
                        .await;
                }
            });
            let finished =
                hrdr_tools::run_user_command(shell, &task_command, timeout, true, &ctx).await;
            // Close the stream and let the forwarder drain before `ToolEnd`.
            // Both tasks push onto the same channel, so without this the settle
            // can overtake output still in flight and land in a closed block.
            drop(ctx);
            let _ = forwarder.await;
            match finished {
                Ok(run) => {
                    let exit = run
                        .exit_code
                        .map_or_else(|| "?".to_string(), |c| c.to_string());
                    // Bound what lands in the transcript result + history (the
                    // live stream already showed everything).
                    //
                    // `truncate`, not `truncate_inline`: the latter is for
                    // one-line previews and replaces every newline with a
                    // space, which turned a `!git log` or `!seq 1 500` into a
                    // single wrapped line in the settled block — and handed the
                    // model the same flattened blob inside a fence.
                    let bounded = hrdr_tools::truncate(&run.output, 50_000);
                    let note = format!(
                        "I ran `{task_command}` in the shell (exit {exit}). Output:\n\n```\n{}\n```",
                        bounded.trim_end()
                    );
                    let result = if bounded.trim().is_empty() {
                        format!("(no output — exit {exit})")
                    } else {
                        bounded
                    };
                    let _ = tx
                        .send(TurnMsg::UserShell(
                            AgentEvent::ToolEnd {
                                id: task_id,
                                name: "shell".to_string(),
                                result,
                                ok: run.passed,
                            },
                            Some(note),
                        ))
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(TurnMsg::UserShell(
                            AgentEvent::ToolEnd {
                                id: task_id,
                                name: "shell".to_string(),
                                result: format!("(error: {e})"),
                                ok: false,
                            },
                            None,
                        ))
                        .await;
                }
            }
        });
        self.user_shell = Some(UserShell {
            id,
            name: "shell".to_string(),
            command,
            handle,
        });
    }

    /// Cancel the running `!command`: abort its task (killing the child via
    /// `kill_on_drop`), close the transcript block as cancelled, and leave a
    /// history note so the model knows the command didn't finish.
    pub(crate) fn cancel_user_shell(&mut self) {
        let Some(shell) = self.user_shell.take() else {
            return;
        };
        if shell.handle.is_finished() {
            return; // it completed; the ToolEnd event already closed the block
        }
        shell.handle.abort();
        self.record_local(AgentEvent::ToolEnd {
            id: shell.id,
            name: shell.name,
            result: "(cancelled)".to_string(),
            ok: false,
        });
        let note = format!(
            "I ran `{}` in the shell but cancelled it before it finished.",
            shell.command
        );
        self.finish_user_shell(Some(note), false);
    }

    /// End-of-`!command` plumbing, mirroring what [`TurnMsg::Done`] does for a
    /// turn: the history note enters the agent's history and the session
    /// autosaves, so the shell block + note survive a quit or crash like any
    /// other transcript entry — instead of riding whenever the next turn's
    /// autosave happens to run.
    fn finish_user_shell(&mut self, note: Option<String>, launch_turn: bool) {
        if let Some(note) = note {
            match self.agent.try_lock() {
                Ok(mut a) => a.push_user_note(note),
                Err(_) => {
                    // A turn started while the shell ran and holds the agent.
                    // The note waits for the lock, landing after that turn's
                    // messages — and its Done autosave persists it.
                    let agent = self.agent.clone();
                    tokio::spawn(async move {
                        agent.lock().await.push_user_note(note);
                    });
                    return;
                }
            }
        }
        self.autosave();
        // The note is now in the agent's history but hasn't been shown to the
        // model yet. Kick off an opener-less turn — nothing enqueued, so `run`
        // pushes no user message of its own (the note is already there) and sends
        // the request with the shell output as context.
        if launch_turn && !self.running() {
            self.launch_turn();
        }
    }

    /// Route pasted text: the `/login` key field takes it whole (an API key
    /// paste must not leak into the editor/history); otherwise it goes to the
    /// input editor.
    pub(crate) fn on_paste(&mut self, text: &str) {
        self.disarm();
        if let Some(LoginModal::Key { input, .. }) = &mut self.login_modal {
            input.push_str(text.trim());
            return;
        }
        self.editor.paste(text);
    }

    /// Mouse: wheel scrolls the transcript; a left click on the follow button
    /// resumes following the newest output.
    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
        self.disarm();
        if let Some(sel) = &mut self.model_selector {
            selector_wheel(sel, m.kind);
            return;
        }
        if let Some(sel) = &mut self.session_selector {
            selector_wheel(sel, m.kind);
            return;
        }
        if let Some(sel) = &mut self.theme_selector {
            selector_wheel(sel, m.kind);
            self.preview_selected_theme();
            return;
        }
        if let Some(sel) = &mut self.effort_selector {
            selector_wheel(sel, m.kind);
            return;
        }
        if let Some(sel) = &mut self.command_selector {
            selector_wheel(sel, m.kind);
            return;
        }
        if let Some(LoginModal::Providers(sel)) = &mut self.login_modal {
            selector_wheel(sel, m.kind);
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => {
                // The rows under a selection are about to be different rows.
                self.selection = None;
                self.scroll_offset = self.scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::ScrollDown => {
                self.selection = None;
                self.scroll_offset = self.scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                if let Some(rect) = self.end_button
                    && rect.contains(m.column, m.row)
                {
                    self.scroll_offset = 0; // jump to the newest output
                    return;
                }
                if let Some(rect) = self.home_button
                    && rect.contains(m.column, m.row)
                {
                    self.scroll_offset = self.max_scroll; // jump to the top
                    return;
                }
                // A button going down is the start of a drag, not yet a click:
                // what it turns out to be is settled on the way up, once we know
                // whether the pointer moved. Select-to-copy works over the
                // transcript, the input pane and the status bar alike; whichever
                // area the press lands in bounds the drag.
                let area = if self.transcript_rect.contains(m.column, m.row) {
                    SelectionArea::Transcript
                } else if self.input_rect.contains(m.column, m.row) {
                    SelectionArea::Input
                } else if self.status_rect.contains(m.column, m.row) {
                    SelectionArea::Status
                } else {
                    return;
                };
                self.selection = Some(MouseSelection {
                    anchor: (m.column, m.row),
                    head: (m.column, m.row),
                    area,
                    dragging: true,
                    moved: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // `MouseSelection` is `Copy`: take it out, move the head, put it
                // back — `area_rect` borrows `self` while the selection's own
                // borrow is live, so the two cannot overlap.
                let Some(mut sel) = self.selection else {
                    return;
                };
                if !sel.dragging {
                    return;
                }
                let head = self.area_rect(sel.area).clamp(m.column, m.row);
                sel.moved |= head != sel.anchor;
                sel.head = head;
                self.selection = Some(sel);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(sel) = &mut self.selection else {
                    return;
                };
                let (moved, area) = (sel.moved, sel.area);
                sel.dragging = false;
                if moved {
                    // A real drag: the next frame reads the cells under it back
                    // out and copies them.
                    self.pending_copy = true;
                } else {
                    // A click after all — the transcript's own hit targets get
                    // it. Elsewhere a click just clears the selection: pressing
                    // the input pane or status bar starts no other action.
                    self.selection = None;
                    if area == SelectionArea::Transcript {
                        self.click_transcript(m.column, m.row);
                    }
                }
            }
            _ => {}
        }
    }

    /// Answer a click inside the transcript. The live panels ride at its bottom,
    /// so their rows are hit-tested first — they are the specific targets; a tool
    /// block is the general one.
    fn click_transcript(&mut self, col: u16, row: u16) {
        if let Some(hit) = self
            .row_hits
            .iter()
            .find(|(r, _)| r.contains(col, row))
            .map(|(_, hit)| *hit)
        {
            match hit {
                // Switch the view to that agent: the transcript, the scroll
                // position and the input all follow it. Main is the first row,
                // so it is always the way back.
                crate::ui::RowHit::Agent(id) => self.focus_pane(id),
                crate::ui::RowHit::ToggleDoneTodos => {
                    self.show_done_todos = !self.show_done_todos;
                }
                crate::ui::RowHit::ToggleToolCall(idx) => self.toggle_tool_call_at(idx, row),
            }
            return;
        }
        self.toggle_tool_at(col, row);
    }

    /// Toggle the expansion of the tool block under `(col, row)`, if a click
    /// landed on one. A click on a tool GROUP's summary header toggles the
    /// whole group (the `called N tools` line appears or the calls fan out);
    /// a click on an individual block toggles that call's full output. A click
    /// on a folded thinking summary opens (or closes) that one thought.
    fn toggle_tool_at(&mut self, col: u16, row: u16) {
        let rect = self
            .tool_hits
            .iter()
            .find(|(r, _)| r.contains(col, row))
            .copied();
        let Some((rect, hit)) = rect else { return };
        // Hold the chunk steady on the screen row it is on now: the next draw
        // keeps its top at `rect.y` while its height changes, so the viewport
        // does not jump to the entry (see `draw_chunks`).
        self.pending_scroll_entry = Some(hit);
        self.pending_scroll_row = Some(rect.y);
        let transcript = self.panes.active_transcript();
        // A hidden thought's summary toggles its own expansion, keyed by the
        // entry's transcript index (its content hash moves as the text
        // streams, which would un-key an open thought mid-stream).
        if let EntryKind::Reasoning { .. } = &transcript[hit].kind {
            if !self.thinking_open.remove(&hit) {
                self.thinking_open.insert(hit);
            }
            return;
        }
        // One expansion level: any click on a grouped tool — the summary
        // header or one of its rendered calls — folds or fans out the whole
        // group, keyed by the group head's tool-call id.
        let Some(head) = crate::ui::tool_group_head(transcript, hit) else {
            return;
        };
        let id = transcript
            .get(head)
            .and_then(|e| crate::ui::tool_call_id(&e.kind))
            .map(str::to_owned);
        if let Some(id) = id
            && !self.tool_groups.remove(&id)
        {
            self.tool_groups.insert(id);
        }
        // The group's height is about to change; the group chunk's top is the
        // same chunk the click landed on.
        self.pending_scroll_entry = Some(head);
    }

    /// A click on one call inside a group (its preview, or its full body):
    /// expand that one call to its full output or fold it back to its preview,
    /// holding the group chunk steady on its current screen row.
    fn toggle_tool_call_at(&mut self, idx: usize, row: u16) {
        let transcript = self.panes.active_transcript();
        let Some(head) = crate::ui::tool_group_head(transcript, idx) else {
            return;
        };
        let Some(id) = transcript
            .get(idx)
            .and_then(|e| crate::ui::tool_call_id(&e.kind))
            .map(str::to_owned)
        else {
            return;
        };
        // `pending_scroll_row` is the screen row the pinned chunk's TOP lands
        // on (`draw_chunks`' `entry_pin`), so it must be the group summary's
        // top row — not the clicked call's row, which sits below it. Pinning to
        // the click row slides the whole view down by that gap on every toggle
        // while scrolled up. The summary's rect is the `tool_hits` entry keyed
        // by the head index (this frame's layout — the same one that produced
        // the click).
        let summary_top = self
            .tool_hits
            .iter()
            .find(|(_, i)| *i == head)
            .map(|(r, _)| r.y);
        self.pending_scroll_entry = Some(head);
        self.pending_scroll_row = Some(summary_top.unwrap_or(row));
        if !self.tool_open.remove(&id) {
            self.tool_open.insert(id);
        }
    }

    /// The screen rect of the pane a mouse selection is anchored in — the band
    /// its drag head clamps to and its copied rows are read over. Published
    /// during draw, exactly like the rects themselves.
    pub(crate) fn area_rect(&self, area: SelectionArea) -> HitRect {
        match area {
            SelectionArea::Transcript => self.transcript_rect,
            SelectionArea::Input => self.input_rect,
            SelectionArea::Status => self.status_rect,
        }
    }

    /// The selection as an ordered `(start, end)` pair of screen cells, reading
    /// order (top-left first). `None` when nothing is selected.
    pub(crate) fn selection_span(&self) -> Option<((u16, u16), (u16, u16))> {
        let sel = self.selection?;
        let (a, b) = (sel.anchor, sel.head);
        // Compare row-major: a selection flows through the ends of the rows it
        // crosses, like a terminal's own, not as a rectangular block.
        Some(if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        })
    }

    /// Put the text under the finished selection on the clipboard, and say so in
    /// a toast — the transcript belongs to the conversation, so copy feedback
    /// goes to the toast stack rather than pushing a notice into it.
    pub(crate) fn copy_selection(&mut self, text: &str) {
        let text = text.trim_end_matches('\n');
        if text.trim().is_empty() {
            return;
        }
        let lines = text.lines().count();
        let plural = if lines == 1 { "" } else { "s" };
        match hrdr_app::clipboard_copy(&mut self.clipboard, text) {
            hrdr_app::ClipboardWrite::Copied => {
                self.toasts
                    .info(format!("copied {lines} line{plural} to clipboard"));
            }
            hrdr_app::ClipboardWrite::Failed => {
                self.toasts.error("clipboard write failed");
            }
            hrdr_app::ClipboardWrite::Unavailable => {
                self.toasts.warn("clipboard unavailable");
            }
        }
    }

    /// Whether the input pane should render masked (every char hidden) —
    /// while the `/login` wizard is waiting for the actual API key. The real
    /// value stays in the editor buffer untouched (`/login` reads it via
    /// `self.editor.content()` as usual); only the on-screen rendering
    /// changes, so the key isn't fully visible on screen as it's typed.
    /// Show a transient status toast: a command's output, a usage hint, a busy
    /// guard, a reload notice. These are chrome — regenerated on demand and
    /// never persisted, and they no longer live in the transcript at all: the
    /// toast stack is the transcript's `::Notice` replacement (see
    /// [`hrdr_app::EntryKind::Notice`]).
    ///
    /// Content that belongs to the conversation's history — a turn's error, a
    /// cancel, a compaction result, an agent warning — pushes `Entry::system`
    /// directly instead.
    pub(crate) fn system(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg.into());
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
    /// Append a transcript entry. Each entry carries its own timestamp, set when
    /// it was constructed.
    /// The main agent's transcript — the session's conversation, and the very one
    /// its state persists. (Tests reach for this; the app goes through `panes`.)
    #[cfg(test)]
    pub(crate) fn transcript(&self) -> &Vec<Entry> {
        self.panes.main().transcript()
    }

    /// Mutable access to the main agent's transcript.
    #[cfg(test)]
    pub(crate) fn transcript_mut(&mut self) -> &mut Vec<Entry> {
        self.panes.main_mut().transcript_mut()
    }

    /// Whether the session's agent has a turn in flight.
    ///
    /// Read from the agent, not remembered here: the agent is the one that knows,
    /// and every other agent's `running` already came from the registry. A copy in
    /// the frontend is a copy that can be wrong.
    pub(crate) fn running(&self) -> bool {
        self.registry.is_running(hrdr_agent::MAIN_KEY)
    }

    /// Drop both double-press arms (quit, interrupt): any input that isn't the
    /// armed key itself breaks the sequence.
    fn disarm(&mut self) {
        self.quit_armed = false;
        self.cancel_armed = false;
    }

    /// Whether anything the user can interrupt is in flight: a turn, or a user
    /// `!command` (never both — `!` is rejected while a turn runs).
    pub(crate) fn in_flight(&self) -> bool {
        self.running() || self.user_shell.is_some()
    }

    /// Interrupt whatever [`Self::in_flight`] reports, and say whether there was
    /// anything to interrupt. The one cancel path behind both Esc and Ctrl+C.
    fn cancel_in_flight(&mut self) -> bool {
        if self.running() {
            self.cancel_turn();
        } else if self.user_shell.is_some() {
            self.cancel_user_shell();
        } else {
            return false;
        }
        true
    }

    /// Whether the session's agent is summarizing its own context.
    pub(crate) fn compacting(&self) -> bool {
        self.registry.is_compacting(hrdr_agent::MAIN_KEY)
    }

    /// What the user has said to the session's agent that has not reached it yet.
    /// (The renderer reads the *active* pane's queue; this is main's, for tests.)
    #[cfg(test)]
    pub(crate) fn pending(&self) -> Vec<String> {
        self.registry.pending(hrdr_agent::MAIN_KEY)
    }

    /// The main agent's state: its name, model, endpoint, history, transcript and
    /// token counters — and the payload the session file stores.
    ///
    /// It lives on the main *pane*, because it is the main agent's state and every
    /// agent has one. The status bar reads whichever pane is active
    /// ([`hrdr_app::PaneSet::active_pane`]); this is simply the main one by name.
    pub(crate) fn state(&self) -> &hrdr_app::SessionState {
        &self.panes.main().state
    }

    pub(crate) fn state_mut(&mut self) -> &mut hrdr_app::SessionState {
        &mut self.panes.main_mut().state
    }

    /// Reconcile the pane list against the agent's live sub-agents, and refresh
    /// the main pane's row. Called each frame: `sync` is also what *pins* the pane
    /// being viewed, which is the only thing keeping the agent from releasing it.
    pub(crate) fn sync_panes(&mut self) {
        // The registry drives every pane's status, main included — so tell it
        // whether the session's agent is working.
        let running = self.running();
        self.registry
            .update(hrdr_agent::MAIN_KEY, |e| e.running = running);
        self.panes.sync(&self.registry);
    }

    /// Send `input` to the sub-agent whose pane is on screen.
    ///
    /// The routing rule — steer a turn in flight, start a new one on an idle agent
    /// — is not the TUI's to own: it is the same for any agent driven by anything,
    /// so it lives in `AgentRegistry::send_prompt`. All the frontend does here is
    /// show what was said, and say where the events should be surfaced.
    fn send_to_subagent(&mut self, key: u64, input: String) {
        // Expanded with the main agent's cwd/names, but delivered to the
        // sub-agent — so no `@file` read-state marking on this handle.
        let sent =
            hrdr_app::prepare_outgoing_relayed(&self.agent, &input, self.project_instructions);
        let input = hrdr_agent::Steer::new(sent, input);
        // What was said and everything that comes back is recorded on the agent's
        // own entry; the pane is rebuilt from that record by `sync_panes`. Nothing
        // is folded into the transcript here — doing it in both places would show
        // every message twice.
        let tx = self.tx.clone();
        let delivered = self.registry.send_prompt(key, input, move |ev| {
            // The events go to the agent's log; this only wakes the UI so the next
            // frame picks them up. Sync callback — can't await; and since the
            // event is already durably in the agent's log, a dropped wake (full
            // channel) only defers a redraw, never loses data.
            let _ = tx.try_send(TurnMsg::SubAgent(key, ev));
        });
        self.sync_panes();
        if delivered.is_none() {
            // Released while we were looking at it (finished, delivered, and the
            // prune won the race). Fall back rather than swallow what was typed.
            self.focus_pane(hrdr_app::PaneId::MAIN);
            self.system("that sub-agent has finished and been released".to_string());
        }
    }

    /// Switch the view to `id`: the transcript, the reader's place in it, and the
    /// half-written message all follow.
    ///
    /// The place and the draft belong to the *conversation*, so they are stowed on
    /// the pane being left and restored from the one being entered — glance at the
    /// main agent and come back, and you are where you were with what you were
    /// typing still in the box.
    pub(crate) fn focus_pane(&mut self, id: hrdr_app::PaneId) {
        if self.panes.active() == id {
            return;
        }
        self.stow_view();
        self.panes.focus(id);
        let view = self.panes.active_pane().view.clone();
        self.scroll_offset = view.scroll;
        self.editor.set_content(&view.draft);
        // The pin follows the view: `sync` marks the newly active pane, so the
        // agent keeps it alive, and releases the one we just left.
        self.sync_panes();
        crate::ui::clear_transcript_cache();
    }

    /// The agent behind the pane on screen. `/compact` and anything else that acts
    /// on *a conversation* uses this, so it acts on the one you are looking at —
    /// the same rule as the input box. (Session-scoped commands still use the main
    /// agent: `self.agent`.)
    pub(crate) fn active_agent(&self) -> Arc<tokio::sync::Mutex<Agent>> {
        self.agent_for(self.panes.active())
    }

    /// The agent behind a given pane.
    pub(crate) fn agent_for(&self, id: hrdr_app::PaneId) -> Arc<tokio::sync::Mutex<Agent>> {
        // Every agent is in the registry, the session's own included, so this is
        // one lookup with no pane-kind branch in it.
        self.registry
            .handle(id.key())
            .map(|(a, _)| a)
            // Released while being viewed — fall back rather than do nothing.
            .unwrap_or_else(|| self.agent.clone())
    }

    /// Repoint the **active** agent's chrome — the model/provider/endpoint/window
    /// the status bar shows for it.
    ///
    /// For the main agent that is the session's state, which is what gets saved.
    /// For a sub-agent it is its **registry entry**: the pane is rebuilt from the
    /// registry every frame ([`hrdr_app::PaneSet::sync`]), so a write only to the
    /// pane would be silently overwritten on the next draw. The registry is the
    /// agent's own record of what it is running on, so that is where it belongs.
    fn update_chrome(&mut self, id: hrdr_app::PaneId, f: impl FnOnce(&mut hrdr_app::SessionState)) {
        let key = id.key();
        let Some(pane) = self.panes.pane_mut(id) else {
            return; // released while we were switching it
        };
        // Apply to the pane's state, then push the fields the registry owns back
        // onto the entry. The registry is what the pane is rebuilt from every
        // frame, main agent included — a pane-only write would be undone at the
        // next draw.
        let mut s = std::mem::take(&mut pane.state);
        f(&mut s);
        self.registry.update(key, |e| {
            e.model = s.model.model().to_string();
            e.provider = Some(s.model.provider().to_string());
            e.base_url = s.base_url.clone();
            e.usage = s.usage;
        });
        if let Some(p) = self.panes.pane_mut(id) {
            p.state = s;
        }
    }

    fn update_active_chrome(&mut self, f: impl FnOnce(&mut hrdr_app::SessionState)) {
        self.update_chrome(self.panes.active(), f);
    }

    /// Record a freshly-probed context window against the pane whose agent was
    /// switched (see [`TurnMsg::ContextWindow`]).
    fn set_pane_context_window(&mut self, id: hrdr_app::PaneId, tokens: Option<u32>) {
        self.update_chrome(id, |s| s.usage.context_window = tokens);
    }

    /// What the agent being viewed is running on, as ONE value — read back out of
    /// the ONE value the pane's display state holds it in.
    pub(crate) fn active_model_ref(&self) -> hrdr_agent::ModelRef {
        self.panes.active_pane().model_ref().clone()
    }

    /// `/model` (and `/login`'s provider switch) set the identity of the agent
    /// being viewed — the same agent the input box talks to and `/compact`
    /// compacts. Provider and model land together: the display can no more show a
    /// mismatched pair than the agent can run one.
    pub(crate) fn set_active_model_ref(&mut self, reference: hrdr_agent::ModelRef) {
        self.update_active_chrome(|s| s.model = reference);
    }

    pub(crate) fn set_active_base_url(&mut self, url: String) {
        self.update_active_chrome(|s| s.base_url = url);
    }

    pub(crate) fn set_active_context_window(&mut self, tokens: Option<u32>) {
        self.update_active_chrome(|s| s.usage.context_window = tokens);
    }

    /// Stow the reader's place and their unsent draft on the pane they are leaving.
    fn stow_view(&mut self) {
        let scroll = self.scroll_offset;
        let draft = self.editor.content();
        let view = &mut self.panes.active_pane_mut().view;
        view.scroll = scroll;
        view.draft = draft;
    }

    /// Append a chrome entry (a stats row, a cancel/compaction message) to the
    /// main pane's transcript. Notices no longer pass through here — they go to
    /// the toast stack ([`Self::system`]) or an Esc-dismissible popup.
    fn push_entry(&mut self, e: Entry) {
        self.panes.main_mut().transcript_mut().push(e);
        self.prune_scrollback();
    }

    /// Evict oldest entries from the transcript front when the scrollback cap
    /// is exceeded. The window of intro entries (the header banner + the
    /// welcome/config/project-docs notices — see `App::new`) is always kept
    /// so the user never loses the intro banner.
    fn prune_scrollback(&mut self) {
        if self.panes.main_mut().transcript_mut().len() <= self.scrollback {
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
            .panes
            .main()
            .transcript()
            .iter()
            .take_while(|e| matches!(e.kind, EntryKind::Header | EntryKind::Notice(_)))
            .count();
        let excess = self
            .panes
            .main()
            .transcript()
            .len()
            .saturating_sub(self.scrollback);
        // Ensure we always keep at least `head` entries.
        let remove = excess.min(
            self.panes
                .main_mut()
                .transcript_mut()
                .len()
                .saturating_sub(head),
        );
        if remove == 0 {
            return;
        }
        // Drop the oldest non-head entries.
        let keep_start = head
            .saturating_add(remove)
            .min(self.panes.main_mut().transcript_mut().len());
        self.panes
            .main_mut()
            .transcript_mut()
            .drain(head..keep_start);
        // `thinking_open` is keyed by transcript index; the drain shifted every
        // surviving entry's index down by `remove`. Renumber the open set so an
        // opened thought stays open under its new index, and drop the entries
        // that were themselves evicted (they are off the transcript for good).
        // Without this an open thought folds back to its summary silently, or
        // — worse — a later Reasoning entry landing on the stale index renders
        // expanded without ever being clicked.
        self.thinking_open = self
            .thinking_open
            .iter()
            .filter_map(|&i| {
                if (head..keep_start).contains(&i) {
                    None
                } else if i >= keep_start {
                    Some(i - remove)
                } else {
                    Some(i)
                }
            })
            .collect();
        // Prune the render cache: any key with an entry_idx that has shifted
        // is stale.  Easiest way: clear the whole thread-local transcript cache
        // once (cheap — it rebuilds lazily on the next frame).
        crate::ui::clear_transcript_cache();
    }

    /// Clear the transcript.
    fn clear_transcript(&mut self) {
        self.panes.main_mut().transcript_mut().clear();
        // A wholesale clear invalidates every index-based view state.
        self.thinking_open.clear();
        crate::ui::clear_transcript_cache();
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
        self.arm_file_watcher(&new);
        self.commands = hrdr_app::discover_commands(&new, self.project_instructions);
        self.skills = hrdr_app::discover_skills(&new, self.project_instructions);
    }

    /// Apply the live-changeable settings from a (config, ui-config) pair. Does
    /// NOT touch the model/provider/endpoint (those are session-scoped).
    fn apply_runtime_config(&mut self, cfg: &AgentConfig, ui: &hrdr_app::UiConfig) {
        self.theme = Theme::load(ui.theme.as_deref());
        crate::ui::clear_transcript_cache();
        // Effort and the compaction thresholds are the *agent's* — it publishes them
        // back into the chrome. Updating a frontend copy instead was how a reload
        // could move the context gauge while the agent kept its old behaviour.
        let (effort, auto_compact, reserved) = (
            cfg.effort.clone(),
            cfg.auto_compact,
            cfg.compaction_reserved,
        );
        let agent = self.agent.clone();
        tokio::spawn(async move {
            let mut a = agent.lock().await;
            a.set_effort(effort);
            a.set_auto_compact(auto_compact);
            a.set_compaction_reserved(reserved);
        });
        self.bell = ui.bell;
        self.todo_ttl = ui.todo_ttl;
        self.statusbar_mode = StatusBarMode::from_config(ui.statusbar.as_deref());
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

    /// Abort the in-flight agent task, returning anything queued behind it to
    /// the composer. Cancel stops: it never starts the next turn.
    fn cancel_turn(&mut self) {
        if let Some(handle) = self.turn_handle.take() {
            handle.abort();
            // Keep the aborted handle so the quit path can await it (releasing
            // the agent lock) before the final save. In the stay-in-app cancel
            // case it's simply reaped by the next quit or overwritten by the
            // next turn — harmless either way.
            self.quit_reap = Some(handle);
        }
        self.registry.end_turn(hrdr_agent::MAIN_KEY);
        // The turn never reached `Done`, so nothing has autosaved the visible
        // user message + whatever partial reply streamed in before the
        // cancel. Persist it now — the same best-effort save every other
        // checkpoint uses (skips if the agent lock is still busy; a later
        // save, or the one on quit, catches up).
        self.autosave();
        // Settle any tool calls left mid-execution when the turn was cancelled.
        // The turn was aborted before `finish_tool_call` could emit a `ToolEnd`
        // event, so those entries keep `done: false` and spin forever.
        self.sync_panes();
        hrdr_agent::settle_restored_tools(&mut self.panes.main_mut().state.transcript);
        // If the user typed while the turn was running, those messages are still
        // in the steering queue. They are neither dropped nor sent: they go back
        // into the composer, where the user can edit, resend or clear them.
        //
        // Cancel must *stop*. Sending them straight into a fresh turn made Esc
        // start work instead of ending it — a runaway agent took two presses to
        // stop, and the second only worked if the queue happened to be empty.
        // Leaving them on the queue instead would have them ride out silently on
        // whatever turn came next, minutes later. The composer is the one place
        // they are visible and under the user's control.
        let pending = self.registry.pending(hrdr_agent::MAIN_KEY);
        self.registry.clear_pending(hrdr_agent::MAIN_KEY);
        if pending.is_empty() {
            self.push_entry(Entry::system(hrdr_app::cancel_message(0)));
            return;
        }
        let restored = pending.join("\n");
        let restored_lines = restored.lines().count();
        let typed = self.editor.content();
        let content = if typed.trim().is_empty() {
            restored
        } else {
            // Whatever they are typing now is the newer thought — it stays last.
            format!("{restored}\n{typed}")
        };
        self.editor.set_content(&content);
        self.push_entry(Entry::system(hrdr_app::cancel_message_restored(
            restored_lines,
        )));
    }

    /// Quit the session. If a turn is running, cancel it first — which
    /// autosaves the in-progress transcript — so quitting mid-turn (Ctrl+Q,
    /// double Ctrl+C, Ctrl+D on empty input, `/exit`) never drops the visible
    /// user message or a partial reply.
    fn request_quit(&mut self) {
        if self.running() {
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

    /// True exactly once after a turn caught a tool panic: the terminal driver
    /// must re-enter the alt screen before its next frame. Clears the flag.
    pub(crate) fn take_terminal_lost(&self) -> bool {
        self.terminal_lost.swap(false, Ordering::AcqRel)
    }

    fn spawn_turn(&mut self, input: String) {
        // Prepare the outgoing message: expand `@file` mentions and route any
        // `@agent` mention to the matching sub-agent via a delegation directive.
        let sent = hrdr_app::prepare_outgoing_via(&self.agent, &input, self.project_instructions);
        // Reserve the session id from what the user actually sent (seeds the saved
        // mirror so a first save is named), then enqueue the message as the turn's
        // opener onto the agent's own queue — the same queue a mid-turn steer lands
        // on. `run` drains it, emits `Steered`, and the frontend folds that into the
        // user entry; nothing is pushed into the transcript here.
        self.reserve_session_id(&sent);
        self.registry
            .enqueue(hrdr_agent::MAIN_KEY, hrdr_agent::Steer::new(sent, input));
        self.launch_turn();
    }

    /// Run a turn against the model. Any opener is drained from the agent's steering
    /// queue by `run` itself (enqueued by [`Self::spawn_turn`]); an opener-less call
    /// exists to hand the agent something already in its history (a `!command`'s
    /// output, a landed background result). The user message is shown by folding the
    /// `Steered` event `run` emits — not by pushing an entry here — so a normal
    /// message and a steering message reach the transcript the same way.
    fn launch_turn(&mut self) {
        // An Esc armed against the *previous* turn must not carry over and kill
        // this one on a single press.
        self.cancel_armed = false;
        // Keep last_usage so the status-bar context size persists between turns;
        // it's refreshed when this turn's Usage event arrives.
        let tx = self.tx.clone();
        // The coalescing sink is shared by the two hooks below: one feeds it, the
        // other flushes what it still holds once the turn is over.
        let sink = Arc::new(std::sync::Mutex::new(EventSender::new(tx.clone())));
        let terminal_lost = self.terminal_lost.clone();
        let on_event = {
            let sink = Arc::clone(&sink);
            move |ev| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .send(ev)
            }
        };
        // The turn belongs to the registry: it starts the clock, runs the agent,
        // guards against a panicking tool, records every event on the agent's own
        // entry, and marks it idle again on every exit — including cancellation.
        // This frontend only says how to surface it.
        self.turn_handle =
            self.registry
                .start_turn(hrdr_agent::MAIN_KEY, on_event, move |outcome| async move {
                    if outcome.panicked {
                        // The panic hook already left the alt screen and dropped raw
                        // mode; tell the driver to restore before it draws again.
                        terminal_lost.store(true, Ordering::Release);
                    }
                    // Flush anything the coalescing sink still holds (a stall that
                    // outlived the turn) before signalling completion, so no tool/state
                    // event is lost and `Done` never overtakes them.
                    let queued = sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take_backlog();
                    for msg in queued {
                        if tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(TurnMsg::Done(outcome.error)).await;
                });
        if self.turn_handle.is_none() {
            // The session's agent is registered before any input can reach here, so
            // this is unreachable in practice — but a silently dropped turn would
            // look like a hang, so say so.
            self.system("no agent to run this turn on".to_string());
        }
    }

    /// Launch a turn whose opener enters the model's history but is NOT shown in
    /// the transcript (`/init`). The prompt is pushed as a user note, then an
    /// opener-less turn runs against it — so the model acts on the instruction
    /// without it appearing as something the user typed. The queue-driven opener
    /// path is deliberately not used: it emits `Steered`, which the frontend would
    /// fold into a visible user entry.
    fn launch_hidden(&mut self, prompt: String) {
        // The command guard ensured no turn is running, so the lock is free; push
        // the note synchronously so it precedes the request the opener-less turn
        // issues. On the off chance the lock is momentarily held, fall back to a
        // task — it still lands before `run`'s first request, which waits on the
        // same lock.
        match self.agent.try_lock() {
            Ok(mut a) => a.push_user_note(prompt),
            Err(_) => {
                let agent = self.agent.clone();
                tokio::spawn(async move {
                    agent.lock().await.push_user_note(prompt);
                });
            }
        }
        self.launch_turn();
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
        let elapsed = self
            .registry
            .turn(hrdr_agent::MAIN_KEY)
            .and_then(|t| t.started)
            .map(|t| t.elapsed().as_secs_f64());
        if hrdr_app::should_bell(self.bell, elapsed) {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07"); // BEL
            let _ = out.flush();
        }
    }

    /// Run a compaction pass on the background task, reporting via `TurnMsg`.
    fn spawn_compaction(&mut self, instructions: Option<String>) {
        let pane = self.panes.active();
        self.spawn_compaction_for(pane, instructions);
    }

    /// The queued-`/compact` entry point: remember the request (and the pane it
    /// was made on) and announce it. The turn-end handler runs it via
    /// [`Self::drain_pending_compaction`] once the agent is idle.
    fn queue_compaction(&mut self, instructions: Option<String>) {
        let pane = self.panes.active();
        self.pending_compaction = Some((pane, instructions));
        self.system("compact queued — runs after the current turn ends".to_string());
    }

    /// If a `/compact` was queued while the agent was busy and nothing is
    /// running any more, run it now. Called at the end of turn and compaction
    /// completion, AFTER any queued-steer relaunch — a fresh turn keeps the
    /// compaction queued, because it must see the whole conversation.
    fn drain_pending_compaction(&mut self) {
        if self.running() || self.compacting() || self.turn_handle.is_some() {
            return; // still busy — the queued compaction waits
        }
        let Some((pane, instructions)) = self.pending_compaction.take() else {
            return;
        };
        self.spawn_compaction_for(pane, instructions);
    }

    /// Run a compaction pass on the background task for a specific pane,
    /// reporting via `TurnMsg`.
    fn spawn_compaction_for(&mut self, id: hrdr_app::PaneId, instructions: Option<String>) {
        // Compaction acts on the conversation you are looking at. `run_compaction`
        // takes any agent — a sub-agent's history fills a context window like any
        // other, and it is the agent's own to manage.
        // Summarizing is the model working: its own clock, no tools — on the agent
        // doing the work, so the pane that shows busy is the one being summarized.
        // Keyed to main regardless, this said a sub-agent's `/compact` was the main
        // conversation working.
        self.registry.begin_turn(id.key());
        let agent = self.agent_for(id);
        let tx = self.tx.clone();
        // Compaction's model calls are recorded on the agent's own registry
        // entry, exactly as a turn's are (see `AgentRegistry::start_turn`) —
        // that entry is what every pane is rebuilt from each frame, so the
        // counters pick them up without this task touching pane state. Before
        // this they were recorded nowhere: their cost reached the session total
        // and their tokens reached nothing.
        let live = self.registry.clone();
        let handle = tokio::spawn(async move {
            let res = hrdr_app::run_compaction(agent, instructions, &mut |ev| {
                live.record(id.key(), &ev);
            })
            .await;
            let _ = tx.send(TurnMsg::Compacted(id, res)).await;
        });
        self.turn_handle = Some(handle);
    }

    /// Record a submitted input for Up/Down recall (shared browser).
    fn record_history(&mut self, input: &str) {
        self.history.record(input);
    }

    /// Recall the previous (older) submission into the input.
    fn history_prev(&mut self) {
        self.suppress_completions = true;
        let current = self.editor.content();
        if let Some(text) = self.history.recall_prev(&current) {
            self.editor.set_content(&text);
        }
    }

    /// Take the newest message still queued for the pane on screen back into the
    /// input box, so it can be edited before it is said. `false` when there was
    /// nothing queued and the caller should fall through to history.
    ///
    /// Up already means "the last thing I typed". While a turn runs, that is not
    /// the newest history entry — it is the message sitting in the queue, which is
    /// both more recent and the only one still changeable: history has already
    /// been delivered, and this has not. So an empty box gives the queue first.
    ///
    /// **Taking it off the queue is the point, not a side effect.** Leaving it
    /// there and merely copying the text would send it twice — once when the queue
    /// drains and again when the edit is submitted — which is exactly the bug the
    /// user would then have to notice. Cancelling a turn already hands queued
    /// messages back this way (`cancel_turn`), so the input box is the established
    /// place for a message that has left the queue but not yet been said.
    ///
    /// Keyed to the ACTIVE pane, because the queue is the agent's: typing at a
    /// sub-agent's pane queues on that sub-agent, and Up there must take back what
    /// was said to it rather than something waiting for the main agent.
    fn take_queued_into_input(&mut self) -> bool {
        let key = self.panes.active_pane().id.key();
        let Some(steer) = self.registry.take_newest_pending(key) else {
            return false;
        };
        // The recalled text is a draft again, so keep the completion popup dormant
        // over it — the same reason `history_prev` does.
        self.suppress_completions = true;
        // `display`, not `sent`: what the user typed, before `@file` mentions were
        // expanded into it. They get to edit their sentence, not a file dump — and
        // submitting expands it again.
        self.editor.set_content(&steer.display);
        // Drop the pending block right away rather than at the next frame's sync,
        // so the message is never on screen in two places at once.
        self.sync_panes();
        true
    }

    /// Move toward newer submissions; past the newest, restore the draft.
    fn history_next(&mut self) {
        self.suppress_completions = true;
        let current = self.editor.content();
        if let Some(text) = self.history.recall_next(&current) {
            self.editor.set_content(&text);
        }
    }

    pub(crate) fn on_turn_msg(&mut self, msg: TurnMsg) {
        match msg {
            TurnMsg::Event(ev) => {
                // Ignore buffered events after cancellation — and only then.
                // Neither signal answers this alone: the turn clears the agent's
                // `running` flag itself as it ends (`RunGuard`), so trailing messages
                // from a turn that finished *normally* would be dropped if that were
                // the test; and a turn driven without a handle (a queued message
                // waiting on the agent's own queue) still has events worth applying.
                // A cancelled turn is the one case where both are gone.
                if self.turn_handle.is_some() || self.running() {
                    self.apply_event(ev);
                }
            }
            TurnMsg::UserShell(ev, note) => {
                let ended = matches!(ev, AgentEvent::ToolEnd { .. });
                if ended {
                    self.user_shell = None;
                }
                self.record_local(ev);
                if ended {
                    self.finish_user_shell(note, true);
                }
            }
            TurnMsg::System(text) => {
                // An async/passive line (e.g. a late `/models` result) is
                // transient session chrome: a toast, never a transcript entry.
                self.toasts.info(text);
                // Do NOT reset scroll_offset here: this is an async/passive line
                // (e.g. a late `/models` result). Resetting would yank the user's
                // view when they are scrolled up reading back-scroll. When the
                // user is already following (offset == 0), it stays 0 unchanged.
            }
            TurnMsg::Popup(text) => {
                self.popup = Some(NoticePopup::new(text));
            }
            TurnMsg::Diff(text) => {
                self.push_entry(Entry::diff(text));
                // Same rationale as TurnMsg::System above: passive async output.
            }
            TurnMsg::Done(err) => {
                // A cancelled turn took the handle with it *and* marked the agent
                // idle; a `Done` arriving after that is stale. Both signals are
                // needed — see the `Event` arm above.
                if self.turn_handle.take().is_none() && !self.running() {
                    return;
                }
                self.registry.end_turn(hrdr_agent::MAIN_KEY);
                // The turn is over — clear any sub-agents still in the live panel
                // (an interrupted turn may not have delivered their ToolEnd).
                if let Some(e) = err {
                    self.push_entry(Entry::system(format!("[error] {e}")));
                }
                // Append the final stats for the turn (before stats are reset by
                // any queued turn that spawns next).
                if let Some(stats) = self.turn_stats() {
                    self.push_entry(Entry::stats(stats));
                }
                // Age out completed TODOs once per turn.
                // NOTE: TODO ageing is the agent's now (`Agent::age_todos`, at turn
                // end). The list is agent state the model re-reads every turn, so
                // doing it only here meant a headless run — and every delegated
                // sub-agent — kept its finished items forever and paid for them in
                // context on every request.
                // Notify on completion of a non-trivial turn (if enabled).
                self.maybe_bell();
                // Persist the completed turn into the active session, if any.
                self.autosave();
                // NOTE: an `/init` turn does NOT re-seed the system prompt with the
                // `AGENTS.md` it just wrote. The agent wrote it — it has the content
                // in its context already, and injecting it again would say the same
                // thing twice. The next conversation (`/new`) starts from the file on
                // disk, which is where a change belongs.
                // NOTE: no auto-compaction here any more. The agent compacts itself
                // when its context fills (`Agent::maybe_self_compact`), before each
                // request rather than only between turns — so it also protects a
                // long tool-calling turn, and it works identically with no UI
                // attached (headless, and every delegated sub-agent). A frontend
                // copy of the same threshold only re-compacted what the agent had
                // just compacted. `/compact` remains, as a deliberate user action.
                // The turn ended without draining what was queued (the model
                // answered instead of calling a tool). Launch a fresh turn if
                // anything is still queued: `run` drains the head as its opener and
                // emits `Steered`, which the frontend folds into the user entry. Any
                // further queued messages steer that new turn, exactly as before.
                let has_pending = self.steering.lock().map(|q| !q.is_empty()).unwrap_or(false);
                if has_pending {
                    self.launch_turn();
                }
                // A `/compact` queued while the turn ran waits for a quiet moment:
                // this one, unless the relaunch above made the agent busy again
                // (then it waits for that turn too — a compaction must see the
                // whole conversation, and the steer that just started is part of it).
                self.drain_pending_compaction();
            }
            TurnMsg::FileIndex(cwd, files) => {
                self.file_index = files;
                self.file_index_building = false;
                if self.file_index_dirty {
                    // A filesystem change landed while the walk was running —
                    // this freshly built list is already stale. Keep the cache
                    // invalidated; the next `@` keystroke builds again.
                    self.file_index_dirty = false;
                    self.file_index_cwd = None;
                } else {
                    self.file_index_cwd = Some(cwd);
                }
            }
            TurnMsg::FileIndexDirty => self.on_file_index_dirty(),
            TurnMsg::SaveDone(result) => self.on_save_done(result),
            TurnMsg::Identity(id, reference, base_url, window) => {
                // The agent has taken it; the chrome may now say so.
                self.update_chrome(id, |s| s.model = reference);
                if let Some(url) = base_url {
                    self.update_chrome(id, |s| s.base_url = url);
                }
                if let Some(w) = window {
                    self.set_pane_context_window(id, Some(w));
                }
            }
            TurnMsg::ContextWindow(id, tokens) => {
                // A model/provider switch re-probed the endpoint; honor the new
                // advertised max (drives "X of Y" + the auto-compaction trigger)
                // for the agent that was actually switched.
                self.set_pane_context_window(id, Some(tokens));
                // Hand it to that agent as well. The probe is the only place this
                // figure exists, and keeping it in frontend state is what left the
                // agent unable to tell how full it was — so it could never compact
                // itself, and nor could any sub-agent that inherited from it.
                let agent = self.agent_for(id);
                tokio::spawn(async move {
                    agent.lock().await.set_context_window(Some(tokens));
                });
            }
            // A sub-agent's events are recorded on its own registry entry, and
            // `sync_panes` replays them into its pane. This message carries no
            // transcript work of its own — it exists to wake the UI so the next
            // frame shows them.
            TurnMsg::SubAgent(_key, _ev) => self.sync_panes(),
            TurnMsg::BrowserLogin(outcome) => self.on_browser_login(outcome),
            TurnMsg::ModelCatalog {
                generation,
                models,
                source,
                warning,
            } => self.apply_catalog_result(generation, models, source, warning),
            TurnMsg::ConfigChanged => self.maybe_reload_config(),
            TurnMsg::Compacted(pane, res) => {
                self.turn_handle = None;
                self.registry.end_turn(pane.key());
                // The gauge's reading described the pre-compaction history.
                // When the pass actually shrank it, swap in the report's
                // post-compaction estimate so the bar shows how much room the
                // summary bought — clearing it to zero claimed the context was
                // empty. A no-op compaction leaves the reading alone: nothing
                // changed, so it is still accurate. Through `update_chrome`,
                // so it lands on the compacted agent's registry entry — which
                // is what every pane, main included, is rebuilt from each
                // frame; a write to the pane alone was undone at the next draw.
                if let Ok(report) = &res
                    && report.shrank()
                {
                    self.update_chrome(pane, |s| s.usage.set_last(Some((report.context_after, 0))));
                }
                self.push_entry(Entry::system(hrdr_app::compaction_message(&res)));
                if res.is_ok() {
                    self.autosave();
                }
                self.scroll_offset = 0;
                // Resume any queued work now that the context is compact: `run`
                // drains the head as its opener (shown via the folded `Steered`).
                let has_pending = self.steering.lock().map(|q| !q.is_empty()).unwrap_or(false);
                if has_pending {
                    self.launch_turn();
                }
                // Another `/compact` typed while this one ran: run it now that the
                // agent is idle (or wait, if the relaunch above started a turn).
                self.drain_pending_compaction();
            }
        }
    }

    /// Format the final stats line for the just-finished turn, if it produced
    /// any output.
    fn turn_stats(&self) -> Option<String> {
        let turn = self.registry.turn(hrdr_agent::MAIN_KEY)?;
        turn.started?;
        hrdr_app::turn_stats_line(hrdr_app::TurnStatsLine {
            // The model's working time, excluding the tool calls it waited on.
            elapsed_secs: turn.infer_elapsed().as_secs_f64(),
            // The slice of it that was actual generation — what the rate is over.
            gen_secs: turn.decode_elapsed().as_secs_f64(),
            ttft_secs: turn.ttft(),
            out_tokens: turn.out_tokens(),
            tok_per_sec: turn.tok_per_sec(),
            usage: self.state().usage.last(),
            cached_tokens: turn.last_cached_tokens,
            reasoning_tokens: turn.last_reasoning_tokens,
        })
    }

    /// A detached sub-agent or watch finished while nothing was running: wake
    /// the model so it reacts to the result instead of sitting on it until the
    /// user's next message.
    ///
    /// `Agent::run` folds finished background tasks into the conversation before
    /// each request, so an empty turn is enough to deliver them — it pushes no
    /// user message of its own. Only fires when idle: a running turn already
    /// drains them at its next request, and a compaction is about to. A
    /// CANCELLED entry (`task_cancel`) never wakes anyone — the turn it would
    /// spawn has nothing to deliver.
    pub(crate) fn maybe_deliver_background(&mut self) {
        if self.running() || self.compacting() {
            return;
        }
        let ready = self
            .background_tasks
            .lock()
            .map(|v| v.iter().any(|t| t.done && !t.delivered && !t.cancelled))
            .unwrap_or(false);
        if ready {
            self.launch_turn();
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
        self.registry.begin_turn(hrdr_agent::MAIN_KEY);
    }

    /// Apply a `/model` pick without driving the picker's UI — the same
    /// `apply_choice` call its confirm makes.
    #[cfg(test)]
    pub(crate) fn apply_model_choice_for_test(
        &mut self,
        provider: &str,
        model: &str,
        window: Option<u32>,
    ) {
        let mut host = commands::TuiHost { app: self };
        hrdr_app::apply_choice(&mut host, provider, model.to_string(), window)
            .expect("the model switch is applied");
    }

    /// Record an event this frontend produced itself on the session agent's own
    /// entry, then bring the panes up to date.
    ///
    /// A turn does both halves for the events it drives
    /// ([`hrdr_agent::AgentRegistry::start_turn`] records, then wakes the
    /// frontend). A `!command` is a real tool block with no turn behind it, so the
    /// frontend has to record it the same way — otherwise it renders once and is
    /// missing from the agent's record, its durable transcript, and a resume.
    fn record_local(&mut self, ev: AgentEvent) {
        self.registry.record(hrdr_agent::MAIN_KEY, &ev);
        self.apply_event(ev);
    }

    /// Handle one of the **main agent's** events.
    ///
    /// The transcript is not built here. It is built by replaying the agent's own
    /// record ([`hrdr_app::PaneSet::sync`]) — the same way a sub-agent's is, by the
    /// same reducer, from the same kind of record. There is one implementation of
    /// "what does this event do to a conversation", and it does not live in a
    /// frontend.
    ///
    /// What is left here is what is genuinely the terminal's: writing the session
    /// file, and the wall-clock it holds for a reasoning block's duration.
    fn apply_event(&mut self, ev: AgentEvent) {
        // The agent already emits a steered message in the form the user typed it:
        // the queue carries both, so nothing here has to pair them up.
        // Mid-turn durability: the agent committed a round and sent its history.
        if let AgentEvent::History(messages) = &ev {
            self.persist_mid_turn((**messages).clone());
        }
        // The event is already recorded on the agent's own entry — its transcript,
        // its counters and its turn clock — by the turn that produced it
        // (`AgentRegistry::start_turn`), for every agent alike. Nothing is folded
        // here; this wake-up only brings the panes up to date with that record.
        self.sync_panes();
    }
}

/// Wheel over an open picker: scroll lines move the highlight (which the view
/// follows); any other event is swallowed — the modal owns the mouse.
fn selector_wheel<T>(sel: &mut Selector<T>, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollUp => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.up()),
        MouseEventKind::ScrollDown => (0..MOUSE_SCROLL_LINES).for_each(|_| sel.down()),
        _ => {}
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
        assert_eq!(5, hrdr_app::DEFAULT_TODO_TTL_TURNS);
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

    // ---- /compact acts on the pane you are looking at ----

    use super::App;
    use hrdr_agent::{AgentConfig, AgentEntry, AgentRegistry, MAIN_KEY, PaneId};

    /// An app with one delegated sub-agent registered, whose pane is the one on
    /// screen — the state a `/compact` typed into a sub-agent's pane runs in.
    ///
    /// The endpoint is a closed loopback port: nothing here is allowed to reach a
    /// model, and the sub-agent's history is empty, so the compaction pass returns
    /// before it would make a request.
    fn app_viewing_a_sub_agent() -> (App, u64, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "local://test-model".parse().unwrap(),
            cwd: tmp.path().to_path_buf(),
            sandbox: hrdr_tools::SandboxMode::None,
            ..Default::default()
        };
        let ui = hrdr_app::UiConfig {
            auto_resume: false, // never pick up the developer's real sessions
            ..Default::default()
        };
        let mut app = App::new(config.clone(), ui, "logo").unwrap();

        let sub = hrdr_agent::Agent::new(config).expect("minimal sub-agent");
        let key = AgentRegistry::next_key();
        app.registry.register(AgentEntry {
            key,
            bg_id: None,
            tool_id: None,
            label: "sub".to_string(),
            model: sub.model_name(),
            provider: Some(sub.provider_name().to_string()),
            base_url: sub.endpoint_base_url(),
            effort: None,
            auto_compact: true,
            compaction_reserved: 0,
            sandbox: hrdr_tools::SandboxMode::None,
            todos: Default::default(),
            usage: Default::default(),
            events: hrdr_agent::event_log(),
            reasoning_open: false,
            pending_notices: Vec::new(),
            turn: hrdr_agent::TurnStats::default(),
            agent: std::sync::Arc::new(tokio::sync::Mutex::new(sub)),
            steering: hrdr_agent::steering_queue(),
            running: false,
            compacting: false,
            done: false,
            delivered: false,
            pinned: true,
            transcript: None,
        });
        // The pane only exists once the registry has been folded in.
        app.sync_panes();
        app.panes.focus(PaneId(key));
        assert_eq!(
            app.panes.active(),
            PaneId(key),
            "the sub-agent's pane is the one on screen"
        );
        // The cwd goes back with it: an app whose working directory has been
        // deleted saves and reads nothing like a real one.
        (app, key, tmp)
    }

    /// That agent's latest `(prompt, completion)` context reading.
    fn usage_last(live: &AgentRegistry, key: u64) -> Option<(u32, u32)> {
        live.with(|v| v.iter().find(|e| e.key == key).and_then(|e| e.usage.last()))
    }

    /// Compacting a sub-agent's pane is that agent working, so its pane is the one
    /// that shows busy and its gauge is the one updated. Keyed to `MAIN_KEY`
    /// regardless, the main conversation showed as working while a sub-agent
    /// summarized, and the main gauge was the one reset.
    #[tokio::test]
    async fn compacting_a_sub_agent_pane_runs_that_panes_clock() {
        let (mut app, key, _tmp) = app_viewing_a_sub_agent();
        let live = app.registry.clone();
        live.update(MAIN_KEY, |e| e.usage.set_last(Some((100, 5))));
        live.update(key, |e| e.usage.set_last(Some((200, 7))));

        app.spawn_compaction(None);
        assert!(
            live.is_running(key),
            "the summarized pane is the one whose clock runs"
        );
        assert!(
            !live.is_running(MAIN_KEY),
            "the main conversation is not the one working"
        );

        // What the compaction task reports back when it lands.
        app.on_turn_msg(TurnMsg::Compacted(
            PaneId(key),
            Ok(hrdr_agent::CompactionReport {
                reason: hrdr_agent::CompactionReason::UserRequested,
                before: 10,
                after: 3,
                context_after: 42,
                prompt_tokens: 0,
                cached_prompt_tokens: None,
                output_tokens: 0,
                cost_usd: None,
                stage: hrdr_agent::ShrinkStage::Full,
                attempts: 1,
            }),
        ));
        assert!(
            !live.is_running(key),
            "the clock stops on the pane it started on"
        );
        assert_eq!(
            usage_last(&live, key),
            Some((42, 0)),
            "the gauge shows the context the summary left, not the pre-compaction reading — and not zero"
        );
        assert_eq!(
            usage_last(&live, MAIN_KEY),
            Some((100, 5)),
            "the main conversation's context reading is untouched"
        );

        // A no-op compaction (nothing to summarize) leaves the reading alone:
        // nothing changed, so it is still accurate.
        app.on_turn_msg(TurnMsg::Compacted(
            PaneId(key),
            Ok(hrdr_agent::CompactionReport {
                reason: hrdr_agent::CompactionReason::UserRequested,
                before: 3,
                after: 3,
                context_after: 0,
                prompt_tokens: 0,
                cached_prompt_tokens: None,
                output_tokens: 0,
                cost_usd: None,
                stage: hrdr_agent::ShrinkStage::Full,
                attempts: 0,
            }),
        ));
        assert_eq!(
            usage_last(&live, key),
            Some((42, 0)),
            "a nothing-to-do pass keeps the existing reading"
        );
    }

    // ---- EventSender: bounded, coalescing UI event sink ----

    use super::{AgentEvent, EventSender, TurnMsg};
    use tokio::sync::mpsc;

    /// Canonical, order-preserving fold of an event stream: adjacent streaming
    /// deltas of the same kind (and, for tool output, the same call id) merge
    /// into one concatenated token; every other event is an opaque marker. Two
    /// streams with equal canonical forms render identically (each delta is a
    /// `push_str`), so this is the yardstick for "coalescing lost nothing".
    fn canon(evs: &[AgentEvent]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for ev in evs {
            let (tag, text): (String, Option<&str>) = match ev {
                AgentEvent::Text(t) => ("T".to_string(), Some(t)),
                AgentEvent::Reasoning(t) => ("R".to_string(), Some(t)),
                AgentEvent::ToolOutput { id, chunk } => (format!("O:{id}"), Some(chunk)),
                AgentEvent::ToolStart { id, .. } => (format!("start:{id}"), None),
                AgentEvent::ToolEnd { id, ok, .. } => (format!("end:{id}:{ok}"), None),
                other => (format!("{other:?}"), None),
            };
            match text {
                // A same-kind delta right behind us: fold into it.
                Some(t)
                    if out
                        .last()
                        .is_some_and(|p| p.starts_with(&format!("{tag}="))) =>
                {
                    out.last_mut().unwrap().push_str(t);
                }
                Some(t) => out.push(format!("{tag}={t}")),
                None => out.push(tag),
            }
        }
        out
    }

    fn text(s: &str) -> AgentEvent {
        AgentEvent::Text(s.to_string())
    }
    fn reasoning(s: &str) -> AgentEvent {
        AgentEvent::Reasoning(s.to_string())
    }
    fn tool_out(id: &str, s: &str) -> AgentEvent {
        AgentEvent::ToolOutput {
            id: id.to_string(),
            chunk: s.to_string(),
        }
    }

    /// A fast producer streaming into a sink whose consumer isn't draining must
    /// never let the channel exceed its capacity — the whole point of bounding.
    /// Control events (tool start/end) interleaved with the flood must still be
    /// retained (in the sink's backlog), never dropped.
    #[tokio::test]
    async fn event_sender_never_exceeds_channel_capacity() {
        const CAP: usize = 4;
        let (tx, mut rx) = mpsc::channel::<TurnMsg>(CAP);
        let mut sender = EventSender::new(tx);

        // 5000 events, no consumer draining. The channel must stay <= CAP the
        // whole time; the overflow lives in the coalescing backlog.
        for i in 0..5000u32 {
            if i % 500 == 0 {
                sender.send(AgentEvent::ToolStart {
                    id: i.to_string(),
                    name: "x".to_string(),
                    args: String::new(),
                });
            } else {
                sender.send(text("tok "));
            }
            assert!(
                rx.len() <= CAP,
                "channel held {} items, over cap {CAP}",
                rx.len()
            );
        }

        // Nothing was lost: drain everything (channel + flushed backlog) and
        // confirm every ToolStart survived.
        tokio::spawn(async move { sender.drain().await });
        let mut starts = 0;
        while let Some(TurnMsg::Event(ev)) = rx.recv().await {
            if matches!(ev, AgentEvent::ToolStart { .. }) {
                starts += 1;
            }
        }
        assert_eq!(starts, 10, "a ToolStart was dropped under backpressure");
    }

    /// Coalescing must be invisible: the drained stream, folded canonically,
    /// must equal the input folded canonically — same content, same order —
    /// while actually reducing the message count (proving it engaged).
    #[tokio::test]
    async fn event_sender_coalescing_preserves_content_and_order() {
        let input = vec![
            text("Hello, "),
            text("world"),
            reasoning("think"),
            reasoning("ing"),
            AgentEvent::ToolStart {
                id: "1".to_string(),
                name: "sh".to_string(),
                args: String::new(),
            },
            tool_out("1", "aa"),
            tool_out("1", "bb"),
            AgentEvent::ToolEnd {
                id: "1".to_string(),
                name: "sh".to_string(),
                result: "done".to_string(),
                ok: true,
            },
            // A different tool id must NOT coalesce with the previous output.
            tool_out("2", "zz"),
            text("bye"),
        ];

        // A tiny channel forces the backlog to build, so coalescing engages.
        let (tx, mut rx) = mpsc::channel::<TurnMsg>(2);
        let mut sender = EventSender::new(tx);
        for ev in &input {
            sender.send(ev.clone());
        }
        tokio::spawn(async move { sender.drain().await });

        let mut got = Vec::new();
        while let Some(TurnMsg::Event(ev)) = rx.recv().await {
            got.push(ev);
        }

        assert_eq!(
            canon(&got),
            canon(&input),
            "coalesced stream renders differently than the original"
        );
        assert!(
            got.len() < input.len(),
            "coalescing did not engage (got {} vs input {})",
            got.len(),
            input.len()
        );
    }

    /// Shutdown while a producer is blocked on capacity must not deadlock: once
    /// the receiver is dropped, `drain` (which awaits `send`) must return
    /// promptly with the backlog abandoned, not hang forever.
    #[tokio::test]
    async fn event_sender_drain_does_not_deadlock_after_receiver_drop() {
        let (tx, rx) = mpsc::channel::<TurnMsg>(1);
        let mut sender = EventSender::new(tx);

        // Fill the channel and pile more into the backlog.
        sender.send(text("a")); // -> channel
        sender.send(text("b")); // -> backlog (channel full)
        sender.send(AgentEvent::ToolEnd {
            id: "1".to_string(),
            name: "sh".to_string(),
            result: String::new(),
            ok: true,
        }); // -> backlog

        // "Shutdown": the render loop is gone.
        drop(rx);

        // drain must observe the closed channel and return, not block.
        tokio::time::timeout(std::time::Duration::from_secs(2), sender.drain())
            .await
            .expect("drain deadlocked after the receiver was dropped");
    }
}
