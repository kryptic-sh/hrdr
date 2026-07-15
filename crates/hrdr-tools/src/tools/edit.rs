use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::MAX_READ_BYTES;
use super::mutation::apply_file_change;
use super::write::unified_diff;

/// Ceiling on the projected output of a `replace_all`. A growing replacement
/// (`old="e"`, `new=50KB`) across even a modest file can project to gigabytes —
/// enough to OOM the process before the `String` finishes allocating. 64 MiB is
/// far above any legitimate edit, so this only ever trips pathological input.
pub(crate) const MAX_EDIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

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
        if a.old_string.is_empty() {
            bail!(
                "`old_string` is empty — that matches at every position in the file, and with \
                 `replace_all` would corrupt it; pass the exact text to replace"
            );
        }
        let path = ctx.resolve(&a.path);
        ctx.ensure_writable_ext(&path)?;
        if !ctx.was_read(&path) {
            bail!(
                "you haven't read {} yet — call read first, then copy old_string \
                 exactly from its output",
                path.display()
            );
        }
        // Stat before reading: `read_to_string` buffers the whole file, so a
        // multi-gigabyte target would OOM before a single match is found. Reuse
        // `read`'s cap — an edit to a file larger than `read` can even show is a
        // mistake, not a workflow to support.
        if let Ok(meta) = tokio::fs::metadata(&path).await
            && meta.len() > MAX_READ_BYTES
        {
            bail!(
                "{} is {} bytes; too large to edit — narrow the change or use `replace`/`bash`",
                path.display(),
                meta.len()
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
            // Bound the allocation before making it: only a growing replacement
            // can blow up, and its output size is exactly computable from the
            // match count. Bail rather than let `String::replace` OOM.
            if a.new_string.len() > a.old_string.len() {
                let projected = text
                    .len()
                    .saturating_add(count.saturating_mul(a.new_string.len() - a.old_string.len()));
                if projected > MAX_EDIT_OUTPUT_BYTES {
                    bail!(
                        "this edit would produce ~{projected} bytes; narrow `old_string` or \
                         drop `replace_all`"
                    );
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;

    /// A file over `read`'s size cap is refused before `read_to_string` would
    /// buffer it whole, and the byte count is in the message so the model knows
    /// why. A sparse file (`set_len`) hits the cap without writing 50+ MiB.
    #[tokio::test]
    async fn edit_refuses_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_READ_BYTES + 1).unwrap();
        drop(f);
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({"path": path.to_str().unwrap(), "old_string": "a", "new_string": "b"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("too large to edit"), "{err}");
        assert!(
            err.contains(&(MAX_READ_BYTES + 1).to_string()),
            "the byte count must be reported: {err}"
        );
    }

    /// A `replace_all` whose projected output blows past the expansion cap is
    /// refused *before* the giant `String` is allocated — the guard is
    /// arithmetic on the match count, not a failed allocation.
    #[tokio::test]
    async fn edit_refuses_a_replace_all_that_would_explode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        // 2000 "e"s → 2000 matches; each grows by ~50 KB → ~100 MB projected,
        // well over the 64 MiB cap, but the file and replacement are tiny.
        std::fs::write(&path, "e".repeat(2000)).unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let big = "x".repeat(50_000);
        let err = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "e",
                    "new_string": big,
                    "replace_all": true,
                }),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("would produce"), "{err}");
        assert!(err.contains("narrow"), "{err}");
        // The file is untouched — the guard fired before any write.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "e".repeat(2000));
    }
}
