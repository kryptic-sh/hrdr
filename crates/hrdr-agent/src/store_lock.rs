//! Cross-process write lock for hrdr's on-disk stores.
//!
//! Two stores use it, for the same underlying reason — an atomic rename protects
//! *readers* but does not serialize *writers*:
//!
//! * The **credential store** (`auth.json`) is updated read-modify-write: read
//!   the whole store, change one entry, then atomically rename a temp over the
//!   target. Two processes can each read the same old store, add a different
//!   provider, and both rename; the second rename wins and the first process's
//!   new entry is lost.
//! * The **attachment blob store** (`sessions/<cwd-slug>/blobs/`) is garbage
//!   collected mark-and-sweep, while another process may be writing a blob and
//!   the session file that references it. Both sides take this lock — the writer
//!   across blob write + session write, the collector across mark + sweep — so
//!   the collector never observes the gap between the two.
//!
//! This module closes that gap with an advisory cross-process lock built on the
//! same zero-dependency `O_EXCL` reservation scheme the session store uses (see
//! `hrdr-app`'s `session.rs`). A writer takes the lock, *then* does the whole
//! read-modify-write, then drops the lock — so a concurrent writer waits for the
//! rename to land and re-reads the merged store instead of an older snapshot.
//!
//! ## Design
//!
//! * The lock is a sibling file, `<store>.lock`, created with `create_new(true)`
//!   (`O_EXCL`) so exactly one process can hold it. Its content is
//!   `PID TIMESTAMP` (space-separated), matching the session reservation format,
//!   so a concurrent process can judge staleness.
//! * [`StoreLock`] is an RAII guard: dropping it removes the lock file, on every
//!   exit path (normal return, `?`, panic) — but only if the file still carries
//!   this guard's PID, so a lock another process reaped and re-claimed is never
//!   deleted out from under its new owner. A crash between create and drop
//!   leaves the file behind, which the staleness check below reaps.
//! * **Staleness**: a lock whose owning PID is gone *and* whose timestamp is
//!   older than its [`StoreKind`]'s age is reaped and re-claimed. A lock whose
//!   content doesn't parse (an empty or truncated file) is aged by its mtime
//!   alone so it can never wedge the store forever. The age is per-kind because
//!   it is a property of the work done under the lock, not of this module — see
//!   [`StoreKind`].
//! * **Bounded retries**: acquisition spins with a short sleep up to
//!   [`LOCK_ACQUIRE_ATTEMPTS`] times (≈ a few seconds total). If it still can't
//!   claim the lock it returns an error rather than blocking forever — an
//!   unwritable directory or a wedged peer surfaces cleanly instead of hanging.
//!
//! ## Policy (credential store)
//!
//! Same-key concurrent writers are **last-writer-wins**: two processes both
//! logging in to the *same* provider serialize on the lock, and whichever runs
//! its read-modify-write second overwrites the first's value for that key. That
//! is the intended behavior — the later login is the fresher credential.
//! *Different*-key concurrent writers both survive: the second writer re-reads
//! the store the first one wrote and merges its own key in.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};

/// [`StoreKind::SmallFileRewrite`]'s staleness age. A writer holds the lock for
/// one read-modify-write of a tiny file, so a lock much older than this almost
/// certainly belongs to a crashed process. Kept generous so a legitimately slow
/// filesystem doesn't get its lock stolen mid-write.
const SMALL_FILE_STALE_LOCK_AGE_SECS: u64 = 60;

/// A pessimistic write-throughput floor, for sizing how long a lock may honestly
/// be held. Not a disk's speed: it is what a congested network share or a synced
/// home directory degrades to, which is the case a staleness age has to survive
/// without stealing a live writer's lock.
const SLOW_FS_BYTES_PER_SEC: u64 = 1024 * 1024;

