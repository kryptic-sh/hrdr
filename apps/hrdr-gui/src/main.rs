//! `hrdr-gui` — a floem desktop frontend for the agentic coding harness.
//!
//! A **proof-of-concept** that drives the same UI-agnostic core the TUI uses —
//! `hrdr_agent::Agent` — rendering its streamed [`AgentEvent`]s in a floem
//! window: assistant text + `<think>` reasoning, tool calls with live output
//! and pass/fail results, and Enter-to-send. As GUI features grow, the parts
//! shared with the TUI (transcript model, slash commands, sessions, …) get
//! lifted out of `hrdr-tui` into a shared crate both frontends consume.

use std::cell::RefCell;
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
    let theme_path = ui.theme.clone();
    let base_url = config.base_url.clone();
    let show_thinking = ui.show_thinking;
    let agent = Arc::new(TokioMutex::new(Agent::new(config)?));

    floem::launch(move || {
        app_view(
            agent,
            model,
            ctx_window,
            theme_path,
            base_url,
            show_thinking,
        )
    });
    Ok(())
}

fn app_view(
    agent: Arc<TokioMutex<Agent>>,
    model: String,
    ctx_window: Option<u32>,
    theme_path: Option<String>,
    base_url: String,
    show_thinking: bool,
) -> impl IntoView {
    let theme = GuiTheme::load(theme_path.as_deref());
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
    // Populate the file index once, when the user starts an `@file` mention.
    create_effect(move |_| {
        if input.get().contains('@')
            && file_index.with_untracked(Vec::is_empty)
            && let Ok(cwd) = std::env::current_dir()
        {
            file_index.set(hrdr_app::walk_files(&cwd));
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

    // Bridge background turns → the UI thread over one long-lived channel.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
    let events = create_signal_from_tokio_channel(rx);
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
                handle_event(cx, transcript, next_id, usage, ev);
            }
            UiMsg::Done(err) => {
                running.set(false);
                if let Some(e) = err {
                    push_item(transcript, next_id, Body::Error(e));
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

    let th_for_send = turn_handle.clone();
    let history_for_send = history.clone();
    let send = move || {
        // Trim like the TUI does, so " /help" is still a command.
        let text = input.get().trim().to_string();
        if text.is_empty() || running.get() {
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
        // and GUI share one implementation). An unrecognized `/…` falls through to
        // the model (a literal path still works, matching the TUI) — unless it's a
        // registered command the GUI just doesn't implement, which gets a notice
        // instead of confusing the model.
        if let Some(rest) = text.strip_prefix('/') {
            let mut host = GuiHost {
                cx,
                transcript,
                next_id,
                usage,
                model,
                session_id,
                session_label,
                save_gen,
                turn_start,
                ttft,
                show_reasoning,
                clipboard: clipboard.clone(),
                agent: agent.clone(),
                tx: tx.clone(),
                base_url: base_url.clone(),
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
        push_item(transcript, next_id, Body::User(text.clone()));
        running.set(true);
        // Start the TTFT clock; cleared until the first token of this turn.
        turn_start.set(Some(Instant::now()));
        ttft.set(None);

        // Expand `@file` mentions for the model only; the transcript keeps the
        // bare `@path` the user typed (same split as the TUI). Paths resolve
        // against the agent's cwd (it follows resumed sessions).
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
        // Snapshot session state for the post-turn auto-save (signals can't be
        // read from the spawned task).
        let existing_id = session_id.get_untracked();
        let session_label = session_label.get_untracked();
        let cur_model = model.get_untracked();
        let base_url = base_url.clone();
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
            if let Some(o) =
                hrdr_app::save_agent_session(agent, existing_id, session_label, cur_model, base_url)
                    .await
            {
                let _ = tx.send(UiMsg::Saved {
                    id: o.id,
                    first_save: o.first_save,
                    generation,
                });
            }
        });
        *th_for_send.borrow_mut() = Some(handle);
    };

    // Cancel the in-flight turn: abort the task (dropping its future releases
    // the agent lock; the next turn repairs any dangling tool calls) and mark
    // the turn done. Late buffered events are dropped via the `running` guard.
    let cancel = move || {
        if !running.get_untracked() {
            return;
        }
        if let Some(h) = turn_handle.borrow_mut().take() {
            h.abort();
        }
        running.set(false);
        system(transcript, next_id, "[cancelled]");
    };

    let send_enter = send.clone();
    let send_btn = send.clone();
    let cancel_esc = cancel.clone();
    let cancel_btn = cancel.clone();

    let transcript_view = scroll(
        dyn_stack(
            move || transcript.get(),
            |item: &Item| item.id,
            move |item| render_item(item, theme, show_reasoning),
        )
        .style(|s| s.flex_col().width_full().gap(10.0)),
    )
    .style(|s| s.flex_grow(1.0).width_full().padding(10.0));

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
    let status_bar = h_stack((
        label(move || {
            let (used, out) = usage.get().unwrap_or((0, 0));
            let ctx = match ctx_window {
                Some(w) => format!("{used} of {w}"),
                None if used > 0 => format!("{used} tok"),
                None => "—".to_string(),
            };
            // Time-to-first-token for the last turn, once measured.
            let ttft = match ttft.get() {
                Some(secs) => format!("   ·   ttft {secs:.2}s"),
                None => String::new(),
            };
            format!("{}   ·   ctx {ctx}   ·   ↓{out}{ttft}", model.get())
        })
        .style(move |s| s.color(theme.dim)),
        label(|| "● thinking…").style(move |s| {
            if running.get() {
                s.color(theme.tool)
            } else {
                s.hide()
            }
        }),
    ))
    .style(|s| {
        s.width_full()
            .padding_horiz(10.0)
            .padding_vert(4.0)
            .justify_between()
            .items_center()
    });

    v_stack((transcript_view, completions, status_bar, input_row)).style(move |s| {
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
    transcript.update(|t| t.push(Item { id, body, scope }));
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

/// The GUI's [`hrdr_app::CommandHost`] — the capability surface the shared
/// slash-command dispatcher drives. Holds clones of the reactive signals +
/// agent handle + clipboard so the shared commands can mutate GUI state.
struct GuiHost {
    cx: Scope,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    usage: RwSignal<Option<(u32, u32)>>,
    model: RwSignal<String>,
    session_id: RwSignal<Option<String>>,
    session_label: RwSignal<Option<String>>,
    save_gen: RwSignal<u64>,
    turn_start: RwSignal<Option<Instant>>,
    ttft: RwSignal<Option<f64>>,
    show_reasoning: RwSignal<bool>,
    clipboard: Rc<RefCell<Option<hjkl_clipboard::Clipboard>>>,
    agent: Arc<TokioMutex<Agent>>,
    tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,
    base_url: String,
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
        self.base_url.clone()
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
        let base_url = self.base_url.clone();
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
        if session.base_url != self.base_url {
            system(
                self.transcript,
                self.next_id,
                format!(
                    "note: session endpoint was {} (current: {})",
                    session.base_url, self.base_url
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

fn render_item(item: Item, th: GuiTheme, show_reasoning: RwSignal<bool>) -> AnyView {
    match item.body {
        Body::User(text) => v_stack((
            label(|| "you").style(move |s| s.color(th.user).font_bold().margin_bottom(2.0)),
            text_label(text),
        ))
        .into_any(),
        Body::Assistant(a) => v_stack((
            label(|| "assistant").style(move |s| s.color(th.dim).font_bold().margin_bottom(2.0)),
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
