//! A unique, hidden sibling temp path for a crash-safe write-then-atomic-
//! rename: build the new bytes at `<dir>/.<filename>.<tag>-<pid>-<seq>` next
//! to the real target, fsync/write it, then `rename` it over the target.
//! Same directory keeps the rename intra-filesystem (hence atomic); the
//! dot-prefix keeps it out of normal directory listings; PID plus a
//! process-wide counter keep it unique across both concurrent processes and
//! concurrent calls within one process, so two callers racing to write the
//! same target never collide on one temp name (no random/time API needed —
//! names stay deterministic).
//!
//! One scheme shared by every crate that does this: `hrdr-tools`' file writer
//! and its move/copy staging, `hrdr-llm`'s catalog cache writer, and
//! `hrdr-agent`'s credential-store writer.
//!
//! This module also owns the other cross-crate file policy — "only the owner
//! may read this" — in [`owner_only_options`], for the same reason: every crate
//! that writes a secret (credentials, wire logs, transcripts, the catalog cache)
//! needs the identical rule stated once.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counter giving each temp path a unique name (paired with the
/// PID). Shared by every call site so two unrelated writers can never be
/// handed the same sequence number.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique, hidden sibling path for `target`: `<dir>/.<filename>.<tag>-<pid>-<seq>`,
/// living in `target`'s parent directory so a subsequent `rename` onto
/// `target` stays on one filesystem. `tag` labels the caller/use-case (e.g.
/// `hrdr-tmp`, `hrdr-stage`, `hrdr-aside`) so a stray leftover is
/// recognizable in a directory listing.
pub fn unique_sibling_path(target: &Path, tag: &str) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.{tag}-{}-{seq}", std::process::id()))
}

/// `path` with `suffix` appended to its final component, in the same directory:
/// `requests.log` → `requests.log.1`, `auth.json` → `auth.json.lock`. The one
/// scheme for "a sibling that is the same name plus a marker", shared by the
/// wire-log rotation (`.1`), the config backup (`.bak`) and the store lock
/// (`.lock`), which used to each re-derive it.
pub fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// [`std::fs::OpenOptions`] that create a file only its owner can read, ready
/// for the caller to add its own create/append/truncate semantics and `open`.
/// The single owner of that policy: the credential store, the wire log, the
/// agent transcripts and the catalog cache all hold data that must not leak to
/// other local users, and they must not each decide what "owner-only" means.
///
/// On unix that is `mode(0o600)`, applied *at creation* so the file never
/// exists with broader permissions — there is no window between `open` and a
/// `set_permissions` fixup. A pre-existing file keeps whatever mode it has
/// (`mode` only applies to a file this open creates); a caller that must tighten
/// an existing file does so itself on the returned handle.
///
/// Confidentiality on **Windows**, stated once and honestly: hrdr sets **no**
/// explicit ACL. `mode` has no Windows analogue in std, and the read-only
/// *attribute* is not an access control (reads are governed by the ACL) and
/// would make the file un-replaceable by a later atomic rename. The guarantee is
/// therefore the inherited default ACL of the containing directory — user-scoped
/// for anything under the per-user profile, which is where hrdr's own state
/// lives, but nothing hrdr enforces. Setting a per-user ACL would need a new
/// dependency in this crate and is a deliberate non-goal here; a caller writing
/// to a *caller-chosen* directory (e.g. `HRDR_LOG_REQUESTS`) inherits that
/// directory's ACL and should say so at its own call site.
pub fn owner_only_options() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    harden_owner_only(&mut opts);
    opts
}

/// The unix half of [`owner_only_options`]: create at `0o600`.
#[cfg(unix)]
fn harden_owner_only(opts: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
}

/// The non-unix half of [`owner_only_options`]: nothing to set. Deliberately a
/// no-op rather than a `cfg` at the call site — see [`owner_only_options`] for
/// why hrdr sets no Windows ACL and what it relies on instead.
#[cfg(not(unix))]
fn harden_owner_only(_opts: &mut std::fs::OpenOptions) {}

