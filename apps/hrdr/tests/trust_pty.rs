//! The trust question, driven for real: a pty, the built binary, and keystrokes.
//!
//! This is a security gate whose whole value is that a person answered it, so the
//! parts worth testing are the parts a unit test cannot reach — that it renders at
//! all in a terminal, that the selection **starts on cancel**, that `j`/`k` and the
//! arrows move it, and that a stray Enter therefore opens nothing. The decision
//! table itself is covered in `main.rs`; this covers the keyboard.
//!
//! Nothing here talks to a model: the store is answered (or not) and the process
//! either exits or goes on to fail at a dead endpoint, which is enough to observe.

// Its own test binary, so it links the sandbox ctor itself — see `tui_pty.rs`.
extern crate hrdr_test_support;

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{drain_pty, pty_text};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Generous: a cold runner is slow, and a flaky timeout here is worse than a slow
/// test.
const BOOT: Duration = Duration::from_secs(60);

fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 90,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

/// Skip for want of a pty — never in CI, where a missing pty is a broken
/// environment rather than the local Landlock sandbox blocking `/dev/ptmx`.
fn skip_for_want_of_a_pty() -> bool {
    if pty_available() || std::env::var_os("CI").is_some() {
        return false;
    }
    eprintln!("skipping: no pty available (a Landlock sandbox blocks /dev/ptmx)");
    true
}

