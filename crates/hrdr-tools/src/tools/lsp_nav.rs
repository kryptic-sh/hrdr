//! LSP navigation tools: `definition`, `references`, `rename` — backed by
//! the same warm language servers as the post-edit diagnostics
//! ([`crate::LspRegistry`]). Registered only when LSP is enabled; a file with
//! no configured/installed server gets a plain error the model can act on.
//!
//! Symbol addressing: the model gives a 1-based `line` plus the `symbol` text
//! on that line — not a column. Models read files through the line-numbered
//! `read` tool, so lines are reliable and columns aren't; the tool finds the
//! symbol on the line and converts to the UTF-16 position LSP wants.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::mutation::apply_file_change;
use crate::{Tool, ToolContext, truncate};

/// Most locations listed before "…and N more" (references in a big codebase
/// can be thousands).
const MAX_LOCATIONS: usize = 50;

#[derive(Deserialize)]
struct NavArgs {
    path: String,
    line: u32,
    #[serde(default)]
    symbol: Option<String>,
}

#[derive(Deserialize)]
struct RenameArgs {
    path: String,
    line: u32,
    symbol: String,
    new_name: String,
}

/// Resolve (path, line, symbol) to the request inputs: the file's current
/// content and the 0-based UTF-16 position of the symbol on that line.
///
/// Deliberately does **not** call [`ToolContext::ensure_writable_ext`] — this
/// is shared by the read-only `definition`/`references` tools too, and that
/// check exists to scope a sub-agent's *writes* (e.g. `write_allow_ext =
/// ["md"]`). Applying it here rejected a plain navigation of a `.rs` file
/// with a misleading "may only modify .md files", even though nothing was
/// being modified. `RenameTool`, which does write, checks it itself instead.
async fn locate(
    ctx: &ToolContext,
    path: &str,
    line: u32,
    symbol: Option<&str>,
) -> Result<(std::path::PathBuf, String, u32, u32)> {
    let path = ctx.resolve(path);
    let content = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    if line == 0 {
        bail!("line is 1-based");
    }
    let line0 = line - 1;
    let text = content
        .lines()
        .nth(line0 as usize)
        .with_context(|| format!("{} has no line {line}", path.display()))?;
    let character = match symbol {
        Some(sym) if !sym.is_empty() => {
            let byte = text.find(sym).with_context(|| {
                format!(
                    "`{sym}` does not appear on line {line} of {} (that line is: {})",
                    path.display(),
                    text.trim()
                )
            })?;
            crate::lsp::byte_to_utf16_col(text, byte)
        }
        _ => 0,
    };
    Ok((path, content, line0, character))
}

/// Format resolved locations as `path:line:col` lines, root-relative, capped.
fn format_locations(ctx: &ToolContext, locations: &[crate::LspLocation]) -> String {
    let mut lines: Vec<String> = locations
        .iter()
        .take(MAX_LOCATIONS)
        .map(|l| {
            let rel = l.path.strip_prefix(&ctx.cwd).unwrap_or(&l.path);
            format!("{}:{}:{}", rel.display(), l.line, l.column)
        })
        .collect();
    if locations.len() > MAX_LOCATIONS {
        lines.push(format!("…and {} more", locations.len() - MAX_LOCATIONS));
    }
    lines.join("\n")
}

fn nav_params_schema(symbol_desc: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File containing the symbol."},
            "line": {"type": "integer", "description": "1-based line number (as shown by `read`)."},
            "symbol": {"type": "string", "description": symbol_desc}
        },
        "required": ["path", "line", "symbol"]
    })
}

fn registry(ctx: &ToolContext) -> Result<&crate::LspRegistry> {
    ctx.lsp
        .as_deref()
        .context("LSP support is disabled (`[lsp] enabled = false`)")
}

pub struct DefinitionTool;

