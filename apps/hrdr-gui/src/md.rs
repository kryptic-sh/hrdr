//! Markdown rendering for the GUI: turn an assistant reply into a floem view
//! tree — styled prose (bold/italic/inline-code/headings/lists/quotes) via
//! `rich_text`, and fenced code blocks syntax-highlighted with syntect on a
//! panel background. Consumes `hjkl_markdown`'s renderer-agnostic event stream
//! (the same one the TUI's ratatui backend uses).

use std::sync::OnceLock;

use floem::peniko::Color;
use floem::text::{Attrs, AttrsList, FamilyOwned, Style as FontStyle, TextLayout, Weight, Wrap};
use floem::views::{Decorators, RichText, container, empty, rich_text, v_stack_from_iter};
use floem::{AnyView, IntoView};
use hjkl_markdown::{Event, parse};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::GuiTheme;

const BODY_SIZE: f32 = 14.0;
const CODE_SIZE: f32 = 13.0;

/// A styled inline text run within a paragraph/heading/list line.
struct Run {
    text: String,
    color: Color,
    bold: bool,
    italic: bool,
    mono: bool,
    size: f32,
}

/// A rendered markdown block.
enum Block {
    /// Inline runs (a paragraph, heading, list item, or blockquote line).
    Rich { runs: Vec<Run>, indent: f32 },
    /// A fenced code block.
    Code { lang: String, content: String },
    /// A thematic break (`---`).
    Rule,
}

/// Render markdown `src` into a vertical stack of block views. The returned
/// view owns its data (`use<>` — it doesn't borrow `src`).
pub fn markdown_view(src: &str, th: GuiTheme) -> impl IntoView + use<> {
    let blocks = to_blocks(src, th);
    v_stack_from_iter(blocks.into_iter().map(move |b| render_block(b, th)))
        .style(|s| s.flex_col().width_full().gap(3.0))
}

/// Parse the event stream into blocks, tracking inline emphasis (pre-computed on
/// each `Text` event), list indentation, and blockquote state.
fn to_blocks(src: &str, th: GuiTheme) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur: Vec<Run> = Vec::new();
    let mut indent = 0.0f32;
    let mut quote = false;

    for ev in parse(src) {
        match ev {
            Event::Text {
                content,
                bold,
                italic,
                strikethrough: _,
                code_span,
            } => {
                let color = if code_span {
                    th.tool
                } else if quote {
                    th.dim
                } else {
                    th.assistant
                };
                cur.push(Run {
                    text: content,
                    color,
                    bold,
                    italic,
                    mono: code_span,
                    size: BODY_SIZE,
                });
            }
            Event::Heading { level, text } => {
                flush(&mut blocks, &mut cur, indent);
                let size = match level {
                    1 => 22.0,
                    2 => 19.0,
                    3 => 17.0,
                    _ => 15.0,
                };
                blocks.push(Block::Rich {
                    runs: vec![Run {
                        text,
                        color: th.user,
                        bold: true,
                        italic: false,
                        mono: false,
                        size,
                    }],
                    indent: 0.0,
                });
            }
            Event::CodeBlock { lang, content } => {
                flush(&mut blocks, &mut cur, indent);
                blocks.push(Block::Code { lang, content });
            }
            Event::Rule => {
                flush(&mut blocks, &mut cur, indent);
                blocks.push(Block::Rule);
            }
            Event::Blank => {
                flush(&mut blocks, &mut cur, indent);
                indent = 0.0;
            }
            Event::ListItem {
                depth,
                bullet,
                number,
                task,
            } => {
                flush(&mut blocks, &mut cur, indent);
                indent = 16.0 * (depth as f32 + 1.0);
                let prefix = match task {
                    Some(true) => "☑ ".to_string(),
                    Some(false) => "☐ ".to_string(),
                    None if bullet == '\0' => format!("{number}. "),
                    None => "• ".to_string(),
                };
                cur.push(Run {
                    text: prefix,
                    color: th.dim,
                    bold: false,
                    italic: false,
                    mono: false,
                    size: BODY_SIZE,
                });
            }
            Event::Link { text, url: _ } => cur.push(Run {
                text,
                color: th.user,
                bold: false,
                italic: false,
                mono: false,
                size: BODY_SIZE,
            }),
            Event::Image { alt, url: _ } => cur.push(Run {
                text: format!("🖼 {alt}"),
                color: th.dim,
                bold: false,
                italic: true,
                mono: false,
                size: BODY_SIZE,
            }),
            Event::BlockQuoteStart => {
                flush(&mut blocks, &mut cur, indent);
                quote = true;
                indent = 16.0;
            }
            Event::BlockQuoteEnd => {
                flush(&mut blocks, &mut cur, indent);
                quote = false;
                indent = 0.0;
            }
            Event::Table { header, rows, .. } => {
                flush(&mut blocks, &mut cur, indent);
                let mut content = header.join("  |  ");
                for r in &rows {
                    content.push('\n');
                    content.push_str(&r.join("  |  "));
                }
                blocks.push(Block::Code {
                    lang: String::new(),
                    content,
                });
            }
            _ => {}
        }
    }
    flush(&mut blocks, &mut cur, indent);
    blocks
}

