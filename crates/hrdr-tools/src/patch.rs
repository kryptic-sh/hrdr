//! `patch` tool: apply a unified diff (git/patch format) across one or more
//! files in a single call. Integrates with hrdr's cwd confinement, the
//! read-before-edit gate, per-turn checkpoints, and post-edit hooks. Applied
//! **atomically** — every file's hunks are validated in memory first, and only
//! if all apply is anything written.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

pub struct PatchTool;

#[derive(Deserialize)]
struct PatchArgs {
    patch: String,
}

/// One file's slice of a (possibly multi-file) unified diff.
struct FileDiff {
    /// Path from the `--- a/<path>` header (`None` for `/dev/null` = creation).
    old_path: Option<String>,
    /// Path from the `+++ b/<path>` header (`None` for `/dev/null` = deletion).
    new_path: Option<String>,
    /// The `--- / +++ / @@…` text for this file (what diffy parses).
    diff: String,
}

/// A validated, ready-to-write change.
enum FileOp {
    Write {
        path: PathBuf,
        content: String,
        existed: bool,
    },
    Delete {
        path: PathBuf,
    },
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &'static str {
        "patch"
    }
    fn description(&self) -> &'static str {
        "Apply a unified diff (git/patch format) across one or more files in a single call — \
         the efficient way to make multi-file or multi-hunk changes. Each file section has \
         `--- a/<path>` and `+++ b/<path>` headers followed by `@@` hunks (a `diff --git` line \
         is optional; use `/dev/null` as the path to create or delete a file). Read each file \
         first. Atomic: if any hunk fails to apply, nothing is written. For a single small \
         change, prefer `edit`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "A unified diff (git/patch format) covering one or more files."
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: PatchArgs = crate::tool_args("patch", args)?;
        let files = split_patch(&a.patch);
        if files.is_empty() {
            bail!("no file sections in the patch — need `--- `/`+++ ` headers per file");
        }

        // Phase 1 — validate + compute each file's result in memory (atomic).
        let mut ops = Vec::new();
        let mut errors = Vec::new();
        for fd in &files {
            match plan_file(fd, ctx).await {
                Ok(op) => ops.push(op),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if !errors.is_empty() {
            bail!(
                "patch not applied (no files changed):\n{}",
                errors.join("\n")
            );
        }

        // Phase 2 — write.
        let mut summary = Vec::new();
        for op in ops {
            match op {
                FileOp::Write {
                    path,
                    content,
                    existed,
                } => {
                    ctx.checkpoint(&path);
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await.ok();
                    }
                    let bytes = content.len();
                    tokio::fs::write(&path, &content)
                        .await
                        .with_context(|| format!("writing {}", path.display()))?;
                    let notes = crate::run_file_hooks(&ctx.hooks, "patch", &path, &ctx.cwd).await;
                    ctx.mark_read(&path);
                    let verb = if existed { "patched" } else { "created" };
                    let mut line = format!("{verb} {} ({bytes} bytes)", rel(&path, ctx));
                    if !notes.is_empty() {
                        line.push_str(&format!("  [{}]", notes.join("; ")));
                    }
                    summary.push(line);
                }
                FileOp::Delete { path } => {
                    ctx.checkpoint(&path);
                    tokio::fs::remove_file(&path).await.ok();
                    summary.push(format!("deleted {}", rel(&path, ctx)));
                }
            }
        }
        Ok(truncate(
            &format!(
                "Applied patch to {} file{}:\n{}",
                summary.len(),
                if summary.len() == 1 { "" } else { "s" },
                summary.join("\n")
            ),
            ctx.max_output,
        ))
    }
}

/// Read the target, apply the file's hunks with [`diffy`], and return the
/// pending [`FileOp`] — enforcing confinement and the read-before-edit gate.
async fn plan_file(fd: &FileDiff, ctx: &ToolContext) -> Result<FileOp> {
    // Deletion: `+++ /dev/null`.
    if fd.new_path.is_none() {
        let old = fd
            .old_path
            .as_ref()
            .ok_or_else(|| anyhow!("patch section has no file path"))?;
        let path = ctx.resolve(old);
        ctx.ensure_within_cwd(&path)?;
        if !ctx.was_read(&path) {
            bail!("{}: read it before deleting it via patch", path.display());
        }
        return Ok(FileOp::Delete { path });
    }

    let path = ctx.resolve(fd.new_path.as_ref().unwrap());
    ctx.ensure_within_cwd(&path)?;
    let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let base = if exists {
        if !ctx.was_read(&path) {
            bail!("{}: read it before patching it", path.display());
        }
        tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new() // creating a new file
    };
    let patch = diffy::Patch::from_str(&fd.diff)
        .map_err(|e| anyhow!("{}: invalid diff — {e}", path.display()))?;
    let content = diffy::apply(&base, &patch)
        .map_err(|e| anyhow!("{}: hunks don't apply — {e}", path.display()))?;
    Ok(FileOp::Write {
        path,
        content,
        existed: exists,
    })
}

