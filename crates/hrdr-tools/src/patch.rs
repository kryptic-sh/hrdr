//! `patch` tool: apply a unified diff (git/patch format) across one or more
//! files in a single call. Integrates with the read-before-edit gate and
//! post-edit hooks. Applied **atomically** — every file's hunks are validated in
//! memory first, and only if all apply is anything written.

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
        original: Option<String>,
    },
    Delete {
        path: PathBuf,
        original: String,
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
         is optional; use `/dev/null` as the path to create or delete a file). Hunk line counts \
         are derived from the body, so inaccurate counts are repaired and a bare `@@` header is \
         accepted when its context identifies one unique location. Read each file first. Atomic: \
         if any hunk fails to apply, nothing is written. For a single small change, prefer `edit`."
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
        let files = split_patch(&a.patch)?;
        if files.is_empty() {
            bail!("no file sections in the patch — need `--- `/`+++ ` headers per file");
        }

        // Repeated sections would each plan against the same on-disk base, so a
        // later section could silently overwrite an earlier section's result.
        let mut targets = std::collections::HashSet::new();
        for fd in &files {
            let target = fd.new_path.as_ref().or(fd.old_path.as_ref());
            if let Some(path) = target {
                let resolved = crate::canonicalize_nearest(&ctx.resolve(path));
                if !targets.insert(resolved) {
                    bail!(
                        "patch not applied (no files changed): duplicate file section for {path}"
                    );
                }
            }
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

        // Phase 2 — commit. Roll back every attempted path if any filesystem
        // operation fails, so validation atomicity extends through writes.
        for i in 0..ops.len() {
            let result = match &ops[i] {
                FileOp::Write { path, content, .. } => {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await.ok();
                    }
                    crate::tools::mutation::atomic_write(path, content)
                        .await
                        .with_context(|| format!("writing {}", path.display()))
                }
                FileOp::Delete { path, .. } => tokio::fs::remove_file(path)
                    .await
                    .with_context(|| format!("deleting {}", path.display())),
            };
            if let Err(error) = result {
                let rollback_errors = rollback_ops(&ops[..=i]).await;
                let suffix = if rollback_errors.is_empty() {
                    "earlier filesystem changes rolled back".to_string()
                } else {
                    format!("rollback incomplete: {}", rollback_errors.join("; "))
                };
                return Err(error.context(format!("patch not applied; {suffix}")));
            }
        }

        let mut summary = Vec::new();
        for op in &ops {
            match op {
                FileOp::Write {
                    path,
                    content,
                    original,
                } => {
                    let mut notes =
                        crate::run_file_hooks(&ctx.hooks, "patch", path, &ctx.cwd).await;
                    ctx.mark_read(path);
                    if let Some(lsp) = &ctx.lsp {
                        let on_disk = tokio::fs::read_to_string(path)
                            .await
                            .unwrap_or_else(|_| content.clone());
                        if let Some(note) = lsp.diagnostics_note(path, &on_disk).await {
                            notes.push(note);
                        }
                    }
                    let verb = if original.is_some() {
                        "patched"
                    } else {
                        "created"
                    };
                    let mut line = format!("{verb} {} ({} bytes)", rel(path, ctx), content.len());
                    if !notes.is_empty() {
                        line.push_str(&format!("  [{}]", notes.join("; ")));
                    }
                    summary.push(line);
                }
                FileOp::Delete { path, .. } => summary.push(format!("deleted {}", rel(path, ctx))),
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

async fn rollback_ops(ops: &[FileOp]) -> Vec<String> {
    let mut errors = Vec::new();
    for op in ops.iter().rev() {
        let result = match op {
            FileOp::Write {
                path,
                original: Some(content),
                ..
            }
            | FileOp::Delete {
                path,
                original: content,
            } => tokio::fs::write(path, content).await,
            FileOp::Write {
                path,
                original: None,
                ..
            } => tokio::fs::remove_file(path).await,
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", op.path().display()));
        }
    }
    errors
}

impl FileOp {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Write { path, .. } | Self::Delete { path, .. } => path,
        }
    }
}