#[async_trait]
impl Tool for DefinitionTool {
    fn name(&self) -> &'static str {
        "definition"
    }
    fn description(&self) -> &'static str {
        "Jump to a symbol's definition via the language server: give the file, the 1-based \
         line, and the symbol text on that line. Returns the definition site(s) as \
         `path:line:col`. More precise than grep for \"where is this defined\". Requires a \
         language server for the file type (see /doctor)."
    }
    fn parameters(&self) -> serde_json::Value {
        nav_params_schema("The symbol text on that line to resolve (e.g. `parse_config`).")
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: NavArgs = crate::tool_args("definition", args)?;
        let (path, content, line0, character) =
            locate(ctx, &a.path, a.line, a.symbol.as_deref()).await?;
        let result = registry(ctx)?
            .nav_request(
                "textDocument/definition",
                "definitionProvider",
                &path,
                &content,
                (line0, character),
                json!({}),
            )
            .await?;
        let locations = crate::lsp::parse_locations(&result);
        if locations.is_empty() {
            bail!(
                "no definition found for `{}` at {}:{} (the server may still be indexing — try again)",
                a.symbol.as_deref().unwrap_or("?"),
                a.path,
                a.line
            );
        }
        Ok(truncate(&format_locations(ctx, &locations), ctx.max_output))
    }
}

pub struct ReferencesTool;

#[async_trait]
impl Tool for ReferencesTool {
    fn name(&self) -> &'static str {
        "references"
    }
    fn description(&self) -> &'static str {
        "List every reference to a symbol via the language server (declaration included): give \
         the file, the 1-based line, and the symbol text on that line. Returns use sites as \
         `path:line:col` — precise impact analysis before changing a symbol. Requires a \
         language server for the file type (see /doctor)."
    }
    fn parameters(&self) -> serde_json::Value {
        nav_params_schema("The symbol text on that line to look up (e.g. `parse_config`).")
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: NavArgs = crate::tool_args("references", args)?;
        let (path, content, line0, character) =
            locate(ctx, &a.path, a.line, a.symbol.as_deref()).await?;
        let result = registry(ctx)?
            .nav_request(
                "textDocument/references",
                "referencesProvider",
                &path,
                &content,
                (line0, character),
                json!({"context": {"includeDeclaration": true}}),
            )
            .await?;
        let locations = crate::lsp::parse_locations(&result);
        if locations.is_empty() {
            bail!(
                "no references found for `{}` at {}:{} (the server may still be indexing — try again)",
                a.symbol.as_deref().unwrap_or("?"),
                a.path,
                a.line
            );
        }
        Ok(format!(
            "{} reference(s):\n{}",
            locations.len(),
            truncate(&format_locations(ctx, &locations), ctx.max_output)
        ))
    }
}

pub struct RenameTool;

