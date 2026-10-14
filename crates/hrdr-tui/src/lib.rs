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
mod cursor_backend;
mod theme;
mod trust_prompt;
mod tui;
pub use app::{
    AdmittedUpdate, DeferredUpdate, PreparedUpdateCoordinator, UpdateRequestReceiver,
    UpdateRequestSender, update_request_bridge,
};
pub use trust_prompt::ask_trust;
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
        push_keyboard_enhancement(&mut out);
        Ok(Self)
    }
}

/// Ask for keyboard enhancement, and carry on without it if refused.
///
/// Enhancement is a *nicety*: with it, `Shift+Enter` and friends arrive
/// unambiguously; without it, they don't, and everything else works exactly as
/// before. So it must never be the reason hrdr fails to start — and it was.
/// crossterm implements neither the push nor the pop for Windows at all (both
/// report `Unsupported` on every Windows terminal, VT-capable or not), and a
/// `?` on it made hrdr print "Keyboard progressive enhancement not implemented
/// for the legacy Windows API" and exit 1 before painting a single frame.
fn push_keyboard_enhancement(out: &mut impl std::io::Write) {
    let _ = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    );
}

/// Restore the terminal to its normal state: the cursor style, keyboard
/// enhancement flags, mouse capture, alternate screen and bracketed paste
/// hrdr switched on. Idempotent; errors are the caller's to handle.
fn restore_terminal_state(out: &mut impl std::io::Write) -> std::io::Result<()> {
    // Hand the cursor back the way we found it.
    execute!(out, SetCursorStyle::DefaultUserShape)?;
    // Popped before leaving the alternate screen, whose flag stack it belongs to,
    // and on its own: it fails on every Windows terminal (see
    // [`push_keyboard_enhancement`]), and inside one `execute!` that error
    // short-circuits every command after it — the alternate screen was never
    // left, mouse capture never released, and suspending for `$EDITOR` quit hrdr.
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    execute!(
        out,
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste,
    )
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = restore_terminal_state(&mut out);
        let _ = disable_raw_mode();
    }
}

type Tui = Terminal<cursor_backend::CursorBackend<CrosstermBackend<Stdout>>>;

/// Leave the TUI screen so an external program (e.g. `$EDITOR`) can use the
/// terminal: drop raw mode, the alt screen, and the keyboard enhancements.
pub(crate) fn suspend_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    restore_terminal_state(terminal.backend_mut())?;
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
    )?;
    push_keyboard_enhancement(terminal.backend_mut());
    Ok(())
}

pub fn discover_handoff_recovery(config: &AgentConfig) -> Result<Option<String>> {
    let cwd = config.cwd.to_string_lossy();
    hrdr_agent::Session::discover_handoff(&cwd, config.unix_style_paths, |payload, session| {
        app::validate_handoff_payload(payload, session).map(|_| ())
    })
    .map_err(anyhow::Error::from)
}

const HANDOFF_RECOVERY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
const HANDOFF_RECOVERY_RETRY_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

#[cfg(feature = "test-support")]
async fn handoff_recovery_retry_test_barrier() {
    let (Some(marker), Some(release)) = (
        std::env::var_os("HRDR_TEST_HANDOFF_RETRY_MARKER"),
        std::env::var_os("HRDR_TEST_HANDOFF_RETRY_RELEASE"),
    ) else {
        return;
    };
    let marker = std::path::PathBuf::from(marker);
    let release = std::path::PathBuf::from(release);
    assert!(
        !marker.exists(),
        "handoff retry test marker already exists: {}",
        marker.display()
    );
    std::fs::write(&marker, b"snapshot retry reached").expect("write handoff retry test marker");
    while !release.exists() {
        tokio::time::sleep(HANDOFF_RECOVERY_RETRY_DELAY).await;
    }
}

fn can_retry_handoff_recovery(
    cwd: &str,
    id: &str,
    source: Option<hrdr_tools::ProcessLease>,
    deadline: std::time::Instant,
) -> bool {
    source.is_some_and(|source| {
        std::time::Instant::now() < deadline
            && matches!(
                hrdr_tools::process_lease_liveness(source),
                hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced
            )
            && hrdr_agent::HandoffJournal::claimable_source(cwd, id)
                .is_ok_and(|claimable| claimable == Some(source))
    })
}

fn can_retry_snapshot_io_handoff_recovery(
    cwd: &str,
    id: &str,
    source: Option<hrdr_tools::ProcessLease>,
    deadline: std::time::Instant,
) -> bool {
    source.is_some_and(|source| {
        std::time::Instant::now() < deadline
            && matches!(
                hrdr_tools::process_lease_liveness(source),
                hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced
            )
            && hrdr_agent::HandoffJournal::has_claimable_source_without_snapshot(cwd, id, source)
                .unwrap_or(false)
    })
}

/// Which process a handoff recovery claims the checkpoint as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryClaim {
    /// The newest committed checkpoint, whose source is found dead.
    Discovered,
    /// The checkpoint committed by this exact source, which a coordinator saw
    /// exit.
    Source(hrdr_tools::ProcessLease),
    /// The checkpoint this very process committed before `exec`ing its
    /// successor image (unix self-update).
    Successor,
}

