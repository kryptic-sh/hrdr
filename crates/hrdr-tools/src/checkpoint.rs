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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Keep at most this many of the most-recent turns' checkpoints; older turns are
/// pruned on `open`. Bounds journal + blob growth across long/many sessions.
const CHECKPOINT_KEEP_TURNS: u64 = 200;
/// Also drop any checkpoint whose record is older than this (abandoned
/// sessions). Kept generous — the turn cap is the primary bound.
const CHECKPOINT_MAX_AGE_SECS: u64 = 14 * 24 * 60 * 60;

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
    /// Filesystem object type. `None` is a legacy journal record: `pre: Some`
    /// means file, `pre: None` means the path did not exist.
    #[serde(default)]
    kind: Option<NodeKind>,
    /// Unix permission bits captured at snapshot time (regular files only), so a
    /// revert can restore an executable/script's mode instead of resurrecting it
    /// at the default umask. `#[serde(default)]` (→ `None`) so journals written
    /// before this field existed still deserialize; a `None` mode means "don't
    /// touch permissions on revert" — today's behavior — and is what non-unix
    /// records always carry.
    #[serde(default)]
    mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NodeKind {
    Missing,
    File,
    Directory,
    Symlink,
}

/// One revertible checkpoint (a turn that changed files).
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub turn: u64,
    pub ts: u64,
    pub files: Vec<String>,
}

