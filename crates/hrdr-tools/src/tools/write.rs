use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

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
         directories are created as needed. Overwriting an existing file requires reading \
         it first. Prefer `edit` for changing part of an existing file."
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
        ctx.ensure_writable_ext(&path)?;
        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
        if existed && !ctx.was_read(&path) {
            bail!(
                "{} exists but you haven't read it — call read first so the rewrite \
                 starts from its real content (or use edit for a partial change)",
                path.display()
            );
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
