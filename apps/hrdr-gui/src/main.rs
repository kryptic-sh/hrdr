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
use floem::keyboard::{Key, NamedKey};
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
    let theme = GuiTheme::load(ui.theme.as_deref());
    let show_thinking = ui.show_thinking;
    // Persistent scope for dynamically-created per-message signals, so they
    // outlive the effect that creates them.
    let cx = Scope::current();
    let transcript: RwSignal<Vec<Item>> = create_rw_signal(Vec::new());
    let input = create_rw_signal(String::new());
    let running = create_rw_signal(false);
    let next_id = create_rw_signal(0u64);
    // Model is a signal so `/model <name>` can switch it and the status bar
    // reflects the change live.
    let model = create_rw_signal(model);
    // Cached relative file paths under the cwd, for `@file` completion. Built
    // lazily the first time an `@` mention is typed (see the effect below).
    let file_index: RwSignal<Vec<String>> = create_rw_signal(Vec::new());
    // Populate the file index once, when the user starts an `@file` mention
    // (re-populated after /cwd or /revert empty it). Paths come from the
    // agent's cwd, which follows /cwd and resumed sessions.
    let agent_for_index = agent.clone();
    create_effect(move |_| {
        if input.get().contains('@') && file_index.with_untracked(Vec::is_empty) {
            let cwd = agent_for_index
                .try_lock()
                .map(|a| a.cwd())
                .ok()
                .or_else(|| std::env::current_dir().ok());
            if let Some(cwd) = cwd {
                file_index.set(hrdr_app::walk_files(&cwd));
            }
        }
    });
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
    let start_cwd = agent
        .try_lock()
        .map(|a| a.cwd())
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
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
    let goto_view: RwSignal<Option<floem::ViewId>> = create_rw_signal(None);
    // Messages submitted while a turn runs, sent FIFO as turns finish (like
    // the TUI's queue).
    let queue: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));

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
            // Expand `@file` mentions for the model only; the transcript keeps
            // the bare `@path` the user typed (same split as the TUI). Paths
            // resolve against the agent's cwd (it follows /cwd and /resume).
            let mention_cwd = agent
                .try_lock()
                .map(|a| a.cwd())
                .ok()
                .or_else(|| std::env::current_dir().ok());
            let sent = match mention_cwd {
                Some(cwd) => hrdr_app::expand_mentions(&text, &cwd),
                None => text,
            };
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

    let spawn_for_done = spawn_turn.clone();
    let queue_for_done = queue.clone();
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
                handle_event(cx, transcript, next_id, usage, ev);
                if refresh_todos && let Ok(t) = todos_for_events.lock() {
                    todos_sig.set(t.clone());
                }
            }
            UiMsg::Done(err) => {
                running.set(false);
                if let Some(e) = err {
                    push_item(transcript, next_id, Body::Error(e));
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
                // Start the next queued message, if any (FIFO, like the TUI).
                let next = queue_for_done.borrow_mut().pop_front();
                if let Some(next) = next {
                    (spawn_for_done)(next, true);
                }
            }
            UiMsg::System(s) => push_item(transcript, next_id, Body::System(s)),
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
                    system(
                        transcript,
                        next_id,
                        format!("session saved as '{id}' — /resume {id}"),
                    );
                }
                session_id.set(Some(id));
            }
        }
    });

    let history_for_send = history.clone();
    let msg_ids_for_send = msg_view_ids.clone();
    let spawn_for_send = spawn_turn.clone();
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
                session_id,
                session_label,
                save_gen,
                turn_start,
                ttft,
                show_reasoning,
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
                find_query,
                find_pos,
                ctx_window,
                cfg: cfg_for_send.clone(),
                queue: queue_for_send.clone(),
                turn_handle: th_for_host.clone(),
                spawn_turn: spawn_for_send.clone(),
                clipboard: clipboard_for_send.clone(),
                agent: agent_for_send.clone(),
                tx: tx_for_send.clone(),
                base_url,
            };
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
        let dropped = {
            let mut q = queue_for_cancel.borrow_mut();
            let n = q.len();
            q.clear();
            n
        };
        let msg = if dropped > 0 {
            format!("[cancelled · {dropped} queued message(s) discarded]")
        } else {
            "[cancelled]".to_string()
        };
        system(transcript, next_id, msg);
    };

    let send_enter = send.clone();
    let send_btn = send.clone();
    let cancel_esc = cancel.clone();
    let cancel_btn = cancel.clone();

    let ids_for_render = msg_view_ids.clone();
    let transcript_view = scroll(
        dyn_stack(
            move || transcript.get(),
            |item: &Item| item.id,
            move |item| {
                let view = render_item(item.clone(), theme, show_reasoning, timestamp_style);
                // Register user/assistant views so /goto //find can scroll to
                // them (re-registered whenever the item re-renders).
                if let Some(n) = item.msg_no {
                    ids_for_render.borrow_mut().insert(n, view.id());
                }
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
        move || todos_sig.get(),
        |t: &hrdr_tools::TodoItem| format!("{}:{}", t.status, t.content),
        move |t| {
            let (glyph, color) = match t.status.as_str() {
                "completed" => ("✓", theme.ok),
                "in_progress" => ("▸", theme.tool),
                _ => ("·", theme.dim),
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
    let input_box = text_input(input)
        .placeholder("Message hrdr…  (Enter to send)")
        .on_key_down(
            Key::Named(NamedKey::Enter),
            |m| m.is_empty(),
            move |_| send_enter(),
        )
        // Up/Down recall previous submissions (like the TUI's history).
        .on_key_down(
            Key::Named(NamedKey::ArrowUp),
            |m| m.is_empty(),
            move |_| {
                let current = input.get_untracked();
                if let Some(text) = hist_up.borrow_mut().recall_prev(&current) {
                    input.set(text);
                }
            },
        )
        .on_key_down(
            Key::Named(NamedKey::ArrowDown),
            |m| m.is_empty(),
            move |_| {
                if let Some(text) = hist_down.borrow_mut().recall_next() {
                    input.set(text);
                }
            },
        )
        // Esc cancels the in-flight turn (otherwise unused in the single-line input).
        .on_key_down(
            Key::Named(NamedKey::Escape),
            |_| true,
            move |_| cancel_esc(),
        )
        .style(|s| s.flex_grow(1.0).padding(8.0));

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
            label(move || name.to_string()).style(move |s| s.color(theme.user).font_bold()),
            label(move || desc.to_string()).style(move |s| s.color(theme.dim)),
        ))
        .style(|s| s.gap(8.0).padding_horiz(10.0).padding_vert(2.0))
        .on_click_stop(move |_| input.set(format!("{name} ")))
        .into_any(),
        CompRow::File { start, path } => label({
            let path = path.clone();
            move || path.clone()
        })
        .style(move |s| s.color(theme.user).padding_horiz(10.0).padding_vert(2.0))
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
            .background(theme.bg);
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
    let auto_compact_ratio = cfg.auto_compact;
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
            auto_compact_ratio,
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
        status_segs,
        |(i, seg): &(usize, hrdr_app::StatusSeg)| {
            let text: String = seg.runs.iter().map(|r| r.text.as_str()).collect();
            format!("{i}:{text}")
        },
        move |(i, seg)| {
            let mut children: Vec<AnyView> = Vec::new();
            if i > 0 {
                children.push(
                    label(|| "│")
                        .style(move |s| s.color(theme.dim).margin_horiz(6.0))
                        .into_any(),
                );
            }
            if let Some(gauge) = seg.gauge {
                // The context gauge as a real progress container: a fill layer
                // sized by the fraction behind the label (instead of the
                // character-cell split the TUI's text runs use).
                children.push(ctx_gauge_view(gauge, theme));
            } else {
                for run in seg.runs {
                    let text = run.text.clone();
                    let role = run.role;
                    children.push(
                        label(move || text.clone())
                            .style(move |s| status_run_style(s, role, theme))
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
                s.color(theme.tool)
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
            .background(theme.bg)
            .color(theme.assistant)
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
                Body::Tool(t) => out.push_str(&format!("[tool: {}]\n\n", t.name)),
            }
        }
        out.trim_end().to_string()
    })
}

/// Write `text` to the OS clipboard, returning a status line for the transcript.
fn copy_to_clipboard(
    clipboard: &Rc<RefCell<Option<hjkl_clipboard::Clipboard>>>,
    text: &str,
    label: &str,
) -> String {
    use hjkl_clipboard::{MimeType, Selection};
    let res = clipboard
        .borrow_mut()
        .as_mut()
        .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()));
    match res {
        Some(Ok(())) => format!("copied {label} to clipboard"),
        Some(Err(_)) => "clipboard write failed".to_string(),
        None => "clipboard unavailable".to_string(),
    }
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
    /// `/goto <N | 5m | 1h | top | end>`.
    fn goto_cmd(&self, arg: &str) {
        let count = self.message_count();
        if count == 0 {
            self.info("no messages to jump to yet");
            return;
        }
        let a = arg.trim().to_ascii_lowercase();
        let target = match a.as_str() {
            "" => {
                self.info("usage: /goto <N | 5m | 1h | top | end>");
                return;
            }
            "top" | "start" | "first" => 1,
            "end" | "bottom" | "last" => count,
            _ => {
                if let Ok(n) = a.parse::<usize>() {
                    n.clamp(1, count)
                } else if let Some(secs) = hrdr_app::parse_duration(&a) {
                    let cutoff = chrono::Local::now() - chrono::Duration::seconds(secs);
                    // First message at/after the cutoff; all older → the newest.
                    self.transcript
                        .with_untracked(|t| {
                            t.iter()
                                .find(|i| i.msg_no.is_some() && i.time >= cutoff)
                                .and_then(|i| i.msg_no)
                        })
                        .unwrap_or(count)
                } else {
                    self.info("usage: /goto <N | 5m | 1h | top | end>");
                    return;
                }
            }
        };
        self.scroll_to_msg(target);
        self.info(format!("jumped to message #{target}"));
    }
    /// `/find <text>` — search + jump; no arg cycles; `clear` drops the search.
    fn find_cmd(&self, arg: &str) {
        if matches!(
            arg.trim().to_ascii_lowercase().as_str(),
            "clear" | "off" | "discard"
        ) {
            if self.find_query.get_untracked().is_some() {
                self.find_query.set(None);
                self.find_pos.set(0);
                self.info("search cleared");
            } else {
                self.info("no active search");
            }
            return;
        }
        let arg = arg.trim();
        if arg.is_empty() {
            if self.find_query.get_untracked().is_none() {
                self.info("usage: /find <text>");
                return;
            }
        } else {
            // A new query restarts cycling from the top.
            if self.find_query.get_untracked().as_deref() != Some(arg) {
                self.find_pos.set(0);
            }
            self.find_query.set(Some(arg.to_string()));
        }
        self.cycle(true);
    }
    /// Cycle to the next/previous match of the active query, wrapping.
    fn cycle(&self, forward: bool) {
        let Some(query) = self.find_query.get_untracked() else {
            self.info("no active search — /find <text>");
            return;
        };
        let hits = self.hits(&query);
        if hits.is_empty() {
            self.info(format!("no match for {query:?}"));
            return;
        }
        let pos = self.find_pos.get_untracked();
        let target = if forward {
            hits.iter().copied().find(|&n| n > pos).unwrap_or(hits[0])
        } else {
            hits.iter()
                .rev()
                .copied()
                .find(|&n| n < pos)
                .unwrap_or(*hits.last().unwrap())
        };
        let idx = hits.iter().position(|&n| n == target).unwrap_or(0) + 1;
        self.find_pos.set(target);
        self.scroll_to_msg(target);
        self.info(format!(
            "match {idx}/{} for {query:?} → message #{target}",
            hits.len()
        ));
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
    session_id: RwSignal<Option<String>>,
    session_label: RwSignal<Option<String>>,
    save_gen: RwSignal<u64>,
    turn_start: RwSignal<Option<Instant>>,
    ttft: RwSignal<Option<f64>>,
    show_reasoning: RwSignal<bool>,
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
    find_query: RwSignal<Option<String>>,
    find_pos: RwSignal<usize>,
    ctx_window: RwSignal<Option<u32>>,
    cfg: Rc<AgentConfig>,
    queue: Rc<RefCell<VecDeque<String>>>,
    turn_handle: Rc<RefCell<Option<tokio::task::JoinHandle<()>>>>,
    spawn_turn: Rc<dyn Fn(String, bool)>,
    clipboard: Rc<RefCell<Option<hjkl_clipboard::Clipboard>>>,
    agent: Arc<TokioMutex<Agent>>,
    tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,
    base_url: RwSignal<String>,
}

impl hrdr_app::CommandHost for GuiHost {
    fn info(&mut self, line: String) {
        system(self.transcript, self.next_id, line);
    }
    fn spawn_line(&self, fut: hrdr_app::LineFuture) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let line = fut.await;
            if !line.is_empty() {
                let _ = tx.send(UiMsg::System(line));
            }
        });
    }
    fn agent(&self) -> Arc<TokioMutex<Agent>> {
        self.agent.clone()
    }
    fn cwd(&self) -> std::path::PathBuf {
        // The agent's cwd is authoritative (it follows a resumed session);
        // fall back to the process cwd if a turn holds the lock.
        self.agent
            .try_lock()
            .map(|a| a.cwd())
            .ok()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
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
        let count = session.messages.len();
        let prev_cwd = self.cwd();
        self.model.set(session.model.clone());
        self.session_id.set(Some(id));
        self.session_label.set(Some(session.name.clone()));
        // Pending saves from before the resume belong to the old conversation.
        self.save_gen.update(|g| *g += 1);
        rebuild_transcript(self.cx, self.transcript, self.next_id, &session.messages);
        // Follow the session's working directory (in-process only), matching
        // the TUI — the tools must operate where the session's work lives.
        let target = std::path::PathBuf::from(&session.cwd);
        let new_cwd = (!session.cwd.is_empty() && target != prev_cwd && target.is_dir())
            .then_some(target.clone());
        // Synchronous when the lock is free (see clear_conversation) so a
        // send right after /resume can't race the message swap.
        let msgs = session.messages.clone();
        let m = session.model.clone();
        let cwd_for_agent = new_cwd.clone();
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
        system(
            self.transcript,
            self.next_id,
            format!("resumed '{}' ({count} messages)", session.name),
        );
        if let Some(c) = new_cwd {
            system(
                self.transcript,
                self.next_id,
                format!("cwd → {}", c.display()),
            );
        } else if !session.cwd.is_empty()
            && std::path::Path::new(&session.cwd) != prev_cwd
            && !std::path::Path::new(&session.cwd).is_dir()
        {
            system(
                self.transcript,
                self.next_id,
                format!(
                    "note: session cwd {} no longer exists; staying in {}",
                    session.cwd,
                    prev_cwd.display()
                ),
            );
        }
        let cur_url = self.base_url.get_untracked();
        if session.base_url != cur_url {
            system(
                self.transcript,
                self.next_id,
                format!(
                    "note: session endpoint was {} (current: {cur_url})",
                    session.base_url
                ),
            );
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
        use hjkl_clipboard::{MimeType, Selection};
        let bytes = self
            .clipboard
            .borrow_mut()
            .as_mut()
            .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok())?;
        Some(String::from_utf8_lossy(&bytes).to_string())
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
                for t in &tools {
                    t.collapsed.set(false);
                }
                "tool output expanded (all)".to_string()
            }
            hrdr_app::ExpandMode::Off => {
                for t in &tools {
                    t.collapsed.set(true);
                }
                "tool output collapsed".to_string()
            }
            hrdr_app::ExpandMode::ToggleLast => match tools.last() {
                Some(t) => {
                    let now = !t.collapsed.get_untracked();
                    t.collapsed.set(now);
                    if now {
                        "collapsed last tool output".to_string()
                    } else {
                        "expanded last tool output".to_string()
                    }
                }
                None => "no tool output to expand".to_string(),
            },
        }
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
    fn set_effort(&mut self, label: String) {
        self.effort.set(Some(label));
    }
    fn cwd_changed(&mut self, new: &std::path::Path) {
        self.dir.set(hrdr_app::display_dir(new));
        self.branch.set(hrdr_app::git_branch(new));
        // Rebuilt lazily on the next `@` mention, for the new directory.
        self.file_index.set(Vec::new());
    }
    fn files_changed(&mut self) {
        self.file_index.set(Vec::new());
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
        // Re-read the display config and apply what the GUI can change live.
        let ui = hrdr_app::UiConfig::load();
        self.show_reasoning.set(ui.show_thinking);
        self.timestamp_style
            .set(hrdr_app::TimestampStyle::from_config(
                ui.timestamps.as_deref(),
            ));
        self.todo_ttl.set(ui.todo_ttl);
        system(
            self.transcript,
            self.next_id,
            "reloaded config (thinking, timestamps, todo-ttl; theme needs a restart)",
        );
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
                collapsed: item_cx.create_rw_signal(true),
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
            let args = hrdr_tools::truncate_inline(&t.args, 80);
            let (output, result, ok, collapsed) = (t.output, t.result, t.ok, t.collapsed);
            v_stack((
                // Clickable header — caret reflects/toggles the output collapse.
                label(move || {
                    let caret = if collapsed.get() { "▸" } else { "▾" };
                    format!("{caret} ⚙ {name} {args}")
                })
                .style(move |s| s.color(th.tool).font_bold())
                .on_click_stop(move |_| collapsed.update(|c| *c = !*c)),
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
            .style(|s| s.padding(6.0).gap(2.0))
            .into_any()
        }
        Body::System(s) => text_label(s).style(move |st| st.color(th.dim)).into_any(),
        Body::Error(s) => text_label(s).style(move |st| st.color(th.err)).into_any(),
    }
}

