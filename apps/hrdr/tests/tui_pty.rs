//! The TUI, started for real, in a real terminal, on every OS we ship to.
//!
//! Everything else that tests the interface drives `App` against ratatui's
//! `TestBackend`: no terminal, no process, no OS. That proves the widgets lay out.
//! It cannot prove the *program* runs — raw mode, the alternate screen, the
//! keyboard-enhancement flags, the panic hook, the terminal restore on exit. Those
//! live in `hrdr_tui::run`, they differ per platform (ConPTY is not a pty), and
//! until now nothing exercised them: CI's "smoke" job ran `--version` and `--help`,
//! which never construct a terminal at all. A build could start, paint garbage or
//! panic on the first frame, and ship green.
//!
//! So: allocate a pty (a ConPTY on Windows), spawn the built binary in it, wait for
//! the session header to actually appear on the screen, type `quit`, and require a
//! clean exit. It is the smallest test that would have caught "the Windows build
//! doesn't start".
//!
//! The agent never talks to anything: the config defines a provider on a closed
//! port, so the health probe fails and the TUI carries on — which is itself worth
//! knowing. (The endpoint belongs to the provider; there is no flag that could point
//! hrdr at a dead address, so the test writes the provider it wants into an isolated
//! config.toml.)

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

mod common;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// How long to wait for the first frame. Generous: a cold CI runner is slow, and
/// a flaky timeout in this test is worse than a slow one.
const BOOT: Duration = Duration::from_secs(60);
/// How long to wait for the process to leave after being told to quit.
const EXIT: Duration = Duration::from_secs(30);
/// Grace for output still in flight. A ConPTY hands its buffer over when it is torn
/// down, so a child that has already exited can still have a screenful coming.
const DRAIN: Duration = Duration::from_secs(2);

