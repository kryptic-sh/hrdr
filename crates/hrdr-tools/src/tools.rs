//! The seven MVP tools.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{TodoItem, Tool, ToolContext, truncate};

/// Hard cap on a rendered source line, so one minified file can't blow context.
const MAX_LINE: usize = 2_000;
const DEFAULT_READ_LIMIT: usize = 2_000;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

// ---- read_file ----

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
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Read a file from disk. Returns 1-based line-numbered content. Use `offset`/`limit` \
         to page through large files instead of reading the whole thing."
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
        let a: ReadArgs = serde_json::from_value(args).context("invalid read_file args")?;
        let path = ctx.resolve(&a.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let start = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let n = i + 1;
            let line = if line.len() > MAX_LINE {
                &line[..MAX_LINE]
            } else {
                line
            };
            out.push_str(&format!("{n:>6}\t{line}\n"));
        }
        if out.is_empty() {
            out.push_str("(file is empty or offset past end)");
        }
        Ok(truncate(&out, ctx.max_output))
    }
}

// ---- write_file ----

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Create a new file or overwrite an existing one with `content`. Parent directories \
         are created as needed. Prefer `edit` for changing part of an existing file."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "content": {"type": "string", "description": "Full file contents to write."}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: WriteArgs = serde_json::from_value(args).context("invalid write_file args")?;
        let path = ctx.resolve(&a.path);
        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
        let old = if existed {
            tokio::fs::read_to_string(&path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // Snapshot the pre-write state so the change can be reverted.
        ctx.checkpoint(&path);
        let bytes = a.content.len();
        tokio::fs::write(&path, &a.content)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        if existed {
            let diff = unified_diff(&path.display().to_string(), &old, &a.content);
            let body = if diff.is_empty() {
                "(no changes)".to_string()
            } else {
                diff
            };
            Ok(truncate(
                &format!("Wrote {bytes} bytes to {}\n{body}", path.display()),
                ctx.max_output,
            ))
        } else {
            Ok(format!(
                "Created {} ({} lines)",
                path.display(),
                a.content.lines().count()
            ))
        }
    }
}

/// A unified diff of `old` → `new` for `path`, or empty if unchanged.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

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
        "Replace an exact substring in a file. `old_string` must match uniquely unless \
         `replace_all` is set. This is the preferred, token-cheap way to mutate a file."
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
        let a: EditArgs = serde_json::from_value(args).context("invalid edit args")?;
        let path = ctx.resolve(&a.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let count = text.matches(&a.old_string).count();
        if count == 0 {
            bail!("old_string not found in {}", path.display());
        }
        if count > 1 && !a.replace_all {
            bail!(
                "old_string is not unique in {} ({count} matches) — add context or set replace_all",
                path.display()
            );
        }
        let updated = if a.replace_all {
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        // Snapshot the pre-edit state so the change can be reverted.
        ctx.checkpoint(&path);
        tokio::fs::write(&path, &updated)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        let diff = unified_diff(&path.display().to_string(), &text, &updated);
        Ok(truncate(
            &format!(
                "Replaced {count} occurrence(s) in {}\n{diff}",
                path.display()
            ),
            ctx.max_output,
        ))
    }
}

// ---- bash ----

pub struct BashTool;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command via `bash -c` in the working directory. Use for build, test, \
         git, and anything without a dedicated tool. Output is captured and length-bounded."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms (default 120000)."}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: BashArgs = serde_json::from_value(args).context("invalid bash args")?;
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&a.command).current_dir(&ctx.cwd);
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_BASH_TIMEOUT_MS));
        run_streamed_command(cmd, timeout, ctx).await
    }
}

/// Append a captured line (with newline) to `out` and stream it to the UI sink.
fn emit_line(out: &mut String, ctx: &ToolContext, line: String) {
    out.push_str(&line);
    out.push('\n');
    ctx.emit(format!("{line}\n"));
}