/// A plain (non-reactive) text label.
fn text_label(s: String) -> impl IntoView {
    label(move || s.clone())
}

/// The context gauge as a real progress bar: a rounded track with a fill
/// layer whose width is the used fraction, and the shared label on top.
fn ctx_gauge_view(gauge: hrdr_app::CtxGauge, th: GuiTheme) -> AnyView {
    use floem::views::{empty, stack};
    let fill_color = match gauge.level {
        hrdr_app::CtxLevel::Ok => th.ok,
        hrdr_app::CtxLevel::Warn => th.tool,
        hrdr_app::CtxLevel::Critical => th.err,
    };
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

/// Map a shared status color role onto the GUI theme (mirrors the TUI's
/// `status_role_style`).
fn status_run_style(
    s: floem::style::Style,
    role: hrdr_app::StatusRole,
    th: GuiTheme,
) -> floem::style::Style {
    use hrdr_app::{CtxLevel, StatusRole};
    match role {
        StatusRole::Dir => s.color(th.user),
        StatusRole::Branch => s.color(th.ok),
        StatusRole::TokensIn => s.color(th.accent),
        StatusRole::TokensOut => s.color(th.accent2),
        StatusRole::CtxFill(level) => {
            let bg = match level {
                CtxLevel::Ok => th.ok,
                CtxLevel::Warn => th.tool,
                CtxLevel::Critical => th.err,
            };
            s.color(Color::BLACK).background(bg).font_bold()
        }
        StatusRole::CtxRest => s.color(th.assistant).background(th.dim),
        StatusRole::CtxPlain => s.color(th.tool),
        StatusRole::Model => s.color(th.assistant),
        StatusRole::Effort => s.color(th.tool),
        StatusRole::Ttft => s.color(th.dim),
    }
}
