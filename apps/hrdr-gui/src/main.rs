//! `hrdr-gui` — a floem desktop frontend for the agentic coding harness.
//!
//! A **proof-of-concept** that drives the same UI-agnostic core the TUI uses —
//! `hrdr_agent::Agent` — rendering its streamed [`AgentEvent`]s in a floem
//! window: assistant text + `<think>` reasoning, tool calls with live output
//! and pass/fail results, and Enter-to-send. As GUI features grow, the parts
//! shared with the TUI (transcript model, slash commands, sessions, …) get
//! lifted out of `hrdr-tui` into a shared crate both frontends consume.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use floem::AnyView;
use floem::ext_event::create_signal_from_tokio_channel;
use floem::prelude::*;
use floem::reactive::{Scope, create_effect};
use floem::views::Decorators;

mod md;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Message, Session};
use tokio::sync::Mutex as TokioMutex;

// ---- colors -------------------------------------------------------------
// Fallbacks when a theme omits a role (or no theme loads).
const DIM: Color = Color::rgb8(0x8a, 0x8a, 0x8a);
const USER: Color = Color::rgb8(0x6c, 0xb6, 0xff);
const OK: Color = Color::rgb8(0x5f, 0xd0, 0x7a);
const ERR: Color = Color::rgb8(0xe0, 0x6c, 0x6c);
const TOOL: Color = Color::rgb8(0xc9, 0xa2, 0x66);
const ACCENT: Color = Color::rgb8(0x6c, 0x9e, 0xff);
const ACCENT2: Color = Color::rgb8(0xc7, 0x8f, 0xe3);

/// Chat-role colors resolved from an hjkl theme — the same theme system the TUI
/// uses (see `hrdr-tui`'s `Theme`), mapped here to floem colors.
#[derive(Clone, Copy)]
struct GuiTheme {
    bg: Color,
    user: Color,
    assistant: Color,
    dim: Color,
    tool: Color,
    ok: Color,
    err: Color,
    accent: Color,
    accent2: Color,
}

impl GuiTheme {
    /// Load an hjkl theme TOML (or hjkl's bundled default). The role mapping is
    /// shared with the TUI ([`hrdr_app::ChatPalette`]); this only converts the
    /// resolved RGB roles to floem colors with GUI fallbacks.
    fn load(path: Option<&str>) -> Self {
        let p = hrdr_app::ChatPalette::load(path);
        let c = |rgb: Option<(u8, u8, u8)>, fb: Color| {
            rgb.map(|(r, g, b)| Color::rgb8(r, g, b)).unwrap_or(fb)
        };
        Self {
            bg: c(p.background, Color::rgb8(0x1e, 0x1e, 0x24)),
            user: c(p.user, USER),
            assistant: c(p.assistant, Color::rgb8(0xe0, 0xe0, 0xe0)),
            dim: c(p.dim, DIM),
            tool: c(p.warn, TOOL),
            ok: c(p.success, OK),
            err: c(p.error, ERR),
            accent: c(p.accent, ACCENT),
            accent2: c(p.accent2, ACCENT2),
        }
    }
}

// ---- transcript model ---------------------------------------------------
// Streamed fields are signals so tokens update the view in place; the item
// list only changes when a new item is pushed (keyed by a stable id).

#[derive(Clone)]
struct Assistant {
    reasoning: RwSignal<String>,
    text: RwSignal<String>,
}

#[derive(Clone)]
struct Tool {
    call_id: String,
    name: String,
    args: String,
    output: RwSignal<String>,
    result: RwSignal<String>,
    ok: RwSignal<bool>,
    done: RwSignal<bool>,
    /// Collapse the (potentially long) streamed output; toggled by clicking the
    /// tool header. Starts collapsed.
    collapsed: RwSignal<bool>,
}

#[derive(Clone)]
enum Body {
    User(String),
    Assistant(Assistant),
    Tool(Tool),
    System(String),
    Error(String),
    /// A unified diff (`/diff`), rendered with +/− coloring.
    Diff(String),
}

#[derive(Clone)]
struct Item {
    id: u64,
    body: Body,
    /// The child scope this item's signals were created on (`None` for bodies
    /// with no signals), disposed when the item is cleared — see
    /// [`clear_items`].
    scope: Option<Scope>,
    /// When the item was pushed (per-message timestamps, `/goto 5m`).
    time: chrono::DateTime<chrono::Local>,
    /// 1-based message number (user/assistant items only) — the `#N` shown in
    /// the meta line and targeted by `/goto`, `/find`, `/copy msg N`.
    msg_no: Option<usize>,
}

/// One row in the completion dropdown: a slash command or an `@file` match.
#[derive(Clone)]
enum CompRow {
    Slash {
        name: &'static str,
        desc: &'static str,
    },
    /// `start` is the byte offset of the `@` in the input; `path` is the match.
    File { start: usize, path: String },
}

impl CompRow {
    /// Stable dyn_stack key.
    fn key(&self) -> String {
        match self {
            CompRow::Slash { name, .. } => format!("/{name}"),
            CompRow::File { path, .. } => format!("@{path}"),
        }
    }
}

/// UI-thread message from a running turn (mirrors the TUI's `TurnMsg`).
#[derive(Clone)]
enum UiMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
    /// Out-of-band system line (e.g. an async `/models` result).
    System(String),
    /// `@file` completion index built off-thread.
    FileIndex(Vec<String>),
    /// Compaction finished: `Ok((before, after))` message counts, or an error.
    Compacted(Result<(usize, usize), String>),
    /// The config file changed on disk (from the shared watcher).
    ConfigChanged,
    /// Out-of-band diff block (the async `/diff` result).
    Diff(String),
    /// A completed turn was auto-saved; carries the session id and whether this
    /// was the first save (so the UI thread can adopt the id and notify once).
    /// `generation` is the save-generation at spawn time: `/clear` bumps it, so
    /// a save that raced a clear is ignored instead of resurrecting the old
    /// session id (which would make the next conversation overwrite its file).
    Saved {
        id: String,
        first_save: bool,
        generation: u64,
    },
}

