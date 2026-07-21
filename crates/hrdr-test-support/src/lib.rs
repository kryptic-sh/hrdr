//! A test can never touch the developer's real files — and no test has to ask.
//!
//! hrdr's user state lives under `$HOME` and the XDG roots: sessions and the
//! input history in `$XDG_DATA_HOME/hrdr`, `config.toml` / `auth.json` in
//! `$XDG_CONFIG_HOME/hrdr`, the models.dev catalog in
//! `$XDG_CACHE_HOME/hrdr`. The code that writes them is the code under test: run an
//! agent, submit a message, pick a model, and the *developer's* store is what moves.
//! It did, for a long time — the owner's machine ended up with 3,179 junk `tmp-*`
//! session directories and a `last_model.json` the suite had quietly rewritten.
//!
//! The previous fix was a helper called from three test constructors. That only ever
//! protects the tests that remember it, and a new test written by someone who has
//! read no documentation remembers nothing. So the isolation moved to where it cannot
//! be forgotten: a **life-before-main constructor**.
//!
//! [`sandbox_user_state`] is a `#[ctor]`. Any binary that links this crate runs it at
//! load, before `main`, before the test harness has spawned a single thread — which is
//! also the only moment `std::env::set_var` is sound (it is `unsafe` in edition 2024
//! and racy once threads exist). It points `$HOME` and all four XDG roots at a
//! throwaway per-process directory. Only test binaries link this crate (it is a
//! dev-dependency, `publish = false`), so nothing here can reach a release build.
//!
//! Every crate in the workspace pulls it in with a single line in its crate root:
//!
//! ```ignore
//! #[cfg(test)] extern crate hrdr_test_support; // sandboxes $HOME before any test runs
//! ```
//!
//! and every `tests/*.rs` integration binary — its own crate, which does *not* get the
//! library's `#[cfg(test)]` code — carries the same line without the `cfg`. A bare
//! dev-dependency is not enough: rustc never links a crate nothing references, and an
//! unlinked ctor never runs (proven, and the reason for the `extern crate` line). Two
//! automatic checks keep that from rotting:
//!
//! * `tests/every_test_binary_is_sandboxed.rs` fails if any crate or any `tests/*.rs`
//!   in the workspace is missing the line.
//! * `tests/leak_guard.rs` runs the whole suite in a subprocess whose `$HOME` and XDG
//!   roots are a sentinel directory, and fails — naming the files — if anything lands
//!   in it.

use std::path::{Path, PathBuf};

/// Env var the ctor exports: the sandbox root for this process.
const SANDBOX_ENV: &str = "HRDR_TEST_SANDBOX";
/// Env var the ctor exports: `$HOME` as it was *before* the sandbox replaced it, so a
/// test can assert it never wrote there.
const REAL_HOME_ENV: &str = "HRDR_TEST_REAL_HOME";

/// Point `$HOME` and every XDG root at a throwaway directory, before `main`.
///
/// Runs once per test binary, at load, single-threaded.
#[ctor::ctor]
fn sandbox_user_state() {
    let root = std::env::temp_dir().join(format!("hrdr-test-sandbox-{}", std::process::id()));
    // A process with this pid is long gone; start from nothing.
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    for dir in ["home", "data", "config", "state", "cache"] {
        std::fs::create_dir_all(root.join(dir)).expect("a test sandbox is creatable");
    }

    // SAFETY: a ctor runs before `main`, and therefore before the test harness has
    // created any thread. This is the one point in a test binary's life at which
    // `set_var` has no other thread to race.
    unsafe {
        if let Some(real) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            std::env::set_var(REAL_HOME_ENV, real);
        }
        std::env::set_var(SANDBOX_ENV, &root);
        std::env::set_var("HOME", &home);
        // `dirs::home_dir()` reads this one on Windows; hrdr's own `home_dir()` falls
        // back to it everywhere.
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
    }
}

/// Delete the sandbox when the test binary exits — /tmp does not need one of these per
/// run. Best-effort: a leaked thread still holding a file is not worth a failure.
#[ctor::dtor]
fn remove_sandbox() {
    if let Some(root) = std::env::var_os(SANDBOX_ENV) {
        let _ = std::fs::remove_dir_all(PathBuf::from(root));
    }
}

/// The throwaway root the ctor installed for this process.
///
/// Panics if the ctor did not run — which means this crate was not linked, and the test
/// asking is running against the developer's real files.
pub fn sandbox_root() -> PathBuf {
    std::env::var_os(SANDBOX_ENV).map(PathBuf::from).expect(
        "the hrdr-test-support ctor ran: `extern crate hrdr_test_support;` is in the crate root",
    )
}

/// The XDG roots as the ctor set them: `(data, config, cache)`.
///
/// What a test holding a *private* root of its own restores to when it releases it.
pub fn user_state_dirs() -> (PathBuf, PathBuf, PathBuf) {
    let root = sandbox_root();
    (root.join("data"), root.join("config"), root.join("cache"))
}

/// `$HOME` as it was before the sandbox replaced it — the developer's real home.
///
/// For tests that assert a write did *not* land there. `None` when the environment had
/// no `HOME` to begin with (a bare CI container).
pub fn real_home() -> Option<PathBuf> {
    std::env::var_os(REAL_HOME_ENV)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Assert that `path` is inside this process's sandbox, and not under the real home.
///
/// The shape of the promise this crate makes, spelled out for a test that wants to
/// check it: whatever hrdr resolved as a user-state path came from the sandbox.
pub fn assert_sandboxed(path: &Path) {
    let root = sandbox_root();
    assert!(
        path.starts_with(&root),
        "{} is outside the test sandbox {}",
        path.display(),
        root.display()
    );
    if let Some(real) = real_home() {
        assert!(
            !path.starts_with(&real),
            "{} is under the developer's real home {}",
            path.display(),
            real.display()
        );
    }
}
