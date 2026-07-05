//! The terminal driver: owns the ratatui `Terminal` + the crossterm event loop,
//! translating input into [`App`] method calls and rendering `App` state. This
//! is the only place tied to the terminal — `App` itself carries no terminal
//! I/O or renderer types, so a GUI frontend can drive the same `App` with its
//! own loop + renderer.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;

use crate::app::{Action, App, run_editor};
use crate::{Tui, resume_terminal, suspend_terminal, ui};

/// Drive `app` against the terminal until it quits: draw, then await terminal
/// input, agent messages, config-file changes, or a spinner tick.
pub(crate) async fn run_loop(app: &mut App, terminal: &mut Tui) -> Result<()> {
    // Probe the endpoint in the background and warn if it's unreachable or
    // doesn't have the configured model — surfaced before the first turn.
    app.spawn_health_check();
    let mut events = EventStream::new();
    let mut rx = app.rx.take().expect("run_loop called once");
    // Periodic wake so the inference spinner animates between tokens.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    // Shared config watch (OS watcher with polling fallback); pings arrive as
    // TurnMsg::ConfigChanged. Kept alive for the loop.
    let _config_watch = app.start_config_watch();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(key))) => match app.on_key(key) {
                    Action::OpenEditor => open_in_editor(app, terminal)?,
                    Action::OpenFile(path) => open_file_in_editor(app, terminal, &path)?,
                    Action::Redraw => terminal.clear()?,
                    Action::None => {}
                },
                Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                Some(Ok(Event::Paste(text))) => {
                    app.quit_armed = false;
                    app.editor.paste(&text);
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            Some(msg) = rx.recv() => {
                app.on_turn_msg(msg);
                // Drain any further messages that arrived in the same burst so
                // fast-streaming endpoints don't cause 100+ full redraws/sec —
                // all buffered tokens are folded into state before the next draw.
                while let Ok(msg) = rx.try_recv() {
                    app.on_turn_msg(msg);
                }
            }
            _ = ticker.tick() => {}
        }
    }
    Ok(())
}

/// Hand the input buffer to `$EDITOR`/`$VISUAL`, then read it back.
///
/// The draft is written to a randomly-named temporary file created by
/// [`tempfile::Builder`], which gives it 0600 permissions on Unix so other
/// local users cannot read prompt content that may include pasted secrets.
/// The unpredictable name also prevents symlink-planting attacks that a
/// guessable `hrdr-input-<pid>.md` path would allow.  The `.md` suffix is
/// preserved so editors detect markdown for syntax highlighting.
fn open_in_editor(app: &mut App, terminal: &mut Tui) -> Result<()> {
    // Random name, 0600 perms (tempfile's platform default), `.md` extension.
    let mut named = tempfile::Builder::new()
        .prefix("hrdr-input-")
        .suffix(".md")
        .tempfile()?;
    // Write the draft through the already-open fd before the editor opens it.
    use std::io::Write as _;
    named.write_all(app.editor.content().as_bytes())?;
    named.flush()?;
    let path = named.path().to_path_buf();

    suspend_terminal(terminal)?;
    let status = run_editor(&path);
    resume_terminal(terminal)?;
    terminal.clear()?;

    if status.is_ok()
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        // Editors append a trailing newline; drop one so it doesn't submit blank.
        let text = text.strip_suffix('\n').unwrap_or(&text);
        app.editor.set_content(text);
    }
    // `named` drop closes the fd and deletes the temp file.
    drop(named);
    Ok(())
}

/// Open an arbitrary file in `$EDITOR` (from `/edit <file>`), suspending the TUI
/// for the duration. The file may not exist yet — the editor creates it.
fn open_file_in_editor(app: &mut App, terminal: &mut Tui, path: &std::path::Path) -> Result<()> {
    suspend_terminal(terminal)?;
    let status = run_editor(path);
    resume_terminal(terminal)?;
    terminal.clear()?;
    match status {
        Ok(_) => app.system(format!("edited {}", path.display())),
        Err(e) => app.system(format!("editor failed: {e}")),
    }
    Ok(())
}