fn main() -> anyhow::Result<()> {
    // A tokio runtime entered on this (UI) thread so floem's tokio-channel
    // bridge + per-turn agent tasks can `tokio::spawn`. Held for program life.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let config = AgentConfig::load();
    let ui = hrdr_app::UiConfig::load();
    let model = config.model.clone();
    let ctx_window = config.context_window;
    let base_url = config.base_url.clone();
    // Keep the config around for `/provider` preset resolution.
    let cfg = Rc::new(config.clone());
    let agent_raw = Agent::new(config)?;
    // Shared TODO list, mutated by the todo_write tool during turns.
    let todos = agent_raw.todos();
    let agent = Arc::new(TokioMutex::new(agent_raw));

    floem::launch(move || app_view(agent, todos, model, ctx_window, base_url, cfg, ui));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn app_view(
    agent: Arc<TokioMutex<Agent>>,
    todos: Arc<std::sync::Mutex<Vec<hrdr_tools::TodoItem>>>,
    model: String,
    ctx_window: Option<u32>,
    base_url: String,
    cfg: Rc<AgentConfig>,
    ui: hrdr_app::UiConfig,
) -> impl IntoView {
    // Theme as a signal so `/theme` can live-swap it; the revision counter is
    // baked into the dyn_stack keys below so cached children (whose colors are
    // captured at build time) rebuild on a swap.
    let theme_sig: RwSignal<GuiTheme> = create_rw_signal(GuiTheme::load(ui.theme.as_deref()));
    let theme_rev: RwSignal<u64> = create_rw_signal(0);
    let show_thinking = ui.show_thinking;
    // Persistent scope for dynamically-created per-message signals, so they
    // outlive the effect that creates them.
    let cx = Scope::current();
    let transcript: RwSignal<Vec<Item>> = create_rw_signal(Vec::new());
    let input = create_rw_signal(String::new());
    let running = create_rw_signal(false);
    // A running `/login` wizard; while `Some`, submitted lines feed it instead
    // of the model or the slash dispatcher (an API key must never be echoed).
    let login: RwSignal<Option<hrdr_app::LoginWizard>> = create_rw_signal(None);
    // Sticky `/expand all`: new tools spawn expanded while set (TUI parity).
    let expand_all = create_rw_signal(false);
    // Finish nudge (the TUI's bell → a desktop notification here); reloadable.
    let bell = create_rw_signal(ui.bell);
    let next_id = create_rw_signal(0u64);
    // Model is a signal so `/model <name>` can switch it and the status bar
    // reflects the change live.
    let model = create_rw_signal(model);
    // Cached relative file paths under the cwd, for `@file` completion. Built
    // lazily the first time an `@` mention is typed (see the effect below).
    let file_index: RwSignal<Vec<String>> = create_rw_signal(Vec::new());
    // Off-thread build in flight / already built (reset by /cwd //revert).
    let file_index_state: RwSignal<u8> = create_rw_signal(0); // 0 stale, 1 building, 2 ready
    // Last turn's reported (prompt, completion) token usage, for the status bar.
    let usage: RwSignal<Option<(u32, u32)>> = create_rw_signal(None);
    // Turn-start instant + measured time-to-first-token (seconds) for the last
    // turn, shown in the status bar.
    let turn_start: RwSignal<Option<Instant>> = create_rw_signal(None);
    let ttft: RwSignal<Option<f64>> = create_rw_signal(None);
    // Active session's file id (stem), once assigned by the first auto-save (or
    // adopted on `/resume`). Subsequent saves reuse it; `/clear` resets it.
    let session_id: RwSignal<Option<String>> = create_rw_signal(None);
    // Display-name override for the session (`/rename`); `None` derives it from
    // the first user message.
    let session_label: RwSignal<Option<String>> = create_rw_signal(None);
    // Save generation: bumped by `/clear` so an in-flight save's late `Saved`
    // message can be told apart from one belonging to the current conversation.
    let save_gen: RwSignal<u64> = create_rw_signal(0);
    // Whether to show the model's `<think>` reasoning (`/thinking` toggles);
    // initial value from config (`show_thinking`).
    let show_reasoning = create_rw_signal(show_thinking);
    // OS clipboard for `/copy`, held for the app's life so the selection stays
    // served (X11 requires the owning process to stay alive). `None` if
    // unavailable. `Rc<RefCell<…>>` since the UI thread is single-threaded.
    let clipboard = Rc::new(RefCell::new(hjkl_clipboard::Clipboard::new().ok()));
    // Handle to the in-flight turn task; `abort()` cancels it (Esc / Stop).
    let turn_handle: Rc<RefCell<Option<tokio::task::JoinHandle<()>>>> = Rc::new(RefCell::new(None));
    // Submitted-input history + Up/Down browsing (shared with the TUI).
    let history = Rc::new(RefCell::new(hrdr_app::HistoryBrowser::load()));

    // Startup notices (parity with the TUI): warn if the config file is
    // invalid, note the AGENTS.md pickup.
    if let Some(warning) = hrdr_app::startup_config_warning() {
        system(transcript, next_id, warning);
    }
    if agent
        .try_lock()
        .map(|a| a.project_docs().is_some())
        .unwrap_or(false)
    {
        system(transcript, next_id, hrdr_app::PROJECT_DOCS_LOADED_MSG);
    }

    // Startup auto-resume: pick up the most recent saved session for this
    // directory (like the TUI; `auto_resume = false` / --no-auto-resume in the
    // TUI config disables it — the GUI honors the same knob).
    if ui.auto_resume {
        let cwd = agent
            .try_lock()
            .map(|a| a.cwd().display().to_string())
            .unwrap_or_default();
        if let Some((id, session)) = hrdr_app::latest_session_for_cwd(&cwd) {
            if let Ok(mut a) = agent.try_lock() {
                a.set_messages(session.messages.clone());
                a.set_model(session.model.clone());
            }
            model.set(session.model.clone());
            rebuild_transcript(cx, transcript, next_id, &session.messages);
            session_id.set(Some(id));
            session_label.set(Some(session.name.clone()));
            system(
                transcript,
                next_id,
                format!(
                    "resumed most recent session '{}' ({} messages) — /clear to start fresh",
                    session.name,
                    session.messages.len()
                ),
            );
        }
    }

    // Reasoning-effort label (status bar; `/effort` sets it).
    let effort: RwSignal<Option<String>> = create_rw_signal(None);
    // Endpoint + context window as signals so `/provider` updates them live.
    let base_url: RwSignal<String> = create_rw_signal(base_url);
    let ctx_window: RwSignal<Option<u32>> = create_rw_signal(ctx_window);
    // Status-bar mode (`/statusbar`; from config).
    let statusbar_mode: RwSignal<hrdr_app::StatusBarMode> = create_rw_signal(
        hrdr_app::StatusBarMode::from_config(ui.statusbar.as_deref()),
    );
    // Working-directory + git-branch display for the status bar (follow /cwd
    // and resumed sessions).
    let start_cwd = hrdr_app::agent_cwd(&agent);
    let dir_sig: RwSignal<String> = create_rw_signal(hrdr_app::display_dir(&start_cwd));
    let branch_sig: RwSignal<Option<String>> = create_rw_signal(hrdr_app::git_branch(&start_cwd));
    // Session-cumulative token counters (the status bar's ↑/↓).
    let session_in: RwSignal<usize> = create_rw_signal(0);
    let session_out: RwSignal<usize> = create_rw_signal(0);
    // Per-message timestamp style (`/timestamps`; from config).
    let timestamp_style: RwSignal<hrdr_app::TimestampStyle> = create_rw_signal(
        hrdr_app::TimestampStyle::from_config(ui.timestamps.as_deref()),
    );
    // TODO panel state: a reactive mirror of the shared list (refreshed on tool
    // events), the turn counter + completion stamps for aging, and the TTL.
    let todos_sig: RwSignal<Vec<hrdr_tools::TodoItem>> = create_rw_signal(Vec::new());
    let todo_turn: RwSignal<u64> = create_rw_signal(0);
    let todo_completed_at: Rc<RefCell<std::collections::HashMap<String, u64>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let todo_ttl: RwSignal<u64> = create_rw_signal(ui.todo_ttl);
    // `/find` state: the active query + the message number cycling last landed on.
    let find_query: RwSignal<Option<String>> = create_rw_signal(None);
    let find_pos: RwSignal<usize> = create_rw_signal(0);
    // Scroll target for /goto //find: message-number → ViewId registry filled at
    // render time, and the view the transcript scroll should bring into view.
    let msg_view_ids: Rc<RefCell<std::collections::HashMap<usize, floem::ViewId>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    // Every rendered item keyed by its transcript id — the max key is the
    // bottom of the transcript (`/goto end`).
    let item_view_ids: Rc<RefCell<std::collections::HashMap<u64, floem::ViewId>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));
    let goto_view: RwSignal<Option<floem::ViewId>> = create_rw_signal(None);
    // Messages submitted while a turn runs, sent FIFO as turns finish (like
    // the TUI's queue).
    let queue: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
    // True while a compaction (summarization) pass runs; auto-compaction
    // triggers at this fraction of the context window (0 disables).
    let compacting: RwSignal<bool> = create_rw_signal(false);
    // Signal so `/reload` + hot-reload pick up config edits (the TUI does).
    let auto_compact_ratio = create_rw_signal(cfg.auto_compact);
    let compaction_reserved = create_rw_signal(cfg.compaction_reserved);
    // Streamed tokens this turn (the per-turn stats line's token count/rate).
    let out_tokens: RwSignal<usize> = create_rw_signal(0);
    // The in-flight turn is an /init run → reload AGENTS.md when it completes.
    let pending_init: RwSignal<bool> = create_rw_signal(false);
    // Last config mtime we applied/wrote, so persisting a setting doesn't
    // bounce back as a hot-reload (same dedup the TUI uses).
    let config_mtime_seen: Rc<std::cell::Cell<Option<std::time::SystemTime>>> =
        Rc::new(std::cell::Cell::new(hrdr_app::config_mtime()));

    // Bridge background turns → the UI thread over one long-lived channel.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
    let events = create_signal_from_tokio_channel(rx);

    // Launch a model turn. Shared by Enter/Send, the queue drain on `Done`,
    // and the CommandHost hooks (`/retry`, `/init`). `show_as_user` displays
    // the prompt as a user message (`false` for /init's internal prompt).
    let spawn_turn: Rc<dyn Fn(String, bool)> = {
        let agent = agent.clone();
        let tx = tx.clone();
        let th = turn_handle.clone();
        Rc::new(move |text: String, show_as_user: bool| {
            if show_as_user {
                push_item(transcript, next_id, Body::User(text.clone()));
            }
            running.set(true);
            // Start the TTFT clock; cleared until the first token of this turn.
            turn_start.set(Some(Instant::now()));
            ttft.set(None);
            out_tokens.set(0);
            // Expand `@file` mentions for the model only; the transcript keeps
            // the bare `@path` the user typed (same split as the TUI). Paths
            // resolve against the agent's cwd (it follows /cwd and /resume).
            let sent = hrdr_app::expand_mentions(&text, &hrdr_app::agent_cwd(&agent));
            let agent = agent.clone();
            let tx = tx.clone();
            // Snapshot session state for the post-turn auto-save (signals
            // can't be read from the spawned task).
            let existing_id = session_id.get_untracked();
            let session_label = session_label.get_untracked();
            let cur_model = model.get_untracked();
            let base_url = base_url.get_untracked();
            let generation = save_gen.get_untracked();
            let handle = tokio::spawn(async move {
                let tx_ev = tx.clone();
                let result = agent
                    .lock()
                    .await
                    .run(sent, move |ev| {
                        let _ = tx_ev.send(UiMsg::Event(ev));
                    })
                    .await;
                let _ = tx.send(UiMsg::Done(result.err().map(|e| e.to_string())));
                // Auto-save the (now-updated) conversation, best-effort.
                if let Some(o) = hrdr_app::save_agent_session(
                    agent,
                    existing_id,
                    session_label,
                    cur_model,
                    base_url,
                )
                .await
                {
                    let _ = tx.send(UiMsg::Saved {
                        id: o.id,
                        first_save: o.first_save,
                        generation,
                    });
                }
            });
            *th.borrow_mut() = Some(handle);
        })
    };

    // Populate the file index off-thread when the user starts an `@file`
    // mention (walking a big tree must not stall the UI); re-armed after
    // /cwd //revert reset the state. Paths come from the agent's cwd.
    let agent_for_index = agent.clone();
    let tx_for_index = tx.clone();
    create_effect(move |_| {
        if input.get().contains('@') && file_index_state.get_untracked() == 0 {
            {
                let cwd = hrdr_app::agent_cwd(&agent_for_index);
                file_index_state.set(1);
                let tx = tx_for_index.clone();
                hrdr_app::spawn_file_index(cwd, move |files| {
                    let _ = tx.send(UiMsg::FileIndex(files));
                });
            }
        }
    });

    // Probe the endpoint in the background and warn if it's unreachable or
    // doesn't have the configured model (shared core with the TUI).
    {
        let agent = agent.clone();
        let tx_health = tx.clone();
        let m = model.get_untracked();
        let url = base_url.get_untracked();
        tokio::spawn(async move {
            if let Some(warning) = hrdr_app::endpoint_health_warning(agent, m, url).await {
                let _ = tx_health.send(UiMsg::System(warning));
            }
        });
    }
    // Shared config-file watch → hot-reload (the guard must outlive the app;
    // the window lives for the process, so leaking it is fine).
    {
        let tx_watch = tx.clone();
        let guard = hrdr_app::watch_config(move || {
            let _ = tx_watch.send(UiMsg::ConfigChanged);
        });
        std::mem::forget(guard);
    }

    // Start a compaction pass (shared by /compact and the auto-compaction
    // trigger below): runs like a turn — input queues behind it, Esc/Stop
    // cancels it — and lands as UiMsg::Compacted.
    let start_compaction: Rc<dyn Fn(Option<String>)> = {
        let agent = agent.clone();
        let tx = tx.clone();
        let th = turn_handle.clone();
        Rc::new(move |instructions: Option<String>| {
            running.set(true);
            compacting.set(true);
            turn_start.set(Some(Instant::now()));
            ttft.set(None);
            let agent = agent.clone();
            let tx = tx.clone();
            let handle = tokio::spawn(async move {
                let res = hrdr_app::run_compaction(agent, instructions).await;
                let _ = tx.send(UiMsg::Compacted(res));
            });
            *th.borrow_mut() = Some(handle);
        })
    };

    let spawn_for_done = spawn_turn.clone();
    let queue_for_done = queue.clone();
    let compact_for_done = start_compaction.clone();
    let agent_for_events = agent.clone();
    let tx_for_events = tx.clone();
    let mtime_for_events = config_mtime_seen.clone();
    let base_url_for_events = base_url;
    let todos_for_events = todos.clone();
    let todo_stamps_for_done = todo_completed_at.clone();
    create_effect(move |_| {
        let Some(msg) = events.get() else { return };
        match msg {
            // Ignore events buffered before a cancel (the task was aborted).
            UiMsg::Event(_) if !running.get_untracked() => {}
            UiMsg::Event(ev) => {
                // First streamed token → record time-to-first-token.
                if ttft.get_untracked().is_none()
                    && matches!(ev, AgentEvent::Text(_) | AgentEvent::Reasoning(_))
                    && let Some(start) = turn_start.get_untracked()
                {
                    ttft.set(Some(start.elapsed().as_secs_f64()));
                }
                if matches!(ev, AgentEvent::Text(_) | AgentEvent::Reasoning(_)) {
                    out_tokens.set(out_tokens.get_untracked() + 1);
                }
                // Session-cumulative token counters for the status bar.
                if let AgentEvent::Usage {
                    prompt_tokens,
                    completion_tokens,
                } = &ev
                {
                    session_in.set(session_in.get_untracked() + *prompt_tokens as usize);
                    session_out.set(session_out.get_untracked() + *completion_tokens as usize);
                }
                // Tool completions may have rewritten the shared TODO list.
                let refresh_todos = matches!(ev, AgentEvent::ToolEnd { .. });
                handle_event(cx, transcript, next_id, usage, expand_all, ev);
                if refresh_todos && let Ok(t) = todos_for_events.lock() {
                    todos_sig.set(t.clone());
                }
            }
            UiMsg::Done(err) => {
                // Stale Done from an aborted task (cancel raced the channel);
                // discard, like the TUI.
                if !running.get_untracked() {
                    return;
                }
                running.set(false);
                let failed = err.is_some();
                if let Some(e) = err {
                    push_item(transcript, next_id, Body::Error(e));
                }
                // Finish nudge — the GUI's bell (shared gate with the TUI:
                // enabled + the turn ran long enough to be worth it).
                if hrdr_app::should_bell(
                    bell.get_untracked(),
                    turn_start
                        .get_untracked()
                        .map(|t| t.elapsed().as_secs_f64()),
                ) {
                    notify_turn_done(failed);
                }
                // Per-turn stats line (shared formatting with the TUI).
                if let Some(started) = turn_start.get_untracked()
                    && let Some(line) = hrdr_app::turn_stats_line(
                        started.elapsed().as_secs_f64(),
                        ttft.get_untracked(),
                        out_tokens.get_untracked(),
                        usage.get_untracked(),
                    )
                {
                    system(transcript, next_id, line);
                }
                // An /init turn just wrote AGENTS.md — load it into the
                // system prompt (shared core with the TUI's /reload).
                if pending_init.get_untracked() {
                    pending_init.set(false);
                    let agent = agent_for_events.clone();
                    let tx = tx_for_events.clone();
                    tokio::spawn(async move {
                        if let Some(line) = hrdr_app::reload_project_docs(agent).await {
                            let _ = tx.send(UiMsg::System(line));
                        }
                    });
                }
                // Age out completed TODOs (a completed item stays visible for
                // todo_ttl turns, like the TUI panel).
                todo_turn.set(todo_turn.get_untracked() + 1);
                if let Ok(mut t) = todos_for_events.lock() {
                    hrdr_app::age_completed_todos(
                        &mut t,
                        &mut todo_stamps_for_done.borrow_mut(),
                        todo_turn.get_untracked(),
                        todo_ttl.get_untracked(),
                    );
                    todos_sig.set(t.clone());
                }
                // Proactively compact near the context limit before more work
                // (shared threshold; the queue resumes from Compacted).
                if !compacting.get_untracked()
                    && hrdr_app::should_auto_compact(
                        usage.get_untracked().map(|(p, _)| p),
                        ctx_window.get_untracked(),
                        compaction_reserved.get_untracked(),
                        {
                            let r = auto_compact_ratio.get_untracked();
                            r > 0.0 && r <= 1.0
                        },
                    )
                {
                    system(
                        transcript,
                        next_id,
                        "context near the limit — auto-compacting…",
                    );
                    (compact_for_done)(None);
                    return;
                }
                // Start the next queued message, if any (FIFO, like the TUI).
                let next = queue_for_done.borrow_mut().pop_front();
                if let Some(next) = next {
                    (spawn_for_done)(next, true);
                }
            }
            UiMsg::System(s) => push_item(transcript, next_id, Body::System(s)),
            UiMsg::Diff(d) => push_item(transcript, next_id, Body::Diff(d)),
            UiMsg::FileIndex(files) => {
                file_index.set(files);
                file_index_state.set(2);
            }
            UiMsg::ConfigChanged => {
                // Ignore self-inflicted writes (persisting a setting) via the
                // mtime guard, like the TUI.
                let now = hrdr_app::config_mtime();
                if now == mtime_for_events.get() {
                    return;
                }
                mtime_for_events.set(now);
                let line = apply_config_reload(
                    false,
                    &agent_for_events,
                    show_reasoning,
                    theme_sig,
                    theme_rev,
                    timestamp_style,
                    statusbar_mode,
                    todo_ttl,
                    bell,
                    effort,
                    auto_compact_ratio,
                    compaction_reserved,
                );
                system(transcript, next_id, line);
            }
            UiMsg::Compacted(res) => {
                running.set(false);
                compacting.set(false);
                // Context shrank; drop stale usage so the gauge refreshes on
                // the next turn (and we don't immediately re-trigger).
                usage.set(None);
                system(transcript, next_id, hrdr_app::compaction_message(&res));
                if res.is_ok() {
                    // Persist the compacted conversation (same as turn end).
                    let agent = agent_for_events.clone();
                    let tx = tx_for_events.clone();
                    let existing = session_id.get_untracked();
                    let label = session_label.get_untracked();
                    let cur_model = model.get_untracked();
                    let url = base_url_for_events.get_untracked();
                    let generation = save_gen.get_untracked();
                    tokio::spawn(async move {
                        if let Some(o) =
                            hrdr_app::save_agent_session(agent, existing, label, cur_model, url)
                                .await
                        {
                            let _ = tx.send(UiMsg::Saved {
                                id: o.id,
                                first_save: o.first_save,
                                generation,
                            });
                        }
                    });
                }
                // Resume any queued work now that the context is compact.
                let next = queue_for_done.borrow_mut().pop_front();
                if let Some(next) = next {
                    (spawn_for_done)(next, true);
                }
            }
            UiMsg::Saved {
                id,
                first_save,
                generation,
            } => {
                // Stale save (a /clear happened after the turn spawned) —
                // adopting its id would attach the next conversation to the
                // old session's file.
                if generation != save_gen.get_untracked() {
                    return;
                }
                if first_save {
                    system(transcript, next_id, hrdr_app::session_saved_notice(&id));
                }
                session_id.set(Some(id));
            }
        }
    });

    let history_for_send = history.clone();
    let msg_ids_for_send = msg_view_ids.clone();
    let item_ids_for_send = item_view_ids.clone();
    let spawn_for_send = spawn_turn.clone();
    let compact_for_send = start_compaction.clone();
    let mtime_for_send = config_mtime_seen.clone();
    let queue_for_send = queue.clone();
    let th_for_host = turn_handle.clone();
    let clipboard_for_send = clipboard.clone();
    let agent_for_send = agent.clone();
    let tx_for_send = tx.clone();
    let todos_for_send = todos.clone();
    let todo_stamps_for_send = todo_completed_at.clone();
    let cfg_for_send = cfg.clone();
    let send = move || {
        // Trim like the TUI does, so " /help" is still a command.
        let text = input.get().trim().to_string();
        if text.is_empty() {
            return;
        }
        // The command host, built once and reused by the `/login` wizard and the
        // slash dispatcher below.
        let mut host = GuiHost {
            cx,
            transcript,
            next_id,
            usage,
            model,
            input,
            effort,
            running,
            file_index,
            file_index_state,
            session_id,
            session_label,
            save_gen,
            turn_start,
            ttft,
            show_reasoning,
            theme: theme_sig,
            theme_rev,
            timestamp_style,
            statusbar_mode,
            dir: dir_sig,
            branch: branch_sig,
            session_in,
            session_out,
            todo_ttl,
            todos: todos_for_send.clone(),
            todos_sig,
            todo_turn,
            todo_completed_at: todo_stamps_for_send.clone(),
            expand_all,
            bell,
            auto_compact_ratio,
            compaction_reserved,
            find_query,
            find_pos,
            ctx_window,
            cfg: cfg_for_send.clone(),
            queue: queue_for_send.clone(),
            turn_handle: th_for_host.clone(),
            spawn_turn: spawn_for_send.clone(),
            start_compaction: compact_for_send.clone(),
            compacting,
            pending_init,
            config_mtime_seen: mtime_for_send.clone(),
            clipboard: clipboard_for_send.clone(),
            agent: agent_for_send.clone(),
            tx: tx_for_send.clone(),
            base_url,
            login,
        };
        // A running `/login` wizard captures the line before history/quit/
        // dispatch — an API key must never be echoed, recalled, or sent.
        if login.get_untracked().is_some() {
            if let Some(mut wizard) = login.get_untracked() {
                let done = wizard.step(&text, &mut host);
                login.set((!done).then_some(wizard));
            }
            input.set(String::new());
            return;
        }
        // Record every submitted line for Up/Down recall, and reset browsing.
        history_for_send.borrow_mut().record(&text);
        // Common quit words (shared with the TUI) close the window.
        if hrdr_app::is_quit_command(&text) {
            floem::quit_app();
            return;
        }
        // Slash commands run through the shared `hrdr_app` dispatcher (so the TUI
        // and GUI share one implementation) — also while a turn runs, like the
        // TUI (turn-coupled commands busy-guard themselves). An unrecognized
        // `/…` falls through to the model (a literal path still works) — unless
        // it's a registered command the GUI just doesn't implement, which gets
        // a notice instead of confusing the model.
        if let Some(rest) = text.strip_prefix('/') {
            // Frontend-coupled scroll/search commands are handled locally
            // (they drive the GUI's scroll container), like the TUI's local
            // arms; everything else goes to the shared dispatcher.
            {
                let mut parts = rest.splitn(2, char::is_whitespace);
                let lcmd = hrdr_app::resolve_alias(parts.next().unwrap_or(""));
                let larg = parts.next().unwrap_or("").trim().to_string();
                let f = FindCtx {
                    transcript,
                    next_id,
                    find_query,
                    find_pos,
                    ids: msg_ids_for_send.clone(),
                    all_ids: item_ids_for_send.clone(),
                    goto_view,
                };
                let handled = match lcmd {
                    "goto" => {
                        f.goto_cmd(&larg);
                        true
                    }
                    "find" | "search" => {
                        f.find_cmd(&larg);
                        true
                    }
                    "next" => {
                        f.cycle(true);
                        true
                    }
                    "prev" | "previous" => {
                        f.cycle(false);
                        true
                    }
                    _ => false,
                };
                if handled {
                    input.set(String::new());
                    return;
                }
            }
            if hrdr_app::dispatch(&mut host, &text) {
                input.set(String::new());
                return;
            }
            let cmd = rest.split_whitespace().next().unwrap_or("");
            if hrdr_app::is_known_command(cmd) {
                system(
                    transcript,
                    next_id,
                    format!("/{} isn't available in the GUI yet (see /help)", cmd),
                );
                input.set(String::new());
                return;
            }
        }
        input.set(String::new());
        // A turn is running → queue the message; it's sent when the turn ends
        // (FIFO, like the TUI).
        if running.get_untracked() {
            let n = {
                let mut q = queue_for_send.borrow_mut();
                q.push_back(text);
                q.len()
            };
            system(
                transcript,
                next_id,
                format!("queued ({n}) — sends when the current turn finishes"),
            );
            return;
        }
        (spawn_for_send)(text, true);
    };

    // Cancel the in-flight turn: abort the task (dropping its future releases
    // the agent lock; the next turn repairs any dangling tool calls) and mark
    // the turn done. Late buffered events are dropped via the `running` guard.
    // Queued follow-ups are discarded, like the TUI's cancel.
    let queue_for_cancel = queue.clone();
    let cancel = move || {
        if !running.get_untracked() {
            return;
        }
        if let Some(h) = turn_handle.borrow_mut().take() {
            h.abort();
        }
        running.set(false);
        compacting.set(false);
        pending_init.set(false); // a cancelled /init must not reload docs later
        let dropped = {
            let mut q = queue_for_cancel.borrow_mut();
            let n = q.len();
            q.clear();
            n
        };
        let msg = hrdr_app::cancel_message(dropped);
        system(transcript, next_id, msg);
    };

    let send_enter = send.clone();
    let send_btn = send.clone();
    let cancel_esc = cancel.clone();
    let cancel_btn = cancel.clone();

    let ids_for_render = msg_view_ids.clone();
    let all_ids_for_render = item_view_ids.clone();
    let transcript_view = scroll(
        dyn_stack(
            move || {
                let rev = theme_rev.get();
                transcript
                    .get()
                    .into_iter()
                    .map(move |i| (rev, i))
                    .collect::<Vec<_>>()
            },
            |(rev, item): &(u64, Item)| (*rev, item.id),
            move |(_, item)| {
                let view = render_item(
                    item.clone(),
                    theme_sig.get_untracked(),
                    show_reasoning,
                    timestamp_style,
                );
                // Register user/assistant views so /goto //find can scroll to
                // them (re-registered whenever the item re-renders).
                if let Some(n) = item.msg_no {
                    ids_for_render.borrow_mut().insert(n, view.id());
                }
                all_ids_for_render.borrow_mut().insert(item.id, view.id());
                view
            },
        )
        .style(|s| s.flex_col().width_full().gap(10.0)),
    )
    // Bring the /goto //find target into view when one is set.
    .scroll_to_view(move || goto_view.get())
    .style(|s| s.flex_grow(1.0).width_full().padding(10.0));

    // TODO panel (mirrors the TUI's): the model's task list, shown while
    // non-empty; completed items age out via todo_ttl.
    let todo_panel = dyn_stack(
        move || {
            let rev = theme_rev.get();
            todos_sig
                .get()
                .into_iter()
                .map(move |t| (rev, t))
                .collect::<Vec<_>>()
        },
        |(rev, t): &(u64, hrdr_tools::TodoItem)| format!("{rev}:{}:{}", t.status, t.content),
        move |(_, t)| {
            let th = theme_sig.get_untracked();
            let (glyph, color) = match t.status.as_str() {
                "completed" => ("✓", th.ok),
                "in_progress" => ("▸", th.tool),
                _ => ("·", th.dim),
            };
            let content = t.content.clone();
            label(move || format!("{glyph} {content}"))
                .style(move |s| s.color(color))
                .into_any()
        },
    )
    .style(move |s| {
        let s = s
            .flex_col()
            .width_full()
            .padding_horiz(10.0)
            .padding_vert(4.0);
        if todos_sig.with(Vec::is_empty) {
            s.hide()
        } else {
            s
        }
    });

    let hist_up = history.clone();
    let hist_down = history.clone();
    // Multi-line input (parity with the TUI's plain engine): Enter sends;
    // Shift+Enter / Alt+Enter — and Enter after a trailing `\` — insert a
    // newline; Up/Down recall history for single-line input only (multi-line
    // editing keeps them as cursor moves); Esc cancels the in-flight turn.
    let input_box = {
        use floem::keyboard::Modifiers;
        use floem::views::editor::command::CommandExecuted;
        use floem::views::editor::keypress::default_key_handler;
        use floem::views::editor::keypress::key::KeyInput;
        use floem::views::editor::keypress::press::KeyPress;
        use std::str::FromStr;

        let key_enter = KeyInput::from_str("enter").expect("enter key parses");
        let key_up = KeyInput::from_str("up").expect("up key parses");
        let key_down = KeyInput::from_str("down").expect("down key parses");
        let key_esc = KeyInput::from_str("escape").expect("escape key parses");
        let plain_enter = KeyPress::new(key_enter.clone(), Modifiers::empty());

        let ed = floem::views::text_editor::text_editor_keys(
            input.get_untracked(),
            move |editor_sig, kp, mods| {
                if kp.key == key_enter {
                    let text = input.get_untracked();
                    if mods.shift() || mods.alt() || text.trim_end().ends_with('\\') {
                        // Insert a newline (synthesized plain Enter → the
                        // default keymap's InsertNewLine).
                        return default_key_handler(editor_sig)(&plain_enter, Modifiers::empty());
                    }
                    send_enter();
                    return CommandExecuted::Yes;
                }
                if kp.key == key_esc && mods.is_empty() {
                    cancel_esc();
                    return CommandExecuted::Yes;
                }
                // Up/Down recall previous submissions (readline-style), but
                // only for single-line input, like the TUI.
                if mods.is_empty() && kp.key == key_up && !input.get_untracked().contains('\n') {
                    let current = input.get_untracked();
                    if let Some(text) = hist_up.borrow_mut().recall_prev(&current) {
                        input.set(text);
                    }
                    return CommandExecuted::Yes;
                }
                if mods.is_empty() && kp.key == key_down && !input.get_untracked().contains('\n') {
                    if let Some(text) = hist_down.borrow_mut().recall_next() {
                        input.set(text);
                    }
                    return CommandExecuted::Yes;
                }
                default_key_handler(editor_sig)(kp, mods)
            },
        );
        let doc = ed.doc();

        // Editor → `input` signal (the source of truth everything else reads).
        let doc_for_update = doc.clone();
        let ed = ed
            .update(move |_| input.set(doc_for_update.text().to_string()))
            .placeholder("Message hrdr…  (Enter to send · Shift/Alt+Enter for a newline)")
            .editor_style(|s| s.hide_gutter(true));

        // `input` signal → editor (history recall, /undo, /add, clearing after
        // send): replace the whole document when they disagree. The guard
        // breaks the update↔effect cycle.
        create_effect(move |_| {
            let want = input.get();
            let have = doc.text().to_string();
            if want != have {
                use floem::views::editor::core::editor::EditType;
                use floem::views::editor::core::selection::Selection;
                doc.edit_single(
                    Selection::region(0, have.len()),
                    &want,
                    EditType::InsertChars,
                );
            }
        });

        // Auto-grow with content like the TUI's input (1..=6 text rows).
        ed.style(move |s| {
            let rows = input
                .get()
                .lines()
                .count()
                .clamp(1, hrdr_app::INPUT_MAX_ROWS as usize) as f32;
            s.flex_grow(1.0).height(rows * 22.0 + 14.0).padding(4.0)
        })
    };

    // One button: "Stop" (cancel) while a turn runs, "Send" otherwise.
    let action_button = button(label(move || if running.get() { "Stop" } else { "Send" }))
        .on_click_stop(move |_| {
            if running.get_untracked() {
                cancel_btn();
            } else {
                send_btn();
            }
        });
    let input_row = h_stack((input_box, action_button))
        .style(|s| s.width_full().gap(8.0).padding(10.0).items_center());

    // Completion list shown above the input: slash commands while a `/…` is being
    // typed, or ranked `@file` paths while an `@…` mention is active (both use the
    // shared rankers). Clicking a row fills the input.
    let comp_rows = move || -> Vec<CompRow> {
        let inp = input.get();
        if inp.starts_with('/') {
            return hrdr_app::slash_completions(&inp)
                .into_iter()
                // Only offer what the GUI implements (see TUI_ONLY_COMMANDS).
                .filter(|(name, _)| !hrdr_app::is_tui_only(name))
                .map(|(name, desc)| CompRow::Slash { name, desc })
                .collect();
        }
        if let Some((start, query)) = hrdr_app::active_file_token(&inp) {
            return file_index.with(|files| {
                hrdr_app::rank_file_matches(files, &query)
                    .into_iter()
                    .map(|path| CompRow::File { start, path })
                    .collect()
            });
        }
        Vec::new()
    };
    let completions = dyn_stack(comp_rows, CompRow::key, move |row| match row {
        CompRow::Slash { name, desc } => h_stack((
            label(move || name.to_string())
                .style(move |s| s.color(theme_sig.get().user).font_bold()),
            label(move || desc.to_string()).style(move |s| s.color(theme_sig.get().dim)),
        ))
        .style(|s| s.gap(8.0).padding_horiz(10.0).padding_vert(2.0))
        .on_click_stop(move |_| input.set(format!("{name} ")))
        .into_any(),
        CompRow::File { start, path } => label({
            let path = path.clone();
            move || path.clone()
        })
        .style(move |s| {
            s.color(theme_sig.get().user)
                .padding_horiz(10.0)
                .padding_vert(2.0)
        })
        .on_click_stop(move |_| {
            // Replace the partial `@…` token with `@<path> `.
            let inp = input.get_untracked();
            let prefix = inp.get(..start).unwrap_or("");
            input.set(format!("{prefix}@{path} "));
        })
        .into_any(),
    })
    .style(move |s| {
        let s = s
            .flex_col()
            .width_full()
            .max_height(160.0)
            .padding_vert(4.0)
            .background(theme_sig.get().bg);
        // Collapse entirely when there's nothing to show.
        let inp = input.get();
        if inp.starts_with('/') || hrdr_app::active_file_token(&inp).is_some() {
            s
        } else {
            s.hide()
        }
    });

    // Status bar: model · context · last-turn output tokens, + a live "thinking"
    // indicator while a turn runs.
    // Status bar from the shared content model (same sections/colors as the
    // TUI); /statusbar picks hidden / one row / wrapping rows.
    let status_segs = move || -> Vec<(usize, hrdr_app::StatusSeg)> {
        let dir = dir_sig.get();
        let branch = branch_sig.get();
        let model_name = model.get();
        let effort_label = effort.get();
        hrdr_app::status_sections(&hrdr_app::StatusInputs {
            dir: &dir,
            branch: branch.as_deref(),
            tokens_in: session_in.get(),
            tokens_out: session_out.get(),
            ctx_used: usage.get().map(|(p, _)| p as usize).unwrap_or(0),
            context_window: ctx_window.get(),
            auto_compact_enabled: {
                let r = auto_compact_ratio.get();
                r > 0.0 && r <= 1.0
            },
            compaction_reserved: compaction_reserved.get(),
            model: &model_name,
            effort: effort_label.as_deref(),
            ttft: ttft.get(),
            nerd_icons: false,
        })
        .into_iter()
        .enumerate()
        .collect()
    };
    let status_row = dyn_stack(
        move || {
            let rev = theme_rev.get();
            status_segs()
                .into_iter()
                .map(move |(i, s)| (rev, i, s))
                .collect::<Vec<_>>()
        },
        |(rev, i, seg): &(u64, usize, hrdr_app::StatusSeg)| {
            let text: String = seg.runs.iter().map(|r| r.text.as_str()).collect();
            format!("{rev}:{i}:{text}")
        },
        move |(_, i, seg)| {
            let th = theme_sig.get_untracked();
            let mut children: Vec<AnyView> = Vec::new();
            if i > 0 {
                children.push(
                    label(|| "│")
                        .style(move |s| s.color(th.dim).margin_horiz(6.0))
                        .into_any(),
                );
            }
            if let Some(gauge) = seg.gauge {
                // The context gauge as a real progress container: a fill layer
                // sized by the fraction behind the label (instead of the
                // character-cell split the TUI's text runs use).
                children.push(ctx_gauge_view(gauge, th));
            } else {
                for run in seg.runs {
                    let text = run.text.clone();
                    let role = run.role;
                    children.push(
                        label(move || text.clone())
                            .style(move |s| status_run_style(s, role, th))
                            .into_any(),
                    );
                }
            }
            h_stack_from_iter(children)
                .style(|s| s.items_center())
                .into_any()
        },
    )
    .style(move |s| {
        let s = s.flex_row().items_center();
        if statusbar_mode.get() == hrdr_app::StatusBarMode::Wrap {
            s.flex_wrap(floem::style::FlexWrap::Wrap)
        } else {
            s
        }
    });
    let status_bar = h_stack((
        status_row,
        label(|| "● thinking…").style(move |s| {
            if running.get() {
                s.color(theme_sig.get().tool)
            } else {
                s.hide()
            }
        }),
    ))
    .style(move |s| {
        let s = s
            .width_full()
            .padding_horiz(10.0)
            .padding_vert(4.0)
            .justify_between()
            .items_center();
        if statusbar_mode.get() == hrdr_app::StatusBarMode::None {
            s.hide()
        } else {
            s
        }
    });

    v_stack((
        transcript_view,
        todo_panel,
        completions,
        status_bar,
        input_row,
    ))
    .style(move |s| {
        s.width_full()
            .height_full()
            .background(theme_sig.get().bg)
            .color(theme_sig.get().assistant)
    })
}

