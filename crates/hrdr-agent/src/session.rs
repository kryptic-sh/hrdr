//! On-disk session persistence.
//!
//! A session is the conversation (`ChatMessage` history) plus light metadata,
//! stored as JSON under `$XDG_DATA_HOME/hrdr/sessions` (default
//! `~/.local/share/hrdr/sessions`). Sessions are partitioned by working
//! directory: each lives at `sessions/<cwd-slug>/<name-slug>.json`, so the
//! files are easy to manage by hand and `/sessions` can scope to one project.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hrdr_llm::ChatMessage;
use serde::{Deserialize, Serialize};

const SESSION_VERSION: u32 = 1;

/// A saved conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Human-friendly session name (defaults to the first user message).
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub cwd: String,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
    pub messages: Vec<ChatMessage>,
}

/// Lightweight directory listing entry.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// File stem — the id you `/resume` by.
    pub id: String,
    /// The session's display name.
    pub name: String,
    /// The working directory this session belongs to.
    pub cwd: String,
    pub updated: u64,
    /// Absolute path to the session file.
    pub path: PathBuf,
}

/// `$XDG_DATA_HOME/hrdr/sessions`, or `~/.local/share/hrdr/sessions`.
pub fn sessions_dir() -> PathBuf {
    // The fallback must be absolute: a relative path would scatter session
    // JSON into whatever directory the agent happens to run in.
    hjkl_xdg::data_dir("hrdr")
        .unwrap_or_else(|_| std::env::temp_dir().join("hrdr"))
        .join("sessions")
}

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

/// The per-cwd directory a session lives in.
fn session_dir(cwd: &str) -> PathBuf {
    sessions_dir().join(cwd_slug(cwd))
}

/// Reduce an arbitrary name to a safe, length-capped, lowercase file stem.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let s = s.trim_matches('-').to_lowercase();
    if s.is_empty() {
        "session".to_string()
    } else {
        s
    }
}

