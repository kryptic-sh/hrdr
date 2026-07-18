//! `hrdr-editor` — the FSM-agnostic editing seam.
//!
//! The rest of hrdr talks only to the [`EditorEngine`] trait — never to vim
//! types. The concrete [`VimEngine`] wraps the hjkl engine and drives it
//! through `hjkl_vim::dispatch_input`. When hjkl's pluggable-FSM work (epic
//! #265) lands a vscode/helix discipline, add a sibling `EditorEngine` impl
//! and the TUI swaps it in with zero churn — it reads `mode_label()` /
//! `is_insert()` (projected from hjkl's FSM-agnostic `CoarseMode`), not
//! `VimMode`.

// Every test in this crate — including one written tomorrow by someone who read none
// of this — runs with `$HOME` and the XDG roots pointed at a throwaway directory. The
// `extern crate` is what links `hrdr-test-support`'s life-before-main ctor into this
// test binary; rustc drops a dependency nothing references, and a dropped ctor is a
// test writing the developer's real sessions. Do not remove it.
#[cfg(test)]
extern crate hrdr_test_support;

mod host;
mod plain;

use hjkl_buffer_tui::{BufferView, Gutter};
use hjkl_engine::{CoarseMode, Editor, Host, Options};
// `buffer_selection`/`vim_mode`/`visual_anchor` moved from inherent `Editor`
// methods into this extension trait (hjkl 0.33.6); bring it into scope to call
// them.
use hjkl_vim::VimEditorExt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthChar;

pub use host::HrdrHost;
pub use plain::PlainEngine;

/// The seam's renderer-agnostic key DTO — hjkl's own toolkit-neutral
/// `Input { key, ctrl, alt, shift }`, re-exported so consumers never touch
/// hjkl types directly.
pub use hjkl_engine::{Input as EditorKey, Key as EditorKeyCode};

/// Convert a crossterm key event to the seam's [`EditorKey`] (the terminal
/// frontend's adapter). `None` for key-release events, which must not reach
/// the engines (terminals reporting them would double keystrokes).
pub fn key_from_crossterm(key: &crossterm::event::KeyEvent) -> Option<EditorKey> {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return None;
    }
    Some(hjkl_engine_tui::crossterm_to_input(*key))
}