/// [`owner_only_options`] plus, on unix, `O_NOFOLLOW`: if the final path
/// component is a symlink at open time the open fails with `ELOOP` instead of
/// following it, so the open *is* the symlink check and no attacker can swap a
/// link in between a caller's preflight `symlink_metadata` and this open.
///
/// A separate constructor rather than a flag on [`owner_only_options`]: refusing
/// to follow a link is a different guarantee from keeping the bytes private,
/// only the wire log needs it (its path is caller-chosen, so it is the one that
/// can be aimed at an attacker-controlled directory), and a bare `true` at the
/// call site would say nothing about which of the two it turns on.
///
/// Residual, on unix: `O_NOFOLLOW` covers only the final component — a symlinked
/// *parent directory* is still traversed. On Windows this is exactly
/// [`owner_only_options`]; there is no `O_NOFOLLOW` equivalent applied, so a
/// caller that relies on it must keep its own preflight check.
pub fn owner_only_options_no_follow() -> std::fs::OpenOptions {
    let mut opts = owner_only_options();
    refuse_symlinks(&mut opts);
    opts
}

/// The unix half of [`owner_only_options_no_follow`]: `O_NOFOLLOW`, so the open
/// itself fails on a final-component symlink.
#[cfg(unix)]
fn refuse_symlinks(opts: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.custom_flags(libc::O_NOFOLLOW);
}

/// The non-unix half of [`owner_only_options_no_follow`]: no `O_NOFOLLOW`
/// equivalent, so nothing is set and callers keep their own preflight check.
#[cfg(not(unix))]
fn refuse_symlinks(_opts: &mut std::fs::OpenOptions) {}

