//! What the spawning tests need from the machine — a shell, a Python — and the
//! one probe the tree-kill tests share.
//!
//! Every resolver here skips locally and fails on CI (see
//! [`hrdr_test_support::skip_for_want_of`]), so a runner missing a prerequisite
//! cannot report a test that exercised nothing as passed.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::Shell;

/// The shell hrdr itself would run commands through on this machine.
///
/// Tests take this rather than naming `Shell::Bash`: on Windows `bash` on `PATH`
/// is usually the WSL launcher, which cannot run anything without a distro, while
/// Git for Windows' `sh` beside it is exactly what a real session there detects.
pub(crate) fn shell() -> Option<Shell> {
    let found = Shell::detect();
    if hrdr_test_support::skip_for_want_of("a shell (bash or sh)", found.is_some()) {
        return None;
    }
    found
}

/// A working Python 3 interpreter, for the mock MCP and LSP servers.
///
/// Probed by running it, not by `which`: on Windows `python3` can resolve to the
/// Microsoft Store alias, which exists on `PATH` and runs nothing.
pub(crate) fn python() -> Option<&'static str> {
    let found = ["python3", "python"].into_iter().find(|exe| {
        std::process::Command::new(exe)
            .arg("--version")
            .output()
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).starts_with("Python 3")
            })
    });
    if hrdr_test_support::skip_for_want_of("a Python 3 interpreter", found.is_some()) {
        return None;
    }
    found
}

/// How long a test grandchild sleeps before it proves it is still alive.
const GRANDCHILD_SECS: u64 = 2;

/// Headroom past [`GRANDCHILD_SECS`] for a slow runner to start the grandchild:
/// process creation under Git Bash on Windows costs a visible fraction of a
/// second per fork.
const GRANDCHILD_STARTUP_SLACK: Duration = Duration::from_secs(3);

/// A shell fragment that backgrounds a grandchild which, if nothing kills it,
/// creates `marker` after [`GRANDCHILD_SECS`].
///
/// The marker is the liveness probe, and it is the portable one: a pid from
/// `$!` is an MSYS pid under Git Bash, which no Windows API can look up. A test
/// asserting a grandchild was *killed* passes just as well if this fragment never
/// ran at all, so `backgrounded_child_survives_a_successful_run` asserts the
/// marker *does* appear when nothing kills it — the control that keeps the
/// others honest on every platform.
pub(crate) fn backgrounded_grandchild(shell: Shell, marker: &Path) -> String {
    format!(
        "(sleep {GRANDCHILD_SECS} && touch {m}) </dev/null >/dev/null 2>&1 &",
        m = shell.quote(&marker.to_string_lossy()),
    )
}

/// Wait until a grandchild started at `started` would have created `marker`,
/// then report whether it did.
///
/// Waiting past the deadline is what makes the answer mean something: checking
/// sooner reads "not yet" as "never", and passes whether or not anything was
/// killed.
pub(crate) async fn grandchild_finished(marker: &Path, started: Instant) -> bool {
    let deadline = started + Duration::from_secs(GRANDCHILD_SECS) + GRANDCHILD_STARTUP_SLACK;
    while Instant::now() < deadline {
        if marker.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    marker.exists()
}
