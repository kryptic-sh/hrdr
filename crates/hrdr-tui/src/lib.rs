//! `hrdr-tui` — the interactive terminal UI.
//!
//! Layout: a scrolling transcript (assistant text, reasoning, tool calls) above
//! a vim-keybound input pane. The agent runs on a background task; its
//! [`AgentEvent`]s stream over a channel and the UI selects them against
//! crossterm's async `EventStream`, so input stays responsive during a turn.
//!
//! Workflow: type in the input (Insert mode), `Esc` to Normal, `Enter` to send.

// Every test in this crate — including one written tomorrow by someone who read none
// of this — runs with `$HOME` and the XDG roots pointed at a throwaway directory. The
// `extern crate` is what links `hrdr-test-support`'s life-before-main ctor into this
// test binary; rustc drops a dependency nothing references, and a dropped ctor is a
// test writing the developer's real sessions. Do not remove it.
#[cfg(test)]
extern crate hrdr_test_support;

mod app;
mod theme;
mod tui;
mod ui;

use std::io::{Stdout, stdout};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hrdr_agent::AgentConfig;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;

/// Restores the terminal to a sane state on drop, even on panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
        )?;
        // Keyboard enhancement is a *nicety*: with it, `Shift+Enter` and friends
        // arrive unambiguously; without it, they don't, and everything else works
        // exactly as before. So it must never be the reason hrdr fails to start —
        // and it was. crossterm has no implementation of it for the legacy Windows
        // console API and returns an error there, which this propagated with `?`:
        // on a Windows terminal without VT support, hrdr printed
        // "Keyboard progressive enhancement not implemented for the legacy Windows
        // API" and exited 1, before painting a single frame. Ask for it; carry on
        // without it.
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(
            out,
            // Hand the cursor back the way we found it.
            SetCursorStyle::DefaultUserShape,
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            LeaveAlternateScreen,
            DisableBracketedPaste,
        );
        let _ = disable_raw_mode();
    }
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Leave the TUI screen so an external program (e.g. `$EDITOR`) can use the
/// terminal: drop raw mode, the alt screen, and the keyboard enhancements.
pub(crate) fn suspend_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-enter the TUI screen after [`suspend_terminal`].
pub(crate) fn resume_terminal(terminal: &mut Tui) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )?;
    Ok(())
}

/// Launch the interactive TUI against the configured agent, with `ui` holding
/// the display knobs (theme, icons, vim mode, …) split out of the agent config.
///
/// `logo` is the ASCII art the session header animates — the caller owns it (the
/// binary also prints it above `--help`), so the TUI never embeds one.
///
/// `command` is a line of input to run as soon as the session is up, exactly as if
/// it had been typed into the input box and submitted: `hrdr /new` starts fresh,
/// `hrdr /model` opens the picker, `hrdr '!git status'` runs the shell escape,
/// `hrdr ':review src/lib.rs'` invokes a skill, and anything else is a first
/// message to the model.
pub async fn run(
    config: AgentConfig,
    ui: hrdr_app::UiConfig,
    logo: &'static str,
    command: Option<String>,
) -> Result<()> {
    // Install a panic hook that restores the terminal to its normal state
    // *before* the panic message and backtrace are printed.  Without this the
    // message lands inside the alt screen and is immediately cleared on exit —
    // the crash looks like a silent exit.  `TerminalGuard::drop` performs the
    // same cleanup during the unwind, so a double-restore is harmless: every
    // crossterm operation used here is idempotent and errors are ignored.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort restore; errors are intentionally swallowed so we never
        // mask the real panic message with a secondary I/O failure.
        let mut out = stdout();
        let _ = execute!(
            out,
            // Hand the cursor back the way we found it.
            SetCursorStyle::DefaultUserShape,
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            LeaveAlternateScreen,
            DisableBracketedPaste,
        );
        let _ = disable_raw_mode();
        prev_hook(info);
    }));

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal: Tui = Terminal::new(backend)?;

    let mut app = App::new(config, ui, logo)?;
    app.connect_mcp().await;
    tui::run_loop(&mut app, &mut terminal, command).await?;
    Ok(())
}
