use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

// ---- glob ----

pub struct FindTool;

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
}

#[async_trait]
impl Tool for FindTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (supports `**`), relative to cwd. Returns matching \
         paths. Use `ls` to list one directory; use this to search a tree by name."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. 'src/**/*.rs'."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: FindArgs = crate::tool_args("find", args)?;
        // Compile the glob pattern once; validated eagerly for a clear error.
        let pat = glob::Pattern::new(&a.pattern)
            .with_context(|| format!("invalid glob pattern: {}", a.pattern))?;

        // Walk with `ignore::WalkBuilder` so `.gitignore` / `.ignore` rules are
        // honoured and build artifacts (`target/`, `node_modules/`, …) are skipped
        // automatically. Hidden `.git/` is also excluded by the default
        // `hidden(true)` flag — the same walker that `grep_builtin` uses.
        let mut paths: Vec<String> = Vec::new();
        let walker = ignore::WalkBuilder::new(&ctx.cwd)
            .max_depth(Some(20))
            .hidden(true)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&ctx.cwd).unwrap_or(path);
            // Match both the bare filename and the full relative path so that
            // simple patterns like `*.rs` work without a leading `**/`, while
            // patterns like `src/**/*.rs` still match across directory depth.
            let name = path.file_name().map(|n| n.to_string_lossy());
            let hit = name.as_deref().is_some_and(|n| pat.matches(n)) || pat.matches_path(rel);
            if hit {
                paths.push(rel.to_string_lossy().to_string());
            }
        }
        paths.sort();
        if paths.is_empty() {
            return Ok("(no matches)".to_string());
        }
        Ok(truncate(&paths.join("\n"), ctx.max_output))
    }
}
