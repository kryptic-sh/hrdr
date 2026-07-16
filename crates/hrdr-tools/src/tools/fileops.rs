//! Guarded file operations: `move`, `delete`, `copy`.
//!
//! The shells can already do all three, but a shell mutation escapes the safety
//! net the file tools sit behind: it is not held to a sub-agent's `write_ext`
//! allow-list. These tools route the same operations through
//! [`ToolContext::ensure_writable_ext`], which also makes them available to
//! sub-agents that have no shell at all (`plan` writes markdown, and can now
//! rename and delete it).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

/// Guard a path that is about to be created or overwritten: inside the working
/// directory, and of a permitted extension for this agent.
fn guard_dest(ctx: &ToolContext, path: &std::path::Path) -> Result<()> {
    ctx.ensure_writable_ext(path)
}

/// Guard a path whose current contents are about to disappear (the source of a
/// move, or the target of a delete). Same confinement, plus the read-before-
/// mutate gate the other tools apply: the model must have seen what it's about
/// to destroy.
async fn guard_victim(ctx: &ToolContext, path: &std::path::Path, verb: &str) -> Result<()> {
    ctx.ensure_writable_ext(path)?;
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        bail!(
            "{} does not exist — relative paths resolve against the project root ({}); \
             use ls or find to locate it",
            path.display(),
            ctx.cwd.display()
        );
    }
    if path.is_file() && !ctx.was_read(path) {
        bail!(
            "{} hasn't been read — call read first so you know what you're about to {verb}",
            path.display()
        );
    }
    Ok(())
}

fn reject_descendant_destination(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    let source = crate::canonicalize_nearest(from);
    let destination = crate::canonicalize_nearest(to);
    if destination == source || destination.starts_with(&source) {
        bail!(
            "{} is inside its source {} — copying or moving a directory into itself is refused",
            to.display(),
            from.display()
        );
    }
    Ok(())
}

// ---- move ----

pub struct MoveTool;

#[derive(Deserialize)]
struct MoveArgs {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for MoveTool {
    fn name(&self) -> &'static str {
        "move"
    }
    fn description(&self) -> &'static str {
        "Rename or relocate a file or directory. Parent directories of the destination are \
         created as needed. Refuses to clobber an existing destination unless `overwrite` is \
         true. Prefer this over `bash mv`: it validates the operation and reports what changed."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "Existing path, absolute or relative to cwd."},
                "to": {"type": "string", "description": "Destination path."},
                "overwrite": {"type": "boolean", "description": "Replace the destination if it exists. Default false."}
            },
            "required": ["from", "to"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: MoveArgs = crate::tool_args("move", args)?;
        let from = ctx.resolve(&a.from);
        let to = ctx.resolve(&a.to);
        guard_victim(ctx, &from, "move").await?;
        guard_dest(ctx, &to)?;
        if from.is_dir() {
            reject_descendant_destination(&from, &to)?;
        }

        let dest_exists = tokio::fs::try_exists(&to).await.unwrap_or(false);
        if dest_exists && !a.overwrite {
            bail!(
                "{} already exists — pass overwrite: true to replace it",
                to.display()
            );
        }
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        guard_victim(ctx, &from, "move").await?;
        guard_dest(ctx, &to)?;
        rename_or_copy(&from, &to)
            .await
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        // The model has seen this content; carry that over to the new path so an
        // immediate `edit` isn't blocked by the read-before-edit gate.
        if to.is_file() {
            ctx.mark_read(&to);
        }
        Ok(format!("Moved {} → {}", from.display(), to.display()))
    }
}

