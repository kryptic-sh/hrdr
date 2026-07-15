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
/// 1, and wide glyphs (CJK, many emoji) 2. Shared by [`wrapped_row_count`] and
/// [`plain::PlainEngine`]'s renderer so wrap math and cursor placement agree
/// with what the terminal actually draws — counting chars instead of columns
/// overflows the input box on wide glyphs and misplaces the cursor.
pub(crate) fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Number of display rows `text` occupies when hard-wrapped at `width` display
/// columns (not chars). A wide glyph that would straddle the boundary wraps
/// whole onto the next row, matching the terminal's own behavior, and
/// zero-width marks never trigger a wrap.
pub(crate) fn wrapped_row_count(text: &str, width: u16) -> usize {
    let w = width.max(1) as usize;
    text.split('\n')
        .map(|line| {
            let mut rows = 1usize;
            let mut col = 0usize;
            for c in line.chars() {
                let cw = char_width(c);
                if cw == 0 {
                    continue;
                }
                if col + cw > w {
                    rows += 1;
                    col = 0;
                }
                col += cw;
            }
            rows
        })
        .sum::<usize>()
        .max(1)
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
    use super::wrapped_row_count;

    /// Plain ASCII: unchanged behavior from the char-counting version — one
    /// row exactly at the boundary, two rows one char past it.
    #[test]
    fn ascii_wraps_at_the_column_boundary() {
        assert_eq!(wrapped_row_count("0123456789", 10), 1);
        assert_eq!(wrapped_row_count("01234567890", 10), 2);
    }

    /// Wide glyphs (CJK, width 2) count as 2 columns each, so half as many fit
    /// per row as ASCII — the char-counting bug undercounted rows by 2x here.
    #[test]
    fn wide_glyphs_count_as_two_columns() {
        // 6 wide chars = 12 columns; at width 10 that's 2 rows (a 7th char
        // would overflow the 5th slot, so only 5 fit on the first row).
        let line: String = std::iter::repeat_n('国', 6).collect();
        assert_eq!(wrapped_row_count(&line, 10), 2);
    }

    /// A wide glyph that would straddle the boundary wraps whole onto the next
    /// row rather than splitting across it.
    #[test]
    fn a_wide_glyph_does_not_split_across_the_boundary() {
        // 9 narrow chars fill columns 0..9, leaving 1 free column — too
        // narrow for the wide glyph that follows, so it wraps whole.
        let line = format!("{}{}", "a".repeat(9), '国');
        assert_eq!(wrapped_row_count(&line, 10), 2);
    }

    /// Zero-width combining marks ride on the previous cell — they never
    /// advance the column count or trigger a wrap.
    #[test]
    fn zero_width_combining_marks_are_free() {
        // "e" + combining acute accent (U+0301), repeated to fill a row.
        let line: String = std::iter::repeat_n("e\u{0301}", 10).collect();
        assert_eq!(wrapped_row_count(&line, 10), 1);
    }
}
