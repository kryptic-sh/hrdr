//! Rendering: transcript + TODO panel + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Entry, EntryKind, StatusBarMode, TimestampStyle};
use crate::theme::Theme;
use hrdr_app::relative_time;

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Diff results (edit/write) get a larger preview since the diff is the point.
const DIFF_PREVIEW_LINES: usize = 40;
/// Max lines shown in the TODO panel (plus 2 for borders).
const TODO_PANEL_MAX_ITEMS: u16 = 6;
/// Max rows (one per agent) the sub-agent panel lists; beyond this the panel
/// scrolls its rows off the top (newest at the bottom).
const SUBAGENT_PANEL_MAX_ROWS: u16 = 18;

/// Outer height (with padding) of the sub-agent panel; 0 when nothing is shown.
fn subagent_panel_height(items: &[hrdr_app::PaneRow]) -> u16 {
    if items.is_empty() {
        return 0;
    }
    (items.len() as u16).min(SUBAGENT_PANEL_MAX_ROWS) + 2
}

/// Rows clipped from the top so the newest agent sits at the bottom of a panel
/// `height` rows tall. Pure, so the scroll math is testable without rendering.
fn subagent_scroll(items: usize, height: u16) -> u16 {
    (items as u16).saturating_sub(height)
}

/// TODO-panel sort order by status: the one in progress on top, the not-yet-
/// started (pending) ones in the middle, the finished/cancelled ones at the bottom.
fn todo_sort_key(status: &str) -> u8 {
    match status {
        "in_progress" => 0,
        "completed" | "cancelled" => 2,
        _ => 1,
    }
}

/// Agent-panel sort order: the main agent on top, the still-running (or idle)
/// sub-agents in the middle, the finished (Done) ones at the bottom.
fn agent_sort_key(row: &hrdr_app::PaneRow) -> u8 {
    if row.id.is_main() {
        0
    } else if row.status == hrdr_app::PaneStatus::Done {
        2
    } else {
        1
    }
}

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Bring every pane up to date with its agent *before* anything reads one: the
    // loader's height, the status bar and the transcript must all describe the same
    // frame. (This is also what pins the pane being viewed, so the agent does not
    // release it out from under the reader.)
    app.sync_panes();

    // Snapshot the TODO list while briefly holding the lock; the height and
    // the renderer both use the same snapshot.
    // The TODO list belongs to the agent on screen — every agent has its own, and
    // the `todo` tool a sub-agent calls writes to *its* list, not the main one's.
    let mut todos = app
        .panes
        .active_pane()
        .todos
        .lock()
        .map(|t| t.clone())
        .unwrap_or_default();
    // Sort for the panel: the one being worked on (in_progress) at the top, then
    // the not-yet-started (pending) ones, then the completed ones at the bottom.
    // Stable, so items keep their relative order within each group.
    todos.sort_by_key(|t| todo_sort_key(&t.status));
    let todo_height = if todos.is_empty() {
        0
    } else {
        (todos.len() as u16).min(TODO_PANEL_MAX_ITEMS) + 2
    };

    // The loader heads the input section while **the agent on screen** works. It
    // hides while that agent's tool calls run: the model is idle then, and a spinner
    // would claim otherwise (the running tool's own block carries the `…` mark), and
    // it hides entirely when the agent you are looking at is not working — even if
    // another one is.
    // Compaction is the agent's, like the turn clock: a sub-agent summarizing itself
    // says so on its own pane rather than looking hung.
    let working = app.panes.active_pane().turn.inferring() || app.panes.active_pane().compacting;
    let loader_height: u16 = if working { 1 } else { 0 };

    // Input pane auto-grows 1..=INPUT_MAX_ROWS text rows with the content.
    // Inner width = full width minus the horizontal padding on both sides; the
    // extra two rows are the blank padding above and below.
    let input_inner_w = area.width.saturating_sub(INPUT_PAD_X as u16 * 2);
    let input_height = app
        .editor
        .desired_rows(input_inner_w, hrdr_app::INPUT_MAX_ROWS)
        + 2;

    // Built once per frame; both the layout height and the renderer use it.
    // The switcher lists every agent — main first, so there is always a way back.
    // It stays hidden while the main agent is the only one: a one-row list of the
    // thing you are already looking at is noise.
    let mut subagent_items: Vec<hrdr_app::PaneRow> = if app.panes.show_switcher() {
        hrdr_app::pane_rows(&app.panes)
    } else {
        Vec::new()
    };
    // Sort the agent list: the main agent on top (always the way back), then the
    // still-running sub-agents, then the finished (Done) ones at the bottom.
    // Stable, so agents keep their spawn order within each group.
    subagent_items.sort_by_key(agent_sort_key);
    let subagent_height = subagent_panel_height(&subagent_items);

    // Build the row stack dynamically, remembering each section's index. Every
    // section of the input area carries a blank row above itself, so none butts
    // up against the one above it (the scrollback's last block no longer trails
    // a separator of its own) — and that row costs nothing when the section
    // isn't rendered. Each section's own blank is the previous one's blank below.
    let mut constraints = vec![Constraint::Min(3)];
    let section = |constraints: &mut Vec<Constraint>, height: u16| {
        (height > 0).then(|| {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(height));
            constraints.len() - 1
        })
    };
    let loader_idx = section(&mut constraints, loader_height);
    // TODO list first, then the agent list below it.
    let todo_idx = section(&mut constraints, todo_height);
    let subagent_idx = section(&mut constraints, subagent_height);
    let input_idx = section(&mut constraints, input_height).expect("the input pane always renders");
    // Status bar: hidden (0 rows), one row (truncate), or wrapped (≤4 rows). It
    // renders as a block, so it carries the same padding as the transcript's —
    // two columns either side, and a blank row above and below. That top row is
    // what separates it from the tinted input pane above.
    let (sb_left, sb_right) = build_status_sections(app);
    let sb_inner = inner_width(area.width as usize);
    let sb_rows: u16 = match app.statusbar_mode {
        StatusBarMode::None => 0,
        StatusBarMode::Truncate => 1,
        StatusBarMode::Wrap => {
            // Wrap packs both sides into the flow; size for the combined set.
            let combined: Vec<StatusSection> = sb_left.iter().chain(&sb_right).cloned().collect();
            status_wrap_rows(&combined, sb_inner as usize).clamp(1, 4)
        }
    };
    let sb_height = if sb_rows > 0 { sb_rows + 2 } else { 0 };
    let statusbar_idx = (sb_height > 0).then(|| {
        constraints.push(Constraint::Length(sb_height));
        constraints.len() - 1
    });
    // With no status bar there's nothing to separate the tinted input pane from
    // the bottom of the screen, so supply the blank row directly.
    if sb_height == 0 {
        constraints.push(Constraint::Length(1));
    }

    let chunks = Layout::vertical(constraints).split(area);

    draw_transcript(f, app, chunks[0]);
    if let Some(i) = subagent_idx {
        draw_subagents(f, app, chunks[i], &subagent_items);
    } else {
        app.subagent_hits.clear();
    }
    if let Some(i) = todo_idx {
        draw_todos(f, app, chunks[i], &todos);
    }
    if let Some(i) = loader_idx {
        draw_loader(f, app, chunks[i]);
    }
    draw_input(f, app, chunks[input_idx]);
    if let Some(i) = statusbar_idx {
        draw_statusbar(f, app, chunks[i], &sb_left, &sb_right);
    }

    // The `/model` selector and `/resume` picker are full modals; when one is
    // open it owns the screen (and every key), so the completion popup stands
    // down.
    if let Some(sel) = &app.model_selector {
        draw_model_selector(f, &app.theme, sel, app.model_loading, app.model_source);
    } else if let Some(sel) = &app.session_selector {
        draw_session_selector(f, &app.theme, sel);
    } else if let Some(sel) = &app.theme_selector {
        draw_theme_selector(f, &app.theme, sel);
    } else if let Some(sel) = &app.effort_selector {
        draw_effort_selector(f, &app.theme, sel);
    } else if let Some(sel) = &app.skill_selector {
        draw_skill_selector(f, &app.theme, sel);
    } else if let Some(modal) = &app.login_modal {
        draw_login_modal(f, &app.theme, modal);
    } else if let Some(comp) = app.active_completions() {
        // Completion popup (slash command or `@file`), overlaid above the input.
        app.completion_idx = app.completion_idx.min(comp.items.len() - 1);
        draw_completion(f, app, chunks[input_idx], &comp);
    }
}

