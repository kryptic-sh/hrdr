//! File checkpoints: content-addressed pre-image snapshots so the agent's file
//! edits can be reverted per turn.
//!
//! Storage is git-like — each changed file's prior content is deflate-compressed
//! and stored once per unique content (content-addressed by SHA-256), and a
//! journal records which turn touched which file. Only files the agent modifies
//! (via `edit`/`write`) are snapshotted, and only their pre-image (the
//! content just before the first edit in a turn), so it's fast and small.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Keep at most this many of the most-recent turns' checkpoints; older turns are
/// pruned on `open`. Bounds journal + blob growth across long/many sessions.
const CHECKPOINT_KEEP_TURNS: u64 = 200;
/// Also drop any checkpoint whose record is older than this (abandoned
/// sessions). Kept generous — the turn cap is the primary bound.
const CHECKPOINT_MAX_AGE_SECS: u64 = 14 * 24 * 60 * 60;
/// A `journal.lock` older than this is presumed abandoned (crashed holder) and
/// stolen, so a stale lock can't wedge the store forever.
const LOCK_STALE_SECS: u64 = 30;

/// A single file change: the file `path` and its content hash *before* the turn
/// modified it (`pre = None` if the file didn't exist yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeRecord {
    turn: u64,
    /// Unix seconds when the record was written. `#[serde(default)]` (→ 0) so
    /// journals from before this field existed still deserialize instead of
    /// being silently dropped on upgrade; a legacy `ts == 0` is exempt from the
    /// age cutoff (bounded only by the turn cap — see [`Checkpoints::prune`]).
    #[serde(default)]
    ts: u64,
    path: String,
    pre: Option<String>,
}

/// One revertible checkpoint (a turn that changed files).
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub turn: u64,
    pub ts: u64,
    pub files: Vec<String>,
}

/// A disk-backed store of per-turn file pre-images.
pub struct Checkpoints {
    dir: PathBuf,
    blobs_dir: PathBuf,
    journal_path: PathBuf,
    records: Vec<ChangeRecord>,
    turn: u64,
    touched: HashSet<String>,
}

impl Checkpoints {
    /// Open (or create) the checkpoint store rooted at `dir`.
    pub fn open(dir: PathBuf) -> Result<Self> {
        let blobs_dir = dir.join("blobs");
        std::fs::create_dir_all(&blobs_dir)
            .with_context(|| format!("creating {}", blobs_dir.display()))?;
        let journal_path = dir.join("journal.jsonl");
        let mut cp = Self {
            dir,
            blobs_dir,
            journal_path,
            records: Vec::new(),
            turn: 0,
            touched: HashSet::new(),
        };
        // Prune old checkpoints + GC orphan blobs under the lock, reconciling
        // with anything another instance in this dir has written.
        {
            let _lock = JournalLock::acquire(&cp.dir);
            cp.reload_records();
            let _ = cp.prune();
        }
        cp.turn = cp.records.iter().map(|r| r.turn).max().unwrap_or(0);
        Ok(cp)
    }

