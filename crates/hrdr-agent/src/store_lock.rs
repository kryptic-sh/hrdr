//! Cross-process write lock for the credential stores.
//!
//! The credential files (`auth.toml`, `oauth.json`) are updated read-modify-
//! write: read the whole store, change one entry, then atomically rename a temp
//! over the target. The atomic rename protects *readers* — nobody ever sees a
//! half-written file — but it does NOT serialize *writers*. Two processes can
//! each read the same old store, add a different provider, and both rename; the
//! second rename wins and the first process's new entry is lost.
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
//!   exit path (normal return, `?`, panic). A crash between create and drop
//!   leaves the file behind, which the staleness check below reaps.
//! * **Staleness**: a lock whose owning PID is gone, or whose timestamp is older
//!   than [`STALE_LOCK_AGE_SECS`], is reaped and re-claimed. A lock whose content
//!   doesn't parse (an empty or truncated file) is aged by its mtime alone so it
//!   can never wedge the store forever.
//! * **Bounded retries**: acquisition spins with a short sleep up to
//!   [`LOCK_ACQUIRE_ATTEMPTS`] times (≈ a few seconds total). If it still can't
//!   claim the lock it returns an error rather than blocking forever — an
//!   unwritable directory or a wedged peer surfaces cleanly instead of hanging.
//!
//! ## Policy
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

/// How old (seconds) a lock must be before it may be reaped as stale. A writer
/// only holds the lock for one read-modify-write of a tiny file, so a lock much
/// older than this almost certainly belongs to a crashed process. Kept generous
/// so a legitimately slow filesystem doesn't get its lock stolen mid-write.
const STALE_LOCK_AGE_SECS: u64 = 60;

/// How many times [`StoreLock::acquire`] retries a contended lock before giving
/// up. With [`LOCK_RETRY_DELAY`] this bounds the wait so an unwritable directory
/// (every attempt fails) or a wedged peer cannot hang a login forever.
const LOCK_ACQUIRE_ATTEMPTS: u32 = 100;

/// Delay between acquisition attempts. `100 * 50ms = 5s` worst-case wait, which
/// comfortably outlasts any honest read-modify-write of a small credential file
/// while still failing fast against a truly stuck lock.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

/// RAII cross-process write lock for a credential store file.
///
/// Held for the duration of one read-modify-write. Dropping it releases the lock
/// (removes the lock file) on every exit path, so a failed or panicking write
/// never leaves a permanent lock behind. See the module docs for the full design.
#[derive(Debug)]
pub struct StoreLock {
    lock_path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

impl StoreLock {
    /// Acquire the write lock for the store at `store_path`, blocking (with a
    /// bounded retry loop) until it is free or [`LOCK_ACQUIRE_ATTEMPTS`] is
    /// exhausted.
    ///
    /// The parent directory must already exist (the callers `create_dir_all` it
    /// before locking). Returns an error — never hangs — when the lock stays
    /// contended past the retry budget or the directory is unwritable.
    pub fn acquire(store_path: &Path) -> Result<Self> {
        let lock_path = lock_path_for(store_path);
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
                    return Ok(StoreLock { lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Someone holds it. Reap it if stale and retry immediately;
                    // otherwise wait a beat and try again (bounded).
                    if is_stale_lock(&lock_path) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(e) => {
                    // A non-contention error (e.g. an unwritable directory) will
                    // not fix itself by retrying — surface it right away.
                    return Err(anyhow!(
                        "acquiring credential-store lock {}: {e}",
                        lock_path.display()
                    ));
                }
            }
        }
        Err(anyhow!(
            "timed out acquiring credential-store lock {} (held by another process?)",
            lock_path.display()
        ))
    }
}

/// The sibling lock path for a store file: `<store>.lock`. Placed alongside the
/// store (same directory) so it shares the store's permissions/ownership and is
/// obvious to anyone inspecting the config dir.
fn lock_path_for(store_path: &Path) -> PathBuf {
    let mut name = store_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    match store_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Whether the lock file at `path` is stale — owned by a dead process, older
/// than [`STALE_LOCK_AGE_SECS`], or unparseable and old by mtime.
///
/// Mirrors the session store's `is_stale_lock`: parse `PID TIMESTAMP`, and if
/// the content doesn't parse (empty/truncated lock) fall back to the file's
/// mtime so an unparseable lock can still be aged out rather than wedging the
/// store forever.
fn is_stale_lock(path: &Path) -> bool {
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
        return age.is_some_and(|a| a.as_secs() >= STALE_LOCK_AGE_SECS);
    };
    let now = hrdr_tools::unix_now();
    // Not old enough yet — a live writer may legitimately hold it.
    if now < ts || now.saturating_sub(ts) < STALE_LOCK_AGE_SECS {
        return false;
    }
    // Old enough: reap only if the owning process is really gone.
    if process_alive(pid) {
        return false;
    }
    true
}

/// Best-effort check for whether process `pid` is still alive, zero-dependency.
/// Errs on the side of "alive" (returns `true` when it can't tell) so a live
/// writer's lock is never stolen on a platform where the probe is unavailable.
fn process_alive(pid: u32) -> bool {
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
    // reaches here once the lock is already older than STALE_LOCK_AGE_SECS, so
    // assume the owner is gone — a crashed writer's lock can still be reaped.
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
        assert_eq!(lock_path_for(p), PathBuf::from("/some/dir/auth.toml.lock"));
        let p = Path::new("/x/oauth.json");
        assert_eq!(lock_path_for(p), PathBuf::from("/x/oauth.json.lock"));
    }

