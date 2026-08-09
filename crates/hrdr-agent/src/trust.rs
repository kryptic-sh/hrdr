//! Trust on first use, per working directory.
//!
//! A project's `AGENTS.md` and its `.hrdr/commands` are instructions that reach the
//! model, and they arrive from a checkout the user may have done nothing but
//! clone. The question "should this directory's files steer my agent?" has one
//! honest answer and it is not a heuristic over the file contents: it is the
//! user's. So hrdr asks, once per directory, and remembers the yes.
//!
//! **Only the yes.** The store is a list of trusted directories, so declining
//! leaves no record and the question comes back next time — a standing reminder
//! rather than a decision that quietly sticks. Declining is not a dead end: the
//! session opens in [`SandboxMode::Jail`](hrdr_tools::SandboxMode::Jail), which
//! reads the tree without running it.
//!
//! **Exact paths, never ancestors.** Trusting `~/Projects` must not trust
//! `~/Projects/something-i-just-cloned`, which is precisely the directory whose
//! instructions have never been read by anyone. Every directory is its own
//! answer.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The store, under the XDG cache dir: one directory per line.
///
/// Cache rather than config because losing it is safe by construction — a
/// cleared cache means the question is asked again, which is the same answer
/// hrdr gives a directory it has never seen. Nothing here is worth restoring
/// from a backup, and a stale entry is the only outcome worth avoiding.
const TRUSTED_DIRS_FILE: &str = "trusted-dirs";

/// Path to the trusted-directory store, or `None` when no cache dir resolves.
pub fn trusted_dirs_path() -> Option<PathBuf> {
    hjkl_xdg::cache_dir("hrdr")
        .ok()
        .map(|d| d.join(TRUSTED_DIRS_FILE))
}

/// The stored form of `dir`: symlinks resolved, so one directory has one
/// spelling and a symlink into a trusted tree does not inherit its answer.
///
/// Falls back to the path as given when it cannot be canonicalized (it does not
/// exist yet, or is unreadable) — a path that cannot be resolved cannot match a
/// stored entry either, so the failure direction is "ask again", not "trust".
fn key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Every directory in the store. Missing file, unreadable file and empty file
/// are all the same answer: nothing is trusted yet.
fn stored() -> Vec<String> {
    let Some(p) = trusted_dirs_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Has the user vouched for exactly this directory?
///
/// Exact match on the canonical path. A parent being trusted says nothing about
/// a child, by design — see the module note.
pub fn is_trusted(dir: &Path) -> bool {
    let k = key(dir);
    stored().contains(&k)
}

/// Record `dir` as trusted. Idempotent: trusting an already-trusted directory
/// rewrites nothing.
///
/// Appends rather than rewriting, so a second hrdr session answering the same
/// question at the same moment cannot lose the first one's entry — an `O_APPEND`
/// write of one short line is not interleaved with another. The file is created
/// owner-only: it decides which directories may steer this user's agent.
pub fn trust(dir: &Path) -> Result<()> {
    let path = trusted_dirs_path().context("no cache directory to store trusted directories in")?;
    let k = key(dir);
    if stored().contains(&k) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = hrdr_llm::owner_only_options()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{k}").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// What the user chose when asked about a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustChoice {
    /// Open normally; the directory's instructions are loaded and its tools run.
    Trusted,
    /// Open jailed: read the tree, run nothing from it. Not recorded.
    Untrusted,
    /// Do not open at all.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests below. `XDG_CACHE_HOME` is process-global, so two
    /// of these running at once would read each other's store — and the failure
    /// is a *passing* test reading a neighbour's entry, not an obvious crash.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A private store for one test: its own `$XDG_CACHE_HOME`, held until the
    /// returned guard drops.
    ///
    /// Returns the lock alongside the directory so a caller cannot take one
    /// without the other — binding it to `_` would release the lock immediately
    /// and reintroduce the race, so every caller binds both.
    fn private_store() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = tempfile::tempdir().unwrap();
        // SAFETY: the lock above makes this the only thread touching the
        // environment, which is the condition `set_var` needs.
        unsafe { std::env::set_var("XDG_CACHE_HOME", d.path()) };
        (d, lock)
    }

    #[test]
    fn an_unknown_directory_is_not_trusted_and_trusting_it_sticks() {
        let (_store, _lock) = private_store();
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_trusted(dir.path()), "nothing is trusted to begin with");
        trust(dir.path()).unwrap();
        assert!(is_trusted(dir.path()));
    }

    /// The whole reason the match is exact: a directory cloned into a trusted
    /// parent is exactly the one nobody has read.
    #[test]
    fn trusting_a_parent_does_not_trust_a_child() {
        let (_store, _lock) = private_store();
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("just-cloned");
        std::fs::create_dir(&child).unwrap();

        trust(parent.path()).unwrap();

        assert!(is_trusted(parent.path()));
        assert!(
            !is_trusted(&child),
            "a child of a trusted directory must be answered on its own"
        );
    }

    /// Trusting twice must not append twice — the store is a set.
    #[test]
    fn trusting_twice_records_one_entry() {
        let (_store, _lock) = private_store();
        let dir = tempfile::tempdir().unwrap();
        trust(dir.path()).unwrap();
        trust(dir.path()).unwrap();
        let text = std::fs::read_to_string(trusted_dirs_path().unwrap()).unwrap();
        assert_eq!(text.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    /// A symlink into a trusted tree is not a way to borrow its answer: both
    /// spellings resolve to the same canonical path, so the answer is the same
    /// one, not a second one.
    #[cfg(unix)]
    #[test]
    fn a_symlink_resolves_to_the_directory_it_points_at() {
        let (_store, _lock) = private_store();
        let real = tempfile::tempdir().unwrap();
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        trust(&link).unwrap();
        assert!(
            is_trusted(real.path()),
            "the link was stored as its target, so the target reads as trusted"
        );
    }

    /// No store yet is the same answer as an empty one, not an error.
    #[test]
    fn a_missing_store_trusts_nothing() {
        let (_store, _lock) = private_store();
        let dir = tempfile::tempdir().unwrap();
        assert!(trusted_dirs_path().is_some_and(|p| !p.exists()));
        assert!(!is_trusted(dir.path()));
    }
}
