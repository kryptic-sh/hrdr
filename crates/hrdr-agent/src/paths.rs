//! Path helpers shared across hrdr's on-disk state (sessions, per-project
//! memory): all of them partition by working directory using the same slug, so
//! they must agree on how it's computed.

use std::hash::{DefaultHasher, Hash, Hasher};

/// Slug for a working directory — the per-cwd subdirectory name. The full path
/// is flattened (e.g. `/home/me/Projects/foo` → `home-me-projects-foo`). A hash
/// of the original path is appended to avoid collisions between distinct paths
/// that map to the same slug (e.g. `foo-bar` vs `foo_bar`).
pub fn cwd_slug(cwd: &str) -> String {
    let raw: String = cwd
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = raw
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    let suffix = format!("-{:016x}", hasher.finish());
    if s.is_empty() {
        format!("root{suffix}")
    } else {
        format!("{s}{suffix}")
    }
}