/// Strip ANSI escapes so assertions read the text, not the codes that placed it.
fn visible(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
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

/// One run: an untrusted project, the keys to answer with, and what came back.
struct Asked {
    screen: String,
    /// The bytes as they went to the terminal, escape sequences intact — what a
    /// colour assertion has to read.
    raw: String,
    /// Everything painted since the last full-screen clear — i.e. what the user
    /// is actually looking at, as opposed to every frame ever drawn.
    last_frame: String,
    /// The trusted-dirs store's contents afterwards, if it was written at all.
    store: Option<String>,
    /// The process left on its own after the keys were sent, rather than having
    /// to be killed. Cancel is the only answer that exits; every other answer
    /// goes on to open a session. Without this an assertion on the store alone
    /// passes for "cancelled" and for "sitting on the confirmation" equally.
    exited_itself: bool,
}

/// Spawn hrdr in a fresh (therefore untrusted) directory, wait for the question,
/// send `keys`, and collect the screen and the store.
fn ask(keys: &[&str]) -> Asked {
    ask_themed(keys, None)
}

/// As [`ask`], with `theme` written into the config the child reads.
fn ask_themed(keys: &[&str], theme: Option<&str>) -> Asked {
    let home = tempfile::tempdir().expect("temp home");
    let project = tempfile::tempdir().expect("temp project");
    let runtime = tempfile::tempdir().expect("temp runtime");

    // A provider on a closed port: if the session does open, it fails there
    // rather than reaching anything real.
    let config_dir = home.path().join("hrdr");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let theme_line = theme
        .map(|t| format!("theme = \"{t}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "{theme_line}model = \"dead://trust\"\n\n[providers.dead]\nbase_url = \"http://127.0.0.1:1/v1\"\n"
        ),
    )
    .expect("write config.toml");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
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
        // Deliberately NOT pre-trusted: this test is about being asked.
        ("XDG_CACHE_HOME", home.path()),
        ("XDG_RUNTIME_DIR", runtime.path()),
    ] {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    for key in ["HRDR_MODEL", "HRDR_API_KEY", "RUST_LOG"] {
        cmd.env_remove(key);
    }
    // These tests assert on the escape codes the question is painted in, so
    // color must be on regardless of the ambient environment: a machine (or a
    // CI job) that exports NO_COLOR would otherwise strip the very output
    // under test.
    for key in ["NO_COLOR", "CLICOLOR"] {
        cmd.env_remove(key);
    }

    let mut child = pty.slave.spawn_command(cmd).expect("spawn hrdr");
    drop(pty.slave);
    let reader = pty.master.try_clone_reader().expect("pty reader");
    // Shared with the drainer: on Windows the child blocks on a cursor-position
    // query until the harness answers it, and the answer goes out this way.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pty.master.take_writer().expect("pty writer")));
    let screen = drain_pty(reader, Arc::clone(&writer));

    // Wait for the question to paint before answering it.
    let deadline = Instant::now() + BOOT;
    while Instant::now() < deadline {
        let seen = visible(&pty_text(&screen));
        if seen.contains("has not been opened in this directory before") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    for k in keys {
        // A beat between keys: the menu redraws on a timer, and a burst would
        // test the input buffer rather than the menu.
        std::thread::sleep(Duration::from_millis(150));
        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(k.as_bytes());
        let _ = w.flush();
    }
    // Give it a moment to leave if the answer was cancel.
    let mut exited_itself = false;
    let leave_by = Instant::now() + Duration::from_secs(5);
    while Instant::now() < leave_by {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited_itself = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    // Any other answer is sitting in a TUI against a dead endpoint.
    let _ = child.kill();
    let _ = child.wait();

    let store = std::fs::read_to_string(home.path().join("hrdr").join("trusted-dirs")).ok();
    let raw = pty_text(&screen);
    // Each frame begins by homing the cursor and clearing, so the text after the
    // last clear is the frame on screen.
    let last_frame = visible(raw.rsplit("\x1b[2J").next().unwrap_or(&raw));
    let out = visible(&raw);
    Asked {
        screen: out,
        raw,
        last_frame,
        store,
        exited_itself,
    }
}

/// The question is asked, and it is asked as a menu — the options are on screen
/// with the keys that move between them.
#[test]
fn an_unknown_directory_is_asked_about_as_a_menu() {
    if skip_for_want_of_a_pty() {
        return;
    }
    // Esc: answer nothing, so this test only observes what was drawn.
    let asked = ask(&["\x1b"]);
    assert!(
        asked
            .screen
            .contains("has not been opened in this directory before"),
        "the question is put: {}",
        asked.screen
    );
    for option in ["trust", "untrusted", "cancel"] {
        assert!(
            asked.screen.contains(option),
            "option `{option}` is offered: {}",
            asked.screen
        );
    }
    assert!(
        asked.screen.contains("j/k to move"),
        "the keys are stated: {}",
        asked.screen
    );
    assert!(
        asked.store.is_none(),
        "escaping records nothing: {:?}",
        asked.store
    );
}

/// The selection starts on cancel, so the reflex answer — Enter, without reading
/// — opens nothing and records nothing. This is the whole reason for the default.
#[test]
fn enter_on_the_default_selection_trusts_nothing() {
    if skip_for_want_of_a_pty() {
        return;
    }
    let asked = ask(&["\r"]);
    assert!(
        asked.store.is_none(),
        "a bare Enter must not have trusted the directory: {:?}",
        asked.store
    );
    // The load-bearing half: cancel EXITS. Asserting only on the store would
    // pass just as happily if Enter had selected trust and left the process
    // waiting on the confirmation, which is the opposite of this guarantee.
    assert!(
        asked.exited_itself,
        "cancel opens nothing, so the process leaves: {}",
        asked.screen
    );
}

/// `k` twice walks cancel → untrusted → trust, Enter opens the confirmation, and
/// `k` once more selects "yes, I'm sure" over the default "no". Only then is
/// anything written.
#[test]
fn walking_up_to_trust_and_confirming_records_the_directory() {
    if skip_for_want_of_a_pty() {
        return;
    }
    let asked = ask(&["k", "k", "\r", "k", "\r"]);
    let store = asked
        .store
        .unwrap_or_else(|| panic!("the directory should be recorded: {}", asked.screen));
    assert_eq!(
        store.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "one directory, one line: {store:?}"
    );
}

/// The confirmation defaults to "no", and taking that default goes back to the
/// first question rather than trusting.
#[test]
fn accepting_the_confirmations_default_records_nothing() {
    if skip_for_want_of_a_pty() {
        return;
    }
    // Up to `trust`, Enter, then Enter again on the confirmation's default.
    let asked = ask(&["k", "k", "\r", "\r", "\x1b"]);
    assert!(
        asked.store.is_none(),
        "the confirmation's default must not trust: {:?}",
        asked.store
    );
}

/// Declining the confirmation goes back to the first question **in place**. The
/// menu owns the screen and repaints it, so the header appears once — not once
/// per visit, scrolled under the last copy.
#[test]
fn going_back_from_the_confirmation_does_not_stack_a_second_header() {
    if skip_for_want_of_a_pty() {
        return;
    }
    // Up to `trust`, Enter to confirm, Enter again on "no, go back".
    let asked = ask(&["k", "k", "\r", "\r"]);
    assert_eq!(
        asked.last_frame.matches("has not been opened").count(),
        1,
        "one header on screen after going back, not a stack: {}",
        asked.last_frame
    );
    assert!(
        !asked.last_frame.contains("Trusting is remembered"),
        "the confirmation is replaced, not left above: {}",
        asked.last_frame
    );
    assert!(
        asked.store.is_none(),
        "going back records nothing: {:?}",
        asked.store
    );
}

/// The truecolour-foreground *parameters* for a palette role, or `None` when the
/// theme left it unset (nothing to assert on).
///
/// Deliberately not a whole terminated sequence: ratatui batches attributes, so
/// a foreground goes out as `…38;2;R;G;B;49m` with the background reset folded
/// into the same escape. Matching the parameters is what survives that batching
/// — and it is still specific enough that another theme's colour cannot satisfy
/// it.
fn fg_params(role: Option<(u8, u8, u8)>) -> Option<String> {
    role.map(|(r, g, b)| format!("38;2;{r};{g};{b}"))
}

/// The question is painted in the theme from **config.toml**, not only the one
/// `--theme` names — the config is where a user actually sets it.
///
/// The expected colours come from `ChatPalette` rather than being written out
/// here, so this tracks the theme file: it proves the config reached the screen,
/// and it cannot go stale when a palette is retuned.
#[test]
fn the_question_is_painted_in_the_theme_from_config() {
    if skip_for_want_of_a_pty() {
        return;
    }
    const THEME: &str = "dracula";
    let want = hrdr_app::ChatPalette::load(Some(THEME));
    let default = hrdr_app::ChatPalette::load(None);
    // The premise: these two themes must actually differ, or the assertion below
    // would pass without the config ever being read.
    assert_ne!(
        want.assistant, default.assistant,
        "{THEME} and the default theme must differ for this test to mean anything"
    );

    let asked = ask_themed(&["\x1b"], Some(THEME));

    let question = fg_params(want.assistant).expect("the theme sets `assistant`");
    assert!(
        asked.raw.contains(&question),
        "the question is drawn in {THEME}'s `assistant`"
    );
    let selection = fg_params(want.user).expect("the theme sets `user`");
    assert!(
        asked.raw.contains(&selection),
        "the selected row is drawn in {THEME}'s `user`"
    );
    assert!(
        !asked
            .raw
            .contains(&fg_params(default.assistant).expect("default sets `assistant`")),
        "the default theme's colours must not appear when config named another"
    );
}