/// A pluggable editing discipline — the renderer-agnostic core of the seam.
///
/// Implementors hide their concrete editor/FSM entirely, and nothing here
/// names a UI toolkit: keys arrive as [`EditorKey`]s (each frontend converts
/// its native events) and painting lives behind the separate [`TuiRender`]
/// half (or another render adapter).
pub trait EditorEngine {
    /// Feed a key into the engine.
    fn feed_key(&mut self, key: EditorKey);
    /// Current buffer text.
    fn content(&self) -> String;
    /// Replace the buffer text.
    fn set_content(&mut self, text: &str);
    /// FSM-agnostic uppercase mode label for status chrome (e.g. "NORMAL").
    fn mode_label(&self) -> &'static str;
    /// Whether the engine is in a text-insertion mode (cursor-shape hint).
    fn is_insert(&self) -> bool;
    /// Whether `key`, in the engine's current state, should submit the buffer
    /// as a message rather than be fed to the editor. (e.g. vim: Enter in
    /// Normal mode; plain: Enter without Shift / trailing backslash.)
    fn wants_submit(&self, key: &EditorKey) -> bool;
    /// One-line key hint for the status bar, specific to this discipline.
    fn keybind_hint(&self) -> &'static str;
    /// Desired number of text rows for the input box given the inner `width`,
    /// clamped to `1..=max`. Lets the host auto-grow the input with content.
    /// Default counts logical lines (suits no-wrap engines like vim).
    fn desired_rows(&self, _width: u16, max: u16) -> u16 {
        let lines = self.content().split('\n').count().max(1);
        (lines as u16).clamp(1, max)
    }
    /// Insert pasted (bracketed-paste) text at the cursor. Default feeds each
    /// character as a key event (works for any insert-mode engine); engines may
    /// override for a faster/correct direct insertion.
    fn paste(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\r' {
                continue;
            }
            let key = match c {
                '\n' => EditorKeyCode::Enter,
                '\t' => EditorKeyCode::Tab,
                other => EditorKeyCode::Char(other),
            };
            self.feed_key(EditorKey {
                key,
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
    }
}

/// The terminal renderer half of the seam (ratatui). Kept out of
/// [`EditorEngine`] so the core stays renderer-agnostic — another frontend can
/// host the same engines behind its own render adapter.
pub trait TuiRender {
    /// Draw the editable pane into `area` and place the cursor.
    fn render(&mut self, frame: &mut Frame, area: Rect);
}

/// What the TUI hosts: both halves of the seam.
pub trait TuiEditorEngine: EditorEngine + TuiRender {}
impl<T: EditorEngine + TuiRender> TuiEditorEngine for T {}

/// Display width of one char, terminal-cell accounting: a zero-width
/// combining mark contributes 0 (it rides on the previous cell), most glyphs
/// 1, and wide glyphs (CJK, many emoji) 2. Shared by the wrapping layout and
/// [`plain::PlainEngine`]'s renderer so wrap math and cursor placement agree
/// with what the terminal actually draws — counting chars instead of columns
/// overflows the input box on wide glyphs and misplaces the cursor.
pub(crate) fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Word-wrapping layout — shared by [`wrapped_row_count`] and
// [`plain::PlainEngine::render`] so both count and visual lines agree.
// ---------------------------------------------------------------------------

/// A single visual (wrapped) line: the chars rendered on one terminal row.
#[derive(Debug)]
pub(crate) struct VisualLine {
    pub(crate) chars: Vec<char>,
    pub(crate) width: usize,
}

/// The result of word-wrapping text at a given display width.
///
/// Provides the visual lines for rendering and a mapping from source char
/// index to (row, col) for cursor placement.
#[derive(Debug)]
pub(crate) struct WrappedLayout {
    pub(crate) lines: Vec<VisualLine>,
    /// `positions[i]` = (visual_row, visual_col) where char index `i` sits.
    /// `positions[text.chars().count()]` = position after the last char.
    positions: Vec<(usize, usize)>,
}

impl WrappedLayout {
    /// Total visual lines — at least 1 for an empty buffer.
    pub(crate) fn row_count(&self) -> usize {
        self.lines.len().max(1)
    }

