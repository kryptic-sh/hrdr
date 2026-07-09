//! Rendering: transcript + TODO panel + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::app::{App, Entry, StatusBarMode, TimestampStyle};
use crate::theme::Theme;
use hrdr_app::{
    PanelHit, PanelItem, panel_item_body, panel_item_header, panel_item_rows, panel_items,
    relative_time,
};

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Diff results (edit/write) get a larger preview since the diff is the point.
const DIFF_PREVIEW_LINES: usize = 40;
/// Max lines shown in the TODO panel (plus 2 for borders).
const TODO_PANEL_MAX_ITEMS: u16 = 6;
/// Max content rows the sub-agent panel occupies (plus 2 for borders); beyond
/// this the panel scrolls its content off the top (newest at the bottom).
const SUBAGENT_PANEL_MAX_ROWS: u16 = 18;

/// Outer height (with borders) of the sub-agent panel; 0 when nothing is shown.
fn subagent_panel_height(items: &[PanelItem]) -> u16 {
    if items.is_empty() {
        return 0;
    }
    let content: usize = items.iter().map(panel_item_rows).sum();
    (content as u16).min(SUBAGENT_PANEL_MAX_ROWS) + 2
}

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Snapshot the TODO list while briefly holding the lock; the height and
    // the renderer both use the same snapshot.
    let todos = app.todos.lock().map(|t| t.clone()).unwrap_or_default();
    let todo_height = if todos.is_empty() {
        0
    } else {
        (todos.len() as u16).min(TODO_PANEL_MAX_ITEMS) + 2
    };

    // The inference loader sits just above the input while a turn runs.
    let loader_height: u16 = if app.running { 1 } else { 0 };

    // Input box auto-grows 1..=INPUT_MAX_ROWS text rows with the content.
    // Inner width = full width minus 2 border + 2 horizontal padding columns.
    let input_inner_w = area.width.saturating_sub(4);
    let input_height = app
        .editor
        .desired_rows(input_inner_w, hrdr_app::INPUT_MAX_ROWS)
        + 2;

    // Built once per frame; both the layout height and the renderer use it
    // (each sub-agent's log is cloned into the items, so don't recompute).
    let subagent_items = panel_items(
        &app.subagent_panel.agents,
        &app.background_tasks,
        &app.background_expanded,
    );
    let subagent_height = subagent_panel_height(&subagent_items);

    // Build the row stack dynamically, remembering each section's index.
    let mut constraints = vec![Constraint::Min(3)];
    let subagent_idx = (subagent_height > 0).then(|| {
        constraints.push(Constraint::Length(subagent_height));
        constraints.len() - 1
    });
    let todo_idx = (todo_height > 0).then(|| {
        constraints.push(Constraint::Length(todo_height));
        constraints.len() - 1
    });
    let loader_idx = (loader_height > 0).then(|| {
        constraints.push(Constraint::Length(loader_height));
        constraints.len() - 1
    });
    constraints.push(Constraint::Length(input_height));
    let input_idx = constraints.len() - 1;
    // Status bar: hidden (0 rows), one row (truncate), or wrapped (≤4 rows).
    let sb_sections = build_status_sections(app);
    let sb_height: u16 = match app.statusbar_mode {
        StatusBarMode::None => 0,
        StatusBarMode::Truncate => 1,
        StatusBarMode::Wrap => status_wrap_rows(&sb_sections, area.width as usize).clamp(1, 4),
    };
    let statusbar_idx = (sb_height > 0).then(|| {
        constraints.push(Constraint::Length(sb_height));
        constraints.len() - 1
    });
    constraints.push(Constraint::Length(1)); // help / keybind line
    let help_idx = constraints.len() - 1;

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
        draw_statusbar(f, app, chunks[i], &sb_sections);
    }
    draw_help(f, app, chunks[help_idx]);

    // Completion popup (slash command or `@file`), overlaid above the input.
    if let Some(comp) = app.active_completions() {
        app.completion_idx = app.completion_idx.min(comp.items.len() - 1);
        draw_completion(f, app, chunks[input_idx], &comp);
    }
}

