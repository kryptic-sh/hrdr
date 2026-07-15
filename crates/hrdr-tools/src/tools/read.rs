use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::{DEFAULT_READ_LIMIT, MAX_LINE, MAX_READ_BYTES};

// ---- read ----

pub struct ReadTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a file from disk. Returns 1-based line-numbered content (the `N: ` prefix is \
         display-only — never include it in edit strings). Use `offset`/`limit` to page \
         through large files. You must read a file before editing it."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ReadArgs = crate::tool_args("read", args)?;
        let path = ctx.resolve(&a.path);
        crate::guard_secret_read(&path)?;
        if let Ok(meta) = tokio::fs::metadata(&path).await
            && meta.len() > MAX_READ_BYTES
        {
            bail!(
                "{} is {} bytes, over this tool's {MAX_READ_BYTES}-byte cap — it's too large to \
                 load whole; use `grep` to search it or `bash` (`sed`/`head`/`tail`) to slice out \
                 the range you need",
                path.display(),
                meta.len()
            );
        }
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                bail!(
                    "{} is not a text file (invalid UTF-8) — this tool only reads text; \
                     inspect binaries via bash (`file`, `hexdump -C`, `strings`) if needed",
                    path.display()
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        ctx.mark_read(&path);
        let start = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let n = i + 1;
            let line = &line[..crate::floor_char_boundary(line, MAX_LINE)];
            out.push_str(&format!("{n:>6}: {line}\n"));
        }
        if out.is_empty() {
            out.push_str("(file is empty or offset past end)");
        }
        Ok(truncate(&out, ctx.max_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_allows_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("notes.txt");
        std::fs::write(&target, "data").unwrap();

        let ctx = ToolContext::new(cwd.path().to_path_buf());
        let out = ReadTool
            .execute(serde_json::json!({"path": target.to_str().unwrap()}), &ctx)
            .await
            .expect("reads are not confined to cwd");
        assert!(out.contains("data"), "got: {out}");
    }
}
