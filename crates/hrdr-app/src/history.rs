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
/// silently ignored. Handed to the history-writer thread: the write ends in two
/// fsyncs (`write_atomic`), and the caller of [`HistoryBrowser::record`] is the
/// TUI's event loop.
pub fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    persist_history_async(path, history.to_vec());
}

/// Queue a history snapshot for the writer thread. Never blocks the caller —
/// not on the disk, and not on a busy queue.
///
/// Writes land in submit order because [`HistoryWriter`]'s thread is the only
/// consumer of the queue.
///
/// A write that loses the race with process exit is dropped: nobody joins the
/// writer thread, so a queued or in-flight snapshot dies with the process —
/// same as a crash, and the in-memory list is unaffected.
fn persist_history_async(path: PathBuf, entries: Vec<String>) {
    HISTORY_WRITER
        .get_or_init(HistoryWriter::spawn)
        .queue(path, entries);
}

/// The process-wide history writer.
static HISTORY_WRITER: std::sync::OnceLock<HistoryWriter> = std::sync::OnceLock::new();

/// Snapshots waiting to be written, at most one per path.
type Pending = std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>;

/// One long-lived thread that performs every history write, fed by a queue of
/// pending snapshots and a wakeup channel.
///
/// Shaped like [`crate::watch_config`]'s debounce thread — an `mpsc` channel and
/// one worker — except the channel carries a wakeup and the snapshots live in
/// `pending`. That split is what lets a busy writer neither stall the UI thread
/// (which a bounded `send` would) nor lose the newest state (which a bounded
/// `try_send` would, by dropping exactly the snapshot that describes the list as
/// it now stands). Superseding a snapshot inside `pending` discards only an
/// older one, and then the wakeup channel can be capacity-1: a full one means
/// the worker has a wakeup coming that will find whatever was just stored.
struct HistoryWriter {
    /// The queue, shared with the writer thread. A later snapshot for a path
    /// *replaces* the one queued for it, because every write is a full snapshot
    /// of the list — [`persist_history_to`] renders the whole slice and
    /// `write_atomic` replaces the file — so a superseded snapshot would only
    /// write bytes the next one immediately overwrites. Coalescing turns a burst
    /// of submits into one write; it would be wrong if a write were a delta.
    /// Keyed by path so one file's snapshot can never displace another's.
    pending: std::sync::Arc<Pending>,
    /// Wakes the writer thread. Capacity 1 — see the type docs.
    wake: std::sync::mpsc::SyncSender<()>,
}

impl HistoryWriter {
    /// Start the writer thread. The sender lives in a `static`, so it is never
    /// dropped and the thread runs until the process exits.
    fn spawn() -> Self {
        let pending = std::sync::Arc::<Pending>::default();
        let (wake, rx) = std::sync::mpsc::sync_channel(1);
        let queue = std::sync::Arc::clone(&pending);
        std::thread::spawn(move || history_writer_loop(&rx, &queue));
        Self { pending, wake }
    }

    /// Queue `entries` as the snapshot for `path` and wake the writer.
    fn queue(&self, path: PathBuf, entries: Vec<String>) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.iter_mut().find(|(queued, _)| *queued == path) {
            Some(slot) => slot.1 = entries,
            None => pending.push((path, entries)),
        }
        // Ping while still holding the lock, so a dropped ping is never a
        // dropped write: the worker locks the queue only *after* taking a
        // wakeup, so `Full` here means a wakeup it has not taken yet, and by the
        // time it does this snapshot is in the queue. The lock covers a move and
        // a send, never the write itself.
        let _ = self.wake.try_send(());
    }
}

/// Write queued history snapshots until the sender is dropped (in practice,
/// until the process exits). Draining the whole queue per wakeup is what makes
/// the coalescing in [`HistoryWriter::pending`] pay off: snapshots queued while
/// a write was in flight have already collapsed to the newest per path.
fn history_writer_loop(rx: &std::sync::mpsc::Receiver<()>, pending: &Pending) {
    while rx.recv().is_ok() {
        // May be empty: a wakeup can arrive for a snapshot the previous drain
        // already picked up.
        let queued = std::mem::take(
            &mut *pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (path, entries) in queued {
            persist_history_to(&path, &entries);
        }
    }
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

    /// Poll `path` until it reads exactly `want`, or fail after a deadline so
    /// generous that only a write that never happens can trip it: the writer has
    /// one small file to render and rename, and the bound allows seconds of
    /// scheduling delay on a loaded machine.
    fn wait_for_file(path: &std::path::Path, want: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let got = std::fs::read_to_string(path);
            if got.as_deref().ok() == Some(want) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "history file never reached {want:?} (last read: {got:?})"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The persist `record` triggers runs on the writer thread and lands in
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

        // One consumer writes both snapshots in submit order (and may coalesce
        // them into just the second), so the file can never end on the shorter
        // list — but it may legitimately read the shorter list while the second
        // write is still in flight, so only the final state is asserted.
        wait_for_file(&path, "one\ntwo\nthree");
    }

    /// A burst of submits converges on the *last* snapshot, even when that
    /// snapshot is shorter than the ones it superseded — coalescing keeps the
    /// newest, not the biggest, and a queued snapshot never lands after the one
    /// that replaced it.
    ///
    /// A regression guard as much as a test of the coalescing: the old
    /// thread-per-write implementation converged on the same content, just at
    /// the cost of a thread and two fsyncs per submit. What it does discriminate
    /// is the new machinery going wrong — a snapshot stored without a wakeup, or
    /// a drain that picks the wrong queued entry.
    #[test]
    fn a_burst_of_persists_converges_on_the_last() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let mut entries: Vec<String> = Vec::new();
        for i in 0..200 {
            entries.push(format!("entry {i}"));
            persist_history_async(path.clone(), entries.clone());
        }
        // Last one wins even though it is the shortest of the burst.
        persist_history_async(path.clone(), vec!["last".into()]);

        wait_for_file(&path, "last");
    }

    /// The writer thread outlives the queue going empty: a submit that arrives
    /// long after an earlier one has landed still gets written. (The old
    /// implementation spawned a thread per write, so it could not fail this; the
    /// single worker can, by returning once it has drained the queue.)
    #[test]
    fn the_writer_thread_serves_submits_after_going_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        persist_history_async(path.clone(), vec!["first".into()]);
        wait_for_file(&path, "first");

        // The queue is empty and the worker is parked on the wakeup channel.
        persist_history_async(path.clone(), vec!["first".into(), "second".into()]);
        wait_for_file(&path, "first\nsecond");
    }

    /// Two paths never displace each other in the queue: coalescing is per file,
    /// so a burst on one history file cannot swallow another's snapshot.
    #[test]
    fn snapshots_for_different_paths_all_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("history-a");
        let b = dir.path().join("history-b");
        for i in 0..50 {
            persist_history_async(a.clone(), vec![format!("a {i}")]);
            persist_history_async(b.clone(), vec![format!("b {i}")]);
        }

        wait_for_file(&a, "a 49");
        wait_for_file(&b, "b 49");
    }
}