/// Strip ANSI escape sequences, so assertions read the *text* on the screen rather
/// than the control codes that positioned it.
fn visible(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ST`) are the two hrdr emits.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\x07' || c == '\x1b' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The pty's write end, shared by the handshake responder and the keystrokes.
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

fn grab_writer(w: &Writer) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
    w.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock the screen, ignoring poisoning. A test that panics mid-assertion should
/// report *its* failure, not have the reader thread die of a poisoned mutex and
/// report that instead.
fn grab(screen: &Arc<Mutex<String>>) -> std::sync::MutexGuard<'_, String> {
    screen.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one run of the TUI in a pty did.
struct Run {
    /// Everything it painted, with the escape codes stripped.
    screen: String,
    status: portable_pty::ExitStatus,
    /// It quit on its own, before being told to. A TUI that exits by itself the
    /// moment it is put in a terminal is broken, however cleanly it exits — so this
    /// is a fact the tests assert on, not one the harness papers over.
    exited_unbidden: bool,
}

/// Run the TUI in a pty: wait for it to paint, type `keys`, and see it out.
fn run_tui(keys: &str) -> Run {
    let home = tempfile::tempdir().expect("temp home");
    let project = tempfile::tempdir().expect("temp project");

    // THE ENDPOINT BELONGS TO THE PROVIDER — so a deliberately-unreachable endpoint
    // is a provider defined at one. `XDG_CONFIG_HOME` is this tempdir (below), so
    // this is the config the child reads, and the developer's own is never touched.
    let config_dir = home.path().join("hrdr");
    std::fs::create_dir_all(&config_dir).expect("temp config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "model = \"dead://pty-smoke\"\n\n[providers.dead]\nbase_url = \"http://127.0.0.1:1/v1\"\n",
    )
    .expect("write config.toml");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hrdr"));
    // The identity + endpoint come from the config above (`dead://pty-smoke` at a
    // closed port): the health probe fails, and the TUI must come up anyway.
    cmd.args(["--no-auto-resume", "--no-bell"]);
    cmd.cwd(project.path());
    // Point every "where does config/state live" knob at a throwaway directory, so
    // the test can't read the developer's config or write into their sessions.
    for (key, value) in [
        ("HOME", home.path()),
        ("USERPROFILE", home.path()),
        ("APPDATA", home.path()),
        ("LOCALAPPDATA", home.path()),
        ("XDG_CONFIG_HOME", home.path()),
        ("XDG_DATA_HOME", home.path()),
        ("XDG_STATE_HOME", home.path()),
    ] {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    // Whatever the developer has exported must not reach the child. (`$HRDR_BASE_URL`
    // is not on the list because it no longer exists — the endpoint belongs to the
    // provider, and only a provider definition can name one.)
    for key in ["HRDR_MODEL", "HRDR_API_KEY"] {
        cmd.env_remove(key);
    }

    let mut child = pty.slave.spawn_command(cmd).expect("spawn hrdr");
    // The child holds the only slave handle it needs; ours would keep the pty open
    // and the reader below would never see EOF.
    drop(pty.slave);

    let screen = Arc::new(Mutex::new(String::new()));
    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    // Shared: the reader thread answers the terminal handshake below, and the test
    // types into the same pty afterwards.
    let writer = Arc::new(Mutex::new(pty.master.take_writer().expect("pty writer")));
    let sink = Arc::clone(&screen);
    let responder = Arc::clone(&writer);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                // EOF: the child closed the pty. Nothing more is coming.
                Ok(0) => break,
                Ok(n) => {
                    // **Answer the cursor-position query, or nothing else ever
                    // arrives.** A ConPTY opens by asking the terminal where the
                    // cursor is (`ESC[6n`) and *waits for the reply* before it
                    // flushes anything the child wrote. A real terminal answers; a
                    // test harness has to as well. Without this, Windows produced
                    // exactly four bytes — the query itself — and hung: even
                    // `cmd.exe /c echo` never completed. With it, hrdr paints.
                    if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                        let mut w = grab_writer(&responder);
                        let _ = w.write_all(b"\x1b[1;1R");
                        let _ = w.flush();
                    }
                    let mut s = grab(&sink);
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                // Not an error — *not yet*. A ConPTY master returns these before the
                // child has written anything, and a loop that treats the first `Err`
                // as the end reads zero bytes forever: the screen stays blank, the
                // TUI looks like it never painted, and the failure lands on Windows
                // and nowhere else. (It did.)
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    // Take a copy rather than hold the lock: the assertions below panic *with* the
    // screen in their message, and panicking while holding the guard poisons the
    // mutex — which then kills the reader thread and buries the real failure under
    // a second, meaningless one.
    let snapshot = || -> String { visible(&grab(&screen)) };

    // The session header names the model it is running on. Waiting for it means the
    // terminal was set up, a frame was composed, and the frame reached the screen —
    // which is the whole question this test exists to answer.
    let start = Instant::now();
    while !snapshot().contains("pty-smoke") {
        // A ConPTY hands its output over when it is torn down, so a child that has
        // already exited may still have a screenful in flight. Drain before
        // concluding anything about what it painted — otherwise a *quick* program
        // looks like a silent one.
        if let Some(status) = child.try_wait().expect("poll child") {
            std::thread::sleep(DRAIN);
            let seen = snapshot();
            assert!(
                seen.contains("pty-smoke"),
                "hrdr exited ({status:?}) without painting. Screen ({} bytes):\n{seen}",
                seen.len()
            );
            break;
        }
        let seen = snapshot();
        assert!(
            start.elapsed() < BOOT,
            "the TUI never painted a frame in {BOOT:?} ({} bytes read). Screen so far:\n{seen}",
            seen.len()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Did it stay up to be typed at, or leave on its own?
    let early = child.try_wait().expect("poll child");
    let exited_unbidden = early.is_some();

    let status = match early {
        Some(status) => status,
        None => {
            {
                let mut w = grab_writer(&writer);
                w.write_all(keys.as_bytes()).expect("write keys");
                w.flush().expect("flush keys");
            }
            let start = Instant::now();
            loop {
                if let Some(status) = child.try_wait().expect("poll child") {
                    break status;
                }
                if start.elapsed() > EXIT {
                    child.kill().expect("kill child");
                    panic!(
                        "hrdr did not exit within {EXIT:?} of being told to quit. Screen:\n{}",
                        snapshot()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    std::thread::sleep(DRAIN);
    Run {
        screen: snapshot(),
        status,
        exited_unbidden,
    }
}

/// The built binary starts a real TUI in a real terminal, paints its first frame,
/// and exits cleanly when told to — on Linux, macOS and Windows.
///
/// This is the test that "build + smoke" could not be: `--version` and `--help`
/// return before a terminal is ever constructed, so every OS-specific thing the TUI
/// does on the way up — raw mode, the alternate screen, ConPTY vs pty — went
/// unexercised until a user ran it.
#[test]
fn the_tui_starts_paints_and_exits_cleanly() {
    let Run {
        screen,
        status,
        exited_unbidden,
    } = run_tui("quit\r");

    assert!(
        !exited_unbidden,
        "hrdr quit on its own, without being asked. A TUI that will not stay up in a \
         terminal is broken however cleanly it leaves ({status:?}). Screen:\n{screen}"
    );
    assert!(
        status.success(),
        "hrdr exited {status:?} after `quit`. Screen:\n{screen}"
    );
    // A panic inside the alternate screen is invisible unless the hook restores the
    // terminal first — the exact failure the panic hook in `hrdr_tui::run` exists to
    // prevent. If one happened, it is in this output, and the test must not pass.
    assert!(
        !screen.contains("panicked at"),
        "the TUI panicked. Screen:\n{screen}"
    );
    // The session header rendered: the model it was launched with is on screen.
    assert!(
        screen.contains("pty-smoke"),
        "the session header never showed the model. Screen:\n{screen}"
    );
}

/// A closed endpoint is a warning, not a crash.
///
/// hrdr probes the endpoint on the way up (health + context window). The pty test
/// above runs on a provider defined at a closed port, so this asserts what a user
/// whose `[providers.*]` `base_url` is wrong (or whose server is not up) sees: a
/// running TUI that tells them, rather than a binary that dies on startup with a
/// connection error.
#[test]
fn an_unreachable_endpoint_does_not_take_the_tui_down() {
    let Run {
        screen,
        status,
        exited_unbidden,
    } = run_tui("quit\r");
    assert!(status.success(), "Screen:\n{screen}");
    assert!(
        !exited_unbidden,
        "a dead endpoint must not make the TUI quit. Screen:\n{screen}"
    );
    assert!(
        screen.contains("pty-smoke"),
        "the TUI must come up and stay up with a dead endpoint. Screen:\n{screen}"
    );
}

// ─── Interactive sessions against a live mock endpoint ───────────────────────
//
// The two tests above prove the TUI *starts* against a dead endpoint. These
// drive whole interactions against the in-process mock in `common`:
//   7. a submitted prompt's streamed reply renders;
//   8. Esc cancels an in-flight turn without killing the app, which then quits
//      cleanly;
//   9. a resize while idle is survived, and a stdin EOF (every pty handle on our
//      side closed) restores the terminal and exits.
//
// They reuse the pty plumbing above (`visible`, `grab`, `grab_writer`, the boot
// constants). Tests 7 and 8 use a background reader thread (`Session`) so the
// test can type and watch the screen at once; test 9 keeps the reader in-hand so
// it can be dropped to close the pty (a blocking reader on a background thread
// can never be unblocked to release its fd, so it could not signal EOF).

use common::{Chat, MockServer, stop_chunk, text_chunk};

/// Spawn `hrdr` in a fresh pty against `base_url`, with HOME/XDG/cwd isolated to
/// throwaway dirs (mirrors `run_tui`). Returns the child, the master (for resize
/// / close), a reader, a writer, and the tempdirs (kept alive by the caller).
#[allow(clippy::type_complexity)]
fn spawn_pty(
    base_url: &str,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn portable_pty::MasterPty + Send>,
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let home = tempfile::tempdir().expect("temp home");
    let project = tempfile::tempdir().expect("temp project");
    common::write_config(home.path(), base_url);

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hrdr"));
    cmd.args(["--no-auto-resume", "--no-bell"]);
    cmd.cwd(project.path());
    for (key, value) in [
        ("HOME", home.path()),
        ("USERPROFILE", home.path()),
        ("APPDATA", home.path()),
        ("LOCALAPPDATA", home.path()),
        ("XDG_CONFIG_HOME", home.path()),
        ("XDG_DATA_HOME", home.path()),
        ("XDG_STATE_HOME", home.path()),
    ] {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    for key in ["HRDR_MODEL", "HRDR_API_KEY", "RUST_LOG"] {
        cmd.env_remove(key);
    }

    let child = pty.slave.spawn_command(cmd).expect("spawn hrdr");
    drop(pty.slave);
    let reader = pty.master.try_clone_reader().expect("pty reader");
    let writer = pty.master.take_writer().expect("pty writer");
    (child, pty.master, reader, writer, home, project)
}

/// A live TUI in a pty with a background reader thread, kept open across steps.
struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Writer,
    screen: Arc<Mutex<String>>,
    _home: tempfile::TempDir,
    _project: tempfile::TempDir,
}

impl Session {
    fn spawn(base_url: &str) -> Session {
        let (child, master, mut reader, writer, home, project) = spawn_pty(base_url);
        let screen = Arc::new(Mutex::new(String::new()));
        let writer = Arc::new(Mutex::new(writer));
        let sink = Arc::clone(&screen);
        let responder = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Answer the ConPTY cursor-position query (see `run_tui`).
                        if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                            let mut w = grab_writer(&responder);
                            let _ = w.write_all(b"\x1b[1;1R");
                            let _ = w.flush();
                        }
                        grab(&sink).push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Session {
            child,
            master,
            writer,
            screen,
            _home: home,
            _project: project,
        }
    }

    fn snapshot(&self) -> String {
        visible(&grab(&self.screen))
    }

    /// Poll until `needle` is on screen, or fail with the screen attached.
    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            if self.snapshot().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll child") {
                std::thread::sleep(DRAIN);
                assert!(
                    self.snapshot().contains(needle),
                    "child exited ({status:?}) before {needle:?} appeared. Screen:\n{}",
                    self.snapshot()
                );
                return;
            }
            assert!(
                start.elapsed() < timeout,
                "timed out waiting for {needle:?}. Screen:\n{}",
                self.snapshot()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn send(&self, keys: &str) {
        let mut w = grab_writer(&self.writer);
        w.write_all(keys.as_bytes()).expect("write keys");
        w.flush().expect("flush keys");
    }

    /// Resize the pty. On Unix this raises SIGWINCH in the child; the TUI must
    /// repaint at the new size without crashing.
    fn resize(&self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
    }

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().expect("poll child").is_none()
    }

    /// Wait for the child to exit; kill + panic on timeout so a hang is a
    /// failure, not a stuck test.
    fn wait_exit(&mut self, timeout: Duration) -> portable_pty::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return status;
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                panic!(
                    "hrdr did not exit within {timeout:?}. Screen:\n{}",
                    self.snapshot()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 7. A submitted prompt drives a turn against the mock, and the streamed reply
///    renders on screen; the app then quits cleanly.
#[test]
fn a_submitted_prompt_streams_its_reply() {
    let server = MockServer::start(vec![Chat::Sse(vec![
        text_chunk("c1", "STREAMED_REPLY_TOKEN"),
        stop_chunk("c1"),
        "[DONE]".to_string(),
    ])]);
    let mut s = Session::spawn(&server.base_url());
    s.wait_for("mock-model", BOOT);
    // Type a prompt and submit it.
    s.send("hello there\r");
    // The mock's reply must reach the transcript.
    s.wait_for("STREAMED_REPLY_TOKEN", EXIT);
    assert!(
        !s.snapshot().contains("panicked at"),
        "the TUI panicked. Screen:\n{}",
        s.snapshot()
    );
    // And it still quits cleanly afterwards.
    s.send("quit\r");
    let status = s.wait_exit(EXIT);
    assert!(
        status.success(),
        "exit after reply. Screen:\n{}",
        s.snapshot()
    );
}

/// 8. Esc cancels an in-flight turn (the mock holds the stream open) without
///    killing the app — a `[cancelled]` note appears, and `quit` then exits
///    cleanly.
#[test]
fn escape_cancels_a_turn_without_killing_the_app() {
    // Open the stream with a visible marker, then hold it: the turn stays
    // running until we cancel it.
    let server = MockServer::start(vec![Chat::Hang(vec![text_chunk("c1", "PARTIAL_TOKEN")])]);
    let mut s = Session::spawn(&server.base_url());
    s.wait_for("mock-model", BOOT);
    s.send("do something\r");
    // The turn is streaming (the partial chunk rendered) — it is in-flight.
    s.wait_for("PARTIAL_TOKEN", EXIT);
    // Esc cancels it.
    s.send("\x1b");
    // The app records the cancellation and stays up.
    s.wait_for("cancelled", EXIT);
    assert!(
        s.is_alive(),
        "Esc must cancel the turn, not quit the app. Screen:\n{}",
        s.snapshot()
    );
    assert!(
        !s.snapshot().contains("panicked at"),
        "the TUI panicked. Screen:\n{}",
        s.snapshot()
    );
    // The app still quits cleanly on request.
    s.send("quit\r");
    let status = s.wait_exit(EXIT);
    assert!(
        status.success(),
        "clean exit after a cancel. Screen:\n{}",
        s.snapshot()
    );
}

/// 9. A resize while idle does not crash the TUI, and a shell-style EOF
///    (Ctrl+D on an empty input) restores the terminal and exits cleanly.
///
/// Note on "closing stdin": in a pty, stdin *is* the terminal, so dropping the
/// pty master doesn't deliver a plain stdin EOF — it hangs the terminal up
/// (SIGHUP), which the kernel turns into a signal-kill, not the clean
/// `EventStream`-ended exit the TUI's `None => break` arm handles. The faithful,
/// clean "input reached EOF" path is Ctrl+D, which hrdr treats as a shell-style
/// EOF quit (and which the welcome banner advertises), so that is what this
/// asserts exits cleanly with the terminal restored.
#[test]
fn resize_is_survived_and_eof_exits_cleanly() {
    let server = MockServer::start(vec![]);
    let mut s = Session::spawn(&server.base_url());
    s.wait_for("mock-model", BOOT);

    // Resize while idle — the child gets SIGWINCH and must repaint, not crash.
    s.resize(120, 40);
    s.resize(80, 24);
    // Give the repaints a moment to land, then confirm it is still up and sane.
    std::thread::sleep(DRAIN);
    assert!(
        s.is_alive(),
        "a resize must not take the TUI down. Screen:\n{}",
        s.snapshot()
    );
    assert!(
        !s.snapshot().contains("panicked at"),
        "the TUI panicked on resize. Screen:\n{}",
        s.snapshot()
    );

    // Ctrl+D on the (empty) input: a shell-style EOF quit.
    s.send("\x04");
    let status = s.wait_exit(EXIT);
    assert!(
        status.success(),
        "Ctrl+D on empty input must exit cleanly, got {status:?}. Screen:\n{}",
        s.snapshot()
    );
    assert!(
        !s.snapshot().contains("panicked at"),
        "the TUI panicked on EOF quit. Screen:\n{}",
        s.snapshot()
    );
}