/// The `/model` selector modal: a search line, a hint, and a two-column list
/// (friendly model name · friendly provider name) of every model across the
/// configured providers, narrowed by the fuzzy filter. Same chrome as the
/// blocks and the completion popup — solid background, 1×2 padding, no border.
fn draw_model_selector(
    f: &mut Frame,
    theme: &Theme,
    sel: &crate::app::ModelSelector,
    loading: bool,
    source: Option<hrdr_agent::CatalogSource>,
) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 92);
    let height = area.height.saturating_sub(2).clamp(1, 32);
    let rect = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 6 {
        return;
    }

    let rows: Vec<&hrdr_agent::ModelChoice> = sel.rows().collect();
    // Search line + a dim hint, then a blank row, then the list.
    let search = Line::from(vec![
        Span::styled("Search  ", Style::default().fg(theme.dim)),
        Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
        Span::styled("▌", Style::default().fg(theme.accent)),
    ]);
    // ChatGPT catalog provenance / loading, rendered on the hint line — kept
    // separate from the startup guidance so the two never share a block.
    let status = if loading {
        " · loading ChatGPT models…"
    } else {
        match source {
            Some(hrdr_agent::CatalogSource::Fresh) => " · ChatGPT: live",
            Some(hrdr_agent::CatalogSource::Stale) => " · ChatGPT: cached",
            Some(hrdr_agent::CatalogSource::BuiltInFallback) => " · ChatGPT: built-in",
            None => "",
        }
    };
    let hint = Line::from(Span::styled(
        format!(
            "{} model{} · ↑↓ select · Enter switch · ^D default · Esc cancel{status}",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ),
        Style::default().fg(theme.dim),
    ));

    let list_height = inner.height.saturating_sub(3) as usize; // search + hint + blank
    let inner_w = inner.width as usize;
    // Scroll so the selected row stays visible.
    let start = if sel.selected >= list_height {
        (sel.selected + 1).saturating_sub(list_height)
    } else {
        0
    };

    let mut lines = vec![search, hint, Line::from("")];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no models match",
            Style::default().fg(theme.dim),
        )));
    }
    for (i, c) in rows.iter().enumerate().skip(start).take(list_height) {
        let selected = i == sel.selected;
        // Model name on the left, provider right-aligned; the row fills the full
        // inner width so a selected row highlights end to end.
        let provider = truncate_chars(&c.provider_label, (inner_w / 2).max(1));
        let avail = inner_w.saturating_sub(provider.chars().count() + 1).max(1);
        let model = truncate_chars(&c.model_label, avail);
        let pad = inner_w
            .saturating_sub(model.chars().count() + provider.chars().count())
            .max(1);
        let line = if selected {
            Line::from(Span::styled(
                format!("{model}{}{provider}", " ".repeat(pad)),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(model, Style::default().fg(theme.user)),
                Span::styled(
                    format!("{}{provider}", " ".repeat(pad)),
                    Style::default().fg(theme.dim),
                ),
            ])
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The `/skills` picker modal: a search line, a hint, and a two-column list
/// (`:name` · description [source]); Enter inserts the invocation into the
/// input. Same chrome as the other pickers.
fn draw_skill_selector(f: &mut Frame, theme: &Theme, sel: &crate::app::SkillSelector) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 92);
    let height = area.height.saturating_sub(2).clamp(1, 24);
    let rect = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 6 {
        return;
    }

    let rows: Vec<&hrdr_app::Skill> = sel.rows().collect();
    let search = Line::from(vec![
        Span::styled("Search  ", Style::default().fg(theme.dim)),
        Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
        Span::styled("▌", Style::default().fg(theme.accent)),
    ]);
    let hint = Line::from(Span::styled(
        format!(
            "{} skill{} · ↑↓ select · Enter insert · Esc cancel",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ),
        Style::default().fg(theme.dim),
    ));

    let list_height = inner.height.saturating_sub(3) as usize;
    let inner_w = inner.width as usize;
    let start = if sel.selected >= list_height {
        (sel.selected + 1).saturating_sub(list_height)
    } else {
        0
    };

    let mut lines = vec![search, hint, Line::from("")];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no skills match",
            Style::default().fg(theme.dim),
        )));
    }
    for (i, sk) in rows.iter().enumerate().skip(start).take(list_height) {
        let selected = i == sel.selected;
        let right = if sk.description.is_empty() {
            sk.source.clone()
        } else {
            sk.description.clone()
        };
        let right = truncate_chars(&right, (inner_w * 2 / 3).max(1));
        let name = format!(":{}", sk.name);
        let avail = inner_w.saturating_sub(right.chars().count() + 1).max(1);
        let name = truncate_chars(&name, avail);
        let pad = inner_w
            .saturating_sub(name.chars().count() + right.chars().count())
            .max(1);
        let line = if selected {
            Line::from(Span::styled(
                format!("{name}{}{right}", " ".repeat(pad)),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(name, Style::default().fg(theme.user)),
                Span::styled(
                    format!("{}{right}", " ".repeat(pad)),
                    Style::default().fg(theme.dim),
                ),
            ])
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The `/login` modal. Provider phase: the same two-column picker chrome as
/// every other picker (label · auth method). Key phase: a masked input field
/// under the plaintext-storage warning — the key never touches the editor,
/// history, or transcript.
fn draw_login_modal(f: &mut Frame, theme: &Theme, modal: &crate::app::LoginModal) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 76);
    match modal {
        crate::app::LoginModal::Providers(sel) => {
            let height = area.height.saturating_sub(2).clamp(1, 16);
            let rect = Rect {
                x: (area.width.saturating_sub(width)) / 2,
                y: (area.height.saturating_sub(height)) / 2,
                width,
                height,
            };
            f.render_widget(Clear, rect);
            let block = Block::default()
                .style(Style::default().bg(theme.user_bg))
                .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            if inner.height < 3 || inner.width < 6 {
                return;
            }
            let rows: Vec<&hrdr_app::LoginProviderChoice> = sel.rows().collect();
            let search = Line::from(vec![
                Span::styled("🔑 /login  ", Style::default().fg(theme.warn)),
                Span::styled("Search  ", Style::default().fg(theme.dim)),
                Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ]);
            let hint = Line::from(Span::styled(
                format!(
                    "{} provider{} · ↑↓ select · Enter continue · Esc cancel",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" },
                ),
                Style::default().fg(theme.dim),
            ));
            let list_height = inner.height.saturating_sub(3) as usize;
            let inner_w = inner.width as usize;
            let start = if sel.selected >= list_height {
                (sel.selected + 1).saturating_sub(list_height)
            } else {
                0
            };
            let mut lines = vec![search, hint, Line::from("")];
            if rows.is_empty() {
                lines.push(Line::from(Span::styled(
                    "no providers match",
                    Style::default().fg(theme.dim),
                )));
            }
            for (i, c) in rows.iter().enumerate().skip(start).take(list_height) {
                let selected = i == sel.selected;
                let detail = truncate_chars(&c.detail, (inner_w / 2).max(1));
                let label = format!("{}  ({})", c.label, c.name);
                let avail = inner_w.saturating_sub(detail.chars().count() + 1).max(1);
                let label = truncate_chars(&label, avail);
                let pad = inner_w
                    .saturating_sub(label.chars().count() + detail.chars().count())
                    .max(1);
                let line = if selected {
                    Line::from(Span::styled(
                        format!("{label}{}{detail}", " ".repeat(pad)),
                        Style::default()
                            .fg(Color::Black)
                            .bg(theme.user)
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(vec![
                        Span::styled(label, Style::default().fg(theme.user)),
                        Span::styled(
                            format!("{}{detail}", " ".repeat(pad)),
                            Style::default().fg(theme.dim),
                        ),
                    ])
                };
                lines.push(line);
            }
            f.render_widget(Paragraph::new(lines), inner);
        }
        crate::app::LoginModal::Key {
            label,
            warning,
            input,
            ..
        } => {
            // Title + wrapped warning + masked field + hint.
            let warn_rows = (warning.chars().count() / (width.saturating_sub(6) as usize).max(1)
                + warning.matches('\n').count()
                + 1) as u16;
            let height = (warn_rows + 6).min(area.height.saturating_sub(2).max(1));
            let rect = Rect {
                x: (area.width.saturating_sub(width)) / 2,
                y: (area.height.saturating_sub(height)) / 2,
                width,
                height,
            };
            f.render_widget(Clear, rect);
            let block = Block::default()
                .style(Style::default().bg(theme.user_bg))
                .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            if inner.height < 4 || inner.width < 6 {
                return;
            }
            let masked: String = std::iter::repeat_n('•', input.chars().count())
                .take(inner.width.saturating_sub(2) as usize)
                .collect();
            let lines = vec![
                Line::from(Span::styled(
                    format!("🔑 API key — {label}"),
                    Style::default().fg(theme.warn),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    warning.clone(),
                    Style::default().fg(theme.dim),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Key  ", Style::default().fg(theme.dim)),
                    Span::styled(masked, Style::default().fg(theme.user)),
                    Span::styled("▌", Style::default().fg(theme.accent)),
                ]),
                Line::from(Span::styled(
                    "paste or type · Enter save & switch · Esc cancel",
                    Style::default().fg(theme.dim),
                )),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        crate::app::LoginModal::Authorizing { label, .. }
        | crate::app::LoginModal::Switching { label, .. } => {
            let switching = matches!(modal, crate::app::LoginModal::Switching { .. });
            let height = 5u16.min(area.height.saturating_sub(2).max(1));
            let rect = Rect {
                x: (area.width.saturating_sub(width)) / 2,
                y: (area.height.saturating_sub(height)) / 2,
                width,
                height,
            };
            f.render_widget(Clear, rect);
            let block = Block::default()
                .style(Style::default().bg(theme.user_bg))
                .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            if inner.height < 2 || inner.width < 6 {
                return;
            }
            let (title, hint) = if switching {
                (
                    format!("🔑 {label} — switching…"),
                    "finalizing — please wait".to_string(),
                )
            } else {
                (
                    format!("🔑 {label} — waiting for your browser…"),
                    "finish in the browser · Esc cancel".to_string(),
                )
            };
            let lines = vec![
                Line::from(Span::styled(title, Style::default().fg(theme.warn))),
                Line::from(""),
                Line::from(Span::styled(hint, Style::default().fg(theme.dim))),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
    }
}

/// The `/effort` picker modal: a search line, a hint, and a two-column list
/// (level label · detail) of the reasoning levels the current model accepts,
/// highest first with "Default" on top, narrowed by the fuzzy filter. Same
/// chrome as the other pickers.
fn draw_effort_selector(f: &mut Frame, theme: &Theme, sel: &crate::app::EffortSelector) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 64);
    let height = area.height.saturating_sub(2).clamp(1, 20);
    let rect = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 6 {
        return;
    }

    let rows: Vec<&hrdr_app::EffortChoice> = sel.rows().collect();
    let search = Line::from(vec![
        Span::styled("Search  ", Style::default().fg(theme.dim)),
        Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
        Span::styled("▌", Style::default().fg(theme.accent)),
    ]);
    let hint = Line::from(Span::styled(
        format!(
            "{} level{} · ↑↓ select · Enter apply · Esc cancel",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ),
        Style::default().fg(theme.dim),
    ));

    let list_height = inner.height.saturating_sub(3) as usize; // search + hint + blank
    let inner_w = inner.width as usize;
    let start = if sel.selected >= list_height {
        (sel.selected + 1).saturating_sub(list_height)
    } else {
        0
    };

    let mut lines = vec![search, hint, Line::from("")];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no levels match",
            Style::default().fg(theme.dim),
        )));
    }
    for (i, c) in rows.iter().enumerate().skip(start).take(list_height) {
        let selected = i == sel.selected;
        // Label on the left, detail right-aligned — the pickers' shared layout.
        let detail = truncate_chars(&c.detail, (inner_w / 2).max(1));
        let avail = inner_w.saturating_sub(detail.chars().count() + 1).max(1);
        let label = truncate_chars(&c.label, avail);
        let pad = inner_w
            .saturating_sub(label.chars().count() + detail.chars().count())
            .max(1);
        let line = if selected {
            Line::from(Span::styled(
                format!("{label}{}{detail}", " ".repeat(pad)),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(label, Style::default().fg(theme.user)),
                Span::styled(
                    format!("{}{detail}", " ".repeat(pad)),
                    Style::default().fg(theme.dim),
                ),
            ])
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The `/theme` picker modal: a search line, a hint, and a two-column list
/// (theme name · source) of the baked-in themes plus any user theme files,
/// narrowed by the fuzzy filter. Same chrome as the `/model` selector — and
/// since the highlighted theme is live-previewed, the modal itself repaints in
/// the candidate's colors as the highlight moves.
fn draw_theme_selector(f: &mut Frame, theme: &Theme, sel: &crate::app::ThemeSelector) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 92);
    let height = area.height.saturating_sub(2).clamp(1, 32);
    let rect = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 6 {
        return;
    }

    let rows: Vec<&hrdr_app::ThemeChoice> = sel.rows().collect();
    let search = Line::from(vec![
        Span::styled("Search  ", Style::default().fg(theme.dim)),
        Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
        Span::styled("▌", Style::default().fg(theme.accent)),
    ]);
    let hint = Line::from(Span::styled(
        format!(
            "{} theme{} · ↑↓ preview · Enter apply · Esc cancel",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ),
        Style::default().fg(theme.dim),
    ));

    let list_height = inner.height.saturating_sub(3) as usize; // search + hint + blank
    let inner_w = inner.width as usize;
    let start = if sel.selected >= list_height {
        (sel.selected + 1).saturating_sub(list_height)
    } else {
        0
    };

    let mut lines = vec![search, hint, Line::from("")];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no themes match",
            Style::default().fg(theme.dim),
        )));
    }
    for (i, c) in rows.iter().enumerate().skip(start).take(list_height) {
        let selected = i == sel.selected;
        // Theme name on the left, source right-aligned — the same two-column
        // layout as the `/model` selector.
        let source = truncate_chars(&c.source, (inner_w / 2).max(1));
        let avail = inner_w.saturating_sub(source.chars().count() + 1).max(1);
        let name = truncate_chars(&c.name, avail);
        let pad = inner_w
            .saturating_sub(name.chars().count() + source.chars().count())
            .max(1);
        let line = if selected {
            Line::from(Span::styled(
                format!("{name}{}{source}", " ".repeat(pad)),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(name, Style::default().fg(theme.user)),
                Span::styled(
                    format!("{}{source}", " ".repeat(pad)),
                    Style::default().fg(theme.dim),
                ),
            ])
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The `/resume` session picker modal: a search line, a hint, and a
/// four-column list (id · name · age · cwd) of every saved session, newest
/// first, narrowed by the fuzzy filter. Same chrome as the `/model` selector.
fn draw_session_selector(f: &mut Frame, theme: &Theme, sel: &crate::app::SessionSelector) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(1, 110);
    let height = area.height.saturating_sub(2).clamp(1, 32);
    let rect = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 3 || inner.width < 6 {
        return;
    }

    // Pre-render each visible row's cells: id · name · age · cwd · error.
    let rows: Vec<(String, String, String, String, Option<String>)> = sel
        .rows()
        .map(|m| {
            let ts = chrono::DateTime::from_timestamp(m.updated as i64, 0)
                .map(|t| hrdr_app::relative_time(t.with_timezone(&chrono::Local)))
                .unwrap_or_else(|| "—".to_string());
            let cwd = hrdr_app::display_dir(std::path::Path::new(&m.cwd));
            (m.id.clone(), m.name.clone(), ts, cwd, m.error.clone())
        })
        .collect();

    let search = Line::from(vec![
        Span::styled("Search  ", Style::default().fg(theme.dim)),
        Span::styled(sel.filter.clone(), Style::default().fg(theme.user)),
        Span::styled("▌", Style::default().fg(theme.accent)),
    ]);
    let hint = Line::from(Span::styled(
        format!(
            "{} session{} · ↑↓ select · Enter resume · Esc cancel",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
        ),
        Style::default().fg(theme.dim),
    ));

    let list_height = inner.height.saturating_sub(3) as usize; // search + hint + blank
    let inner_w = inner.width as usize;
    let start = if sel.selected >= list_height {
        (sel.selected + 1).saturating_sub(list_height)
    } else {
        0
    };

    // Column widths from the data: id and age fit their longest value (capped),
    // the cwd gets up to a third of the width, and the name takes the rest.
    // Error rows use the error text as the display name.
    let id_w = rows
        .iter()
        .map(|r| r.0.chars().count())
        .max()
        .unwrap_or(2)
        .min(20);
    let ts_w = rows
        .iter()
        .map(|r| r.2.chars().count())
        .max()
        .unwrap_or(2)
        .min(12);
    let cwd_w = rows
        .iter()
        .filter(|r| r.4.is_none())
        .map(|r| r.3.chars().count())
        .max()
        .unwrap_or(2)
        .min(inner_w / 3);
    let gaps = 3 * 2; // three two-space column separators
    let name_w = inner_w.saturating_sub(id_w + ts_w + cwd_w + gaps).max(4);

    let mut lines = vec![search, hint, Line::from("")];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no sessions match",
            Style::default().fg(theme.dim),
        )));
    }
    let cell = |s: &str, w: usize| format!("{:<w$}", truncate_chars(s, w), w = w);
    for (i, (id, name, ts, cwd, error)) in rows.iter().enumerate().skip(start).take(list_height) {
        let selected = i == sel.selected;
        if let Some(err) = error {
            // Error row: show id + truncated error message in dim style.
            let err_label = format!("[corrupt: {err}]");
            let line = if selected {
                let row = format!("{id}  {err_label}");
                Line::from(Span::styled(
                    format!("{row:<inner_w$}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(theme.user)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{id}  "), Style::default().fg(theme.accent)),
                    Span::styled(err_label, Style::default().fg(theme.dim)),
                ])
            };
            lines.push(line);
        } else {
            let (id, name, ts, cwd) = (
                cell(id, id_w),
                cell(name, name_w),
                cell(ts, ts_w),
                truncate_chars(cwd, cwd_w),
            );
            let line = if selected {
                let row = format!("{id}  {name}  {ts}  {cwd}");
                Line::from(Span::styled(
                    format!("{row:<inner_w$}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(theme.user)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{id}  "), Style::default().fg(theme.accent)),
                    Span::styled(format!("{name}  "), Style::default().fg(theme.user)),
                    Span::styled(format!("{ts}  {cwd}"), Style::default().fg(theme.dim)),
                ])
            };
            lines.push(line);
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Truncate `s` to at most `max` characters, appending `…` when it was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("{}…", s.chars().take(keep).collect::<String>())
}

/// Rows the completion popup shows at once; the selection scrolls the window.
const COMPLETION_MAX_ROWS: usize = 5;

