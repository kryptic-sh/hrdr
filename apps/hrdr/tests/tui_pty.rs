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
//! The agent never talks to anything: the endpoint points at a closed port, so the
//! health probe fails and the TUI carries on — which is itself worth knowing.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// How long to wait for the first frame. Generous: a cold CI runner is slow, and
/// a flaky timeout in this test is worse than a slow one.
const BOOT: Duration = Duration::from_secs(60);
/// How long to wait for the process to leave after being told to quit.
const EXIT: Duration = Duration::from_secs(30);

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

/// Run the TUI in a pty; return everything it painted, plus how it exited.
fn run_tui(keys: &str) -> (String, portable_pty::ExitStatus) {
    let home = tempfile::tempdir().expect("temp home");
    let project = tempfile::tempdir().expect("temp project");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hrdr"));
    // A closed port: the health probe fails, and the TUI must come up anyway.
    cmd.args([
        "--no-auto-resume",
        "--no-bell",
        "--base-url",
        "http://127.0.0.1:1",
        "--model",
        "pty-smoke",
    ]);
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
    // Whatever the developer has exported must not reach the child.
    for key in [
        "HRDR_BASE_URL",
        "HRDR_MODEL",
        "HRDR_PROVIDER",
        "HRDR_API_KEY",
    ] {
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
        let seen = snapshot();
        assert!(
            start.elapsed() < BOOT,
            "the TUI never painted a frame in {BOOT:?} ({} bytes read). Screen so far:\n{seen}",
            seen.len()
        );
        assert!(
            child.try_wait().expect("poll child").is_none(),
            "hrdr exited before painting anything. Screen:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    {
        let mut w = grab_writer(&writer);
        w.write_all(keys.as_bytes()).expect("write keys");
        w.flush().expect("flush keys");
    }

    let start = Instant::now();
    let status = loop {
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
    };

    let out = snapshot();
    (out, status)
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
    let (screen, status) = run_tui("quit\r");

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
/// above points it at a closed port, so this asserts what a user with a wrong
/// `--base-url` sees: a running TUI that tells them, rather than a binary that dies
/// on startup with a connection error.
#[test]
fn an_unreachable_endpoint_does_not_take_the_tui_down() {
    let (screen, status) = run_tui("quit\r");
    assert!(status.success(), "Screen:\n{screen}");
    assert!(
        screen.contains("pty-smoke"),
        "the TUI must come up and stay up with a dead endpoint. Screen:\n{screen}"
    );
}