/// `fs::rename` fails across filesystems (`EXDEV`), which a project spanning a
/// mount point can hit. Fall back to copy-then-remove **only** for that case —
/// a permission error, `ENOSPC`, or a missing source is a real failure that must
/// surface, not be escalated into an expensive recursive copy that then fails
/// (or worse, half-succeeds) for the same reason.
async fn rename_or_copy(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            if from.is_dir() {
                staged_copy_dir(from, to).await?;
                tokio::fs::remove_dir_all(from).await?;
            } else {
                tokio::fs::copy(from, to).await?;
                tokio::fs::remove_file(from).await?;
            }
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

async fn validate_copy_tree(root: &std::path::Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if file_type.is_symlink() {
                bail!(
                    "{} is a symlink inside the source tree — recursive copy refuses symlinks to avoid following content outside the project",
                    entry.path().display()
                );
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// A unique sibling staging path for `to` (see [`hrdr_llm::unique_sibling_path`]),
/// so two concurrent copies to the *same* destination never collide on one
/// staging path and clobber each other's in-flight tree.
fn staging_path(to: &std::path::Path) -> std::path::PathBuf {
    hrdr_llm::unique_sibling_path(to, "hrdr-stage")
}

/// A second sibling staging name, for holding the *existing* destination aside
/// while the replacement is swapped in — so `to` is never a missing path.
fn aside_path(to: &std::path::Path) -> std::path::PathBuf {
    hrdr_llm::unique_sibling_path(to, "hrdr-aside")
}

/// Remove a path whether it is a file or a directory.
async fn remove_path(path: &std::path::Path) -> std::io::Result<()> {
    if path.is_dir() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    }
}

async fn staged_copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    validate_copy_tree(from).await?;
    let stage = staging_path(to);
    if tokio::fs::try_exists(&stage).await.unwrap_or(false) {
        remove_path(&stage).await?;
    }
    if let Err(error) = copy_dir(from, &stage).await {
        let _ = tokio::fs::remove_dir_all(&stage).await;
        return Err(error);
    }

    // Swap the freshly-built copy into place without ever leaving `to` missing.
    // The old code did `remove(to)` then `rename(stage → to)`: if the rename
    // failed, `to` was already gone. Instead, move the existing destination
    // *aside* first, swap the stage in, then delete the aside copy — and on a
    // failed swap, put the original straight back.
    if tokio::fs::try_exists(to).await.unwrap_or(false) {
        let aside = aside_path(to);
        if tokio::fs::try_exists(&aside).await.unwrap_or(false) {
            remove_path(&aside).await?;
        }
        tokio::fs::rename(to, &aside).await.with_context(|| {
            format!(
                "staging replacement of {} (new copy left at {})",
                to.display(),
                stage.display()
            )
        })?;
        if let Err(error) = tokio::fs::rename(&stage, to).await {
            // Restore the original destination; leave the stage copy as
            // recoverable litter and name it so it can be reclaimed.
            let _ = tokio::fs::rename(&aside, to).await;
            return Err(anyhow::Error::new(error).context(format!(
                "replacing {} (original left intact; new copy still at {})",
                to.display(),
                stage.display()
            )));
        }
        // Swap done: the destination is correct. A failure to delete the old
        // copy now is non-fatal litter, not a correctness problem.
        let _ = remove_path(&aside).await;
    } else {
        tokio::fs::rename(&stage, to).await.with_context(|| {
            format!(
                "moving new copy into place at {} (new copy left at {})",
                to.display(),
                stage.display()
            )
        })?;
    }
    Ok(())
}

/// Recursive directory copy (`tokio::fs` has no equivalent). Iterative, so a
/// deep tree can't blow the stack.
async fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        tokio::fs::create_dir_all(&dst).await?;
        let mut entries = tokio::fs::read_dir(&src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let (s, d) = (entry.path(), dst.join(entry.file_name()));
            let file_type = entry.file_type().await?;
            if file_type.is_symlink() {
                bail!(
                    "{} is a symlink inside the source tree — recursive copy refuses symlinks to avoid following content outside the project",
                    s.display()
                );
            }
            if file_type.is_dir() {
                stack.push((s, d));
            } else {
                tokio::fs::copy(&s, &d).await?;
            }
        }
    }
    Ok(())
}

// ---- delete ----

pub struct DeleteTool;

