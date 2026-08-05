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

/// Max bytes for the persisted history file. [`MAX_HISTORY`] entries × 4 KiB
/// average line is safely under 1 MiB, but actual input lines are much shorter;
/// this generous cap prevents OOM on a corrupted or replaced history file.
const MAX_HISTORY_FILE_BYTES: u64 = 10 * 1024 * 1024;

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
    ///
    /// The disk persist is fire-and-forget: it is a best-effort mirror for the
    /// next launch (the in-memory list stays the source of truth for this
    /// session), and [`persist_history`] ends in `write_atomic`'s two fsyncs —
    /// which must not sit on the caller's thread. `record` runs on the TUI's
    /// event loop at every submit, so a synchronous write there was what made
    /// each Enter stall for the disk.
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
    /// the first step — and again on any later step where the loaded entry was
    /// edited before stepping on, so a modified recall is never lost. `None`
    /// when there's no history to recall.
    pub fn recall_prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let pos = match self.pos {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(p) => {
                if current != self.entries[p] {
                    self.draft = current.to_string();
                }
                p.saturating_sub(1)
            }
        };
        self.pos = Some(pos);
        Some(self.entries[pos].clone())
    }

    /// Step toward newer entries; past the newest, restore the stashed draft.
    /// An edited recall is stashed first, exactly like [`Self::recall_prev`].
    /// `None` when not currently browsing.
    pub fn recall_next(&mut self, current: &str) -> Option<String> {
        let pos = self.pos?;
        if current != self.entries[pos] {
            self.draft = current.to_string();
        }
        if pos + 1 < self.entries.len() {
            self.pos = Some(pos + 1);
            Some(self.entries[pos + 1].clone())
        } else {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// Where the browser currently stands, for a UI indicator: `Some((selected,
    /// total))` while browsing — `selected` counts from the newest entry
    /// (1 = the most recent), `total` is how many entries there are. `None` when
    /// not browsing (nothing recalled yet, or the draft was restored past the
    /// newest).
    pub fn browsing(&self) -> Option<(usize, usize)> {
        let total = self.entries.len();
        let pos = self.pos?;
        Some((total - pos, total))
    }
}

/// Persist input history (one entry per line; multi-line entries are skipped to
/// keep the line-based file well-formed). Best-effort — filesystem errors are
/// silently ignored. Runs on a detached thread: the write ends in two fsyncs
/// (`write_atomic`), and the caller of [`HistoryBrowser::record`] is the TUI's
/// event loop.
pub fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    persist_history_async(path, history.to_vec());
}

/// Fire-and-forget history write, serialized behind any still-running one so
/// two rapid submits can't land out of order (each write waits for the previous
/// to finish before renaming its own file). A write that loses the race with
/// process exit is dropped — same as a crash, and the in-memory list is
/// unaffected.
fn persist_history_async(path: PathBuf, entries: Vec<String>) {
    let writer = HISTORY_WRITER.get_or_init(|| std::sync::Mutex::new(None));
    let prev = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let handle = std::thread::spawn(move || {
        if let Some(prev) = prev {
            let _ = prev.join();
        }
        persist_history_to(&path, &entries);
    });
    *writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
}

