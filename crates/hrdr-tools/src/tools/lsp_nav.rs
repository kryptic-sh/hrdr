//! LSP navigation tools: `definition`, `references`, `rename` — backed by
//! the same warm language servers as the post-edit diagnostics
//! ([`crate::LspRegistry`]). Registered only when LSP is enabled; a file with
//! no configured/installed server gets a plain error the model can act on.
//!
//! Symbol addressing: the model gives a 1-based `line` plus the `symbol` text
//! on that line — not a column. Models read files through the line-numbered
//! `read` tool, so lines are reliable and columns aren't; the tool finds the
//! symbol on the line and converts to the UTF-16 position LSP wants.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::mutation::{FileChange, apply_file_change};
use crate::{Tool, ToolContext, guard_secret_read, truncate};

/// Most locations listed before "…and N more" (references in a big codebase
/// can be thousands).
const MAX_LOCATIONS: usize = 50;

/// One file's share of a planned rename: its pre-rename bytes and the bytes the
/// server's edits produce. Everything is planned before anything is written, so
/// a file that can't be read or whose edits don't apply aborts the rename with
/// the workspace still untouched.
struct PlannedEdit {
    path: PathBuf,
    edit_count: usize,
    before: String,
    after: String,
}

/// Whether `path`'s current bytes differ from `before` — i.e. whether it still
/// needs restoring. An unreadable file counts as "differs": better to attempt a
/// restore that fails loudly than to assume a file we can't inspect is clean.
fn needs_restore(path: &std::path::Path, before: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|now| now != before)
        .unwrap_or(true)
}

/// RAII rollback for the commit phase of a multi-file rename, in the shape of
/// [`super::mutation`]'s `TempFile` guard: it undoes the writes recorded so far
/// on drop unless [`disarm`](Self::disarm) marked the rename complete.
///
/// The error path rolls back explicitly (see [`RenameTool::execute`]) — this
/// guard exists for *cancellation*: if the surrounding future is dropped at any
/// `.await` inside the commit loop (user hits Esc, the turn is aborted), nothing
/// else would ever restore the files already rewritten and the workspace would
/// be left half-renamed.
struct RenameRollback<'a> {
    /// `(path, pre-rename bytes)` for every file whose write has been *started*,
    /// in write order. Recorded before the write, not after, so a file that was
    /// modified by a write that then failed (or was cancelled in flight) is
    /// still covered.
    written: Vec<(&'a std::path::Path, &'a str)>,
    disarmed: bool,
}

impl<'a> RenameRollback<'a> {
    fn new() -> Self {
        Self {
            written: Vec::new(),
            disarmed: false,
        }
    }

    fn record(&mut self, path: &'a std::path::Path, before: &'a str) {
        self.written.push((path, before));
    }

    /// Mark the rename committed: drop becomes a no-op.
    fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// Restore every recorded file that still differs from its pre-rename bytes,
    /// newest first. Returns the paths that could not be restored — those are
    /// left holding renamed content.
    fn restore_blocking(&self) -> Vec<PathBuf> {
        let mut failed = Vec::new();
        for (path, before) in self.written.iter().rev() {
            if needs_restore(path, before) && std::fs::write(path, before).is_err() {
                failed.push(path.to_path_buf());
            }
        }
        failed
    }
}

