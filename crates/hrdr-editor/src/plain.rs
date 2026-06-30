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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::EditorEngine;

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

    /// Cursor position as `(row, col)` in display lines from the buffer top.
    fn cursor_rowcol(&self) -> (usize, usize) {
        let mut row = 0;
        let mut col = 0;
        for &c in &self.chars[..self.cursor] {
            if c == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    }
}

impl EditorEngine for PlainEngine {
    fn feed_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.home(),
                'e' => self.end(),
                'w' => self.delete_word(),
                'u' => self.kill_to_line_start(),
                _ => {}
            },
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Enter => {
                // Reached only when this Enter is NOT a submit (Shift+Enter, or
                // `\`+Enter). Strip the escape backslash if present.
                if self.pending_backslash() {
                    self.backspace();
                }
                self.insert('\n');
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
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

    fn wants_submit(&self, key: &KeyEvent) -> bool {
        // Enter sends — unless Shift is held or the line ends with a backslash,
        // in which case it inserts a newline instead.
        key.code == KeyCode::Enter
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !self.pending_backslash()
    }

    fn keybind_hint(&self) -> &'static str {
        "Enter=send · Shift/\\+Enter=newline · Ctrl+G=$EDITOR · Ctrl+C=quit"
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let text: String = self.chars.iter().collect();
        frame.render_widget(Paragraph::new(text), area);

        let (row, col) = self.cursor_rowcol();
        let max_x = area.width.saturating_sub(1);
        let max_y = area.height.saturating_sub(1);
        let x = area.x + (col as u16).min(max_x);
        let y = area.y + (row as u16).min(max_y);
        frame.set_cursor_position((x, y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn enter(mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, mods)
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
        assert!(e.wants_submit(&enter(KeyModifiers::NONE)));
    }

    #[test]
    fn shift_enter_inserts_newline_and_does_not_submit() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "a");
        let se = enter(KeyModifiers::SHIFT);
        assert!(!e.wants_submit(&se));
        e.feed_key(se);
        type_str(&mut e, "b");
        assert_eq!(e.content(), "a\nb");
    }

    #[test]
    fn backslash_enter_is_newline_and_strips_the_backslash() {
        let mut e = PlainEngine::new();
        type_str(&mut e, "a\\");
        let ret = enter(KeyModifiers::NONE);
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
}
