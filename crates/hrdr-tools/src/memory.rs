//! The `memory` tool — durable, agent-written notes that persist across
//! sessions, in two scopes: **project** (this working directory) and **global**
//! (all projects). Storage roots are supplied by the caller via
//! [`ToolContext::memory_project`] / [`ToolContext::memory_global`]; the format
//! is plain Markdown (an `MEMORY.md` index plus topic files), OKF-flavored —
//! greppable, git-diffable, and fail-open.
//!
//! Reads are unrestricted elsewhere, so the agent can `read`/`grep` memory files
//! directly by path; this tool exists for the **writes** (which are otherwise
//! confined to the working directory) plus a convenience `view`/list.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, truncate_saved};

pub struct MemoryTool;

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Durable memory that persists across sessions. Save facts worth keeping — project \
         conventions, decisions and their rationale, the user's stable preferences, where \
         things live, gotchas — so you don't re-derive them next time. Two scopes: `project` \
         (this repo, default) and `global` (all projects, e.g. personal preferences). Keep \
         a short index — `MEMORY.md` (or `index.md`) — and put detail in topic files (e.g. \
         `auth.md`); the index is loaded into your context at session start. Actions: `view` \
         (no path = list the scope; \
         with path = read a file), `write` (create/overwrite), `append` (add to a file, \
         creating it if needed), `delete`. Save only durable, reusable facts — not transient \
         task state. Prune entries that become wrong."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["view", "write", "append", "delete"],
                    "description": "view (list scope, or read a file with `path`), write, append, or delete."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "Which store — `project` (this repo, default) or `global` (all projects)."
                },
                "path": {
                    "type": "string",
                    "description": "Relative file within the scope (e.g. `MEMORY.md`, `auth.md`). Omit with `view` to list."
                },
                "content": {
                    "type": "string",
                    "description": "Text to write or append."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let a: MemoryArgs = crate::tool_args("memory", args)?;
        let scope = a.scope.as_deref().unwrap_or("project");
        let root = match scope {
            "project" => ctx.memory_project.as_ref(),
            "global" => ctx.memory_global.as_ref(),
            other => bail!("unknown memory scope '{other}' (use `project` or `global`)"),
        }
        .ok_or_else(|| {
            anyhow::anyhow!("memory is disabled (no storage directory) — enable it in config")
        })?;

        match a.action.as_str() {
            "view" => match a.path.as_deref().filter(|p| !p.trim().is_empty()) {
                None => Ok(list_scope(scope, root)),
                Some(rel) => {
                    let file = resolve(root, rel)?;
                    let text = std::fs::read_to_string(&file)
                        .map_err(|e| anyhow::anyhow!("reading {scope} memory '{rel}': {e}"))?;
                    Ok(truncate_saved(
                        &text,
                        ctx.max_output,
                        ctx.max_output_lines,
                        crate::TruncateSide::Head,
                        "memory",
                    ))
                }
            },
            "write" => {
                let rel = require_path(&a.path)?;
                let content = a.content.unwrap_or_default();
                let file = resolve(root, rel)?;
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, &content)?;
                Ok(format!(
                    "saved {} bytes to {scope} memory: {rel}",
                    content.len()
                ))
            }
            "append" => {
                let rel = require_path(&a.path)?;
                let content = a
                    .content
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("`append` needs `content`"))?;
                let file = resolve(root, rel)?;
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let existing = std::fs::read_to_string(&file).unwrap_or_default();
                let mut out = existing;
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&content);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                std::fs::write(&file, &out)?;
                Ok(format!("appended to {scope} memory: {rel}"))
            }
            "delete" => {
                let rel = require_path(&a.path)?;
                let file = resolve(root, rel)?;
                std::fs::remove_file(&file)
                    .map_err(|e| anyhow::anyhow!("deleting {scope} memory '{rel}': {e}"))?;
                Ok(format!("deleted {scope} memory: {rel}"))
            }
            other => bail!("unknown memory action '{other}'"),
        }
    }
}

