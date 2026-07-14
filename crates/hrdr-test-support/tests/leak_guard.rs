//! The check that would have caught this bug three years ago: run the WHOLE suite with
//! `$HOME` and every XDG root pointed at an empty sentinel directory, and fail if a
//! single byte lands in it.
//!
//! Layer 1 (the ctor in `hrdr-test-support`) makes a leak impossible for any test that
//! links it. This is the proof that nothing slipped the net — a doctest, a `tests/*.rs`
//! that lost its `extern crate` line, a future crate added to the workspace without the
//! dev-dependency. Whatever wrote to the sentinel is named in the failure.
//!
//! It shells out to `cargo test`, so it must not run itself: `HRDR_LEAK_GUARD` gates it,
//! and the child has that variable removed. Locally:
//!
//! ```sh
//! HRDR_LEAK_GUARD=1 cargo test -p hrdr-test-support --test leak_guard -- --nocapture
//! ```
//!
//! CI runs exactly that, on every PR (the `leak-guard` job).

extern crate hrdr_test_support;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Set this to arm the guard. Absent (a plain `cargo test`), the test is a no-op — the
/// suite must not spend a full second build inside itself, and must never recurse.
const GATE: &str = "HRDR_LEAK_GUARD";

#[test]
fn no_test_in_the_workspace_writes_real_user_state() {
    if std::env::var_os(GATE).is_none() {
        eprintln!(
            "leak guard idle: set {GATE}=1 to run the workspace suite against a sentinel $HOME"
        );
        return;
    }

    let root = workspace_root();
    let sentinel = std::env::temp_dir().join(format!("hrdr-leak-sentinel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sentinel);
    std::fs::create_dir_all(&sentinel).expect("a sentinel dir is creatable");

    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(&root)
        .args(["test", "--workspace", "--all-features", "--no-fail-fast"])
        // The child IS the suite. It must not arm the guard again.
        .env_remove(GATE)
        // Every root a test could resolve user state through, aimed at the sentinel. A
        // test binary's ctor overrides all of these with its own /tmp sandbox — which is
        // the point: what reaches the sentinel is what the ctor did not cover.
        .env("HOME", &sentinel)
        .env("USERPROFILE", &sentinel)
        .env("XDG_CONFIG_HOME", &sentinel)
        .env("XDG_DATA_HOME", &sentinel)
        .env("XDG_STATE_HOME", &sentinel)
        .env("XDG_CACHE_HOME", &sentinel)
        // ...but cargo and rustup resolve THEIR homes from `$HOME` too, and would
        // re-download the registry and the toolchain into the sentinel — a leak that is
        // not hrdr's. Pin them to the real ones.
        .env("CARGO_HOME", tool_home("CARGO_HOME", ".cargo"))
        .env("RUSTUP_HOME", tool_home("RUSTUP_HOME", ".rustup"));

    let status = cmd.status().expect("cargo test is runnable");

    let written = files_under(&sentinel);
    let _ = std::fs::remove_dir_all(&sentinel);

    assert!(
        written.is_empty(),
        "A TEST WROTE THE USER'S REAL STATE.\n\
         The suite ran with $HOME and every XDG root pointed at {}, and these files \
         appeared in it — on a developer's machine they would have landed in their real \
         home:\n{}\n\n\
         Fix: the test's binary is missing the sandbox ctor. A crate needs\n    \
         #[cfg(test)] extern crate hrdr_test_support;\nin its crate root (plus \
         `hrdr-test-support.workspace = true` under [dev-dependencies]); a tests/*.rs \
         binary needs the same line without the `cfg`. A doctest cannot link it at all — \
         a doctest must not touch user state.",
        sentinel.display(),
        written
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Checked after the leak assertion on purpose: a failing suite must not hide a leak.
    assert!(
        status.success(),
        "the workspace suite failed under the guard"
    );
}

/// The workspace root: this crate is `<root>/crates/hrdr-test-support`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> sits two levels under the workspace root")
        .to_path_buf()
}

/// `$CARGO_HOME` / `$RUSTUP_HOME` as they really are — from the environment, else from
/// the developer's real home, which the ctor stashed before it moved `$HOME`.
fn tool_home(var: &str, fallback: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    hrdr_test_support::real_home()
        .map(|h| h.join(fallback))
        .unwrap_or_else(|| PathBuf::from(fallback))
}

/// Every path under `dir`, recursively — files *and* directories. An empty
/// `sessions/tmp-1234/` the suite left behind is a leak too: 3,179 of those is how this
/// was noticed.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}