/// Read the target, apply the file's hunks with [`diffy`], and return the
/// pending [`FileOp`] — enforcing confinement and the read-before-edit gate.
async fn plan_file(fd: &FileDiff, ctx: &ToolContext) -> Result<FileOp> {
    // Deletion: `+++ /dev/null`. Validate that the hunk applies and removes
    // the complete file before scheduling the filesystem operation.
    if fd.new_path.is_none() {
        let old = fd
            .old_path
            .as_ref()
            .ok_or_else(|| anyhow!("patch section has no file path"))?;
        let path = ctx.resolve(old);
        match ctx.read_state(&path) {
            crate::ReadState::Unread => {
                bail!("{}: read it before deleting it via patch", path.display())
            }
            crate::ReadState::Stale => bail!(
                "{}: changed on disk since you read it — re-read it before patching",
                path.display()
            ),
            crate::ReadState::Partial | crate::ReadState::Fresh => {}
        }
        let original = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let normalized = normalize_diff(&fd.diff, &original, false)
            .map_err(|e| anyhow!("{}: invalid diff — {e}", path.display()))?;
        let patch = diffy::Patch::from_str(&normalized).map_err(|e| {
            anyhow!(
                "{}: invalid diff after repairing hunk headers — {e}",
                path.display()
            )
        })?;
        let remaining = diffy::apply(&original, &patch)
            .map_err(|e| anyhow!("{}: hunks don't apply — {e}", path.display()))?;
        if !remaining.is_empty() {
            bail!(
                "{}: deletion patch leaves {} bytes; a /dev/null patch must remove the complete file",
                path.display(),
                remaining.len()
            );
        }
        return Ok(FileOp::Delete { path, original });
    }

    if fd.old_path.is_some() && fd.old_path != fd.new_path {
        bail!("patch renames are not supported; old and new paths must match");
    }

    let path = ctx.resolve(fd.new_path.as_ref().unwrap());
    let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
    let base = if exists {
        match ctx.read_state(&path) {
            crate::ReadState::Unread => bail!("{}: read it before patching it", path.display()),
            crate::ReadState::Stale => bail!(
                "{}: changed on disk since you read it — re-read it before patching",
                path.display()
            ),
            crate::ReadState::Partial | crate::ReadState::Fresh => {}
        }
        tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new() // creating a new file
    };
    let original = exists.then(|| base.clone());
    // Run the diffy round-trip in LF space. `normalize_diff` splits on
    // `str::lines()` (which drops a trailing `\r`) and the diff the model
    // supplies is LF, so the repaired patch is always LF; a CRLF base would then
    // never byte-match the patch's context lines and every hunk would fail to
    // apply. Normalize the base to LF for location + application, then restore
    // the file's CRLF endings on the result so untouched lines keep them.
    let uses_crlf = base.contains("\r\n");
    let base_lf: std::borrow::Cow<str> = if uses_crlf {
        std::borrow::Cow::Owned(base.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(&base)
    };
    let normalized = normalize_diff(&fd.diff, &base_lf, !exists)
        .map_err(|e| anyhow!("{}: invalid diff — {e}", path.display()))?;
    let patch = diffy::Patch::from_str(&normalized).map_err(|e| {
        anyhow!(
            "{}: invalid diff after repairing hunk headers — {e}",
            path.display()
        )
    })?;
    let content_lf = diffy::apply(&base_lf, &patch)
        .map_err(|e| anyhow!("{}: hunks don't apply — {e}", path.display()))?;
    let content = if uses_crlf {
        content_lf.replace('\n', "\r\n")
    } else {
        content_lf
    };
    Ok(FileOp::Write {
        path,
        content,
        original,
    })
}

/// Repair hunk metadata before handing a patch to `diffy`. Models reliably
/// produce body prefixes and context but often miscount header rows. Counts are
/// therefore derived from the body. A bare `@@` gets its old-file start from a
/// unique exact match of the hunk's context/removal sequence.
fn normalize_diff(diff: &str, base: &str, creating: bool) -> Result<String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() && !lines[i].starts_with("@@") {
        out.push(lines[i].to_string());
        i += 1;
    }
    let base_lines: Vec<&str> = base.lines().collect();
    let mut line_delta = 0isize;
    let mut hunk_number = 0usize;
    while i < lines.len() {
        if !lines[i].starts_with("@@") {
            bail!("expected a hunk header, found {:?}", lines[i]);
        }
        hunk_number += 1;
        let header = lines[i];
        let bare = header == "@@";
        if !bare && !valid_standard_header(header) {
            bail!(
                "hunk {hunk_number} has a malformed header {header:?}; use `@@`, or `@@ -OLD[,COUNT] +NEW[,COUNT] @@`"
            );
        }
        i += 1;
        let body_start = i;
        while i < lines.len() && !lines[i].starts_with("@@") {
            i += 1;
        }
        let body = &lines[body_start..i];
        if body.is_empty() {
            bail!("hunk {hunk_number} has no body");
        }
        for line in body {
            if !matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\')) {
                bail!(
                    "hunk {hunk_number} has a line without a diff prefix: {line:?}; prefix it with space, `+`, or `-`"
                );
            }
        }
        let old_count = body
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('-'))
            .count();
        let new_count = body
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('+'))
            .count();
        let old_sequence: Vec<&str> = body
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('-'))
            .map(|line| &line[1..])
            .collect();
        let old_start = locate_old_sequence(
            &base_lines,
            &old_sequence,
            parse_old_start(header),
            hunk_number,
            bare,
            creating,
        )?;
        let new_start = if old_start == 0 {
            usize::from(new_count > 0)
        } else {
            (old_start as isize + line_delta).max(1) as usize
        };
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            old_start, old_count, new_start, new_count
        ));
        out.extend(body.iter().map(|line| (*line).to_string()));
        line_delta += new_count as isize - old_count as isize;
    }
    Ok(format!("{}\n", out.join("\n")))
}