/// Commit the accumulated inline runs as a `Rich` block (if any).
fn flush(blocks: &mut Vec<Block>, cur: &mut Vec<Run>, indent: f32) {
    if !cur.is_empty() {
        blocks.push(Block::Rich {
            runs: std::mem::take(cur),
            indent,
        });
    }
}

fn render_block(b: Block, th: GuiTheme) -> AnyView {
    match b {
        Block::Rich { runs, indent } => rich_runs(runs)
            .style(move |s| s.margin_left(indent).width_full())
            .into_any(),
        Block::Code { lang, content } => {
            let bg = panel_bg();
            let layout = code_layout(&lang, &content);
            container(rich_text(move || layout.clone()).style(|s| s.width_full()))
                .style(move |s| {
                    s.width_full()
                        .padding(8.0)
                        .margin_vert(2.0)
                        .border_radius(4.0)
                        .background(bg)
                })
                .into_any()
        }
        Block::Rule => empty()
            .style(move |s| {
                s.width_full()
                    .height(1.0)
                    .margin_vert(4.0)
                    .background(th.dim)
            })
            .into_any(),
    }
}

/// Build a `rich_text` view from styled inline runs (per-run color, weight,
/// italic, and monospace for inline code).
fn rich_runs(runs: Vec<Run>) -> RichText {
    let mono = [FamilyOwned::Monospace];
    let mut s = String::new();
    let mut attrs = AttrsList::new(Attrs::new());
    for run in &runs {
        let start = s.len();
        s.push_str(&run.text);
        let end = s.len();
        if end == start {
            continue;
        }
        let mut a = Attrs::new().color(run.color).font_size(run.size);
        if run.bold {
            a = a.weight(Weight::BOLD);
        }
        if run.italic {
            a = a.style(FontStyle::Italic);
        }
        if run.mono {
            a = a.family(&mono);
        }
        attrs.add_span(start..end, a);
    }
    let mut layout = TextLayout::new();
    layout.set_text(&s, attrs);
    layout.set_wrap(Wrap::Word);
    rich_text(move || layout.clone())
}

/// Syntax-highlight a code block into a `TextLayout` of monospace colored runs.
fn code_layout(lang: &str, content: &str) -> TextLayout {
    let mono = [FamilyOwned::Monospace];
    let ss = syntax_set();
    let theme = syntect_theme();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_first_line(content))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme);
    let base = Attrs::new().family(&mono).font_size(CODE_SIZE);
    let mut s = String::new();
    let mut attrs = AttrsList::new(base);
    for line in LinesWithEndings::from(content) {
        let ranges = hl.highlight_line(line, ss).unwrap_or_default();
        for (style, piece) in ranges {
            let start = s.len();
            s.push_str(piece);
            let end = s.len();
            if end == start {
                continue;
            }
            let fg = Color::rgb8(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            );
            attrs.add_span(
                start..end,
                Attrs::new().color(fg).family(&mono).font_size(CODE_SIZE),
            );
        }
    }
    let mut layout = TextLayout::new();
    layout.set_text(&s, attrs);
    layout.set_wrap(Wrap::Word);
    layout
}

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

/// Background for code blocks: the syntect theme's background, dark fallback.
fn panel_bg() -> Color {
    syntect_theme()
        .settings
        .background
        .map(|c| Color::rgb8(c.r, c.g, c.b))
        .unwrap_or(Color::rgb8(30, 32, 40))
}
