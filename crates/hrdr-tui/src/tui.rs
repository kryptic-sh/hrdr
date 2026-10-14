//! The terminal driver: owns the ratatui `Terminal` + the crossterm event loop,
//! translating input into [`App`] method calls and rendering `App` state. This
//! is the only place tied to the terminal — `App` itself carries no terminal
//! I/O or renderer types, so another frontend can drive the same `App` with its
//! own loop + renderer.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use futures_util::StreamExt;

use crate::app::{Action, App, draft_from_editor, run_editor};
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

fn sync_title<W: std::io::Write>(
    out: &mut W,
    title: String,
    last: &mut Option<String>,
) -> Result<()> {
    if last.as_deref() == Some(title.as_str()) {
        return Ok(());
    }
    execute!(out, SetTitle(title.clone()))?;
    *last = Some(title);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A frame woken only by agent messages waits out the rest of the frame
    /// interval since the last draw; the first frame, input and animation
    /// never wait, and a message after the interval draws at once.
    #[test]
    fn message_frames_are_paced_to_the_frame_interval() {
        use super::{Duration, Instant, defer_message_frame};
        let interval = Duration::from_millis(crate::ui::SPINNER_FRAME_MS);
        let drawn = Instant::now();
        let soon = drawn + interval / 4;
        assert_eq!(
            defer_message_frame(true, None, soon),
            None,
            "the first frame"
        );
        assert_eq!(defer_message_frame(false, Some(drawn), soon), None, "input");
        assert_eq!(
            defer_message_frame(true, Some(drawn), soon),
            Some(drawn + interval),
            "a message burst inside the interval"
        );
        assert_eq!(
            defer_message_frame(true, Some(drawn), drawn + interval),
            None,
            "a message burst once the interval has passed"
        );
    }

    use super::{App, edited_file_message, handle_recovery_event};

    #[test]
    fn edited_file_path_style_preserves_raw_path() {
        let raw = r"nested\literal/file.md";
        let path = std::path::PathBuf::from(raw);
        for (style, windows_label) in [
            (true, "nested/literal/file.md"),
            (false, r"nested\literal\file.md"),
        ] {
            let label = if cfg!(windows) { windows_label } else { raw };
            assert_eq!(edited_file_message(&path, style), format!("edited {label}"));
            assert_eq!(path.as_os_str(), std::ffi::OsStr::new(raw));
        }
    }

    #[test]
    fn recovery_loop_ignores_paste_and_mouse_events() {
        let tmp = tempfile::tempdir().unwrap();
        let config = hrdr_agent::AgentConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "local://test-model".parse().unwrap(),
            cwd: tmp.path().to_path_buf(),
            sandbox: hrdr_tools::SandboxMode::None,
            ..Default::default()
        };
        let ui = hrdr_app::UiConfig {
            auto_resume: false,
            ..Default::default()
        };
        let mut app =
            App::new_recovery(config, ui, "logo", hrdr_agent::HandoffIntent::Idle).unwrap();

        handle_recovery_event(
            &mut app,
            crossterm::event::Event::Paste("!echo unsafe".into()),
        );
        handle_recovery_event(
            &mut app,
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        );

        assert!(app.composer_is_empty());
        assert_eq!(app.steering_len_for_test(), 0);
    }
}

pub(crate) enum Wake {
    Input(Option<std::io::Result<Event>>),
    Message(crate::app::TurnMsg),
    Animation,
}

/// The event wait the headless rendering tests drive: the terminal driver's,
/// without frame pacing.
#[cfg(test)]
pub(crate) async fn wait_for_event(
    app: &App,
    events: &mut (impl futures_util::Stream<Item = std::io::Result<Event>> + Unpin),
    rx: &mut tokio::sync::mpsc::Receiver<crate::app::TurnMsg>,
    ticker: &mut tokio::time::Interval,
) -> Wake {
    wait_for_event_until(app, events, rx, ticker, None).await
}

