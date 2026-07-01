//! Rendering: transcript + TODO panel + vim input pane + status line.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::{App, Entry, StatusBarMode, TimestampStyle};
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
    if let Some(i) = todo_idx {
        draw_todos(f, app, chunks[i]);
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

    let (lines, msg_starts) = transcript_lines(app, text_area.width);
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
        let phase = if app.first_token_at.is_some() {
            "generating"
        } else {
            "inferring"
        };
        format!(
            " {frame} {phase}  ·  {ctx}  ·  {speed:.1} tok/s ({} out)  ·  {:.1}s{started}",
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

/// One status-bar section: `(priority, spans)`. Lower priority is kept longer;
/// higher is dropped first in truncate mode.
type StatusSection = (u8, Vec<Span<'static>>);

fn status_section_width(s: &StatusSection) -> usize {
    s.1.iter().map(Span::width).sum()
}

/// Build the status-bar sections (cwd, branch, in/out tokens, context, model,
/// effort) in display order.
fn build_status_sections(app: &App) -> Vec<StatusSection> {
    let t = &app.theme;
    let mut sections: Vec<StatusSection> = Vec::new();

    // cwd basename + folder icon (Nerd glyphs only when the icon mode allows).
    let nerd = app.icon_mode == hjkl_icons::IconMode::Nerd;
    let folder = if nerd { "\u{f07b} " } else { "" };
    let branch_icon = if nerd { "\u{e0a0} " } else { "" };
    let dir_label = app
        .dir
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&app.dir);
    sections.push((
        0,
        vec![Span::styled(
            format!(" {folder}{dir_label}"),
            Style::default().fg(t.user),
        )],
    ));
    if let Some(branch) = &app.branch {
        sections.push((
            3,
            vec![Span::styled(
                format!("{branch_icon}{branch}"),
                Style::default().fg(t.success),
            )],
        ));
    }
    sections.push((
        4,
        vec![Span::styled(
            format!("↑{}", fmt_count(app.session_in)),
            Style::default().fg(t.accent),
        )],
    ));
    sections.push((
        4,
        vec![Span::styled(
            format!("↓{}", fmt_count(app.session_out)),
            Style::default().fg(t.accent2),
        )],
    ));
    // Context: a used/free bar when the window is known, else a plain count.
    let ctx = app.last_usage.map(|(p, _)| p as usize).unwrap_or(0);
    let ctx_spans = match app.context_window {
        Some(w) if w > 0 => {
            let frac = (ctx as f64 / w as f64).clamp(0.0, 1.0);
            // Fill color escalates with usage: green → amber → red at the
            // auto-compact threshold (where compaction kicks in next turn).
            let fill_color = if app.auto_compact_ratio > 0.0 && frac >= app.auto_compact_ratio {
                t.error
            } else if frac >= 0.70 {
                t.warn
            } else {
                t.success
            };
            let label = format!(" {} of {} ctx ", fmt_count(ctx), fmt_count(w as usize));
            let chars: Vec<char> = label.chars().collect();
            let fill = ((frac * chars.len() as f64).round() as usize).min(chars.len());
            let used: String = chars[..fill].iter().collect();
            let free: String = chars[fill..].iter().collect();
            let mut s = Vec::new();
            if !used.is_empty() {
                s.push(Span::styled(
                    used,
                    Style::default()
                        .fg(Color::Black)
                        .bg(fill_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if !free.is_empty() {
                s.push(Span::styled(
                    free,
                    Style::default().fg(t.assistant).bg(t.dim),
                ));
            }
            s
        }
        _ => vec![Span::styled(
            format!(" {} ctx ", fmt_count(ctx)),
            Style::default().fg(t.warn),
        )],
    };
    sections.push((1, ctx_spans));
    sections.push((
        2,
        vec![Span::styled(
            app.model.clone(),
            Style::default().fg(t.assistant),
        )],
    ));
    if let Some(effort) = &app.effort {
        sections.push((
            5,
            vec![Span::styled(effort.clone(), Style::default().fg(t.warn))],
        ));
    }
    sections
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

// syntect resources are loaded once (deserialized from the bundled dumps).
fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntect_theme() -> &'static syntect::highlighting::Theme {
    static TH: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        ts.themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .expect("syntect ships default themes")
    })
}

thread_local! {
    // Cache highlighted code blocks (keyed by lang+content+width) so the ~8/sec
    // redraw doesn't re-run syntect every frame.
    static HL_CACHE: RefCell<HashMap<u64, Vec<Line<'static>>>> = RefCell::new(HashMap::new());
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

fn render_code_block(lang: &str, content: &str, width: u16) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let theme = syntect_theme();
    let bg = theme
        .settings
        .background
        .map(|c| Color::Rgb(c.r, c.g, c.b))
        .unwrap_or(Color::Rgb(30, 32, 40));
    let bg_only = Style::default().bg(bg);
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_first_line(content))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme);
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

    for line in LinesWithEndings::from(content) {
        let ranges = hl.highlight_line(line, ss).unwrap_or_default();
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

/// Human-friendly elapsed time since `then`, with compound units for the larger
/// ranges (`now`, `42s ago`, `5m ago`, `1h30m ago`, `2d3h ago`).
fn relative_time(then: chrono::DateTime<chrono::Local>) -> String {
    let secs = (chrono::Local::now() - then).num_seconds().max(0);
    if secs < 5 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m > 0 {
            format!("{h}h{m}m ago")
        } else {
            format!("{h}h ago")
        }
    } else {
        let (d, h) = (secs / 86_400, (secs % 86_400) / 3600);
        if h > 0 {
            format!("{d}d{h}h ago")
        } else {
            format!("{d}d ago")
        }
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

/// Returns the rendered transcript lines plus, for each user/assistant message
/// (1-based), the logical-line index where it starts (for `/goto`).
fn transcript_lines(app: &App, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
    let theme = &app.theme;
    let md_theme = theme.md_theme();
    let mut out: Vec<Line> = Vec::new();
    let mut msg_starts: Vec<usize> = Vec::new();
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
                msg_starts.push(out.len());
                meta(&mut out, i, msg_num, "you");
                push_text(
                    &mut out,
                    Span::styled("❯ ", Style::default().fg(theme.user).bold()),
                    text,
                    Style::default().fg(theme.user),
                );
            }
            // Assistant text is rendered as markdown (headings, lists, emphasis,
            // inline/code spans) via hjkl-markdown; fenced code blocks are pulled
            // out and syntax-highlighted with syntect on a distinct background.
            Entry::Assistant(text) => {
                msg_num += 1;
                msg_starts.push(out.len());
                meta(&mut out, i, msg_num, "assistant");
                let mut buf: Vec<hjkl_markdown::Event> = Vec::new();
                for ev in hjkl_markdown::parse(text) {
                    if let hjkl_markdown::Event::CodeBlock { lang, content } = ev {
                        if !buf.is_empty() {
                            out.extend(hjkl_markdown_tui::to_lines(&buf, &md_theme, width.max(1)));
                            buf.clear();
                        }
                        out.extend(highlight_code_block(&lang, &content, width.max(1)));
                    } else {
                        buf.push(ev);
                    }
                }
                if !buf.is_empty() {
                    out.extend(hjkl_markdown_tui::to_lines(&buf, &md_theme, width.max(1)));
                }
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
                expanded,
                ..
            } => push_tool(
                &mut out,
                theme,
                name,
                args,
                result,
                *ok,
                *done,
                *expanded || app.expand_tools,
            ),
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

    // Highlight the active /find query across the committed transcript.
    if let Some(needle) = app
        .find_query
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

    (out, msg_starts)
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
    if result.is_empty() {
        return;
    }
    // edit/write_file return a unified diff — color it and show more lines.
    let is_diff = matches!(name, "edit" | "write_file");
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
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(color),
            )));
        }
        let extra = lines.len().saturating_sub(shown);
        if extra > 0 {
            out.push(Line::from(Span::styled(
                format!("  … (+{extra} more lines · /expand)"),
                Style::default().fg(theme.dim),
            )));
        }
    } else {
        // Still running: show the live tail so the newest output is visible.
        let start = lines.len().saturating_sub(preview);
        if start > 0 {
            out.push(Line::from(Span::styled(
                format!("  ⋮ (live · {start} earlier line(s))"),
                Style::default().fg(theme.dim),
            )));
        }
        for line in &lines[start..] {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
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