// ---- event handling -----------------------------------------------------

fn push_item(transcript: RwSignal<Vec<Item>>, next_id: RwSignal<u64>, body: Body) {
    push_item_scoped(transcript, next_id, body, None);
}

/// Push an item whose signals live on `scope` (a per-item child scope, so the
/// signals can be disposed when the item is cleared).
fn push_item_scoped(
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    body: Body,
    scope: Option<Scope>,
) {
    let id = next_id.get_untracked();
    next_id.set(id + 1);
    // User/assistant items get the next message number (matching the shared
    // transcript numbering used by /copy msg, /find, /goto).
    let msg_no = matches!(body, Body::User(_) | Body::Assistant(_)).then(|| {
        transcript.with_untracked(|t| t.iter().filter(|i| i.msg_no.is_some()).count()) + 1
    });
    transcript.update(|t| {
        t.push(Item {
            id,
            body,
            scope,
            time: chrono::Local::now(),
            msg_no,
        })
    });
}

/// Clear the transcript and dispose each item's signal scope. Per-item signals
/// are created on child scopes so a long-lived window doesn't leak a few
/// signals per message across every `/clear` and `/resume`. Disposal happens
/// after the update: the views referencing the signals are torn down
/// synchronously by the clear, so they stay valid during teardown.
fn clear_items(transcript: RwSignal<Vec<Item>>) {
    let scopes: Vec<Scope> =
        transcript.with_untracked(|t| t.iter().filter_map(|i| i.scope).collect());
    transcript.update(|t| t.clear());
    for s in scopes {
        s.dispose();
    }
}

