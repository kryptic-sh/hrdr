//! `hrdr-editor` — the FSM-agnostic editing seam.
//!
//! The rest of hrdr talks only to the [`EditorEngine`] trait — never to vim
//! types. The concrete [`VimEngine`] wraps the hjkl engine and drives it
//! through `hjkl_vim::dispatch_input`. When hjkl's pluggable-FSM work (epic
//! #265) lands a vscode/helix discipline, add a sibling `EditorEngine` impl
//! and the TUI swaps it in with zero churn — it reads `mode_label()` /
//! `is_insert()` (projected from hjkl's FSM-agnostic `CoarseMode`), not
//! `VimMode`.

mod host;
mod plain;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hjkl_buffer_tui::{BufferView, Gutter};
use hjkl_engine::{CoarseMode, Editor, Host, Options};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

pub use host::HrdrHost;
pub use plain::PlainEngine;

/// A pluggable editing discipline embedded in the TUI.
///
/// Implementors hide their concrete editor/FSM entirely. The TUI only needs
/// these operations to host an editable text pane.
pub trait EditorEngine {
    /// Feed a terminal key event into the engine.
    fn feed_key(&mut self, key: KeyEvent);
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
    fn wants_submit(&self, key: &KeyEvent) -> bool;
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
            let code = match c {
                '\n' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                other => KeyCode::Char(other),
            };
            self.feed_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }
    /// Draw the editable pane into `area` and place the cursor.
    fn render(&mut self, frame: &mut Frame, area: Rect);
}

/// Number of display rows `text` occupies when hard-wrapped at `width` columns.
pub(crate) fn wrapped_row_count(text: &str, width: u16) -> usize {
    let w = width.max(1) as usize;
    text.split('\n')
        .map(|line| {
            let cells = line.chars().count();
            if cells == 0 { 1 } else { cells.div_ceil(w) }
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

    pub fn with_content(text: &str) -> Self {
        let mut e = Self::new();
        e.set_content(text);
        e
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
    fn feed_key(&mut self, key: KeyEvent) {
        // We only push DISAMBIGUATE_ESCAPE_CODES (not REPORT_EVENT_TYPES), but
        // guard against release events doubling keystrokes on terminals that
        // report them anyway.
        if key.kind == KeyEventKind::Release {
            return;
        }
        let input = hjkl_engine_tui::crossterm_to_input(key);
        hjkl_vim::dispatch_input(&mut self.editor, input);
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

    fn wants_submit(&self, key: &KeyEvent) -> bool {
        // Vim convention: Enter in Normal mode sends; in Insert it's a newline.
        key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && matches!(self.editor.coarse_mode(), CoarseMode::Normal)
    }

    fn keybind_hint(&self) -> &'static str {
        "Esc=normal · Enter(normal)=send · Ctrl+G=$EDITOR · Ctrl+C×2=quit"
    }

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
