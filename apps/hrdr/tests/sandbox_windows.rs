//! The Windows Low-integrity confinement wrapper, end to end.
//!
//! `hrdr __sandbox-exec -- <program> <args…>` lowers its own token to Low
//! integrity and runs the rest of the argv, so every descendant inherits the
//! confinement. That is the whole of the Windows backend's mechanism, and it is
//! only observable by running the real binary — which is why this is an
//! integration test here rather than a unit test in `hrdr-tools`.
//!
//! The first attempt WAS a unit test there, and it wedged the Windows CI job for
//! 37 minutes: the backend re-execs `std::env::current_exe()`, which inside a
//! `hrdr-tools` test binary is the test harness, so the spawn handed
//! `__sandbox-exec -- …` to libtest as filter arguments. `CARGO_BIN_EXE_hrdr` is
//! the fix — it names the actual wrapper.

// Inner attribute: must precede every item in the file, `extern crate` included.
#![cfg(windows)]

// Every test in this crate runs with `$HOME` and the XDG roots pointed at a
// throwaway directory; see the note in `main.rs`.
extern crate hrdr_test_support;

use std::process::Command;

/// Run `hrdr __sandbox-exec -- cmd /c <command>` and hand back (success, stdout).
///
/// `cmd.exe` rather than a shell hrdr detects: it is present on every Windows
/// install, so the test exercises the wrapper rather than the shell probe.
fn under_low_integrity(command: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_hrdr"))
        .args(["__sandbox-exec", "--", "cmd", "/c", command])
        .output()
        .expect("spawning the hrdr sandbox wrapper");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A write is refused wherever it points. `%TEMP%` is labelled Medium like
/// everything else the user owns, so a Low-integrity child cannot write there —
/// which is the whole of what `SandboxMode::Read` promises.
#[test]
fn a_write_is_denied_under_the_low_integrity_wrapper() {
    if hrdr_test_support::skip_for_want_of(
        "cmd.exe",
        std::path::Path::new(r"C:\Windows\System32\cmd.exe").exists(),
    ) {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("escaped.txt");

    let (ok, _) = under_low_integrity(&format!("echo x > {}", target.display()));

    assert!(!ok, "the write was allowed under Low integrity");
    assert!(
        !target.exists(),
        "the write landed anyway at {}",
        target.display()
    );
}

/// …and reads are untouched. Mandatory Integrity Control's default policy is
/// NO_WRITE_UP only, so a Low-integrity process still reads Medium objects. A
/// backend that blocked reads too would be enforcing `jail`, not `read`.
#[test]
fn a_read_still_succeeds_under_the_low_integrity_wrapper() {
    if hrdr_test_support::skip_for_want_of(
        "cmd.exe",
        std::path::Path::new(r"C:\Windows\System32\cmd.exe").exists(),
    ) {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let readable = dir.path().join("readable.txt");
    std::fs::write(&readable, "visible").expect("seed the file");

    let (ok, stdout) = under_low_integrity(&format!("type {}", readable.display()));

    assert!(ok, "Read mode must not confine reads");
    assert!(
        stdout.contains("visible"),
        "the file's contents did not come back: {stdout:?}"
    );
}

/// The child's exit code comes back whole. The `shell` tool reports it to the
/// model, and a wrapper that turned every failure into `1` said `cargo test`'s
/// 101 and grep's "no match" 1 were the same thing.
#[test]
fn the_wrapper_propagates_the_childs_exit_code() {
    if hrdr_test_support::skip_for_want_of(
        "cmd.exe",
        std::path::Path::new(r"C:\Windows\System32\cmd.exe").exists(),
    ) {
        return;
    }
    let status = Command::new(env!("CARGO_BIN_EXE_hrdr"))
        .args(["__sandbox-exec", "--", "cmd", "/c", "exit 101"])
        .status()
        .expect("spawning the hrdr sandbox wrapper");
    assert_eq!(status.code(), Some(101));
}

/// The wrapper must not run the command when it cannot confine it. Given no
/// program after `--` there is nothing to run, and it has to fail rather than
/// fall through into an ordinary hrdr session.
#[test]
fn the_wrapper_refuses_an_empty_command() {
    let out = Command::new(env!("CARGO_BIN_EXE_hrdr"))
        .args(["__sandbox-exec", "--"])
        .output()
        .expect("spawning the hrdr sandbox wrapper");
    assert!(
        !out.status.success(),
        "an empty wrapper invocation must fail"
    );
}