fn draw_completion(f: &mut Frame, app: &App, input_area: Rect, comp: &crate::app::Completions) {
    let theme = &app.theme;
    // Clamped to the frame itself, not just the input pane: on a very short
    // or narrow terminal, an unclamped popup could ask for more rows/columns
    // than exist, leaving its border/top items rendered outside the visible
    // area (ratatui clips silently — no panic, but nothing there to click).
    let frame_area = f.area();
    // At most COMPLETION_MAX_ROWS rows show; the window slides so the
    // selection stays visible.
    let total = comp.items.len();
    let sel = app.completion_idx.min(total - 1);
    let start = if sel >= COMPLETION_MAX_ROWS {
        sel + 1 - COMPLETION_MAX_ROWS
    } else {
        0
    };
    let shown = &comp.items[start..(start + COMPLETION_MAX_ROWS).min(total)];
    // Height: one row per shown item, plus the block's padded row above and
    // below, plus a trailing "N more" hint when the list is windowed.
    let more = total - (start + shown.len());
    let hint_rows = usize::from(more > 0);
    let height = ((shown.len() + hint_rows) as u16 + 2).min(frame_area.height.max(1));
    let widest = shown
        .iter()
        .map(|(n, d)| n.chars().count() + d.chars().count() + 3)
        .max()
        .unwrap_or(24);
    // Outer width adds the block's two padding columns on each side.
    let width = ((widest + BLOCK_PAD_X * 2) as u16)
        .clamp(20, input_area.width.max(20))
        .min(frame_area.width.max(1));
    // Anchor the popup at the column of the token being completed (the `/` or
    // `@`), so it sits above the text it belongs to instead of the pane's
    // left edge. INPUT_PAD_X mirrors the input pane's own left padding.
    let anchor_x = input_area.x + INPUT_PAD_X as u16 + clamp_u16(comp.anchor_col);
    let rect = Rect {
        x: anchor_x.min(frame_area.width.saturating_sub(width)),
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };
    f.render_widget(Clear, rect);
    // Same chrome as the transcript blocks: solid background, two columns of
    // padding either side and one padded row above and below, no border.
    let block = Block::default()
        .style(Style::default().bg(theme.user_bg))
        .padding(Padding::new(BLOCK_PAD_X as u16, BLOCK_PAD_X as u16, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let name_style = if start + i == sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.user)
            };
            Line::from(vec![
                Span::styled(format!(" {name} "), name_style),
                Span::styled(format!(" {desc}"), Style::default().fg(theme.dim)),
            ])
        })
        .collect();
    if more > 0 {
        lines.push(Line::from(Span::styled(
            format!(" … {more} more"),
            Style::default().fg(theme.dim),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Saturate a `usize` row/column count down to `u16`, for the handful of
/// places a `usize` accumulator (kept wide to avoid overflow on a long
/// transcript) finally has to cross into ratatui's `u16`-only APIs
/// (`Paragraph::scroll`, `Rect` fields). A raw `as u16` truncates instead of
/// saturating, wrapping a >65535-row transcript's offset back down near 0.
fn clamp_u16(n: usize) -> u16 {
    n.min(u16::MAX as usize) as u16
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    // Publish the height so key handlers can compute half-page offsets.
    app.transcript_height = area.height;

    // Reserve the rightmost column for the scrollbar. Left padding is applied
    // per-block via pad_line's leading bg-coloured space.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    // A block is laid out against `app` (the header reads live session state), so
    // the frame *reads* everything it needs here and hands the writes back below —
    // the chunks borrow the app until they are painted.
    let pending_goto = app.pending_goto.take();
    let pending_entry = app.pending_scroll_entry.take();
    let (scroll_offset, max_scroll, tool_hits) =
        draw_chunks(f, app, area, text_area, pending_goto, pending_entry);

    // scroll_offset is rows scrolled UP from the bottom; 0 == follow newest. Write
    // it back so "scrolled up" state (and the follow button) is accurate even after
    // the content shrinks.
    app.scroll_offset = scroll_offset;
    app.max_scroll = max_scroll;
    app.tool_hits = tool_hits;

    draw_scrollbar(f, app, area, max_scroll, scroll_offset);
}

/// Lay the transcript out, place the viewport in it, and paint the rows it lands
/// on. Returns what the frame learned: where the reader ended up, how far there is
/// to scroll, and which screen rows a click would land on a tool block in.
fn draw_chunks(
    f: &mut Frame,
    app: &App,
    area: Rect,
    text_area: Rect,
    pending_goto: Option<usize>,
    pending_entry: Option<usize>,
) -> (usize, usize, Vec<(crate::app::HitRect, usize)>) {
    let (chunks, msg_at) = transcript_chunks(app, text_area.width);
    // The screen row each block starts at. A block's rows come out of
    // `render_block` already wrapped and padded to the render width, so a row is a
    // row: no measuring, no re-wrapping, just a running total.
    //
    // Use usize throughout to avoid u16 overflow on long transcripts; only the
    // final Paragraph::scroll cast (u16) is clamped at the last moment.
    let mut cum = Vec::with_capacity(chunks.len() + 1);
    let mut acc: usize = 0;
    cum.push(0usize);
    for c in &chunks {
        acc = acc.saturating_add(c.rows.height());
        cum.push(acc);
    }
    // Resolve a pending /goto to a from-top row offset.
    let goto_top: Option<u16> = pending_goto.and_then(|num| {
        let at = (*msg_at.get(num.checked_sub(1)?)?).min(chunks.len());
        Some(clamp_u16(cum[at]))
    });
    // A tool block that was just expanded or collapsed changed height. While the
    // reader is scrolled up, pull its top to the top of the viewport: the offset
    // is measured from the bottom, so the block would otherwise slide by however
    // many rows it gained or lost. Following the newest output is left alone —
    // the bottom is already pinned.
    let entry_top: Option<u16> = pending_entry.and_then(|idx| {
        let at = chunks.iter().position(|c| c.tool_idx == Some(idx))?;
        (app.scroll_offset > 0).then(|| clamp_u16(cum[at]))
    });
    let goto_top = goto_top.or(entry_top);
    // Total rows at this width.
    // Clamped (not truncated) to u16::MAX: ratatui's `Paragraph::scroll` only
    // takes a u16, and a long enough transcript would otherwise wrap the raw
    // `as u16` cast past 65535 back down near 0, snapping the view to the top.
    let total = clamp_u16(*cum.last().unwrap_or(&0));
    let max_scroll = total.saturating_sub(area.height);
    // Pin the view to the same content while scrolled up. `scroll_offset` is
    // measured from the bottom, so as streaming appends rows `max_scroll` grows
    // and the rendered `scroll = max_scroll - offset` would drift downward. Bump
    // the offset by however much `max_scroll` grew since the last draw (held in
    // `app.max_scroll`) so the from-top position stays put. `offset == 0`
    // (following the newest output) is left untouched — it stays pinned to the
    // bottom by design.
    let mut scroll_offset = app.scroll_offset;
    if scroll_offset > 0 {
        let grown = max_scroll.saturating_sub(clamp_u16(app.max_scroll));
        scroll_offset = scroll_offset.saturating_add(grown as usize);
    }
    // A /goto puts the target message at the top of the viewport.
    if let Some(wrapped_start) = goto_top {
        scroll_offset = max_scroll.saturating_sub(wrapped_start) as usize;
    }
    let offset = clamp_u16(scroll_offset).min(max_scroll);
    let scroll = max_scroll.saturating_sub(offset);

    let scroll_us = scroll as usize;
    let view_end = scroll_us.saturating_add(area.height as usize);

    // Map each tool block's row span to the visible screen rows (clipped to the
    // viewport) so a left click can toggle that tool's expansion. Arithmetic is in
    // usize (cum values) to avoid overflow; only the final HitRect fields are cast
    // back to u16.
    let mut tool_hits = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        let Some(idx) = c.tool_idx else { continue };
        let vis_start = cum[i].max(scroll_us);
        let vis_end = cum[i + 1].min(view_end);
        if vis_end > vis_start {
            tool_hits.push((
                crate::app::HitRect {
                    x: text_area.x,
                    y: text_area.y + (vis_start - scroll_us) as u16,
                    w: text_area.width,
                    h: (vis_end - vis_start) as u16,
                },
                idx,
            ));
        }
    }

    // Paint only what the viewport can show.
    //
    // `Paragraph` lays out from its first row every frame and throws away
    // everything above `scroll`, so handing it the whole transcript makes every
    // frame cost the *whole session* — at a thousand entries that was ~24ms of a
    // ~26ms frame, and it grows without bound. Each block is self-contained, so the
    // same pixels come out of the blocks the viewport overlaps, with the scroll
    // rebased to the first of them.
    //
    // `cum` is nondecreasing, so binary-search it: `first` is the block that owns
    // row `scroll`, `last` the one past the final visible row.
    let first = cum.partition_point(|&c| c <= scroll_us).saturating_sub(1);
    let last = cum.partition_point(|&c| c < view_end).min(chunks.len());
    let mut visible: Vec<Line<'static>> = chunks
        .get(first..last.max(first))
        .unwrap_or_default()
        .iter()
        .flat_map(|c| c.rows.rows().iter().cloned().collect::<Vec<_>>())
        .collect();
    // Rows of the first visible block that sit above the viewport.
    let inner_scroll = clamp_u16(scroll_us.saturating_sub(cum[first.min(chunks.len())]));

    // Highlight the active /find query. Only the rows about to be painted need it,
    // and it only restyles them — the blocks in the cache stay as they were.
    if let Some(needle) = app
        .find
        .query
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|q| !q.is_empty())
    {
        let hl = Style::default()
            .fg(Color::Black)
            .bg(app.theme.warn)
            .add_modifier(Modifier::BOLD);
        for line in visible.iter_mut() {
            *line = highlight_line(std::mem::take(line), &needle, hl);
        }
    }

    let para = Paragraph::new(visible).wrap(Wrap { trim: false });
    f.render_widget(para.scroll((inner_scroll, 0)), text_area);

    (offset as usize, max_scroll as usize, tool_hits)
}

/// The scrollbar: total session length, and where the reader is within it.
fn draw_scrollbar(f: &mut Frame, app: &App, area: Rect, max_scroll: usize, offset: usize) {
    // ratatui maps `position` over `0..=content_length-1`, so content_length is the
    // number of scroll positions (max_scroll + 1) — not the raw row total, or the
    // thumb never reaches the bottom when following.
    let scroll = max_scroll.saturating_sub(offset);
    let mut sb_state = ScrollbarState::new(max_scroll + 1)
        .viewport_content_length(area.height as usize)
        .position(scroll);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .style(Style::default().fg(app.theme.dim));
    // Scrollbar lives in the rightmost 1-column strip so it never overlaps the
    // transcript content (important for markdown tables whose right-border
    // characters — ┐, ┤, ┘, │ — would otherwise get clobbered by the track).
    let sb_area = Rect {
        x: area.x + area.width.saturating_sub(1),
        width: 1,
        ..area
    };
    f.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
}

fn draw_todos(f: &mut Frame, app: &App, area: Rect, todos: &[hrdr_agent::Todo]) {
    if todos.is_empty() {
        return;
    }

    // The input pane's chrome, with a green rule instead of the prompt's.
    let bg = app.theme.user_bg;
    let inner = draw_pane(f, &app.theme, area, app.theme.success);

    let frame = SPINNER[(app.header_anchor.elapsed().as_millis() / 120) as usize % SPINNER.len()];

    let lines: Vec<Line<'static>> = todos
        .iter()
        .map(|t| {
            let (mark, color) = match t.status.as_str() {
                "completed" => ("✓", app.theme.success),
                "cancelled" => ("✗", app.theme.dim),
                "in_progress" => (frame, app.theme.warn),
                _ => (" ", app.theme.dim),
            };
            Line::from(Span::styled(
                format!("{mark} {}", t.content),
                Style::default().fg(color).bg(bg),
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

/// The live sub-agent panel: one block per running `task`, each showing a header
/// (its `↳ task …` line) plus the tail of its output (collapsed) or the whole
/// log (expanded). A left click on a row toggles that agent's expansion; the
/// clickable rects are recorded in `app.subagent_hits`.
///
/// When the total content rows exceed `SUBAGENT_PANEL_MAX_ROWS`, the panel
/// scrolls so the newest agents/logs stay visible at the bottom.
fn draw_subagents(f: &mut Frame, app: &mut App, area: Rect, items: &[hrdr_app::PaneRow]) {
    let (accent, success) = (app.theme.accent, app.theme.success);
    // The input pane's chrome, with the accent rule a running agent wears — the
    // todo panel's is green.
    let bg = app.theme.user_bg;
    let inner = draw_pane(f, &app.theme, area, accent);

    // One row per agent, newest pinned to the bottom when they overflow. A
    // running row leads with the same animated spinner as the inference loader
    // (driven off the free-running header clock so it ticks even while idle).
    let scroll = subagent_scroll(items.len(), inner.height);
    let frame = SPINNER[(app.header_anchor.elapsed().as_millis() / 120) as usize % SPINNER.len()];
    let lines: Vec<Line<'static>> = items
        .iter()
        .map(|item| {
            let fg = match item.status {
                hrdr_app::PaneStatus::Done => success,
                _ => accent,
            };
            // The agent you are looking at is the highlighted row. That is the
            // whole signal — a caret as well would be saying it twice, and it
            // costs two columns of every other row to do it.
            let marker = hrdr_app::pane_row_marker(item.status, frame);
            let mut style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
            if item.active {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(Span::styled(
                format!("{marker} {}", item.title.trim()),
                style,
            ))
        })
        .collect();

    // A click on a visible row switches the view to that agent.
    app.subagent_hits = items
        .iter()
        .enumerate()
        .skip(scroll as usize)
        .take(inner.height as usize)
        .map(|(i, item)| {
            (
                crate::app::HitRect {
                    x: inner.x,
                    y: inner.y + (i as u16 - scroll),
                    w: inner.width,
                    h: 1,
                },
                item.id,
            )
        })
        .collect();

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The inference loader: spinner + live stats (context size, in/out ratio,
/// token throughput) shown above the input while a turn runs.
fn draw_loader(f: &mut Frame, app: &App, area: Rect) {
    // The loader describes **the agent you are looking at**, and the clock it reads
    // is that agent's own. Watching a sub-agent work used to show the *main* agent's
    // spinner, throughput and elapsed time — and a sub-agent grinding away under an
    // idle main agent showed no loader at all.
    let pane = app.panes.active_pane();
    let turn = &pane.turn;
    // The model's own working time: the tool calls it waited on don't count, so
    // the clock freezes while they run rather than inflating the turn.
    let elapsed = turn.infer_elapsed();
    let frame = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];
    let speed = turn.tok_per_sec();

    let ctx = match pane.state.usage.last() {
        Some((prompt, completion)) => {
            let ratio = if completion > 0 {
                prompt as f64 / completion as f64
            } else {
                0.0
            };
            format!("ctx {prompt} tok · in/out {prompt}/{completion} ({ratio:.1}:1)")
        }
        None => "ctx —".to_string(),
    };

    // "started …" segment, respecting the timestamp style (omitted when off).
    let started_at = turn.started_at.map(hrdr_app::time_from_system);
    let started = match (app.timestamp_style, started_at) {
        (TimestampStyle::None, _) | (_, None) => String::new(),
        (TimestampStyle::Relative, Some(t)) => format!("  ·  started {}", relative_time(t)),
        (TimestampStyle::Exact, Some(t)) => format!("  ·  started {}", t.format("%H:%M")),
    };
    // Compaction is the session's agent summarizing itself — not something a
    // sub-agent's pane should claim to be doing.
    let text = if pane.compacting {
        format!(
            " {frame} compacting context — summarizing the conversation…  ·  {:.1}s{started}",
            elapsed.as_secs_f64(),
        )
    } else {
        // Time to first token: how long the provider took to start streaming.
        let ttft = match turn.ttft() {
            Some(secs) => format!("  ·  ttft {secs:.2}s"),
            None => String::new(),
        };
        let phase = if turn.first_token_at.is_some() {
            "generating"
        } else {
            "inferring"
        };
        format!(
            " {frame} {phase}  ·  {ctx}  ·  {speed:.1} tok/s ({} out){ttft}  ·  {:.1}s{started}",
            turn.out_tokens,
            elapsed.as_secs_f64(),
        )
    };
    f.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .fg(app.theme.warn)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

/// Paint a pane wearing the chrome of the user's own surfaces: the prompt's
/// background, a transcript block's padding (two columns either side, one blank
/// row above and below), and a `bar`-colored rule down the left edge. Returns
/// the inner rect for the caller's content.
///
/// The input pane and the todo list share this so they can't drift apart; the
/// bar's color is all that tells them apart.
fn draw_pane(f: &mut Frame, theme: &Theme, area: Rect, bar: Color) -> Rect {
    let bg = theme.user_bg;
    let block = Block::default()
        .style(Style::default().bg(bg))
        .padding(Padding::new(INPUT_PAD_X as u16, INPUT_PAD_X as u16, 1, 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rule: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled(BORDER_BAR, Style::default().fg(bar).bg(bg))))
        .collect();
    f.render_widget(Paragraph::new(rule), Rect { width: 1, ..area });
    inner
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = draw_pane(f, &app.theme, area, app.theme.prompt_border);
    app.editor.render(f, inner);

    // Both banners float on the same row above the pane; the quit confirmation
    // takes the spot when both would apply. Only the follow banner is clickable.
    if app.quit_armed {
        // `●` (U+25CF) is neutral-width and lives in the same geometric-shapes
        // block as the bars and blocks the TUI already draws, so it can't be
        // emoji-substituted or rendered double-wide the way `⚠` was.
        banner(
            f,
            area,
            "●",
            "Press Ctrl+C again to quit",
            Color::White,
            app.theme.error,
        );
        app.follow_button = None;
    } else if app.scroll_offset > 0 {
        // The arrows point at the output the banner returns to.
        let rect = banner(
            f,
            area,
            "↓",
            "Press END to follow output",
            Color::Black,
            app.theme.warn,
        );
        app.follow_button = Some(crate::app::HitRect {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: rect.height,
        });
    } else {
        app.follow_button = None;
    }
}

/// Rows above the input pane that a banner floats on, clear of its padding.
const BANNER_LIFT: u16 = 2;

/// Render a banner over the transcript, just above the input pane: a bold,
/// centered, single-row `label` in `fg` on `bg`, flanked by `icon` on each side.
/// Returns its rect, for click hit-testing.
///
/// Both banners — "follow output" and the quit confirmation — go through here,
/// so they sit on the same row and differ only in their icon, text, and colors.
fn banner(f: &mut Frame, input: Rect, icon: &str, label: &str, fg: Color, bg: Color) -> Rect {
    let text = format!(" {icon} {label} {icon} ");
    let w = (text.width() as u16).min(input.width);
    let rect = Rect {
        x: input.x + input.width.saturating_sub(w) / 2,
        y: input.y.saturating_sub(BANNER_LIFT),
        width: w,
        height: 1,
    };
    let style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
    f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), rect);
    rect
}

/// One status-bar section: `(priority, spans)`. Lower priority is kept longer;
/// higher is dropped first in truncate mode.
type StatusSection = (u8, Vec<Span<'static>>);

fn status_section_width(s: &StatusSection) -> usize {
    s.1.iter().map(Span::width).sum()
}

/// Build the status-bar sections from the shared content model
/// ([`hrdr_app::status_sections`] — the shared sections/priorities),
/// mapping each color role onto the terminal theme.
/// Build the status bar's `(left, right)` sections. The left side carries the
/// dir/branch/tokens/context/model chrome; the right side carries the session
/// name, laid out flush-right by [`draw_statusbar`].
fn build_status_sections(app: &App) -> (Vec<StatusSection>, Vec<StatusSection>) {
    let t = &app.theme;
    // The bar describes **the agent you are looking at**. Model, provider, context
    // gauge, tokens and cost all come off the active pane's state: a sub-agent on
    // a different provider fills a different window at a different price, and a bar
    // that always reported the main agent's figures was describing a conversation
    // that wasn't on screen.
    let pane = app.panes.active_pane();
    // Its turn's time-to-first-token — every agent is clocked, so this is the one
    // on screen rather than the main agent's borrowed for it.
    let ttft = pane.turn.ttft();
    // The session name is the session's, whichever agent is being viewed — it is
    // what the file on disk is called.
    let session = truncate_chars(&app.state().name, 28);
    let inputs = hrdr_app::StatusInputs {
        dir: &app.dir,
        branch: app.branch.as_deref(),
        tokens_in: pane.state.usage.tokens_in,
        tokens_out: pane.state.usage.tokens_out,
        ctx_used: pane.state.usage.ctx_used(),
        context_window: pane.state.usage.context_window,
        // Both belong to the agent being shown: they set where *its* gauge turns
        // red, and a sub-agent on a 64k local model has its own threshold.
        auto_compact_enabled: pane.auto_compact,
        compaction_reserved: pane.compaction_reserved,
        provider: Some(pane.provider()),
        model: pane.model(),
        session: Some(session.as_str()),
        effort: pane.effort.as_deref(),
        ttft,
        nerd_icons: app.icon_mode == hjkl_icons::IconMode::Nerd,
    };
    let to_sections = |segs: Vec<hrdr_app::StatusSeg>| -> Vec<StatusSection> {
        segs.into_iter()
            .map(|seg| {
                let spans = seg
                    .runs
                    .into_iter()
                    .map(|run| Span::styled(run.text, status_role_style(run.role, t)))
                    .collect();
                (seg.priority, spans)
            })
            .collect()
    };
    (
        to_sections(hrdr_app::status_sections(&inputs)),
        to_sections(hrdr_app::status_right_sections(&inputs)),
    )
}

/// Resolve a shared theme slot to this theme's concrete color.
fn slot_color(slot: hrdr_app::ThemeSlot, t: &Theme) -> Color {
    use hrdr_app::ThemeSlot;
    match slot {
        ThemeSlot::User => t.user,
        ThemeSlot::Assistant => t.assistant,
        ThemeSlot::Dim => t.dim,
        ThemeSlot::Warn => t.warn,
        ThemeSlot::Success => t.success,
        ThemeSlot::Error => t.error,
        ThemeSlot::Accent => t.accent,
        ThemeSlot::Accent2 => t.accent2,
    }
}

/// Terminal style for a shared status color role (semantics live in
/// [`hrdr_app::status_role_style`]; only slot → color is local).
fn status_role_style(role: hrdr_app::StatusRole, t: &Theme) -> Style {
    let spec = hrdr_app::status_role_style(role);
    let mut style = Style::default().fg(match spec.fg {
        Some(slot) => slot_color(slot, t),
        None => Color::Black, // inverted text over the bg slot
    });
    if let Some(bg) = spec.bg {
        style = style.bg(slot_color(bg, t));
    }
    if spec.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

/// Separator width: " │ ".
const STATUS_SEP_W: usize = 3;

/// Rows the status bar needs in wrap mode at `width`.
fn status_wrap_rows(sections: &[StatusSection], width: usize) -> u16 {
    let mut rows: u16 = 1;
    let mut cur_w = 0usize;
    let mut empty = true;
    for s in sections {
        let w = status_section_width(s);
        let needed = if empty { w } else { STATUS_SEP_W + w };
        if !empty && cur_w + needed > width {
            rows += 1;
            cur_w = w; // section starts the new row
        } else {
            cur_w += needed;
            empty = false;
        }
    }
    rows
}

/// Render the status bar into `area` according to the active mode (Truncate or
/// Wrap; None is handled by the caller not allocating a row). The `left`
/// sections flow from the left; the `right` sections (the session) sit flush
/// against the right edge.
fn draw_statusbar(
    f: &mut Frame,
    app: &App,
    area: Rect,
    left: &[StatusSection],
    right: &[StatusSection],
) {
    let t = &app.theme;
    let width = area.width as usize;
    let inner = inner_width(width) as usize;
    let lines = match app.statusbar_mode {
        StatusBarMode::Wrap => {
            // Wrap has no right edge to pin to; pack both sides into the flow.
            let combined: Vec<StatusSection> = left.iter().chain(right).cloned().collect();
            status_wrap_lines(&combined, inner, t)
        }
        _ => vec![status_truncate_line_split(left, right, inner, t)],
    };
    // The same chrome a transcript block wears: two columns either side, a blank
    // row above and below. No background of its own, and no bar.
    f.render_widget(
        Paragraph::new(render_block(lines, width, Color::Reset, None)),
        area,
    );
}

/// One-row status bar with a left-flowing group and a right-aligned group: the
/// right sections are pinned to the right edge, the left sections truncate into
/// whatever width is left over.
fn status_truncate_line_split(
    left: &[StatusSection],
    right: &[StatusSection],
    width: usize,
    t: &Theme,
) -> Line<'static> {
    // Right group: joined verbatim (it's small — just the session).
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    for (i, (_, ss)) in right.iter().enumerate() {
        if i > 0 {
            right_spans.push(Span::styled(" │ ", Style::default().fg(t.dim)));
        }
        right_spans.extend(ss.iter().cloned());
    }
    let right_w: usize = right_spans.iter().map(Span::width).sum();
    let gap = if right_w > 0 { 2 } else { 0 };

    let left_line = status_truncate_line(left, width.saturating_sub(right_w + gap), t);
    let left_w: usize = left_line.spans.iter().map(Span::width).sum();

    let mut spans = left_line.spans;
    if right_w > 0 {
        let pad = width.saturating_sub(left_w + right_w).max(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right_spans);
    }
    Line::from(spans)
}

/// One-row status bar that drops the least-important sections until it fits.
fn status_truncate_line(sections: &[StatusSection], width: usize, t: &Theme) -> Line<'static> {
    let widths: Vec<usize> = sections.iter().map(status_section_width).collect();
    let mut keep = vec![true; sections.len()];
    loop {
        let kept: Vec<usize> = (0..sections.len()).filter(|&i| keep[i]).collect();
        let total: usize = kept.iter().map(|&i| widths[i]).sum::<usize>()
            + STATUS_SEP_W * kept.len().saturating_sub(1);
        if total <= width || kept.len() <= 1 {
            break;
        }
        // Drop the kept section with the largest (priority, index): least
        // important first, and later-in-order to break ties (e.g. out before in).
        if let Some(&drop) = kept.iter().max_by_key(|&&i| (sections[i].0, i)) {
            keep[drop] = false;
        }
    }
    let truncated = keep.iter().any(|k| !k);
    let mut spans: Vec<Span> = Vec::new();
    let mut first = true;
    for (i, (_, ss)) in sections.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if !first {
            spans.push(Span::styled(" │ ", Style::default().fg(t.dim)));
        }
        spans.extend(ss.iter().cloned());
        first = false;
    }
    if truncated {
        let used: usize = spans.iter().map(Span::width).sum();
        if used + 2 <= width {
            spans.push(Span::styled(" …", Style::default().fg(t.dim)));
        }
    }
    Line::from(spans)
}

/// Multi-row status bar that packs sections across rows so nothing is dropped.
fn status_wrap_lines(sections: &[StatusSection], width: usize, t: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    for s in sections {
        let w = status_section_width(s);
        let needed = if cur.is_empty() { w } else { STATUS_SEP_W + w };
        if !cur.is_empty() && cur_w + needed > width {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur_w = 0;
        }
        if !cur.is_empty() {
            cur.push(Span::styled(" │ ", Style::default().fg(t.dim)));
            cur_w += STATUS_SEP_W;
        }
        cur.extend(s.1.iter().cloned());
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Restyle every (ASCII case-insensitive) occurrence of `needle` within `line`
/// with `hl`, preserving each span's original style elsewhere. `needle` must be
/// lowercase. Operates on byte indices (safe because `to_ascii_lowercase`
/// preserves length).
fn highlight_line(line: Line<'static>, needle: &str, hl: Style) -> Line<'static> {
    if needle.is_empty() {
        return line;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let content = span.content.into_owned();
        let lower = content.to_ascii_lowercase();
        let style = span.style;
        let mut start = 0;
        let mut search = 0;
        while let Some(rel) = lower[search..].find(needle) {
            let at = search + rel;
            if at > start {
                spans.push(Span::styled(content[start..at].to_string(), style));
            }
            let end = at + needle.len();
            spans.push(Span::styled(content[at..end].to_string(), hl));
            start = end;
            search = end;
        }
        if start < content.len() {
            spans.push(Span::styled(content[start..].to_string(), style));
        }
    }
    Line::from(spans)
}

/// Cache key for an entry's rendered *body* — the markdown/tool render, before
/// the block chrome around it.
/// Fields: (content_fingerprint, render_width, expand_all, show_reasoning).
type BodyKey = (u64, u16, bool, bool);

/// Cache key for a finished *block* — the body plus the chrome that frames it,
/// which the body key can't see: the block kind, the timestamp/stats rows lent to
/// it by a following entry, and whether its bottom pad is dropped because the
/// block after it is untinted too.
type BlockKey = (BodyKey, u64);

/// Rows shared between the cache and the frames that paint them.
type Rows = Rc<Vec<Line<'static>>>;

/// A render cache: one slot per transcript entry, holding the key it was rendered
/// under and the rows it produced.
type SlotCache<K> = RefCell<HashMap<usize, (K, Rows)>>;

thread_local! {
    // Incremental syntect state: a streaming block's content grows every token,
    // so only pay for the new lines. (The rendered rows are cached per entry by
    // BLOCK_CACHE below.)
    static INC_HL: RefCell<hrdr_app::HighlightCache> = RefCell::new(hrdr_app::HighlightCache::new());
    // Both caches are keyed by *transcript index*, holding one slot per entry —
    // not by content, with the whole map dropped when it outgrows a cap. A cap is
    // what made a long session lurch: past it, every frame evicted the entries the
    // next frame needed, so a 2000-entry transcript re-rendered itself from scratch
    // ~8 times a second. One slot per entry can't thrash (the working set *is* the
    // transcript), and it can't grow without bound either. `clear_transcript_cache`
    // drops both when the indices themselves move (prune, resume, /clear).
    static BODY_CACHE: SlotCache<BodyKey> = RefCell::new(HashMap::new());
    static BLOCK_CACHE: SlotCache<BlockKey> = RefCell::new(HashMap::new());
    // Heights of the blocks that are rebuilt rather than cached (the header). Keyed
    // by the same block key, so a header whose *shape* changed — a model with a
    // longer name, an effort row appearing — is measured again rather than trusted.
    static LAZY_HEIGHTS: RefCell<HashMap<BlockKey, usize>> = RefCell::new(HashMap::new());
}

/// Syntax-highlight `content` into unpadded lines on `bg` — the raw text, one
/// `Line` per source line, no gutter and no width fill. Callers that want a
/// solid code rectangle (fenced markdown blocks) pad it themselves; callers
/// rendering into an already-padded block (the `write` tool body) use it as-is.
fn highlight_lines(lang: &str, content: &str, bg: Color) -> Vec<Line<'static>> {
    // Incremental: a streaming block only highlights its new lines per frame
    // (the shared cache resumes syntect state from the last call).
    let hl_lines = INC_HL.with(|c| c.borrow_mut().highlight(lang, content));
    hl_lines
        .into_iter()
        .map(|ranges| {
            let spans: Vec<Span<'static>> = ranges
                .into_iter()
                .filter_map(|(style, piece)| {
                    let piece = piece.trim_end_matches(['\n', '\r']);
                    if piece.is_empty() {
                        return None;
                    }
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    Some(Span::styled(
                        piece.to_string(),
                        Style::default().fg(fg).bg(bg),
                    ))
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

// ── Session header ─────────────────────────────────────────────────────────

/// Columns between the logo and the details column.
const HEADER_GAP: usize = 4;

/// Width of the details column's key field; values start after it, so they all
/// line up regardless of key length.
const DETAIL_KEY_W: usize = 9;
/// Width a tool-call detail value is clipped to while its block is collapsed.
const DETAIL_VALUE_W: usize = 100;

/// The cursor path the splash animation traces through `art`: every glyph cell,
/// column by column, so the highlight sweeps left-to-right across the letters.
///
/// Derived from the art rather than hand-listed (as `hjkl_splash::presets` does)
/// so changing the art can't silently desynchronise the two. Recomputed per
/// frame — it's a hundred-odd cells, and caching it globally would pin the first
/// art the process ever rendered.
fn logo_path(art: &str) -> Vec<(u8, u8, char)> {
    let rows: Vec<Vec<char>> = art.lines().map(|l| l.chars().collect()).collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut path = Vec::new();
    for col in 0..cols {
        for (row_idx, row) in rows.iter().enumerate() {
            match row.get(col) {
                Some(&ch) if !ch.is_whitespace() => path.push((row_idx as u8, col as u8, ch)),
                _ => {}
            }
        }
    }
    path
}

/// Rows and columns `art` occupies.
fn logo_size(art: &str) -> (u16, u16) {
    let rows = art.lines().count() as u16;
    let cols = art.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    (rows, cols)
}

/// The session banner: the animated logo on the left, session details on the
/// right. `anchor` is the app's persistent animation clock — passing
/// `Instant::now()` here would re-anchor every frame and freeze the tick at 0.
fn header_lines(app: &App, anchor: std::time::Instant, width: u16) -> Vec<Line<'static>> {
    use hjkl_splash::{CellKind, Layout, Rgb, Splash, default_trail_color};

    let theme = &app.theme;
    let art = app.logo;
    let (rows, cols) = logo_size(art);
    let path = logo_path(art);
    let splash = Splash::new(art, &path).with_anchor(anchor);
    let layout = Layout {
        origin_x: 0,
        origin_y: 0,
        rows,
        cols,
    };

    // Paint the animation's cells into a grid, then flatten each row to a Line.
    // `cells()` yields art first, then the trail (oldest → cursor), and later
    // cells overwrite earlier ones — so a plain grid write in iteration order
    // gives the same result the crate's own renderer produces.
    let blank = (' ', Style::default().fg(theme.dim));
    let mut grid = vec![vec![blank; cols as usize]; rows as usize];
    let rgb = |Rgb(r, g, b): Rgb| Color::Rgb(r, g, b);
    for cell in splash.cells(layout) {
        let (Some(row), true) = (grid.get_mut(cell.y as usize), cell.x < cols) else {
            continue;
        };
        let style = match cell.kind {
            CellKind::Art => Style::default().fg(theme.dim),
            CellKind::Trail { age } => Style::default().fg(rgb(default_trail_color(age))),
            CellKind::Cursor => Style::default().fg(theme.user).bold(),
        };
        row[cell.x as usize] = (cell.ch, style);
    }

    // The details column, beside the logo. Every row is a `key value` pair on
    // the same two columns, so the values line up down the block.
    let key = Style::default().fg(theme.dim);
    let val = Style::default().fg(theme.assistant);
    let mut details: Vec<Vec<Span<'static>>> = Vec::new();
    let mut field = |name: &str, value: String, style: Style| {
        details.push(vec![
            Span::styled(format!("{name:<w$}", w = DETAIL_KEY_W), key),
            Span::styled(value, style),
        ]);
    };
    field(
        "version",
        env!("CARGO_PKG_VERSION").to_string(),
        Style::default().fg(theme.user).bold(),
    );
    // The header block describes the agent on screen, like the status bar.
    let pane = app.panes.active_pane();
    field("model", pane.model().to_string(), val);
    field("provider", pane.provider().to_string(), val);
    if let Some(e) = &pane.effort {
        field("effort", e.clone(), val);
    }
    field("cwd", app.dir.clone(), val);

    // Zip the two columns, padding the logo side to a fixed width so the
    // details always start at the same column.
    let logo_w = cols as usize + HEADER_GAP;
    let inner = inner_width(width as usize) as usize;
    (0..grid.len().max(details.len()))
        .map(|i| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0;
            if let Some(row) = grid.get(i) {
                for (ch, style) in row {
                    spans.push(Span::styled(ch.to_string(), *style));
                }
                used = cols as usize;
            }
            if let Some(detail) = details.get(i)
                && !detail.is_empty()
            {
                // Skip the details entirely on a viewport too narrow for them.
                if logo_w + 12 <= inner {
                    spans.push(Span::raw(" ".repeat(logo_w - used)));
                    spans.extend(detail.iter().cloned());
                }
            }
            Line::from(spans)
        })
        .collect()
}

// ── Block rendering ────────────────────────────────────────────────────────
//
// Every transcript entry is a *block*: a full-width rectangle on its own
// background with one column of padding on the left/right and one blank row
// above/below. `render_block` is the single code path all entry kinds go
// through, so a change to block chrome lands everywhere at once.

/// Columns of padding between a block's edge and its content, per side. The
/// vertical padding is one blank row above and below (see [`render_block`]).
pub(crate) const BLOCK_PAD_X: usize = 2;

/// The input pane wears the same horizontal padding as a transcript block.
const INPUT_PAD_X: usize = BLOCK_PAD_X;

/// The bar drawn down the left edge of the user's own surfaces.
pub(crate) const BORDER_BAR: &str = "┃";

/// The visual identity of a transcript block. `bg` is the only thing that
/// varies between kinds today; text styling is decided by each body builder.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// The session banner: animated logo + session details.
    Header,
    User,
    Assistant,
    Reasoning,
    Tool,
    /// Slash-command output and system notices (`/diff`, `/sessions`, …).
    Command,
    /// The per-turn stats line.
    Stats,
    /// A queued user message, awaiting its turn.
    Queued,
}

impl BlockKind {
    /// The bar drawn down the block's left edge, if any. Marks the surfaces that
    /// are the user's own: the prompt, and a prompt still queued.
    fn border(self, theme: &Theme) -> Option<Color> {
        matches!(self, BlockKind::User | BlockKind::Queued).then_some(theme.prompt_border)
    }

    /// The block's background. This is the override point: give a kind its own
    /// theme slot here and every line it renders picks it up.
    fn bg(self, theme: &Theme) -> Color {
        match self {
            // A tool call is something the user's turn set in motion, so it
            // sits on the prompt's background rather than a surface of its own.
            BlockKind::User | BlockKind::Queued | BlockKind::Tool => theme.user_bg,
            BlockKind::Command => theme.command_bg,
            // The banner, the model's output, and its thinking sit on the
            // terminal's own background — no override, so they read as the page.
            BlockKind::Header | BlockKind::Assistant | BlockKind::Reasoning => Color::Reset,
            BlockKind::Stats => theme.stats_bg,
        }
    }
}

/// Width available to a block's *content*, i.e. minus the horizontal padding.
fn inner_width(width: usize) -> u16 {
    width.saturating_sub(BLOCK_PAD_X * 2).max(1) as u16
}

/// Wrap `body` (built at [`inner_width`]) in the shared block chrome: a blank
/// padded row above and below, and one padded column either side, all filled
/// with `bg`. Empty bodies render nothing.
fn render_block(
    body: Vec<Line<'static>>,
    width: usize,
    bg: Color,
    border: Option<Color>,
) -> Vec<Line<'static>> {
    if body.is_empty() {
        return Vec::new();
    }
    let inner = inner_width(width) as usize;
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(pad_line(Vec::new(), width, bg, border));
    for mut line in body {
        // Body spans inherit the block's background unless they set their own
        // (a fenced code block inside a message keeps its distinct bg).
        for span in &mut line.spans {
            if span.style.bg.is_none() {
                span.style = span.style.bg(bg);
            }
        }
        // Wrap to the content width here rather than letting ratatui re-wrap the
        // padded line: its continuation rows would start at column 0, outside
        // the block's padding and background.
        for rows in wrap_spans(line.spans, inner) {
            out.push(pad_line(rows, width, bg, border));
        }
    }
    out.push(pad_line(Vec::new(), width, bg, border));
    out
}

/// Wrap `spans` to `max_w` display columns, preferring to break at the last
/// whitespace on the row and falling back to a hard break for a single word
/// longer than the row. Each returned row is a fresh span vector carrying the
/// original styles.
fn wrap_spans(spans: Vec<Span<'static>>, max_w: usize) -> Vec<Vec<Span<'static>>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= max_w {
        return vec![spans];
    }
    // Flatten to styled characters — the only representation where a break can
    // land mid-span.
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        // Advance by display columns, not chars: a CJK glyph is two columns
        // wide, and overshooting would push the row past the block's padding.
        let mut end = start;
        let mut used = 0usize;
        while end < chars.len() {
            let cw = UnicodeWidthChar::width(chars[end].0).unwrap_or(0);
            if used + cw > max_w {
                break;
            }
            used += cw;
            end += 1;
        }
        // A single glyph wider than the row would loop forever otherwise.
        if end == start {
            end += 1;
        }
        if end < chars.len() {
            // Break after the last space that fits, when there is one.
            if let Some(pos) = chars[start..end]
                .iter()
                .rposition(|(c, _)| c.is_whitespace())
            {
                end = start + pos + 1;
            }
        }
        rows.push(spans_from_chars(&chars[start..end]));
        start = end;
    }
    rows
}

/// Rebuild spans from styled characters, coalescing runs that share a style.
fn spans_from_chars(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for &(c, style) in chars {
        match out.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => out.push(Span::styled(c.to_string(), style)),
        }
    }
    out
}

/// Pad a line of spans to full `width` with `bg`-coloured spaces: `BLOCK_PAD_X`
/// leading columns, then a right fill. When `border` is set, the first of those
/// columns is a [`BORDER_BAR`] in that color, so the bar runs the block's whole
/// height — padding rows included.
fn pad_line(
    mut spans: Vec<Span<'static>>,
    width: usize,
    bg: Color,
    border: Option<Color>,
) -> Line<'static> {
    let bg_only = Style::default().bg(bg);
    match border {
        Some(fg) => {
            spans.insert(0, Span::styled(" ".repeat(BLOCK_PAD_X - 1), bg_only));
            spans.insert(0, Span::styled(BORDER_BAR, Style::default().fg(fg).bg(bg)));
        }
        None => spans.insert(0, Span::styled(" ".repeat(BLOCK_PAD_X), bg_only)),
    }
    let used: usize = spans.iter().map(Span::width).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), bg_only));
    }
    Line::from(spans)
}

/// Compute a stable fingerprint for the content-dependent parts of a transcript
/// entry. Only the parts that affect the visual output are hashed; the
/// timestamp meta line (which changes on /timestamps) is rendered separately and
/// is intentionally excluded so timestamp-only frames still get cache hits.
///
/// Uses the precomputed [`Entry::content_hash`] for most entry types; only Tool
/// entries need the per-frame `expanded` flag mixed in at lookup time.
fn entry_content_hash(entry: &Entry, expand_all: bool) -> u64 {
    match &entry.kind {
        // The header animates and reads live session state; it is never cached.
        EntryKind::Header => 0,
        EntryKind::Tool { expanded, .. } => {
            // Mix the effective expand state into the cached content hash.
            let expand = *expanded || expand_all;
            entry.content_hash ^ (expand as u64)
        }
        _ => entry.content_hash,
    }
}

/// Clear the thread-local transcript render caches. Call after mutating the
/// transcript vector (prune, clear, truncate) so stale entry indices — which are
/// what both caches are keyed by — don't serve one entry's rows for another.
pub(crate) fn clear_transcript_cache() {
    BODY_CACHE.with(|c| c.borrow_mut().clear());
    BLOCK_CACHE.with(|c| c.borrow_mut().clear());
    LAZY_HEIGHTS.with(|c| c.borrow_mut().clear());
}

/// What the session header *shows* — everything but the animation, which changes
/// every frame and changes nothing about the header's shape.
fn header_hash(app: &App) -> u64 {
    let mut h = DefaultHasher::new();
    let pane = app.panes.active_pane();
    pane.model().hash(&mut h);
    pane.provider().hash(&mut h);
    pane.effort.hash(&mut h);
    app.dir.hash(&mut h);
    app.logo.hash(&mut h);
    h.finish()
}

/// The rendered body of entry `idx`, from [`BODY_CACHE`] when `key` still
/// matches the slot, otherwise from `render()`.
///
/// Shared by [`Rc`]: an unchanged entry costs a refcount bump per frame, not a
/// copy of every [`Span`] in it.
fn cached_body<F>(idx: usize, key: BodyKey, render: F) -> Rows
where
    F: FnOnce() -> Vec<Line<'static>>,
{
    if let Some(hit) = BODY_CACHE.with(|c| {
        c.borrow()
            .get(&idx)
            .filter(|(k, _)| *k == key)
            .map(|(_, rows)| Rc::clone(rows))
    }) {
        return hit;
    }
    let rows = Rc::new(render());
    BODY_CACHE.with(|c| c.borrow_mut().insert(idx, (key, Rc::clone(&rows))));
    rows
}

/// The finished rows of entry `idx`'s block, from [`BLOCK_CACHE`] when `key`
/// still matches, otherwise from `render()`.
fn cached_block<F>(idx: usize, key: BlockKey, render: F) -> Rows
where
    F: FnOnce() -> Rows,
{
    if let Some(hit) = BLOCK_CACHE.with(|c| {
        c.borrow()
            .get(&idx)
            .filter(|(k, _)| *k == key)
            .map(|(_, rows)| Rc::clone(rows))
    }) {
        return hit;
    }
    let rows = render();
    BLOCK_CACHE.with(|c| c.borrow_mut().insert(idx, (key, Rc::clone(&rows))));
    rows
}

/// How tall the block under `key` came out the last time it was built, for the
/// blocks whose *rows* can't be cached but whose height doesn't change: the
/// animated header. Knowing the height is enough to place the viewport, and a
/// viewport that doesn't reach the block never builds it.
fn lazy_height(key: BlockKey) -> Option<usize> {
    LAZY_HEIGHTS.with(|c| c.borrow().get(&key).copied())
}

fn remember_lazy_height(key: BlockKey, height: usize) {
    LAZY_HEIGHTS.with(|c| c.borrow_mut().insert(key, height));
}

/// The identity of the rows cached for entry `idx`, or `None` if it has no slot.
/// A frame that reuses a block returns the same pointer; a frame that re-rendered
/// it returns a different one. Used by
/// `an_unchanged_block_is_reused_not_rerendered` to hold the invariant that keeps
/// a long transcript's frame cost flat.
#[cfg(test)]
pub(crate) fn block_cache_ptr(idx: usize) -> Option<usize> {
    BLOCK_CACHE.with(|c| {
        c.borrow()
            .get(&idx)
            .map(|(_, rows)| Rc::as_ptr(rows) as usize)
    })
}

/// Fingerprint the parts of a block that its body key can't see: the kind (which
/// picks the background and border), the rows lent to it by a following entry (a
/// text-less turn's `#N` label, a stats line), its own timestamp footer, and
/// whether its bottom pad is dropped.
fn chrome_hash(
    kind: BlockKind,
    lent: &[Lent],
    footer: &Option<MetaSpec>,
    drop_bottom: bool,
) -> u64 {
    let mut h = DefaultHasher::new();
    (kind as u8).hash(&mut h);
    drop_bottom.hash(&mut h);
    footer.hash(&mut h);
    lent.len().hash(&mut h);
    for l in lent {
        // Scalars only — never the rendered text. This runs for every entry on
        // every frame, and hashing the rows would put the transcript's *bytes* back
        // in the frame's path, which is the cost this design exists to avoid.
        match l {
            Lent::Meta(meta) => (0u8, meta).hash(&mut h),
            Lent::Stats(key, _) => (1u8, key).hash(&mut h),
        }
    }
    h.finish()
}

/// One painted transcript block, held by the frame that assembles the transcript.
///
/// Its rows come out of [`render_block`], which has already wrapped every body
/// line to the block's inner width and padded it out to the full render width —
/// so **each row is exactly one screen row**, and a chunk's height is just
/// `rows.len()`. That invariant is what lets a frame place the viewport by
/// counting rows instead of re-wrapping the session
/// (`every_block_row_is_exactly_one_screen_row` holds it in place).
struct Chunk<'a> {
    rows: ChunkRows<'a>,
    /// Transcript index, when this chunk is a tool call (for click-to-expand).
    tool_idx: Option<usize>,
}

/// A chunk's rows — laid out already, or laid out only if they are looked at.
enum ChunkRows<'a> {
    /// Shared with the cache: an unchanged block is a refcount bump per frame.
    Ready(Rows),
    /// The session header, whose logo animates: it cannot be cached (the frame it
    /// would serve is the one before), and it paints a span per glyph, which is the
    /// single most expensive block in the transcript. In any session long enough to
    /// scroll it off the top, that is a hundred-odd microseconds a frame spent on
    /// rows nobody is looking at. Its *height* doesn't animate, so it is cached the
    /// first time the block is built and the rows come back only when the viewport
    /// reaches them.
    Lazy {
        height: usize,
        build: Box<dyn Fn() -> Rows + 'a>,
    },
}

impl ChunkRows<'_> {
    /// Screen rows this chunk occupies — never builds anything.
    fn height(&self) -> usize {
        match self {
            ChunkRows::Ready(rows) => rows.len(),
            ChunkRows::Lazy { height, .. } => *height,
        }
    }

    /// The rows themselves, laying them out if that was deferred.
    fn rows(&self) -> Rows {
        match self {
            ChunkRows::Ready(rows) => Rc::clone(rows),
            ChunkRows::Lazy { build, .. } => build(),
        }
    }
}