/// Push a system line into the transcript.
fn system(transcript: RwSignal<Vec<Item>>, next_id: RwSignal<u64>, msg: impl Into<String>) {
    push_item(transcript, next_id, Body::System(msg.into()));
}

/// Rebuild the display transcript from a restored message history (for
/// `/resume`). The entry construction is shared with the TUI
/// ([`hrdr_app::messages_to_entries`]); this only wraps each entry in the
/// GUI's reactive signals.
fn rebuild_transcript(
    cx: Scope,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    msgs: &[Message],
) {
    clear_items(transcript);
    next_id.set(0);
    for e in hrdr_app::messages_to_entries(msgs) {
        match e {
            hrdr_app::Entry::User(c) => push_item(transcript, next_id, Body::User(c)),
            hrdr_app::Entry::Assistant(c) => {
                let item_cx = cx.create_child();
                push_item_scoped(
                    transcript,
                    next_id,
                    Body::Assistant(Assistant {
                        reasoning: item_cx.create_rw_signal(String::new()),
                        text: item_cx.create_rw_signal(c),
                    }),
                    Some(item_cx),
                )
            }
            hrdr_app::Entry::Tool {
                id,
                name,
                args,
                result,
                ok,
                ..
            } => {
                let item_cx = cx.create_child();
                push_item_scoped(
                    transcript,
                    next_id,
                    Body::Tool(Tool {
                        call_id: id,
                        name,
                        args,
                        output: item_cx.create_rw_signal(String::new()),
                        result: item_cx.create_rw_signal(result),
                        ok: item_cx.create_rw_signal(ok),
                        done: item_cx.create_rw_signal(true),
                        collapsed: item_cx.create_rw_signal(true),
                    }),
                    Some(item_cx),
                )
            }
            _ => {}
        }
    }
}