/// The pre-`turn` state of one path, reconstructed by [`Checkpoints::revert_to`]
/// from the earliest record that touched it: the turn it was first touched, its
/// pre-image blob hash, its node kind, and its permission mode.
struct RestoreState {
    turn: u64,
    pre: Option<String>,
    kind: Option<NodeKind>,
    mode: Option<u32>,
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
            // The gate: a store we cannot lock is a store we cannot safely share
            // with another agent in this directory, so we decline to open it. The
            // caller turns that into "checkpoints are off for this session" — which
            // is what it already did for every other kind of open failure.
            let _lock = JournalLock::acquire(&cp.dir)?;
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
        // `open` already proved the lock works, so a failure here is something
        // changing underfoot (the data dir went away, fds ran out). Say so and take
        // the turn anyway: refusing to start one would be a worse answer than a
        // checkpoint that might race a second agent in the same directory.
        let _lock = match JournalLock::acquire(&self.dir) {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!("hrdr: checkpoint: {error} — continuing without the journal lock");
                None
            }
        };
        self.reload_records();
        let counter_path = self.dir.join("next-turn");
        let reserved = std::fs::read_to_string(&counter_path)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_else(|| {
                self.records
                    .iter()
                    .map(|record| record.turn)
                    .max()
                    .unwrap_or(0)
                    + 1
            });
        self.turn = reserved;
        let _ = std::fs::write(counter_path, (reserved + 1).to_string());
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
        // record that references it. Without the lock we cannot promise that, so
        // take the path this function already has for "couldn't snapshot it": skip
        // the checkpoint, say why, and leave `touched` clear so a later write this
        // turn can still try. The edit itself goes ahead — a file that is not
        // revertible is a smaller loss than a turn that dies.
        let _lock = match JournalLock::acquire(&self.dir) {
            Ok(lock) => lock,
            Err(error) => {
                self.touched.remove(&key);
                eprintln!(
                    "hrdr: checkpoint: {error} — {} won't be revertible for this turn",
                    path.display()
                );
                return;
            }
        };
        let (pre, mode) = match std::fs::read(path) {
            Ok(bytes) => match self.store_blob(&bytes) {
                // Capture the file's permission mode alongside its content so a
                // revert restores it in place (see `revert_to`'s `File` arm)
                // rather than recreating it at the default umask.
                Ok(hash) => (Some(hash), file_mode(path)),
                // Couldn't store the blob (e.g. disk full). Mirror the sibling
                // read-failure arm below: drop the key from `touched` so a later
                // write this turn can retry, and say why — don't silently return
                // and leave the change looking checkpointed when it isn't.
                Err(e) => {
                    self.touched.remove(&key);
                    eprintln!(
                        "hrdr: checkpoint: couldn't store a pre-image blob for {} ({e}) — \
                         this file won't be revertible for this turn",
                        path.display()
                    );
                    return;
                }
            },
            // The file genuinely didn't exist before this turn's write — a
            // revert should delete it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
            // Some other read failure (permissions, I/O error, …): the file
            // may well exist. Recording `pre = None` here would make a later
            // revert *delete* a file that was never actually new — worse than
            // doing nothing. Log and skip the checkpoint for this touch
            // instead of recording a bad one; `touched` stays unmarked so a
            // later successful read this turn can still record it.
            Err(e) => {
                self.touched.remove(&key);
                eprintln!(
                    "hrdr: checkpoint: couldn't read {} before recording a change ({e}) — \
                     this file won't be revertible for this turn",
                    path.display()
                );
                return;
            }
        };
        let rec = ChangeRecord {
            turn: self.turn,
            ts: crate::unix_now(),
            path: key,
            pre,
            kind: None,
            mode,
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

    pub fn record_missing_pre(&mut self, path: &Path) -> Result<()> {
        let key = path.to_string_lossy().to_string();
        if !self.touched.insert(key.clone()) {
            return Ok(());
        }
        if std::fs::symlink_metadata(path).is_ok() {
            self.touched.remove(&key);
            anyhow::bail!("{} exists; cannot checkpoint it as missing", path.display());
        }
        self.append_record(ChangeRecord {
            turn: self.turn,
            ts: crate::unix_now(),
            path: key,
            pre: None,
            kind: Some(NodeKind::Missing),
            mode: None,
        })
    }

    /// Snapshot an entire filesystem tree, including empty directories and
    /// symlink identity. Records children before parents so revert can recreate
    /// parent directories before restoring their contents.
    pub fn record_tree_pre(&mut self, root: &Path) -> Result<()> {
        // Keep blob creation and journal references under one lock so concurrent
        // garbage collection cannot remove a newly stored tree blob.
        let _lock = JournalLock::acquire(&self.dir)?;
        let mut nodes = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("reading checkpoint metadata for {}", path.display()))?;
            if metadata.file_type().is_dir() {
                let mut children = Vec::new();
                for entry in std::fs::read_dir(&path)
                    .with_context(|| format!("reading directory {}", path.display()))?
                {
                    children.push(entry?.path());
                }
                stack.extend(children);
            }
            nodes.push(path);
        }
        nodes.sort_by_key(|path| path.components().count());
        for path in nodes {
            self.record_node_pre(&path)?;
        }
        Ok(())
    }

    fn record_node_pre(&mut self, path: &Path) -> Result<()> {
        let key = path.to_string_lossy().to_string();
        if !self.touched.insert(key.clone()) {
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading checkpoint metadata for {}", path.display()))?;
        let (kind, pre, mode) = if metadata.file_type().is_dir() {
            (NodeKind::Directory, None, None)
        } else if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(path)
                .with_context(|| format!("reading symlink {}", path.display()))?;
            let hash = self.store_blob(target.to_string_lossy().as_bytes())?;
            (NodeKind::Symlink, Some(hash), None)
        } else if metadata.file_type().is_file() {
            let hash = self.store_blob(&std::fs::read(path)?)?;
            (NodeKind::File, Some(hash), file_mode(path))
        } else {
            self.touched.remove(&key);
            anyhow::bail!(
                "unsupported filesystem object in checkpoint: {}",
                path.display()
            );
        };
        self.append_record_unlocked(ChangeRecord {
            turn: self.turn,
            ts: crate::unix_now(),
            path: key,
            pre,
            kind: Some(kind),
            mode,
        })
    }

    fn append_record(&mut self, rec: ChangeRecord) -> Result<()> {
        let _lock = JournalLock::acquire(&self.dir)?;
        self.append_record_unlocked(rec)
    }

    fn append_record_unlocked(&mut self, rec: ChangeRecord) -> Result<()> {
        let line = serde_json::to_string(&rec)?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)?;
        writeln!(file, "{line}")?;
        self.records.push(rec);
        Ok(())
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
        let _lock = JournalLock::acquire(&self.dir)?;
        self.reload_records();
        // For each file touched in turns >= `turn`, the pre-`turn` state is the
        // pre-image recorded at the SMALLEST such turn.
        let mut earliest: BTreeMap<String, RestoreState> = BTreeMap::new();
        for r in self.records.iter().filter(|r| r.turn >= turn) {
            let e = earliest.entry(r.path.clone()).or_insert(RestoreState {
                turn: r.turn,
                pre: r.pre.clone(),
                kind: r.kind,
                mode: r.mode,
            });
            if r.turn < e.turn {
                *e = RestoreState {
                    turn: r.turn,
                    pre: r.pre.clone(),
                    kind: r.kind,
                    mode: r.mode,
                };
            }
        }
        let mut restore_entries: Vec<_> = earliest.into_iter().collect();
        restore_entries.sort_by_key(|(path, _)| Path::new(path).components().count());
        let mut restored = Vec::new();
        for (
            path,
            RestoreState {
                pre, kind, mode, ..
            },
        ) in &restore_entries
        {
            let p = PathBuf::from(path);
            let effective = kind.unwrap_or(if pre.is_some() {
                NodeKind::File
            } else {
                NodeKind::Missing
            });
            match effective {
                NodeKind::Directory => std::fs::create_dir_all(&p)
                    .with_context(|| format!("restoring directory {}", p.display()))?,
                NodeKind::File => {
                    let bytes =
                        self.load_blob(pre.as_deref().context("file checkpoint has no blob")?)?;
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    // Write in place when the existing path is already a regular
                    // file: truncate-and-write keeps the inode, so any hardlinks
                    // survive and the file's own permission bits aren't reset by
                    // an unlink+recreate at the default umask. Only when the node
                    // type changed (was a dir/symlink) or it's gone do we
                    // unlink+recreate.
                    let existing_is_regular_file = std::fs::symlink_metadata(&p)
                        .map(|m| m.file_type().is_file())
                        .unwrap_or(false);
                    if !existing_is_regular_file {
                        remove_existing_node(&p)?;
                    }
                    std::fs::write(&p, bytes)
                        .with_context(|| format!("restoring {}", p.display()))?;
                    // Restore the recorded permission mode. Absent (legacy record,
                    // or non-unix) → leave permissions as-is, today's behavior.
                    restore_mode(&p, *mode)?;
                }
                NodeKind::Symlink => {
                    let bytes =
                        self.load_blob(pre.as_deref().context("symlink checkpoint has no blob")?)?;
                    let target = PathBuf::from(
                        String::from_utf8(bytes).context("symlink target is not UTF-8")?,
                    );
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    remove_existing_node(&p)?;
                    create_symlink(&target, &p)?;
                }
                NodeKind::Missing => remove_existing_node(&p)?,
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let hash = sha256_hex(bytes);
        let path = self.blobs_dir.join(&hash);
        if !path.exists() {
            let compressed = miniz_oxide::deflate::compress_to_vec(bytes, 6);
            // Write to a temp sibling then rename into place: a crash mid-write
            // leaves only the throwaway temp (GC'd as an orphan), never a
            // truncated blob under its content hash that would silently decompress
            // wrong on a later revert. Rename is atomic on the same filesystem.
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let tmp = self
                .blobs_dir
                .join(format!(".tmp-{}-{seq}", std::process::id()));
            std::fs::write(&tmp, compressed).with_context(|| format!("writing blob {hash}"))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("finalizing blob {hash}"))?;
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
        // Write to `<journal>.tmp` then rename over the target so a crash mid-write
        // can't truncate the journal and lose every checkpoint — the rename either
        // fully lands the new journal or leaves the old one intact.
        let mut tmp = self.journal_path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, out).context("writing checkpoint journal")?;
        std::fs::rename(&tmp, &self.journal_path).context("rewriting checkpoint journal")?;
        Ok(())
    }
}