/// A message's closing `#N you · 2m ago` label — as a *recipe*, not as rows.
///
/// The label is the one part of a block that changes on its own, without the entry
/// changing: a relative timestamp ticks over. Formatting it costs a clock read and
/// an allocation, and a frame has one per message — so the frame keys the block
/// cache on this (all scalars: `bucket` stands in for the rendered time, see
/// [`hrdr_app::relative_time_bucket`]) and only builds the rows when the block
/// itself has to be laid out again.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MetaSpec {
    /// The message's `#N`.
    num: usize,
    role: &'static str,
    style: TimestampStyle,
    /// What the time reads as now, without saying it in words.
    bucket: u64,
    /// When the entry landed — read only when the rows are actually built.
    time: chrono::DateTime<chrono::Local>,
}

impl MetaSpec {
    /// The blank row and the label itself, on the block's own background.
    fn rows(&self, now: chrono::DateTime<chrono::Local>, theme: &Theme) -> Vec<Line<'static>> {
        if self.style == TimestampStyle::None {
            return Vec::new();
        }
        let time = if self.style == TimestampStyle::Relative {
            hrdr_app::relative_time_since(self.time, now)
        } else {
            self.time.format("%H:%M").to_string()
        };
        let (num, role) = (self.num, self.role);
        vec![
            Line::raw(""),
            Line::from(Span::styled(
                format!("#{num} {role} · {time}"),
                Style::default().fg(theme.dim),
            )),
        ]
    }
}

