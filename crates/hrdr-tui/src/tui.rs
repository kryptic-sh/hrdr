//! The terminal driver: owns the ratatui `Terminal` + the crossterm event loop,
//! translating input into [`App`] method calls and rendering `App` state. This
//! is the only place tied to the terminal — `App` itself carries no terminal
//! I/O or renderer types, so another frontend can drive the same `App` with its
//! own loop + renderer.

use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use futures_util::StreamExt;

use crate::app::{Action, App, run_editor};
use crate::{Tui, resume_terminal, suspend_terminal, ui};

/// Tell the terminal what the cursor should look like, when it changes.
///
/// The editor already places the real cursor (`Frame::set_cursor_position`), so
/// this only picks its *shape*: a blinking bar while inserting text, a blinking
/// block otherwise (vim's Normal mode). Terminals often default to a steady
/// cursor, which is why it has to be asked for. Emitted only on a change — an
/// escape sequence per frame would be noise.
fn sync_cursor<W: std::io::Write>(
    out: &mut W,
    insert: bool,
    last: &mut Option<bool>,
) -> Result<()> {
    if *last == Some(insert) {
        return Ok(());
    }
    let style = if insert {
        SetCursorStyle::BlinkingBar
    } else {
        SetCursorStyle::BlinkingBlock
    };
    execute!(out, style)?;
    *last = Some(insert);
    Ok(())
}

/// Drive `app` against the terminal until it quits: draw, then await terminal
/// input, agent messages, config-file changes, or a spinner tick.
pub(crate) async fn run_loop(
    app: &mut App,
    terminal: &mut Tui,
    command: Option<String>,
) -> Result<()> {
    // Probe the endpoint in the background and warn if it's unreachable or
    // doesn't have the configured model — surfaced before the first turn.
    app.spawn_health_check();
    // The endpoint's advertised context window drives the status bar's gauge and
    // the auto-compaction threshold; probed once, in the background.
    app.spawn_context_probe();
    // `session_start` lifecycle hooks, off-thread like the probes.
    app.spawn_session_start_hooks();
    let mut events = EventStream::new();
    let mut rx = app.rx.take().expect("run_loop called once");
    // Periodic wake so the inference spinner animates between tokens.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    // Shared config watch (OS watcher with polling fallback); pings arrive as
    // TurnMsg::ConfigChanged. Kept alive for the loop.
    let _config_watch = app.start_config_watch();
    // The editor's insert state drives the cursor shape (see `sync_cursor`).
    // `None` forces the first frame to emit it.
    let mut cursor_insert: Option<bool> = None;

    // A command handed to hrdr on the command line (`hrdr /new`, `hrdr /model`,
    // `hrdr '!git status'`) runs here — after the session is up and any auto-resume
    // has happened, so `/new` starts from a real session and `/resume` has one to
    // pick from, and *before* the first frame, so the picker a command opens is
    // already on screen when the terminal first paints.
    //
    // It goes through `submit_input`, the same path `Enter` takes: the command line
    // gets whatever the input box gets, and neither has to know what the other
    // supports.
    if let Some(input) = command {
        match app.submit_input(input) {
            Action::OpenEditor => open_in_editor(app, terminal)?,
            Action::OpenFile(path) => open_file_in_editor(app, terminal, &path)?,
            Action::Redraw => terminal.clear()?,
            Action::None => {}
        }
    }

    loop {
        // A detached sub-agent that finished while the agent was idle wakes it,
        // so its result reaches the model without waiting for the user to type.
        app.maybe_deliver_background();
        terminal.draw(|f| ui::draw(f, app))?;
        sync_cursor(
            terminal.backend_mut(),
            app.editor.is_insert(),
            &mut cursor_insert,
        )?;
        if app.should_quit {
            // Reap a turn cancelled on the quit path first: awaiting the aborted
            // task drops its future and releases the agent lock, so the save
            // below can't lose the race and skip. Then the final save — a
            // backstop for the idle-quit paths too, with no "next autosave" to
            // catch it since the app is exiting. `save_session`'s own
            // is-there-anything-to-save guard keeps this a no-op when there's
            // nothing new, so it never double-writes.
            app.reap_cancelled_turn().await;
            app.autosave();
            // `session_end` hooks run after the final save, awaited (each
            // hook's timeout bounds the wait) — a spawned task would be
            // killed when the process exits right after.
            app.run_session_end_hooks().await;
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(key))) => match app.on_key(key) {
                    // Leaving the alt screen for `$EDITOR` resets the cursor
                    // shape; forget ours so the next frame asks for it again.
                    Action::OpenEditor => {
                        open_in_editor(app, terminal)?;
                        cursor_insert = None;
                    }
                    Action::OpenFile(path) => {
                        open_file_in_editor(app, terminal, &path)?;
                        cursor_insert = None;
                    }
                    Action::Redraw => terminal.clear()?,
                    Action::None => {}
                },
                Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                Some(Ok(Event::Paste(text))) => app.on_paste(&text),
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

#[cfg(test)]
mod cursor_tests {
    use super::sync_cursor;

    /// The cursor blinks: a bar while inserting text, a block otherwise (vim's
    /// Normal mode). The escape is emitted only when the shape changes — one per
    /// frame would be noise on the wire.
    ///
    /// Regression: nothing ever asked for a shape, so the cursor kept whatever
    /// the terminal defaulted to, which is usually steady.
    #[test]
    fn the_cursor_style_is_emitted_on_change_only() {
        // DECSCUSR: 5 = blinking bar, 1 = blinking block.
        const BLINKING_BAR: &str = "\x1b[5 q";
        const BLINKING_BLOCK: &str = "\x1b[1 q";

        let mut out: Vec<u8> = Vec::new();
        let mut last = None;

        // First frame always emits, whatever the state.
        sync_cursor(&mut out, true, &mut last).unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), BLINKING_BAR);
        assert_eq!(last, Some(true));

        // Unchanged: nothing on the wire.
        out.clear();
        sync_cursor(&mut out, true, &mut last).unwrap();
        assert!(out.is_empty(), "re-emitted an unchanged cursor style");

        // Leaving insert switches to a blinking block.
        sync_cursor(&mut out, false, &mut last).unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), BLINKING_BLOCK);
        assert_eq!(last, Some(false));

        // …and back.
        out.clear();
        sync_cursor(&mut out, true, &mut last).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), BLINKING_BAR);
    }
}
