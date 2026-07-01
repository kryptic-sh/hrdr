//! Markdown rendering for the GUI: turn an assistant reply into a floem view
//! tree — styled prose (bold/italic/inline-code/headings/lists/quotes) via
//! `rich_text`, and fenced code blocks syntax-highlighted with syntect on a
//! panel background. Consumes `hjkl_markdown`'s renderer-agnostic event stream
//! (the same one the TUI's ratatui backend uses).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use floem::peniko::Color;
use floem::prelude::SignalGet;
use floem::reactive::RwSignal;
use floem::text::{Attrs, AttrsList, FamilyOwned, Style as FontStyle, TextLayout, Weight, Wrap};
use floem::views::{Decorators, RichText, container, dyn_stack, empty, rich_text};
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

/// A block plus a stable identity key, so a `dyn_stack` only re-renders blocks
/// whose content changed. During streaming, earlier blocks keep their key (and
/// their already-rendered view — no re-highlight), so only the growing tail
/// block is rebuilt each token instead of the whole reply.
struct KeyedBlock {
    key: String,
    block: Block,
}

/// Render an assistant reply (the `text` signal) as markdown, re-parsing on each
/// change but re-rendering only the blocks that actually changed (keyed by
/// content hash). This keeps streaming cheap: syntect only re-highlights the
/// still-growing code block, not every prior one.
pub fn markdown_stack(text: RwSignal<String>, th: GuiTheme) -> impl IntoView {
    dyn_stack(
        move || keyed_blocks(&text.get(), th),
        |kb: &KeyedBlock| kb.key.clone(),
        move |kb| render_block(kb.block, th),
    )
    .style(|s| s.flex_col().width_full().gap(3.0))
}

/// Parse `src` into blocks, each tagged with an index + content-hash key.
fn keyed_blocks(src: &str, th: GuiTheme) -> Vec<KeyedBlock> {
    to_blocks(src, th)
        .into_iter()
        .enumerate()
        .map(|(i, block)| KeyedBlock {
            key: block_key(i, &block),
            block,
        })
        .collect()
}

/// A stable key for a block: its index plus a hash of its rendered content, so an
/// unchanged block keeps its key across re-parses (colors derive from the fixed
/// theme, so hashing text + emphasis flags is enough).
fn block_key(i: usize, b: &Block) -> String {
    let mut h = DefaultHasher::new();
    match b {
        Block::Rich { runs, indent } => {
            for r in runs {
                r.text.hash(&mut h);
                r.bold.hash(&mut h);
                r.italic.hash(&mut h);
                r.mono.hash(&mut h);
                r.size.to_bits().hash(&mut h);
            }
            indent.to_bits().hash(&mut h);
        }
        Block::Code { lang, content } => {
            lang.hash(&mut h);
            content.hash(&mut h);
        }
        Block::Rule => 0u8.hash(&mut h),
    }
    format!("{i}:{:x}", h.finish())
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
            let fg = Color::rgb8(style.foreground.r, style.foreground.g, style.foreground.b);
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