/// Split a (possibly multi-file) unified diff into per-file slices. Splits on
/// `diff --git ` lines when present, else on each `--- `/`+++ ` header pair — so
/// a removed line that happens to start with `--- ` isn't mistaken for a header.
fn split_patch(patch: &str) -> Vec<FileDiff> {
    let lines: Vec<&str> = patch.lines().collect();
    let has_git = lines.iter().any(|l| l.starts_with("diff --git "));
    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            if has_git {
                lines[i].starts_with("diff --git ")
            } else {
                lines[i].starts_with("--- ")
                    && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))
            }
        })
        .collect();

    let mut out = Vec::new();
    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];
        let (Some(mi), Some(pi)) = (
            section.iter().position(|l| l.starts_with("--- ")),
            section.iter().position(|l| l.starts_with("+++ ")),
        ) else {
            continue; // header lines missing → not a real file section
        };
        let old_path = parse_diff_path(&section[mi][4..]);
        let new_path = parse_diff_path(&section[pi][4..]);
        // diffy wants the file's `--- … @@ …` block, newline-terminated.
        let diff = format!("{}\n", section[mi..].join("\n"));
        out.push(FileDiff {
            old_path,
            new_path,
            diff,
        });
    }
    out
}

/// A diff header path (`a/foo.rs`, `b/foo.rs`, `/dev/null`, possibly followed by
/// a tab + timestamp) → the repo-relative path, or `None` for `/dev/null`.
fn parse_diff_path(header: &str) -> Option<String> {
    let p = header.split('\t').next().unwrap_or(header).trim();
    if p == "/dev/null" {
        return None;
    }
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    Some(p.to_string())
}

/// `path` relative to the cwd (for display), else the full path.
fn rel(path: &std::path::Path, ctx: &ToolContext) -> String {
    path.strip_prefix(&ctx.cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_multi_file_git_patch() {
        let patch = "\
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1 +1 @@
-old
+new
diff --git a/bar.txt b/bar.txt
--- a/bar.txt
+++ b/bar.txt
@@ -1 +1 @@
-a
+b
";
        let files = split_patch(patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("foo.txt"));
        assert_eq!(files[1].new_path.as_deref(), Some("bar.txt"));
        assert!(files[0].diff.contains("+new"));
    }

    #[test]
    fn split_headerless_patch_and_dev_null() {
        let patch = "\
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = split_patch(patch);
        assert_eq!(files.len(), 2);
        assert!(files[0].old_path.is_none()); // creation
        assert_eq!(files[0].new_path.as_deref(), Some("new.txt"));
        assert!(files[1].new_path.is_none()); // deletion
        assert_eq!(files[1].old_path.as_deref(), Some("gone.txt"));
    }

    #[tokio::test]
    async fn applies_a_multi_hunk_patch_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "one\ntwo\nthree\n").await.unwrap();
        tokio::fs::write(&b, "x\ny\n").await.unwrap();

        let mut ctx = ToolContext::new(dir.path());
        ctx.restrict_to_cwd = false;
        ctx.mark_read(&a);
        ctx.mark_read(&b);

        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
--- a/b.txt
+++ b/b.txt
@@ -1,2 +1,2 @@
-x
+X
 y
";
        let out = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .expect("patch applies");
        assert!(out.contains("2 files"));
        assert_eq!(
            tokio::fs::read_to_string(&a).await.unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(tokio::fs::read_to_string(&b).await.unwrap(), "X\ny\n");
    }

    #[tokio::test]
    async fn a_bad_hunk_aborts_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "one\n").await.unwrap();
        tokio::fs::write(&b, "keep\n").await.unwrap();

        let mut ctx = ToolContext::new(dir.path());
        ctx.restrict_to_cwd = false;
        ctx.mark_read(&a);
        ctx.mark_read(&b);

        // b's hunk expects "wrong" context that isn't there → whole patch fails.
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-one
+ONE
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-wrong
+changed
";
        let err = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not applied"), "{err}");
        // Neither file changed.
        assert_eq!(tokio::fs::read_to_string(&a).await.unwrap(), "one\n");
        assert_eq!(tokio::fs::read_to_string(&b).await.unwrap(), "keep\n");
    }
}
