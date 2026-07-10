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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Entry, EntryKind, StatusBarMode, TimestampStyle};
use crate::theme::Theme;
use hrdr_app::{PanelItem, panel_item_header, panel_items, relative_time};

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Diff results (edit/write) get a larger preview since the diff is the point.
const DIFF_PREVIEW_LINES: usize = 40;
/// Max lines shown in the TODO panel (plus 2 for borders).
const TODO_PANEL_MAX_ITEMS: u16 = 6;
/// Max rows (one per agent) the sub-agent panel lists; beyond this the panel
/// scrolls its rows off the top (newest at the bottom).
const SUBAGENT_PANEL_MAX_ROWS: u16 = 18;

/// Outer height (with padding) of the sub-agent panel; 0 when nothing is shown.
fn subagent_panel_height(items: &[PanelItem]) -> u16 {
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

    // The loader heads the input section while the *model* works. It hides while
    // its tool calls run: the model is idle then, and a spinner would claim
    // otherwise. The running tool's own block carries the `…` mark.
    let loader_height: u16 = if app.inferring || app.compacting {
        1
    } else {
        0
    };

    // Input pane auto-grows 1..=INPUT_MAX_ROWS text rows with the content.
    // Inner width = full width minus the horizontal padding on both sides; the
    // extra two rows are the blank padding above and below.
    let input_inner_w = area.width.saturating_sub(INPUT_PAD_X as u16 * 2);
    let input_height = app
        .editor
        .desired_rows(input_inner_w, hrdr_app::INPUT_MAX_ROWS)
        + 2;

    // Built once per frame; both the layout height and the renderer use it
    // (each sub-agent's log is cloned into the items, so don't recompute).
    let subagent_items = panel_items(&app.subagent_panel.agents, &app.background_tasks);
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
    let subagent_idx = section(&mut constraints, subagent_height);
    let todo_idx = section(&mut constraints, todo_height);
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

    // The `/model` selector is a full modal; when it's open it owns the screen
    // (and every key), so the completion popup stands down.
    if let Some(sel) = &app.model_selector {
        draw_model_selector(f, &app.theme, sel);
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
fn draw_model_selector(f: &mut Frame, theme: &Theme, sel: &crate::app::ModelSelector) {
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
    let hint = Line::from(Span::styled(
        format!(
            "{} model{} · ↑↓ select · Enter switch · Esc cancel",
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
        // Provider on the left, model name right-aligned; the row fills the full
        // inner width so a selected row highlights end to end.
        let provider = truncate_chars(&c.provider_label, (inner_w / 2).max(1));
        let avail = inner_w.saturating_sub(provider.chars().count() + 1).max(1);
        let model = truncate_chars(&c.model_label, avail);
        let pad = inner_w
            .saturating_sub(provider.chars().count() + model.chars().count())
            .max(1);
        let line = if selected {
            Line::from(Span::styled(
                format!("{provider}{}{model}", " ".repeat(pad)),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme.user)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(vec![
                Span::styled(provider, Style::default().fg(theme.dim)),
                Span::styled(
                    format!("{}{model}", " ".repeat(pad)),
                    Style::default().fg(theme.user),
                ),
            ])
        };
        lines.push(line);
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

fn draw_completion(f: &mut Frame, app: &App, input_area: Rect, comp: &crate::app::Completions) {
    let theme = &app.theme;
    // Clamped to the frame itself, not just the input pane: on a very short
    // or narrow terminal, an unclamped popup could ask for more rows/columns
    // than exist, leaving its border/top items rendered outside the visible
    // area (ratatui clips silently — no panic, but nothing there to click).
    let frame_area = f.area();
    // Height: one row per item, plus the block's one padded row above and below.
    let height = (comp.items.len() as u16 + 2).min(frame_area.height.max(1));
    let widest = comp
        .items
        .iter()
        .map(|(n, d)| n.chars().count() + d.chars().count() + 3)
        .max()
        .unwrap_or(24);
    // Outer width adds the block's two padding columns on each side.
    let width = ((widest + BLOCK_PAD_X * 2) as u16)
        .clamp(20, input_area.width.max(20))
        .min(frame_area.width.max(1));
    let rect = Rect {
        x: input_area.x.min(frame_area.width.saturating_sub(width)),
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

    let (lines, msg_starts, tool_regions) = transcript_lines(app, text_area.width);
    // Cumulative wrapped-row height at each logical-line boundary — always built
    // from cached per-line measurements (cheap HashMap lookups) so we never pay
    // for ratatui's full-text re-wrap in para.line_count() every frame.
    //
    // Use usize throughout to avoid u16 overflow on long transcripts; only the
    // final Paragraph::scroll cast (u16) is clamped at the last moment.
    let mut cum = Vec::with_capacity(lines.len() + 1);
    let mut acc: usize = 0;
    cum.push(0usize);
    for line in &lines {
        let h = cached_line_wrap(line, text_area.width);
        acc = acc.saturating_add(h);
        cum.push(acc);
    }
    // Resolve a pending /goto to a from-top wrapped-row offset using cum
    // (avoids another Paragraph::line_count allocation + re-wrap).
    let goto_top: Option<u16> = app.pending_goto.take().and_then(|num| {
        let start = (*msg_starts.get(num.checked_sub(1)?)?).min(lines.len());
        Some(clamp_u16(cum[start]))
    });
    // A tool block that was just expanded or collapsed changed height. While the
    // reader is scrolled up, pull its top to the top of the viewport: the offset
    // is measured from the bottom, so the block would otherwise slide by however
    // many rows it gained or lost. Following the newest output is left alone —
    // the bottom is already pinned.
    let entry_top: Option<u16> = app.pending_scroll_entry.take().and_then(|idx| {
        let (start, ..) = tool_regions.iter().find(|(.., i)| *i == idx)?;
        (app.scroll_offset > 0).then(|| clamp_u16(cum[(*start).min(lines.len())]))
    });
    // A click on a sub-agent row focuses that `task` call: unlike the collapse
    // case above, it moves the view even while following the newest output.
    let focus_top: Option<u16> = app.pending_focus_entry.take().and_then(|idx| {
        let (start, ..) = tool_regions.iter().find(|(.., i)| *i == idx)?;
        Some(clamp_u16(cum[(*start).min(lines.len())]))
    });
    let goto_top = goto_top.or(focus_top).or(entry_top);
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Total wrapped rows at this width — from cum, not para.line_count().
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
    if app.scroll_offset > 0 {
        let grown = max_scroll.saturating_sub(clamp_u16(app.max_scroll));
        app.scroll_offset = app.scroll_offset.saturating_add(grown as usize);
    }
    // A /goto puts the target message at the top of the viewport.
    if let Some(wrapped_start) = goto_top {
        app.scroll_offset = max_scroll.saturating_sub(wrapped_start) as usize;
    }
    // scroll_offset is rows scrolled UP from the bottom; 0 == follow newest.
    // Clamp and write back so "scrolled up" state (and the follow button) is
    // accurate even after the content shrinks.
    let offset = clamp_u16(app.scroll_offset).min(max_scroll);
    app.scroll_offset = offset as usize;
    app.max_scroll = max_scroll as usize;
    let scroll = max_scroll.saturating_sub(offset);

    // Map each tool block's wrapped-row span to the visible screen rows (clipped
    // to the viewport) so a left click can toggle that tool's expansion.
    // Arithmetic is in usize (cum values) to avoid overflow; only the final
    // HitRect fields are cast back to u16.
    app.tool_hits.clear();
    {
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

    // The input pane's chrome, with a green rule instead of the prompt's.
    let bg = app.theme.user_bg;
    let inner = draw_pane(f, &app.theme, area, app.theme.success);

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
fn draw_subagents(f: &mut Frame, app: &mut App, area: Rect, items: &[PanelItem]) {
    let (accent, success) = (app.theme.accent, app.theme.success);
    // The input pane's chrome, with the accent rule a running agent wears — the
    // todo panel's is green.
    let bg = app.theme.user_bg;
    let inner = draw_pane(f, &app.theme, area, accent);

    // One row per agent, newest pinned to the bottom when they overflow.
    let scroll = subagent_scroll(items.len(), inner.height);
    let lines: Vec<Line<'static>> = items
        .iter()
        .map(|item| {
            let fg = if item.done { success } else { accent };
            Line::from(Span::styled(
                panel_item_header(item),
                Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    // A click on a visible row jumps to the `task` call that spawned it.
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
                item.tool_id.clone(),
            )
        })
        .collect();

    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The inference loader: spinner + live stats (context size, in/out ratio,
/// token throughput) shown above the input while a turn runs.
fn draw_loader(f: &mut Frame, app: &App, area: Rect) {
    // The model's own working time: the tool calls it waited on don't count, so
    // the clock freezes while they run rather than inflating the turn.
    let elapsed = app.infer_elapsed();
    let frame = SPINNER[(elapsed.as_millis() / 120) as usize % SPINNER.len()];

    // Live throughput over that same working time — tokens per second of
    // inference, not per second of wall clock.
    let speed = match app.out_tokens {
        0 => 0.0,
        n => {
            let secs = elapsed.as_secs_f64();
            if secs > 0.0 { n as f64 / secs } else { 0.0 }
        }
    };

    let ctx = match app.state.usage.last() {
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
    if app.masks_input() {
        draw_masked_input(f, app, inner);
    } else {
        app.editor.render(f, inner);
    }

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

/// Render the input pane with every character replaced by `•`, for the
/// `/login` API-key prompt. The key is already kept out of history, the
/// transcript, and the session file (see `LoginWizard::enter_key`); this
/// keeps it off the screen too, while the editor still holds — and `/login`
/// still reads — the real text underneath.
///
/// Cursor placement assumes the common case of typing/pasting straight
/// through to the end (true for every key-entry path today); editing back
/// into the middle of a masked value would place the cursor by count alone,
/// same as the wrap below.
fn draw_masked_input(f: &mut Frame, app: &App, area: Rect) {
    let count = app.editor.content().chars().count();
    let width = (area.width.max(1)) as usize;
    let bullets: Vec<char> = std::iter::repeat_n('•', count).collect();
    let lines: Vec<Line> = if bullets.is_empty() {
        vec![Line::from("")]
    } else {
        bullets
            .chunks(width)
            .map(|c| Line::from(c.iter().collect::<String>()))
            .collect()
    };
    f.render_widget(Paragraph::new(lines), area);

    let row = (count / width) as u16;
    let col = (count % width) as u16;
    let sy = area.y + row.min(area.height.saturating_sub(1));
    let sx = area.x + col.min(area.width.saturating_sub(1));
    f.set_cursor_position((sx, sy));
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
    let ttft = match (app.turn_started, app.first_token_at) {
        (Some(start), Some(first)) => Some(first.duration_since(start).as_secs_f64()),
        _ => None,
    };
    // Cap the session name so a long one can't crowd out the left side.
    let session = truncate_chars(&app.state.name, 28);
    let inputs = hrdr_app::StatusInputs {
        dir: &app.dir,
        branch: app.branch.as_deref(),
        tokens_in: app.state.usage.tokens_in,
        tokens_out: app.state.usage.tokens_out,
        ctx_used: app.state.usage.ctx_used(),
        context_window: app.state.usage.context_window,
        auto_compact_enabled: app.auto_compact_enabled,
        compaction_reserved: app.compaction_reserved,
        provider: app.state.provider.as_deref(),
        model: &app.state.model,
        session: Some(session.as_str()),
        effort: app.effort.as_deref(),
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

/// Cache key for rendered transcript entry content lines.
/// Fields: (entry_idx, content_fingerprint, render_width, expand_all, show_reasoning).
type TranscriptKey = (usize, u64, u16, bool, bool);

thread_local! {
    // Incremental syntect state: a streaming block's content grows every token,
    // so only pay for the new lines. (The rendered rows are cached per entry by
    // TRANSCRIPT_CACHE below.)
    static INC_HL: RefCell<hrdr_app::HighlightCache> = RefCell::new(hrdr_app::HighlightCache::new());
    // Cache rendered transcript entry content lines (excluding the per-message
    // timestamp meta line, which is always fresh).
    // Key: (entry_idx, content_fingerprint, render_width, expand_all, show_reasoning).
    // Evicted in bulk at 1024 entries.
    static TRANSCRIPT_CACHE: RefCell<HashMap<TranscriptKey, Vec<Line<'static>>>> = RefCell::new(HashMap::new());
    // Cached wrapped-row height per logical line, keyed on (span-content hash,
    // width). Avoids repeated Paragraph::line_count calls for stable lines when
    // building the cumulative-height array used for tool click hit-testing.
    static LINE_WRAP_CACHE: RefCell<HashMap<(u64, u16), usize>> = RefCell::new(HashMap::new());
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
    field("model", app.state.model.clone(), val);
    field(
        "provider",
        app.state.provider.clone().unwrap_or_else(|| "—".into()),
        val,
    );
    if let Some(e) = &app.effort {
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
fn entry_content_hash(entry: &Entry, expand_all: bool) -> u64 {
    let mut h = DefaultHasher::new();
    match &entry.kind {
        // The header animates and reads live session state; it is never cached.
        EntryKind::Header => {}
        // `took_ms` doesn't affect the rendered rows.
        EntryKind::Reasoning { text, .. } => text.hash(&mut h),
        EntryKind::User(t)
        | EntryKind::Assistant(t)
        | EntryKind::System(t)
        | EntryKind::Notice(t)
        | EntryKind::Stats(t)
        | EntryKind::Diff(t) => t.hash(&mut h),
        EntryKind::Tool {
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

/// Clear the thread-local transcript render cache. Call after mutating the
/// transcript vector (prune, clear, truncate) so stale entry indices — which
/// are part of the cache key — don't cause the wrong content to be displayed.
pub(crate) fn clear_transcript_cache() {
    TRANSCRIPT_CACHE.with(|c| c.borrow_mut().clear());
}

/// Look up `key` in [`TRANSCRIPT_CACHE`]; on miss, call `render()` to produce
/// lines, cache them (evicting the whole map when it exceeds 256 entries), then
/// return, evicting the whole map when it exceeds its cap.
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
        if m.len() > 1024 {
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
        if m.len() > 8192 {
            m.clear();
        }
        m.insert(key, wc);
        wc
    })
}

/// A block whose lines are built but not yet painted. Held for one iteration so
/// a text-less assistant turn — which has no block of its own — can append its
/// `#N assistant` jump label to it.
struct PendingBlock {
    kind: BlockKind,
    lines: Vec<Line<'static>>,
    /// Transcript index, when this block is a tool call (for click-to-expand).
    tool_idx: Option<usize>,
    /// How many numbered messages start at this block. Usually 1 (or 0 for
    /// blocks that aren't messages); a block carrying a borrowed assistant label
    /// counts that message too.
    msgs: usize,
    /// Rows at the end of `lines` that close the block (its `#N …` label). Rows
    /// borrowed by a later entry are inserted *above* them.
    footer_len: usize,
}

impl PendingBlock {
    /// Append rows lent by a following entry — a text-less turn's `#N` label, a
    /// per-turn stats line — keeping the block's own closing label last.
    fn lend(&mut self, rows: Vec<Line<'static>>) {
        let at = self.lines.len() - self.footer_len;
        self.lines.splice(at..at, rows);
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
fn flush(
    out: &mut Vec<Line<'static>>,
    msg_starts: &mut Vec<usize>,
    tool_regions: &mut Vec<(usize, usize, usize)>,
    pending: Option<PendingBlock>,
    next_bg: Option<Color>,
    width: usize,
    theme: &Theme,
) {
    let Some(block) = pending else { return };
    let start = out.len();
    let bg = block.kind.bg(theme);
    out.extend(render_block(
        block.lines,
        width,
        bg,
        block.kind.border(theme),
    ));
    for _ in 0..block.msgs {
        msg_starts.push(start);
    }
    if let Some(i) = block.tool_idx {
        tool_regions.push((start, out.len(), i));
    }
    match (bg == Color::Reset, next_bg.map(|n| n == Color::Reset)) {
        (false, Some(false)) => out.push(Line::raw("")),
        // Drop this block's bottom pad; the next block's top pad is the gap.
        (true, Some(true)) => {
            out.pop();
        }
        _ => {}
    }
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
    // The `#N you · 2m ago` row that closes a message block, preceded by a blank
    // row separating it from the message. Both are block *body* lines (not
    // chrome outside the block), so they sit on the block's background like
    // everything else. Kept out of the render cache: the relative time changes
    // every frame.
    let meta = |i: usize, num: usize, role: &str| -> Vec<Line<'static>> {
        if app.timestamp_style == TimestampStyle::None {
            return Vec::new();
        }
        let time = app
            .state
            .transcript
            .get(i)
            .map(|e| {
                if app.timestamp_style == TimestampStyle::Relative {
                    relative_time(e.time)
                } else {
                    e.time.format("%H:%M").to_string()
                }
            })
            .unwrap_or_default();
        vec![
            Line::raw(""),
            Line::from(Span::styled(
                format!("#{num} {role} · {time}"),
                Style::default().fg(theme.dim),
            )),
        ]
    };
    // Block width and the width its content is laid out at (minus padding).
    let w = width as usize;
    let inner = inner_width(w);
    let mut pending: Option<PendingBlock> = None;
    for (i, entry) in app.state.transcript.iter().enumerate() {
        // Cache key shared by all arms (Reasoning skip happens before this).
        let ck = (
            i,
            entry_content_hash(entry, app.expand_tools),
            width,
            app.expand_tools,
            app.show_reasoning,
        );
        // Every arm produces (kind, header rows, cached body rows, footer rows)
        // and is then funneled through the one `render_block` call below — no
        // entry paints its own chrome.
        let mut header: Vec<Line<'static>> = Vec::new();
        let mut footer: Vec<Line<'static>> = Vec::new();
        // Numbered messages starting at the block this entry produces.
        let mut msg_here = 0usize;
        let (kind, body) = match &entry.kind {
            // Rebuilt every frame: the logo animation advances with the wall
            // clock, and the details mirror the live model/provider.
            EntryKind::Header => (
                BlockKind::Header,
                header_lines(app, app.header_anchor, width),
            ),
            // An assistant turn that only called tools has no text, so it gets no
            // block of its own — but its `#N assistant` label is a `/goto` jump
            // point, so it rides along on the previous block instead.
            EntryKind::Assistant(text) if text.trim().is_empty() => {
                msg_num += 1;
                if let Some(block) = pending.as_mut() {
                    block.lend(meta(i, msg_num, "assistant"));
                    block.msgs += 1;
                } else {
                    // Nothing to append to (it opens the transcript): the label
                    // has nowhere to live, so the message keeps no jump point.
                    msg_starts.push(out.len());
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
                footer.extend(meta(i, msg_num, "you"));
                let md = md_theme.clone();
                let bg = BlockKind::User.bg(theme);
                let body = cache_entry(ck, || markdown_lines(text, &md, bg, inner));
                (BlockKind::User, body)
            }
            // Assistant text is rendered as markdown (headings, lists, emphasis,
            // inline/code spans) via hjkl-markdown; fenced code blocks are pulled
            // out and syntax-highlighted with syntect.
            EntryKind::Assistant(text) => {
                msg_num += 1;
                msg_here += 1;
                footer.extend(meta(i, msg_num, "assistant"));
                let md = md_theme.clone();
                let bg = BlockKind::Assistant.bg(theme);
                let body = cache_entry(ck, || markdown_lines(text, &md, bg, inner));
                (BlockKind::Assistant, body)
            }
            EntryKind::Reasoning { .. } if !app.show_reasoning => continue, // hidden via /reasoning
            // No `⠋ Thinking` / `Thought: 1.2s` label: the dimmer text already
            // says it's the model thinking, and the loader above the input shows
            // that a turn is running. (`took_ms` is still recorded — it's the
            // only trace of how long the model thought.)
            EntryKind::Reasoning { text, .. } => {
                // Same markdown pipeline as assistant, in the same colors —
                // only dimmer, so thoughts read as a quieter version of output.
                let md = theme.md_theme_dim();
                let bg = BlockKind::Reasoning.bg(theme);
                let body = cache_entry(ck, || markdown_lines(text, &md, bg, inner));
                (BlockKind::Reasoning, body)
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
                let expand = *expanded || app.expand_tools;
                let (ok_v, done_v) = (*ok, *done);
                let (name_s, args_s, result_s) = (name.clone(), args.clone(), result.clone());
                let theme_snap = theme.clone();
                let body = cache_entry(ck, || {
                    tool_lines(
                        &theme_snap,
                        &name_s,
                        &args_s,
                        &result_s,
                        ok_v,
                        done_v,
                        expand,
                    )
                });
                (BlockKind::Tool, body)
            }
            // Slash-command output and status notices read like assistant output
            // — same markdown, same colors, no dimming — on their own background.
            EntryKind::System(text) | EntryKind::Notice(text) => {
                let md = md_theme.clone();
                let bg = BlockKind::Command.bg(theme);
                let body = cache_entry(ck, || markdown_lines(text, &md, bg, inner));
                (BlockKind::Command, body)
            }
            // The per-turn stats line belongs to the turn that just ended, so it
            // closes that turn's block rather than opening one of its own.
            EntryKind::Stats(text) => {
                let dim = theme.dim;
                let body = cache_entry(ck, || text_lines(text, Style::default().fg(dim)));
                match pending.as_mut() {
                    Some(block) => {
                        let mut rows = vec![Line::raw("")];
                        rows.extend(body);
                        block.lend(rows);
                        continue;
                    }
                    // Nothing to attach to (it opens the transcript): fall back
                    // to a block of its own.
                    None => (BlockKind::Stats, body),
                }
            }
            // `/diff` is slash-command output too, but with diff coloring
            // instead of markdown.
            EntryKind::Diff(text) => {
                let theme_snap = theme.clone();
                let body = cache_entry(ck, || {
                    text.lines()
                        .map(|line| {
                            Line::from(Span::styled(
                                line.to_string(),
                                Style::default().fg(diff_line_color(line, &theme_snap)),
                            ))
                        })
                        .collect()
                });
                (BlockKind::Command, body)
            }
        };
        // Flush the previous block, then hold this one: a text-less assistant
        // turn that follows appends its label to whatever is pending.
        flush(
            &mut out,
            &mut msg_starts,
            &mut tool_regions,
            pending.take(),
            Some(kind.bg(theme)),
            w,
            theme,
        );
        header.extend(body);
        let footer_len = footer.len();
        header.extend(footer);
        pending = Some(PendingBlock {
            kind,
            lines: header,
            tool_idx: matches!(entry.kind, EntryKind::Tool { .. }).then_some(i),
            msgs: msg_here,
            footer_len,
        });
    }
    // Queued prompts follow the transcript, so the last block is separated from
    // them exactly as it would be from any other tinted block.
    let queued_bg = (!app.queue.is_empty()).then(|| BlockKind::Queued.bg(theme));
    flush(
        &mut out,
        &mut msg_starts,
        &mut tool_regions,
        pending.take(),
        queued_bg,
        w,
        theme,
    );

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

    // Pending queued messages render like user prompts, with a "Queued" badge
    // as the block's last row — through the same block path as everything else,
    // so they pick up the same padding and background.
    if !app.queue.is_empty() {
        let bg = BlockKind::Queued.bg(theme);
        let badge = Style::default().fg(Color::Black).bg(theme.warn).bold();
        for msg in &app.queue {
            let mut body = markdown_lines(msg, &md_theme, bg, inner);
            // A blank row inside the block, so the badge doesn't sit flush
            // against the message text above it.
            body.push(Line::raw(""));
            body.push(Line::from(Span::styled(" Queued ", badge)));
            out.extend(render_block(body, w, bg, BlockKind::Queued.border(theme)));
            // Queued blocks are tinted: a blank row separates them from each
            // other, and the last one from the input pane below.
            out.push(Line::raw(""));
        }
    }

    (out, msg_starts, tool_regions)
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

/// Block body for one tool call: a status header (`… / ✓ / ✗` + tool name +
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
) -> Vec<Line<'static>> {
    let bg = BlockKind::Tool.bg(theme);
    let dim_bg = Style::default().fg(theme.dim).bg(bg);
    let mark = if !done {
        ("…", theme.warn)
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
    use super::{SUBAGENT_PANEL_MAX_ROWS, subagent_panel_height, subagent_scroll};
    use hrdr_app::PanelItem;

    fn items(n: usize) -> Vec<PanelItem> {
        (0..n)
            .map(|i| PanelItem {
                title: format!("agent {i}"),
                done: false,
                tool_id: Some(format!("call-{i}")),
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
            let mut out = Vec::new();
            let (mut starts, mut regions) = (Vec::new(), Vec::new());
            let block = |kind| PendingBlock {
                kind,
                lines: vec![Line::from(Span::raw("x"))],
                tool_idx: None,
                msgs: 0,
                footer_len: 0,
            };
            flush(
                &mut out,
                &mut starts,
                &mut regions,
                Some(block(first)),
                second.map(|k| k.bg(&theme)),
                10,
                &theme,
            );
            let after_first = out.len();
            if let Some(second) = second {
                flush(
                    &mut out,
                    &mut starts,
                    &mut regions,
                    Some(block(second)),
                    None,
                    10,
                    &theme,
                );
            }
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
            let mut out = Vec::new();
            let (mut starts, mut regions) = (Vec::new(), Vec::new());
            flush(
                &mut out,
                &mut starts,
                &mut regions,
                Some(PendingBlock {
                    kind,
                    lines: vec![Line::from(Span::raw("x"))],
                    tool_idx: None,
                    msgs: 0,
                    footer_len: 0,
                }),
                None,
                10,
                &theme,
            );
            out.len() - 2 // minus the content row and the top pad
        };
        assert_eq!(trailing(tinted), 1, "bottom pad only");
        assert_eq!(trailing(plain), 1, "bottom pad only");
    }

    /// `flush` still records where a block's messages start and, for a tool
    /// call, the rows a click can land on — even as the gap rules shift rows.
    #[test]
    fn flush_records_message_starts_and_tool_regions() {
        let theme = Theme::default();
        let mut out = vec![Line::raw("existing")];
        let (mut starts, mut regions) = (Vec::new(), Vec::new());
        flush(
            &mut out,
            &mut starts,
            &mut regions,
            Some(PendingBlock {
                kind: BlockKind::Tool,
                lines: vec![Line::from(Span::raw("x"))],
                tool_idx: Some(7),
                msgs: 2, // a block carrying a borrowed assistant label
                footer_len: 0,
            }),
            None,
            10,
            &theme,
        );
        assert_eq!(starts, vec![1, 1], "both messages start at the block");
        assert_eq!(regions.len(), 1);
        let (start, end, idx) = regions[0];
        assert_eq!((start, idx), (1, 7));
        assert!(end > start, "the tool block spans rows");
    }

    /// The tool header always leads with a status mark that reflects the call's
    /// state: running, succeeded, or failed.
    #[test]
    fn tool_header_mark_tracks_call_status() {
        let t = Theme::default();
        let head = |ok, done| {
            let lines = tool_lines(&t, "ls", r#"{"path":"src"}"#, "", ok, done, false);
            text(&lines[0])
        };
        assert!(head(false, false).starts_with('…'), "running");
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
        let rows: Vec<String> = tool_lines(&t, "write", args, diff_result, true, true, false)
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
        let rows: Vec<String> = tool_lines(&t, "write", args, "", true, true, false)
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
        let rows: Vec<String> = tool_lines(&t, "task", args, "", true, true, false)
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
        let row = |expanded| text(&tool_lines(&t, "task", &args, "", true, true, expanded)[1]);

        let collapsed = row(false);
        assert!(collapsed.len() < prompt.len(), "clipped: {collapsed}");
        assert!(row(true).contains(&prompt), "expanded shows it whole");
    }

    /// A failed `write` still surfaces the error the tool returned.
    #[test]
    fn failed_write_shows_the_error_result() {
        let t = Theme::default();
        let args = r#"{"path":"a.rs","content":"x"}"#;
        let rows: Vec<String> = tool_lines(&t, "write", args, "Error: denied", false, true, false)
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
        let lines = tool_lines(&t, "edit", args, "@@ -1 +1 @@\n-a\n+b", true, true, false);
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

        let collapsed = tool_lines(&t, "ls", args, &result, true, true, false);
        let rows: Vec<String> = collapsed.iter().map(text).collect();
        assert_eq!(rows.len(), 1 + TOOL_RESULT_PREVIEW_LINES + 1, "{rows:?}");
        assert!(rows.last().unwrap().contains("+5 more lines"), "{rows:?}");

        let expanded = tool_lines(&t, "ls", args, &result, true, true, true);
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
        let rows: Vec<String> =
            tool_lines(&t, "read", r#"{"path":"a.rs"}"#, &result, true, true, false)
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