#[async_trait]
impl Tool for RenameTool {
    fn name(&self) -> &'static str {
        "rename"
    }
    fn description(&self) -> &'static str {
        "Rename a symbol across the workspace via the language server: give the file, the \
         1-based line, the current symbol text on that line, and the new name. The server \
         computes every edit; hrdr applies them through the normal write path. Prefer this \
         over the textual `replace` tool for renaming a code symbol — it's scope-aware, so \
         it won't also rewrite comments, strings, or substrings of unrelated names. \
         Requires a language server for the file type."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File containing the symbol."},
                "line": {"type": "integer", "description": "1-based line number (as shown by `read`)."},
                "symbol": {"type": "string", "description": "The current symbol text on that line."},
                "new_name": {"type": "string", "description": "The new name."}
            },
            "required": ["path", "line", "symbol", "new_name"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: RenameArgs = crate::tool_args("rename", args)?;
        if a.new_name.trim().is_empty() {
            bail!("new_name must not be empty");
        }
        let (path, content, line0, character) =
            locate(ctx, &a.path, a.line, Some(&a.symbol)).await?;
        // Unlike `definition`/`references`, `rename` writes — so, unlike
        // `locate` (shared with those read-only tools), it must honour a
        // write-scoped sub-agent's allowed extensions for the file the
        // symbol was found in, not just the (already separately checked)
        // files the server's edit ends up touching.
        ctx.ensure_writable_ext(&path)?;
        let result = registry(ctx)?
            .nav_request(
                "textDocument/rename",
                "renameProvider",
                &path,
                &content,
                (line0, character),
                json!({"newName": a.new_name}),
            )
            .await?;
        let files = crate::lsp::parse_workspace_edit(&result)?;
        if files.is_empty() {
            bail!(
                "the server returned no edits for renaming `{}` at {}:{}",
                a.symbol,
                a.path,
                a.line
            );
        }
        // Validate everything before writing anything (atomic, like `patch`):
        // confinement plus a clean application of every file's edits.
        let mut planned = Vec::with_capacity(files.len());
        for file in &files {
            ctx.ensure_writable_ext(&file.path)?;
            let before = tokio::fs::read_to_string(&file.path)
                .await
                .with_context(|| format!("reading {}", file.path.display()))?;
            let after = crate::lsp::apply_lsp_edits(&before, &file.edits)
                .with_context(|| format!("applying rename edits to {}", file.path.display()))?;
            planned.push((file.path.clone(), file.edits.len(), after));
        }
        // The edits are server-computed against on-disk content, so the
        // read-before-edit gate doesn't apply; every touched file is marked
        // read afterwards (the model has effectively seen the change).
        let mut summary = Vec::with_capacity(planned.len());
        for (path, edit_count, after) in planned {
            let fc = apply_file_change(ctx, &path, "rename", &after).await?;
            ctx.mark_read(&path);
            let rel = path.strip_prefix(&ctx.cwd).unwrap_or(&path).display();
            let mut line = format!("{rel} ({edit_count} edit(s))");
            if !fc.notes.is_empty() {
                line.push_str(&format!("  [{}]", fc.notes.join("; ")));
            }
            summary.push(line);
        }
        Ok(truncate(
            &format!(
                "Renamed `{}` → `{}` in {} file(s):\n{}",
                a.symbol,
                a.new_name,
                summary.len(),
                summary.join("\n")
            ),
            ctx.max_output,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (MINOR): `locate` is shared by the read-only
    /// `definition`/`references` tools and the writing `rename` tool. Before
    /// the fix it ran `ctx.ensure_writable_ext` unconditionally, so a
    /// sub-agent scoped to e.g. `write_allow_ext = ["md"]` got "may only
    /// modify .md files" when merely navigating a `.rs` file — even though
    /// nothing was being modified. With LSP left disabled (the
    /// `ToolContext::new` default), the call now fails for the *next* reason
    /// in the pipeline instead — "LSP support is disabled" — proving the
    /// write-ext gate no longer runs first for a read-only nav tool.
    #[tokio::test]
    async fn definition_is_not_blocked_by_write_allow_ext() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.write_allow_ext = Some(vec!["md".to_string()]);

        let err = DefinitionTool
            .execute(json!({"path": "a.rs", "line": 1, "symbol": "main"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("may only modify"),
            "a read-only nav tool must not be blocked by write_allow_ext: {err}"
        );
        assert!(err.contains("LSP support is disabled"), "{err}");
    }

    /// The same scenario for `references` — also read-only, also shares
    /// `locate`.
    #[tokio::test]
    async fn references_is_not_blocked_by_write_allow_ext() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.write_allow_ext = Some(vec!["md".to_string()]);

        let err = ReferencesTool
            .execute(json!({"path": "a.rs", "line": 1, "symbol": "main"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("may only modify"),
            "a read-only nav tool must not be blocked by write_allow_ext: {err}"
        );
        assert!(err.contains("LSP support is disabled"), "{err}");
    }

    /// `rename` **does** write, so — unlike `definition`/`references` — it
    /// must still honour `write_allow_ext` for the file the symbol was found
    /// in; that check just moved from the shared `locate` into `RenameTool`
    /// itself.
    #[tokio::test]
    async fn rename_is_still_blocked_by_write_allow_ext() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.write_allow_ext = Some(vec!["md".to_string()]);

        let err = RenameTool
            .execute(
                json!({"path": "a.rs", "line": 1, "symbol": "main", "new_name": "run"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("may only modify"), "{err}");
    }
}
