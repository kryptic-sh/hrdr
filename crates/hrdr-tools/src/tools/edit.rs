use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::mutation::apply_file_change;
use super::write::unified_diff;

// ---- edit ----

pub struct EditTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace an exact substring in a file (the preferred, token-cheap way to change \
         it). Copy `old_string` exactly from read output — same whitespace, line-number \
         prefixes stripped — and include enough surrounding lines to be unique. Requires \
         having read the file first."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace (include surrounding context to make it unique)."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: EditArgs = crate::tool_args("edit", args)?;
        let path = ctx.resolve(&a.path);
        ctx.ensure_within_cwd(&path)?;
        if !ctx.was_read(&path) {
            bail!(
                "you haven't read {} yet — call read first, then copy old_string \
                 exactly from its output",
                path.display()
            );
        }
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let count = text.matches(&a.old_string).count();
        if count == 0 {
            // The #1 retry cause: right text, wrong whitespace. Detect it and
            // say so instead of the generic error.
            let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
            let normalized_old = norm(&a.old_string);
            if !normalized_old.is_empty() && norm(&text).contains(&normalized_old) {
                bail!(
                    "old_string not found in {}, but a near-match differing only in \
                     whitespace/indentation exists — copy the exact text from read \
                     output (keep tabs/spaces, strip the line-number prefix)",
                    path.display()
                );
            }
            bail!(
                "old_string not found in {} — the file may have changed since you read it; \
                 re-read it and copy the exact current text (whitespace included, no \
                 line-number prefixes)",
                path.display()
            );
        }
        if count > 1 && !a.replace_all {
            bail!(
                "old_string is not unique in {} ({count} matches) — include more \
                 surrounding lines to pin one occurrence, or set replace_all",
                path.display()
            );
        }
        let updated = if a.replace_all {
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        let fc = apply_file_change(ctx, &path, "edit", &updated).await?;
        let mut warn = fc.notes.join("\n");
        if !warn.is_empty() {
            warn.insert(0, '\n');
        }
        let diff = unified_diff(&path.display().to_string(), &text, &fc.content_after);
        Ok(truncate(
            &format!(
                "Replaced {count} occurrence(s) in {}{warn}\n{diff}",
                path.display()
            ),
            ctx.max_output,
        ))
    }
}
