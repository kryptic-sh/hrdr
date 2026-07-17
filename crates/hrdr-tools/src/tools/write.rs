use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::MAX_LINE;

// ---- write ----

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Create a new file or fully rewrite an existing one with `content`. Parent \
         directories are created as needed. Overwriting an existing file requires a \
         complete, fresh read first — a partial read (paged, or clipped by a long line) or \
         a stale one (the file changed on disk since) is refused; re-read after any \
         external change. Prefer `edit` for changing part of an existing file."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "content": {"type": "string", "description": "Full file contents to write."}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: WriteArgs = crate::tool_args("write", args)?;
        let path = ctx.resolve(&a.path);
        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
        if existed {
            // A `write` replaces the whole file, so the model must have seen the
            // whole current content: not unread, not a partial page, and not a
            // version that has since changed on disk.
            match ctx.read_state(&path) {
                crate::ReadState::Unread => bail!(
                    "{} exists but you haven't read it — call read first so the rewrite \
                     starts from its real content (or use edit for a partial change)",
                    path.display()
                ),
                crate::ReadState::Partial => bail!(
                    "you've only read part of {} — a write replaces the whole file, so read \
                     it in full first (no offset/limit, or page to the end) or the unread \
                     lines will be lost; use edit for a partial change. Note: if this file \
                     has a line over {MAX_LINE} bytes, `read` clips that line every time no \
                     matter how it's paged, so it can never be marked fully read — retrying \
                     read then write will loop forever; use `edit` (targets known text, not \
                     the whole file) or `bash` instead",
                    path.display()
                ),
                crate::ReadState::Stale => bail!(
                    "{} changed on disk since you read it — re-read it before overwriting, \
                     or the edit made in the meantime (an editor save, a formatter) is lost",
                    path.display()
                ),
                crate::ReadState::Fresh => {}
            }
        }
        let old = if existed {
            tokio::fs::read_to_string(&path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = a.content.len();
        let fc = super::mutation::apply_file_change(ctx, &path, "write", &a.content).await?;
        ctx.mark_read(&path); // the model authored (or just saw) this content
        let mut warn = fc.notes.join("\n");
        if !warn.is_empty() {
            warn.insert(0, '\n');
        }
        if existed {
            let diff = unified_diff(&path.display().to_string(), &old, &fc.content_after);
            let body = if diff.is_empty() {
                "(no changes)".to_string()
            } else {
                diff
            };
            Ok(truncate(
                &format!("Wrote {bytes} bytes to {}{warn}\n{body}", path.display()),
                ctx.max_output,
            ))
        } else {
            Ok(format!(
                "Created {} ({} lines){warn}",
                path.display(),
                fc.content_after.lines().count()
            ))
        }
    }
}

/// A unified diff of `old` → `new` for `path`, or empty if unchanged.
pub(crate) fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (MINOR): a file with a line over `MAX_LINE` bytes is
    /// *permanently* `Partial` (`read` clips that line every time, no matter
    /// how it's paged — see `read.rs`), so the generic "read it in full"
    /// advice can never be satisfied and the model would loop forever on
    /// read-then-write. The refusal must instead point at `edit`/`bash`.
    #[tokio::test]
    async fn write_refusal_on_an_over_long_line_points_at_edit_not_a_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big_line.txt");
        // One line comfortably over MAX_LINE, so `read` always clips it.
        let long_line = "x".repeat(MAX_LINE + 500);
        std::fs::write(&path, format!("{long_line}\n")).unwrap();

        let ctx = ToolContext::new(dir.path());
        // A full read (default offset/limit reaches EOF) still can't see the
        // over-long line whole, so it's recorded as partial, not complete.
        crate::ReadTool
            .execute(serde_json::json!({"path": "big_line.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);

        let err = WriteTool
            .execute(
                serde_json::json!({"path": "big_line.txt", "content": "replacement\n"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("edit") && err.contains("bash"),
            "the refusal must point at a workaround that can actually succeed: {err}"
        );
        assert!(
            err.contains("never"),
            "and explain why re-reading won't help: {err}"
        );
    }
}