pub async fn run_handoff_recovery(
    config: AgentConfig,
    mut ui: hrdr_app::UiConfig,
    logo: &'static str,
    id: &str,
    update_failure: Option<&str>,
    claim: RecoveryClaim,
) -> Result<()> {
    ui.auto_resume = false;
    let cwd = config.cwd.to_string_lossy();
    let deadline = std::time::Instant::now() + HANDOFF_RECOVERY_RETRY_WINDOW;
    let source = match claim {
        RecoveryClaim::Source(source) => Some(source),
        RecoveryClaim::Discovered | RecoveryClaim::Successor => None,
    };
    let recovery = loop {
        if claim == RecoveryClaim::Successor {
            // The committing image released the lock before the `exec`, and
            // nothing else can claim this checkpoint, so there is nothing to
            // wait out.
            break hrdr_agent::Session::recover_handoff_as_successor(
                &cwd,
                id,
                config.unix_style_paths,
                app::validate_handoff_payload,
            )?;
        }
        match hrdr_agent::Session::recover_handoff(
            &cwd,
            id,
            config.unix_style_paths,
            source,
            app::validate_handoff_payload,
        ) {
            Ok(recovery) => break recovery,
            Err(hrdr_agent::OpenError::Busy { .. })
                if can_retry_handoff_recovery(&cwd, id, source, deadline) =>
            {
                tokio::time::sleep(HANDOFF_RECOVERY_RETRY_DELAY).await;
            }
            Err(hrdr_agent::OpenError::Load(error))
                if hrdr_agent::HandoffJournal::is_transient_snapshot_io_error(&error)
                    && can_retry_snapshot_io_handoff_recovery(&cwd, id, source, deadline) =>
            {
                #[cfg(feature = "test-support")]
                handoff_recovery_retry_test_barrier().await;
                tokio::time::sleep(HANDOFF_RECOVERY_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    };

    let _guard = TerminalGuard::enter()?;
    let backend = cursor_backend::CursorBackend::new(CrosstermBackend::new(stdout()));
    let mut terminal: Tui = Terminal::new(backend)?;
    let mut app = App::new_recovery(config, ui, logo, recovery.intent)?;
    app.adopt_handoff_recovery(id.to_string(), recovery.session, recovery.lock);
    if let Some(update_failure) = update_failure {
        app.update_recovery_failure(update_failure);
    }
    if app.activate_update_continuation() {
        tui::run_loop(&mut app, &mut terminal, None, false).await?;
    } else {
        tui::run_recovery_loop(&mut app, &mut terminal).await?;
        if app.recovery_activated() {
            tui::run_loop(&mut app, &mut terminal, None, false).await?;
        }
    }
    drop(terminal);
    drop(_guard);
    report_undelivered_peer_messages(&app);
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
/// `hrdr ':review src/lib.rs'` invokes a command, and anything else is a first
/// message to the model.
pub async fn run(
    config: AgentConfig,
    ui: hrdr_app::UiConfig,
    logo: &'static str,
    command: Option<String>,
) -> Result<()> {
    run_inner(config, ui, logo, command, None).await
}

/// Launch the interactive TUI with a frontend-owned tool installed on the main agent.
pub async fn run_with_update_tool(
    config: AgentConfig,
    ui: hrdr_app::UiConfig,
    logo: &'static str,
    command: Option<String>,
    receiver: UpdateRequestReceiver,
    tool: std::sync::Arc<dyn hrdr_tools::Tool>,
) -> Result<()> {
    run_inner(config, ui, logo, command, Some((receiver, tool))).await
}

async fn run_inner(
    config: AgentConfig,
    ui: hrdr_app::UiConfig,
    logo: &'static str,
    command: Option<String>,
    update_tool: Option<(UpdateRequestReceiver, std::sync::Arc<dyn hrdr_tools::Tool>)>,
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
        let _ = restore_terminal_state(&mut out);
        let _ = disable_raw_mode();
        prev_hook(info);
    }));

    let _guard = TerminalGuard::enter()?;
    let backend = cursor_backend::CursorBackend::new(CrosstermBackend::new(stdout()));
    let mut terminal: Tui = Terminal::new(backend)?;

    // Session retention: a background worker compresses idle sessions and purges
    // old auto-named ones. First pass 30s after start (out of the way of launch),
    // then hourly. Peer-safe via each session's open-lock, so several instances
    // coexist. The sweep is blocking fs I/O, so it runs on a blocking thread —
    // `hrdr_agent::sweep_sessions` is the whole of it.
    let compress_after = config.session_compress_after.filter(|&t| t > 0);
    let purge_after = config.session_purge_after.filter(|&t| t > 0);
    if compress_after.is_some() || purge_after.is_some() {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            loop {
                let _ = tokio::task::spawn_blocking(move || {
                    hrdr_app::sweep_sessions(compress_after, purge_after)
                })
                .await;
                tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
            }
        });
    }

    let mut app = App::new(config, ui, logo)?;
    if let Some((receiver, tool)) = update_tool {
        app.install_update_tool(receiver, tool).await;
    }
    app.connect_mcp().await;
    tui::run_loop(&mut app, &mut terminal, command, true).await?;
    drop(terminal);
    drop(_guard);
    report_undelivered_peer_messages(&app);
    Ok(())
}

/// Say, on the restored terminal, how many peer messages the exiting session
/// accepted but never handed its agent — their senders were told "accepted".
fn report_undelivered_peer_messages(app: &App) {
    let undelivered = app.messaging.undelivered_on_exit;
    if undelivered > 0 {
        eprintln!("hrdr: {undelivered} peer message(s) were not delivered before exit");
    }
}