/// Spawn a configured command, streaming its stdout/stderr line-by-line to the
/// UI sink while accumulating the full (length-bounded) output. Shared by the
/// `bash` and `powershell` tools.
async fn run_streamed_command(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    ctx: &ToolContext,
) -> Result<String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning command")?;
    let stdout = child.stdout.take().context("capturing stdout")?;
    let stderr = child.stderr.take().context("capturing stderr")?;
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();

    // Read stdout + stderr concurrently, accumulating the full output while
    // streaming each line. Interleaving order isn't guaranteed — fine for a live
    // view.
    let mut out = String::new();
    let collect = async {
        let mut out_done = false;
        let mut err_done = false;
        loop {
            tokio::select! {
                r = out_lines.next_line(), if !out_done => match r? {
                    Some(line) => emit_line(&mut out, ctx, line),
                    None => out_done = true,
                },
                r = err_lines.next_line(), if !err_done => match r? {
                    Some(line) => emit_line(&mut out, ctx, line),
                    None => err_done = true,
                },
                else => break,
            }
        }
        let status = child.wait().await.context("waiting on command")?;
        anyhow::Ok(status)
    };
    let status = tokio::time::timeout(timeout, collect)
        .await
        .map_err(|_| anyhow!("command timed out after {}ms", timeout.as_millis()))??;

    if !status.success() {
        out.push_str(&format!("[exit status: {status}]\n"));
    }
    let out = out.trim_end();
    if out.is_empty() {
        return Ok("(no output)".to_string());
    }
    Ok(truncate(out, ctx.max_output))
}

/// The available shell tools for this machine (bash and/or PowerShell), only
/// including a tool when its interpreter is actually on `PATH`.
pub fn available_shell_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    let mut tools: Vec<std::sync::Arc<dyn Tool>> = Vec::new();
    if which::which("bash").is_ok() {
        tools.push(std::sync::Arc::new(BashTool));
    }
    if let Some(program) = detect_powershell() {
        tools.push(std::sync::Arc::new(PowerShellTool { program }));
    }
    tools
}

/// Locate a PowerShell interpreter: prefer `pwsh` (PowerShell 7+, cross-platform)
/// then `powershell` (Windows PowerShell). `None` if neither is on `PATH`.
fn detect_powershell() -> Option<String> {
    ["pwsh", "powershell"]
        .into_iter()
        .find(|p| which::which(p).is_ok())
        .map(str::to_string)
}

// ---- powershell ----

pub struct PowerShellTool {
    program: String,
}

#[derive(Deserialize)]
struct PowerShellArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &'static str {
        "powershell"
    }
    fn description(&self) -> &'static str {
        "Run a command via PowerShell (`pwsh`/`powershell`) in the working \
         directory. Use for build, test, and anything without a dedicated tool, \
         especially on Windows. Output is captured and length-bounded."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "PowerShell command to run."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms (default 120000)."}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: PowerShellArgs = serde_json::from_value(args).context("invalid powershell args")?;
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &a.command])
            .current_dir(&ctx.cwd);
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_BASH_TIMEOUT_MS));
        run_streamed_command(cmd, timeout, ctx).await
    }
}

// ---- grep ----

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with ripgrep. Returns `path:line:match`. Optionally scope to a \
         `path` and/or filter files with a `glob` (e.g. '*.rs')."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default cwd)."},
                "glob": {"type": "string", "description": "Glob to filter files, e.g. '*.rs'."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GrepArgs = serde_json::from_value(args).context("invalid grep args")?;
        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .current_dir(&ctx.cwd);
        if let Some(g) = &a.glob {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg("--").arg(&a.pattern);
        if let Some(p) = &a.path {
            cmd.arg(p);
        }
        let output = cmd
            .output()
            .await
            .context("running ripgrep (is `rg` installed?)")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.is_empty() {
            // rg exits 1 with no output when there are no matches.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                bail!("ripgrep: {}", stderr.trim());
            }
            return Ok("(no matches)".to_string());
        }
        Ok(truncate(&stdout, ctx.max_output))
    }
}

// ---- glob ----

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (supports `**`), relative to cwd. Returns matching paths."
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
        let a: GlobArgs = serde_json::from_value(args).context("invalid glob args")?;
        let joined = ctx.cwd.join(&a.pattern);
        let pat = joined.to_string_lossy().to_string();
        let mut paths: Vec<String> = glob::glob(&pat)
            .with_context(|| format!("invalid glob pattern: {pat}"))?
            .filter_map(|r| r.ok())
            .map(|p| {
                p.strip_prefix(&ctx.cwd)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Ok("(no matches)".to_string());
        }
        Ok(truncate(&paths.join("\n"), ctx.max_output))
    }
}

// ---- todo_write ----

