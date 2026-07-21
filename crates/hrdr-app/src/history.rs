//! Persisted single-line input history, shared by hrdr's frontends. A newline-
//! delimited file under `$XDG_DATA_HOME/hrdr/history` holds the most recent
//! [`MAX_HISTORY`] submitted lines (oldest first) for Up/Down recall. No UI —
//! just load/save over the XDG data dir.

use std::path::PathBuf;

/// Max input-history entries kept (in memory and on disk).
pub const MAX_HISTORY: usize = 200;

/// Path to the persisted input history (`$XDG_DATA_HOME/hrdr/history`).
fn history_path() -> Option<PathBuf> {
    hjkl_xdg::data_dir("hrdr").ok().map(|d| d.join("history"))
}

/// Max bytes for the persisted history file. 200 entries × 4 KiB average line
/// is safely under 1 MiB, but actual input lines are much shorter; this generous
/// cap prevents OOM on a corrupted or replaced history file.
const MAX_HISTORY_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

/// Load persisted single-line input history (most recent [`MAX_HISTORY`], oldest
/// first). Blank lines are skipped; a missing/unreadable file yields an empty
/// history.
pub fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_HISTORY_FILE_BYTES {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if v.len() > MAX_HISTORY {
        let drop = v.len() - MAX_HISTORY;
        v.drain(0..drop);
    }
    v
}

/// Input-history browsing shared by the frontends: record with
/// consecutive-duplicate skip + [`MAX_HISTORY`] cap + persistence, and Up/Down
/// recall that stashes the live draft on the first step back and restores it
/// past the newest entry. A frontend decides where the returned text goes (for
/// the TUI, its editor buffer).
#[derive(Default)]
pub struct HistoryBrowser {
    entries: Vec<String>,
    pos: Option<usize>,
    draft: String,
}

impl HistoryBrowser {
    /// Start from the persisted history file.
    pub fn load() -> Self {
        Self {
            entries: load_history(),
            ..Self::default()
        }
    }

    /// Record a submitted input (skips a consecutive duplicate, bounds the
    /// buffer, persists on change) and reset browsing state.
    pub fn record(&mut self, input: &str) {
        if self.entries.last().map(String::as_str) != Some(input) {
            self.entries.push(input.to_string());
            if self.entries.len() > MAX_HISTORY {
                let drop = self.entries.len() - MAX_HISTORY;
                self.entries.drain(0..drop);
            }
            persist_history(&self.entries);
        }
        self.pos = None;
        self.draft.clear();
    }

    /// Step to the previous (older) entry, stashing `current` as the draft on
    /// the first step. `None` when there's no history to recall.
    pub fn recall_prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let pos = match self.pos {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.pos = Some(pos);
        Some(self.entries[pos].clone())
    }

    /// Step toward newer entries; past the newest, restore the stashed draft.
    /// `None` when not currently browsing.
    pub fn recall_next(&mut self) -> Option<String> {
        let pos = self.pos?;
        if pos + 1 < self.entries.len() {
            self.pos = Some(pos + 1);
            Some(self.entries[pos + 1].clone())
        } else {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        }
    }
}

/// Persist input history (one entry per line; multi-line entries are skipped to
/// keep the line-based file well-formed). Best-effort — filesystem errors are
/// silently ignored.
pub fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    persist_history_to(&path, history);
}

/// Persist input history to an explicit path. Best-effort — filesystem errors
/// are silently ignored. The write goes through [`hrdr_agent::write_atomic`],
/// which creates the file owner-only (`0600` on Unix) and renames it into place,
/// so the history (which may contain pasted secrets) is never world-readable.
fn persist_history_to(path: &std::path::Path, history: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = history
        .iter()
        .filter(|s| !s.contains('\n'))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = hrdr_agent::write_atomic(path, body.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_prev_next_restores_draft() {
        let mut b = HistoryBrowser {
            entries: vec!["one".into(), "two".into()],
            ..Default::default()
        };
        assert_eq!(b.recall_prev("draft").as_deref(), Some("two"));
        assert_eq!(b.recall_prev("ignored").as_deref(), Some("one"));
        // Clamped at the oldest entry.
        assert_eq!(b.recall_prev("ignored").as_deref(), Some("one"));
        assert_eq!(b.recall_next().as_deref(), Some("two"));
        // Past the newest, the stashed draft comes back.
        assert_eq!(b.recall_next().as_deref(), Some("draft"));
        // Not browsing anymore.
        assert_eq!(b.recall_next(), None);
        // Empty history: Up does nothing.
        let mut empty = HistoryBrowser::default();
        assert_eq!(empty.recall_prev("draft"), None);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_history_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        persist_history_to(&path, &["one".into(), "two".into()]);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "history file must be owner-only, got {mode:o}");
    }
}
