//! Shared syntect resources for code highlighting. Fenced code blocks (and the
//! tint on tool-output panels) resolve against one syntax set, theme, and panel
//! background, loaded once here rather than per call site. The actual
//! span→color rendering stays with the renderer (the TUI maps them onto
//! ratatui `Line`s).
//!
//! [`HighlightCache`] adds *incremental* highlighting for streaming blocks: a
//! code block that only grows (tokens appending during a turn) re-highlights
//! just its new lines instead of the whole block on every frame/update.

use std::sync::OnceLock;

use syntect::highlighting::{
    HighlightIterator, HighlightState, Highlighter, Style as SyntectStyle, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

/// The bundled syntax set, deserialized once.
pub fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The highlight theme (base16-ocean.dark, falling back to any bundled theme).
pub fn syntect_theme() -> &'static Theme {
    static TH: OnceLock<Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        ts.themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .expect("syntect ships default themes")
    })
}

/// Panel background RGB for code blocks and tool output: the syntect theme's
/// background with a dark fallback. Frontends convert to their color type.
pub fn panel_bg_rgb() -> (u8, u8, u8) {
    syntect_theme()
        .settings
        .background
        .map(|c| (c.r, c.g, c.b))
        .unwrap_or((30, 32, 40))
}

/// One highlighted line: `(style, text)` runs. Text keeps syntect's trailing
/// newline (frontends trim as their renderer needs).
pub type HlLine = Vec<(SyntectStyle, String)>;

/// Highlighter state for one code block: everything needed to resume
/// highlighting where the last call left off.
struct BlockState {
    lang: String,
    /// The complete-line prefix already highlighted (ends with `\n` or empty).
    consumed: String,
    parse: ParseState,
    hstate: HighlightState,
    /// Highlighted spans, one entry per consumed line.
    lines: Vec<HlLine>,
    /// LRU stamp.
    tick: u64,
}

/// Incremental code-block highlighter with a small LRU of in-progress blocks.
///
/// [`highlight`](Self::highlight) matches the request against a cached block
/// whose already-highlighted content is a *prefix* of the new content (the
/// shape of a streaming block: it only grows), highlights just the new
/// complete lines (committing parser state), and highlights the partial tail
/// line on cloned state so the next append can redo it. A finished block hits
/// the prefix-equality fast path on subsequent calls and does no syntect work.
pub struct HighlightCache {
    highlighter: Highlighter<'static>,
    blocks: Vec<BlockState>,
    tick: u64,
}

/// In-progress + recently finished blocks kept per cache (per UI thread).
const HL_CACHE_BLOCKS: usize = 64;

impl Default for HighlightCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HighlightCache {
    pub fn new() -> Self {
        Self {
            highlighter: Highlighter::new(syntect_theme()),
            blocks: Vec::new(),
            tick: 0,
        }
    }

    /// [`Self::highlight_each`], collected: an owned copy of every line.
    pub fn highlight(&mut self, lang: &str, content: &str) -> Vec<HlLine> {
        let mut out = Vec::new();
        self.highlight_each(lang, content, |line| out.push(line.clone()));
        out
    }

    /// Highlight a fenced block's body, reusing all work from a previous call
    /// whose content is a prefix of `content`, and hand `each` the spans of
    /// every line in order — including the partial (unterminated) tail line.
    ///
    /// Lines are lent, not returned: a streaming block is highlighted again on
    /// every frame, and its committed lines are already cached here, so copying
    /// them out would double the work of a caller that builds its own rows.
    pub fn highlight_each(&mut self, lang: &str, content: &str, mut each: impl FnMut(&HlLine)) {
        self.tick += 1;
        let ss = syntax_set();
        let mut block = match self
            .blocks
            .iter()
            .position(|b| b.lang == lang && content.starts_with(&b.consumed))
        {
            Some(i) => self.blocks.swap_remove(i),
            None => {
                let syntax = ss
                    .find_syntax_by_token(lang)
                    .or_else(|| ss.find_syntax_by_first_line(content))
                    .unwrap_or_else(|| ss.find_syntax_plain_text());
                BlockState {
                    lang: lang.to_string(),
                    consumed: String::new(),
                    parse: ParseState::new(syntax),
                    hstate: HighlightState::new(&self.highlighter, ScopeStack::new()),
                    lines: Vec::new(),
                    tick: 0,
                }
            }
        };

        // Split what's new into complete lines (committed) + a partial tail.
        let rest = &content[block.consumed.len()..];
        let complete_end = rest.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let (complete, partial) = rest.split_at(complete_end);
        for line in LinesWithEndings::from(complete) {
            let ops = block.parse.parse_line(line, ss).unwrap_or_default();
            let spans: HlLine =
                HighlightIterator::new(&mut block.hstate, &ops, line, &self.highlighter)
                    .map(|(st, t)| (st, t.to_string()))
                    .collect();
            block.lines.push(spans);
        }
        block.consumed.push_str(complete);

        block.lines.iter().for_each(&mut each);
        if !partial.is_empty() {
            // Highlight the tail on cloned state — the next append re-does it.
            let mut parse = block.parse.clone();
            let mut hstate = block.hstate.clone();
            let ops = parse.parse_line(partial, ss).unwrap_or_default();
            let spans: HlLine =
                HighlightIterator::new(&mut hstate, &ops, partial, &self.highlighter)
                    .map(|(st, t)| (st, t.to_string()))
                    .collect();
            each(&spans);
        }

        block.tick = self.tick;
        self.blocks.push(block);
        if self.blocks.len() > HL_CACHE_BLOCKS {
            // Drop the least-recently-used block.
            if let Some(i) = self
                .blocks
                .iter()
                .enumerate()
                .min_by_key(|(_, b)| b.tick)
                .map(|(i, _)| i)
            {
                self.blocks.swap_remove(i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[HlLine]) -> String {
        lines
            .iter()
            .flat_map(|l| l.iter().map(|(_, t)| t.as_str()))
            .collect()
    }

    #[test]
    fn incremental_matches_streaming_appends() {
        let mut cache = HighlightCache::new();
        // Streamed in three appends, the last leaving a partial line.
        let a = "fn main() {\n";
        let b = "fn main() {\n    let x = 1;\n";
        let c = "fn main() {\n    let x = 1;\n    println!(\"{x}\")";
        assert_eq!(text(&cache.highlight("rust", a)), a);
        assert_eq!(text(&cache.highlight("rust", b)), b);
        let inc = cache.highlight("rust", c);
        assert_eq!(text(&inc), c);
        // A fresh cache highlighting the full content in one shot must agree
        // (styles and split) with the incremental path.
        let full = HighlightCache::new().highlight("rust", c);
        assert_eq!(inc.len(), full.len());
        for (li, fl) in inc.iter().zip(full.iter()) {
            assert_eq!(li, fl);
        }
        // Re-requesting identical content is a pure cache hit (same result).
        assert_eq!(cache.highlight("rust", c), inc);
    }
}