    /// Visual position `(row, col)` for the cursor at `cursor` (a char index
    /// in `0..=text.chars().count()`).
    pub(crate) fn cursor_pos(&self, cursor: usize) -> (usize, usize) {
        self.positions.get(cursor).copied().unwrap_or((0, 0))
    }
}

/// Word-wrap `text` at `width` display columns.
///
/// Wrapping rules (in order):
///
/// 1. **Word wrap** — when a word (contiguous non-whitespace) does not fit
///    on the current visual line but would fit on an empty line, start a new
///    line and place it there. Whitespace remains at the end of the previous
///    line when it fits; whitespace that crosses the edge is omitted visually
///    but remains unchanged in the editable buffer.
///
/// 2. **Hard wrap** — words longer than `width` are split character-by-
///    character across multiple lines, as in the original hard-wrap.
///
/// 3. **Explicit newlines** (`\n`) always force a line break.
///
/// 4. **Unicode width** — wide glyphs (CJK, emoji, width 2) and zero-width
///    combining marks are accounted for identically to `char_width`.
pub(crate) fn compute_wrapped_layout(text: &str, width: usize) -> WrappedLayout {
    let w = width.max(1);
    let src: Vec<char> = text.chars().collect();
    let n = src.len();

    let mut lines: Vec<VisualLine> = vec![VisualLine {
        chars: vec![],
        width: 0,
    }];
    let mut positions = vec![(0usize, 0usize); n + 1];

    let mut i = 0; // byte-iteration equivalent via src index
    while i < n {
        if src[i] == '\n' {
            // Record position for this newline char.
            positions[i] = (lines.len() - 1, lines[lines.len() - 1].width);
            lines.push(VisualLine {
                chars: vec![],
                width: 0,
            });
            i += 1;
            continue;
        }

        // Collect a homogeneous run: word (non-whitespace) or whitespace.
        let run_start = i;
        let is_word = !src[i].is_whitespace();
        while i < n && src[i] != '\n' && is_word == !src[i].is_whitespace() {
            i += 1;
        }
        let run: Vec<char> = src[run_start..i].to_vec();
        let run_width: usize = run.iter().map(|&c| char_width(c)).sum();

        if is_word {
            let cur = lines.len() - 1;
            if lines[cur].width + run_width <= w {
                // Fits on the current line.
                let mut col = lines[cur].width;
                for (j, &c) in run.iter().enumerate() {
                    positions[run_start + j] = (cur, col);
                    lines[cur].chars.push(c);
                    col += char_width(c);
                }
                lines[cur].width = col;
            } else if run_width <= w {
                // Fits on an empty line — wrap the whole word there.
                let new_idx = lines.len();
                lines.push(VisualLine {
                    chars: vec![],
                    width: 0,
                });
                let mut col = 0usize;
                for (j, &c) in run.iter().enumerate() {
                    positions[run_start + j] = (new_idx, col);
                    lines[new_idx].chars.push(c);
                    col += char_width(c);
                }
                lines[new_idx].width = col;
            } else {
                // Word is wider than a whole row — hard-break it.
                let mut cur = lines.len() - 1;
                let mut col = lines[cur].width;
                for (j, &c) in run.iter().enumerate() {
                    let cw = char_width(c);
                    if cw > 0 && col + cw > w {
                        cur = lines.len();
                        lines.push(VisualLine {
                            chars: vec![],
                            width: 0,
                        });
                        col = 0;
                    }
                    positions[run_start + j] = (cur, col);
                    lines[cur].chars.push(c);
                    if cw > 0 {
                        col += cw;
                    }
                }
                lines[cur].width = col;
            }
        } else {
            // Keep whitespace that fits. A run crossing the edge is a soft-wrap
            // boundary: omit it visually while retaining it in the buffer.
            let cur = lines.len() - 1;
            if lines[cur].width + run_width <= w {
                let mut col = lines[cur].width;
                for (j, &c) in run.iter().enumerate() {
                    positions[run_start + j] = (cur, col);
                    lines[cur].chars.push(c);
                    col += char_width(c);
                }
                lines[cur].width = col;
            } else {
                let end = (cur, lines[cur].width);
                for j in 0..run.len() {
                    positions[run_start + j] = end;
                }
            }
        }
    }

    // Position after the very last char.
    positions[n] = (lines.len() - 1, lines[lines.len() - 1].width);

    WrappedLayout { lines, positions }
}

/// Number of display rows `text` occupies when word-wrapped at `width`
/// display columns (not chars).  Delegates to [`compute_wrapped_layout`],
/// guaranteeing that [`PlainEngine::desired_rows`] and `PlainEngine::render`
/// always agree.
pub(crate) fn wrapped_row_count(text: &str, width: u16) -> usize {
    compute_wrapped_layout(text, width.max(1) as usize).row_count()
}

/// The default discipline: hjkl's vim FSM.
pub struct VimEngine {
    editor: Editor<hjkl_buffer::Buffer, HrdrHost>,
}

impl VimEngine {
    pub fn new() -> Self {
        let editor = Editor::new(
            hjkl_buffer::Buffer::new(),
            HrdrHost::new(),
            Options {
                shiftwidth: 4,
                ..Default::default()
            },
        );
        Self { editor }
    }
}

impl Default for VimEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Project hjkl's FSM-agnostic [`CoarseMode`] to a status label. Any future
/// discipline supplies its own projection through the same seam.
fn coarse_label(mode: CoarseMode) -> &'static str {
    match mode {
        CoarseMode::Normal => "NORMAL",
        CoarseMode::Insert => "INSERT",
        CoarseMode::Select => "SELECT",
        CoarseMode::SelectLine => "S-LINE",
        CoarseMode::SelectBlock => "S-BLOCK",
    }
}

