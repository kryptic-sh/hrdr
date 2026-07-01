//! `hrdr-gui` — a floem desktop frontend for the agentic coding harness.
//!
//! A **proof-of-concept** that drives the same UI-agnostic core the TUI uses —
//! `hrdr_agent::Agent` — rendering its streamed [`AgentEvent`]s in a floem
//! window: assistant text + `<think>` reasoning, tool calls with live output
//! and pass/fail results, and Enter-to-send. As GUI features grow, the parts
//! shared with the TUI (transcript model, slash commands, sessions, …) get
//! lifted out of `hrdr-tui` into a shared crate both frontends consume.

use std::sync::Arc;

use floem::AnyView;
use floem::ext_event::create_signal_from_tokio_channel;
use floem::keyboard::{Key, NamedKey};
use floem::prelude::*;
use floem::reactive::{Scope, create_effect};
use floem::views::Decorators;
use hrdr_agent::{Agent, AgentConfig, AgentEvent};
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
    /// Load an hjkl theme TOML (or hjkl's bundled default), mapping its palette
    /// + UI styles onto hrdr's chat roles with fallbacks.
    fn load(path: Option<&str>) -> Self {
        use hjkl_theme::{Theme as HjklTheme, loader};
        let t = match path {
            Some(p) => HjklTheme::from_path(std::path::Path::new(p))
                .unwrap_or_else(|_| loader::default_theme()),
            None => loader::default_theme(),
        };
        let pal = |name: &str| t.palette.get(name).copied().map(to_floem);
        Self {
            bg: t
                .ui
                .background
                .map(to_floem)
                .unwrap_or(Color::rgb8(0x1e, 0x1e, 0x24)),
            user: pal("teal").or_else(|| pal("blue")).unwrap_or(USER),
            assistant: t
                .ui
                .foreground
                .map(to_floem)
                .or_else(|| pal("fg"))
                .unwrap_or(Color::rgb8(0xe0, 0xe0, 0xe0)),
            dim: t
                .ui
                .gutter
                .map(to_floem)
                .or_else(|| pal("comment"))
                .unwrap_or(DIM),
            tool: pal("yellow").unwrap_or(TOOL),
            ok: pal("green").unwrap_or(OK),
            err: t
                .ui
                .diagnostic_error
                .map(to_floem)
                .or_else(|| pal("red"))
                .unwrap_or(ERR),
        }
    }
}

fn to_floem(c: hjkl_theme::Color) -> Color {
    Color::rgb8(c.r, c.g, c.b)
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
}

fn main() -> anyhow::Result<()> {
    // A tokio runtime entered on this (UI) thread so floem's tokio-channel
    // bridge + per-turn agent tasks can `tokio::spawn`. Held for program life.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let config = AgentConfig::load();
    let model = config.model.clone();
    let ctx_window = config.context_window;
    let theme_path = config.theme.clone();
    let agent = Arc::new(TokioMutex::new(Agent::new(config)?));

    floem::launch(move || app_view(agent, model, ctx_window, theme_path));
    Ok(())
}

