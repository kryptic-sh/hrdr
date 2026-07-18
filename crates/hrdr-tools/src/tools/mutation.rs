use std::path::Path;

use anyhow::{Context, Result};

use crate::ToolContext;

pub struct FileChange {
    pub content_after: String,
    pub notes: Vec<String>,
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
    // Re-check immediately before the pathname operation. This portable guard
    // cannot make arbitrary filesystems transactional, but closes the long
    // validation/planning window and refuses any symlink inserted meanwhile.
    atomic_write(path, content)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
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
    Ok(FileChange {
        content_after,
        notes,
    })
}

/// Write `content` to `path` crash-safely: build the new bytes in a sibling temp
/// file, fsync it, preserve the target's existing permission mode, then `rename`
/// it over the target. A rename is atomic on the same filesystem, so a
/// SIGKILL/OOM/power-loss mid-write leaves the target either wholly the old file
/// or wholly the new one — never truncated or half-written, which the old
/// `open(O_TRUNC)+write` in place could.
///
/// Falls back to an in-place write when a rename would change the file's
/// identity rather than its contents: a hardlinked target (`nlink > 1`) — where
/// rename would detach this name from its siblings — or a symlink — where rename
/// would replace the link with a regular file instead of updating its target.
pub(crate) async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // `symlink_metadata` is an lstat: it describes the symlink itself, not
        // its destination, so the symlink case is detectable here.
        let existing = tokio::fs::symlink_metadata(path).await.ok();
        if let Some(meta) = &existing
            && (meta.file_type().is_symlink() || meta.nlink() > 1)
        {
            return tokio::fs::write(path, content).await;
        }
        // Carry the target's mode onto the replacement; a brand-new file keeps
        // whatever mode the temp file was created with.
        let perms = existing.map(|m| m.permissions());
        write_via_temp(path, content, perms).await
    }
    #[cfg(not(unix))]
    {
        // No portable `nlink`/symlink identity check without unix metadata; on
        // these targets hardlinks/symlinks are rare and temp+rename is still
        // atomic on the same filesystem for the common case.
        write_via_temp(path, content, None).await
    }
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
        .open(&tmp).await?;
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
    #[cfg(unix)]
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

    #[tokio::test]
    async fn atomic_write_creates_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.txt");
        atomic_write(&path, "brand new").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "brand new");
    }
}
