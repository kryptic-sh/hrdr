//! Path helpers shared across hrdr's on-disk state (sessions, per-project
//! memory): all of them partition by working directory using the same slug, so
//! they must agree on how it's computed. Plus the one display helper —
//! [`display_dir`] — that both the agent (command sources) and the frontends
//! (chrome, pickers) render paths with, so they never disagree about where a
//! `~` goes.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

/// The shared flattening core behind every slug: trim, keep only alphanumerics
/// (everything else becomes `-`), collapse runs of separators, and lowercase.
/// `cwd_slug` and the sub-agent transcript ids both need the same
/// "a label becomes a safe file-name component" step, and both must agree on it.
pub(crate) fn flatten_slug(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Slug for a working directory — the per-cwd subdirectory name. The full path
/// is flattened (e.g. `/home/me/Projects/foo` → `home-me-projects-foo`). A hash
/// of the original path is appended to avoid collisions between distinct paths
/// that map to the same slug (e.g. `foo-bar` vs `foo_bar`).
pub fn cwd_slug(cwd: &str) -> String {
    let s = flatten_slug(cwd);
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    let suffix = format!("-{:016x}", hasher.finish());
    if s.is_empty() {
        format!("root{suffix}")
    } else {
        format!("{s}{suffix}")
    }
}

/// Display form of `dir`, with the home directory collapsed to `~`.
pub fn display_dir(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => collapse_home(&s, &home),
        _ => s.into_owned(),
    }
}

/// Collapse `home` at a path boundary in `path` to `~`. A prefix match alone
/// isn't enough: `home = /home/mx` would strip the `/home/mx` off
/// `/home/mxaddict/proj` too, collapsing it to the bogus `~addict/proj`. Only
/// collapse when the match lands on a path boundary — the prefix is the whole
/// string, or the next char is a separator. Pure, so it's testable without
/// touching the process-wide `HOME`.
fn collapse_home(path: &str, home: &str) -> String {
    if let Some(rest) = path.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{rest}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // These test the pure `collapse_home` core rather than `display_dir` so they
    // never touch the process-wide `HOME` — no env mutation, no cross-test race.

    #[test]
    fn display_dir_collapses_home_at_a_path_boundary() {
        assert_eq!(collapse_home("/home/mx", "/home/mx"), "~");
        assert_eq!(collapse_home("/home/mx/proj", "/home/mx"), "~/proj");
    }

    /// Regression: a bare prefix match turned `/home/mxaddict/proj` (a sibling
    /// directory that merely starts with the same characters as HOME) into
    /// the bogus `~addict/proj` — `mx` is not a path component of
    /// `mxaddict`, so it must not collapse at all.
    #[test]
    fn display_dir_does_not_collapse_a_sibling_directory_sharing_a_prefix() {
        assert_eq!(
            collapse_home("/home/mxaddict/proj", "/home/mx"),
            "/home/mxaddict/proj"
        );
    }
}