/// The Unix permission mode of the regular file at `path` (`None` off unix, or
/// if it can't be stat'd — a missing mode just means "don't touch permissions
/// on revert"). Captured at snapshot time and replayed by [`restore_mode`].
#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.mode())
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

/// Reapply a recorded permission mode to a just-restored regular file. `None`
/// (legacy record with no mode, or non-unix) is a no-op — preserving the prior
/// behavior where revert never adjusted permissions.
#[cfg(unix)]
fn restore_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("restoring permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

fn remove_existing_node(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(target: &Path, path: &Path) -> Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, path)?;
    } else {
        std::os::windows::fs::symlink_file(target, path)?;
    }
    Ok(())
}

/// Exclusive OS-backed lock over the checkpoint journal and blob GC.
struct JournalLock {
    file: std::fs::File,
}

impl JournalLock {
    /// Take the lock, or say why not.
    ///
    /// This is fallible because the ways it fails are *environmental*, not bugs: a
    /// filesystem with no `flock` (NFS without lockd, some FUSE and container
    /// volume mounts), a data directory that isn't writable, an exhausted file
    /// descriptor table. None of those are hrdr's fault and none of them are worth
    /// a dead agent — the store lives under the user's XDG data dir, and a home
    /// directory on NFS is an ordinary corporate setup.
    ///
    /// It used to `panic!` on both. That crashed the turn *inside* a bare
    /// `tokio::spawn`, so the TUI never got its `Done` message: the loader span
    /// forever, input queued instead of sending, and nothing said why — while the
    /// panic hook tore the terminal out of the alternate screen. Worse, the one
    /// caller that had *designed* a graceful path for this — `Checkpoints::open`,
    /// whose `Err` disables checkpointing via `.ok()` — could never reach it,
    /// because `.ok()` does not catch a panic.
    ///
    /// Locking still gates the store: `open` refuses to hand back a `Checkpoints`
    /// it cannot lock, so a session either has a store it can serialise against or
    /// no store at all. What it never has again is a crash.
    fn acquire(dir: &Path) -> Result<Self> {
        use fs2::FileExt;
        let path = dir.join("journal.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening checkpoint lock {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("locking checkpoint journal {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
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

    /// A store that cannot be locked is *declined*, not fatal.
    ///
    /// The ways the lock fails are environmental — a filesystem with no `flock`
    /// (NFS without lockd, some FUSE and container mounts), a data dir that isn't
    /// writable, an exhausted fd table — and the store lives under the user's XDG
    /// data dir, so a home directory on NFS is enough to hit it.
    ///
    /// It used to `panic!`. Inside the TUI that killed the turn in a bare
    /// `tokio::spawn`, so the `Done` message never arrived: the loader span forever,
    /// input queued instead of sending, nothing said why. And the one caller that
    /// had a graceful path for exactly this — `Agent::new`, which disables
    /// checkpointing when `open` returns `Err` — could never reach it, because
    /// `.ok()` does not catch a panic.
    ///
    /// Simulated with the failure that is portable: a lock path that cannot be
    /// opened as a file, because a directory already sits there.
    #[test]
    fn a_store_that_cannot_be_locked_is_declined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("cp");
        // `journal.lock` is where the lock file goes; make that name unopenable.
        std::fs::create_dir_all(store.join("journal.lock")).unwrap();

        let err = match Checkpoints::open(store) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a store whose lock cannot be taken must not open"),
        };
        assert!(
            err.contains("checkpoint lock"),
            "the error must name what failed, so the user knows why /undo is gone: {err}"
        );
    }

    /// And the graceful path is real: `Agent::new`'s `Err` arm turns that into
    /// "checkpoints off", which only works because `open` *returns* rather than
    /// panics. This pins the contract that arm depends on.
    #[test]
    fn open_reports_failure_by_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("cp");
        std::fs::create_dir_all(store.join("journal.lock")).unwrap();

        // The whole point: a caller can *handle* this. If `open` panicked, this
        // line would take the test process down instead of yielding `None`.
        let handled = Checkpoints::open(store).ok();
        assert!(handled.is_none(), "an unlockable store yields no store");
    }

