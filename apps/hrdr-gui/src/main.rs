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

/// UI-thread message from a running turn (mirrors the TUI's `TurnMsg`).
#[derive(Clone)]
enum UiMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
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
    // Last turn's reported (prompt, completion) token usage, for the status bar.
    let usage: RwSignal<Option<(u32, u32)>> = create_rw_signal(None);

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
        }
    });

    let send = move || {
        let text = input.get();
        if text.trim().is_empty() || running.get() {
            return;
        }
        input.set(String::new());
        push_item(transcript, next_id, Body::User(text.clone()));
        running.set(true);

        let agent = agent.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let tx_ev = tx.clone();
            let result = agent
                .lock()
                .await
                .run(text, move |ev| {
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
            move |item| render_item(item, theme),
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
        .style(|s| s.flex_grow(1.0).padding(8.0));

    let input_row = h_stack((input_box, button("Send").on_click_stop(move |_| send())))
        .style(|s| s.width_full().gap(8.0).padding(10.0).items_center());

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
            format!("{model}   ·   ctx {ctx}   ·   ↓{out}")
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

    v_stack((transcript_view, status_bar, input_row)).style(move |s| {
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

fn render_item(item: Item, th: GuiTheme) -> AnyView {
    match item.body {
        Body::User(text) => v_stack((
            label(|| "you").style(move |s| s.color(th.user).font_bold().margin_bottom(2.0)),
            text_label(text),
        ))
        .into_any(),
        Body::Assistant(a) => v_stack((
            label(|| "assistant").style(move |s| s.color(th.dim).font_bold().margin_bottom(2.0)),
            // Reasoning (dim); empty until the model streams any.
            label(move || a.reasoning.get()).style(move |s| s.color(th.dim).margin_bottom(2.0)),
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
