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
    /// The directory listed — a hostile *filename* is a real injection vector.
    fn output_source(&self, args: &serde_json::Value, unix_style_paths: bool) -> String {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = crate::display_path(std::path::Path::new(path), unix_style_paths);
        format!("listing of {path}")
    }

    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "ls"
    }
    fn description(&self) -> &'static str {
        "List the entries of one directory (defaults to cwd). Directories get a trailing `/`, \
         symlinks a trailing `@`. Unlike `find`/`tree`/`grep`, this lists everything in the \
         directory — dotfiles and `.gitignore`'d entries are not hidden. Use `find` to search \
         a whole tree by glob."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "default": ".", "description": "Directory to list (default: cwd)."}
            }
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: LsArgs = crate::tool_args("ls", args)?;
        let dir = ctx.resolve_read(a.path.as_deref().unwrap_or("."))?;
        let mut rd = tokio::fs::read_dir(&dir).await.with_context(|| {
            format!(
                "listing {}",
                crate::display_path(&dir, ctx.unix_style_paths)
            )
        })?;
        let mut entries: Vec<String> = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let suffix = match e.file_type().await {
                Ok(t) if t.is_dir() && cfg!(windows) && !ctx.unix_style_paths => "\\",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ls_allows_a_directory_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.txt"), "").unwrap();

        let ctx = ToolContext::new(cwd.path().to_path_buf());
        let out = LsTool
            .execute(
                serde_json::json!({"path": outside.path().to_str().unwrap()}),
                &ctx,
            )
            .await
            .expect("listing outside cwd is allowed");
        assert!(out.contains("a.txt"), "got: {out}");
    }

    /// …but not for a read-confined agent: `ls` reads the filesystem, so its
    /// path argument goes through the same guard as `read`.
    #[tokio::test]
    async fn ls_outside_roots_is_refused_in_strict_mode() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ctx = crate::sandbox::confined_ctx(cwd.path(), crate::SandboxMode::Jail);

        let err = LsTool
            .execute(
                serde_json::json!({"path": outside.path().to_str().unwrap()}),
                &ctx,
            )
            .await
            .expect_err("listing outside the readable roots must be refused")
            .to_string();
        assert!(err.contains("sandbox: refusing to read"), "{err}");
    }
}
