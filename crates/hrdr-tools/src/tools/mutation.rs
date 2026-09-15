use std::path::Path;

use anyhow::{Context, Result};

use crate::ToolContext;

pub struct FileChange {
    pub content_after: String,
    pub notes: Vec<String>,
}

impl FileChange {
    /// The notes as one block ready to splice into a tool's success message: one
    /// per line behind a leading newline, or empty when there are none.
    pub fn formatted_notes(&self) -> String {
        let mut out = self.notes.join("\n");
        if !out.is_empty() {
            out.insert(0, '\n');
        }
        out
    }
}

/// RAII guard that removes the temp file on drop unless kept.
struct TempFile {
    path: std::path::PathBuf,
    keep: bool,
}

impl TempFile {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path, keep: false }
    }
    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Checkpoint the file, write `content`, run post-edit hooks, re-read if hooks
/// ran, then collect LSP diagnostics for the final content. Returns the
/// post-hook content plus hook/diagnostic notes (if any).
pub async fn apply_file_change(
    ctx: &ToolContext,
    path: &Path,
    hook_event: &str,
    content: &str,
) -> Result<FileChange> {
    // Snapshot the prior content for the test nudge, but only when a note could
    // actually be owed — see `nudge_baseline`. A `rename`/rollback is excluded
    // there: those rewrite files the model did not choose to change, so counting
    // them as "changed code without a test" would be a lie about what it did.
    let nudge_before = nudge_baseline(ctx, path, hook_event).await;
    // Re-check immediately before the pathname operation. This portable guard
    // cannot make arbitrary filesystems transactional, but closes the long
    // validation/planning window and refuses any symlink inserted meanwhile.
    atomic_write(path, content)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    // The write landed, so whatever this session had verified is now verified
    // about older code. Same gate as the nudge — a `rename`/rollback is not the
    // model changing code, and a file with no test idiom is not code — and the
    // same `markers` table, so the two features can never disagree about what
    // counts as source. Only on the success path: a failed write invalidates
    // nothing.
    if matches!(hook_event, "edit" | "write" | "replace")
        && !crate::test_nudge::markers(path).is_empty()
    {
        ctx.verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bump_source();
    }
    let mut notes = crate::run_file_hooks(&ctx.hooks, hook_event, path, &ctx.cwd).await;
    let content_after = if !ctx.hooks.is_empty() {
        tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("rereading {} after {hook_event} hook", path.display()))?
    } else {
        content.to_string()
    };
    // Diagnostics run on the *post-hook* content — what's actually on disk.
    if let Some(lsp) = &ctx.lsp
        && let Some(note) = lsp.diagnostics_note(path, &content_after).await
    {
        notes.push(note);
    }
    // Last, so a compile error the model must act on is never pushed down the
    // result by a reminder it can act on afterwards.
    if let Some(before) = nudge_before
        && let Some(note) = ctx
            .test_nudge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(path, &before, &content_after)
    {
        notes.push(note.to_string());
    }
    Ok(FileChange {
        content_after,
        notes,
    })
}

/// The file's content before this change, when the test nudge might have
/// something to say about it — and `None` when it certainly won't, which is the
/// point: that answer costs a lock and no I/O, so a session that writes tests
/// (or edits config, or renames a symbol) never pays for the read.
///
/// A missing file reads as empty rather than failing: creating a new source file
/// with no test is exactly the case worth a note, and it is not this function's
/// job to decide whether the write itself will succeed.
async fn nudge_baseline(ctx: &ToolContext, path: &Path, hook_event: &str) -> Option<String> {
    if !matches!(hook_event, "edit" | "write" | "replace") {
        return None;
    }
    let might = ctx
        .test_nudge
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .might_nudge(path);
    if !might {
        return None;
    }
    Some(tokio::fs::read_to_string(path).await.unwrap_or_default())
}