fn parse_old_start(header: &str) -> Option<usize> {
    header
        .strip_prefix("@@ -")?
        .split_once(' ')?
        .0
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn valid_standard_header(header: &str) -> bool {
    let Some(rest) = header.strip_prefix("@@ -") else {
        return false;
    };
    let Some((old, rest)) = rest.split_once(" +") else {
        return false;
    };
    let Some((new, suffix)) = rest.split_once(" @@") else {
        return false;
    };
    valid_range(old) && valid_range(new) && (suffix.is_empty() || suffix.starts_with(' '))
}

fn valid_range(range: &str) -> bool {
    let mut parts = range.split(',');
    parts.next().is_some_and(|v| v.parse::<usize>().is_ok())
        && parts.next().is_none_or(|v| v.parse::<usize>().is_ok())
        && parts.next().is_none()
}

fn locate_old_sequence(
    base: &[&str],
    old_sequence: &[&str],
    declared_start: Option<usize>,
    hunk_number: usize,
    bare: bool,
    creating: bool,
) -> Result<usize> {
    if old_sequence.is_empty() {
        if bare && !creating {
            bail!(
                "hunk {hunk_number} is a location-free addition to an existing file; add an explicit hunk range or unchanged context"
            );
        }
        return Ok(declared_start.unwrap_or(0));
    }
    if let Some(start) = declared_start {
        let index = start.saturating_sub(1);
        if index
            .checked_add(old_sequence.len())
            .and_then(|end| base.get(index..end))
            == Some(old_sequence)
        {
            return Ok(start);
        }
    }
    let starts = sequence_starts(base, old_sequence);
    match starts.as_slice() {
        [start] => Ok(*start + 1),
        [] => bail!(
            "hunk {hunk_number} old/context lines do not match the file; re-read it and copy exact current text"
        ),
        _ => bail!(
            "hunk {hunk_number} old/context lines match {} locations; add more context to identify one location",
            starts.len()
        ),
    }
}

fn sequence_starts(haystack: &[&str], needle: &[&str]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(i, window)| (window == needle).then_some(i))
        .collect()
}