/// Rows a *following* entry lends to the block above it, because it has no block
/// of its own: a text-less turn's `#N assistant` jump label, or the stats line
/// that closes the turn.
#[derive(Clone)]
enum Lent {
    Meta(MetaSpec),
    /// Already rendered and cached under its own entry's body key — which is what
    /// the block hashes, rather than walking the rows.
    Stats(BodyKey, Rows),
}

/// A block whose parts are gathered but which is not yet rendered. Held for one
/// iteration so a text-less assistant turn — which has no block of its own — can
/// lend its `#N assistant` jump label to it, and so the block after it can say
/// whether the two need a separator between them.
struct PendingBlock<'a> {
    /// Transcript index — the cache slot this block owns.
    idx: usize,
    kind: BlockKind,
    /// The entry's own content.
    body: BodySource<'a>,
    /// What a following entry lent this block. Sits below the body, above the
    /// block's own footer.
    lent: Vec<Lent>,
    /// This block's closing `#N you · 2m ago` label.
    footer: Option<MetaSpec>,
    /// The body's cache key, extended with the chrome hash to key the block.
    body_key: BodyKey,
    tool_idx: Option<usize>,
    /// How many numbered messages start at this block. Usually 1 (or 0 for blocks
    /// that aren't messages); a block carrying a lent assistant label counts that
    /// message too.
    msgs: usize,
}

/// Where a block's body comes from.
enum BodySource<'a> {
    /// Rendered and shared with [`BODY_CACHE`] — every entry but the header.
    Cached(Rows),
    /// Rebuilt on demand: the header, whose logo animates. Nothing about it can be
    /// cached, so it is only ever built when it is going to be seen.
    Animated(Box<dyn Fn() -> Vec<Line<'static>> + 'a>),
}

impl PendingBlock<'_> {
    /// Take what a following entry lends.
    fn lend(&mut self, rows: Lent) {
        self.lent.push(rows);
    }
}

/// Paint a held block, recording where its messages start and (for a tool call)
/// the row span a click can hit.
///
/// `next_bg` is the background of the block that follows, if any. Every block
/// carries a blank padded row above and below, so the gap between two blocks is
/// tuned by what sits on either side:
///
/// * **tinted → tinted** — those pads carry their backgrounds, so a prompt and
///   the tool call it triggered (or two tool calls) would merge into one slab.
///   A separator row is added between them.
/// * **untinted → untinted** — both pads are plain blank rows, and two of them
///   is one too many between the model's thought and its output. One is dropped.
/// * **mixed** — the two pads already read as a single gap. Left alone.
/// * **anything → nothing** — the last block gets no separator of its own; the
///   layout keeps a blank row between the transcript and the input pane below.
fn flush<'a>(
    chunks: &mut Vec<Chunk<'a>>,
    msg_at: &mut Vec<usize>,
    pending: Option<PendingBlock<'a>>,
    next_bg: Option<Color>,
    width: usize,
    now: chrono::DateTime<chrono::Local>,
    theme: &Theme,
) {
    let Some(block) = pending else { return };
    let (idx, msgs, tool_idx) = (block.idx, block.msgs, block.tool_idx);
    let animated = matches!(block.body, BodySource::Animated(_));
    let bg = block.kind.bg(theme);
    let border = block.kind.border(theme);
    let untinted = bg == Color::Reset;
    let next_untinted = next_bg.map(|n| n == Color::Reset);
    // Drop this block's bottom pad; the next block's top pad is the gap.
    let drop_bottom = untinted && next_untinted == Some(true);
    // Two tinted blocks would merge into one slab; give them a separator row.
    let separate = !untinted && next_untinted == Some(false);

    let key = (
        block.body_key,
        chrome_hash(block.kind, &block.lent, &block.footer, drop_bottom),
    );
    let theme = theme.clone();
    // Lay the block out: the body, then whatever a following entry lent it, then
    // its own label — all through the one `render_block` call, so no entry paints
    // its own chrome.
    let render = move || -> Rows {
        let mut body: Vec<Line<'static>> = Vec::with_capacity(8);
        match &block.body {
            BodySource::Cached(rows) => body.extend(rows.iter().cloned()),
            BodySource::Animated(build) => body.extend(build()),
        }
        for lent in &block.lent {
            match lent {
                Lent::Meta(meta) => body.extend(meta.rows(now, &theme)),
                // The stats line sits a blank row below the turn it closes.
                Lent::Stats(_, rows) => {
                    body.push(Line::raw(""));
                    body.extend(rows.iter().cloned());
                }
            }
        }
        if let Some(footer) = &block.footer {
            body.extend(footer.rows(now, &theme));
        }
        let mut rows = render_block(body, width, bg, border);
        if drop_bottom {
            rows.pop();
        }
        Rc::new(rows)
    };

    let rows = match animated {
        // Every other block is laid out once and cached.
        false => ChunkRows::Ready(cached_block(idx, key, render)),
        // The header: its rows animate, its height does not. Once we know how tall
        // it is, a frame that doesn't show it doesn't build it.
        true => match lazy_height(key) {
            Some(height) => ChunkRows::Lazy {
                height,
                build: Box::new(render),
            },
            None => {
                let rows = render();
                remember_lazy_height(key, rows.len());
                ChunkRows::Ready(rows)
            }
        },
    };

    for _ in 0..msgs {
        msg_at.push(chunks.len());
    }
    chunks.push(Chunk { rows, tool_idx });
    if separate {
        chunks.push(separator());
    }
}

/// A one-row gap between two blocks. Its `Rc` is cloned, not rebuilt: the row is
/// the same blank line every time.
fn separator<'a>() -> Chunk<'a> {
    thread_local! {
        static SEP: Rc<Vec<Line<'static>>> = Rc::new(vec![Line::raw("")]);
    }
    Chunk {
        rows: ChunkRows::Ready(SEP.with(Rc::clone)),
        tool_idx: None,
    }
}