/// Write `data` to `path` atomically: write to a temp file in the same
/// directory, fsync, then rename over the target — a concurrent reader never
/// sees a partial write. The temp file is created owner-only from the start (see
/// [`owner_only_options`]) so there is no window where it exists with
/// broader permissions.
///
/// Confidentiality guarantee, stated honestly: on Unix the file is owner-only
/// (`0600`), enforced on every write. On Windows hrdr sets **no** explicit ACL —
/// [`owner_only_options`] documents that stance in full. In practice
/// the credential files land under `~/.config/hrdr` (see `hrdr_agent::config_dir`),
/// which on Windows resolves to the per-user profile (`%USERPROFILE%`, not
/// `%APPDATA%`) and is user-scoped by default — so the inherited default ACL
/// that hrdr relies on there is a per-user one.
///
/// On unix the parent directory is fsynced after a successful rename so that the
/// rename is crash-durable (the directory entry change is flushed to media).
/// A directory sync failure is **not** reported as an error: the rename
/// itself is atomic and the data is already on disk — a lost sync only
/// risks losing the rename itself if the machine crashes before the
/// directory metadata write completes.
///
/// The parent directory must already exist. A rename failure removes the temp
/// so no stray file is left behind.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // Write to a temp file in the same directory, then rename atomically.
    // tempfile is a dev-dependency only, so the temp name comes from the
    // shared sibling-temp scheme instead.
    let tmp = unique_sibling_path(path, "hrdr-tmp");

    // `create_new` guarantees we own `tmp`; a failure here means someone else's
    // temp collided, so we must not clean it up. Everything after gets a
    // cleanup-on-error guard so a failed save never leaves a stray temp behind
    // (notably: a rename that fails still removes the temp we wrote).
    let mut f = owner_only_options()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let result = (|| -> std::io::Result<()> {
        f.write_all(data)?;
        // Flush + fsync so the data is on disk before the rename. `sync_all` is
        // portable — it is `FlushFileBuffers` on Windows — so a crash right
        // after the rename cannot lose a freshly saved credential on any
        // platform.
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    drop(f);
    let result = result.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    // After a successful rename, sync the parent directory so the directory
    // entry change (the rename) is crash-durable.  Unix-only, and genuinely so:
    // this needs a *directory* handle, and Windows cannot `File::open` a
    // directory at all (it wants `CreateFile` with
    // FILE_FLAG_BACKUP_SEMANTICS).  Unlike the file `sync_all` above — which is
    // portable and now runs everywhere — there is no std equivalent to reach
    // for here.  A sync failure is silently swallowed either way: the write is
    // atomic and the data is on disk, so a lost directory sync risks only the
    // rename itself in a crash before the directory metadata flushes.
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sibling keeps the parent and appends the suffix to the final
    /// component; a bare path (no parent) still gets the suffix.
    #[test]
    fn sibling_with_suffix_appends_to_the_final_component() {
        assert_eq!(
            sibling_with_suffix(Path::new("/var/log/requests.log"), ".1"),
            PathBuf::from("/var/log/requests.log.1")
        );
        assert_eq!(
            sibling_with_suffix(Path::new("auth.json"), ".lock"),
            PathBuf::from("auth.json.lock")
        );
    }

    /// Two calls for the same target must never collide, must stay hidden
    /// (dot-prefixed), must sit beside the target, and must carry the tag —
    /// the properties every call site relies on.
    #[test]
    fn unique_sibling_path_is_unique_hidden_and_colocated() {
        let target = Path::new("/some/dir/models.json");
        let a = unique_sibling_path(target, "hrdr-tmp");
        let b = unique_sibling_path(target, "hrdr-tmp");

        assert_ne!(a, b, "concurrent callers must not share a temp name");
        assert_eq!(a.parent(), target.parent());
        assert_eq!(b.parent(), target.parent());

        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with('.'), "sibling temp must be hidden: {name}");
            assert!(name.contains("hrdr-tmp"), "tag must appear in name: {name}");
            assert!(
                name.contains(&std::process::id().to_string()),
                "pid must appear in name: {name}"
            );
        }
    }

    /// Different tags for the same target still produce distinct, correctly
    /// labeled names — the `staging_path`/`aside_path` two-tags-one-target
    /// case in `hrdr-tools`' copy staging.
    #[test]
    fn unique_sibling_path_respects_the_tag() {
        let target = Path::new("/proj/dest.txt");
        let stage = unique_sibling_path(target, "hrdr-stage");
        let aside = unique_sibling_path(target, "hrdr-aside");

        assert_ne!(stage, aside);
        assert!(
            stage
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("hrdr-stage")
        );
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("hrdr-aside")
        );
    }

    /// The whole point of the helper: a file it creates is readable and writable
    /// by its owner only, with no window at a broader mode (the mode is set by
    /// the `open` itself, not by a later fixup).
    #[cfg(unix)]
    #[test]
    fn owner_only_options_create_at_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        owner_only_options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();

        // The mode is only a *request*: the kernel applies the umask on top, so a
        // stricter result is still correct. What must hold is that nobody else can
        // read it.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "created file must have no group/other permissions (mode={mode:#o})"
        );
    }

    /// The no-follow variant keeps the 0600 policy *and* refuses a symlinked
    /// final component, so a swapped-in link cannot redirect the write.
    #[cfg(unix)]
    #[test]
    fn owner_only_options_no_follow_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            owner_only_options_no_follow()
                .append(true)
                .create(true)
                .open(&link)
                .is_err(),
            "O_NOFOLLOW must reject a symlinked final component"
        );

        // Same call on a real path still creates an owner-only file.
        let fresh = dir.path().join("fresh");
        owner_only_options_no_follow()
            .append(true)
            .create(true)
            .open(&fresh)
            .unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode={mode:#o}");
    }

    #[test]
    fn write_atomic_produces_content_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write_atomic(&path, b"hello world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
    }

    /// `write_atomic` syncs the parent directory after a successful rename so
    /// the directory entry change is crash-durable on Unix.  This test cannot
    /// verify the sync itself (it is a kernel-level durability guarantee), but
    /// it verifies that the sync does not break the happy path: the file is
    /// written, the content is correct, no stray temps are left, and the
    /// directory is still usable.
    #[test]
    fn write_atomic_with_dir_sync_completes_normally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");

        write_atomic(&path, b"sync test data").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"sync test data",
            "content is preserved"
        );

        // No temp files or other stray files were left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no stray files after write_atomic: {leftovers:?}"
        );

        // A second write_atomic on the same path also succeeds.
        write_atomic(&path, b"second write").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second write");
    }
}
