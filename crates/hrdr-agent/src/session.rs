//! On-disk session persistence.
//!
//! A session is the conversation (`ChatMessage` history) plus light metadata,
//! stored as JSON under `$XDG_DATA_HOME/hrdr/sessions` (default
//! `~/.local/share/hrdr/sessions`).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub updated: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/hrdr/sessions`, or `~/.local/share/hrdr/sessions`.
pub fn sessions_dir() -> PathBuf {
    hjkl_xdg::data_dir("hrdr")
        .unwrap_or_else(|_| PathBuf::from(".local/share/hrdr"))
        .join("sessions")
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
        let t = now();
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

    /// Save as `<name>.json`; returns the written path.
    pub fn save(&self, name: &str) -> Result<PathBuf> {
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.json", sanitize_name(name)));
        let json = serde_json::to_string_pretty(self).context("serializing session")?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Load `<name>.json`.
    pub fn load(name: &str) -> Result<Session> {
        let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Resolve a `/resume` argument to `(id, Session)`: match the file id first,
/// then a session's display `name` (case-insensitive — e.g. after `/rename`).
pub fn resolve_session(arg: &str) -> Option<(String, Session)> {
    let id = sanitize_name(arg);
    if let Ok(s) = Session::load(&id) {
        return Some((id, s));
    }
    list_sessions()
        .into_iter()
        .find(|m| m.name.eq_ignore_ascii_case(arg.trim()) || m.id.eq_ignore_ascii_case(arg.trim()))
        .and_then(|m| Session::load(&m.id).ok().map(|s| (m.id, s)))
}

/// A collision-free file id derived from `name`: the slug, then `slug-2`,
/// `slug-3`, … if files already exist.
pub fn unique_session_id(name: &str) -> String {
    let slug = sanitize_name(name);
    let dir = sessions_dir();
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

/// List saved sessions, newest first.
pub fn list_sessions() -> Vec<SessionMeta> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
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
            if let Ok(data) = std::fs::read_to_string(&path)
                && let Ok(s) = serde_json::from_str::<Session>(&data)
            {
                out.push(SessionMeta {
                    id,
                    name: s.name,
                    updated: s.updated,
                });
            }
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.updated));
    out
}