/// [`StoreKind::BlobStore`]'s staleness age, sized from the work a save does
/// under that lock rather than from a round figure.
///
/// `Session::save` holds it across `attachment_store::write_blobs` *and* the
/// atomic write of the session file, so both are budgeted at
/// [`SLOW_FS_BYTES_PER_SEC`]: the session file at
/// [`MAX_SESSION_FILE_BYTES`](crate::session::MAX_SESSION_FILE_BYTES) — the size
/// past which it can no longer be loaded, so the ceiling on the half that has
/// one — and the same allowance again for the blobs it names, which have no
/// ceiling this crate can state (each attachment is bounded by `hrdr_llm`'s
/// provider caps, their number by nothing).
const BLOB_STORE_STALE_LOCK_AGE_SECS: u64 =
    2 * crate::session::MAX_SESSION_FILE_BYTES / SLOW_FS_BYTES_PER_SEC;

/// How many times [`StoreLock::acquire`] retries a contended lock before giving
/// up. With [`LOCK_RETRY_DELAY`] this bounds the wait so an unwritable directory
/// (every attempt fails) or a wedged peer cannot hang a login forever.
const LOCK_ACQUIRE_ATTEMPTS: u32 = 100;

/// Delay between acquisition attempts. Together with [`LOCK_ACQUIRE_ATTEMPTS`]
/// it bounds the worst-case wait, which comfortably outlasts any honest
/// read-modify-write of a small credential file while still failing fast against
/// a truly stuck lock.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Which store a lock guards — and therefore how long its holder may honestly
/// hold it before a peer is entitled to call the lock abandoned.
///
/// **Every acquire names one, and there is deliberately no default.** The
/// staleness age is a property of the work done under the lock, so a parameter a
/// caller could leave out would hand the short age to a long-held lock in
/// silence — which is exactly the defect this replaced: a save writing megabytes
/// of attachments aged as if it were a credential file's read-modify-write.
///
/// **The age only decides anything on a platform with no liveness probe.**
/// Everywhere else [`process_alive`] answers first: a lock whose owner is still
/// running is never reaped, whatever its age. Windows is that platform — no
/// dependency-free probe, so every pid reads as dead — and there this age is the
/// whole protection a live writer has. A real `OpenProcess` /
/// `GetExitCodeProcess` probe would make both ages a formality again and is the
/// better fix; it needs a Windows API dependency, which is the repo owner's call
/// rather than this module's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// One read-modify-write of a small file — `auth.json`, `config.toml`: read
    /// the whole thing, change one key, rename a temp over it. Milliseconds.
    SmallFileRewrite,
    /// The attachment blob store. Held by a save across writing every new blob
    /// *and* the session file that names them, which is tens of megabytes once a
    /// conversation carries images or PDFs.
    BlobStore,
}

impl StoreKind {
    /// How old (seconds) a lock of this kind must be before a peer may reap it.
    const fn stale_age_secs(self) -> u64 {
        match self {
            Self::SmallFileRewrite => SMALL_FILE_STALE_LOCK_AGE_SECS,
            Self::BlobStore => BLOB_STORE_STALE_LOCK_AGE_SECS,
        }
    }
}

/// RAII cross-process write lock for one on-disk store.
///
/// Held for the duration of one read-modify-write. Dropping it releases the lock
/// (removes the lock file) on every exit path, so a failed or panicking write
/// never leaves a permanent lock behind — but only a lock this guard still
/// owns: if a concurrent process reaped ours as stale and re-claimed the path,
/// `Drop` leaves the new owner's lock alone. See the module docs for the full
/// design.
#[derive(Debug)]
pub struct StoreLock {
    lock_path: PathBuf,
    /// PID written into the lock file at acquire — the only identity that may
    /// release this lock.
    pid: u32,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Release only a lock this guard still owns — see
        // `remove_lock_file_if_owned` for the reap/reclaim race that check is
        // there for.
        remove_lock_file_if_owned(&self.lock_path, self.pid);
    }
}

