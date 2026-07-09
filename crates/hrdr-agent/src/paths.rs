//! Path helpers shared across hrdr's on-disk state (sessions, checkpoints,
//! per-project memory): all of them partition by working directory using the
//! same slug, so they must agree on how it's computed.

/// Slug for a working directory — the per-cwd subdirectory name. The full path
/// is flattened (e.g. `/home/me/Projects/foo` → `home-me-projects-foo`).
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
    if s.is_empty() { "root".to_string() } else { s }
}
