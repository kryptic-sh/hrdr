//! The directory-trust question, asked before a session opens.
//!
//! It lives here, and it is drawn with ratatui, for one reason: **portability**.
//! Hand-written escape sequences are fine on any Unix terminal and are not safe
//! on Windows. crossterm only emits ANSI once it has confirmed the console can
//! parse it ([`crossterm::ansi_support::supports_ansi`], which lazily turns on
//! `ENABLE_VIRTUAL_TERMINAL_PROCESSING`); where it cannot, crossterm falls back
//! to direct WinAPI calls, and any escape byte written by hand around those calls
//! reaches the screen as literal garbage. Going through ratatui's
//! `CrosstermBackend` means every colour and attribute on this screen takes
//! whichever of those two paths the console actually supports.
//!
//! Drawing it here also means it shares the session's own [`Theme`] and the same
//! `hjkl_splash` logo animation the header uses, rather than a second copy that
//! could drift from it.

use std::io::{Stdout, stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hrdr_agent::trust::TrustChoice;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;

/// How long a frame waits for a key before repainting. Short enough that the
/// logo animates smoothly, long enough that an idle question is not a spin.
const FRAME: Duration = Duration::from_millis(80);

/// Left margin for every line on this screen, in columns.
const MARGIN: u16 = 2;

/// Columns between an option's label and its description.
const LABEL_GAP: usize = 3;

/// The question's options, in order. Cancel is last, and is the default.
const ASK: &[(&str, &str)] = &[
    ("trust", "load this directory's instructions, full tool set"),
    (
        "untrusted",
        "open jailed: read the tree, run nothing from it",
    ),
    ("cancel", "quit without opening"),
];

/// The confirmation's options. "No" is last, and is the default.
const CONFIRM: &[(&str, &str)] = &[
    ("yes, I'm sure", "remember this directory as trusted"),
    ("no, go back", "return to the previous question"),
];

/// Ask whether `cwd` may steer this session, on the terminal, before any session
/// exists. `theme` is the same spec the TUI takes.
///
/// Returns [`TrustChoice::Cancel`] when there is no terminal to ask on — the same
/// answer as "I do not know what is being asked".
pub fn ask_trust(cwd: &Path, logo: &str, theme: Option<&str>) -> TrustChoice {
    let Ok(mut screen) = AskScreen::enter() else {
        return TrustChoice::Cancel;
    };
    let theme = Theme::load(theme);
    let choice = run_menus(&mut screen.terminal, cwd, logo, &theme);
    // `screen` drops here: alternate screen left, raw mode off, cursor back —
    // on every path out, including a panic in the loop.
    choice
}

/// The terminal this screen borrows, given back on drop.
///
/// A guard rather than paired calls: there are several ways out of a menu — a
/// choice, Esc, Ctrl-C, a panic — and a terminal left in raw mode on the
/// alternate screen is one the user has to `reset` by hand.
struct AskScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl AskScreen {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        let _ = terminal.hide_cursor();
        Ok(Self { terminal })
    }
}

impl Drop for AskScreen {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// The question, then the confirmation, until one of them settles it.
fn run_menus(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cwd: &Path,
    logo: &str,
    theme: &Theme,
) -> TrustChoice {
    let head = |theme: &Theme| {
        vec![
            Line::raw(""),
            Line::styled(
                "hrdr has not been opened in this directory before:",
                Style::default().fg(theme.assistant),
            ),
            Line::raw(""),
            Line::styled(
                format!("  {}", cwd.display()),
                Style::default().fg(theme.accent),
            ),
            Line::raw(""),
            Line::styled(
                "Its AGENTS.md and command files are instructions that reach the model,",
                Style::default().fg(theme.dim),
            ),
            Line::styled(
                "and its code is what any command you approve will run. Trust it only",
                Style::default().fg(theme.dim),
            ),
            Line::styled(
                "if you know where it came from.",
                Style::default().fg(theme.dim),
            ),
            Line::raw(""),
        ]
    };
    let confirm_head = |theme: &Theme| {
        vec![
            Line::raw(""),
            Line::styled(
                "Trusting is remembered — every future session here loads this",
                Style::default().fg(theme.assistant),
            ),
            Line::styled(
                "directory's instructions without asking again.",
                Style::default().fg(theme.assistant),
            ),
            Line::raw(""),
        ]
    };

    loop {
        // Default: cancel, the last entry.
        match menu(terminal, logo, theme, &head(theme), ASK, ASK.len() - 1) {
            Some(0) => match menu(terminal, logo, theme, &confirm_head(theme), CONFIRM, 1) {
                Some(0) => return TrustChoice::Trusted,
                // "no, go back" and Esc both return to the first question.
                _ => continue,
            },
            Some(1) => return TrustChoice::Untrusted,
            _ => return TrustChoice::Cancel,
        }
    }
}

/// Draw `head` under the animated logo, then `items` as a selectable list
/// starting at `default`. Returns the chosen index, or `None` for Esc, `q` and
/// Ctrl-C.
fn menu(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    logo: &str,
    theme: &Theme,
    head: &[Line<'static>],
    items: &[(&str, &str)],
    default: usize,
) -> Option<usize> {
    let mut sel = default;
    let anchor = Instant::now();
    // Measured from the labels, so adding an option cannot leave the list ragged.
    let label_w = items.iter().map(|(l, _)| l.chars().count()).max()? + LABEL_GAP;

    loop {
        let mut lines = crate::ui::splash_lines(logo, theme, anchor);
        lines.push(Line::raw(""));
        lines.extend(head.iter().cloned());
        for (i, (label, blurb)) in items.iter().enumerate() {
            let selected = i == sel;
            // Caret AND bold AND colour: the selection has to survive a terminal
            // with no truecolour and a theme that left the role unset, and the
            // caret survives both.
            let marker = if selected { "❯ " } else { "  " };
            let label_style = if selected {
                Style::default().fg(theme.user).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.dim)
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_string(), label_style),
                Span::styled(format!("{label:label_w$}"), label_style),
                Span::styled(blurb.to_string(), Style::default().fg(theme.dim)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "↑/↓ or j/k to move · enter to choose · esc cancels",
            Style::default().fg(theme.dim),
        ));

        // A failed draw means the terminal is gone; treat it as no answer.
        if terminal
            .draw(|f| {
                let area = f.area();
                let inner = Rect {
                    x: area.x + MARGIN,
                    y: area.y,
                    width: area.width.saturating_sub(MARGIN),
                    height: area.height,
                };
                f.render_widget(
                    Paragraph::new(lines.clone()).alignment(Alignment::Left),
                    inner,
                );
            })
            .is_err()
        {
            return None;
        }

        // Wake often enough to advance the animation whether or not a key came.
        match event::poll(FRAME) {
            Ok(true) => match event::read() {
                // `Release` and `Repeat` arrive too on terminals with keyboard
                // enhancement; acting on both edges would move two rows per press.
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return None,
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                        sel = sel.checked_sub(1).unwrap_or(items.len() - 1);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                        sel = (sel + 1) % items.len();
                    }
                    (KeyCode::Enter, _) => return Some(sel),
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => return None,
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => return None,
            },
            Ok(false) => {}
            Err(_) => return None,
        }
    }
}