/// Returns the transcript as a list of rendered blocks, plus the chunk each
/// 1-based user/assistant message starts at (for `/goto`).
///
/// Nothing here is laid out against the viewport: a chunk's rows are already
/// wrapped and padded, so the caller places the viewport by counting them. Blocks
/// whose entry has not changed come straight out of [`BLOCK_CACHE`] as an `Rc`,
/// which is what keeps a frame's cost proportional to what *changed* rather than
/// to the length of the session.
fn transcript_chunks<'a>(app: &'a App, width: u16) -> (Vec<Chunk<'a>>, Vec<usize>) {
    let theme = &app.theme;
    let md_theme = theme.md_theme();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut msg_at: Vec<usize> = Vec::new();
    // Number user/assistant messages so `/copy msg N` lines up with the display.
    let mut msg_num = 0usize;
    // One clock read for the whole frame. Reading it per message — which is what
    // formatting a relative time does — is a timezone lookup and an allocation per
    // entry, every frame, for a label that changes at most once a minute.
    let now = chrono::Local::now();
    // The `#N you · 2m ago` row that closes a message block, as a recipe: the rows
    // are built only if the block has to be laid out again ([`MetaSpec`]).
    let meta = |e: &Entry, num: usize, role: &'static str| -> MetaSpec {
        MetaSpec {
            num,
            role,
            style: app.timestamp_style,
            bucket: hrdr_app::relative_time_bucket(e.time, now),
            time: e.time,
        }
    };
    // Block width and the width its content is laid out at (minus padding).
    let w = width as usize;
    let inner = inner_width(w);
    let mut pending: Option<PendingBlock> = None;
    for (i, entry) in app.panes.active_transcript().iter().enumerate() {
        // Body cache key shared by all arms (Reasoning skip happens before this).
        // The header has no content of its own to hash — it reads live session
        // state — so it hashes what it *displays*. Its rows are never cached, but
        // its height is, and a header that grew a row (an effort field appearing, a
        // longer model name wrapping) has to be measured again.
        // For unfinished tool entries the current spinner frame is mixed into the
        // hash so the cached block invalidates on each tick, animating the marker.
        let frame_idx = (app.header_anchor.elapsed().as_millis() / 120) as u64;
        let frame = SPINNER[frame_idx as usize % SPINNER.len()];
        let base_hash = match entry.kind {
            EntryKind::Header => header_hash(app),
            _ => entry_content_hash(entry, app.expand_tools),
        };
        let base_hash = match &entry.kind {
            EntryKind::Tool { done: false, .. } => base_hash ^ frame_idx,
            _ => base_hash,
        };
        let ck: BodyKey = (base_hash, width, app.expand_tools, app.show_reasoning);
        // Every arm produces (kind, header rows, cached body rows, footer rows)
        // and is then funneled through the one `render_block` call below — no
        // entry paints its own chrome.
        let mut footer: Option<MetaSpec> = None;
        // Numbered messages starting at the block this entry produces.
        let mut msg_here = 0usize;
        let (kind, body) = match &entry.kind {
            // Rebuilt every frame it is *seen*: the logo animation advances with
            // the wall clock, and the details mirror the live model/provider. A
            // frame that has scrolled past it doesn't build it at all.
            EntryKind::Header => (
                BlockKind::Header,
                BodySource::Animated(Box::new(move || {
                    header_lines(app, app.header_anchor, width)
                })),
            ),
            // An assistant turn that only called tools has no text, so it gets no
            // block of its own — but its `#N assistant` label is a `/goto` jump
            // point, so it rides along on the previous block instead.
            EntryKind::Assistant(text) if text.trim().is_empty() => {
                msg_num += 1;
                if let Some(block) = pending.as_mut() {
                    block.lend(Lent::Meta(meta(entry, msg_num, "assistant")));
                    block.msgs += 1;
                } else {
                    // Nothing to append to (it opens the transcript): the label
                    // has nowhere to live, so the message keeps no jump point.
                    msg_at.push(chunks.len());
                }
                continue;
            }
            // A thinking block with no thought in it renders nothing at all.
            EntryKind::Reasoning { text, .. } if text.trim().is_empty() => continue,
            // A user prompt renders exactly like the model's output — same
            // markdown, same colors. Only the block's background differs.
            EntryKind::User(text) => {
                msg_num += 1;
                msg_here += 1;
                footer = Some(meta(entry, msg_num, "you"));
                let body = cached_body(i, ck, || {
                    markdown_lines(text, &md_theme, BlockKind::User.bg(theme), inner)
                });
                (BlockKind::User, BodySource::Cached(body))
            }
            // Assistant text is rendered as markdown (headings, lists, emphasis,
            // inline/code spans) via hjkl-markdown; fenced code blocks are pulled
            // out and syntax-highlighted with syntect.
            EntryKind::Assistant(text) => {
                msg_num += 1;
                msg_here += 1;
                footer = Some(meta(entry, msg_num, "assistant"));
                let body = cached_body(i, ck, || {
                    markdown_lines(text, &md_theme, BlockKind::Assistant.bg(theme), inner)
                });
                (BlockKind::Assistant, BodySource::Cached(body))
            }
            EntryKind::Reasoning { .. } if !app.show_reasoning => continue, // hidden via /reasoning
            // No `⠋ Thinking` / `Thought: 1.2s` label: the dimmer text already
            // says it's the model thinking, and the loader above the input shows
            // that a turn is running. (`took_ms` is still recorded — it's the
            // only trace of how long the model thought.)
            EntryKind::Reasoning { text, .. } => {
                // Same markdown pipeline as assistant, in the same colors —
                // only dimmer, so thoughts read as a quieter version of output.
                let body = cached_body(i, ck, || {
                    markdown_lines(
                        text,
                        &theme.md_theme_dim(),
                        BlockKind::Reasoning.bg(theme),
                        inner,
                    )
                });
                (BlockKind::Reasoning, BodySource::Cached(body))
            }
            EntryKind::Tool {
                name,
                args,
                result,
                ok,
                done,
                expanded,
                ..
            } => {
                let body = cached_body(i, ck, || {
                    tool_lines(
                        theme,
                        name,
                        args,
                        result,
                        *ok,
                        *done,
                        *expanded || app.expand_tools,
                        frame,
                    )
                });
                (BlockKind::Tool, BodySource::Cached(body))
            }
            // Slash-command output and status notices read like assistant output
            // — same markdown, same colors, no dimming — on their own background.
            EntryKind::System(text) | EntryKind::Notice(text) => {
                let body = cached_body(i, ck, || {
                    markdown_lines(text, &md_theme, BlockKind::Command.bg(theme), inner)
                });
                (BlockKind::Command, BodySource::Cached(body))
            }
            // The per-turn stats line belongs to the turn that just ended, so it
            // closes that turn's block rather than opening one of its own.
            EntryKind::Stats(text) => {
                let body = cached_body(i, ck, || text_lines(text, Style::default().fg(theme.dim)));
                match pending.as_mut() {
                    Some(block) => {
                        block.lend(Lent::Stats(ck, body));
                        continue;
                    }
                    // Nothing to attach to (it opens the transcript): fall back
                    // to a block of its own.
                    None => (BlockKind::Stats, BodySource::Cached(body)),
                }
            }
            // `/diff` is slash-command output too, but with diff coloring
            // instead of markdown.
            EntryKind::Diff(text) => {
                let body = cached_body(i, ck, || {
                    text.lines()
                        .map(|line| {
                            Line::from(Span::styled(
                                line.to_string(),
                                Style::default().fg(diff_line_color(line, theme)),
                            ))
                        })
                        .collect()
                });
                (BlockKind::Command, BodySource::Cached(body))
            }
        };
        // Flush the previous block, then hold this one: a text-less assistant
        // turn that follows appends its label to whatever is pending.
        flush(
            &mut chunks,
            &mut msg_at,
            pending.take(),
            Some(kind.bg(theme)),
            w,
            now,
            theme,
        );
        pending = Some(PendingBlock {
            idx: i,
            kind,
            body,
            lent: Vec::new(),
            footer,
            body_key: ck,
            tool_idx: matches!(entry.kind, EntryKind::Tool { .. }).then_some(i),
            msgs: msg_here,
        });
    }
    // Queued prompts follow the transcript, so the last block is separated from
    // them exactly as it would be from any other tinted block.
    // Each agent's own queue: what is waiting to reach *this* agent.
    let queued = app.panes.active_pane().pending.clone();
    let queued_bg = (!queued.is_empty()).then(|| BlockKind::Queued.bg(theme));
    flush(
        &mut chunks,
        &mut msg_at,
        pending.take(),
        queued_bg,
        w,
        now,
        theme,
    );

    // Pending queued messages render like user prompts, with a "Queued" badge
    // as the block's last row — through the same block path as everything else,
    // so they pick up the same padding and background. They are not transcript
    // entries and have no cache slot: there are never more than a few, and each
    // one is consumed the moment the agent is free.
    if !queued.is_empty() {
        let bg = BlockKind::Queued.bg(theme);
        let badge = Style::default().fg(Color::Black).bg(theme.warn).bold();
        for msg in &queued {
            let mut body = markdown_lines(msg, &md_theme, bg, inner);
            // A blank row inside the block, so the badge doesn't sit flush
            // against the message text above it.
            body.push(Line::raw(""));
            body.push(Line::from(Span::styled(" Queued ", badge)));
            chunks.push(Chunk {
                rows: ChunkRows::Ready(Rc::new(render_block(
                    body,
                    w,
                    bg,
                    BlockKind::Queued.border(theme),
                ))),
                tool_idx: None,
            });
            // Queued blocks are tinted: a blank row separates them from each
            // other, and the last one from the input pane below.
            chunks.push(separator());
        }
    }

    (chunks, msg_at)
}

/// Split plain text into styled block-body lines (no padding — [`render_block`]
/// adds that).
fn text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|raw| Line::from(Span::styled(raw.to_string(), style)))
        .collect()
}

/// Render markdown into block-body lines: prose through hjkl-markdown, fenced
/// code blocks pulled out and syntax-highlighted. `bg` is the enclosing block's
/// background — code sits on it rather than a surface of its own, at the block's
/// own indentation and with no language tag.
fn markdown_lines(
    text: &str,
    md: &hjkl_markdown_tui::MdTheme,
    bg: Color,
    width: u16,
) -> Vec<Line<'static>> {
    let mut buf = Vec::new();
    let mut ev_buf: Vec<hjkl_markdown::Event> = Vec::new();
    for ev in hjkl_markdown::parse(text) {
        if let hjkl_markdown::Event::CodeBlock { lang, content } = ev {
            if !ev_buf.is_empty() {
                buf.extend(hjkl_markdown_tui::to_lines(&ev_buf, md, width.max(1)));
                ev_buf.clear();
            }
            buf.extend(highlight_lines(&lang, &content, bg));
        } else {
            ev_buf.push(ev);
        }
    }
    if !ev_buf.is_empty() {
        buf.extend(hjkl_markdown_tui::to_lines(&ev_buf, md, width.max(1)));
    }
    buf
}

/// Block body for one tool call: a status header (SPINNER / ✓ / ✗ + tool name +
/// headline) followed by tool-specific detail — the command and its output for
/// shell calls, the file contents for `write`, the patch for `edit`/`patch`,
/// the tail of the file for `read`, plain output otherwise.
#[allow(clippy::too_many_arguments)]
fn tool_lines(
    theme: &Theme,
    name: &str,
    args: &str,
    result: &str,
    ok: bool,
    done: bool,
    expanded: bool,
    frame: &'static str,
) -> Vec<Line<'static>> {
    let bg = BlockKind::Tool.bg(theme);
    let dim_bg = Style::default().fg(theme.dim).bg(bg);
    let mark = if !done {
        (frame, theme.warn)
    } else if ok {
        ("✓", theme.success)
    } else {
        ("✗", theme.error)
    };
    let disp = hrdr_app::tool_display(name, args);
    let mut header = vec![
        Span::styled(format!("{} ", mark.0), Style::default().fg(mark.1).bg(bg)),
        Span::styled(
            name.to_string(),
            Style::default().fg(theme.warn).bg(bg).bold(),
        ),
    ];
    if !disp.headline.is_empty() {
        header.push(Span::styled(format!(" {}", disp.headline), dim_bg));
    }
    let mut out: Vec<Line<'static>> = vec![Line::from(header)];

    // The `write` body is the file contents from the args, not the result diff.
    // Rendered raw — no gutter, no width fill, no language bar: the block's own
    // padding is the only indent, so the contents read as the file's own text.
    if let hrdr_app::ToolBody::Code { lang, content } = &disp.body {
        let mut code = highlight_lines(lang, content, bg);
        if !expanded && code.len() > DIFF_PREVIEW_LINES {
            let extra = code.len() - DIFF_PREVIEW_LINES;
            code.truncate(DIFF_PREVIEW_LINES);
            code.push(Line::from(Span::styled(more_hint(extra), dim_bg)));
        }
        out.extend(code);
        // Only the failure is worth showing; the success diff duplicates the
        // contents we just rendered.
        if done && !ok {
            out.extend(text_lines(result, Style::default().fg(theme.error).bg(bg)));
        }
        return out;
    }

    // Shell calls: the command on its own lines, verbatim. The block's `bash`
    // header already says what it is, so no `$ ` prompt is drawn.
    if let hrdr_app::ToolBody::Shell { command } = &disp.body {
        for line in command.lines() {
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme.assistant).bg(bg),
            )));
        }
    }

    // Everything else with structured args (`task`, `todo`, MCP tools): one
    // `key  value` row per argument, below the name — never raw JSON beside it.
    // Keys are padded to a common width so the values line up.
    if let hrdr_app::ToolBody::Details(rows) = &disp.body {
        let key_w = rows.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
        for (key, value) in rows {
            let mut spans = Vec::new();
            if !key.is_empty() {
                spans.push(Span::styled(
                    format!("{key}{}  ", " ".repeat(key_w - key.width())),
                    dim_bg,
                ));
            }
            // A long value (a `task` prompt) is clipped until the block is
            // expanded, when it wraps in full like any other block content.
            let text = if expanded {
                value.clone()
            } else {
                hrdr_tools::truncate_inline(value, DETAIL_VALUE_W)
            };
            spans.push(Span::styled(
                text,
                Style::default().fg(theme.assistant).bg(bg),
            ));
            out.push(Line::from(spans));
        }
    }

    if result.is_empty() {
        return out;
    }
    let is_diff = disp.body == hrdr_app::ToolBody::Diff;
    let is_read = disp.body == hrdr_app::ToolBody::Read;
    // A diff is the point of an edit/patch block, so it gets a taller preview.
    let preview = if is_diff {
        DIFF_PREVIEW_LINES
    } else {
        TOOL_RESULT_PREVIEW_LINES
    };
    let lines: Vec<&str> = result.lines().collect();
    if done {
        // Finished: show the head of the result (or all of it when expanded).
        // For read tools, show the tail (the actual content read, not preamble).
        let shown = if expanded { lines.len() } else { preview };
        let extra = lines.len().saturating_sub(shown);
        // read shows the tail, so its hidden lines are *above* the preview —
        // the hint goes there too. Everything else hides its tail.
        let (start, end) = if is_read {
            (extra, lines.len())
        } else {
            (0, shown.min(lines.len()))
        };
        if extra > 0 && is_read {
            out.push(Line::from(Span::styled(more_hint(extra), dim_bg)));
        }
        for line in &lines[start..end] {
            let color = if is_diff {
                diff_line_color(line, theme)
            } else {
                theme.dim
            };
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(color).bg(bg),
            )));
        }
        if extra > 0 && !is_read {
            out.push(Line::from(Span::styled(more_hint(extra), dim_bg)));
        }
    } else {
        // Still running: show the live tail so the newest output is visible.
        let start = lines.len().saturating_sub(preview);
        if start > 0 {
            out.push(Line::from(Span::styled(
                format!("⋮ (live · {start} earlier line(s))"),
                dim_bg,
            )));
        }
        for line in &lines[start..] {
            out.push(Line::from(Span::styled(line.to_string(), dim_bg)));
        }
    }
    out
}

/// The collapsed-block footer hint.
fn more_hint(extra: usize) -> String {
    format!("… (+{extra} more lines · click or /expand)")
}

/// Color for one unified-diff line: additions green, deletions red, hunk
/// headers in the accent color, file headers and context dim.
fn diff_line_color(line: &str, theme: &Theme) -> Color {
    // Shared classification + color semantics.
    slot_color(
        hrdr_app::diff_kind_slot(hrdr_app::classify_diff_line(line)),
        theme,
    )
}

#[cfg(test)]
mod clamp_tests {
    use super::clamp_u16;

    /// The transcript's cumulative wrapped-row count is kept in `usize` to
    /// avoid overflow, but the handful of places it crosses into ratatui's
    /// `u16`-only scroll APIs must saturate at `u16::MAX`, not truncate — a
    /// raw `as u16` on a session taller than 65535 rows wraps back down
    /// (mod 65536) to a small, unrelated number, which would snap the
    /// scrollbar/offset math to somewhere near the top instead of the bottom.
    #[test]
    fn clamp_u16_saturates_instead_of_wrapping() {
        assert_eq!(clamp_u16(0), 0);
        assert_eq!(clamp_u16(65_535), u16::MAX);
        assert_eq!(clamp_u16(65_536), u16::MAX, "one past the max still clamps");
        assert_eq!(clamp_u16(100_000), u16::MAX);
        // What the bug actually did: `100_000 as u16` truncates to
        // `100_000 % 65_536 = 34_464` — nowhere near u16::MAX.
        assert_ne!(clamp_u16(100_000), (100_000usize % 65_536) as u16);
    }
}

#[cfg(test)]
mod subagent_tests {
    use super::{
        SUBAGENT_PANEL_MAX_ROWS, agent_sort_key, subagent_panel_height, subagent_scroll,
        todo_sort_key,
    };
    use hrdr_app::{PaneId, PaneRow, PaneStatus};

    /// The TODO panel groups by status: in-progress on top, pending in the
    /// middle, completed/cancelled at the bottom — and stable within each group.
    #[test]
    fn todos_sort_in_progress_then_pending_then_done() {
        let mut rows = [
            ("done a", "completed"),
            ("pending a", "pending"),
            ("active", "in_progress"),
            ("cancelled c", "cancelled"),
            ("done b", "completed"),
            ("pending b", "pending"),
        ];
        rows.sort_by_key(|(_, status)| todo_sort_key(status));
        let order: Vec<&str> = rows.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            order,
            vec![
                "active",
                "pending a",
                "pending b",
                "done a",
                "cancelled c",
                "done b"
            ]
        );
    }

    /// The agent panel groups: main on top, running/idle in the middle, finished
    /// (Done) at the bottom — stable within each group.
    #[test]
    fn agents_sort_main_then_running_then_done() {
        let row = |id: PaneId, status: PaneStatus| PaneRow {
            id,
            title: String::new(),
            status,
            active: false,
        };
        let mut rows = [
            row(PaneId::Sub(1), PaneStatus::Done),
            row(PaneId::Sub(2), PaneStatus::Running),
            row(PaneId::Main, PaneStatus::Idle),
            row(PaneId::Sub(3), PaneStatus::Idle),
            row(PaneId::Sub(4), PaneStatus::Done),
        ];
        rows.sort_by_key(agent_sort_key);
        let order: Vec<PaneId> = rows.iter().map(|r| r.id).collect();
        assert_eq!(
            order,
            vec![
                PaneId::Main,
                PaneId::Sub(2), // running
                PaneId::Sub(3), // idle (not finished) stays with the middle group
                PaneId::Sub(1), // done
                PaneId::Sub(4),
            ]
        );
    }

    fn items(n: usize) -> Vec<PaneRow> {
        (0..n)
            .map(|i| PaneRow {
                id: if i == 0 {
                    PaneId::Main
                } else {
                    PaneId::Sub(i as u64)
                },
                title: format!("agent {i}"),
                status: PaneStatus::Idle,
                active: i == 0,
            })
            .collect()
    }

    /// One row per agent plus the pane's two padding rows, capped — the panel is
    /// a list now, so an agent's log length can't grow it.
    #[test]
    fn the_panel_is_one_row_per_agent_plus_padding() {
        assert_eq!(subagent_panel_height(&items(0)), 0, "hidden when empty");
        assert_eq!(subagent_panel_height(&items(1)), 3);
        assert_eq!(subagent_panel_height(&items(4)), 6);
        assert_eq!(
            subagent_panel_height(&items(SUBAGENT_PANEL_MAX_ROWS as usize + 5)),
            SUBAGENT_PANEL_MAX_ROWS + 2,
            "capped"
        );
    }

    /// Rows past the panel's height scroll off the top, newest at the bottom.
    #[test]
    fn scroll_pins_the_newest_agent_to_the_bottom() {
        assert_eq!(subagent_scroll(2, 4), 0, "content fits");
        assert_eq!(subagent_scroll(0, 4), 0, "empty");
        assert_eq!(subagent_scroll(6, 4), 2, "two oldest scrolled off");
    }
}

/// Test 5 — render-cache equivalence and invalidation.
///
/// `transcript_chunks()` cannot be called in unit tests (requires a live `App`
/// and a ratatui `Frame`), so these tests exercise the pure building blocks it
/// is made of: `entry_content_hash`, `cached_body`, and `cached_block`.  Each
/// test targets a distinct failure mode:
///
/// * **wrong-content-served**: a warm cache hit returns different bytes than
///   the closure that originally populated it (caught by `cached_body_warm_hit_equals_cold_render`).
/// * **cross-entry contamination**: two entries share a cache slot and entry B's
///   render is served for entry A (caught by
///   `cached_body_different_entries_do_not_collide`).
/// * **stale expand state**: the user runs `/expand` but the collapsed render
///   is returned because `expand_all` is not part of the hash (caught by
///   `entry_content_hash_tool_expand_flag_changes_hash`).
/// * **stale rows after an edit**: an entry's content changes but its slot still
///   holds the old render (caught by `a_changed_key_replaces_the_slot`).
/// * **stale chrome**: the *body* is unchanged but the rows around it are not —
///   a timestamp ticked over, a following turn lent the block its `#N` label — and
///   the block is served with the old chrome (caught by
///   `cached_block_keys_on_the_chrome_around_the_body`).
#[cfg(test)]
mod cache_tests {
    use super::{
        BLOCK_CACHE, BODY_CACHE, BlockKind, ChunkRows, Lent, MetaSpec, Rc, TimestampStyle,
        cached_block, cached_body, chrome_hash, entry_content_hash,
    };
    use crate::app::{Entry, EntryKind};
    use ratatui::text::{Line, Span};

