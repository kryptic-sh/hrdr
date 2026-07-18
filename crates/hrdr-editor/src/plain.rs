//! `PlainEngine` — a simple, claude-style text input discipline.
//!
//! Always in "insert": typing inserts, `Enter` sends (handled by the host via
//! [`EditorEngine::wants_submit`]). Newlines come from `Shift+Enter` or a
//! trailing backslash before `Enter` (`\` + `Enter`). Readline-style cursor
//! keys: `Ctrl+A`/`Ctrl+E` line start/end, `Ctrl+W` delete word, `Ctrl+U` kill
//! to line start. Heavy edits go through `$EDITOR` (the host's `Ctrl+G`).
//!
//! This is a second [`EditorEngine`] discipline alongside [`crate::VimEngine`],
//! proving the FSM-agnostic seam: the TUI swaps engines without changing.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::{EditorEngine, wrapped_row_count};

/// A plain UTF-8 text buffer with a char-index cursor.
#[derive(Default)]
pub struct PlainEngine {
    chars: Vec<char>,
    /// Cursor position as an index into `chars`, in `0..=chars.len()`.
    cursor: usize,
}

impl PlainEngine {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    fn line_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end(&self) -> usize {
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i] != '\n' {
            i += 1;
        }
        i
    }

    fn home(&mut self) {
        self.cursor = self.line_start();
    }

    fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn up(&mut self) {
        let ls = self.line_start();
        if ls == 0 {
            self.cursor = 0;
            return;
        }
        let col = self.cursor - ls;
        let prev_end = ls - 1; // the '\n' terminating the previous line
        let mut prev_start = prev_end;
        while prev_start > 0 && self.chars[prev_start - 1] != '\n' {
            prev_start -= 1;
        }
        let prev_len = prev_end - prev_start;
        self.cursor = prev_start + col.min(prev_len);
    }

    fn down(&mut self) {
        let le = self.line_end();
        if le == self.chars.len() {
            self.cursor = self.chars.len();
            return;
        }
        let col = self.cursor - self.line_start();
        let next_start = le + 1;
        let mut next_end = next_start;
        while next_end < self.chars.len() && self.chars[next_end] != '\n' {
            next_end += 1;
        }
        let next_len = next_end - next_start;
        self.cursor = next_start + col.min(next_len);
    }

    fn delete_word(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
    }

    fn kill_to_line_start(&mut self) {
        let start = self.line_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// True when the char immediately before the cursor is a backslash — the
    /// "`\` + Enter = newline" escape.
    fn pending_backslash(&self) -> bool {
        self.cursor > 0 && self.chars[self.cursor - 1] == '\\'
    }
}

impl EditorEngine for PlainEngine {
    fn feed_key(&mut self, key: crate::EditorKey) {
        use crate::EditorKeyCode as K;
        match key.key {
            K::Char(c) if key.ctrl => match c {
                'a' => self.home(),
                'e' => self.end(),
                'w' => self.delete_word(),
                'u' => self.kill_to_line_start(),
                _ => {}
            },
            K::Char(c) => self.insert(c),
            K::Enter => {
                // Reached only when this Enter is NOT a submit (Shift+Enter, or
                // `\`+Enter). Strip the escape backslash if present.
                if self.pending_backslash() {
                    self.backspace();
                }
                self.insert('\n');
            }
            K::Backspace => self.backspace(),
            K::Delete => self.delete(),
            K::Left => self.left(),
            K::Right => self.right(),
            K::Up => self.up(),
            K::Down => self.down(),
            K::Home => self.home(),
            K::End => self.end(),
            _ => {}
        }
    }

    fn content(&self) -> String {
        self.chars.iter().collect()
    }

    fn set_content(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    fn mode_label(&self) -> &'static str {
        "TEXT"
    }

    fn is_insert(&self) -> bool {
        true
    }

    fn wants_submit(&self, key: &crate::EditorKey) -> bool {
        // Enter sends — UNLESS it's a newline gesture: Shift+Enter (only on
        // terminals that report it), Alt+Enter (reported by far more
        // terminals), or a trailing backslash (`\`+Enter, works everywhere).
        key.key == crate::EditorKeyCode::Enter
            && !key.shift
            && !key.alt
            && !self.pending_backslash()
    }

    fn keybind_hint(&self) -> &'static str {
        "Enter=send · Alt/Shift+Enter or \\+Enter=newline · Ctrl+G=$EDITOR · Ctrl+C×2=quit"
    }

    fn desired_rows(&self, width: u16, max: u16) -> u16 {
        wrapped_row_count(&self.content(), width)
            .clamp(1, max as usize) as u16
    }

    fn paste(&mut self, text: &str) {
        for c in text.chars() {
            if c != '\r' {
                self.insert(c);
            }
        }
    }
}