    /// Re-read the on-disk journal into `records` (adopting any records another
    /// `Checkpoints` over the same dir appended since we loaded). Callers hold
    /// [`JournalLock`] so this reflects a consistent view.
    fn reload_records(&mut self) {
        let mut records = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&self.journal_path) {
            for line in text.lines() {
                if let Ok(r) = serde_json::from_str::<ChangeRecord>(line) {
                    records.push(r);
                }
            }
        }
        self.records = records;
    }

    /// Drop checkpoints older than the turn/age caps and GC blobs no record
    /// references. Rewrites the journal only when something changed. Caller holds
    /// [`JournalLock`].
    fn prune(&mut self) -> Result<()> {
        let max_turn = self.records.iter().map(|r| r.turn).max().unwrap_or(0);
        let turn_floor = max_turn.saturating_sub(CHECKPOINT_KEEP_TURNS);
        let age_cutoff = crate::unix_now().saturating_sub(CHECKPOINT_MAX_AGE_SECS);
        let before = self.records.len();
        // Legacy records (ts == 0, pre-timestamp journals) are exempt from the
        // age cutoff so an upgrade doesn't wipe still-recent checkpoints; the
        // turn cap alone bounds them.
        self.records
            .retain(|r| r.turn > turn_floor && (r.ts == 0 || r.ts >= age_cutoff));
        let changed = self.records.len() != before;
        self.gc_blobs();
        if changed {
            self.rewrite_journal()?;
        }
        Ok(())
    }

    /// Delete blob files not referenced by any live record (orphans left behind
    /// by pruned/reverted turns). Caller holds [`JournalLock`].
    fn gc_blobs(&self) {
        let live: HashSet<&str> = self
            .records
            .iter()
            .filter_map(|r| r.pre.as_deref())
            .collect();
        let Ok(entries) = std::fs::read_dir(&self.blobs_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !live.contains(name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Begin a new turn (its file changes form one checkpoint).
    pub fn begin_turn(&mut self) {
        self.turn += 1;
        self.touched.clear();
    }

    /// Record a file's pre-modification content (only on the first touch in the
    /// current turn). Call *before* a tool writes to `path`.
    pub fn record_pre(&mut self, path: &Path) {
        let key = path.to_string_lossy().to_string();
        if !self.touched.insert(key.clone()) {
            return; // already snapshotted this file this turn
        }
        // Hold the lock across blob-store + journal-append so a concurrent
        // instance's blob GC can't delete a blob between our write and the
        // record that references it.
        let _lock = JournalLock::acquire(&self.dir);
        let pre = match std::fs::read(path) {
            Ok(bytes) => match self.store_blob(&bytes) {
                Ok(hash) => Some(hash),
                Err(_) => return, // couldn't store — don't record a bad checkpoint
            },
            Err(_) => None, // file didn't exist before
        };
        let rec = ChangeRecord {
            turn: self.turn,
            ts: crate::unix_now(),
            path: key,
            pre,
        };
        if let Ok(line) = serde_json::to_string(&rec) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.journal_path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        self.records.push(rec);
    }

    /// The revertible checkpoints (turns with changes), newest first.
    pub fn list(&self) -> Vec<CheckpointInfo> {
        let mut by_turn: BTreeMap<u64, CheckpointInfo> = BTreeMap::new();
        for r in &self.records {
            let e = by_turn.entry(r.turn).or_insert_with(|| CheckpointInfo {
                turn: r.turn,
                ts: r.ts,
                files: Vec::new(),
            });
            if !e.files.contains(&r.path) {
                e.files.push(r.path.clone());
            }
        }
        let mut v: Vec<_> = by_turn.into_values().collect();
        v.reverse();
        v
    }

    /// Revert the most recent turn's file changes. Returns the restored paths.
    pub fn revert_last(&mut self) -> Result<Vec<PathBuf>> {
        match self.records.iter().map(|r| r.turn).max() {
            Some(last) => self.revert_to(last),
            None => Ok(Vec::new()),
        }
    }

    /// Restore files to their state *before* `turn` — i.e. undo `turn` and every
    /// later turn. Returns the restored paths.
    pub fn revert_to(&mut self, turn: u64) -> Result<Vec<PathBuf>> {
        // Reconcile under the lock: re-read the journal so a rewrite is applied
        // to the current on-disk record set (which may include records another
        // instance in this dir appended) rather than this instance's stale
        // in-memory view — otherwise the rewrite would silently discard them.
        let _lock = JournalLock::acquire(&self.dir);
        self.reload_records();
        // For each file touched in turns >= `turn`, the pre-`turn` state is the
        // pre-image recorded at the SMALLEST such turn.
        let mut earliest: BTreeMap<String, (u64, Option<String>)> = BTreeMap::new();
        for r in self.records.iter().filter(|r| r.turn >= turn) {
            let e = earliest
                .entry(r.path.clone())
                .or_insert((r.turn, r.pre.clone()));
            if r.turn < e.0 {
                *e = (r.turn, r.pre.clone());
            }
        }
        let mut restored = Vec::new();
        for (path, (_t, pre)) in &earliest {
            let p = PathBuf::from(path);
            match pre {
                Some(hash) => {
                    let bytes = self.load_blob(hash)?;
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&p, bytes)
                        .with_context(|| format!("restoring {}", p.display()))?;
                }
                None => {
                    let _ = std::fs::remove_file(&p); // didn't exist before the turn
                }
            }
            restored.push(p);
        }
        // Drop reverted records, rewrite the journal, and GC blobs the dropped
        // records were the only referents of.
        self.records.retain(|r| r.turn < turn);
        self.rewrite_journal()?;
        self.gc_blobs();
        Ok(restored)
    }

    fn store_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = sha256_hex(bytes);
        let path = self.blobs_dir.join(&hash);
        if !path.exists() {
            let compressed = miniz_oxide::deflate::compress_to_vec(bytes, 6);
            std::fs::write(&path, compressed).with_context(|| format!("writing blob {hash}"))?;
        }
        Ok(hash)
    }

    fn load_blob(&self, hash: &str) -> Result<Vec<u8>> {
        let comp = std::fs::read(self.blobs_dir.join(hash))
            .with_context(|| format!("reading blob {hash}"))?;
        miniz_oxide::inflate::decompress_to_vec(&comp)
            .map_err(|e| anyhow::anyhow!("decompressing blob {hash}: {e:?}"))
    }

    fn rewrite_journal(&self) -> Result<()> {
        let mut out = String::new();
        for r in &self.records {
            if let Ok(line) = serde_json::to_string(r) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        std::fs::write(&self.journal_path, out).context("rewriting checkpoint journal")?;
        Ok(())
    }
}

/// Best-effort advisory lock over a checkpoint directory's journal so two
/// `Checkpoints` in the same dir (e.g. a main agent + a background sub-agent
/// sharing a non-git cwd) serialize journal rewrites and blob GC. Implemented as
/// an `O_EXCL` lock file — no extra dependency — with staleness detection so a
/// crashed holder's lock (older than [`LOCK_STALE_SECS`]) is stolen rather than
/// wedging the store. Released on drop. If the lock can't be acquired within the
/// spin budget it proceeds *unlocked* (best-effort — never wedge the agent);
/// `held` records whether this instance owns the file so drop doesn't remove a
/// lock it didn't create.
struct JournalLock {
    path: PathBuf,
    held: bool,
}

impl JournalLock {
    fn acquire(dir: &Path) -> Self {
        let path = dir.join("journal.lock");
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Self { path, held: true };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue; // steal it
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break, // can't create (perms) — proceed unlocked
            }
        }
        Self { path, held: false }
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether the lock file is missing/unreadable or older than the staleness
/// window (its holder presumed crashed).
fn lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            t.elapsed()
                .map(|e| e > Duration::from_secs(LOCK_STALE_SECS))
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_restores_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let existing = work.join("a.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = work.join("b.txt");

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        // Turn 1: modify a.txt, create b.txt.
        cp.begin_turn();
        cp.record_pre(&existing); // pre = "original"
        std::fs::write(&existing, "changed").unwrap();
        cp.record_pre(&created); // pre = None (new file)
        std::fs::write(&created, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "changed");
        assert!(created.exists());

        let restored = cp.revert_last().unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists(), "new file should be removed on revert");
        assert!(cp.list().is_empty(), "checkpoint consumed after revert");
    }

    #[test]
    fn revert_uses_earliest_preimage_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn(); // turn 1
        cp.record_pre(&f); // pre = v0
        std::fs::write(&f, "v1").unwrap();

        cp.begin_turn(); // turn 2
        cp.record_pre(&f); // pre = v1
        std::fs::write(&f, "v2").unwrap();

        // Revert last (turn 2) → back to v1.
        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // Revert again (turn 1) → back to v0.
        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v0");
    }

    #[test]
    fn store_and_load_blob_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        let data = b"hello checkpoint world";
        let hash = cp.store_blob(data).unwrap();
        let loaded = cp.load_blob(&hash).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn identical_blobs_are_deduped_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        let data = b"same content";
        let h1 = cp.store_blob(data).unwrap();
        let h2 = cp.store_blob(data).unwrap();
        // Same content → same hash, written only once.
        assert_eq!(h1, h2);
        let blob_count = std::fs::read_dir(&cp.blobs_dir).unwrap().count();
        assert_eq!(
            blob_count, 1,
            "identical content should produce exactly one blob file"
        );
    }

    #[test]
    fn record_pre_only_first_touch_recorded_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "original").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn();
        cp.record_pre(&f);
        cp.record_pre(&f); // second call for the same file in the same turn is a no-op
        // Only one journal record — the first touch.
        assert_eq!(cp.records.len(), 1);
    }

    #[test]
    fn prune_drops_old_records_and_orphan_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let cpdir = dir.path().join("cp");
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        {
            let mut cp = Checkpoints::open(cpdir.clone()).unwrap();
            cp.begin_turn();
            cp.record_pre(&f); // stores a blob for "v0"
            std::fs::write(&f, "v1").unwrap();
        }
        let journal = cpdir.join("journal.jsonl");
        let blobs = cpdir.join("blobs");
        assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 1);

        // Backdate the record beyond the age cutoff.
        let text = std::fs::read_to_string(&journal).unwrap();
        let mut recs: Vec<ChangeRecord> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for r in &mut recs {
            r.ts = 1; // ancient
        }
        let out: String = recs
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        std::fs::write(&journal, out).unwrap();
        // An orphan blob no record references.
        std::fs::write(blobs.join("deadbeefdeadbeef"), b"junk").unwrap();

        // Reopen → prune drops the ancient record and GCs all now-orphan blobs.
        let cp = Checkpoints::open(cpdir.clone()).unwrap();
        assert!(cp.list().is_empty(), "stale record pruned");
        assert!(!blobs.join("deadbeefdeadbeef").exists(), "orphan blob GC'd");
        assert_eq!(
            std::fs::read_dir(&blobs).unwrap().count(),
            0,
            "blobs GC'd once no record references them"
        );
    }

    #[test]
    fn prune_keeps_recent_records() {
        let dir = tempfile::tempdir().unwrap();
        let cpdir = dir.path().join("cp");
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        {
            let mut cp = Checkpoints::open(cpdir.clone()).unwrap();
            cp.begin_turn();
            cp.record_pre(&f);
            std::fs::write(&f, "v1").unwrap();
        }
        // Reopen without backdating → the fresh record and its blob survive.
        let cp = Checkpoints::open(cpdir.clone()).unwrap();
        assert_eq!(cp.list().len(), 1, "recent checkpoint kept");
        assert_eq!(std::fs::read_dir(cpdir.join("blobs")).unwrap().count(), 1);
    }

    #[test]
    fn revert_reconciles_concurrent_records() {
        let dir = tempfile::tempdir().unwrap();
        let cpdir = dir.path().join("cp");
        let fa = dir.path().join("a.txt");
        std::fs::write(&fa, "a0").unwrap();
        let fb = dir.path().join("b.txt");
        std::fs::write(&fb, "b0").unwrap();

        // Instance A: turn 1 touches a.txt (appended to the shared journal).
        let mut a = Checkpoints::open(cpdir.clone()).unwrap();
        a.begin_turn();
        a.record_pre(&fa);
        std::fs::write(&fa, "a1").unwrap();

        // Instance B over the same dir: its own turn touches b.txt. A's
        // in-memory view never learns about this record.
        let mut b = Checkpoints::open(cpdir.clone()).unwrap();
        b.begin_turn(); // turn = max(disk) + 1 = 2
        b.record_pre(&fb);
        std::fs::write(&fb, "b1").unwrap();

        // A rewrites the journal (via a no-op revert of a higher turn). Without
        // reconcile this discards B's turn-2 record; with it, B survives.
        a.revert_to(99).unwrap();

        let reopened = Checkpoints::open(cpdir.clone()).unwrap();
        let turns: Vec<u64> = reopened.list().iter().map(|c| c.turn).collect();
        assert!(turns.contains(&1), "A's record survives: {turns:?}");
        assert!(
            turns.contains(&2),
            "B's concurrent record survives: {turns:?}"
        );
    }

    #[test]
    fn revert_to_specific_turn_only_undoes_that_turn_forward() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn(); // turn 1: v0 → v1
        cp.record_pre(&f);
        std::fs::write(&f, "v1").unwrap();

        cp.begin_turn(); // turn 2: v1 → v2
        cp.record_pre(&f);
        std::fs::write(&f, "v2").unwrap();

        cp.begin_turn(); // turn 3: v2 → v3
        cp.record_pre(&f);
        std::fs::write(&f, "v3").unwrap();

        // revert_to(2) undoes turns 2 and 3; pre-turn-2 content is v1.
        cp.revert_to(2).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // Turn 1 must still be listed — it was not reverted.
        let remaining = cp.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].turn, 1);
    }
}