/// Write `content` to `path` crash-safely: build the new bytes in a sibling temp
/// file, fsync it, preserve the target's existing permission mode, then `rename`
/// it over the target. A rename is atomic on the same filesystem, so a
/// SIGKILL/OOM/power-loss mid-write leaves the target either wholly the old file
/// or wholly the new one — never truncated or half-written, which the old
/// `open(O_TRUNC)+write` in place could.
///
/// Falls back to an in-place write when a rename would change the file's
/// identity rather than its contents: a symlink — where rename would replace the
/// link with a regular file instead of updating its target — or a hardlinked
/// target (link count `> 1`) — where rename would detach this name from its
/// siblings. Both cases are detected on every platform: symlinks and hard links
/// exist on Windows too, and silently converting one into a regular file is the
/// same data loss there as anywhere else.
pub(crate) async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    // Unlike the read path (`guard_not_swapped` in lib.rs), the write path
    // cannot close the TOCTOU window between the sandbox check and the write:
    // there is no pre-existing open handle to compare against.  The sandbox
    // check (`resolve_write`) is the primary defense.
    // `symlink_metadata` is an lstat and portable: it describes the symlink
    // itself, not its destination, and `is_symlink` reports Windows symlinks as
    // well. Ask it first — it is one cheap call that needs no open, and it must
    // come before the link count because reading that count *does* open the path
    // (on Windows) and an open would follow the very link we are looking for.
    let existing = tokio::fs::symlink_metadata(path).await.ok();
    // `is_ok_and`, not `?`, on the link count: one we cannot read (a Windows
    // sharing violation, say) must not fail the write — fall through to the
    // temp+rename path, which is what a single-linked file wants anyway. The `||`
    // also short-circuits, which is what keeps the count from being asked for a
    // symlink.
    if let Some(meta) = &existing
        && (meta.file_type().is_symlink() || crate::hardlink_count(path).is_ok_and(|n| n > 1))
    {
        return tokio::fs::write(path, content).await;
    }
    write_via_temp(path, content, preserved_permissions(existing.as_ref())).await
}

/// The permissions to carry from an existing target onto its replacement, or
/// `None` to leave the temp file's own.
///
/// On unix that is the target's mode, so an executable script stays executable
/// across an edit; a brand-new file keeps whatever mode the temp was created
/// with.
#[cfg(unix)]
fn preserved_permissions(existing: Option<&std::fs::Metadata>) -> Option<std::fs::Permissions> {
    existing.map(|m| m.permissions())
}

/// The permissions to carry from an existing target onto its replacement.
///
/// Always `None` on Windows: the only bit `Permissions` carries there is
/// read-only, and copying that onto the temp file would make the temp
/// un-renameable — the write would fail on exactly the files it was trying to
/// preserve something about. Access is governed by the ACL, which the rename
/// leaves to the containing directory's inheritance either way.
#[cfg(not(unix))]
fn preserved_permissions(_existing: Option<&std::fs::Metadata>) -> Option<std::fs::Permissions> {
    None
}

/// Write `content` to a sibling temp file, fsync it, apply `perms` (if any), and
/// rename it over `path`. Any error removes the temp file so a failed write
/// leaves no `.hrdr-tmp` litter behind.
async fn write_via_temp(
    path: &Path,
    content: &str,
    perms: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    // Same directory as the target, so the final `rename` is intra-filesystem
    // (hence atomic) and unique per call — two concurrent writes to the *same*
    // path must not share a temp name, or one would truncate the other's
    // in-flight file and the renames would race.
    let tmp = hrdr_llm::unique_sibling_path(path, "hrdr-tmp");

    let mut _guard = TempFile::new(tmp.clone());

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    file.write_all(content.as_bytes()).await?;
    file.sync_all().await?;
    drop(file);
    if let Some(perms) = perms {
        tokio::fs::set_permissions(&tmp, perms).await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    _guard.keep();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_mode_and_replaces_content() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        tokio::fs::write(&path, "old").await.unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();

        atomic_write(&path, "new content").await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "new content"
        );
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the executable bit must survive the write"
        );
        // No temp litter is left behind.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            assert!(
                !e.file_name().to_string_lossy().contains("hrdr-tmp"),
                "temp file leaked: {:?}",
                e.file_name()
            );
        }
    }

    /// A hardlinked target keeps both names pointing at the same bytes: the
    /// in-place fallback updates the shared inode instead of renaming a fresh
    /// inode over one name (which would silently split the link).
    #[tokio::test]
    async fn atomic_write_keeps_hardlinks_in_sync() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "old").await.unwrap();
        tokio::fs::hard_link(&a, &b).await.unwrap();

        atomic_write(&a, "shared new").await.unwrap();

        assert_eq!(tokio::fs::read_to_string(&a).await.unwrap(), "shared new");
        assert_eq!(
            tokio::fs::read_to_string(&b).await.unwrap(),
            "shared new",
            "the hardlinked twin must see the update too"
        );
    }

    /// A symlinked target keeps being a symlink: the in-place fallback writes
    /// *through* the link to its destination, where a temp+rename would have
    /// replaced the link itself with a regular file and orphaned the real file.
    /// (Unix-only as a test — creating a symlink on Windows needs a privilege the
    /// CI runner may not have — but the guard itself now runs on both.)
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        tokio::fs::write(&target, "old").await.unwrap();
        tokio::fs::symlink(&target, &link).await.unwrap();

        atomic_write(&link, "through the link").await.unwrap();

        assert!(
            tokio::fs::symlink_metadata(&link)
                .await
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive the write, not be replaced by a file"
        );
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "through the link",
            "the link's destination must have received the content"
        );
    }

    #[tokio::test]
    async fn atomic_write_creates_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.txt");
        atomic_write(&path, "brand new").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "brand new");
    }
}