impl crate::TuiRender for PlainEngine {
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.max(1) as usize;
        let layout = crate::compute_wrapped_layout(&self.content(), width);

        // Vertically scroll so the cursor row stays visible when the content
        // is taller than the (capped) box.
        let (crow, ccol) = layout.cursor_pos(self.cursor);
        let height = area.height.max(1) as usize;
        let top = crow.saturating_sub(height - 1);
        let visible: Vec<Line> = layout
            .lines
            .iter()
            .skip(top)
            .take(height)
            .map(|vl| Line::from(vl.chars.iter().collect::<String>()))
            .collect();
        frame.render_widget(Paragraph::new(visible), area);

        let sx = (ccol as u16).min(area.width.saturating_sub(1));
        let sy = (crow - top) as u16;
        frame.set_cursor_position((area.x + sx, area.y + sy));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditorKey, EditorKeyCode};

    fn key(code: EditorKeyCode) -> EditorKey {
        EditorKey {
            key: code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }
    fn k(c: char) -> EditorKey {
        key(EditorKeyCode::Char(c))
    }
    fn ctrl(c: char) -> EditorKey {
        EditorKey { ctrl: true, ..k(c) }
    }
    /// Enter with the given (shift, alt) modifier pair.
    fn enter(shift: bool, alt: bool) -> EditorKey {
        EditorKey {
            shift,
            alt,
            ..key(EditorKeyCode::Enter)
        }
    }
    fn type_str(e: &mut PlainEngine, s: &str) {
        for c in s.chars() {
            e.feed_key(k(c));
        }
    }

    #[test]
    fn types_and_submits_on_plain_enter() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "hello");
        assert_eq!(e.content(), "hello");
        assert!(e.wants_submit(&enter(false, false)));
    }

    #[test]
    fn shift_or_alt_enter_inserts_newline_and_does_not_submit() {
        for (shift, alt) in [(true, false), (false, true)] {
            let mut e = PlainEngine::new();
            type_str(&mut e, "a");
            let ne = enter(shift, alt);
            assert!(
                !e.wants_submit(&ne),
                "shift={shift} alt={alt} should not submit"
            );
            e.feed_key(ne);
            type_str(&mut e, "b");
            assert_eq!(e.content(), "a\nb", "shift={shift} alt={alt}");
        }
    }

    #[test]
    fn backslash_enter_is_newline_and_strips_the_backslash() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "a\\");
        let ret = enter(false, false);
        assert!(!e.wants_submit(&ret)); // trailing backslash → newline, not submit
        e.feed_key(ret);
        type_str(&mut e, "b");
        assert_eq!(e.content(), "a\nb");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_jump_to_line_bounds() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "hello");
        e.feed_key(ctrl('a'));
        type_str(&mut e, "X");
        assert_eq!(e.content(), "Xhello");
        e.feed_key(ctrl('e'));
        type_str(&mut e, "Y");
        assert_eq!(e.content(), "XhelloY");
    }

    #[test]
    fn ctrl_w_deletes_the_previous_word() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "foo bar");
        e.feed_key(ctrl('w'));
        assert_eq!(e.content(), "foo ");
    }

    /// Wide (CJK) glyphs are 2 display columns each: wrapping and cursor
    /// placement must follow columns, not chars, or the box overflows and the
    /// terminal cursor lands on the wrong cell.
    ///
    /// Regression: both `desired_rows` and `render` counted `chars().count()`,
    /// so 6 double-width glyphs (12 columns) were treated as fitting a
    /// 10-column line with room to spare.
    #[test]
    fn wide_glyphs_wrap_by_display_width_and_place_the_cursor_on_the_right_cell() {
        use crate::TuiRender;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::{Position, Rect};

        let mut e = PlainEngine::new();
        // Six wide glyphs = 12 display columns. At width 10, 5 fit the first
        // row (10 columns exactly) and the 6th wraps — not "6 chars fit in
        // 10", which the old char-counting wrap would have concluded.
        type_str(&mut e, "国国国国国国");
        assert_eq!(
            e.desired_rows(10, 5),
            2,
            "5 glyphs (10 cols) fit the first row; the 6th wraps to a second"
        );

        let area = Rect::new(0, 0, 10, 5);
        let mut term = Terminal::new(TestBackend::new(10, 5)).unwrap();
        term.draw(|f| e.render(f, area)).unwrap();

        // Cursor sits right after the 6th glyph: row 1 (the wrapped row),
        // column 2 (past the one wide glyph that landed there). Counting
        // chars instead of columns would place it at row 0, column 6 —
        // inside the first row, and on the wrong cell entirely.
        assert_eq!(
            term.get_cursor_position().unwrap(),
            Position::new(2, 1),
            "cursor should land on the wrapped row, 2 columns in"
        );
    }
}
