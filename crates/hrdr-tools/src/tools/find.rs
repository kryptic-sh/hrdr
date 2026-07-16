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
    /// Also match hidden files/dirs (dotfiles). Skipped by default.
    #[serde(default)]
    hidden: bool,
    /// Also match .gitignore'd files. Skipped by default.
    #[serde(default)]
    no_ignore: bool,
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
         paths. Use `ls` to list one directory; use this to search a tree by name. By \
         default hidden files/dirs (dotfiles, e.g. `.github/`) and .gitignore'd paths \
         (e.g. `target/`, `node_modules/`) are skipped; set `hidden` and/or `no_ignore` \
         to include them."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. 'src/**/*.rs'."},
                "hidden": {"type": "boolean", "description": "Also match hidden files/dirs (dotfiles). Skipped by default (default false)."},
                "no_ignore": {"type": "boolean", "description": "Also match .gitignore'd files. Skipped by default (default false)."}
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
        // honoured by default and build artifacts (`target/`, `node_modules/`, …)
        // are skipped automatically — same as hidden `.git/` and other dotfiles.
        // Both are overridable via `hidden` / `no_ignore` — the same flags
        // `grep` exposes on its identical walker.
        let mut paths: Vec<String> = Vec::new();
        let walker = ignore::WalkBuilder::new(&ctx.cwd)
            .max_depth(Some(20))
            .hidden(!a.hidden)
            .ignore(!a.no_ignore)
            .git_ignore(!a.no_ignore)
            .git_global(!a.no_ignore)
            .git_exclude(!a.no_ignore)
            .parents(!a.no_ignore)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Hidden dotfiles are skipped by default and only located when
    /// `hidden: true` is set — the undocumented default this change
    /// documents and makes overridable.
    #[tokio::test]
    async fn dotfile_skipped_by_default_and_found_with_hidden_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "").unwrap();
        let ctx = ToolContext::new(dir.path());

        let out = FindTool
            .execute(serde_json::json!({"pattern": ".envrc"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "(no matches)", "{out}");

        let out = FindTool
            .execute(
                serde_json::json!({"pattern": ".envrc", "hidden": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains(".envrc"), "{out}");
    }

    /// `.gitignore`'d files are skipped by default and only located when
    /// `no_ignore: true` is set. Requires a `.git` dir in the fixture: the
    /// `ignore` crate only applies git-related ignore rules (including
    /// `.gitignore`) inside a discovered git repository by default.
    #[tokio::test]
    async fn gitignored_file_skipped_by_default_and_found_with_no_ignore_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        let ctx = ToolContext::new(dir.path());

        let out = FindTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "(no matches)", "{out}");

        let out = FindTool
            .execute(
                serde_json::json!({"pattern": "*.rs", "no_ignore": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("ignored.rs"), "{out}");
    }
}
