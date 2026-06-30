//! Rendering: transcript + TODO panel + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};

use crate::app::{App, Entry, TimestampStyle};
use crate::theme::Theme;

const TOOL_RESULT_PREVIEW_LINES: usize = 8;
/// Diff results (edit/write_file) get a larger preview since the diff is the point.
const DIFF_PREVIEW_LINES: usize = 40;
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

    let para = Paragraph::new(transcript_lines(app, text_area.width)).wrap(Wrap { trim: false });
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

    let text = if app.compacting {
        format!(
            " {frame} compacting context — summarizing the conversation…  ·  {:.1}s",
            elapsed.as_secs_f64(),
        )
    } else {
        let phase = if app.first_token_at.is_some() {
            "generating"
        } else {
            "inferring"
        };
        format!(
            " {frame} {phase}  ·  {ctx}  ·  {speed:.1} tok/s ({} out)  ·  {:.1}s",
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
    let sep = || Span::styled(" │ ", Style::default().fg(t.dim));
    let mut spans: Vec<Span> = Vec::new();

    // Show just the cwd's basename, not the full path, with a folder icon
    // (Nerd-font glyphs only when the icon mode allows them).
    let nerd = app.icon_mode == hjkl_icons::IconMode::Nerd;
    let folder = if nerd { "\u{f07b} " } else { "" };
    let branch_icon = if nerd { "\u{e0a0} " } else { "" };
    let dir_label = app
        .dir
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&app.dir);
    spans.push(Span::styled(
        format!(" {folder}{dir_label}"),
        Style::default().fg(t.user),
    ));
    if let Some(branch) = &app.branch {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{branch_icon}{branch}"),
            Style::default().fg(t.success),
        ));
    }
    spans.push(sep());
    spans.push(Span::styled(
        format!("↑{}", fmt_count(app.session_in)),
        Style::default().fg(t.accent),
    ));
    spans.push(sep());
    spans.push(Span::styled(
        format!("↓{}", fmt_count(app.session_out)),
        Style::default().fg(t.accent2),
    ));
    spans.push(sep());
    let ctx = app.last_usage.map(|(p, _)| p as usize).unwrap_or(0);
    match app.context_window {
        Some(w) if w > 0 => {
            // Render the section as a used/free bar: the label gets a filled
            // background on its left portion (used) and a track background on
            // the right (free), split proportionally to context usage.
            let frac = (ctx as f64 / w as f64).clamp(0.0, 1.0);
            let pct = (frac * 100.0).round() as u32;
            // Fill color escalates with usage: green → amber → red at the
            // auto-compact threshold (where compaction kicks in next turn).
            let fill_color = if app.auto_compact_ratio > 0.0 && frac >= app.auto_compact_ratio {
                t.error
            } else if frac >= 0.70 {
                t.warn
            } else {
                t.success
            };
            let label = format!(
                " {} of {} ctx ({pct}%) ",
                fmt_count(ctx),
                fmt_count(w as usize)
            );
            let chars: Vec<char> = label.chars().collect();
            let fill = ((frac * chars.len() as f64).round() as usize).min(chars.len());
            let used: String = chars[..fill].iter().collect();
            let free: String = chars[fill..].iter().collect();
            if !used.is_empty() {
                spans.push(Span::styled(
                    used,
                    Style::default()
                        .fg(Color::Black)
                        .bg(fill_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if !free.is_empty() {
                spans.push(Span::styled(
                    free,
                    Style::default().fg(t.assistant).bg(t.dim),
                ));
            }
        }
        _ => spans.push(Span::styled(
            format!(" {} ctx ", fmt_count(ctx)),
            Style::default().fg(t.warn),
        )),
    }
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

/// Human-friendly elapsed time since `then` (e.g. `now`, `2m ago`, `3h ago`).
fn relative_time(then: chrono::DateTime<chrono::Local>) -> String {
    let secs = (chrono::Local::now() - then).num_seconds().max(0);
    if secs < 5 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
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

fn transcript_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let md_theme = theme.md_theme();
    let mut out: Vec<Line> = Vec::new();
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
        match entry {
            Entry::User(text) => {
                msg_num += 1;
                meta(&mut out, i, msg_num, "you");
                push_text(
                    &mut out,
                    Span::styled("❯ ", Style::default().fg(theme.user).bold()),
                    text,
                    Style::default().fg(theme.user),
                );
            }
            // Assistant text is rendered as markdown (headings, lists, emphasis,
            // inline/code spans), themed from the active hjkl theme.
            Entry::Assistant(text) => {
                msg_num += 1;
                meta(&mut out, i, msg_num, "assistant");
                let events = hjkl_markdown::parse(text);
                out.extend(hjkl_markdown_tui::to_lines(
                    &events,
                    &md_theme,
                    width.max(1),
                ));
            }
            Entry::Reasoning(_) if !app.show_reasoning => continue, // hidden via /reasoning
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
            Entry::Diff(text) => {
                for line in text.lines() {
                    out.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(diff_line_color(line, theme)),
                    )));
                }
            }
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
        // edit/write_file return a unified diff — color it and show more lines.
        let is_diff = matches!(name, "edit" | "write_file");
        let preview = if is_diff {
            DIFF_PREVIEW_LINES
        } else {
            TOOL_RESULT_PREVIEW_LINES
        };
        for line in result.lines().take(preview) {
            let color = if is_diff {
                diff_line_color(line, theme)
            } else {
                theme.dim
            };
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(color),
            )));
        }
        let extra = result.lines().count().saturating_sub(preview);
        if extra > 0 {
            out.push(Line::from(Span::styled(
                format!("  … (+{extra} more lines)"),
                Style::default().fg(theme.dim),
            )));
        }
    }
}

/// Color for one unified-diff line: additions green, deletions red, hunk
/// headers in the accent color, file headers and context dim.
fn diff_line_color(line: &str, theme: &Theme) -> Color {
    if line.starts_with("+++") || line.starts_with("---") {
        theme.dim
    } else if line.starts_with('@') {
        theme.user
    } else if line.starts_with('+') {
        theme.success
    } else if line.starts_with('-') {
        theme.error
    } else {
        theme.dim
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