impl EditorEngine for VimEngine {
    fn feed_key(&mut self, key: EditorKey) {
        // Release filtering happens in the frontend's key conversion
        // ([`key_from_crossterm`]) — engines only ever see presses.
        hjkl_vim::dispatch_input(&mut self.editor, key);
        self.editor.host_mut().flush_clipboard();
    }

    fn content(&self) -> String {
        self.editor.content()
    }

    fn set_content(&mut self, text: &str) {
        self.editor.set_content(text);
    }

    fn mode_label(&self) -> &'static str {
        coarse_label(self.editor.coarse_mode())
    }

    fn is_insert(&self) -> bool {
        matches!(self.editor.coarse_mode(), CoarseMode::Insert)
    }

    fn wants_submit(&self, key: &EditorKey) -> bool {
        // Vim convention: Enter in Normal mode sends; in Insert it's a newline.
        key.key == EditorKeyCode::Enter
            && !key.ctrl
            && !key.alt
            && !key.shift
            && matches!(self.editor.coarse_mode(), CoarseMode::Normal)
    }

    fn paste(&mut self, text: &str) {
        // The trait default feeds chars as key events, which outside Insert
        // mode would *execute* the pasted text as vim commands (`d`, `x`, `:`…).
        // Insert directly into the buffer instead; in Insert mode the default
        // is fine (and keeps the cursor trailing the paste).
        if self.is_insert() {
            for c in text.chars().filter(|&c| c != '\r') {
                let key = match c {
                    '\n' => EditorKeyCode::Enter,
                    '\t' => EditorKeyCode::Tab,
                    other => EditorKeyCode::Char(other),
                };
                self.feed_key(EditorKey {
                    key,
                    ctrl: false,
                    alt: false,
                    shift: false,
                });
            }
        } else {
            self.editor.insert_str(&text.replace('\r', ""));
        }
    }

    fn keybind_hint(&self) -> &'static str {
        "Esc=normal · Enter(normal)=send · Ctrl+G=$EDITOR · Ctrl+C×2=quit"
    }
}

impl TuiRender for VimEngine {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let editor = &mut self.editor;

        // Publish viewport dimensions into the host (renderer half of the
        // host-owned-viewport contract).
        editor.set_viewport_height(area.height);
        let line_count = editor.row_count().max(1);
        let digits = line_count.to_string().len() as u16;
        let gutter_width = digits + 1;
        {
            let v = editor.host_mut().viewport_mut();
            v.width = area.width;
            v.height = area.height;
            v.text_width = area.width.saturating_sub(gutter_width);
        }

        // Copy out scroll + cursor before taking the immutable render borrows.
        let (cur_row, cur_col) = editor.cursor();
        let top_row = editor.host().viewport().top_row;
        let top_col = editor.host().viewport().top_col;

        // No syntax/search integration in the MVP: spans are empty, so the
        // resolver is never exercised — return a default style.
        let resolver = |_id: u32| Style::default();
        let selection = editor.buffer_selection();
        let gutter = Gutter {
            width: gutter_width,
            ..Default::default()
        };

