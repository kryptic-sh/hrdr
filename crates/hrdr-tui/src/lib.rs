//! `hrdr-tui` — the interactive terminal UI.
//!
//! Layout: a scrolling transcript (assistant text, reasoning, tool calls) above
//! a vim-keybound input pane. The agent runs on a background task; its
//! [`AgentEvent`]s stream over a channel and the UI selects them against
//! crossterm's async `EventStream`, so input stays responsive during a turn.
//!
//! Workflow: type in the input (Insert mode), `Esc` to Normal, `Enter` to send.

mod app;
mod theme;
mod tui;
mod ui;

use std::io::{Stdout, stdout};

use anyhow::Result;
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
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(
            out,
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
pub async fn run(config: AgentConfig, ui: hrdr_app::UiConfig, logo: &'static str) -> Result<()> {
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
    tui::run_loop(&mut app, &mut terminal).await?;
    Ok(())
}