/// [`wait_for_event`], also waking at `deferred_frame`: the moment a frame
/// [`defer_message_frame`] held back is due.
pub(crate) async fn wait_for_event_until(
    app: &App,
    events: &mut (impl futures_util::Stream<Item = std::io::Result<Event>> + Unpin),
    rx: &mut tokio::sync::mpsc::Receiver<crate::app::TurnMsg>,
    ticker: &mut tokio::time::Interval,
    deferred_frame: Option<Instant>,
) -> Wake {
    let deferred = async {
        match deferred_frame {
            Some(at) => tokio::time::sleep_until(at.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        event = events.next() => Wake::Input(event),
        Some(msg) = rx.recv() => Wake::Message(msg),
        // An offscreen logo must not keep an idle transcript redrawing.
        _ = ticker.tick(), if app.spinner_live() => Wake::Animation,
        () = deferred => Wake::Animation,
    }
}

/// When a frame woken only by agent messages may be drawn: `None` now, or the
/// moment one frame interval after the last draw. A fast endpoint delivers a
/// burst every few milliseconds, and drawing after each one re-renders the
/// whole growing reply far more often than anyone can see; input and
/// animation still draw at once.
fn defer_message_frame(
    woken_by_messages: bool,
    last_draw: Option<Instant>,
    now: Instant,
) -> Option<Instant> {
    let due = last_draw? + Duration::from_millis(ui::SPINNER_FRAME_MS);
    (woken_by_messages && now < due).then_some(due)
}

fn handle_recovery_event(app: &mut App, event: Event) {
    if let Event::Key(key) = event {
        app.on_recovery_key(key);
    }
}

pub(crate) async fn run_recovery_loop(app: &mut App, terminal: &mut Tui) -> Result<()> {
    let mut events = EventStream::new();
    let mut cursor_insert = None;
    let mut terminal_title_sent = None;

    loop {
        crate::cursor_backend::draw_frame(terminal, |f| ui::draw(f, app))?;
        sync_title(
            terminal.backend_mut(),
            app.terminal_title(),
            &mut terminal_title_sent,
        )?;
        sync_cursor(
            terminal.backend_mut(),
            app.editor.is_insert(),
            &mut cursor_insert,
        )?;
        if app.should_quit || app.recovery_activated() {
            break;
        }

        match events.next().await {
            Some(Ok(event)) => handle_recovery_event(app, event),
            Some(Err(_)) | None => break,
        }
    }
    Ok(())
}

/// The terminal's input. On Windows under a modern console host it is read
/// as VT sequences (see `vt_input`), which is what lets a bracketed paste
/// arrive as one paste; everywhere else crossterm reads it. If the VT reader
/// cannot start, crossterm's — what every host used before — takes over.
fn input_events()
-> std::pin::Pin<Box<dyn futures_util::Stream<Item = std::io::Result<Event>> + Send>> {
    #[cfg(windows)]
    if crate::vt_input::modern_console_host()
        && let Ok(input) = crate::vt_input::VtInput::start()
    {
        return Box::pin(input);
    }
    Box::pin(EventStream::new())
}

/// Drive `app` against the terminal until it quits: draw, then await terminal
/// input, agent messages, config-file changes, or a spinner tick.
pub(crate) async fn run_loop(
    app: &mut App,
    terminal: &mut Tui,
    command: Option<String>,
    start_runtime: bool,
) -> Result<()> {
    if start_runtime {
        // Probe the endpoint in the background and warn if it's unreachable or
        // doesn't have the configured model — surfaced before the first turn.
        app.spawn_health_check();
        // The endpoint's advertised context window drives the status bar's gauge and
        // the auto-compaction threshold; probed once, in the background.
        app.spawn_context_probe();
        // `session_start` lifecycle hooks, off-thread like the probes.
        app.spawn_session_start_hooks();
    }
    let mut events = input_events();
    let mut rx = app.rx.take().expect("run_loop called once");
    // Periodic wake so the inference spinner animates between tokens.
    let mut ticker = tokio::time::interval(Duration::from_millis(ui::SPINNER_FRAME_MS));
    // Shared config watch (OS watcher with polling fallback); pings arrive as
    // TurnMsg::ConfigChanged. Kept alive for the loop.
    let _config_watch = app.start_config_watch();
    // The editor's insert state drives the cursor shape (see `sync_cursor`).
    // `None` forces the first frame to emit it.
    let mut cursor_insert: Option<bool> = None;
    let mut terminal_title_sent: Option<String> = None;
    // Frame pacing for message-only wakes — see `defer_message_frame`.
    let mut last_draw: Option<Instant> = None;
    let mut woken_by_messages = false;

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
        // Keep this session's peer registration current, and wake (or queue
        // behind the running turn) the next message another session sent.
        app.sync_messaging();
        app.maybe_deliver_peer();
        // A turn that caught a tool panic ran through the process-global panic
        // hook, which left the alt screen and dropped raw mode before
        // `catch_unwind` recovered. Re-enter before drawing, or every frame from
        // here on paints onto the normal screen with input handling broken.
        if app.take_terminal_lost() {
            resume_terminal(terminal)?;
            terminal.clear()?;
            terminal_title_sent = None;
        }
        let now = Instant::now();
        let deferred_frame =
            defer_message_frame(woken_by_messages && !app.should_quit, last_draw, now);
        if deferred_frame.is_none() {
            crate::cursor_backend::draw_frame(terminal, |f| ui::draw(f, app))?;
            last_draw = Some(now);
            sync_title(
                terminal.backend_mut(),
                app.terminal_title(),
                &mut terminal_title_sent,
            )?;
            sync_cursor(
                terminal.backend_mut(),
                app.editor.is_insert(),
                &mut cursor_insert,
            )?;
        }
        if app.should_quit {
            app.shutdown_messaging().await;
            if app.handoff_exit {
                break;
            }
            // Reap a turn cancelled on the quit path first: awaiting the aborted
            // task drops its future and releases the agent lock, so the save
            // below can't lose the race and skip. Then the final save — a
            // backstop for the idle-quit paths too, with no "next autosave" to
            // catch it since the app is exiting. `save_session`'s own
            // is-there-anything-to-save guard keeps this a no-op when there's
            // nothing new, so it never double-writes.
            app.reap_cancelled_turn().await;
            app.autosave();
            // The final save landed off-thread; don't exit before it does.
            // `await_saves` waits for the in-flight save (and any newer
            // snapshot coalesced behind it) to reach disk.
            app.await_saves().await;
            // `session_end` hooks run after the final save, awaited (each
            // hook's timeout bounds the wait) — a spawned task would be
            // killed when the process exits right after.
            app.run_session_end_hooks().await;
            break;
        }

        let wake =
            wait_for_event_until(app, &mut events, &mut rx, &mut ticker, deferred_frame).await;
        woken_by_messages = matches!(wake, Wake::Message(_));
        match wake {
            Wake::Input(maybe_ev) => match maybe_ev {
                Some(Ok(Event::Key(key))) => match app.on_key(key) {
                    // Leaving the alt screen for `$EDITOR` resets the cursor
                    // shape; forget ours so the next frame asks for it again.
                    Action::OpenEditor => {
                        open_in_editor(app, terminal)?;
                        cursor_insert = None;
                        terminal_title_sent = None;
                    }
                    Action::OpenFile(path) => {
                        open_file_in_editor(app, terminal, &path)?;
                        cursor_insert = None;
                        terminal_title_sent = None;
                    }
                    Action::Redraw => terminal.clear()?,
                    Action::None => {}
                },
                Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                Some(Ok(Event::Paste(text))) => app.on_paste(&text),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            Wake::Message(msg) => {
                app.on_turn_msg(msg);
                app.commit_pending_update().await;
                // Drain any further messages that arrived in the same burst so
                // fast-streaming endpoints don't cause 100+ full redraws/sec —
                // all buffered tokens are folded into state before the next draw.
                while let Ok(msg) = rx.try_recv() {
                    app.on_turn_msg(msg);
                    app.commit_pending_update().await;
                }
            }
            Wake::Animation => {}
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

    match status {
        Ok(_) => {
            if let Ok(text) = std::fs::read_to_string(&path) {
                app.editor.set_content(&draft_from_editor(&text));
            }
        }
        Err(e) => app.system(format!("editor failed: {e}")),
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
        Ok(_) => app.system(edited_file_message(path, app.unix_style_paths())),
        Err(e) => app.system(format!("editor failed: {e}")),
    }
    Ok(())
}

fn edited_file_message(path: &std::path::Path, unix_style_paths: bool) -> String {
    format!(
        "edited {}",
        hrdr_tools::display_path(path, unix_style_paths)
    )
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
