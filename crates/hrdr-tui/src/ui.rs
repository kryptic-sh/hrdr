//! Rendering: transcript + TODO panel + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, Entry};

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Max lines shown in the TODO panel (plus 2 for borders).
const TODO_PANEL_MAX_ITEMS: u16 = 6;

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Snapshot TODO count while briefly holding the lock.
    let todo_count = app.todos.lock().map(|t| t.len()).unwrap_or(0);
    let todo_height = if todo_count > 0 {
        // +2 for borders, cap at TODO_PANEL_MAX_ITEMS visible items.
        (todo_count as u16).min(TODO_PANEL_MAX_ITEMS) + 2
    } else {
        0
    };

    // Build constraints dynamically so the TODO panel appears only when needed.
    let (transcript_area, todo_area, input_area, status_area) = if todo_height > 0 {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(todo_height),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);
        (chunks[0], Some(chunks[1]), chunks[2], chunks[3])
    } else {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);
        (chunks[0], None, chunks[1], chunks[2])
    };

    draw_transcript(f, app, transcript_area);
    if let Some(ta) = todo_area {
        draw_todos(f, app, ta);
    }
    draw_input(f, app, input_area);
    draw_status(f, app, status_area);
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    // Publish the height so key handlers can compute half-page offsets.
    app.transcript_height = area.height;

    let para = Paragraph::new(transcript_lines(app)).wrap(Wrap { trim: false });
    // Count the *wrapped* rows at this width — not the logical line count — so
    // long messages that wrap don't push the newest content below the fold.
    let total = para.line_count(area.width) as u16;
    let max_scroll = total.saturating_sub(area.height);
    // scroll_offset is rows scrolled UP from the bottom; 0 == follow newest.
    // Clamp and write back so "scrolled up" state (and the follow button) is
    // accurate even after the content shrinks.
    let offset = (app.scroll_offset as u16).min(max_scroll);
    app.scroll_offset = offset as usize;
    let scroll = max_scroll.saturating_sub(offset);

    f.render_widget(para.scroll((scroll, 0)), area);
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
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line<'static>> = todos
        .iter()
        .map(|t| {
            let (mark, color) = match t.status.as_str() {
                "completed" => ("x", Color::Green),
                "in_progress" => ("~", Color::Yellow),
                _ => (" ", Color::DarkGray),
            };
            Line::from(Span::styled(
                format!("[{mark}] {}", t.content),
                Style::default().fg(color),
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let mode = app.editor.mode_label();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" input [{mode}] "))
        .border_style(Style::default().fg(Color::DarkGray));
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
                .bg(Color::Red)
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
                .bg(Color::Yellow)
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

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
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
        "{dot} {}{queue_hint}  │  {}  │  {}{scroll_hint}",
        app.status,
        app.model,
        app.editor.keybind_hint(),
    );
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for entry in &app.transcript {
        match entry {
            Entry::User(text) => push_text(
                &mut out,
                Span::styled("❯ ", Style::default().fg(Color::Cyan).bold()),
                text,
                Style::default().fg(Color::Cyan),
            ),
            Entry::Assistant(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default().fg(Color::White),
            ),
            Entry::Reasoning(text) => push_text(
                &mut out,
                Span::styled("· ", Style::default().fg(Color::DarkGray)),
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Entry::Tool {
                name,
                args,
                result,
                ok,
                done,
                ..
            } => push_tool(&mut out, name, args, result, *ok, *done),
            Entry::System(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        }
        out.push(Line::raw(""));
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
    name: &str,
    args: &str,
    result: &str,
    ok: bool,
    done: bool,
) {
    let mark = if !done {
        ("…", Color::Yellow)
    } else if ok {
        ("✓", Color::Green)
    } else {
        ("✗", Color::Red)
    };
    let args_preview = truncate_inline(args, 80);
    out.push(Line::from(vec![
        Span::styled(format!("{} ", mark.0), Style::default().fg(mark.1)),
        Span::styled(name.to_string(), Style::default().fg(Color::Yellow).bold()),
        Span::styled(
            format!(" {args_preview}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if done && !result.is_empty() {
        for line in result.lines().take(TOOL_RESULT_PREVIEW_LINES) {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        let extra = result
            .lines()
            .count()
            .saturating_sub(TOOL_RESULT_PREVIEW_LINES);
        if extra > 0 {
            out.push(Line::from(Span::styled(
                format!("  … (+{extra} more lines)"),
                Style::default().fg(Color::DarkGray),
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