        let view = BufferView {
            buffer: editor.buffer(),
            viewport: editor.host().viewport(),
            selection,
            resolver: &resolver,
            cursor_line_bg: Style::default(),
            cursor_line_row: None,
            fold_line_bg: Style::default(),
            folds_override: None,
            cursor_column_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default(),
            gutter: Some(gutter),
            search_bg: Style::default(),
            signs: &[],
            conceals: &[],
            spans: editor.buffer_spans(),
            search_pattern: None,
            non_text_style: Style::default(),
            show_eob: true,
            diag_overlays: &[],
            colorcolumn_cols: &[],
            colorcolumn_style: Style::default(),
            listchars: None,
            indent_guides_enabled: false,
            indent_guide_char: '│',
            indent_guide_shiftwidth: 4,
            indent_guide_fg: Color::DarkGray,
            indent_guide_active_fg: Color::Gray,
            indent_guide_active_col: None,
            eol_hints: &[],
            blame_plan: None,
            diff_filler: None,
        };
        frame.render_widget(view, area);

        // Place the terminal cursor (viewport- and gutter-aware).
        let sx = area.x + gutter_width + cur_col.saturating_sub(top_col) as u16;
        let sy = area.y + cur_row.saturating_sub(top_row) as u16;
        if sx < area.x + area.width && sy < area.y + area.height {
            frame.set_cursor_position((sx, sy));
        }
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    // ------------------------------------------------------------------
    // Basic wrapping (applies to both hard-wrap and word-wrap)
    // ------------------------------------------------------------------

    /// Plain ASCII: one row exactly at the boundary, two rows one char past.
    #[test]
    fn ascii_wraps_at_the_column_boundary() {
        assert_eq!(wrapped_row_count("0123456789", 10), 1);
        assert_eq!(wrapped_row_count("01234567890", 10), 2);
    }

    /// Wide glyphs (CJK, width 2) count as 2 columns each.
    #[test]
    fn wide_glyphs_count_as_two_columns() {
        let line: String = std::iter::repeat_n('国', 6).collect();
        assert_eq!(wrapped_row_count(&line, 10), 2);
    }

    /// A wide glyph that would straddle the boundary wraps whole.
    #[test]
    fn a_wide_glyph_does_not_split_across_the_boundary() {
        let line = format!("{}{}", "a".repeat(9), '国');
        assert_eq!(wrapped_row_count(&line, 10), 2);
    }

    /// Zero-width combining marks ride on the previous cell.
    #[test]
    fn zero_width_combining_marks_are_free() {
        let line: String = std::iter::repeat_n("e\u{0301}", 10).collect();
        assert_eq!(wrapped_row_count(&line, 10), 1);
    }

    // ------------------------------------------------------------------
    // Word-wrap specific behavior
    // ------------------------------------------------------------------

    /// Ordinary word wrapping: avoid splitting a word that can move whole.
    #[test]
    fn word_wrap_does_not_split_words() {
        // "hello world" at width 7: "hello" (5) + ws (1) = 6 fits,
        // "world" (5) would make 11 > 7.  Word-wrap puts "world" on row 2.
        assert_eq!(wrapped_row_count("hello world", 7), 2);

        // At width 11 everything fits on one row.
        assert_eq!(wrapped_row_count("hello world", 11), 1);
    }

    /// A word longer than width is still hard-wrapped.
    #[test]
    fn overlong_word_is_hard_wrapped() {
        // "superlongword" at width 6: each chunk of 6 chars wraps.
        assert_eq!(wrapped_row_count("superlongword", 6), 3); // superl ongwor d
    }

    /// Word-wrap + explicit newlines interact correctly.
    #[test]
    fn explicit_newlines_force_breaks() {
        // Two short logical lines each fit their own row.
        assert_eq!(wrapped_row_count("ab\ncd", 10), 2);
        // Long first line wraps, second fits.
        assert_eq!(wrapped_row_count("hello world\nab", 7), 3);
    }