fn draw_completion(f: &mut Frame, app: &App, input_area: Rect, comp: &crate::app::Completions) {
    let theme = &app.theme;
    let height = comp.items.len() as u16 + 2;
    let widest = comp
        .items
        .iter()
        .map(|(n, d)| n.chars().count() + d.chars().count() + 5)
        .max()
        .unwrap_or(24);
    let width = (widest as u16).clamp(20, input_area.width.max(20));
    let rect = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(comp.title())
        .border_style(Style::default().fg(theme.dim));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines: Vec<Line> = comp
        .items
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let name_style = if i == app.completion_idx {
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
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    // Publish the height so key handlers can compute half-page offsets.
    app.transcript_height = area.height;

    // Reserve the rightmost column for the scrollbar.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    let (lines, msg_starts, tool_regions) = transcript_lines(app, text_area.width);
    // Cumulative wrapped-row height at each logical-line boundary, so each tool
    // block's span can be mapped to on-screen rows for click hit-testing. Built
    // before `lines` is consumed by the Paragraph below.
    //
    // Use usize throughout to avoid u16 overflow on long transcripts; only
    // the final Paragraph::scroll cast (u16) is clamped at the last moment.
    // Per-line heights come from LINE_WRAP_CACHE so stable lines don't get
    // re-measured every frame.
    let cum: Vec<usize> = if tool_regions.is_empty() {
        Vec::new()
    } else {
        let mut cum = Vec::with_capacity(lines.len() + 1);
        let mut acc: usize = 0;
        cum.push(0usize);
        for line in &lines {
            let h = cached_line_wrap(line, text_area.width);
            acc = acc.saturating_add(h);
            cum.push(acc);
        }
        cum
    };
    // Resolve a pending /goto to a from-top wrapped-row offset before `lines`
    // is consumed by the Paragraph.
    let goto_top: Option<u16> = app.pending_goto.take().and_then(|num| {
        let start = (*msg_starts.get(num.checked_sub(1)?)?).min(lines.len());
        Some(
            Paragraph::new(lines[..start].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(text_area.width) as u16,
        )
    });
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Count the *wrapped* rows at this width — not the logical line count — so
    // long messages that wrap don't push the newest content below the fold.
    let total = para.line_count(text_area.width) as u16;
    let max_scroll = total.saturating_sub(area.height);
    // Pin the view to the same content while scrolled up. `scroll_offset` is
    // measured from the bottom, so as streaming appends rows `max_scroll` grows
    // and the rendered `scroll = max_scroll - offset` would drift downward. Bump
    // the offset by however much `max_scroll` grew since the last draw (held in
    // `app.max_scroll`) so the from-top position stays put. `offset == 0`
    // (following the newest output) is left untouched — it stays pinned to the
    // bottom by design.
    if app.scroll_offset > 0 {
        let grown = max_scroll.saturating_sub(app.max_scroll as u16);
        app.scroll_offset = app.scroll_offset.saturating_add(grown as usize);
    }
    // A /goto puts the target message at the top of the viewport.
    if let Some(wrapped_start) = goto_top {
        app.scroll_offset = max_scroll.saturating_sub(wrapped_start) as usize;
    }
    // scroll_offset is rows scrolled UP from the bottom; 0 == follow newest.
    // Clamp and write back so "scrolled up" state (and the follow button) is
    // accurate even after the content shrinks.
    let offset = (app.scroll_offset as u16).min(max_scroll);
    app.scroll_offset = offset as usize;
    app.max_scroll = max_scroll as usize;
    let scroll = max_scroll.saturating_sub(offset);

    // Map each tool block's wrapped-row span to the visible screen rows (clipped
    // to the viewport) so a left click can toggle that tool's expansion.
    // Arithmetic is in usize (cum values) to avoid overflow; only the final
    // HitRect fields are cast back to u16.
    app.tool_hits.clear();
    if !cum.is_empty() {
        let scroll_us = scroll as usize;
        let view_end = scroll_us.saturating_add(area.height as usize);
        let last = cum.len() - 1;
        for (lstart, lend, idx) in tool_regions {
            let ws = cum[lstart.min(last)];
            let we = cum[lend.min(last)];
            let vis_start = ws.max(scroll_us);
            let vis_end = we.min(view_end);
            if vis_end > vis_start {
                app.tool_hits.push((
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
    }

    f.render_widget(para.scroll((scroll, 0)), text_area);

    // Scrollbar shows total session length + where we are within it. ratatui maps
    // `position` over `0..=content_length-1`, so content_length is the number of
    // scroll positions (max_scroll + 1) — not the raw line total, or the thumb
    // never reaches the bottom when following.
    let mut sb_state = ScrollbarState::new(max_scroll as usize + 1)
        .viewport_content_length(area.height as usize)
        .position(scroll as usize);
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

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" todos ")
        .border_style(Style::default().fg(app.theme.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line<'static>> = todos
        .iter()
        .map(|t| {
            let (mark, color) = match t.status.as_str() {
                "completed" => ("x", app.theme.success),
                "in_progress" => ("~", app.theme.warn),
                _ => (" ", app.theme.dim),
            };
            Line::from(Span::styled(
                format!("[{mark}] {}", t.content),
                Style::default().fg(color),
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

/// Pure helper: given the `(row_start, row_end)` spans of each panel item and
/// the inner panel height, compute the scroll offset (rows clipped from the top
/// so the newest content sits at the bottom) and, for each item, the visible
/// screen row range `(start_from_panel_top, end_from_panel_top)` — `None` for
/// items fully scrolled off. Extracted as a pure function so the scroll math is unit-testable
/// independent of ratatui rendering.
fn subagent_scroll(
    item_spans: &[(usize, usize)],
    inner_height: u16,
) -> (u16, Vec<Option<(u16, u16)>>) {
    let total = item_spans.last().map(|&(_, end)| end).unwrap_or(0);
    let scroll = total.saturating_sub(inner_height as usize) as u16;
    let vis = item_spans
        .iter()
        .map(|&(start, end)| {
            let vis_start = (start as u16).max(scroll);
            let vis_end = (end as u16).min(scroll.saturating_add(inner_height));
            if vis_end > vis_start {
                Some((vis_start - scroll, vis_end - scroll))
            } else {
                None
            }
        })
        .collect();
    (scroll, vis)
}

/// The live sub-agent panel: one block per running `task`, each showing a header
/// (its `↳ task …` line) plus the tail of its output (collapsed) or the whole
/// log (expanded). A left click on a row toggles that agent's expansion; the
/// clickable rects are recorded in `app.subagent_hits`.
///
/// When the total content rows exceed `SUBAGENT_PANEL_MAX_ROWS`, the panel
/// scrolls so the newest agents/logs stay visible at the bottom.
fn draw_subagents(f: &mut Frame, app: &mut App, area: Rect, items: &[PanelItem]) {
    let (accent, dim, success) = (app.theme.accent, app.theme.dim, app.theme.success);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" sub-agents ({}) ", items.len()))
        .border_style(Style::default().fg(dim));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // First pass: build all lines and track each item's row span.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut item_spans: Vec<(usize, usize)> = Vec::new(); // (row_start, row_end) per item
    let mut item_hits: Vec<PanelHit> = Vec::new();
    for item in items {
        let start = lines.len();
        let header_color = if item.done { success } else { accent };
        lines.push(Line::from(Span::styled(
            panel_item_header(item),
            Style::default()
                .fg(header_color)
                .add_modifier(Modifier::BOLD),
        )));
        for l in panel_item_body(item) {
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Style::default().fg(dim),
            )));
        }
        item_spans.push((start, lines.len()));
        item_hits.push(item.hit);
    }

    // Compute scroll offset so the newest rows stay visible at the bottom,
    // and derive each item's visible screen-row range.
    let (scroll, vis_ranges) = subagent_scroll(&item_spans, inner.height);

    // Build hit rects for rows that are at least partially visible.
    let hits: Vec<(crate::app::HitRect, PanelHit)> = vis_ranges
        .into_iter()
        .zip(item_hits)
        .filter_map(|(vis, hit)| {
            vis.map(|(y_start, y_end)| {
                (
                    crate::app::HitRect {
                        x: inner.x,
                        y: inner.y + y_start,
                        w: inner.width,
                        h: y_end - y_start,
                    },
                    hit,
                )
            })
        })
        .collect();

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    app.subagent_hits = hits;
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The inference loader: spinner + live stats (context size, in/out ratio,
/// token throughput) shown above the input while a turn runs.
fn draw_loader(f: &mut Frame, app: &App, area: Rect) {
    let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
    let frame = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];

    // Live throughput since the first token arrived.
    let speed = match app.first_token_at {
        Some(t0) if app.out_tokens > 0 => {
            let secs = t0.elapsed().as_secs_f64();
            if secs > 0.0 {
                app.out_tokens as f64 / secs
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let ctx = match app.last_usage {
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
    let started = match (app.timestamp_style, app.turn_started_at) {
        (TimestampStyle::None, _) | (_, None) => String::new(),
        (TimestampStyle::Relative, Some(t)) => format!("  ·  started {}", relative_time(t)),
        (TimestampStyle::Exact, Some(t)) => format!("  ·  started {}", t.format("%H:%M")),
    };
    let text = if app.compacting {
        format!(
            " {frame} compacting context — summarizing the conversation…  ·  {:.1}s{started}",
            elapsed.as_secs_f64(),
        )
    } else {
        // Time to first token: how long the provider took to start streaming.
        let ttft = match (app.turn_started, app.first_token_at) {
            (Some(start), Some(first)) => {
                format!(
                    "  ·  ttft {:.2}s",
                    first.duration_since(start).as_secs_f64()
                )
            }
            _ => String::new(),
        };
        let phase = if app.first_token_at.is_some() {
            "generating"
        } else {
            "inferring"
        };
        format!(
            " {frame} {phase}  ·  {ctx}  ·  {speed:.1} tok/s ({} out){ttft}  ·  {:.1}s{started}",
            app.out_tokens,
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

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let mode = app.editor.mode_label();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" input [{mode}] "))
        .border_style(Style::default().fg(app.theme.dim))
        .padding(Padding::horizontal(1));
    // Rough size of the draft on the bottom-right border (~4 chars/token).
    let chars = app.editor.content().chars().count();
    if chars > 0 {
        let toks = chars.div_ceil(4);
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" ~{toks} tok · {chars} ch "),
                Style::default().fg(app.theme.dim),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.editor.render(f, inner);

    // Overlay on the top border. The quit-confirm hint takes priority over the
    // "follow output" button when both would apply.
    if app.quit_armed {
        top_border_button(
            f,
            area,
            " Press Ctrl+C again to quit ",
            Style::default()
                .fg(Color::White)
                .bg(app.theme.error)
                .add_modifier(Modifier::BOLD),
        );
        // Not clickable; and it sits over the follow button's spot.
        app.follow_button = None;
    } else if app.scroll_offset > 0 {
        let rect = top_border_button(
            f,
            area,
            " Press END to follow output ↓ ",
            Style::default()
                .fg(Color::Black)
                .bg(app.theme.warn)
                .add_modifier(Modifier::BOLD),
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

/// Render a centered, single-row label over the top border of `area` and
/// return its screen rect (for click hit-testing).
fn top_border_button(f: &mut Frame, area: Rect, label: &str, style: Style) -> Rect {
    let w = (label.chars().count() as u16).min(area.width);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let rect = Rect {
        x,
        y: area.y,
        width: w,
        height: 1,
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), rect);
    rect
}

/// One status-bar section: `(priority, spans)`. Lower priority is kept longer;
/// higher is dropped first in truncate mode.
type StatusSection = (u8, Vec<Span<'static>>);

fn status_section_width(s: &StatusSection) -> usize {
    s.1.iter().map(Span::width).sum()
}

/// Build the status-bar sections from the shared content model
/// ([`hrdr_app::status_sections`] — same sections/priorities as the GUI),
/// mapping each color role onto the terminal theme.
fn build_status_sections(app: &App) -> Vec<StatusSection> {
    let t = &app.theme;
    let ttft = match (app.turn_started, app.first_token_at) {
        (Some(start), Some(first)) => Some(first.duration_since(start).as_secs_f64()),
        _ => None,
    };
    let inputs = hrdr_app::StatusInputs {
        dir: &app.dir,
        branch: app.branch.as_deref(),
        tokens_in: app.session_in,
        tokens_out: app.session_out,
        ctx_used: app.last_usage.map(|(p, _)| p as usize).unwrap_or(0),
        context_window: app.context_window,
        auto_compact_enabled: app.auto_compact_enabled,
        compaction_reserved: app.compaction_reserved,
        model: &app.model,
        effort: app.effort.as_deref(),
        ttft,
        nerd_icons: app.icon_mode == hjkl_icons::IconMode::Nerd,
    };
    hrdr_app::status_sections(&inputs)
        .into_iter()
        .map(|seg| {
            let spans = seg
                .runs
                .into_iter()
                .map(|run| Span::styled(run.text, status_role_style(run.role, t)))
                .collect();
            (seg.priority, spans)
        })
        .collect()
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
/// Wrap; None is handled by the caller not allocating a row).
fn draw_statusbar(f: &mut Frame, app: &App, area: Rect, sections: &[StatusSection]) {
    let t = &app.theme;
    let width = area.width as usize;
    let lines = match app.statusbar_mode {
        StatusBarMode::Wrap => status_wrap_lines(sections, width, t),
        _ => vec![status_truncate_line(sections, width, t)],
    };
    f.render_widget(Paragraph::new(lines), area);
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

/// Help / keybind line. (Turn state is shown by the loader above the input, so
/// no ready/thinking status is repeated here.)
fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let scroll_hint = if app.scroll_offset > 0 {
        format!("  [scroll: {}↑]", app.scroll_offset)
    } else {
        String::new()
    };
    let queue_hint = if app.queue.is_empty() {
        String::new()
    } else {
        format!("  [{} queued]", app.queue.len())
    };
    let text = format!("{}{queue_hint}{scroll_hint}", app.editor.keybind_hint());
    let para = Paragraph::new(text).style(Style::default().fg(app.theme.dim));
    f.render_widget(para, area);
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

/// Cache key for rendered transcript entry content lines.
/// Fields: (entry_idx, content_fingerprint, render_width, expand_all, show_reasoning).
type TranscriptKey = (usize, u64, u16, bool, bool);

thread_local! {
    // Cache highlighted code blocks (keyed by lang+content+width) so the ~8/sec
    // redraw doesn't re-run syntect every frame.
    static HL_CACHE: RefCell<HashMap<u64, Vec<Line<'static>>>> = RefCell::new(HashMap::new());
    // Incremental syntect state for streaming blocks (misses in HL_CACHE —
    // the content grows every token — only pay for the new lines).
    static INC_HL: RefCell<hrdr_app::HighlightCache> = RefCell::new(hrdr_app::HighlightCache::new());
    // Cache rendered transcript entry content lines (excluding the per-message
    // timestamp meta line, which is always fresh).
    // Key: (entry_idx, content_fingerprint, render_width, expand_all, show_reasoning).
    // Evicted in bulk at 256 entries — same policy as HL_CACHE.
    static TRANSCRIPT_CACHE: RefCell<HashMap<TranscriptKey, Vec<Line<'static>>>> = RefCell::new(HashMap::new());
    // Cached wrapped-row height per logical line, keyed on (span-content hash,
    // width). Avoids repeated Paragraph::line_count calls for stable lines when
    // building the cumulative-height array used for tool click hit-testing.
    static LINE_WRAP_CACHE: RefCell<HashMap<(u64, u16), usize>> = RefCell::new(HashMap::new());
}

/// Render a fenced code block with syntect highlighting on a distinct
/// background, padded to a solid rectangle. Cached per (lang, content, width).
fn highlight_code_block(lang: &str, content: &str, width: u16) -> Vec<Line<'static>> {
    let mut hasher = DefaultHasher::new();
    lang.hash(&mut hasher);
    content.hash(&mut hasher);
    width.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(cached) = HL_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }
    let lines = render_code_block(lang, content, width);
    HL_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, lines.clone());
    });
    lines
}

/// The shared panel background for code blocks and tool output (the syntect
/// theme's background, with a dark fallback), so both render as solid blocks.
fn panel_bg() -> Color {
    let (r, g, b) = hrdr_app::panel_bg_rgb();
    Color::Rgb(r, g, b)
}

fn render_code_block(lang: &str, content: &str, width: u16) -> Vec<Line<'static>> {
    let bg = panel_bg();
    let bg_only = Style::default().bg(bg);
    let w = width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();

    // A small language tag bar atop the block.
    if !lang.is_empty() {
        out.push(pad_line(
            vec![Span::styled(
                format!(" {lang} "),
                Style::default()
                    .fg(Color::Gray)
                    .bg(bg)
                    .add_modifier(Modifier::ITALIC),
            )],
            w,
            bg,
        ));
    }

    // Incremental: a streaming block only highlights its new lines per frame
    // (the shared cache resumes syntect state from the last call).
    let hl_lines = INC_HL.with(|c| c.borrow_mut().highlight(lang, content));
    for ranges in hl_lines {
        let mut spans: Vec<Span<'static>> = vec![Span::styled(" ", bg_only)]; // left gutter
        for (style, piece) in ranges {
            let piece = piece.trim_end_matches(['\n', '\r']);
            if piece.is_empty() {
                continue;
            }
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            spans.push(Span::styled(
                piece.to_string(),
                Style::default().fg(fg).bg(bg),
            ));
        }
        out.push(pad_line(spans, w, bg));
    }
    out
}

/// Pad a line of spans with background-colored spaces out to `width` columns so
/// the code block renders as a solid block.
fn pad_line(mut spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let used: usize = spans.iter().map(Span::width).sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}

/// Compute a stable fingerprint for the content-dependent parts of a transcript
/// entry. Only the parts that affect the visual output are hashed; the
/// timestamp meta line (which changes on /timestamps) is rendered separately and
/// is intentionally excluded so timestamp-only frames still get cache hits.
fn entry_content_hash(entry: &Entry, expand_all: bool) -> u64 {
    let mut h = DefaultHasher::new();
    match entry {
        Entry::User(t)
        | Entry::Assistant(t)
        | Entry::Reasoning(t)
        | Entry::System(t)
        | Entry::Stats(t)
        | Entry::Diff(t) => t.hash(&mut h),
        Entry::Tool {
            name,
            args,
            result,
            ok,
            done,
            expanded,
            ..
        } => {
            name.hash(&mut h);
            args.hash(&mut h);
            result.hash(&mut h);
            ok.hash(&mut h);
            done.hash(&mut h);
            (*expanded || expand_all).hash(&mut h);
        }
    }
    h.finish()
}

/// Look up `key` in [`TRANSCRIPT_CACHE`]; on miss, call `render()` to produce
/// lines, cache them (evicting the whole map when it exceeds 256 entries), then
/// return. The same eviction policy as `HL_CACHE`.
fn cache_entry<F>(key: TranscriptKey, render: F) -> Vec<Line<'static>>
where
    F: FnOnce() -> Vec<Line<'static>>,
{
    if let Some(cached) = TRANSCRIPT_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }
    let lines = render();
    TRANSCRIPT_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, lines.clone());
    });
    lines
}

/// Return the wrapped-row height of `line` at `width`, using [`LINE_WRAP_CACHE`]
/// so repeated calls for stable lines don't re-measure every frame. Style is
/// omitted from the key (style doesn't affect wrapping, only color).
fn cached_line_wrap(line: &Line<'static>, width: u16) -> usize {
    let mut h = DefaultHasher::new();
    for span in &line.spans {
        span.content.hash(&mut h);
    }
    let key = (h.finish(), width);
    LINE_WRAP_CACHE.with(|c| {
        if let Some(&wc) = c.borrow().get(&key) {
            return wc;
        }
        let wc = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1);
        let mut m = c.borrow_mut();
        if m.len() > 2048 {
            m.clear();
        }
        m.insert(key, wc);
        wc
    })
}

/// Returns the rendered transcript lines, the logical-line index each 1-based
/// user/assistant message starts at (for `/goto`), and each tool block's
/// `(logical_start, logical_end, transcript_index)` span (for click-to-expand).
#[allow(clippy::type_complexity)]
fn transcript_lines(
    app: &App,
    width: u16,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<(usize, usize, usize)>) {
    let theme = &app.theme;
    let md_theme = theme.md_theme();
    let mut out: Vec<Line> = Vec::new();
    let mut msg_starts: Vec<usize> = Vec::new();
    let mut tool_regions: Vec<(usize, usize, usize)> = Vec::new();
    // Number user/assistant messages so `/copy msg N` lines up with the display.
    let mut msg_num = 0usize;
    let meta = |out: &mut Vec<Line<'static>>, i: usize, num: usize, role: &str| {
        if app.timestamp_style == TimestampStyle::None {
            return;
        }
        let time = app
            .entry_times
            .get(i)
            .map(|t| {
                if app.timestamp_style == TimestampStyle::Relative {
                    relative_time(*t)
                } else {
                    t.format("%H:%M").to_string()
                }
            })
            .unwrap_or_default();
        out.push(Line::from(Span::styled(
            format!("#{num} {role} · {time}"),
            Style::default().fg(theme.dim),
        )));
    };
    for (i, entry) in app.transcript.iter().enumerate() {
        // Cache key shared by all arms (Reasoning skip happens before this).
        let ck = (
            i,
            entry_content_hash(entry, app.expand_tools),
            width,
            app.expand_tools,
            app.show_reasoning,
        );
        match entry {
            Entry::User(text) => {
                msg_num += 1;
                msg_starts.push(out.len());
                // Timestamp meta is always rendered fresh (it changes with time).
                meta(&mut out, i, msg_num, "you");
                // Content lines are cached by (index, content hash, width, flags).
                let user_color = theme.user;
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    push_text(
                        &mut buf,
                        Span::styled("❯ ", Style::default().fg(user_color).bold()),
                        text,
                        Style::default().fg(user_color),
                    );
                    buf
                }));
            }
            // Assistant text is rendered as markdown (headings, lists, emphasis,
            // inline/code spans) via hjkl-markdown; fenced code blocks are pulled
            // out and syntax-highlighted with syntect on a distinct background.
            Entry::Assistant(text) => {
                msg_num += 1;
                msg_starts.push(out.len());
                // Timestamp meta is always rendered fresh.
                meta(&mut out, i, msg_num, "assistant");
                let md_theme_c = md_theme.clone();
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    let mut ev_buf: Vec<hjkl_markdown::Event> = Vec::new();
                    for ev in hjkl_markdown::parse(text) {
                        if let hjkl_markdown::Event::CodeBlock { lang, content } = ev {
                            if !ev_buf.is_empty() {
                                buf.extend(hjkl_markdown_tui::to_lines(
                                    &ev_buf,
                                    &md_theme_c,
                                    width.max(1),
                                ));
                                ev_buf.clear();
                            }
                            buf.extend(highlight_code_block(&lang, &content, width.max(1)));
                        } else {
                            ev_buf.push(ev);
                        }
                    }
                    if !ev_buf.is_empty() {
                        buf.extend(hjkl_markdown_tui::to_lines(
                            &ev_buf,
                            &md_theme_c,
                            width.max(1),
                        ));
                    }
                    buf
                }));
            }
            Entry::Reasoning(_) if !app.show_reasoning => continue, // hidden via /reasoning
            Entry::Reasoning(text) => {
                let dim = theme.dim;
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    push_text(
                        &mut buf,
                        Span::styled("· ", Style::default().fg(dim)),
                        text,
                        Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                    );
                    buf
                }));
            }
            Entry::Tool {
                name,
                args,
                result,
                ok,
                done,
                expanded,
                ..
            } => {
                let start = out.len();
                let expand = *expanded || app.expand_tools;
                let (ok_v, done_v) = (*ok, *done);
                let (name_s, args_s, result_s) = (name.clone(), args.clone(), result.clone());
                let theme_snap = theme.clone();
                let w = width as usize;
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    push_tool(
                        &mut buf,
                        &theme_snap,
                        &name_s,
                        &args_s,
                        &result_s,
                        ok_v,
                        done_v,
                        expand,
                        w,
                    );
                    buf
                }));
                tool_regions.push((start, out.len(), i));
            }
            Entry::System(text) => {
                let dim = theme.dim;
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    push_text(
                        &mut buf,
                        Span::raw(""),
                        text,
                        Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                    );
                    buf
                }));
            }
            Entry::Stats(text) => {
                let dim = theme.dim;
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    push_text(
                        &mut buf,
                        Span::styled("└ ", Style::default().fg(dim)),
                        text,
                        Style::default().fg(dim),
                    );
                    buf
                }));
            }
            Entry::Diff(text) => {
                let theme_snap = theme.clone();
                out.extend(cache_entry(ck, || {
                    let mut buf = Vec::new();
                    for line in text.lines() {
                        buf.push(Line::from(Span::styled(
                            line.to_string(),
                            Style::default().fg(diff_line_color(line, &theme_snap)),
                        )));
                    }
                    buf
                }));
            }
        }
        out.push(Line::raw(""));
    }

    // Highlight the active /find query across the committed transcript.
    if let Some(needle) = app
        .find
        .query
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|q| !q.is_empty())
    {
        let hl = Style::default()
            .fg(Color::Black)
            .bg(theme.warn)
            .add_modifier(Modifier::BOLD);
        for line in out.iter_mut() {
            *line = highlight_line(std::mem::take(line), &needle, hl);
        }
    }

    // Pending queued messages float at the bottom (following the output) until
    // they're actually sent — rendered dimmed, distinct from committed entries.
    if !app.queue.is_empty() {
        out.push(Line::from(Span::styled(
            "— queued —",
            Style::default().fg(theme.dim),
        )));
        for msg in &app.queue {
            push_text(
                &mut out,
                Span::styled("❯ ", Style::default().fg(theme.dim).bold()),
                msg,
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::ITALIC),
            );
        }
    }

    (out, msg_starts, tool_regions)
}

