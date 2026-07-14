//! One owner of the rule that a TEST never touches the developer's real files.
//!
//! hrdr's user state — sessions, the input history, the last-used model, the
//! `/model` usage counts, `auth.toml`, `config.toml`, the models.dev cache — lives
//! under `$HOME` / the XDG roots, and the code that writes it is the code under
//! test. A test that runs an agent, submits a message, or picks a model therefore
//! writes the *developer's* store unless something has moved those roots first.
//!
//! Nothing had. Running the suite minted real sessions in `~/.local/share/hrdr`,
//! appended to the real input history, and rewrote the real `last_model.json` —
//! and, three times during this refactor, a bug hid behind exactly that: a test
//! that "passed" because it was reading state a previous run had left behind.
//!
//! [`isolate_user_state`] moves `$HOME` and all three XDG roots to a throwaway
//! directory, ONCE per test process, before any of that can happen. It is called
//! from the few places every user-state-touching test funnels through (the TUI's
//! `Harness`, the agent's mock-server `test_cfg`, the app's `TestHost`), so a NEW
//! test inherits the isolation instead of having to remember it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The throwaway root this process's user state lives under. Created on first use,
/// and kept for the life of the process — tests that assert on what they wrote need
/// it to still be there.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Point `$HOME` and every XDG root at a throwaway directory, once per process.
///
/// Idempotent and cheap after the first call, so it can sit at the top of every test
/// harness constructor. Returns the root, for a test that wants to look at what it
/// wrote.
///
/// A test that needs a *private, empty* root of its own (asserting on the exact
/// contents of the session store, say) still overrides these vars for its own
/// duration — this only guarantees that the floor everything else lands on is never
/// the developer's home directory.
pub fn isolate_user_state() -> &'static Path {
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("hrdr-test-home-{}", std::process::id()));
        // A previous process with the same pid is long gone; start from nothing.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a test home is creatable");
        // SAFETY: this runs exactly once, at the first test harness constructed in the
        // process — the same trade the per-test guards already make. `set_var` is
        // process-global, and there is no other way to move roots that the code under
        // test reads through the environment.
        unsafe {
            std::env::set_var("HOME", &root);
            std::env::set_var("XDG_DATA_HOME", root.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
            std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        }
        root
    })
}

/// The XDG roots as [`isolate_user_state`] set them — what a per-test guard restores
/// to when it releases its own private root.
pub fn user_state_dirs() -> (PathBuf, PathBuf, PathBuf) {
    let root = isolate_user_state();
    (root.join("data"), root.join("config"), root.join("cache"))
}