#[derive(Deserialize)]
struct DeleteArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[async_trait]
impl Tool for DeleteTool {
    fn name(&self) -> &'static str {
        "delete"
    }
    fn description(&self) -> &'static str {
        "Delete a file, or a directory with `recursive: true`. A file must have been read \
         first. Prefer this over `bash rm`: it validates the operation and reports what changed."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to delete, absolute or relative to cwd."},
                "recursive": {"type": "boolean", "description": "Required to delete a directory and everything under it. Default false."}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: DeleteArgs = crate::tool_args("delete", args)?;
        let path = ctx.resolve(&a.path);
        guard_victim(ctx, &path, "delete").await?;

        if path.is_dir() {
            if !a.recursive {
                bail!(
                    "{} is a directory — pass recursive: true to delete it and its contents",
                    path.display()
                );
            }
            let count = walk_files(&path).await?.len();
            guard_victim(ctx, &path, "delete").await?;
            tokio::fs::remove_dir_all(&path)
                .await
                .with_context(|| format!("deleting {}", path.display()))?;
            Ok(format!(
                "Deleted {} ({count} file{})",
                path.display(),
                if count == 1 { "" } else { "s" }
            ))
        } else {
            guard_victim(ctx, &path, "delete").await?;
            tokio::fs::remove_file(&path)
                .await
                .with_context(|| format!("deleting {}", path.display()))?;
            Ok(format!("Deleted {}", path.display()))
        }
    }
}

/// Every file under `root`, depth-first. Iterative for the same reason as
/// [`copy_dir`].
async fn walk_files(root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let p = entry.path();
            if entry.file_type().await?.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    Ok(out)
}

// ---- copy ----

pub struct CopyTool;