fn push_text(out: &mut Vec<Line<'static>>, prefix: Span<'static>, text: &str, style: Style) {
    for (i, raw) in text.split('\n').enumerate() {
        if i == 0 {
            out.push(Line::from(vec![
                prefix.clone(),
                Span::styled(raw.to_string(), style),
            ]));
        } else {
            out.push(Line::from(Span::styled(raw.to_string(), style)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_tool(
    out: &mut Vec<Line<'static>>,
    theme: &Theme,
    name: &str,
    args: &str,
    result: &str,
    ok: bool,
    done: bool,
    expanded: bool,
    width: usize,
) {
    // Tool blocks sit on the shared panel background (like code blocks), so each
    // line's spans carry `bg` and are padded out to the full width.
    let bg = panel_bg();
    let dim_bg = Style::default().fg(theme.dim).bg(bg);
    let mark = if !done {
        ("…", theme.warn)
    } else if ok {
        ("✓", theme.success)
    } else {
        ("✗", theme.error)
    };
    let args_preview = hrdr_tools::truncate_inline(args, hrdr_app::TOOL_ARGS_PREVIEW);
    out.push(pad_line(
        vec![
            Span::styled(format!(" {} ", mark.0), Style::default().fg(mark.1).bg(bg)),
            Span::styled(
                name.to_string(),
                Style::default().fg(theme.warn).bg(bg).bold(),
            ),
            Span::styled(format!(" {args_preview}"), dim_bg),
        ],
        width,
        bg,
    ));
    if result.is_empty() {
        return;
    }
    // edit/write/patch return a unified diff — color it and show more lines.
    let is_diff = matches!(name, "edit" | "write" | "patch");
    let preview = if is_diff {
        DIFF_PREVIEW_LINES
    } else {
        TOOL_RESULT_PREVIEW_LINES
    };
    let lines: Vec<&str> = result.lines().collect();
    if done {
        // Finished: show the head of the result (or all of it when expanded).
        let shown = if expanded { lines.len() } else { preview };
        for line in lines.iter().take(shown) {
            let color = if is_diff {
                diff_line_color(line, theme)
            } else {
                theme.dim
            };
            out.push(pad_line(
                vec![Span::styled(
                    format!("   {line}"),
                    Style::default().fg(color).bg(bg),
                )],
                width,
                bg,
            ));
        }
        let extra = lines.len().saturating_sub(shown);
        if extra > 0 {
            let hint = if expanded {
                "   ⌃ (click or /expand off to collapse)".to_string()
            } else {
                format!("   … (+{extra} more lines · click or /expand)")
            };
            out.push(pad_line(vec![Span::styled(hint, dim_bg)], width, bg));
        }
    } else {
        // Still running: show the live tail so the newest output is visible.
        let start = lines.len().saturating_sub(preview);
        if start > 0 {
            out.push(pad_line(
                vec![Span::styled(
                    format!("   ⋮ (live · {start} earlier line(s))"),
                    dim_bg,
                )],
                width,
                bg,
            ));
        }
        for line in &lines[start..] {
            out.push(pad_line(
                vec![Span::styled(format!("   {line}"), dim_bg)],
                width,
                bg,
            ));
        }
    }
}

/// Color for one unified-diff line: additions green, deletions red, hunk
/// headers in the accent color, file headers and context dim.
fn diff_line_color(line: &str, theme: &Theme) -> Color {
    // Shared classification + color semantics (same mapping in the GUI).
    slot_color(
        hrdr_app::diff_kind_slot(hrdr_app::classify_diff_line(line)),
        theme,
    )
}

#[cfg(test)]
mod subagent_tests {
    use super::subagent_scroll;
    use hrdr_app::{PanelHit, PanelItem, SUBAGENT_TAIL_LINES, panel_item_rows};

    fn item(log: &str, expanded: bool) -> PanelItem {
        PanelItem {
            title: log.lines().next().unwrap_or("").to_string(),
            log: log.to_string(),
            expanded,
            done: false,
            hit: PanelHit::Blocking(0),
        }
    }

    #[test]
    fn item_rows_collapsed_caps_the_tail_expanded_shows_all() {
        // header + 6 body lines
        let log = "↳ task: x\na\nb\nc\nd\ne\nf";
        // Collapsed = header + last SUBAGENT_TAIL_LINES.
        assert_eq!(panel_item_rows(&item(log, false)), 1 + SUBAGENT_TAIL_LINES);
        // Expanded = header + all 6.
        assert_eq!(panel_item_rows(&item(log, true)), 1 + 6);
        // A just-started agent (no output yet) is one header row.
        assert_eq!(panel_item_rows(&item("", false)), 1);
    }

    /// 3 items × 2 rows each = 6 total rows, panel height = 4.
    /// scroll = 2: items 0 (rows 0-2) scrolled off, items 1-2 visible.
    #[test]
    fn scroll_pins_newest_to_bottom() {
        let spans = &[(0usize, 2usize), (2, 4), (4, 6)];
        let (scroll, vis) = subagent_scroll(spans, 4);
        assert_eq!(scroll, 2);
        assert_eq!(vis[0], None, "item 0 fully scrolled off");
        assert_eq!(vis[1], Some((0, 2)), "item 1 at top of viewport");
        assert_eq!(vis[2], Some((2, 4)), "item 2 at bottom of viewport");
    }

    /// Content fits without scrolling: scroll = 0, all items fully visible.
    #[test]
    fn no_scroll_when_content_fits() {
        let spans = &[(0, 2), (2, 4)];
        let (scroll, vis) = subagent_scroll(spans, 10);
        assert_eq!(scroll, 0);
        assert_eq!(vis[0], Some((0, 2)));
        assert_eq!(vis[1], Some((2, 4)));
    }

    /// Empty panel: no items, no scroll.
    #[test]
    fn empty_panel_no_scroll() {
        let (scroll, vis) = subagent_scroll(&[], 10);
        assert_eq!(scroll, 0);
        assert!(vis.is_empty());
    }

    /// Partially visible item at the scroll boundary.
    #[test]
    fn partial_visibility_at_boundary() {
        // 1 item of 6 rows, panel height = 4 → scroll = 2.
        // Item spans rows 0-6; vis: start = max(0,2)=2, end = min(6,2+4)=6 → (0,4).
        let spans = &[(0usize, 6usize)];
        let (scroll, vis) = subagent_scroll(spans, 4);
        assert_eq!(scroll, 2);
        assert_eq!(vis[0], Some((0, 4)));
    }
}

/// Test 5 — render-cache equivalence and invalidation.
///
/// `transcript_lines()` cannot be called in unit tests (requires a live `App`
/// and a ratatui `Frame`), so these tests exercise the three pure building
/// blocks that `transcript_lines()` is built on: `entry_content_hash`,
/// `cache_entry`, and `cached_line_wrap`.  Each test targets a distinct
/// failure mode:
///
/// * **wrong-content-served**: a warm cache hit returns different bytes than
///   the closure that originally populated it (caught by `cache_entry_warm_hit_equals_cold_render`).
/// * **cross-entry contamination**: entries stored under different keys
///   collide and entry B's render is served for entry A (caught by
///   `cache_entry_different_keys_do_not_collide`).
/// * **stale expand state**: the user runs `/expand` but the collapsed render
///   is returned because `expand_all` is not part of the hash (caught by
///   `entry_content_hash_tool_expand_flag_changes_hash`).
/// * **wrong wrap height**: the hit-test geometry for click-to-expand Tool
///   regions miscalculates because width is dropped from the LINE_WRAP_CACHE
///   key (caught by `cached_line_wrap_deterministic_and_width_sensitive`).
#[cfg(test)]
mod cache_tests {
    use super::{
        LINE_WRAP_CACHE, TRANSCRIPT_CACHE, cache_entry, cached_line_wrap, entry_content_hash,
    };
    use crate::app::Entry;
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
        let a = Entry::User("hello".to_string());
        let b = Entry::User("world".to_string());
        assert_ne!(
            entry_content_hash(&a, false),
            entry_content_hash(&b, false),
            "User entries with different text must produce different hashes"
        );

        let c = Entry::Assistant("response one".to_string());
        let d = Entry::Assistant("response two".to_string());
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
        let tool = Entry::Tool {
            id: "t1".to_string(),
            name: "bash".to_string(),
            args: "{}".to_string(),
            result: "long output".to_string(),
            ok: true,
            done: true,
            expanded: false, // not locally expanded
        };

        let h_collapsed = entry_content_hash(&tool, false);
        let h_global_expand = entry_content_hash(&tool, true);
        assert_ne!(
            h_collapsed, h_global_expand,
            "expand_all=true vs false must produce different hashes when \
             the Tool entry itself is not locally expanded"
        );

        // If the Tool is already locally expanded, the effective state is
        // `true` regardless of expand_all → both should hash identically.
        let tool_local = Entry::Tool {
            id: "t2".to_string(),
            name: "bash".to_string(),
            args: "{}".to_string(),
            result: "long output".to_string(),
            ok: true,
            done: true,
            expanded: true, // locally expanded → effective = true in both cases
        };
        assert_eq!(
            entry_content_hash(&tool_local, false),
            entry_content_hash(&tool_local, true),
            "a Tool with expanded=true must hash identically for any expand_all \
             value (effective state is always true)"
        );
    }

    // ── cache_entry ────────────────────────────────────────────────────────────

    /// A warm cache hit must return exactly the same `Vec<Line>` as the initial
    /// cold render, and the render closure must not be called a second time.
    ///
    /// Regression: if `cache_entry` returned a fresh render on every call
    /// (ignoring the stored value), the output would still be correct only if
    /// the render function is deterministic.  Any theme-dependent colour or
    /// incremental syntax-highlight state that differs between frames would
    /// produce visually wrong output.  The panic inside the warm closure proves
    /// the cache is actually consulted.
    #[test]
    fn cache_entry_warm_hit_equals_cold_render() {
        TRANSCRIPT_CACHE.with(|c| c.borrow_mut().clear());

        // Use an unusual key that won't collide with other tests even if they
        // run in the same thread (thread-locals are shared within a thread).
        let key: (usize, u64, u16, bool, bool) = (0xffff, 0xdead_beef_cafe_0001, 80, false, false);
        let expected = vec![Line::from(Span::raw("cold render content"))];

        // Cold miss — closure must be invoked and its result stored.
        let cold = cache_entry(key, || expected.clone());
        assert_eq!(
            cold, expected,
            "cold render must return what the closure produced"
        );

        // Warm hit — the cached value must be returned; closure panics if called.
        let warm = cache_entry(key, || {
            panic!("closure must not be invoked on a warm cache hit")
        });
        assert_eq!(
            warm, expected,
            "warm cache hit must return the same lines as the cold render"
        );
    }

    /// Two entries stored under different cache keys must never cross-serve:
    /// looking up key B after key A is warm must still produce B's own content.
    ///
    /// Regression: a hash-map key collision (e.g. if the key were reduced to
    /// fewer fields) would silently return A's render when B is looked up.
    /// Users would see one transcript entry rendered with another entry's text.
    #[test]
    fn cache_entry_different_keys_do_not_collide() {
        TRANSCRIPT_CACHE.with(|c| c.borrow_mut().clear());

        let key_a: (usize, u64, u16, bool, bool) = (10, 0xaaaa, 80, false, false);
        let key_b: (usize, u64, u16, bool, bool) = (11, 0xbbbb, 80, false, false);

        let lines_a = vec![Line::from(Span::raw("entry A — unique content"))];
        let lines_b = vec![Line::from(Span::raw("entry B — unique content"))];

        let r_a = cache_entry(key_a, || lines_a.clone());
        let r_b = cache_entry(key_b, || lines_b.clone());

        assert_eq!(r_a, lines_a, "key_a lookup must return lines_a");
        assert_eq!(r_b, lines_b, "key_b lookup must return lines_b");
        assert_ne!(
            r_a, r_b,
            "distinct cache keys must not serve each other's stored lines"
        );
    }

    // ── cached_line_wrap ───────────────────────────────────────────────────────

    /// `cached_line_wrap` must be deterministic (identical inputs → identical
    /// output) and must treat different widths as distinct cache entries.
    ///
    /// Regression: if the `width` dimension were accidentally omitted from the
    /// `LINE_WRAP_CACHE` key, a line measured at width=80 would be returned for
    /// the same content at width=2.  This corrupts the cumulative-height array
    /// used for click-to-expand tool-block hit-testing: every region below the
    /// first miscalculated line would have a wrong vertical offset, making
    /// clicks miss their targets.
    #[test]
    fn cached_line_wrap_deterministic_and_width_sensitive() {
        LINE_WRAP_CACHE.with(|c| c.borrow_mut().clear());

        // "abcd" at width=2 must wrap to 2 rows (2 chars per row);
        // at width=80 it fits on 1 row.
        let line: Line<'static> = Line::from(Span::raw("abcd"));

        let at_2 = cached_line_wrap(&line, 2);
        let at_2_again = cached_line_wrap(&line, 2); // warm hit — must not change
        let at_80 = cached_line_wrap(&line, 80);

        assert!(at_2 >= 1, "wrap height must be at least 1 (narrow)");
        assert!(at_80 >= 1, "wrap height must be at least 1 (wide)");
        assert_eq!(
            at_2, at_2_again,
            "same (line, width) must always return the same height (deterministic)"
        );
        assert!(
            at_2 > at_80,
            "width=2 must produce more rows than width=80 for a 4-char line \
             (got narrow={at_2}, wide={at_80})"
        );
    }
}