    #[test]
    fn revert_restores_directory_tree_empty_dirs_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("empty/nested")).unwrap();
        std::fs::write(tree.join("file.txt"), "content").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("file.txt", tree.join("link.txt")).unwrap();

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        cp.begin_turn();
        cp.record_tree_pre(&tree).unwrap();
        std::fs::remove_dir_all(&tree).unwrap();
        cp.revert_last().unwrap();

        assert_eq!(
            std::fs::read_to_string(tree.join("file.txt")).unwrap(),
            "content"
        );
        assert!(tree.join("empty/nested").is_dir());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(tree.join("link.txt")).unwrap(),
            PathBuf::from("file.txt")
        );
    }

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

    /// A read failure other than "file doesn't exist" (here: permission
    /// denied) must not be recorded as `pre = None` — that would make
    /// `revert_to` *delete* a file that actually existed. No record at all is
    /// recorded instead: the change simply isn't revertible for this turn,
    /// which is honest, whereas a phantom "didn't exist" record is actively
    /// wrong. Unix-only: relies on real permission enforcement.
    #[cfg(unix)]
    #[test]
    fn unreadable_existing_file_is_not_recorded_as_absent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("locked.txt");
        std::fs::write(&f, "secret content").unwrap();
        // Remove all permissions so std::fs::read fails with something other
        // than NotFound.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Skip if running as root (root ignores permission bits, so the
        // premise of this test — a non-NotFound read failure — wouldn't hold).
        if std::fs::read(&f).is_ok() {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        cp.begin_turn();
        cp.record_pre(&f);

        // Restore permissions so cleanup (and the revert below) can proceed.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        // No record was written for this file — nothing to revert, and in
        // particular no revert will delete the still-existing file.
        assert!(
            cp.list().is_empty(),
            "no checkpoint should have been recorded"
        );
        assert!(f.exists(), "the file must still exist");
        let restored = cp.revert_last().unwrap();
        assert!(restored.is_empty());
        assert!(
            f.exists(),
            "revert must not delete a file that was never new"
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

    /// Revert restores the file's permission mode captured at snapshot time — an
    /// 0755 script must not come back 0644 after `/undo`.
    #[cfg(unix)]
    #[test]
    fn revert_restores_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("script.sh");
        std::fs::write(&f, "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        cp.begin_turn();
        cp.record_pre(&f);
        // The edit both rewrites content and (as an unlink+recreate would) drops
        // the exec bit.
        std::fs::write(&f, "changed").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "#!/bin/sh\necho hi\n");
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "revert must restore the recorded 0755 mode");
    }

    /// Reverting an existing regular file writes in place (no unlink), so a
    /// hardlink to it survives the revert instead of being severed.
    #[cfg(unix)]
    #[test]
    fn revert_preserves_hardlinks() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        let link = dir.path().join("b.txt");
        std::fs::write(&f, "original").unwrap();
        std::fs::hard_link(&f, &link).unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().ino(),
            std::fs::metadata(&link).unwrap().ino()
        );

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        cp.begin_turn();
        cp.record_pre(&f);
        std::fs::write(&f, "changed").unwrap();

        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
        // Same inode → the hardlink was never broken, and the sibling name sees
        // the reverted content too.
        assert_eq!(
            std::fs::metadata(&f).unwrap().ino(),
            std::fs::metadata(&link).unwrap().ino(),
            "revert must not sever the hardlink"
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "original");
    }

    /// A legacy journal record (no `mode`/`kind`/`ts` fields) still deserializes
    /// via `#[serde(default)]` and reverts, falling back to today's behavior of
    /// not adjusting permissions.
    #[test]
    fn legacy_record_without_mode_field_deserializes_and_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let cpdir = dir.path().join("cp");
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();

        // Snapshot once to produce a blob for "v0", then hand-rewrite the journal
        // as a legacy line carrying only turn/path/pre (the pre-mode schema).
        {
            let mut cp = Checkpoints::open(cpdir.clone()).unwrap();
            cp.begin_turn();
            cp.record_pre(&f);
            std::fs::write(&f, "v1").unwrap();
        }
        let journal = cpdir.join("journal.jsonl");
        let text = std::fs::read_to_string(&journal).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let hash = first["pre"].as_str().unwrap().to_string();
        let legacy = serde_json::json!({
            "turn": 1,
            "path": f.to_string_lossy(),
            "pre": hash,
        });
        std::fs::write(&journal, serde_json::to_string(&legacy).unwrap() + "\n").unwrap();

        // Reopen (must not drop the record) and revert (must restore v0 without
        // touching permissions).
        let mut cp = Checkpoints::open(cpdir.clone()).unwrap();
        assert_eq!(cp.list().len(), 1, "legacy record must survive deserialize");
        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v0");
    }

    /// The happy path leaves no partial/temp file behind: `store_blob` renames
    /// its temp sibling into place, so only the final content-addressed blob
    /// remains.
    #[test]
    fn store_blob_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        let hash = cp.store_blob(b"durable payload").unwrap();

        let entries: Vec<String> = std::fs::read_dir(&cp.blobs_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec![hash],
            "only the finalized blob remains — no `.tmp-*` sibling"
        );
    }
}