impl Session {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        cwd: impl Into<String>,
        messages: Vec<ChatMessage>,
    ) -> Self {
        let t = hrdr_tools::unix_now();
        Self {
            version: SESSION_VERSION,
            name: name.into(),
            model: model.into(),
            base_url: base_url.into(),
            cwd: cwd.into(),
            created: t,
            updated: t,
            messages,
        }
    }

    /// Save as `<cwd-slug>/<id>.json` (the cwd comes from `self.cwd`); returns
    /// the written path.
    pub fn save(&self, id: &str) -> Result<PathBuf> {
        let dir = session_dir(&self.cwd);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.json", sanitize_name(id)));
        // Autosave rebuilds a fresh `Session` per write; keep the original
        // creation time from the file being overwritten.
        let mut snap = self.clone();
        if let Ok(prev) = Self::load_path(&path) {
            snap.created = prev.created;
        }
        let json = serde_json::to_string_pretty(&snap).context("serializing session")?;
        crate::auth::write_atomic(&path, json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Load `<cwd-slug>/<id>.json`.
    pub fn load(cwd: &str, id: &str) -> Result<Session> {
        Self::load_path(&session_dir(cwd).join(format!("{}.json", sanitize_name(id))))
    }

    /// Load a session directly from a file path.
    pub fn load_path(path: &Path) -> Result<Session> {
        let data =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Resolve a `/resume` argument to `(id, Session)`. Looks in `cwd`'s directory
/// first (by file id), then scans every directory — preferring the current
/// `cwd` — matching the file id or the display `name` (case-insensitive, e.g.
/// after `/rename`).
pub fn resolve_session(cwd: &str, arg: &str) -> Option<(String, Session)> {
    let id = sanitize_name(arg);
    if let Ok(s) = Session::load(cwd, &id) {
        return Some((id, s));
    }
    let cur = cwd_slug(cwd);
    let mut metas = list_sessions();
    // Stable sort keeps newest-first ordering within each group; current cwd
    // (key `false`) sorts ahead of the rest.
    metas.sort_by_key(|m| cwd_slug(&m.cwd) != cur);
    let arg = arg.trim();
    metas
        .into_iter()
        .find(|m| m.name.eq_ignore_ascii_case(arg) || m.id.eq_ignore_ascii_case(arg))
        .and_then(|m| Session::load_path(&m.path).ok().map(|s| (m.id, s)))
}

/// A collision-free file id (within `cwd`'s directory) derived from `name`:
/// the slug, then `slug-2`, `slug-3`, … if files already exist.
pub fn unique_session_id(cwd: &str, name: &str) -> String {
    let slug = sanitize_name(name);
    let dir = session_dir(cwd);
    if !dir.join(format!("{slug}.json")).exists() {
        return slug;
    }
    for i in 2..10_000 {
        let cand = format!("{slug}-{i}");
        if !dir.join(format!("{cand}.json")).exists() {
            return cand;
        }
    }
    slug
}

/// Collect session files from one directory into `out`.
fn collect_sessions(dir: &Path, out: &mut Vec<SessionMeta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if let Ok(s) = Session::load_path(&path) {
            out.push(SessionMeta {
                id,
                name: s.name,
                cwd: s.cwd,
                updated: s.updated,
                path,
            });
        }
    }
}

/// List saved sessions across every working directory, newest first. Also
/// picks up legacy flat-layout files written directly under `sessions/`.
pub fn list_sessions() -> Vec<SessionMeta> {
    let base = sessions_dir();
    let mut out = Vec::new();
    // Legacy flat layout.
    collect_sessions(&base, &mut out);
    // Per-cwd subdirectories.
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_sessions(&path, &mut out);
            }
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.updated));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_name ─────────────────────────────────────────────────────────

    #[test]
    fn sanitize_name_lowercases_and_slugifies() {
        assert_eq!(sanitize_name("Hello World"), "hello-world");
        assert_eq!(sanitize_name("  UPPER  "), "upper");
        assert_eq!(sanitize_name("foo/bar.baz"), "foo-bar-baz");
    }

    #[test]
    fn sanitize_name_fallback_on_empty() {
        assert_eq!(sanitize_name(""), "session");
        assert_eq!(sanitize_name("!!!"), "session");
    }

    #[test]
    fn sanitize_name_caps_at_48_chars() {
        let long = "a".repeat(100);
        let result = sanitize_name(&long);
        assert!(result.len() <= 48, "sanitized name must be ≤48 chars");
    }

    // ── unique_session_id ─────────────────────────────────────────────────────

    #[test]
    fn unique_session_id_returns_plain_slug_when_dir_absent() {
        // A cwd whose session directory doesn't exist → the first call returns
        // the plain slug without any suffix.
        let id = unique_session_id("/nonexistent/hrdr/test/path/12345", "my session");
        assert_eq!(id, "my-session");
    }

    #[test]
    fn unique_session_id_appends_suffix_on_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap();

        // No file yet → plain slug returned.
        assert_eq!(unique_session_id(cwd, "chat"), "chat");

        // Create the first session file to simulate a collision.
        let sess = Session::new("chat", "model", "http://x/v1", cwd, vec![]);
        sess.save("chat").unwrap();

        // Next call: "chat.json" exists → returns "chat-2".
        assert_eq!(unique_session_id(cwd, "chat"), "chat-2");
    }

    // ── resolve_session ───────────────────────────────────────────────────────

    #[test]
    fn resolve_session_returns_none_for_unknown_id() {
        assert!(resolve_session("/nonexistent/path/xyz", "no-such-session").is_none());
    }

    #[test]
    fn resolve_session_exact_id_in_current_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap();

        let sess = Session::new("My Chat", "model", "http://x/v1", cwd, vec![]);
        sess.save("my-chat").unwrap();

        let (id, s) = resolve_session(cwd, "my-chat").unwrap();
        assert_eq!(id, "my-chat");
        assert_eq!(s.name, "My Chat");
        assert_eq!(s.cwd, cwd);
    }

    #[test]
    fn resolve_session_case_insensitive_display_name_match() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap();

        // Save with id "work" but display name "Work Session".
        let sess = Session::new("Work Session", "model", "http://x/v1", cwd, vec![]);
        sess.save("work").unwrap();

        // Searching by display name (case-insensitive) should find it.
        let result = resolve_session(cwd, "WORK SESSION");
        assert!(result.is_some(), "case-insensitive name match must work");
        let (id, s) = result.unwrap();
        assert_eq!(id, "work");
        assert_eq!(s.name, "Work Session");
    }

    #[test]
    fn resolve_session_current_cwd_preferred_over_other_cwd() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let cwd_a = tmp_a.path().to_str().unwrap();
        let cwd_b = tmp_b.path().to_str().unwrap();

        // Session "alpha" exists in both cwd_a (exact id) and cwd_b (same id).
        Session::new("Alpha A", "m", "http://x/v1", cwd_a, vec![])
            .save("alpha")
            .unwrap();
        Session::new("Alpha B", "m", "http://x/v1", cwd_b, vec![])
            .save("alpha")
            .unwrap();

        // resolve_session from cwd_a: exact id match in the current cwd wins.
        let (_, s) = resolve_session(cwd_a, "alpha").unwrap();
        assert_eq!(
            s.name, "Alpha A",
            "current-cwd exact match takes precedence"
        );

        // resolve_session from cwd_b: its own "alpha" wins.
        let (_, s) = resolve_session(cwd_b, "alpha").unwrap();
        assert_eq!(s.name, "Alpha B");
    }
}