/// One history-writer slot: the join handle of the most recent persist, chained
/// by the next one so writes never reorder.
static HISTORY_WRITER: std::sync::OnceLock<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::OnceLock::new();

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
        // An unmodified recall passes the loaded entry back — the draft stays.
        assert_eq!(b.recall_prev("two").as_deref(), Some("one"));
        // Clamped at the oldest entry.
        assert_eq!(b.recall_prev("one").as_deref(), Some("one"));
        assert_eq!(b.recall_next("one").as_deref(), Some("two"));
        // Past the newest, the stashed draft comes back.
        assert_eq!(b.recall_next("two").as_deref(), Some("draft"));
        // Not browsing anymore.
        assert_eq!(b.recall_next("draft"), None);
        // Empty history: Up does nothing.
        let mut empty = HistoryBrowser::default();
        assert_eq!(empty.recall_prev("draft"), None);
    }

    /// Editing a recalled entry before stepping on keeps the edit as the draft:
    /// walking back down returns it, not the original pre-browsing text. This is
    /// what lets Up/Down keep navigating multi-line entries without eating edits.
    #[test]
    fn editing_a_recalled_entry_stashes_the_edit() {
        let mut b = HistoryBrowser {
            entries: vec!["one".into(), "two".into()],
            ..Default::default()
        };
        assert_eq!(b.recall_prev("draft").as_deref(), Some("two"));
        assert_eq!(b.recall_prev("two").as_deref(), Some("one"));
        // Edit "one" to "one-edited", then press Up (clamped at the oldest).
        assert_eq!(b.recall_prev("one-edited").as_deref(), Some("one"));
        // Down through the entries; the edited draft comes back past the newest.
        assert_eq!(b.recall_next("one").as_deref(), Some("two"));
        assert_eq!(b.recall_next("two").as_deref(), Some("one-edited"));
        // Down also stashes an edit made on the newest entry.
        assert_eq!(b.recall_prev("one-edited").as_deref(), Some("two"));
        assert_eq!(b.recall_prev("two-edited").as_deref(), Some("one"));
        assert_eq!(b.recall_next("one").as_deref(), Some("two"));
        assert_eq!(b.recall_next("two").as_deref(), Some("two-edited"));
    }

    /// Multi-line entries browse exactly like single-line ones: the arrows walk
    /// history, never the cursor lines.
    #[test]
    fn multi_line_entries_browse_like_single_line() {
        let mut b = HistoryBrowser {
            entries: vec!["line one\nline two".into(), "one\n\ntwo\n".into()],
            ..Default::default()
        };
        assert_eq!(b.recall_prev("draft").as_deref(), Some("one\n\ntwo\n"));
        assert_eq!(
            b.recall_prev("one\n\ntwo\n").as_deref(),
            Some("line one\nline two")
        );
        // Clamped; then back down and the draft returns.
        assert_eq!(
            b.recall_prev("line one\nline two").as_deref(),
            Some("line one\nline two")
        );
        assert_eq!(
            b.recall_next("line one\nline two").as_deref(),
            Some("one\n\ntwo\n")
        );
        assert_eq!(b.recall_next("one\n\ntwo\n").as_deref(), Some("draft"));
    }

    /// `browsing` reports where the walk stands, counting from the newest entry,
    /// and clears once the draft is restored past the newest.
    #[test]
    fn browsing_reports_position_then_clears() {
        let mut b = HistoryBrowser {
            entries: vec!["one".into(), "two".into(), "three".into()],
            ..Default::default()
        };
        assert_eq!(b.browsing(), None, "not browsing yet");
        b.recall_prev("draft");
        assert_eq!(b.browsing(), Some((1, 3)), "first Up lands on the newest");
        b.recall_prev("three");
        assert_eq!(b.browsing(), Some((2, 3)));
        b.recall_prev("two");
        assert_eq!(b.browsing(), Some((3, 3)), "clamped at the oldest");
        b.recall_next("one");
        assert_eq!(b.browsing(), Some((2, 3)));
        b.recall_next("two");
        b.recall_next("three");
        assert_eq!(b.browsing(), None, "draft restored — no longer browsing");
        // Empty history never browses.
        let mut empty = HistoryBrowser::default();
        assert_eq!(empty.recall_prev("d"), None);
        assert_eq!(empty.browsing(), None);
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

    /// The persist `record` triggers runs on a detached thread and lands in
    /// order. It must not run synchronously on the caller's thread — that was
    /// the per-Enter UI stall — so poll for the file rather than reading it
    /// right after `record`.
    #[test]
    fn record_persists_on_a_detached_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        persist_history_async(path.clone(), vec!["one".into(), "two".into()]);
        persist_history_async(
            path.clone(),
            vec!["one".into(), "two".into(), "three".into()],
        );

        // The writes are chained, so the file can never regress from the fuller
        // list to the shorter one — but it may legitimately read the shorter
        // list while the second write is still in flight, so only the final
        // state is asserted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(&path).as_deref().ok() == Some("one\ntwo\nthree") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the detached history write never reached disk"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
