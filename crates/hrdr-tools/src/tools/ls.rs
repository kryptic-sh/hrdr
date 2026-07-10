use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

// ---- ls ----

pub struct LsTool;

#[derive(Deserialize)]
struct LsArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for LsTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "ls"
    }
    fn description(&self) -> &'static str {
        "List the entries of one directory (defaults to cwd). Directories get a trailing `/`, \
         symlinks a trailing `@`. Use `find` to search a whole tree by glob."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default: cwd)."}
            }
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: LsArgs = crate::tool_args("ls", args)?;
        let dir = ctx.resolve(a.path.as_deref().unwrap_or("."));
        let mut rd = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("listing {}", dir.display()))?;
        let mut entries: Vec<String> = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let suffix = match e.file_type().await {
                Ok(t) if t.is_dir() => "/",
                Ok(t) if t.is_symlink() => "@",
                _ => "",
            };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();
        if entries.is_empty() {
            return Ok("(empty directory)".to_string());
        }
        Ok(truncate(&entries.join("\n"), ctx.max_output))
    }
}