/// The Nth (1-based) user/assistant message's text — numbering matches the
/// shared transcript queries (only user/assistant items count).
fn nth_message_text(transcript: RwSignal<Vec<Item>>, n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    transcript.with_untracked(|t| {
        t.iter()
            .filter_map(|i| match &i.body {
                Body::User(s) => Some(s.clone()),
                Body::Assistant(a) => Some(a.text.get_untracked()),
                _ => None,
            })
            .nth(n - 1)
    })
}

/// Text of the most recent assistant reply, if any (non-empty).
fn last_assistant_text(transcript: RwSignal<Vec<Item>>) -> Option<String> {
    transcript
        .with_untracked(|t| {
            t.iter().rev().find_map(|i| match &i.body {
                Body::Assistant(a) => Some(a.text.get_untracked()),
                _ => None,
            })
        })
        .filter(|s| !s.is_empty())
}

/// The transcript as plain text (user/assistant/system lines), for `/copy all`.
fn transcript_text(transcript: RwSignal<Vec<Item>>) -> String {
    transcript.with_untracked(|t| {
        let mut out = String::new();
        for item in t {
            match &item.body {
                Body::User(s) => out.push_str(&format!("## User\n{s}\n\n")),
                Body::Assistant(a) => {
                    out.push_str(&format!("## Assistant\n{}\n\n", a.text.get_untracked()))
                }
                Body::System(s) | Body::Error(s) => out.push_str(&format!("[{s}]\n\n")),
                Body::Diff(d) => out.push_str(&format!("{d}\n\n")),
                Body::Tool(t) => out.push_str(&format!("[tool: {}]\n\n", t.name)),
            }
        }
        out.trim_end().to_string()
    })
}

/// Desktop notification when a long turn finishes — the GUI's answer to the
/// TUI's terminal bell (`bell` config knob). Best-effort, off the UI thread.
fn notify_turn_done(failed: bool) {
    let body = if failed {
        "turn failed"
    } else {
        "turn finished"
    };
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("hrdr")
            .summary("hrdr")
            .body(body)
            .show();
    });
}