fn require_path(path: &Option<String>) -> Result<&str> {
    path.as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow::anyhow!("this action needs a `path`"))
}

/// Resolve `rel` under `root`, rejecting anything that isn't a plain relative
/// path (no `..`, no leading `/`) so a write can't escape the memory store.
fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    for c in p.components() {
        if !matches!(c, Component::Normal(_)) {
            bail!("memory path must be a simple relative path (no '..' or leading '/'): {rel}");
        }
    }
    Ok(root.join(p))
}

/// A listing of the scope's Markdown files (name + size), MEMORY.md first.
fn list_scope(scope: &str, root: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(root) else {
        return format!("(no {scope} memory yet — save some with `memory` write/append)");
    };
    let mut files: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            Some((name, size))
        })
        .collect();
    if files.is_empty() {
        return format!("(no {scope} memory yet — save some with `memory` write/append)");
    }
    // The index (MEMORY.md / index.md) first, then alphabetical.
    files.sort_by(|a, b| {
        let key = |n: &str| (!matches!(n, "MEMORY.md" | "index.md"), n.to_string());
        key(&a.0).cmp(&key(&b.0))
    });
    let mut out = format!("{scope} memory ({}):\n", root.display());
    for (name, size) in files {
        out.push_str(&format!("- {name} ({size} bytes)\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_memory(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::new(dir);
        ctx.memory_project = Some(dir.join("project"));
        ctx.memory_global = Some(dir.join("global"));
        ctx
    }

    #[tokio::test]
    async fn write_view_append_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        // Empty scope lists nothing.
        let listed = tool.execute(json!({"action": "view"}), &ctx).await.unwrap();
        assert!(listed.contains("no project memory"), "{listed}");

        // Write, then read it back.
        tool.execute(
            json!({"action": "write", "path": "MEMORY.md", "content": "- prefers tabs"}),
            &ctx,
        )
        .await
        .unwrap();
        let got = tool
            .execute(json!({"action": "view", "path": "MEMORY.md"}), &ctx)
            .await
            .unwrap();
        assert!(got.contains("prefers tabs"));

        // Append adds a line.
        tool.execute(
            json!({"action": "append", "path": "MEMORY.md", "content": "- uses fish shell"}),
            &ctx,
        )
        .await
        .unwrap();
        let got = tool
            .execute(json!({"action": "view", "path": "MEMORY.md"}), &ctx)
            .await
            .unwrap();
        assert!(got.contains("prefers tabs") && got.contains("fish shell"));

        // Listing now shows the file.
        let listed = tool.execute(json!({"action": "view"}), &ctx).await.unwrap();
        assert!(listed.contains("MEMORY.md"));

        // Delete removes it.
        tool.execute(json!({"action": "delete", "path": "MEMORY.md"}), &ctx)
            .await
            .unwrap();
        assert!(
            tool.execute(json!({"action": "view", "path": "MEMORY.md"}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn global_and_project_scopes_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        tool.execute(
            json!({"action": "write", "scope": "global", "path": "MEMORY.md", "content": "g"}),
            &ctx,
        )
        .await
        .unwrap();
        // Project scope stays empty.
        let proj = tool
            .execute(json!({"action": "view", "scope": "project"}), &ctx)
            .await
            .unwrap();
        assert!(proj.contains("no project memory"), "{proj}");
        let glob = tool
            .execute(json!({"action": "view", "scope": "global"}), &ctx)
            .await
            .unwrap();
        assert!(glob.contains("MEMORY.md"));
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        for bad in ["../escape.md", "/etc/passwd", "sub/../../x.md"] {
            let r = tool
                .execute(
                    json!({"action": "write", "path": bad, "content": "x"}),
                    &ctx,
                )
                .await;
            assert!(r.is_err(), "traversal '{bad}' must be rejected");
        }
    }

    #[tokio::test]
    async fn disabled_when_no_root() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path()); // no memory dirs set
        let tool = MemoryTool;
        let r = tool.execute(json!({"action": "view"}), &ctx).await;
        assert!(r.is_err());
    }
}