    /// Unicode width with word-wrap: wide chars in words.
    #[test]
    fn word_wrap_wide_glyph() {
        // "hello 世界 world" at width 11:
        // "hello " = 6 fits, "世界" (4 cols) + " world" (6) = overflow.
        // "世" alone is 2 cols; "世界" is 4 cols, fits on empty line.
        assert_eq!(wrapped_row_count("hello 世界 world", 11), 2);
    }

    /// Cursor position mapping at various char indices.
    #[test]
    fn cursor_positions_after_wrapping() {
        let layout = compute_wrapped_layout("hello world", 7);
        // Index 0 ('h') → row 0, col 0
        assert_eq!(layout.cursor_pos(0), (0, 0));
        // Index 5 (space) → row 0, col 5  (end of "hello")
        assert_eq!(layout.cursor_pos(5), (0, 5));
        // Index 6 ('w') → row 1, col 0  (start of wrapped "world")
        assert_eq!(layout.cursor_pos(6), (1, 0));
        // Index 11 (EOF) → row 1, col 5 (end of "world")
        assert_eq!(layout.cursor_pos(11), (1, 5));
    }

    /// Cursor at a soft-wrap boundary (the whitespace that triggers the wrap).
    #[test]
    fn cursor_at_wrap_boundary() {
        // "foo bar" at width 5: "foo" fits, space doesn't, word-wraps.
        let layout = compute_wrapped_layout("foo bar", 5);
        // 'foo' on row 0 (cols 0-2), space at (0,3), 'bar' on row 1 (cols 0-2)
        assert_eq!(layout.cursor_pos(3), (0, 3)); // space
        assert_eq!(layout.cursor_pos(4), (1, 0)); // 'b'
        assert_eq!(layout.cursor_pos(0), (0, 0)); // 'f'
        assert_eq!(layout.cursor_pos(7), (1, 3)); // EOF
    }

    /// desired_rows computed by PlainEngine matches the rendered layout.
    #[test]
    fn desired_rows_matches_rendered_rows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut e = PlainEngine::new();
        e.set_content("hello world again");

        let desired = e.desired_rows(7, 10);
        let layout = compute_wrapped_layout(&e.content(), 7);
        assert_eq!(
            desired as usize,
            layout.row_count(),
            "desired_rows must match layout row_count"
        );

        // Render to a terminal and verify the cursor lands where the layout
        // predicts.  After set_content, the cursor sits at the buffer end.
        let area = Rect::new(0, 0, 7, 10);
        let mut term = Terminal::new(TestBackend::new(7, 10)).unwrap();
        term.draw(|f| e.render(f, area)).unwrap();
        let content_len = e.content().chars().count();
        let (rv, cv) = layout.cursor_pos(content_len);
        assert_eq!(
            term.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(cv as u16, rv as u16),
            "terminal cursor must match layout cursor_pos at EOF"
        );
    }

    /// Word-wrap drops whitespace at break boundaries but never modifies the
    /// original buffer content.
    #[test]
    fn content_is_unchanged_by_wrapping() {
        let text = "hello   world  ";
        let layout = compute_wrapped_layout(text, 5);
        // Two visual rows: "hello" and "world"; the spaces between are dropped
        // from display but the source string is never touched.
        assert_eq!(layout.row_count(), 2);
        assert_eq!(text, "hello   world  ", "source text is untouched");
        assert_eq!(layout.lines[0].chars.iter().collect::<String>(), "hello");
        assert_eq!(layout.lines[1].chars.iter().collect::<String>(), "world");
    }

    /// Empty text produces 1 row.
    #[test]
    fn empty_text() {
        assert_eq!(wrapped_row_count("", 10), 1);
        let layout = compute_wrapped_layout("", 10);
        assert_eq!(layout.row_count(), 1);
        assert_eq!(layout.cursor_pos(0), (0, 0));
    }

    /// Text that is only newlines.
    #[test]
    fn only_newlines() {
        assert_eq!(wrapped_row_count("\n", 10), 2);
        assert_eq!(wrapped_row_count("\n\n", 10), 3);
    }
}