/// Write `text` to the OS clipboard, returning a status line for the transcript.
fn copy_to_clipboard(
    clipboard: &Rc<RefCell<Option<hjkl_clipboard::Clipboard>>>,
    text: &str,
    label: &str,
) -> String {
    hrdr_app::clipboard_copy_status(&mut clipboard.borrow_mut(), text, label)
}

/// Transcript search/jump state + helpers for the GUI-local `/find`, `/next`,
/// `/prev`, and `/goto` (frontend-coupled: they drive the scroll container via
/// the render-time message-number → ViewId registry). Mirrors the TUI's logic.
#[derive(Clone)]
struct FindCtx {
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    find_query: RwSignal<Option<String>>,
    find_pos: RwSignal<usize>,
    ids: Rc<RefCell<std::collections::HashMap<usize, floem::ViewId>>>,
    all_ids: Rc<RefCell<std::collections::HashMap<u64, floem::ViewId>>>,
    goto_view: RwSignal<Option<floem::ViewId>>,
}

impl FindCtx {
    fn info(&self, msg: impl Into<String>) {
        system(self.transcript, self.next_id, msg);
    }
    /// Number of user/assistant messages.
    fn message_count(&self) -> usize {
        self.transcript
            .with_untracked(|t| t.iter().filter(|i| i.msg_no.is_some()).count())
    }
    /// Message numbers whose user/assistant text contains `query`.
    fn hits(&self, query: &str) -> Vec<usize> {
        let q = query.to_ascii_lowercase();
        self.transcript.with_untracked(|t| {
            t.iter()
                .filter_map(|i| {
                    let n = i.msg_no?;
                    let text = match &i.body {
                        Body::User(s) => s.clone(),
                        Body::Assistant(a) => a.text.get_untracked(),
                        _ => return None,
                    };
                    text.to_ascii_lowercase().contains(&q).then_some(n)
                })
                .collect()
        })
    }
    /// Scroll to message number `n` via the render-time ViewId registry.
    fn scroll_to_msg(&self, n: usize) {
        let vid = self.ids.borrow().get(&n).copied();
        if let Some(v) = vid {
            self.goto_view.set(Some(v));
        }
    }
    /// Scroll to the very bottom of the transcript (`/goto end`) — the
    /// highest-id rendered item.
    fn scroll_bottom(&self) {
        let vid = self
            .all_ids
            .borrow()
            .iter()
            .max_by_key(|(id, _)| **id)
            .map(|(_, v)| *v);
        if let Some(v) = vid {
            self.goto_view.set(Some(v));
        }
    }
    /// Route a resolved find/goto action to the GUI's scroll primitives (the
    /// parsing/cycling/status lines live in the shared core).
    fn apply(&self, act: hrdr_app::FindAction) {
        match act {
            hrdr_app::FindAction::Info(line) => self.info(line),
            hrdr_app::FindAction::Jump { msg, line } => {
                self.scroll_to_msg(msg);
                self.info(line);
            }
            hrdr_app::FindAction::Bottom { line } => {
                self.scroll_bottom();
                self.info(line);
            }
        }
    }
    /// The shared find state, mirrored out of the signals for one operation.
    fn state(&self) -> hrdr_app::FindState {
        hrdr_app::FindState {
            query: self.find_query.get_untracked(),
            pos: self.find_pos.get_untracked(),
        }
    }
    fn store(&self, st: hrdr_app::FindState) {
        self.find_query.set(st.query);
        self.find_pos.set(st.pos);
    }
    /// `/goto <N | 5m | 1h | top | end>` (shared [`hrdr_app::goto_action`]).
    fn goto_cmd(&self, arg: &str) {
        let act = hrdr_app::goto_action(arg, self.message_count(), |cutoff| {
            self.transcript.with_untracked(|t| {
                t.iter()
                    .find(|i| i.msg_no.is_some() && i.time >= cutoff)
                    .and_then(|i| i.msg_no)
            })
        });
        self.apply(act);
    }
    /// `/find <text>` — search + jump; no arg cycles; `clear` drops the search.
    fn find_cmd(&self, arg: &str) {
        let mut st = self.state();
        let act = st.find(arg, |q| self.hits(q));
        self.store(st);
        self.apply(act);
    }
    /// `/next` / `/prev` — cycle matches of the active query, wrapping.
    fn cycle(&self, forward: bool) {
        let mut st = self.state();
        let act = st.cycle(forward, |q| self.hits(q));
        self.store(st);
        self.apply(act);
    }
}

/// The GUI's [`hrdr_app::CommandHost`] — the capability surface the shared
/// slash-command dispatcher drives. Holds clones of the reactive signals +
/// agent handle + clipboard so the shared commands can mutate GUI state.
struct GuiHost {
    cx: Scope,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    usage: RwSignal<Option<(u32, u32)>>,
    model: RwSignal<String>,
    input: RwSignal<String>,
    effort: RwSignal<Option<String>>,
    running: RwSignal<bool>,
    file_index: RwSignal<Vec<String>>,
    file_index_state: RwSignal<u8>,
    session_id: RwSignal<Option<String>>,
    session_label: RwSignal<Option<String>>,
    save_gen: RwSignal<u64>,
    turn_start: RwSignal<Option<Instant>>,
    ttft: RwSignal<Option<f64>>,
    show_reasoning: RwSignal<bool>,
    theme: RwSignal<GuiTheme>,
    theme_rev: RwSignal<u64>,
    timestamp_style: RwSignal<hrdr_app::TimestampStyle>,
    statusbar_mode: RwSignal<hrdr_app::StatusBarMode>,
    dir: RwSignal<String>,
    branch: RwSignal<Option<String>>,
    session_in: RwSignal<usize>,
    session_out: RwSignal<usize>,
    todo_ttl: RwSignal<u64>,
    todos: Arc<std::sync::Mutex<Vec<hrdr_tools::TodoItem>>>,
    todos_sig: RwSignal<Vec<hrdr_tools::TodoItem>>,
    todo_turn: RwSignal<u64>,
    todo_completed_at: Rc<RefCell<std::collections::HashMap<String, u64>>>,
    expand_all: RwSignal<bool>,
    bell: RwSignal<bool>,
    auto_compact_ratio: RwSignal<f64>,
    compaction_reserved: RwSignal<u32>,
    find_query: RwSignal<Option<String>>,
    find_pos: RwSignal<usize>,
    ctx_window: RwSignal<Option<u32>>,
    cfg: Rc<AgentConfig>,
    queue: Rc<RefCell<VecDeque<String>>>,
    turn_handle: Rc<RefCell<Option<tokio::task::JoinHandle<()>>>>,
    spawn_turn: Rc<dyn Fn(String, bool)>,
    start_compaction: Rc<dyn Fn(Option<String>)>,
    compacting: RwSignal<bool>,
    pending_init: RwSignal<bool>,
    config_mtime_seen: Rc<std::cell::Cell<Option<std::time::SystemTime>>>,
    clipboard: Rc<RefCell<Option<hjkl_clipboard::Clipboard>>>,
    agent: Arc<TokioMutex<Agent>>,
    tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,
    base_url: RwSignal<String>,
    login: RwSignal<Option<hrdr_app::LoginWizard>>,
}