/// Split a (possibly multi-file) unified diff into per-file slices. Splits on
/// `diff --git ` lines when present, else on each `--- `/`+++ ` header pair — so
/// a removed line that happens to start with `--- ` isn't mistaken for a header.
fn split_patch(patch: &str) -> Result<Vec<FileDiff>> {
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
            bail!(
                "patch section {} is missing `---` or `+++` file headers",
                k + 1
            );
        };
        if pi != mi + 1 {
            bail!("patch section {} has unordered file headers", k + 1);
        }
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
    Ok(out)
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
        let files = split_patch(patch).unwrap();
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
        let files = split_patch(patch).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].old_path.is_none()); // creation
        assert_eq!(files[0].new_path.as_deref(), Some("new.txt"));
        assert!(files[1].new_path.is_none()); // deletion
        assert_eq!(files[1].old_path.as_deref(), Some("gone.txt"));
    }

    #[test]
    fn repairs_inaccurate_hunk_counts() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,99 +1,42 @@\n one\n-two\n+TWO\n three\n";
        let normalized = normalize_diff(diff, "one\ntwo\nthree\n", false).unwrap();
        assert!(normalized.contains("@@ -1,3 +1,3 @@"), "{normalized}");
        let patch = diffy::Patch::from_str(&normalized).unwrap();
        assert_eq!(
            diffy::apply("one\ntwo\nthree\n", &patch).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn repairs_wrong_hunk_positions_when_context_is_unique() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -99,1 +88,1 @@\n-old\n+new\n";
        let normalized = normalize_diff(diff, "head\nold\ntail\n", false).unwrap();
        assert!(normalized.contains("@@ -2,1 +2,1 @@"), "{normalized}");
    }

    #[test]
    fn wrong_hunk_position_with_ambiguous_context_errors() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -99 +99 @@\n-same\n+new\n";
        let err = normalize_diff(diff, "same\nother\nsame\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("match 2 locations"), "{err}");
    }

    #[test]
    fn huge_declared_position_does_not_panic() {
        let diff = format!(
            "--- a/a.txt\n+++ b/a.txt\n@@ -{} +1 @@\n-old\n+new\n",
            usize::MAX
        );
        let normalized = normalize_diff(&diff, "old\n", false).unwrap();
        assert!(normalized.contains("@@ -1,1 +1,1 @@"), "{normalized}");
    }

    #[test]
    fn standard_hunk_header_remains_equivalent() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -2,2 +2,2 @@ label\n-old\n+new\n tail\n";
        let normalized = normalize_diff(diff, "head\nold\ntail\n", false).unwrap();
        assert!(normalized.contains("@@ -2,2 +2,2 @@"), "{normalized}");
    }

    #[test]
    fn omitted_counts_are_derived() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -2 +2 @@\n-old\n+new\n";
        let normalized = normalize_diff(diff, "head\nold\n", false).unwrap();
        assert!(normalized.contains("@@ -2,1 +2,1 @@"), "{normalized}");
    }

    #[test]
    fn bare_hunk_header_finds_unique_context() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n beta\n-old\n+new\n gamma\n";
        let normalized = normalize_diff(diff, "alpha\nbeta\nold\ngamma\nomega\n", false).unwrap();
        assert!(normalized.contains("@@ -2,3 +2,3 @@"), "{normalized}");
        let patch = diffy::Patch::from_str(&normalized).unwrap();
        assert_eq!(
            diffy::apply("alpha\nbeta\nold\ngamma\nomega\n", &patch).unwrap(),
            "alpha\nbeta\nnew\ngamma\nomega\n"
        );
    }

    #[test]
    fn bare_hunk_header_rejects_ambiguous_context() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n-same\n+changed\n";
        let err = normalize_diff(diff, "same\nother\nsame\n", false).unwrap_err();
        let shown = err.to_string();
        assert!(shown.contains("match 2 locations"), "{shown}");
        assert!(shown.contains("more context"), "{shown}");
    }

    #[test]
    fn bare_hunk_header_rejects_missing_context() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n-missing\n+changed\n";
        let err = normalize_diff(diff, "present\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("do not match the file"), "{err}");
    }

    #[test]
    fn bare_addition_creates_a_new_file() {
        let diff = "--- /dev/null\n+++ b/a.txt\n@@\n+first\n+second\n";
        let normalized = normalize_diff(diff, "", true).unwrap();
        assert!(normalized.contains("@@ -0,0 +1,2 @@"), "{normalized}");
        let patch = diffy::Patch::from_str(&normalized).unwrap();
        assert_eq!(diffy::apply("", &patch).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn bare_addition_to_existing_file_requires_location() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n+first\n";
        let err = normalize_diff(diff, "existing\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("location-free addition"), "{err}");
    }

    #[test]
    fn malformed_hunk_headers_are_rejected() {
        for header in [
            "@@ nonsense",
            "@@ -x +1 @@",
            "@@ -1 +x @@",
            "@@ -1,2,3 +1 @@",
            "@@ -1 +1",
        ] {
            let diff = format!("--- a/a.txt\n+++ b/a.txt\n{header}\n-old\n+new\n");
            let err = normalize_diff(&diff, "old\n", false)
                .unwrap_err()
                .to_string();
            assert!(err.contains("malformed header"), "{header}: {err}");
        }
    }

    #[test]
    fn bare_deletion_removes_unique_lines() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n-two\n-three\n";
        let normalized = normalize_diff(diff, "one\ntwo\nthree\nfour\n", false).unwrap();
        assert!(normalized.contains("@@ -2,2 +2,0 @@"), "{normalized}");
        let patch = diffy::Patch::from_str(&normalized).unwrap();
        assert_eq!(
            diffy::apply("one\ntwo\nthree\nfour\n", &patch).unwrap(),
            "one\nfour\n"
        );
    }

    #[test]
    fn multiple_bare_hunks_derive_shifted_new_positions() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n-one\n+ONE\n+extra\n@@\n-three\n+THREE\n";
        let normalized = normalize_diff(diff, "one\ntwo\nthree\n", false).unwrap();
        assert!(normalized.contains("@@ -1,1 +1,2 @@"), "{normalized}");
        assert!(normalized.contains("@@ -3,1 +4,1 @@"), "{normalized}");
        let patch = diffy::Patch::from_str(&normalized).unwrap();
        assert_eq!(
            diffy::apply("one\ntwo\nthree\n", &patch).unwrap(),
            "ONE\nextra\ntwo\nTHREE\n"
        );
    }

    #[test]
    fn rejects_unprefixed_hunk_body_lines_with_guidance() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\nold\n+new\n";
        let err = normalize_diff(diff, "old\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("without a diff prefix"), "{err}");
        assert!(err.contains("prefix it with space"), "{err}");
    }

    #[test]
    fn rejects_an_empty_hunk() {
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@\n";
        let err = normalize_diff(diff, "old\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no body"), "{err}");
    }

    #[tokio::test]
    async fn applies_a_multi_hunk_patch_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "one\ntwo\nthree\n").await.unwrap();
        tokio::fs::write(&b, "x\ny\n").await.unwrap();

        let ctx = ToolContext::new(dir.path());
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
    async fn applies_a_patch_to_a_crlf_file_and_keeps_crlf() {
        // Regression: `normalize_diff` works in LF space, so a CRLF base must be
        // normalized for the diffy round-trip or every hunk fails to apply. The
        // file's `\r\n` endings must survive on the untouched lines too.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        tokio::fs::write(&a, "one\r\ntwo\r\nthree\r\n")
            .await
            .unwrap();

        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&a);

        // An LF-terminated diff (what a model emits) against a CRLF file.
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";
        let out = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .expect("patch applies to a CRLF file");
        assert!(out.contains("1 file"), "{out}");
        assert_eq!(
            tokio::fs::read_to_string(&a).await.unwrap(),
            "one\r\nTWO\r\nthree\r\n"
        );
    }

    #[tokio::test]
    async fn a_bad_hunk_aborts_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "one\n").await.unwrap();
        tokio::fs::write(&b, "keep\n").await.unwrap();

        let ctx = ToolContext::new(dir.path());
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

    #[tokio::test]
    async fn duplicate_file_sections_are_rejected_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        tokio::fs::write(&path, "one\ntwo\n").await.unwrap();
        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&path);

        let patch = "--- a/same.txt\n+++ b/same.txt\n@@ -1 +1 @@\n-one\n+ONE\n--- a/same.txt\n+++ b/same.txt\n@@ -2 +2 @@\n-two\n+TWO\n";
        let err = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate file section"), "{err}");
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "one\ntwo\n"
        );
    }

    #[tokio::test]
    async fn wrong_deletion_hunk_does_not_delete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.txt");
        tokio::fs::write(&path, "keep\n").await.unwrap();
        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&path);

        let patch = "--- a/keep.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-wrong\n";
        PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "keep\n");
    }

    #[tokio::test]
    async fn partial_deletion_hunk_does_not_delete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.txt");
        tokio::fs::write(&path, "one\ntwo\n").await.unwrap();
        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&path);

        let patch = "--- a/keep.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-one\n";
        let err = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("must remove the complete file"),
            "{err}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "one\ntwo\n"
        );
    }

    #[tokio::test]
    async fn differing_paths_are_rejected_as_unsupported_rename() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        tokio::fs::write(&old, "old\n").await.unwrap();
        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&old);

        let patch = "--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let err = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("renames are not supported"),
            "{err}"
        );
        assert!(!dir.path().join("new.txt").exists());
    }

    /// A deletion target that cannot be read as a file fails during validation,
    /// before any filesystem changes are attempted.
    #[tokio::test]
    async fn an_invalid_deletion_target_errors_without_removal() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("adir");
        tokio::fs::create_dir(&sub).await.unwrap();

        let ctx = ToolContext::new(dir.path());
        ctx.mark_read(&sub);

        let patch = "--- a/adir\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        let err = PatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reading"), "{err}");
        assert!(sub.exists(), "the directory must still be there");
    }
}