    #[test]
    fn acquire_creates_and_drop_removes_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        {
            let _guard = StoreLock::acquire(&store).unwrap();
            assert!(lock.exists(), "lock file exists while held");
        }
        assert!(!lock.exists(), "lock file removed on drop");
    }

    #[test]
    fn second_acquire_while_held_times_out_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let _held = StoreLock::acquire(&store).unwrap();
        // A live (this process) lock is never stale, so a second acquire runs
        // out its retry budget and errors instead of hanging or corrupting.
        let err = StoreLock::acquire(&store).unwrap_err().to_string();
        assert!(err.contains("timed out acquiring"), "{err}");
    }

    #[test]
    fn stale_lock_with_dead_pid_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        // A dead PID with an old timestamp: PID 4294967294 is effectively never
        // a live process, and the timestamp is well past the staleness window.
        let old = hrdr_tools::unix_now().saturating_sub(STALE_LOCK_AGE_SECS + 60);
        std::fs::write(&lock, format!("4294967294 {old}")).unwrap();
        // Acquire must reap the stale lock and succeed on the first pass.
        let _guard = StoreLock::acquire(&store).unwrap();
        assert!(lock.exists(), "our fresh lock replaced the stale one");
    }

    #[test]
    fn unparseable_old_lock_is_reaped_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.toml");
        let lock = dir.path().join("auth.toml.lock");
        // An empty/garbage lock file (e.g. a truncated write) with no PID.
        std::fs::write(&lock, b"").unwrap();
        // Backdate its mtime past the staleness window so it ages out.
        let old = std::time::SystemTime::now() - Duration::from_secs(STALE_LOCK_AGE_SECS + 60);
        let old = filetime_from(old);
        set_mtime(&lock, old);
        let _guard = StoreLock::acquire(&store).unwrap();
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
        let err = StoreLock::acquire(&store).unwrap_err().to_string();
        assert!(err.contains("timed out acquiring"), "{err}");
    }

    // Small mtime helpers (no external crate): set a file's mtime via a
    // best-effort platform touch. On unix we use `utimensat` through std by
    // reopening; to stay dependency-free we shell out to `touch -d` where
    // available, and otherwise skip the backdating (the test still constructs a
    // valid scenario).
    fn filetime_from(t: std::time::SystemTime) -> std::time::SystemTime {
        t
    }

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

    #[cfg(not(unix))]
    fn set_mtime(_path: &Path, _when: std::time::SystemTime) {
        // No dependency-free mtime setter on Windows in this test; the
        // unparseable-lock reaping is still covered on unix CI.
    }
}