#[derive(Deserialize)]
struct CopyArgs {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for CopyTool {
    fn name(&self) -> &'static str {
        "copy"
    }
    fn description(&self) -> &'static str {
        "Copy a file or directory. Parent directories of the destination are created as \
         needed. Refuses to clobber an existing destination unless `overwrite` is true. \
         Refuses to copy a credential/secret file (`.env`, keys, etc.). Prefer this over \
         `bash cp`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "Existing path, absolute or relative to cwd."},
                "to": {"type": "string", "description": "Destination path."},
                "overwrite": {"type": "boolean", "description": "Replace the destination if it exists. Default false."}
            },
            "required": ["from", "to"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: CopyArgs = crate::tool_args("copy", args)?;
        let from = ctx.resolve(&a.from);
        let to = ctx.resolve(&a.to);
        // The source survives a copy, so it needs confinement but not the
        // read-before-destroy gate. It does need the secret guard, though:
        // otherwise `copy .env notes.txt` then `read notes.txt` launders a
        // credential past the read deny-list (the copy's name matches no
        // pattern).
        if let Some(reason) = crate::secret_file_reason(&crate::canonicalize_nearest(&from)) {
            bail!(
                "refusing to copy {}: {reason} — copying it would place its \
                 contents at a name the read deny-list can't recognize",
                from.display()
            );
        }
        ctx.ensure_writable_ext(&from)?;
        if !tokio::fs::try_exists(&from).await.unwrap_or(false) {
            bail!(
                "{} does not exist — relative paths resolve against the project root ({}); \
                 use ls or find to locate it",
                from.display(),
                ctx.cwd.display()
            );
        }
        guard_dest(ctx, &to)?;
        if from.is_dir() {
            reject_descendant_destination(&from, &to)?;
        }

        let dest_exists = tokio::fs::try_exists(&to).await.unwrap_or(false);
        if dest_exists && !a.overwrite {
            bail!(
                "{} already exists — pass overwrite: true to replace it",
                to.display()
            );
        }
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        ctx.ensure_writable_ext(&from)?;
        guard_dest(ctx, &to)?;
        if from.is_dir() {
            staged_copy_dir(&from, &to).await?;
            Ok(format!("Copied {}/ → {}/", from.display(), to.display()))
        } else {
            tokio::fs::copy(&from, &to)
                .await
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            Ok(format!("Copied {} → {}", from.display(), to.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A context whose files all count as already read, so the read-before-
    /// destroy gate doesn't obscure what a test is actually checking.
    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir)
    }

    async fn write(path: &std::path::Path, body: &str) {
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    #[tokio::test]
    async fn copy_refuses_a_secret_source() {
        // copy .env x.txt would launder the credential past the read deny-list.
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        write(&dir.path().join(".env"), "SECRET=1").await;

        let err = CopyTool
            .execute(json!({"from": ".env", "to": "notes.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to copy"), "{err}");
        assert!(!dir.path().join("notes.txt").exists());
    }

    #[tokio::test]
    async fn copy_and_move_refuse_a_directorys_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        write(&dir.path().join("tree/a.txt"), "a").await;

        let copy_err = CopyTool
            .execute(json!({"from": "tree", "to": "tree/copy"}), &c)
            .await
            .unwrap_err();
        assert!(
            copy_err.to_string().contains("inside its source"),
            "{copy_err}"
        );
        let move_err = MoveTool
            .execute(json!({"from": "tree", "to": "tree/moved"}), &c)
            .await
            .unwrap_err();
        assert!(
            move_err.to_string().contains("inside its source"),
            "{move_err}"
        );
        for destination in ["tree/copy", "tree/moved"] {
            assert!(!dir.path().join(destination).exists());
        }
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("tree/a.txt"))
                .await
                .unwrap(),
            "a"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_copy_refuses_embedded_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), "secret").await;
        write(&dir.path().join("tree/normal.txt"), "normal").await;
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("tree/link.txt"),
        )
        .unwrap();
        let c = ctx(dir.path());
        let err = CopyTool
            .execute(json!({"from": "tree", "to": "copied"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!dir.path().join("copied/link.txt").exists());
    }

    #[tokio::test]
    async fn move_renames_a_read_file_and_carries_its_read_mark() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let from = dir.path().join("a.txt");
        write(&from, "hi").await;
        c.mark_read(&from);

        let out = MoveTool
            .execute(json!({"from": "a.txt", "to": "sub/b.txt"}), &c)
            .await
            .unwrap();
        assert!(out.starts_with("Moved "), "{out}");
        assert!(!from.exists(), "source gone");
        let to = dir.path().join("sub/b.txt");
        assert_eq!(tokio::fs::read_to_string(&to).await.unwrap(), "hi");
        // The destination inherits the read mark: an immediate edit isn't blocked.
        assert!(c.was_read(&to), "the moved file is still 'read'");
    }

    #[tokio::test]
    async fn move_refuses_an_unread_source_and_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        write(&dir.path().join("a.txt"), "a").await;
        write(&dir.path().join("b.txt"), "b").await;

        // Unread source: you must know what you're moving.
        let err = MoveTool
            .execute(json!({"from": "a.txt", "to": "c.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hasn't been read"), "{err}");

        c.mark_read(&dir.path().join("a.txt"));
        let err = MoveTool
            .execute(json!({"from": "a.txt", "to": "b.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // b.txt is untouched.
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("b.txt"))
                .await
                .unwrap(),
            "b"
        );

        // With overwrite it goes through.
        MoveTool
            .execute(
                json!({"from": "a.txt", "to": "b.txt", "overwrite": true}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("b.txt"))
                .await
                .unwrap(),
            "a"
        );
    }

    /// The `write_ext` gate (a `plan` sub-agent) applies to the destination *and*
    /// the source: neither renaming a `.rs` away nor renaming a `.md` into one.
    #[tokio::test]
    async fn move_honors_the_write_ext_allow_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ctx(dir.path());
        c.write_allow_ext = Some(vec!["md".into()]);
        write(&dir.path().join("a.md"), "x").await;
        write(&dir.path().join("code.rs"), "x").await;
        c.mark_read(&dir.path().join("a.md"));
        c.mark_read(&dir.path().join("code.rs"));

        // md → rs: the destination is not writable.
        let err = MoveTool
            .execute(json!({"from": "a.md", "to": "a.rs"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only modify"), "{err}");
        // rs → md: the source is not the agent's to remove.
        let err = MoveTool
            .execute(json!({"from": "code.rs", "to": "code.md"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only modify"), "{err}");
        // md → md is fine.
        MoveTool
            .execute(json!({"from": "a.md", "to": "b.md"}), &c)
            .await
            .unwrap();
        assert!(dir.path().join("b.md").exists());
    }

    #[tokio::test]
    async fn delete_needs_a_read_file_and_recursive_for_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let f = dir.path().join("a.txt");
        write(&f, "a").await;

        let err = DeleteTool
            .execute(json!({"path": "a.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hasn't been read"), "{err}");
        c.mark_read(&f);
        DeleteTool
            .execute(json!({"path": "a.txt"}), &c)
            .await
            .unwrap();
        assert!(!f.exists());

        // A directory needs `recursive`, and is refused otherwise.
        write(&dir.path().join("d/x.txt"), "x").await;
        let err = DeleteTool
            .execute(json!({"path": "d"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("recursive"), "{err}");
        assert!(dir.path().join("d/x.txt").exists(), "nothing was deleted");

        let out = DeleteTool
            .execute(json!({"path": "d", "recursive": true}), &c)
            .await
            .unwrap();
        assert!(out.contains("1 file"), "{out}");
        assert!(!dir.path().join("d").exists());
    }

    #[tokio::test]
    async fn delete_refuses_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let err = DeleteTool
            .execute(json!({"path": "nope.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// `move` and `copy` create the destination's missing parent directories,
    /// however deep — for a file source and for a directory source alike. A
    /// model shouldn't have to `mkdir -p` first (and, having no shell, `plan`
    /// couldn't).
    #[tokio::test]
    async fn move_and_copy_create_nested_destination_directories() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());

        // copy: file into a path whose parents don't exist.
        write(&dir.path().join("a.txt"), "a").await;
        CopyTool
            .execute(json!({"from": "a.txt", "to": "x/y/z/a.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("x/y/z/a.txt"))
                .await
                .unwrap(),
            "a"
        );

        // move: file into a different deep path.
        c.mark_read(&dir.path().join("a.txt"));
        MoveTool
            .execute(json!({"from": "a.txt", "to": "p/q/r/b.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("p/q/r/b.txt"))
                .await
                .unwrap(),
            "a"
        );
        assert!(!dir.path().join("a.txt").exists());

        // copy: a directory into a deep path, nested contents intact.
        write(&dir.path().join("tree/deep/x.txt"), "x").await;
        CopyTool
            .execute(json!({"from": "tree", "to": "one/two/tree"}), &c)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("one/two/tree/deep/x.txt"))
                .await
                .unwrap(),
            "x"
        );

        // move: a directory into a deep path (rename needs the parent to exist).
        MoveTool
            .execute(json!({"from": "tree", "to": "three/four/tree"}), &c)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("three/four/tree/deep/x.txt"))
                .await
                .unwrap(),
            "x"
        );
        assert!(
            !dir.path().join("tree").exists(),
            "source moved, not copied"
        );
    }

    /// Overwriting an existing directory with `copy --overwrite` lands the new
    /// contents, replaces the old ones, and leaves no `.hrdr-stage`/`.hrdr-aside`
    /// litter in the parent — the destination is swapped in via a rename, never
    /// deleted before the replacement is in place.
    #[tokio::test]
    async fn copy_overwrite_of_a_dir_replaces_contents_and_leaves_no_litter() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        write(&dir.path().join("src/new.txt"), "fresh").await;
        write(&dir.path().join("dst/old.txt"), "stale").await;

        CopyTool
            .execute(json!({"from": "src", "to": "dst", "overwrite": true}), &c)
            .await
            .unwrap();

        // The destination now mirrors the source, and the old file is gone.
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("dst/new.txt"))
                .await
                .unwrap(),
            "fresh"
        );
        assert!(
            !dir.path().join("dst/old.txt").exists(),
            "the stale file must be replaced, not merged"
        );
        // No staging litter survives in the parent directory.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains("hrdr-stage") && !name.contains("hrdr-aside"),
                "staging litter left behind: {name}"
            );
        }
    }

    #[tokio::test]
    async fn copy_duplicates_a_file_and_a_tree_without_touching_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        write(&dir.path().join("a.txt"), "a").await;
        // No read mark needed: a copy destroys nothing.
        CopyTool
            .execute(json!({"from": "a.txt", "to": "b.txt"}), &c)
            .await
            .unwrap();
        assert!(dir.path().join("a.txt").exists(), "source survives");
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("b.txt"))
                .await
                .unwrap(),
            "a"
        );

        let err = CopyTool
            .execute(json!({"from": "a.txt", "to": "b.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        // Directories copy recursively, nested files included.
        write(&dir.path().join("tree/deep/x.txt"), "x").await;
        CopyTool
            .execute(json!({"from": "tree", "to": "tree2"}), &c)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("tree2/deep/x.txt"))
                .await
                .unwrap(),
            "x"
        );
    }
}
