//! Rendering: transcript + TODO panel + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::app::{App, Entry};
use crate::theme::Theme;

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Max lines shown in the TODO panel (plus 2 for borders).
const TODO_PANEL_MAX_ITEMS: u16 = 6;
/// Input box grows with content up to this many text rows (plus 2 for borders).
const INPUT_MAX_ROWS: u16 = 5;

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Snapshot TODO count while briefly holding the lock.
    let todo_count = app.todos.lock().map(|t| t.len()).unwrap_or(0);
    let todo_height = if todo_count > 0 {
        (todo_count as u16).min(TODO_PANEL_MAX_ITEMS) + 2
    } else {
        0
    };

    // The inference loader sits just above the input while a turn runs.
    let loader_height: u16 = if app.running { 1 } else { 0 };

    // Input box auto-grows 1..=INPUT_MAX_ROWS text rows with the content.
    // Inner width = full width minus 2 border + 2 horizontal padding columns.
    let input_inner_w = area.width.saturating_sub(4);
    let input_height = app.editor.desired_rows(input_inner_w, INPUT_MAX_ROWS) + 2;

    // Build the row stack dynamically, remembering each section's index.
    let mut constraints = vec![Constraint::Min(3)];
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
    constraints.push(Constraint::Length(1)); // status bar
    let statusbar_idx = constraints.len() - 1;
    constraints.push(Constraint::Length(1)); // help / keybind line
    let help_idx = constraints.len() - 1;

    let chunks = Layout::vertical(constraints).split(area);

    draw_transcript(f, app, chunks[0]);
    if let Some(i) = todo_idx {
        draw_todos(f, app, chunks[i]);
    }
    if let Some(i) = loader_idx {
        draw_loader(f, app, chunks[i]);
    }
    draw_input(f, app, chunks[input_idx]);
    draw_statusbar(f, app, chunks[statusbar_idx]);
    draw_help(f, app, chunks[help_idx]);
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    // Publish the height so key handlers can compute half-page offsets.
    app.transcript_height = area.height;

    // Reserve the rightmost column for the scrollbar.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    let para = Paragraph::new(transcript_lines(app)).wrap(Wrap { trim: false });
    // Count the *wrapped* rows at this width — not the logical line count — so
    // long messages that wrap don't push the newest content below the fold.
    let total = para.line_count(text_area.width) as u16;
    let max_scroll = total.saturating_sub(area.height);
    // scroll_offset is rows scrolled UP from the bottom; 0 == follow newest.
    // Clamp and write back so "scrolled up" state (and the follow button) is
    // accurate even after the content shrinks.
    let offset = (app.scroll_offset as u16).min(max_scroll);
    app.scroll_offset = offset as usize;
    app.max_scroll = max_scroll as usize;
    let scroll = max_scroll.saturating_sub(offset);

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
    f.render_stateful_widget(scrollbar, area, &mut sb_state);
}

fn draw_todos(f: &mut Frame, app: &App, area: Rect) {
    let todos = match app.todos.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
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

    let phase = if app.first_token_at.is_some() {
        "generating"
    } else {
        "inferring"
    };
    let text = format!(
        " {frame} {phase}  ·  {ctx}  ·  {speed:.1} tok/s ({} out)  ·  {:.1}s",
        app.out_tokens,
        elapsed.as_secs_f64(),
    );
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" input [{mode}] "))
        .border_style(Style::default().fg(app.theme.dim))
        .padding(Padding::horizontal(1));
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
        app.follow_button = Some(rect);
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

/// Rich status bar (above the help line): cwd, git branch, in/out tokens,
/// context size, model, and effort.
fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let sep = || Span::styled("  │  ", Style::default().fg(t.dim));
    let mut spans: Vec<Span> = Vec::new();

    spans.push(Span::styled(
        format!(" {}", app.dir),
        Style::default().fg(t.assistant),
    ));
    if let Some(branch) = &app.branch {
        spans.push(sep());
        spans.push(Span::styled(
            format!(" {branch}"),
            Style::default().fg(t.success),
        ));
    }
    spans.push(sep());
    spans.push(Span::styled(
        format!(
            "↑{} ↓{}",
            fmt_count(app.session_in),
            fmt_count(app.session_out)
        ),
        Style::default().fg(t.dim),
    ));
    spans.push(sep());
    let ctx = app.last_usage.map(|(p, _)| p as usize).unwrap_or(0);
    let ctx_str = match app.context_window {
        Some(w) => format!("{} of {} ctx", fmt_count(ctx), fmt_count(w as usize)),
        None => format!("{} ctx", fmt_count(ctx)),
    };
    spans.push(Span::styled(ctx_str, Style::default().fg(t.warn)));
    spans.push(sep());
    spans.push(Span::styled(
        app.model.clone(),
        Style::default().fg(t.assistant),
    ));
    if let Some(effort) = &app.effort {
        spans.push(sep());
        spans.push(Span::styled(effort.clone(), Style::default().fg(t.warn)));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Help / keybind line.
fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let dot = if app.running { "●" } else { "○" };
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
    let text = format!(
        "{dot} {}{queue_hint}  │  {}{scroll_hint}",
        app.status,
        app.editor.keybind_hint(),
    );
    let para = Paragraph::new(text).style(Style::default().fg(app.theme.dim));
    f.render_widget(para, area);
}

/// Compact token count: `840`, `12.4k`, `1.8M`.
fn fmt_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut out: Vec<Line> = Vec::new();
    for entry in &app.transcript {
        match entry {
            Entry::User(text) => push_text(
                &mut out,
                Span::styled("❯ ", Style::default().fg(theme.user).bold()),
                text,
                Style::default().fg(theme.user),
            ),
            Entry::Assistant(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default().fg(theme.assistant),
            ),
            Entry::Reasoning(text) => push_text(
                &mut out,
                Span::styled("· ", Style::default().fg(theme.dim)),
                text,
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::ITALIC),
            ),
            Entry::Tool {
                name,
                args,
                result,
                ok,
                done,
                ..
            } => push_tool(&mut out, theme, name, args, result, *ok, *done),
            Entry::System(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::ITALIC),
            ),
            Entry::Stats(text) => push_text(
                &mut out,
                Span::styled("└ ", Style::default().fg(theme.dim)),
                text,
                Style::default().fg(theme.dim),
            ),
        }
        out.push(Line::raw(""));
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

    out
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

fn push_tool(
    out: &mut Vec<Line<'static>>,
    theme: &Theme,
    name: &str,
    args: &str,
    result: &str,
    ok: bool,
    done: bool,
) {
    let mark = if !done {
        ("…", theme.warn)
    } else if ok {
        ("✓", theme.success)
    } else {
        ("✗", theme.error)
    };
    let args_preview = truncate_inline(args, 80);
    out.push(Line::from(vec![
        Span::styled(format!("{} ", mark.0), Style::default().fg(mark.1)),
        Span::styled(name.to_string(), Style::default().fg(theme.warn).bold()),
        Span::styled(format!(" {args_preview}"), Style::default().fg(theme.dim)),
    ]));
    if done && !result.is_empty() {
        for line in result.lines().take(TOOL_RESULT_PREVIEW_LINES) {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.dim),
            )));
        }
        let extra = result
            .lines()
            .count()
            .saturating_sub(TOOL_RESULT_PREVIEW_LINES);
        if extra > 0 {
            out.push(Line::from(Span::styled(
                format!("  … (+{extra} more lines)"),
                Style::default().fg(theme.dim),
            )));
        }
    }
}

fn truncate_inline(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let truncated: String = one_line.chars().take(max).collect();
        format!("{truncated}…")
    }
}