impl hrdr_app::CommandHost for GuiHost {
    fn info(&mut self, line: String) {
        system(self.transcript, self.next_id, line);
    }
    fn line_poster(&self) -> Box<dyn Fn(hrdr_app::LineKind, String) + Send> {
        let tx = self.tx.clone();
        Box::new(move |kind, line| {
            let msg = match kind {
                hrdr_app::LineKind::Diff => UiMsg::Diff(line),
                hrdr_app::LineKind::System => UiMsg::System(line),
            };
            let _ = tx.send(msg);
        })
    }
    fn agent(&self) -> Arc<TokioMutex<Agent>> {
        self.agent.clone()
    }
    fn cwd(&self) -> std::path::PathBuf {
        hrdr_app::agent_cwd(&self.agent)
    }
    fn base_url(&self) -> String {
        self.base_url.get_untracked()
    }
    fn model(&self) -> String {
        self.model.get_untracked()
    }
    fn set_model(&mut self, model: String) {
        self.model.set(model);
    }
    fn show_thinking(&self) -> bool {
        self.show_reasoning.get_untracked()
    }
    fn set_show_thinking(&mut self, on: bool) {
        self.show_reasoning.set(on);
        // Persist like the TUI does, so the setting survives a restart.
        let _ = hrdr_agent::persist_setting("show_thinking", hrdr_agent::ConfigValue::Bool(on));
    }
    fn clear_conversation(&mut self) {
        // Cancel a running turn first (its autosave would otherwise write the
        // old history to a fresh session) and drop queued follow-ups.
        if self.running.get_untracked() {
            if let Some(h) = self.turn_handle.borrow_mut().take() {
                h.abort();
            }
            self.running.set(false);
        }
        self.compacting.set(false);
        self.login.set(None); // cancel an in-progress /login wizard
        self.queue.borrow_mut().clear();
        if let Ok(mut t) = self.todos.lock() {
            t.clear();
        }
        self.todos_sig.set(Vec::new());
        self.todo_turn.set(0);
        self.todo_completed_at.borrow_mut().clear();
        self.session_in.set(0);
        self.session_out.set(0);
        self.find_query.set(None);
        self.find_pos.set(0);
        clear_items(self.transcript);
        self.next_id.set(0);
        self.usage.set(None);
        self.turn_start.set(None);
        self.ttft.set(None); // don't show the previous session's ttft
        self.session_id.set(None); // detach; the next turn starts a new session
        self.session_label.set(None);
        // Invalidate any in-flight turn's pending auto-save (see UiMsg::Saved).
        self.save_gen.update(|g| *g += 1);
        // Clear synchronously when the lock is free (always, when no turn is
        // running) so an immediately-following send can't win the agent lock
        // before the clear; fall back to a task otherwise.
        if let Ok(mut a) = self.agent.try_lock() {
            a.clear();
        } else {
            let agent = self.agent.clone();
            tokio::spawn(async move { agent.lock().await.clear() });
        }
    }
    fn session_id(&self) -> Option<String> {
        self.session_id.get_untracked()
    }
    fn set_session_label(&mut self, name: String) {
        self.session_label.set(Some(name));
    }
    fn autosave(&mut self) {
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let existing = self.session_id.get_untracked();
        let label = self.session_label.get_untracked();
        let model = self.model.get_untracked();
        let base_url = self.base_url.get_untracked();
        let generation = self.save_gen.get_untracked();
        tokio::spawn(async move {
            if let Some(o) =
                hrdr_app::save_agent_session(agent, existing, label, model, base_url).await
            {
                let _ = tx.send(UiMsg::Saved {
                    id: o.id,
                    first_save: o.first_save,
                    generation,
                });
            }
        });
    }
    fn resume(&mut self, id: String, session: Session) {
        let plan = hrdr_app::resume_plan(&session, &self.cwd(), &self.base_url.get_untracked());
        self.model.set(session.model.clone());
        self.session_id.set(Some(id));
        self.session_label.set(Some(session.name.clone()));
        // Pending saves from before the resume belong to the old conversation.
        self.save_gen.update(|g| *g += 1);
        rebuild_transcript(self.cx, self.transcript, self.next_id, &session.messages);
        // Synchronous when the lock is free (see clear_conversation) so a
        // send right after /resume can't race the message swap.
        let msgs = session.messages;
        let m = session.model;
        let cwd_for_agent = plan.new_cwd.clone();
        let apply = move |a: &mut Agent| {
            a.set_messages(msgs);
            a.set_model(m);
            if let Some(c) = cwd_for_agent {
                a.set_cwd(c);
            }
        };
        if let Ok(mut a) = self.agent.try_lock() {
            apply(&mut a);
        } else {
            let agent = self.agent.clone();
            tokio::spawn(async move { apply(&mut *agent.lock().await) });
        }
        if let Some(c) = &plan.new_cwd {
            // Refresh dir/branch chrome + invalidate the @file index, exactly
            // like /cwd (the TUI's apply_cwd does the same).
            let c = c.clone();
            hrdr_app::CommandHost::cwd_changed(self, &c);
        }
        for line in plan.lines {
            system(self.transcript, self.next_id, line);
        }
    }
    fn copy_to_clipboard(&mut self, text: &str, label: &str) -> String {
        copy_to_clipboard(&self.clipboard, text, label)
    }
    fn last_reply(&self) -> Option<String> {
        last_assistant_text(self.transcript)
    }
    fn transcript_text(&self) -> String {
        transcript_text(self.transcript)
    }
    fn nth_message_text(&self, n: usize) -> Option<String> {
        nth_message_text(self.transcript, n)
    }
    fn is_busy(&self) -> bool {
        self.running.get_untracked()
    }
    fn send_prompt(&mut self, prompt: String, show_as_user: bool) {
        (self.spawn_turn)(prompt, show_as_user);
    }
    fn set_input(&mut self, text: String) {
        self.input.set(text);
    }
    fn prepend_input(&mut self, text: String) {
        self.input.update(|s| *s = format!("{text}{s}"));
    }
    fn insert_input(&mut self, text: String) {
        self.input.update(|s| s.push_str(&text));
    }
    fn read_clipboard(&self) -> Option<String> {
        hrdr_app::clipboard_read_text(&self.clipboard.borrow())
    }
    fn set_tool_expansion(&mut self, mode: hrdr_app::ExpandMode) -> String {
        let tools: Vec<Tool> = self.transcript.with_untracked(|t| {
            t.iter()
                .filter_map(|i| match &i.body {
                    Body::Tool(tool) => Some(tool.clone()),
                    _ => None,
                })
                .collect()
        });
        match mode {
            hrdr_app::ExpandMode::All => {
                // Sticky (as in the TUI): existing tools expand now, new
                // tools spawn expanded until `/expand off`.
                self.expand_all.set(true);
                for t in &tools {
                    t.collapsed.set(false);
                }
                hrdr_app::expand_msg::ALL.to_string()
            }
            hrdr_app::ExpandMode::Off => {
                self.expand_all.set(false);
                for t in &tools {
                    t.collapsed.set(true);
                }
                hrdr_app::expand_msg::OFF.to_string()
            }
            hrdr_app::ExpandMode::ToggleLast => match tools.last() {
                Some(t) => {
                    let now = !t.collapsed.get_untracked();
                    t.collapsed.set(now);
                    if now {
                        hrdr_app::expand_msg::LAST_COLLAPSED.to_string()
                    } else {
                        hrdr_app::expand_msg::LAST_EXPANDED.to_string()
                    }
                }
                None => hrdr_app::expand_msg::NONE.to_string(),
            },
        }
    }
    fn start_compaction(&mut self, instructions: Option<String>) {
        (self.start_compaction)(instructions);
    }
    fn mark_init_turn(&mut self) {
        self.pending_init.set(true);
    }
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        // Suppress the hot-reload bounce from our own write (mtime guard).
        let _ = hrdr_agent::persist_setting(key, value);
        self.config_mtime_seen.set(hrdr_app::config_mtime());
    }
    fn unpersist_setting(&mut self, key: &str) {
        let _ = hrdr_agent::remove_setting(key);
        self.config_mtime_seen.set(hrdr_app::config_mtime());
    }
    fn rewind_last_turn(&mut self) -> Option<String> {
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user())?;
        // Drop display items from the last user message on, disposing their
        // signal scopes (mirrors clear_items).
        let idx = self
            .transcript
            .with_untracked(|t| t.iter().rposition(|i| matches!(i.body, Body::User(_))));
        if let Some(idx) = idx {
            let scopes: Vec<Scope> = self
                .transcript
                .with_untracked(|t| t[idx..].iter().filter_map(|i| i.scope).collect());
            self.transcript.update(|t| t.truncate(idx));
            for sc in scopes {
                sc.dispose();
            }
        }
        Some(text)
    }
    fn effort(&self) -> Option<String> {
        self.effort.get_untracked()
    }
    fn session_label(&self) -> Option<String> {
        self.session_label.get_untracked()
    }
    fn context_usage(&self) -> Option<(u32, u32)> {
        self.usage.get_untracked()
    }
    fn context_window(&self) -> Option<u32> {
        self.ctx_window.get_untracked()
    }
    fn session_tokens(&self) -> (usize, usize) {
        (
            self.session_in.get_untracked(),
            self.session_out.get_untracked(),
        )
    }
    fn set_effort(&mut self, label: String) {
        self.effort.set(Some(label));
    }
    fn cwd_changed(&mut self, new: &std::path::Path) {
        self.dir.set(hrdr_app::display_dir(new));
        self.branch.set(hrdr_app::git_branch(new));
        // Rebuilt lazily (off-thread) on the next `@` mention.
        self.file_index.set(Vec::new());
        self.file_index_state.set(0);
    }
    fn files_changed(&mut self) {
        self.file_index.set(Vec::new());
        self.file_index_state.set(0);
    }
    fn timestamp_style(&self) -> hrdr_app::TimestampStyle {
        self.timestamp_style.get_untracked()
    }
    fn statusbar_mode(&self) -> hrdr_app::StatusBarMode {
        self.statusbar_mode.get_untracked()
    }
    fn set_statusbar_mode(&mut self, mode: hrdr_app::StatusBarMode) {
        self.statusbar_mode.set(mode);
    }
    fn set_theme(&mut self, path: Option<String>) {
        self.theme.set(GuiTheme::load(path.as_deref()));
        // Rebuild cached dyn_stack children (their colors are captured at
        // build time — the revision is part of their keys).
        self.theme_rev.update(|r| *r += 1);
    }
    fn set_timestamp_style(&mut self, style: hrdr_app::TimestampStyle) {
        self.timestamp_style.set(style);
    }
    fn todo_ttl(&self) -> u64 {
        self.todo_ttl.get_untracked()
    }
    fn set_todo_ttl(&mut self, turns: u64) {
        self.todo_ttl.set(turns);
    }
    fn reload_config(&mut self) {
        // Same application path as the hot-reload; the manual command also
        // refreshes AGENTS.md like the TUI's /reload.
        let line = apply_config_reload(
            true,
            &self.agent,
            self.show_reasoning,
            self.theme,
            self.theme_rev,
            self.timestamp_style,
            self.statusbar_mode,
            self.todo_ttl,
            self.bell,
            self.effort,
            self.auto_compact_ratio,
            self.compaction_reserved,
        );
        self.config_mtime_seen.set(hrdr_app::config_mtime());
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(line) = hrdr_app::reload_project_docs(agent).await {
                let _ = tx.send(UiMsg::System(line));
            }
        });
        system(self.transcript, self.next_id, line);
    }
    fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
        self.cfg.resolve_provider(name)
    }
    fn set_base_url(&mut self, url: String) {
        self.base_url.set(url);
    }
    fn set_context_window(&mut self, tokens: Option<u32>) {
        if tokens.is_some() {
            self.ctx_window.set(tokens);
        }
    }
    fn begin_login(&mut self) {
        let wizard = hrdr_app::LoginWizard::start(self);
        self.login.set(Some(wizard));
    }
}

/// The assistant item currently being streamed into — the last item if it's an
/// assistant turn, otherwise a fresh one (a tool call ends the prior segment).
fn current_assistant(
    cx: Scope,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
) -> Assistant {
    let existing = transcript.with_untracked(|t| match t.last().map(|i| &i.body) {
        Some(Body::Assistant(a)) => Some(a.clone()),
        _ => None,
    });
    if let Some(a) = existing {
        return a;
    }
    let item_cx = cx.create_child();
    let a = Assistant {
        reasoning: item_cx.create_rw_signal(String::new()),
        text: item_cx.create_rw_signal(String::new()),
    };
    push_item_scoped(
        transcript,
        next_id,
        Body::Assistant(a.clone()),
        Some(item_cx),
    );
    a
}

fn find_tool(transcript: RwSignal<Vec<Item>>, call_id: &str) -> Option<Tool> {
    // Newest-first and live-only: backends that restart ids each turn
    // (`call_0`, `call_1`, …) must not update a finished tool from an earlier
    // turn (which would also leave the new tool spinning forever).
    transcript.with_untracked(|t| {
        t.iter().rev().find_map(|i| match &i.body {
            Body::Tool(tool) if tool.call_id == call_id && !tool.done.get_untracked() => {
                Some(tool.clone())
            }
            _ => None,
        })
    })
}