pub struct TodoTool;

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<TodoItem>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo_write"
    }
    fn description(&self) -> &'static str {
        "Replace the task list for the current work. Use it to plan and track multi-step \
         coding tasks: mark exactly one item `in_progress`, the rest `pending`/`completed`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: TodoArgs = serde_json::from_value(args).context("invalid todo_write args")?;
        let rendered = render_todos(&a.todos);
        if let Ok(mut todos) = ctx.todos.lock() {
            *todos = a.todos;
        }
        Ok(rendered)
    }
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(todo list cleared)".to_string();
    }
    let mut out = String::from("Updated task list:\n");
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "x",
            "in_progress" => "~",
            _ => " ",
        };
        out.push_str(&format!("[{mark}] {}\n", t.content));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn ctx(cwd: PathBuf) -> ToolContext {
        ToolContext::new(cwd)
    }

    // ---- read_file ----

    #[tokio::test]
    async fn read_file_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = ReadTool
            .execute(serde_json::json!({"path": path.to_str().unwrap()}), &c)
            .await
            .unwrap();
        assert!(out.contains("     1\talpha"), "line 1 not found: {out}");
        assert!(out.contains("     2\tbeta"), "line 2 not found: {out}");
        assert!(out.contains("     3\tgamma"), "line 3 not found: {out}");
    }

    #[tokio::test]
    async fn read_file_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "1\n2\n3\n4\n5\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = ReadTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.contains("     1\t"), "line 1 should be skipped");
        assert!(out.contains("     2\t2"), "line 2 missing: {out}");
        assert!(out.contains("     3\t3"), "line 3 missing: {out}");
        assert!(!out.contains("     4\t"), "line 4 should be skipped");
    }

    // ---- write_file ----

    #[tokio::test]
    async fn write_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let c = ctx(dir.path().to_path_buf());
        WriteTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "content": "hello world"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt");
        let c = ctx(dir.path().to_path_buf());
        WriteTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "content": "nested"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");
    }

    // ---- edit ----

    #[tokio::test]
    async fn edit_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "foo bar baz").unwrap();
        let c = ctx(dir.path().to_path_buf());
        EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "bar", "new_string": "qux"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo qux baz");
    }

    #[tokio::test]
    async fn edit_result_includes_unified_diff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "line one\nline two\nline three\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "two", "new_string": "TWO"}),
                &c,
            )
            .await
            .unwrap();
        assert!(
            out.contains("-line two") && out.contains("+line TWO"),
            "expected diff lines, got: {out}"
        );
    }

    #[tokio::test]
    async fn edit_not_found_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "foo bar baz").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let result = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "zzz", "new_string": "x"}),
                &c,
            )
            .await;
        assert!(result.is_err(), "expected error for not-found old_string");
    }

    #[tokio::test]
    async fn edit_non_unique_without_replace_all_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "aa bb aa").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let result = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "aa", "new_string": "cc"}),
                &c,
            )
            .await;
        assert!(result.is_err(), "expected error for non-unique match");
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "aa bb aa").unwrap();
        let c = ctx(dir.path().to_path_buf());
        EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "aa", "new_string": "cc", "replace_all": true}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "cc bb cc");
    }

    // ---- glob ----

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = GlobTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &c)
            .await
            .unwrap();
        assert!(out.contains("a.rs"), "a.rs missing: {out}");
        assert!(out.contains("b.rs"), "b.rs missing: {out}");
        assert!(!out.contains("c.txt"), "c.txt should not appear: {out}");
    }

    #[tokio::test]
    async fn glob_no_matches_returns_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = GlobTool
            .execute(serde_json::json!({"pattern": "*.nonexistent"}), &c)
            .await
            .unwrap();
        assert_eq!(out, "(no matches)");
    }

    // ---- todo_write ----

    #[tokio::test]
    async fn todo_write_render_marks() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = TodoTool
            .execute(
                serde_json::json!({
                    "todos": [
                        {"content": "pending task",  "status": "pending"},
                        {"content": "active task",   "status": "in_progress"},
                        {"content": "done task",     "status": "completed"}
                    ]
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("[ ] pending task"), "pending: {out}");
        assert!(out.contains("[~] active task"), "in_progress: {out}");
        assert!(out.contains("[x] done task"), "completed: {out}");
    }

    // ---- bash ---- (unix-only: these spawn a real `bash` shell)

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_echo_captures_output() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = BashTool
            .execute(serde_json::json!({"command": "echo hello_hrdr"}), &c)
            .await
            .unwrap();
        assert!(out.contains("hello_hrdr"), "echo output missing: {out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_exit_nonzero_includes_status() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = BashTool
            .execute(serde_json::json!({"command": "exit 42"}), &c)
            .await
            .unwrap();
        assert!(out.contains("exit status"), "status marker missing: {out}");
    }
}