fn app_view(
    agent: Arc<TokioMutex<Agent>>,
    model: String,
    ctx_window: Option<u32>,
    theme_path: Option<String>,
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
    // Whether to show the model's `<think>` reasoning (`/reasoning` toggles).
    let show_reasoning = create_rw_signal(true);
    // Submitted-input history (shared load/persist), for Up/Down recall.
    let history: RwSignal<Vec<String>> = create_rw_signal(hrdr_app::load_history());
    // Position while browsing history (None = editing a fresh draft); the draft
    // is stashed when browsing begins so Down past the newest restores it.
    let hist_pos: RwSignal<Option<usize>> = create_rw_signal(None);
    let hist_draft = create_rw_signal(String::new());

    // Bridge background turns → the UI thread over one long-lived channel.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
    let events = create_signal_from_tokio_channel(rx);
    create_effect(move |_| {
        let Some(msg) = events.get() else { return };
        match msg {
            UiMsg::Event(ev) => handle_event(cx, transcript, next_id, usage, ev),
            UiMsg::Done(err) => {
                running.set(false);
                if let Some(e) = err {
                    push_item(transcript, next_id, Body::Error(e));
                }
            }
            UiMsg::System(s) => push_item(transcript, next_id, Body::System(s)),
        }
    });

    let send = move || {
        let text = input.get();
        if text.trim().is_empty() || running.get() {
            return;
        }
        // Record every submitted line for Up/Down recall, and reset browsing.
        record_history(history, &text);
        hist_pos.set(None);
        // Common quit words (shared with the TUI) close the window.
        if hrdr_app::is_quit_command(&text) {
            floem::quit_app();
            return;
        }
        // Slash commands are handled locally; an unrecognized `/…` falls through
        // to the model (so a literal path still works, matching the TUI).
        if text.starts_with('/')
            && dispatch_slash(
                &text,
                transcript,
                next_id,
                usage,
                model,
                show_reasoning,
                &agent,
                &tx,
            )
        {
            input.set(String::new());
            return;
        }
        input.set(String::new());
        push_item(transcript, next_id, Body::User(text.clone()));
        running.set(true);

        // Expand `@file` mentions for the model only; the transcript keeps the
        // bare `@path` the user typed (same split as the TUI).
        let sent = match std::env::current_dir() {
            Ok(cwd) => hrdr_app::expand_mentions(&text, &cwd),
            Err(_) => text,
        };

        let agent = agent.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let tx_ev = tx.clone();
            let result = agent
                .lock()
                .await
                .run(sent, move |ev| {
                    let _ = tx_ev.send(UiMsg::Event(ev));
                })
                .await;
            let _ = tx.send(UiMsg::Done(result.err().map(|e| e.to_string())));
        });
    };
    let send_enter = send.clone();

    let transcript_view = scroll(
        dyn_stack(
            move || transcript.get(),
            |item: &Item| item.id,
            move |item| render_item(item, theme, show_reasoning),
        )
        .style(|s| s.flex_col().width_full().gap(10.0)),
    )
    .style(|s| s.flex_grow(1.0).width_full().padding(10.0));

    let input_box = text_input(input)
        .placeholder("Message hrdr…  (Enter to send)")
        .on_key_down(
            Key::Named(NamedKey::Enter),
            |m| m.is_empty(),
            move |_| send_enter(),
        )
        // Up/Down recall previous submissions (like the TUI's history).
        .on_key_down(Key::Named(NamedKey::ArrowUp), |m| m.is_empty(), move |_| {
            history_prev(history, input, hist_pos, hist_draft)
        })
        .on_key_down(
            Key::Named(NamedKey::ArrowDown),
            |m| m.is_empty(),
            move |_| history_next(history, input, hist_pos, hist_draft),
        )
        .style(|s| s.flex_grow(1.0).padding(8.0));

    let input_row = h_stack((input_box, button("Send").on_click_stop(move |_| send())))
        .style(|s| s.width_full().gap(8.0).padding(10.0).items_center());

    // Completion list shown above the input: slash commands while a `/…` is being
    // typed, or ranked `@file` paths while an `@…` mention is active (both use the
    // shared rankers). Clicking a row fills the input.
    let comp_rows = move || -> Vec<CompRow> {
        let inp = input.get();
        if inp.starts_with('/') {
            return hrdr_app::slash_completions(&inp)
                .into_iter()
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
            format!("{}   ·   ctx {ctx}   ·   ↓{out}", model.get())
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
    let id = next_id.get_untracked();
    next_id.set(id + 1);
    transcript.update(|t| t.push(Item { id, body }));
}

/// Push a system line into the transcript.
fn system(transcript: RwSignal<Vec<Item>>, next_id: RwSignal<u64>, msg: impl Into<String>) {
    push_item(transcript, next_id, Body::System(msg.into()));
}

/// Append a submitted line to the in-memory history (dropping an immediate
/// duplicate of the last entry) and persist it (capped at `MAX_HISTORY`).
fn record_history(history: RwSignal<Vec<String>>, text: &str) {
    history.update(|h| {
        if h.last().map(String::as_str) != Some(text) {
            h.push(text.to_string());
            if h.len() > hrdr_app::MAX_HISTORY {
                let drop = h.len() - hrdr_app::MAX_HISTORY;
                h.drain(0..drop);
            }
        }
    });
    hrdr_app::persist_history(&history.get_untracked());
}

/// Up-arrow: step to an older history entry, stashing the live draft on the
/// first step so Down can restore it.
fn history_prev(
    history: RwSignal<Vec<String>>,
    input: RwSignal<String>,
    hist_pos: RwSignal<Option<usize>>,
    hist_draft: RwSignal<String>,
) {
    let h = history.get();
    if h.is_empty() {
        return;
    }
    let pos = match hist_pos.get() {
        None => {
            hist_draft.set(input.get_untracked());
            h.len() - 1
        }
        Some(0) => 0,
        Some(p) => p - 1,
    };
    hist_pos.set(Some(pos));
    input.set(h[pos].clone());
}

/// Down-arrow: step to a newer history entry, or past the newest back to the
/// stashed draft.
fn history_next(
    history: RwSignal<Vec<String>>,
    input: RwSignal<String>,
    hist_pos: RwSignal<Option<usize>>,
    hist_draft: RwSignal<String>,
) {
    let h = history.get();
    match hist_pos.get() {
        None => {}
        Some(p) if p + 1 < h.len() => {
            hist_pos.set(Some(p + 1));
            input.set(h[p + 1].clone());
        }
        Some(_) => {
            hist_pos.set(None);
            input.set(hist_draft.get_untracked());
        }
    }
}

/// Handle a `/…` slash command locally. Returns `true` if it was recognized (and
/// thus shouldn't be sent to the model). Mirrors the representation-independent
/// subset of the TUI's `handle_slash`; commands needing the agent lock spawn a
/// task and report back over the `UiMsg::System` channel. Aliases resolve via the
/// shared `hrdr_app::resolve_alias`.
#[allow(clippy::too_many_arguments)]
fn dispatch_slash(
    input: &str,
    transcript: RwSignal<Vec<Item>>,
    next_id: RwSignal<u64>,
    usage: RwSignal<Option<(u32, u32)>>,
    model: RwSignal<String>,
    show_reasoning: RwSignal<bool>,
    agent: &Arc<TokioMutex<Agent>>,
    tx: &tokio::sync::mpsc::UnboundedSender<UiMsg>,
) -> bool {
    let rest = input.strip_prefix('/').unwrap_or(input);
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = hrdr_app::resolve_alias(parts.next().unwrap_or(""));
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "help" => system(transcript, next_id, hrdr_app::help_body()),
        "reasoning" => {
            let on = !show_reasoning.get_untracked();
            show_reasoning.set(on);
            system(
                transcript,
                next_id,
                if on { "reasoning shown" } else { "reasoning hidden" },
            );
        }
        "clear" => {
            transcript.update(|t| t.clear());
            next_id.set(0);
            usage.set(None);
            let agent = agent.clone();
            tokio::spawn(async move { agent.lock().await.clear() });
            system(transcript, next_id, "conversation cleared");
        }
        "model" => {
            if arg.is_empty() {
                system(transcript, next_id, format!("model: {}", model.get()));
            } else {
                model.set(arg.clone());
                let agent = agent.clone();
                let name = arg.clone();
                tokio::spawn(async move { agent.lock().await.set_model(name) });
                system(transcript, next_id, format!("model → {arg}"));
            }
        }
        "models" => {
            let agent = agent.clone();
            let tx = tx.clone();
            system(transcript, next_id, "fetching models…");
            tokio::spawn(async move {
                let client = agent.lock().await.client();
                let msg = match client.list_models().await {
                    Ok(m) if !m.is_empty() => format!("models:\n  {}", m.join("\n  ")),
                    Ok(_) => "endpoint reported no models".to_string(),
                    Err(e) => format!("models error: {e}"),
                };
                let _ = tx.send(UiMsg::System(msg));
            });
        }
        "tools" => {
            let agent = agent.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let tools = agent.lock().await.tools();
                let mut msg = format!("{} tools:", tools.len());
                for (name, desc) in tools {
                    msg.push_str(&format!("\n  {name:<12}{desc}"));
                }
                let _ = tx.send(UiMsg::System(msg));
            });
        }
        "info" => {
            let agent = agent.clone();
            let tx = tx.clone();
            let model = model.get();
            tokio::spawn(async move {
                let a = agent.lock().await;
                let msg = format!(
                    "model: {model}\nmessages: {}\ncwd: {}",
                    a.message_count(),
                    a.cwd().display()
                );
                let _ = tx.send(UiMsg::System(msg));
            });
        }
        _ => return false,
    }
    true
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
    let a = Assistant {
        reasoning: cx.create_rw_signal(String::new()),
        text: cx.create_rw_signal(String::new()),
    };
    push_item(transcript, next_id, Body::Assistant(a.clone()));
    a
}

fn find_tool(transcript: RwSignal<Vec<Item>>, call_id: &str) -> Option<Tool> {
    transcript.with_untracked(|t| {
        t.iter().find_map(|i| match &i.body {
            Body::Tool(tool) if tool.call_id == call_id => Some(tool.clone()),
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
            let tool = Tool {
                call_id: id,
                name,
                args,
                output: cx.create_rw_signal(String::new()),
                result: cx.create_rw_signal(String::new()),
                ok: cx.create_rw_signal(true),
                done: cx.create_rw_signal(false),
                collapsed: cx.create_rw_signal(true),
            };
            push_item(transcript, next_id, Body::Tool(tool));
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
            label(move || a.text.get()).style(move |s| s.color(th.assistant)),
        ))
        .into_any(),
        Body::Tool(t) => {
            let name = t.name.clone();
            let args = one_line(&t.args, 80);
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

/// Collapse to a single line and truncate to `max` chars (char-safe).
fn one_line(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() <= max {
        one
    } else {
        let head: String = one.chars().take(max).collect();
        format!("{head}…")
    }
}