    // ── entry_content_hash ─────────────────────────────────────────────────────

    /// Entries differing only in their text content must produce different hashes.
    ///
    /// Regression: if `entry_content_hash` were stubbed to a constant (or the
    /// text field were accidentally excluded from the `Hash` call), two messages
    /// with distinct text would share a cache key.  After message A warms the
    /// cache, message B would be rendered with A's lines — wrong text displayed.
    #[test]
    fn entry_content_hash_differs_by_text() {
        let a = Entry::user("hello");
        let b = Entry::user("world");
        assert_ne!(
            entry_content_hash(&a, false),
            entry_content_hash(&b, false),
            "User entries with different text must produce different hashes"
        );

        let c = Entry::assistant("response one");
        let d = Entry::assistant("response two");
        assert_ne!(
            entry_content_hash(&c, false),
            entry_content_hash(&d, false),
            "Assistant entries with different text must produce different hashes"
        );
    }

    /// For `Tool` entries the effective expand state `(*expanded || expand_all)`
    /// is folded into the hash.  Changing `expand_all` must therefore invalidate
    /// the cache key, otherwise the collapsed render (showing only a preview)
    /// would be served to the user after they run `/expand all`.
    #[test]
    fn entry_content_hash_tool_expand_flag_changes_hash() {
        let tool = Entry::now(EntryKind::Tool {
            id: "t1".to_string(),
            name: "bash".to_string(),
            args: "{}".to_string(),
            result: "long output".to_string(),
            ok: true,
            done: true,
            expanded: false, // not locally expanded
        });

        let h_collapsed = entry_content_hash(&tool, false);
        let h_global_expand = entry_content_hash(&tool, true);
        assert_ne!(
            h_collapsed, h_global_expand,
            "expand_all=true vs false must produce different hashes when \
             the Tool entry itself is not locally expanded"
        );

        // If the Tool is already locally expanded, the effective state is
        // `true` regardless of expand_all → both should hash identically.
        let tool_local = Entry::now(EntryKind::Tool {
            id: "t2".to_string(),
            name: "bash".to_string(),
            args: "{}".to_string(),
            result: "long output".to_string(),
            ok: true,
            done: true,
            expanded: true, // locally expanded → effective = true in both cases
        });
        assert_eq!(
            entry_content_hash(&tool_local, false),
            entry_content_hash(&tool_local, true),
            "a Tool with expanded=true must hash identically for any expand_all \
             value (effective state is always true)"
        );
    }

    // ── cached_body ────────────────────────────────────────────────────────────

    /// A warm cache hit must return exactly the same rows as the initial cold
    /// render, and the render closure must not be called a second time.
    ///
    /// Regression: if `cached_body` re-rendered on every call (ignoring the
    /// stored value), the output would only be right as long as the render
    /// function is deterministic — and it isn't: the incremental syntax
    /// highlighter resumes state across calls. The panic inside the warm closure
    /// proves the cache is actually consulted.
    #[test]
    fn cached_body_warm_hit_equals_cold_render() {
        BODY_CACHE.with(|c| c.borrow_mut().clear());

        let key = (0xdead_beef_cafe_0001, 80, false, false);
        let expected = vec![Line::from(Span::raw("cold render content"))];

        // Cold miss — closure must be invoked and its result stored.
        let cold = cached_body(0, key, || expected.clone());
        assert_eq!(
            *cold, expected,
            "cold render must return what the closure produced"
        );

        // Warm hit — the cached value must be returned; closure panics if called.
        let warm = cached_body(0, key, || {
            panic!("closure must not be invoked on a warm cache hit")
        });
        assert_eq!(
            *warm, expected,
            "warm cache hit must return the same rows as the cold render"
        );
    }

    /// Two entries must never cross-serve: looking up entry B after entry A is
    /// warm must still produce B's own content.
    #[test]
    fn cached_body_different_entries_do_not_collide() {
        BODY_CACHE.with(|c| c.borrow_mut().clear());

        let key_a = (0xaaaa, 80, false, false);
        let key_b = (0xbbbb, 80, false, false);
        let lines_a = vec![Line::from(Span::raw("entry A — unique content"))];
        let lines_b = vec![Line::from(Span::raw("entry B — unique content"))];

        let r_a = cached_body(10, key_a, || lines_a.clone());
        let r_b = cached_body(11, key_b, || lines_b.clone());

        assert_eq!(*r_a, lines_a, "entry 10 must return its own rows");
        assert_eq!(*r_b, lines_b, "entry 11 must return its own rows");
    }

    /// A slot holds *one* render per entry, so a changed key must replace it —
    /// not sit behind it.
    ///
    /// This is what makes streaming work: the entry the model is writing into
    /// changes its content hash on every token, and each frame must show the text
    /// as it now stands. It is also the eviction policy: one slot per entry means
    /// the cache is bounded by the transcript and can never thrash (the old cache
    /// was capped and dropped *wholesale*, so past the cap every frame threw away
    /// what the next frame needed — a 2000-entry session re-rendered itself from
    /// scratch several times a second).
    #[test]
    fn a_changed_key_replaces_the_slot() {
        BODY_CACHE.with(|c| c.borrow_mut().clear());

        let before = vec![Line::from(Span::raw("hello"))];
        let after = vec![Line::from(Span::raw("hello world"))];

        let old = cached_body(3, (0x1111, 80, false, false), || before.clone());
        assert_eq!(*old, before);

        // Same entry, new content → new key → the slot is re-rendered.
        let new = cached_body(3, (0x2222, 80, false, false), || after.clone());
        assert_eq!(*new, after, "a changed key must not serve the stale render");

        BODY_CACHE.with(|c| {
            assert_eq!(
                c.borrow().len(),
                1,
                "an entry keeps one slot, however many times its content changes"
            );
        });
    }

    // ── cached_block ───────────────────────────────────────────────────────────

    /// A block is its body *plus the chrome around it*, and the chrome can change
    /// while the body does not: a relative timestamp ticks from `1m ago` to `2m
    /// ago`, a text-less turn lends the block its `#N assistant` label, the block
    /// after it turns out to be untinted so the bottom pad is dropped. All of that
    /// is invisible to the body key, so `chrome_hash` carries it — and a block
    /// whose chrome changed must be re-rendered.
    #[test]
    fn cached_block_keys_on_the_chrome_around_the_body() {
        BLOCK_CACHE.with(|c| c.borrow_mut().clear());

        let body_key = (0x3333, 80, false, false);
        let now = chrono::Local::now();
        let at = |mins: i64| MetaSpec {
            num: 1,
            role: "you",
            style: TimestampStyle::Relative,
            bucket: hrdr_app::relative_time_bucket(now - chrono::Duration::minutes(mins), now),
            time: now - chrono::Duration::minutes(mins),
        };
        let none: &[Lent] = &[];
        let (footer_1m, footer_2m) = (Some(at(1)), Some(at(2)));

        let h_1m = chrome_hash(BlockKind::User, none, &footer_1m, false);
        let h_2m = chrome_hash(BlockKind::User, none, &footer_2m, false);
        assert_ne!(h_1m, h_2m, "a ticked-over timestamp must change the hash");
        assert_ne!(
            h_1m,
            chrome_hash(BlockKind::User, none, &footer_1m, true),
            "dropping the bottom pad must change the hash"
        );
        assert_ne!(
            h_1m,
            chrome_hash(BlockKind::Assistant, none, &footer_1m, false),
            "the block kind picks the background — it must change the hash"
        );
        assert_ne!(
            h_1m,
            chrome_hash(
                BlockKind::User,
                &[Lent::Meta(at(1))], // a text-less turn lent it its `#N` label
                &footer_1m,
                false
            ),
            "rows lent by a following entry must change the hash"
        );

        let stale = vec![Line::from(Span::raw("with 1m ago"))];
        let fresh = vec![Line::from(Span::raw("with 2m ago"))];
        let first = cached_block(5, (body_key, h_1m), || Rc::new(stale.clone()));
        assert_eq!(*first, stale);
        let second = cached_block(5, (body_key, h_2m), || Rc::new(fresh.clone()));
        assert_eq!(
            *second, fresh,
            "same body, new chrome → the block must be re-rendered"
        );
    }

    /// A timestamp that has not visibly changed must not re-lay-out its block.
    ///
    /// The block key carries a *bucket* rather than the rendered time, so two
    /// frames a second apart — where the label still reads `5m ago` — hash the
    /// same. If the key carried the raw instant instead, every message in the
    /// transcript would be laid out again on every frame and the cache would buy
    /// nothing.
    #[test]
    fn a_timestamp_that_reads_the_same_does_not_rebuild_the_block() {
        let now = chrono::Local::now();
        let then = now - chrono::Duration::minutes(5);
        let spec = |now: chrono::DateTime<chrono::Local>| MetaSpec {
            num: 1,
            role: "you",
            style: TimestampStyle::Relative,
            bucket: hrdr_app::relative_time_bucket(then, now),
            time: then,
        };

        let a = Some(spec(now));
        let b = Some(spec(now + chrono::Duration::seconds(1))); // still "5m ago"
        let c = Some(spec(now + chrono::Duration::minutes(1))); // now "6m ago"
        let none: &[Lent] = &[];

        assert_eq!(
            chrome_hash(BlockKind::User, none, &a, false),
            chrome_hash(BlockKind::User, none, &b, false),
            "a frame a second later reads the same — reuse the block"
        );
        assert_ne!(
            chrome_hash(BlockKind::User, none, &a, false),
            chrome_hash(BlockKind::User, none, &c, false),
            "the label ticked over — lay the block out again"
        );
    }

    /// The header's logo animates every frame, so its block is never cached — the
    /// frame it would serve is the one before. Instead its *height* is remembered,
    /// and the rows are built only when the viewport actually reaches them.
    ///
    /// It is the most expensive block in the transcript (a span per glyph of the
    /// logo), and in any session long enough to scroll it off the top, it is rows
    /// nobody is looking at. Measuring must not build it; painting must.
    #[test]
    fn a_lazy_block_is_measured_without_being_built() {
        use std::cell::Cell;

        let builds = Rc::new(Cell::new(0));
        let counter = Rc::clone(&builds);
        let chunk = ChunkRows::Lazy {
            height: 7,
            build: Box::new(move || {
                counter.set(counter.get() + 1);
                Rc::new(vec![Line::from(Span::raw("logo"))])
            }),
        };

        // Placing the viewport asks every block how tall it is. That must be free.
        assert_eq!(chunk.height(), 7);
        assert_eq!(chunk.height(), 7);
        assert_eq!(builds.get(), 0, "measuring must not lay the block out");

        // Painting it does build it — and rebuilds it each frame, which is what
        // keeps the animation moving.
        let rows = chunk.rows();
        assert_eq!(rows.len(), 1);
        chunk.rows();
        assert_eq!(
            builds.get(),
            2,
            "a painted block is laid out afresh each frame"
        );
    }
}

/// Block chrome + tool detail routing — the parts of the transcript renderer
/// that don't need a live `App`.
#[cfg(test)]
mod block_tests {
    use super::*;

    /// Total display width of a rendered line.
    fn w(line: &Line<'_>) -> usize {
        line.spans.iter().map(Span::width).sum()
    }

    /// The rendered text of a line, padding included.
    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Every block is padded on all four sides: a blank row above and below, one
    /// column of background either side of the content, and every row filled to
    /// the full block width so the background paints a solid rectangle.
    ///
    /// Regression: dropping the top/bottom rows (or the right-hand fill) makes
    /// user prompts and tool blocks bleed into the terminal background — the
    /// visual separation between transcript entries disappears.
    #[test]
    fn render_block_pads_all_four_sides() {
        let body = vec![Line::from(Span::raw("hi"))];
        let lines = render_block(body, 10, Color::Blue, None);

        assert_eq!(lines.len(), 3, "1 body row + a blank row above and below");
        assert_eq!(text(&lines[0]).trim(), "", "top pad row is blank");
        assert_eq!(text(&lines[2]).trim(), "", "bottom pad row is blank");
        let body_row = text(&lines[1]);
        assert!(
            body_row.starts_with(&" ".repeat(BLOCK_PAD_X)),
            "BLOCK_PAD_X columns of left padding: {body_row:?}"
        );
        assert_eq!(body_row.trim(), "hi");
        assert!(
            body_row.ends_with(&" ".repeat(BLOCK_PAD_X)),
            "at least BLOCK_PAD_X columns on the right: {body_row:?}"
        );
        for line in &lines {
            assert_eq!(w(line), 10, "every row fills the block width");
            for span in &line.spans {
                assert_eq!(
                    span.style.bg,
                    Some(Color::Blue),
                    "every span carries the bg"
                );
            }
        }
    }

    /// A body line longer than the content width wraps *inside* the block: every
    /// continuation row keeps the left padding and the background.
    ///
    /// Regression: letting ratatui re-wrap the padded line put continuation rows
    /// at column 0 with no background — the stats line visibly broke out of its
    /// block.
    #[test]
    fn render_block_wraps_long_lines_inside_the_block() {
        let body = vec![Line::from(Span::raw("aaa bbb ccc ddd"))];
        let lines = render_block(body, 12, Color::Blue, None); // content width 8
        assert!(lines.len() > 3, "the long line wrapped: {lines:?}");
        let pad = " ".repeat(BLOCK_PAD_X);
        for line in &lines {
            assert_eq!(w(line), 12, "every row still fills the block width");
            assert!(text(line).starts_with(&pad), "left padding on every row");
            for span in &line.spans {
                assert_eq!(span.style.bg, Some(Color::Blue));
            }
        }
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(
            rows[1].trim_end(),
            format!("{pad}aaa bbb"),
            "breaks at whitespace"
        );
    }

    /// Wrapping prefers a whitespace break, hard-breaks a word too long to fit,
    /// and measures in display columns so wide glyphs don't overshoot the row.
    #[test]
    fn wrap_spans_breaks_on_words_then_falls_back_to_a_hard_break() {
        let row_text =
            |r: &Vec<Span<'static>>| -> String { r.iter().map(|s| s.content.as_ref()).collect() };

        // Short enough: one row, untouched.
        assert_eq!(wrap_spans(vec![Span::raw("abc")], 8).len(), 1);

        // Word break.
        let rows = wrap_spans(vec![Span::raw("aaa bbb ccc")], 8);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["aaa bbb ", "ccc"]);