impl StoreLock {
    /// Acquire the write lock for the store at `store_path` — a file or a
    /// directory, whichever the store is — blocking (with a bounded retry loop)
    /// until it is free or [`LOCK_ACQUIRE_ATTEMPTS`] is exhausted.
    ///
    /// The lock itself is the sibling `<store_path>.lock`, so `store_path` need
    /// not exist; its parent directory must (the callers `create_dir_all` it
    /// before locking). Returns an error — never hangs — when the lock stays
    /// contended past the retry budget or the directory is unwritable.
    ///
    /// `kind` says what is done under the lock, which is what sets how long a
    /// peer must wait before reaping it — see [`StoreKind`].
    pub fn acquire(store_path: &Path, kind: StoreKind) -> Result<Self> {
        let lock_path = hrdr_llm::sibling_with_suffix(store_path, ".lock");
        let stale_age_secs = kind.stale_age_secs();
        for _ in 0..LOCK_ACQUIRE_ATTEMPTS {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut f) => {
                    // Record owner PID + creation time so a concurrent process
                    // can later judge this lock's staleness. Best-effort: even
                    // an empty lock file is reap-able (aged by mtime).
                    let content = format!("{} {}", std::process::id(), hrdr_tools::unix_now());
                    let _ = f.write_all(content.as_bytes());
                    let _ = f.flush();
                    drop(f);
                    return Ok(StoreLock {
                        lock_path,
                        pid: std::process::id(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Someone holds it. Reap it if stale and retry immediately;
                    // otherwise wait a beat and try again (bounded).
                    if is_stale_lock(&lock_path, stale_age_secs) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Windows only: a lock file in the "delete pending" state —
                    // a concurrent holder's `Drop` is still removing it while a
                    // handle lingers — rejects `CreateFile` with
                    // ERROR_ACCESS_DENIED (os error 5) until the last handle
                    // closes. That is transient contention, not a real
                    // permission fault, so treat it like `AlreadyExists`: reap a
                    // stale lock, else wait and retry (bounded). A genuinely
                    // unwritable directory still terminates — via the timeout
                    // below rather than an immediate error.
                    if is_stale_lock(&lock_path, stale_age_secs) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(e) => {
                    // A non-contention error (e.g. an unwritable directory) will
                    // not fix itself by retrying — surface it right away.
                    return Err(anyhow!("acquiring lock {}: {e}", lock_path.display()));
                }
            }
        }
        Err(anyhow!(
            "timed out acquiring lock {} (held by another process?)",
            lock_path.display()
        ))
    }
}

/// Whether the lock file at `path` is stale — owned by a dead process and older
/// than `stale_age_secs`, or unparseable and that old by mtime.
///
/// `stale_age_secs` comes from the holder's [`StoreKind`], since how long a lock
/// may honestly be held is decided by the work under it. The session store's
/// open-lock and id-reservation paths call this too (with their own
/// `STALE_LOCK_AGE_SECS`), so one predicate serves both lock schemes.
///
/// Parses `PID TIMESTAMP`; if the content doesn't parse (empty/truncated lock)
/// it falls back to the file's mtime so an unparseable lock can still be aged
/// out rather than wedging the store forever.
pub(crate) fn is_stale_lock(path: &Path, stale_age_secs: u64) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        // The lock vanished between the failed create and this read — treat it
        // as not-stale; the next acquire attempt will re-race the create.
        return false;
    };
    let mut parts = content.split_whitespace();
    let parsed: Option<(u32, u64)> = parts
        .next()
        .and_then(|p| p.parse().ok())
        .zip(parts.next().and_then(|t| t.parse().ok()));
    let Some((pid, ts)) = parsed else {
        // Unparseable owner: no PID to probe, so judge by mtime alone.
        let age = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok());
        return age.is_some_and(|a| a.as_secs() >= stale_age_secs);
    };
    let now = hrdr_tools::unix_now();
    // Not old enough yet — a live writer may legitimately hold it.
    if now < ts || now.saturating_sub(ts) < stale_age_secs {
        return false;
    }
    // Old enough: reap only if the owning process is really gone.
    if process_alive(pid) {
        return false;
    }
    true
}

/// Remove the lock file at `lock_path` — but only if it still names `pid` as
/// its owner.
///
/// Deleting by path alone is the race: a holder that was reaped as stale
/// (Windows has no liveness probe, so [`process_alive`] reports every pid dead
/// past the staleness age) has its lock reclaimed by a second process, and the
/// original holder's `Drop` then deletes the *new* holder's lock mid-write —
/// the lost-update these locks exist to prevent. A lock we cannot parse to
/// `pid`, or that no longer exists, needs no removal.
pub(crate) fn remove_lock_file_if_owned(lock_path: &Path, pid: u32) {
    let owned = match std::fs::read_to_string(lock_path) {
        Ok(content) => {
            content
                .split_whitespace()
                .next()
                .and_then(|p| p.parse::<u32>().ok())
                == Some(pid)
        }
        // No lock to read — already removed or never reclaimed — nothing to do.
        Err(_) => false,
    };
    if owned {
        let _ = std::fs::remove_file(lock_path);
    }
}