fn handle_event(
    cx: Scope,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    usage: RwSignal<Option<(u32, u32)>>,
    expand_all: RwSignal<bool>,
    ev: AgentEvent,
) {
    match ev {
        AgentEvent::Reasoning(r) => {
            current_assistant(cx, transcript, next_id)
                .reasoning
                .update(|s| s.push_str(&r));
        }
        AgentEvent::Text(t) => {
            current_assistant(cx, transcript, next_id)
                .text
                .update(|s| s.push_str(&t));
        }
        AgentEvent::ToolStart { id, name, args } => {
            let item_cx = cx.create_child();
            let tool = Tool {
                call_id: id,
                name,
                args,
                output: item_cx.create_rw_signal(String::new()),
                result: item_cx.create_rw_signal(String::new()),
                ok: item_cx.create_rw_signal(true),
                done: item_cx.create_rw_signal(false),
                // Sticky `/expand all` applies to new tools too (as in the TUI).
                collapsed: item_cx.create_rw_signal(!expand_all.get_untracked()),
            };
            push_item_scoped(transcript, next_id, Body::Tool(tool), Some(item_cx));
        }
        AgentEvent::ToolOutput { id, chunk } => {
            if let Some(tool) = find_tool(transcript, &id) {
                tool.output.update(|s| s.push_str(&chunk));
            }
        }
        AgentEvent::ToolEnd { id, result, ok, .. } => {
            if let Some(tool) = find_tool(transcript, &id) {
                tool.result.set(result);
                tool.ok.set(ok);
                tool.done.set(true);
            }
        }
        AgentEvent::Notice(s) => push_item(transcript, next_id, Body::System(s)),
        AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } => usage.set(Some((prompt_tokens, completion_tokens))),
        AgentEvent::TurnDone => {}
    }
}

// ---- rendering ----------------------------------------------------------

fn render_item(
    item: Item,
    th: GuiTheme,
    show_reasoning: RwSignal<bool>,
    timestamp_style: RwSignal<hrdr_app::TimestampStyle>,
) -> AnyView {
    // "#N role · time" header for user/assistant items, per /timestamps style.
    let meta = |role: &'static str, accent: Color| {
        let (n, time) = (item.msg_no.unwrap_or(0), item.time);
        label(move || {
            use hrdr_app::TimestampStyle;
            match timestamp_style.get() {
                TimestampStyle::None => format!("#{n} {role}"),
                TimestampStyle::Relative => {
                    format!("#{n} {role} · {}", hrdr_app::relative_time(time))
                }
                TimestampStyle::Exact => format!("#{n} {role} · {}", time.format("%H:%M")),
            }
        })
        .style(move |s| s.color(accent).font_bold().margin_bottom(2.0))
    };
    match item.body.clone() {
        Body::User(text) => v_stack((meta("you", th.user), text_label(text))).into_any(),
        Body::Assistant(a) => v_stack((
            meta("assistant", th.dim),
            // Reasoning (dim); hidden when `/reasoning` is off or none streamed.
            label(move || a.reasoning.get()).style(move |s| {
                if show_reasoning.get() && !a.reasoning.get().is_empty() {
                    s.color(th.dim).margin_bottom(2.0)
                } else {
                    s.hide()
                }
            }),
            // Assistant text rendered as markdown (headings, emphasis, lists,
            // and syntax-highlighted code blocks). Keyed per block so streaming
            // only re-renders the changed (tail) block, not the whole reply.
            md::markdown_stack(a.text, th),
        ))
        .into_any(),
        Body::Tool(t) => {
            let name = t.name.clone();
            let args = hrdr_tools::truncate_inline(&t.args, hrdr_app::TOOL_ARGS_PREVIEW);
            let (output, result, ok, collapsed) = (t.output, t.result, t.ok, t.collapsed);
            v_stack((
                // Header — caret reflects the output collapse.
                label(move || {
                    let caret = if collapsed.get() { "▸" } else { "▾" };
                    format!("{caret} ⚙ {name} {args}")
                })
                .style(move |s| s.color(th.tool).font_bold()),
                // Streamed output — hidden while collapsed.
                label(move || output.get()).style(move |s| {
                    if collapsed.get() {
                        s.hide()
                    } else {
                        s.color(th.dim)
                    }
                }),
                // Result is always shown, colored by pass/fail.
                label(move || result.get())
                    .style(move |s| s.color(if ok.get() { th.ok } else { th.err })),
            ))
            // The whole block toggles expansion (like the TUI's click target,
            // and a far bigger target than the old header-only click).
            .on_click_stop(move |_| collapsed.update(|c| *c = !*c))
            .style(|s| {
                s.padding(6.0)
                    .gap(2.0)
                    .border_radius(4.0)
                    .hover(|s| s.background(md::tool_hover_bg()))
            })
            .into_any()
        }
        Body::System(s) => text_label(s).style(move |st| st.color(th.dim)).into_any(),
        Body::Error(s) => text_label(s).style(move |st| st.color(th.err)).into_any(),
        Body::Diff(d) => md::diff_view(&d, th),
    }
}

/// A plain (non-reactive) text label.
fn text_label(s: String) -> impl IntoView {
    label(move || s.clone())
}

/// Re-load config and apply the live-changeable settings — the one code path
/// behind both `/reload` and the hot-reload (mirrors the TUI's
/// `apply_config_reload`). On an invalid file, keeps the current settings.
/// Returns the status line to show.
#[allow(clippy::too_many_arguments)]
fn apply_config_reload(
    manual: bool,
    agent: &Arc<TokioMutex<Agent>>,
    show_reasoning: RwSignal<bool>,
    theme_sig: RwSignal<GuiTheme>,
    theme_rev: RwSignal<u64>,
    timestamp_style: RwSignal<hrdr_app::TimestampStyle>,
    statusbar_mode: RwSignal<hrdr_app::StatusBarMode>,
    todo_ttl: RwSignal<u64>,
    bell: RwSignal<bool>,
    effort: RwSignal<Option<String>>,
    auto_compact_ratio: RwSignal<f64>,
    compaction_reserved: RwSignal<u32>,
) -> String {
    match AgentConfig::load_checked() {
        Ok(cfg) => {
            apply_ui_config(
                &hrdr_app::UiConfig::load(),
                show_reasoning,
                theme_sig,
                theme_rev,
                timestamp_style,
                statusbar_mode,
                todo_ttl,
                bell,
            );
            effort.set(cfg.effort.clone());
            auto_compact_ratio.set(cfg.auto_compact);
            compaction_reserved.set(cfg.compaction_reserved);
            if let (Some(t), Ok(mut a)) = (cfg.temperature, agent.try_lock()) {
                a.set_temperature(Some(t));
            }
            if manual {
                hrdr_app::RELOAD_MANUAL_MSG.to_string()
            } else {
                hrdr_app::RELOAD_HOT_MSG.to_string()
            }
        }
        Err(e) => hrdr_app::reload_invalid_message(&e),
    }
}

/// Apply the live-changeable display settings from a (re)loaded [`UiConfig`]
/// — the display half of [`apply_config_reload`].
#[allow(clippy::too_many_arguments)]
fn apply_ui_config(
    ui: &hrdr_app::UiConfig,
    show_reasoning: RwSignal<bool>,
    theme_sig: RwSignal<GuiTheme>,
    theme_rev: RwSignal<u64>,
    timestamp_style: RwSignal<hrdr_app::TimestampStyle>,
    statusbar_mode: RwSignal<hrdr_app::StatusBarMode>,
    todo_ttl: RwSignal<u64>,
    bell: RwSignal<bool>,
) {
    show_reasoning.set(ui.show_thinking);
    bell.set(ui.bell);
    theme_sig.set(GuiTheme::load(ui.theme.as_deref()));
    theme_rev.update(|r| *r += 1);
    timestamp_style.set(hrdr_app::TimestampStyle::from_config(
        ui.timestamps.as_deref(),
    ));
    statusbar_mode.set(hrdr_app::StatusBarMode::from_config(
        ui.statusbar.as_deref(),
    ));
    todo_ttl.set(ui.todo_ttl);
}

/// The context gauge as a real progress bar: a rounded track with a fill
/// layer whose width is the used fraction, and the shared label on top.
fn ctx_gauge_view(gauge: hrdr_app::CtxGauge, th: GuiTheme) -> AnyView {
    use floem::views::{empty, stack};
    let fill_color = slot_color(hrdr_app::ctx_level_slot(gauge.level), th);
    let frac = gauge.frac.clamp(0.0, 1.0) * 100.0;
    let label_text = gauge.label.trim().to_string();
    stack((
        // Fill layer (drawn first = behind the label).
        empty().style(move |s| {
            s.absolute()
                .inset_left(0.0)
                .inset_top(0.0)
                .height_full()
                .width_pct(frac)
                .background(fill_color)
                .border_radius(4.0)
        }),
        label(move || label_text.clone())
            .style(move |s| s.color(th.assistant).padding_horiz(8.0).padding_vert(1.0)),
    ))
    .style(move |s| s.background(th.dim).border_radius(4.0).items_center())
    .into_any()
}

/// Resolve a shared theme slot to this theme's concrete color.
fn slot_color(slot: hrdr_app::ThemeSlot, th: GuiTheme) -> Color {
    use hrdr_app::ThemeSlot;
    match slot {
        ThemeSlot::User => th.user,
        ThemeSlot::Assistant => th.assistant,
        ThemeSlot::Dim => th.dim,
        ThemeSlot::Warn => th.tool,
        ThemeSlot::Success => th.ok,
        ThemeSlot::Error => th.err,
        ThemeSlot::Accent => th.accent,
        ThemeSlot::Accent2 => th.accent2,
    }
}

/// Map a shared status color role onto the GUI theme (semantics live in
/// [`hrdr_app::status_role_style`]; only slot → color is local).
fn status_run_style(
    s: floem::style::Style,
    role: hrdr_app::StatusRole,
    th: GuiTheme,
) -> floem::style::Style {
    let spec = hrdr_app::status_role_style(role);
    let mut s = s.color(match spec.fg {
        Some(slot) => slot_color(slot, th),
        None => Color::BLACK, // inverted text over the bg slot
    });
    if let Some(bg) = spec.bg {
        s = s.background(slot_color(bg, th));
    }
    if spec.bold {
        s = s.font_bold();
    }
    s
}