        // No whitespace to break on → hard break at the row width.
        let rows = wrap_spans(vec![Span::raw("aaaaaaaaaaaa")], 4);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["aaaa", "aaaa", "aaaa"]);

        // CJK glyphs are two columns wide: 2 per 4-column row, not 4.
        let rows = wrap_spans(vec![Span::raw("日本語だ")], 4);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["日本", "語だ"]);
        for r in &rows {
            assert!(r.iter().map(Span::width).sum::<usize>() <= 4);
        }
    }

    /// Wrapping preserves each span's style across the break, and coalesces
    /// same-style runs rather than emitting one span per character.
    #[test]
    fn wrap_spans_preserves_styles_across_the_break() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let rows = wrap_spans(
            vec![Span::styled("aaaa", red), Span::styled("bbbb", blue)],
            4,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 1, "one coalesced span, not four");
        assert_eq!(rows[0][0].style.fg, Some(Color::Red));
        assert_eq!(rows[1][0].style.fg, Some(Color::Blue));
    }

    /// An empty body renders nothing at all — no stray padded rows for entries
    /// that produced no content (e.g. an assistant message before its first token).
    #[test]
    fn render_block_of_an_empty_body_is_empty() {
        assert!(render_block(Vec::new(), 20, Color::Reset, None).is_empty());
    }

    /// Content is laid out at width minus one padding column per side, so a body
    /// line that exactly fills `inner_width` still leaves both margins intact.
    #[test]
    fn inner_width_reserves_the_padding_on_each_side() {
        assert_eq!(inner_width(12) as usize, 12 - BLOCK_PAD_X * 2);
        // Degenerate widths must not underflow or produce a zero-width layout.
        assert_eq!(inner_width(BLOCK_PAD_X * 2), 1);
        assert_eq!(inner_width(0), 1);

        // A body line that exactly fills the content width still leaves both
        // margins intact.
        let inner = inner_width(12) as usize;
        let body = vec![Line::from(Span::raw("x".repeat(inner)))];
        let lines = render_block(body, 12, Color::Reset, None);
        let pad = " ".repeat(BLOCK_PAD_X);
        assert_eq!(
            text(&lines[1]),
            format!("{pad}{}{pad}", "x".repeat(inner)),
            "BLOCK_PAD_X bg columns either side"
        );
    }

    /// Backgrounds by kind: the model's voice (output + thinking) sits on the
    /// terminal background, a tool call shares the prompt's, and the remaining
    /// surfaces are mutually distinct so a reader can see where blocks start and
    /// stop.
    #[test]
    fn block_kinds_have_the_right_backgrounds() {
        let t = Theme::default();

        // No override for the banner, the model's output, or its thinking.
        assert_eq!(BlockKind::Header.bg(&t), Color::Reset);
        assert_eq!(BlockKind::Assistant.bg(&t), Color::Reset);
        assert_eq!(BlockKind::Reasoning.bg(&t), Color::Reset);

        // A tool call is part of the user's turn, so it shares that background.
        assert_eq!(BlockKind::Tool.bg(&t), t.user_bg);
        assert_eq!(BlockKind::Queued.bg(&t), t.user_bg);
        assert_eq!(BlockKind::User.bg(&t), t.user_bg);

        assert_eq!(BlockKind::Command.bg(&t), t.command_bg);
        assert_eq!(BlockKind::Stats.bg(&t), t.stats_bg);

        // The tinted surfaces differ from each other and from the terminal.
        let bgs = [
            BlockKind::User.bg(&t),
            BlockKind::Command.bg(&t),
            BlockKind::Stats.bg(&t),
        ];
        for (i, a) in bgs.iter().enumerate() {
            assert_ne!(*a, Color::Reset, "tinted blocks carry their own bg");
            for b in &bgs[i + 1..] {
                assert_ne!(a, b, "block backgrounds must differ: {bgs:?}");
            }
        }
    }

    /// A one-content-row block of `kind`, rendered fresh (never cached, so these
    /// tests can't serve each other's rows out of the thread-local cache).
    fn test_block<'a>(kind: BlockKind) -> PendingBlock<'a> {
        PendingBlock {
            idx: 0,
            kind,
            body: BodySource::Cached(Rc::new(vec![Line::from(Span::raw("x"))])),
            lent: Vec::new(),
            footer: None,
            body_key: (0, 10, false, false),
            tool_idx: None,
            msgs: 0,
        }
    }

    /// The clock a test frame is drawn at (no block under test carries a
    /// timestamp, so its value never reaches the rows).
    fn test_now() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    /// The rows a list of chunks paints, in order.
    fn flatten(chunks: &[Chunk]) -> Vec<Line<'static>> {
        chunks
            .iter()
            .flat_map(|c| c.rows.rows().iter().cloned().collect::<Vec<_>>())
            .collect()
    }

    /// The gap between two blocks, for every pairing of tinted / untinted.
    ///
    /// Each block carries a blank padded row above and below, so a naive
    /// concatenation always yields two blank rows between them. The rule tunes
    /// that: a separator is added between two tinted blocks (whose pads carry
    /// their backgrounds and would otherwise merge into one slab), and one pad
    /// is dropped between two untinted ones (two plain blanks is one too many).
    #[test]
    fn the_gap_between_blocks_depends_on_both_backgrounds() {
        let theme = Theme::default();
        // A tinted kind without a left bar, so a content row is exactly "x".
        let tinted = BlockKind::Command;
        let plain = BlockKind::Assistant; // Color::Reset
        assert_ne!(tinted.bg(&theme), Color::Reset);
        assert!(tinted.border(&theme).is_none());
        assert_eq!(plain.bg(&theme), Color::Reset);

        // Paint `first`, then `second`, and count the blank rows between their
        // one content row each.
        let gap = |first: BlockKind, second: Option<BlockKind>| -> usize {
            let mut chunks = Vec::new();
            let mut starts = Vec::new();
            flush(
                &mut chunks,
                &mut starts,
                Some(test_block(first)),
                second.map(|k| k.bg(&theme)),
                10,
                test_now(),
                &theme,
            );
            let after_first: usize = chunks.iter().map(|c| c.rows.height()).sum();
            if let Some(second) = second {
                flush(
                    &mut chunks,
                    &mut starts,
                    Some(test_block(second)),
                    None,
                    10,
                    test_now(),
                    &theme,
                );
            }
            let out = flatten(&chunks);
            // Rows after the first block's content row, before the second's.
            let content = |l: &Line<'_>| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .to_string()
            };
            let first_content = out.iter().position(|l| content(l) == "x").unwrap();
            let next_content = out
                .iter()
                .skip(after_first)
                .position(|l| content(l) == "x")
                .map(|i| i + after_first)
                .unwrap_or(out.len());
            next_content - first_content - 1
        };

        // tinted → tinted: bottom pad + separator + top pad.
        assert_eq!(
            gap(tinted, Some(tinted)),
            3,
            "two tinted blocks merge without a separator"
        );
        // untinted → untinted: one pad is dropped.
        assert_eq!(gap(plain, Some(plain)), 1, "a thought and its output");
        // Mixed: the two pads already read as one gap.
        assert_eq!(gap(tinted, Some(plain)), 2, "tinted → untinted");
        assert_eq!(gap(plain, Some(tinted)), 2, "untinted → tinted");

        // Nothing follows: neither kind trails a separator of its own. The blank
        // row between the transcript and the input pane is the layout's, not the
        // last block's — so a tinted block ends flush with its own bottom pad.
        let trailing = |kind: BlockKind| -> usize {
            let mut chunks = Vec::new();
            let mut starts = Vec::new();
            flush(
                &mut chunks,
                &mut starts,
                Some(test_block(kind)),
                None,
                10,
                test_now(),
                &theme,
            );
            flatten(&chunks).len() - 2 // minus the content row and the top pad
        };
        assert_eq!(trailing(tinted), 1, "bottom pad only");
        assert_eq!(trailing(plain), 1, "bottom pad only");
    }

    /// `flush` still records which chunk a block's messages start at and, for a
    /// tool call, which chunk a click can land on — even as the gap rules shift
    /// rows around.
    #[test]
    fn flush_records_message_starts_and_tool_chunks() {
        let theme = Theme::default();
        let mut chunks = vec![separator()]; // something already painted
        let mut starts = Vec::new();
        let mut block = test_block(BlockKind::Tool);
        block.tool_idx = Some(7);
        block.msgs = 2; // a block carrying a borrowed assistant label
        flush(
            &mut chunks,
            &mut starts,
            Some(block),
            None,
            10,
            test_now(),
            &theme,
        );
        assert_eq!(starts, vec![1, 1], "both messages start at the block");
        let tools: Vec<_> = chunks
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.tool_idx.map(|idx| (i, idx)))
            .collect();
        assert_eq!(tools, vec![(1, 7)], "the tool block is chunk 1");
        assert!(chunks[1].rows.height() > 1, "the tool block spans rows");
    }

    /// **Every row a block emits is exactly one screen row.**
    ///
    /// `render_block` wraps each body line to the block's inner width and pads it
    /// out to the full render width, so nothing it produces can wrap again at that
    /// width. The frame leans on this: it places the viewport by *counting* rows
    /// (`cum` in `draw_transcript`) instead of re-wrapping the transcript, which is
    /// what keeps a long session's frame cost flat. If a block ever emitted a row
    /// wider than the render width, that row would wrap on screen, every row below
    /// it would sit one line lower than the scroll math believes, and clicks on
    /// tool blocks would land on the wrong entry.
    #[test]
    fn every_block_row_is_exactly_one_screen_row() {
        let theme = Theme::default();
        let width = 24usize;
        // A long unbroken word, a long sentence, a CJK run (two columns per glyph)
        // and an empty line — every way a body line could overshoot the width.
        let body = vec![
            Line::from(Span::raw("supercalifragilisticexpialidocious")),
            Line::from(Span::raw("the quick brown fox jumps over the lazy dog")),
            Line::from(Span::raw("日本語のテキストは全角です")),
            Line::raw(""),
        ];
        for kind in [BlockKind::User, BlockKind::Assistant, BlockKind::Command] {
            let rows = render_block(body.clone(), width, kind.bg(&theme), kind.border(&theme));
            for line in &rows {
                let w: usize = line.spans.iter().map(Span::width).sum();
                assert_eq!(
                    w, width,
                    "every block row must fill exactly the render width: {line:?}"
                );
            }
        }
    }

    /// The tool header always leads with a status mark that reflects the call's
    /// state: running (a SPINNER frame), succeeded, or failed.
    #[test]
    fn tool_header_mark_tracks_call_status() {
        let t = Theme::default();
        let head = |ok, done| {
            let lines = tool_lines(&t, "ls", r#"{"path":"src"}"#, "", ok, done, false, "⠋");
            text(&lines[0])
        };
        assert!(
            head(false, false).starts_with('⠋'),
            "running with SPINNER frame"
        );
        assert!(head(true, true).starts_with('✓'), "succeeded");
        assert!(head(false, true).starts_with('✗'), "failed");
        // The headline follows the tool name on the same row.
        assert!(head(true, true).contains("ls src"));
    }

    /// Shell calls render the command verbatim on its own rows, with the
    /// command's output below it. No `$ ` prompt — the block's header says
    /// `bash` already.
    #[test]
    fn shell_tool_shows_the_command_then_its_output() {
        let t = Theme::default();
        let lines = tool_lines(
            &t,
            "bash",
            r#"{"command":"ls\nwc -l"}"#,
            "a.rs\nb.rs",
            true,
            true,
            false,
            "",
        );
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows[0].trim(), "✓ bash", "no args preview on the header");
        assert_eq!(rows[1], "ls", "the command, verbatim");
        assert_eq!(rows[2], "wc -l", "its continuation lines too");
        assert_eq!(rows[3], "a.rs");
        assert_eq!(rows[4], "b.rs");
    }

    /// A `write` shows the file name and the contents it wrote — not the unified
    /// diff the tool returns, which would repeat them.
    #[test]
    fn write_tool_shows_the_path_and_the_file_contents() {
        let t = Theme::default();
        let args = r#"{"path":"a.rs","content":"fn main() {}\n"}"#;
        let diff_result = "--- a/a.rs\n+++ b/a.rs\n+fn main() {}";
        let rows: Vec<String> = tool_lines(&t, "write", args, diff_result, true, true, false, "")
            .iter()
            .map(text)
            .collect();
        assert!(rows[0].contains("write a.rs"));
        // Contents render raw: no gutter, no width fill, no language bar. The
        // block's own padding is the only indent the reader sees.
        assert_eq!(rows[1], "fn main() {}", "raw contents: {rows:?}");
        assert!(
            !rows.iter().any(|r| r.trim() == "rs"),
            "no language tag bar: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("+++ b/a.rs")),
            "the result diff is not repeated: {rows:?}"
        );
    }

    /// Indented file contents keep their own indentation exactly — the renderer
    /// must not add to it, or every written file reads as over-indented.
    #[test]
    fn write_tool_preserves_the_files_own_indentation() {
        let t = Theme::default();
        let args = r#"{"path":"a.rs","content":"fn main() {\n    let x = 1;\n}"}"#;
        let rows: Vec<String> = tool_lines(&t, "write", args, "", true, true, false, "")
            .iter()
            .map(text)
            .collect();
        assert_eq!(rows[1], "fn main() {");
        assert_eq!(rows[2], "    let x = 1;", "4 spaces, not 5 or 6");
        assert_eq!(rows[3], "}");
    }

    /// `task` (and any MCP tool) renders its args as aligned `key  value` rows
    /// under the tool's name — the header carries no JSON.
    ///
    /// Regression: the header line was `✓ task {"agent": "explore", …}`.
    #[test]
    fn a_task_call_shows_aligned_detail_rows_under_its_name() {
        let t = Theme::default();
        let args = r#"{"agent":"explore","description":"Explore hrdr-editor","prompt":"line one\nline two"}"#;
        let rows: Vec<String> = tool_lines(&t, "task", args, "", true, true, false, "")
            .iter()
            .map(text)
            .collect();

        assert_eq!(rows[0], "✓ task", "no args on the header line");
        // Keys padded to the widest ("description"), values aligned after it.
        assert_eq!(rows[1], "agent        explore");
        assert_eq!(rows[2], "description  Explore hrdr-editor");
        assert_eq!(rows[3], "prompt       line one line two", "one row per arg");
        assert_eq!(rows.len(), 4);
    }

    /// A long detail value is clipped while collapsed and shown whole when the
    /// block is expanded.
    #[test]
    fn a_long_detail_value_is_clipped_until_expanded() {
        let t = Theme::default();
        let prompt = "x".repeat(DETAIL_VALUE_W + 50);
        let args = format!(r#"{{"prompt":"{prompt}"}}"#);
        let row = |expanded| text(&tool_lines(&t, "task", &args, "", true, true, expanded, "")[1]);

        let collapsed = row(false);
        assert!(collapsed.len() < prompt.len(), "clipped: {collapsed}");
        assert!(row(true).contains(&prompt), "expanded shows it whole");
    }

    /// A failed `write` still surfaces the error the tool returned.
    #[test]
    fn failed_write_shows_the_error_result() {
        let t = Theme::default();
        let args = r#"{"path":"a.rs","content":"x"}"#;
        let rows: Vec<String> =
            tool_lines(&t, "write", args, "Error: denied", false, true, false, "")
                .iter()
                .map(text)
                .collect();
        assert!(rows.iter().any(|r| r.contains("Error: denied")), "{rows:?}");
    }

    /// `edit` colors its result as a unified diff: additions in success green,
    /// deletions in error red.
    #[test]
    fn edit_tool_colors_the_patch() {
        let t = Theme::default();
        let args = r#"{"path":"a.rs","old_string":"a","new_string":"b"}"#;
        let lines = tool_lines(
            &t,
            "edit",
            args,
            "@@ -1 +1 @@\n-a\n+b",
            true,
            true,
            false,
            "",
        );
        assert!(text(&lines[0]).contains("edit a.rs"));
        let color = |i: usize| lines[i].spans[0].style.fg;
        assert_eq!(color(2), Some(t.error), "deletion is red");
        assert_eq!(color(3), Some(t.success), "addition is green");
    }

    /// Collapsed results are capped at a preview and advertise the remainder;
    /// expanding shows every line with no hint.
    #[test]
    fn long_results_are_previewed_until_expanded() {
        let t = Theme::default();
        let result: String = (0..TOOL_RESULT_PREVIEW_LINES + 5)
            .map(|i| format!("line {i}\n"))
            .collect();
        let args = r#"{"path":"src"}"#;

        let collapsed = tool_lines(&t, "ls", args, &result, true, true, false, "");
        let rows: Vec<String> = collapsed.iter().map(text).collect();
        assert_eq!(rows.len(), 1 + TOOL_RESULT_PREVIEW_LINES + 1, "{rows:?}");
        assert!(rows.last().unwrap().contains("+5 more lines"), "{rows:?}");

        let expanded = tool_lines(&t, "ls", args, &result, true, true, true, "");
        let rows: Vec<String> = expanded.iter().map(text).collect();
        assert_eq!(rows.len(), 1 + TOOL_RESULT_PREVIEW_LINES + 5, "{rows:?}");
        assert!(!rows.last().unwrap().contains("more lines"), "{rows:?}");
    }

    /// `read` previews the *tail* of its result — the file content, not the
    /// preamble the tool prints above it.
    #[test]
    fn read_tool_previews_the_tail_of_its_result() {
        let t = Theme::default();
        let result: String = (0..TOOL_RESULT_PREVIEW_LINES + 3)
            .map(|i| format!("line {i}\n"))
            .collect();
        let rows: Vec<String> = tool_lines(
            &t,
            "read",
            r#"{"path":"a.rs"}"#,
            &result,
            true,
            true,
            false,
            "",
        )
        .iter()
        .map(text)
        .collect();
        // 11 result lines, 8 previewed: lines 3..=10, with the hint above them
        // (for `read` the hidden lines are the ones scrolled off the top).
        assert!(rows[1].contains("+3 more lines"), "hint above: {rows:?}");
        assert_eq!(rows[2], "line 3", "{rows:?}");
        assert_eq!(rows.last().unwrap(), "line 10", "tail is last: {rows:?}");
        assert!(
            !rows.iter().any(|r| r == "line 0"),
            "head dropped: {rows:?}"
        );
    }

    /// A running tool shows the live tail of its output, flagged as such.
    #[test]
    fn running_tool_shows_the_live_tail() {
        let t = Theme::default();
        let result: String = (0..TOOL_RESULT_PREVIEW_LINES + 2)
            .map(|i| format!("line {i}\n"))
            .collect();
        let rows: Vec<String> = tool_lines(
            &t,
            "bash",
            r#"{"command":"x"}"#,
            &result,
            false,
            false,
            false,
            "⠋",
        )
        .iter()
        .map(text)
        .collect();
        // 10 result lines, 8 shown live: the 2 oldest are summarized instead.
        assert!(rows[2].contains("live · 2 earlier line(s)"), "{rows:?}");
        assert_eq!(rows[3], "line 2", "{rows:?}");
        assert_eq!(rows.last().unwrap(), "line 9", "newest is last: {rows:?}");
    }

    /// Reasoning uses the same markdown roles as assistant output, only dimmer —
    /// same hue, lower brightness.
    #[test]
    fn reasoning_markdown_is_a_dimmer_assistant() {
        let t = Theme::default();
        let (bright, dim) = (t.md_theme(), t.md_theme_dim());
        assert_ne!(bright.text, dim.text, "reasoning text is dimmed");
        let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (bright.text, dim.text) else {
            panic!("default theme resolves to RGB");
        };
        assert!(r2 < r1 && g2 < g1 && b2 < b1, "every channel is darkened");
    }

    /// The animation path covers every glyph cell of whatever art it's given, so
    /// changing the art can't leave the cursor tracing cells that aren't there.
    #[test]
    fn the_logo_path_is_derived_from_the_art() {
        const ART: &str = "█ █\n███\n█ █";
        let (rows, cols) = logo_size(ART);
        let glyphs = ART.chars().filter(|c| !c.is_whitespace()).count();
        let path = logo_path(ART);
        assert_eq!(path.len(), glyphs, "one path cell per glyph");
        for (row, col, _) in &path {
            assert!((*row as u16) < rows && (*col as u16) < cols, "in bounds");
        }
    }
}