/// Best-effort check for whether process `pid` is still alive, zero-dependency.
/// Errs on the side of "alive" (returns `true` when it can't tell) so a live
/// writer's lock is never stolen on a platform where the probe is unavailable.
pub(crate) fn process_alive(pid: u32) -> bool {
    // `/proc/<pid>` exists iff the process exists — no syscall crate needed.
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    // `kill -0` probes existence without sending a signal.
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(true)
    }
    // No cheap, dependency-free liveness probe on Windows. The caller only
    // reaches here once the lock is already older than its `StoreKind`'s
    // staleness age, so assume the owner is gone — a crashed writer's lock can
    // still be reaped. That age is therefore the only thing keeping a live
    // writer's lock safe here, which is why each kind states its own; a real
    // `OpenProcess`/`GetExitCodeProcess` probe would be the better fix, at the
    // cost of a Windows API dependency (see `StoreKind`).
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_sibling_with_lock_suffix() {
        let p = Path::new("/some/dir/auth.toml");
        assert_eq!(
            hrdr_llm::sibling_with_suffix(p, ".lock"),
            PathBuf::from("/some/dir/auth.toml.lock")
        );
        let p = Path::new("/x/oauth.json");
        assert_eq!(
            hrdr_llm::sibling_with_suffix(p, ".lock"),
            PathBuf::from("/x/oauth.json.lock")
        );
    }

    #[test]
    fn acquire_creates_and_drop_removes_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        {
            let _guard = StoreLock::acquire(&store, StoreKind::SmallFileRewrite).unwrap();
            assert!(lock.exists(), "lock file exists while held");
        }
        assert!(!lock.exists(), "lock file removed on drop");
    }

    #[test]
    fn drop_leaves_a_reclaimed_lock_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        let guard = StoreLock::acquire(&store, StoreKind::SmallFileRewrite).unwrap();
        // A concurrent process reaped our lock as stale (Windows has no
        // liveness probe, so every pid reads as dead past the staleness age)
        // and re-claimed the path with its own pid.
        std::fs::write(&lock, format!("{} {}", 999_999, hrdr_tools::unix_now())).unwrap();
        drop(guard);
        assert!(
            lock.exists(),
            "the old owner's drop must not delete the new owner's lock"
        );
    }

    #[test]
    fn second_acquire_while_held_times_out_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let _held = StoreLock::acquire(&store, StoreKind::SmallFileRewrite).unwrap();
        // A live (this process) lock is never stale, so a second acquire runs
        // out its retry budget and errors instead of hanging or corrupting.
        let err = StoreLock::acquire(&store, StoreKind::SmallFileRewrite)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out acquiring"), "{err}");
    }

    #[test]
    fn stale_lock_with_dead_pid_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        // A dead PID with an old timestamp: PID 4294967294 is effectively never
        // a live process, and the timestamp is well past the staleness window.
        let old = hrdr_tools::unix_now().saturating_sub(SMALL_FILE_STALE_LOCK_AGE_SECS + 60);
        std::fs::write(&lock, format!("4294967294 {old}")).unwrap();
        // Acquire must reap the stale lock and succeed on the first pass.
        let _guard = StoreLock::acquire(&store, StoreKind::SmallFileRewrite).unwrap();
        assert!(lock.exists(), "our fresh lock replaced the stale one");
    }

    // Unix-only: exercising mtime-based reaping needs to backdate the lock's
    // mtime, which we do with `touch` (see `set_mtime`). Windows has no
    // dependency-free mtime setter here, so `set_mtime` is a no-op there and the
    // lock would never age out — the reaping path itself is unix/Windows-agnostic
    // and covered on unix CI.
    #[cfg(unix)]
    #[test]
    fn unparseable_old_lock_is_reaped_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        // An empty/garbage lock file (e.g. a truncated write) with no PID.
        std::fs::write(&lock, b"").unwrap();
        // Backdate its mtime past the staleness window so it ages out.
        let old =
            std::time::SystemTime::now() - Duration::from_secs(SMALL_FILE_STALE_LOCK_AGE_SECS + 60);
        set_mtime(&lock, old);
        let _guard = StoreLock::acquire(&store, StoreKind::SmallFileRewrite).unwrap();
        assert!(lock.exists());
    }

    #[test]
    fn fresh_unparseable_lock_is_not_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        // A just-written garbage lock (fresh mtime) must NOT be treated as
        // stale — a live writer may be mid-write with a slow flush.
        std::fs::write(&lock, b"garbage-no-pid").unwrap();
        let err = StoreLock::acquire(&store, StoreKind::SmallFileRewrite)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out acquiring"), "{err}");
    }

    /// **The blob store's lock outlives a credential file's.** A save holds it
    /// across writing attachment blobs *and* the session file, so it must not be
    /// reapable at the age that suits a read-modify-write of `auth.json` — which
    /// is the whole reason the age hangs off [`StoreKind`] rather than the
    /// module.
    ///
    /// Against [`is_stale_lock`] rather than [`StoreLock::acquire`]: the
    /// judgement is what is under test, and proving the negative half through
    /// `acquire` would spend its whole retry budget doing it.
    #[test]
    fn a_blob_store_lock_outlives_a_small_file_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("blobs.lock");
        // An age between the two windows, so each kind must answer differently.
        let between = SMALL_FILE_STALE_LOCK_AGE_SECS + 1;
        assert!(
            between < BLOB_STORE_STALE_LOCK_AGE_SECS,
            "the blob store's window is the longer one, or this test proves nothing"
        );
        // A dead owner (pid 4294967294 is effectively never a live process), so
        // age is the only thing left to decide it.
        let ts = hrdr_tools::unix_now().saturating_sub(between);
        std::fs::write(&lock, format!("4294967294 {ts}")).unwrap();

        assert!(
            is_stale_lock(&lock, StoreKind::SmallFileRewrite.stale_age_secs()),
            "a credential lock this old is abandoned"
        );
        assert!(
            !is_stale_lock(&lock, StoreKind::BlobStore.stale_age_secs()),
            "the same age is a save still writing its blobs"
        );
    }

    // ── the lock between two real processes ───────────────────────────────
    //
    // Every test above holds the guard from a thread of one process, where a
    // plain `Mutex` would behave identically — so none of them exercises what
    // `O_EXCL` actually promises. These two re-execute this very test binary
    // (`current_exe`) with libtest's own arguments, pointing it at the
    // `#[ignore]`d child test below. No new dependency, and no second binary to
    // build.

    /// Directory the re-executed child takes its lock in. Set only by
    /// [`LockHolder::spawn`]; absent in an ordinary run, which is what makes the
    /// child test a no-op rather than a hang for anyone running
    /// `cargo test -- --ignored`.
    const CHILD_DIR_ENV: &str = "HRDR_TEST_STORE_LOCK_DIR";
    /// Set when the child should leave its lock behind instead of releasing it.
    const CHILD_CRASH_ENV: &str = "HRDR_TEST_STORE_LOCK_CRASH";
    /// libtest path of the child test, as [`LockHolder::spawn`] asks for it. A
    /// rename that misses this makes the child run no test at all — which shows
    /// up as the ready file never appearing, not as a pass.
    const CHILD_TEST_PATH: &str = "store_lock::tests::child_process_holds_the_lock";
    /// The store both sides lock, inside the shared directory.
    const CHILD_STORE_NAME: &str = "auth.toml";

    /// How long the parent waits for the child to signal, and to exit. Generous
    /// enough for a cold process spawn on a loaded runner, and bounded so a
    /// child that never starts fails the test instead of hanging the suite.
    const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
    /// How long the child holds the lock waiting to be released — bounded for
    /// the mirror-image reason: a parent that died must not leave a process
    /// behind sitting on a lock.
    const CHILD_HOLD_TIMEOUT: Duration = Duration::from_secs(60);
    /// Gap between polls of a signal file.
    const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// The child creates this once it *holds* the lock — a signal the parent can
    /// poll for, rather than a guessed sleep that is either flaky or slow.
    fn ready_file(dir: &Path) -> PathBuf {
        dir.join("child-holds-the-lock")
    }

    /// The parent creates this to tell the child to let go.
    fn release_file(dir: &Path) -> PathBuf {
        dir.join("child-may-let-go")
    }

    /// Wait for `path` to appear, up to `limit`; whether it did.
    fn wait_for_file(path: &Path, limit: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < limit {
            if path.exists() {
                return true;
            }
            std::thread::sleep(SIGNAL_POLL_INTERVAL);
        }
        path.exists()
    }

    /// The pid the lock file at `path` names as its owner.
    fn lock_owner_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .expect("the lock file is readable")
            .split_whitespace()
            .next()
            .and_then(|p| p.parse().ok())
            .expect("the lock file names its owner")
    }

    /// A second process holding the lock: this test binary, re-executed.
    ///
    /// `Drop` kills and reaps it on **every** exit path, the assertion-failure
    /// path included — a red test must not leave a process behind still holding
    /// a lock.
    struct LockHolder(std::process::Child);

    impl LockHolder {
        /// Start the holder. `crash` makes it exit without releasing.
        fn spawn(dir: &Path, crash: bool) -> Self {
            let exe = std::env::current_exe().expect("the test binary knows its own path");
            let mut cmd = std::process::Command::new(exe);
            // libtest's own arguments: run exactly the ignored child test and
            // nothing else. `--exact` so no sibling is swept in by a prefix
            // match, `--ignored` because that is the only list it appears on.
            cmd.args(["--exact", "--ignored", "--test-threads=1", CHILD_TEST_PATH])
                .env(CHILD_DIR_ENV, dir)
                // libtest's progress output is not this test's business; stderr
                // is left inherited so a panicking child says why.
                .stdout(std::process::Stdio::null());
            if crash {
                cmd.env(CHILD_CRASH_ENV, "1");
            }
            Self(cmd.spawn().expect("the test binary re-executes"))
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// Wait for the child to exit, up to `limit`; whether it did. Bounded
        /// rather than `wait()`, so a wedged child is a failed assertion.
        fn wait_for_exit(&mut self, limit: Duration) -> bool {
            let started = std::time::Instant::now();
            while started.elapsed() < limit {
                match self.0.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) => std::thread::sleep(SIGNAL_POLL_INTERVAL),
                    Err(_) => return false,
                }
            }
            matches!(self.0.try_wait(), Ok(Some(_)))
        }
    }

    impl Drop for LockHolder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// **A lock one process holds cannot be taken by another** — and is released
    /// to it once that process is gone.
    #[test]
    fn a_second_process_cannot_take_a_held_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(CHILD_STORE_NAME);
        let lock = hrdr_llm::sibling_with_suffix(&store, ".lock");

        let mut child = LockHolder::spawn(dir.path(), false);
        assert!(
            wait_for_file(&ready_file(dir.path()), CHILD_TIMEOUT),
            "the child process took the lock and said so"
        );
        assert_eq!(
            lock_owner_pid(&lock),
            child.pid(),
            "the lock on disk belongs to the other process"
        );
        // Unix-only: `process_alive` has no probe on Windows and answers `false`
        // for every pid, so there this would assert the opposite of the truth.
        // It is the only place the probe meets a real, live, foreign process.
        #[cfg(unix)]
        assert!(
            process_alive(child.pid()),
            "its owner is alive, so the lock is not stale however this test is timed"
        );

        let err = StoreLock::acquire(&store, StoreKind::SmallFileRewrite)
            .expect_err("a lock another process holds cannot be taken")
            .to_string();
        assert!(err.contains("timed out acquiring"), "{err}");

        // Let it go, and wait for the process to really be gone — its guard's
        // `Drop` runs on the way out.
        std::fs::write(release_file(dir.path()), b"").unwrap();
        assert!(
            child.wait_for_exit(CHILD_TIMEOUT),
            "the child exited once released"
        );
        assert!(
            !lock.exists(),
            "the holder's Drop released it across the process boundary"
        );
        StoreLock::acquire(&store, StoreKind::SmallFileRewrite)
            .expect("the lock is free once its holder is gone");
    }

    /// A holder that **died without releasing** — the one case an in-process
    /// test cannot reach, because it can only invent a pid that was never
    /// running. The child takes the lock and leaves through
    /// `std::process::exit`, which runs no destructors, so what it leaves behind
    /// is a real lock file naming a real pid that is really gone.
    ///
    /// Unix-only, deliberately: reaping turns on [`process_alive`], which on
    /// Windows has no dependency-free probe and answers `false` for every pid —
    /// so there this would pass without saying anything about a dead process.
    #[cfg(unix)]
    #[test]
    fn a_dead_holders_lock_is_reaped_by_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(CHILD_STORE_NAME);
        let lock = hrdr_llm::sibling_with_suffix(&store, ".lock");

        let mut child = LockHolder::spawn(dir.path(), true);
        assert!(
            wait_for_file(&ready_file(dir.path()), CHILD_TIMEOUT),
            "the child process took the lock and said so"
        );
        let dead_pid = child.pid();
        assert!(
            child.wait_for_exit(CHILD_TIMEOUT),
            "the child left without releasing"
        );
        assert!(lock.exists(), "it left its lock behind, as a crash does");
        assert_eq!(lock_owner_pid(&lock), dead_pid, "still naming its owner");
        assert!(!process_alive(dead_pid), "whose process is really gone");

        // Staleness is age AND death; only the death half needs a second
        // process. Backdate the timestamp in place — keeping the dead pid the
        // child really had — rather than put a minute of sleeping in the suite.
        let old = hrdr_tools::unix_now().saturating_sub(SMALL_FILE_STALE_LOCK_AGE_SECS + 60);
        std::fs::write(&lock, format!("{dead_pid} {old}")).unwrap();

        let _ours = StoreLock::acquire(&store, StoreKind::SmallFileRewrite)
            .expect("a dead holder's lock is reaped rather than waited on forever");
        assert_eq!(
            lock_owner_pid(&lock),
            std::process::id(),
            "and re-claimed by the process that reaped it"
        );
    }

    /// Not a test of its own: the second process the two above drive.
    ///
    /// `#[ignore]`d so an ordinary run never picks it up, and a no-op without
    /// [`CHILD_DIR_ENV`] so even `cargo test -- --ignored` cannot hang on it.
    #[test]
    #[ignore = "re-executed as a child process by the cross-process lock tests"]
    fn child_process_holds_the_lock() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let guard = StoreLock::acquire(&dir.join(CHILD_STORE_NAME), StoreKind::SmallFileRewrite)
            .expect("the child takes a free lock");
        std::fs::write(ready_file(&dir), std::process::id().to_string())
            .expect("the child can signal that it holds the lock");
        if std::env::var_os(CHILD_CRASH_ENV).is_some() {
            // `exit` runs no destructors, so the guard never releases: what a
            // killed process leaves behind, without killing anything.
            std::process::exit(0);
        }
        assert!(
            wait_for_file(&release_file(&dir), CHILD_HOLD_TIMEOUT),
            "the parent released the child within its bound"
        );
        drop(guard);
    }

    // Small mtime helper (no external crate): backdate a file's mtime by
    // shelling out to `touch`. Unix-only — the sole caller
    // (`unparseable_old_lock_is_reaped_by_mtime`) is `#[cfg(unix)]` too.
    #[cfg(unix)]
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        // `touch -t [[CC]YY]MMDDhhmm[.SS]` is portable across GNU and BSD
        // `touch`; macOS's BSD `touch` rejects GNU's `-d @<epoch>` form (it
        // silently no-ops, leaving the lock fresh and wedging the test). The
        // `-t` argument is interpreted in local time, so format `when` locally.
        let when: chrono::DateTime<chrono::Local> = when.into();
        let stamp = when.format("%Y%m%d%H%M.%S").to_string();
        let _ = std::process::Command::new("touch")
            .args(["-t", &stamp, &path.to_string_lossy()])
            .status();
    }
}