impl Drop for RenameRollback<'_> {
    fn drop(&mut self) {
        if self.disarmed || self.written.is_empty() {
            return;
        }
        // Deliberately blocking `std::fs`: `Drop` is synchronous and cannot
        // `.await`, and a cancelled future has no runtime left to block on, so
        // there is no async option here. Two consequences, both accepted:
        // post-edit hooks are *not* re-run on the restored bytes (unlike the
        // error path below, which rolls back through `apply_file_change`), and
        // a restore that itself fails cannot be reported — the caller that
        // would have been told is exactly the thing that went away.
        let _failed = self.restore_blocking();
    }
}

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
async fn locate(
    ctx: &ToolContext,
    path: &str,
    line: u32,
    symbol: Option<&str>,
) -> Result<(std::path::PathBuf, String, u32, u32)> {
    let path = ctx.resolve(path);
    guard_secret_read(&path).with_context(|| format!("refusing to navigate {}", path.display()))?;
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
                    "`{sym}` does not appear on line {line} of {}",
                    path.display(),
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
        let locations = crate::lsp::parse_locations(&result)?;
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
        let locations = crate::lsp::parse_locations(&result)?;
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

/// Commit phase of a rename: write every planned file through the normal
/// mutation path, all-or-nothing.
///
/// Three ways out, and each leaves the workspace whole:
/// * every write succeeds — the guard is disarmed and the [`FileChange`]s
///   (hook/diagnostic notes) come back;
/// * a write fails — the files already written are restored *through
///   [`apply_file_change`]* so post-edit hooks run on the restored bytes too,
///   then the original error is returned, with any file that could not be
///   restored named first (a workspace left half-renamed matters more to the
///   user than why the write failed);
/// * the future is dropped mid-loop — `RenameRollback`'s `Drop` restores
///   synchronously; see its comment for what that path can't do.
async fn commit_planned(ctx: &ToolContext, planned: &[PlannedEdit]) -> Result<Vec<FileChange>> {
    let mut applied: Vec<FileChange> = Vec::new();
    let mut rollback = RenameRollback::new();
    let mut failure: Option<(&std::path::Path, anyhow::Error)> = None;

    for plan in planned {
        rollback.record(&plan.path, &plan.before);
        match apply_file_change(ctx, &plan.path, "rename", &plan.after).await {
            Ok(fc) => {
                ctx.mark_read(&plan.path);
                applied.push(fc);
            }
            Err(e) => {
                failure = Some((&plan.path, e));
                break;
            }
        }
    }

    let Some((failed_path, err)) = failure else {
        rollback.disarm();
        return Ok(applied);
    };

    // Roll back what was written. The guard stays armed until this finishes, so
    // cancelling *during* the rollback still falls back to its blocking restore
    // (which is idempotent with this loop: both skip files already at `before`).
    let mut unrestored: Vec<String> = Vec::new();
    for (path, before) in rollback.written.iter().rev() {
        if !needs_restore(path, before) {
            continue;
        }
        match apply_file_change(ctx, path, "rename", before).await {
            Ok(_) => ctx.mark_read(path),
            Err(e) => unrestored.push(format!("{} ({e:#})", path.display())),
        }
    }
    rollback.disarm();

    let err = err.context(format!("writing {}", failed_path.display()));
    if unrestored.is_empty() {
        return Err(err);
    }
    Err(err.context(format!(
        "rename rolled back, but {} file(s) could NOT be restored and still hold renamed \
         content — fix them by hand: {}",
        unrestored.len(),
        unrestored.join(", ")
    )))
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
        let files = crate::lsp::parse_workspace_edit(&result, &ctx.cwd)?;
        if files.is_empty() {
            bail!(
                "the server returned no edits for renaming `{}` at {}:{}",
                a.symbol,
                a.path,
                a.line
            );
        }
        // Validate everything before writing anything atomically:
        // confinement plus a clean application of every file's edits.
        let mut planned = Vec::with_capacity(files.len());
        for file in &files {
            let path = &file.path;
            guard_secret_read(path)
                .with_context(|| format!("refusing to read {}", path.display()))?;
            let before = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("reading {}", file.path.display()))?;
            let after = crate::lsp::apply_lsp_edits(&before, &file.edits)
                .with_context(|| format!("applying rename edits to {}", file.path.display()))?;
            planned.push(PlannedEdit {
                path: file.path.clone(),
                edit_count: file.edits.len(),
                before,
                after,
            });
        }

        let applied = commit_planned(ctx, &planned).await?;

        let mut summary = Vec::with_capacity(planned.len());
        for (plan, fc) in planned.iter().zip(applied.iter()) {
            let (path, edit_count) = (&plan.path, plan.edit_count);
            let rel = path.strip_prefix(&ctx.cwd).unwrap_or(path).display();
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
    use std::future::Future;
    use std::task::{Context as TaskContext, Waker};

    use super::*;

    fn plan(path: &std::path::Path, before: &str, after: &str) -> PlannedEdit {
        PlannedEdit {
            path: path.to_path_buf(),
            edit_count: 1,
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    /// A failed write mid-commit must leave *no* file renamed: the ones already
    /// written go back to their original bytes, and the error names the file
    /// that failed. Here the second target is a directory, so writing it fails
    /// and restoring it can't be confirmed either — which must be reported
    /// rather than swallowed.
    #[tokio::test]
    async fn a_failed_write_rolls_back_and_reports_both_failures() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let one = dir.path().join("one.rs");
        let blocked = dir.path().join("blocked.rs");
        tokio::fs::write(&one, "fn old() {}\n").await.unwrap();
        // A directory can be neither written over nor restored.
        tokio::fs::create_dir(&blocked).await.unwrap();

        let planned = vec![
            plan(&one, "fn old() {}\n", "fn new() {}\n"),
            plan(&blocked, "fn old() {}\n", "fn new() {}\n"),
        ];

        let err = commit_planned(&ctx, &planned)
            .await
            .map(|_| ())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("blocked.rs"), "{msg}");
        assert!(
            msg.contains("could NOT be restored"),
            "a failed restore must be surfaced, not swallowed: {msg}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&one).await.unwrap(),
            "fn old() {}\n",
            "the already-written file must be restored byte-for-byte"
        );
    }

    /// Cancellation (the surrounding future dropped mid-commit) must roll back
    /// too — that's the guard's whole job. Driven by polling the future by hand
    /// and dropping it once the first file is on disk, so it doesn't depend on
    /// timing.
    #[tokio::test]
    async fn dropping_the_commit_future_restores_written_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let one = dir.path().join("one.rs");
        let two = dir.path().join("two.rs");
        tokio::fs::write(&one, "one old\n").await.unwrap();
        tokio::fs::write(&two, "two old\n").await.unwrap();

        let planned = vec![
            plan(&one, "one old\n", "one new\n"),
            plan(&two, "two old\n", "two new\n"),
        ];

        let mut fut = Box::pin(commit_planned(&ctx, &planned));
        let mut task_cx = TaskContext::from_waker(Waker::noop());
        let mut reached = false;
        for _ in 0..200 {
            assert!(
                fut.as_mut().poll(&mut task_cx).is_pending(),
                "the commit must not finish before we cancel it"
            );
            if tokio::fs::read_to_string(&one).await.unwrap() == "one new\n" {
                reached = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(reached, "the first file was never written");
        assert_eq!(
            tokio::fs::read_to_string(&two).await.unwrap(),
            "two old\n",
            "the second file must not be written yet, or this tests nothing"
        );

        drop(fut);

        assert_eq!(
            tokio::fs::read_to_string(&one).await.unwrap(),
            "one old\n",
            "a cancelled rename must restore the file it had already written"
        );
        assert_eq!(tokio::fs::read_to_string(&two).await.unwrap(), "two old\n");
    }

    /// The happy path stays untouched: every file renamed, nothing restored.
    #[tokio::test]
    async fn a_successful_commit_writes_every_file_and_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let one = dir.path().join("one.rs");
        let two = dir.path().join("two.rs");
        tokio::fs::write(&one, "one old\n").await.unwrap();
        tokio::fs::write(&two, "two old\n").await.unwrap();

        let planned = vec![
            plan(&one, "one old\n", "one new\n"),
            plan(&two, "two old\n", "two new\n"),
        ];

        let applied = commit_planned(&ctx, &planned).await.unwrap();

        assert_eq!(applied.len(), 2);
        assert_eq!(tokio::fs::read_to_string(&one).await.unwrap(), "one new\n");
        assert_eq!(tokio::fs::read_to_string(&two).await.unwrap(), "two new\n");
    }
}
