//! The seven MVP tools.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{TodoItem, Tool, ToolContext, cap_matches, truncate, truncate_middle};

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
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Read a file from disk. Returns 1-based line-numbered content (the `N\\t` prefix is \
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
        let a: ReadArgs = serde_json::from_value(args).context("invalid read_file args")?;
        let path = ctx.resolve(&a.path);
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
        "Create a new file or fully rewrite an existing one with `content`. Parent \
         directories are created as needed. Overwriting an existing file requires reading \
         it first. Prefer `edit` for changing part of an existing file."
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
        ctx.ensure_within_cwd(&path)?;
        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
        if existed && !ctx.was_read(&path) {
            bail!(
                "{} exists but you haven't read it — call read_file first so the rewrite \
                 starts from its real content (or use edit for a partial change)",
                path.display()
            );
        }
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
        // Post-edit hooks (formatters); the diff below is taken against the
        // post-hook content so the model's view matches the disk.
        let notes = crate::run_file_hooks(&ctx.hooks, "write_file", &path, &ctx.cwd).await;
        let finall = if notes.is_empty() && ctx.hooks.is_empty() {
            a.content.clone()
        } else {
            tokio::fs::read_to_string(&path)
                .await
                .unwrap_or_else(|_| a.content.clone())
        };
        ctx.mark_read(&path); // the model authored (or just saw) this content
        let mut warn = notes.join("\n");
        if !warn.is_empty() {
            warn.insert(0, '\n');
        }
        if existed {
            let diff = unified_diff(&path.display().to_string(), &old, &finall);
            let body = if diff.is_empty() {
                "(no changes)".to_string()
            } else {
                diff
            };
            Ok(truncate(
                &format!("Wrote {bytes} bytes to {}{warn}\n{body}", path.display()),
                ctx.max_output,
            ))
        } else {
            Ok(format!(
                "Created {} ({} lines){warn}",
                path.display(),
                finall.lines().count()
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
        "Replace an exact substring in a file (the preferred, token-cheap way to change \
         it). Copy `old_string` exactly from read_file output — same whitespace, line-number \
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
        let a: EditArgs = serde_json::from_value(args).context("invalid edit args")?;
        let path = ctx.resolve(&a.path);
        ctx.ensure_within_cwd(&path)?;
        if !ctx.was_read(&path) {
            bail!(
                "you haven't read {} yet — call read_file first, then copy old_string \
                 exactly from its output",
                path.display()
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
                     whitespace/indentation exists — copy the exact text from read_file \
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
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        // Snapshot the pre-edit state so the change can be reverted.
        ctx.checkpoint(&path);
        tokio::fs::write(&path, &updated)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        // Post-edit hooks (formatters); diff against the post-hook content so
        // the model's next old_string matches what's really on disk.
        let notes = crate::run_file_hooks(&ctx.hooks, "edit", &path, &ctx.cwd).await;
        let finall = if ctx.hooks.is_empty() {
            updated
        } else {
            tokio::fs::read_to_string(&path).await.unwrap_or(updated)
        };
        let mut warn = notes.join("\n");
        if !warn.is_empty() {
            warn.insert(0, '\n');
        }
        let diff = unified_diff(&path.display().to_string(), &text, &finall);
        Ok(truncate(
            &format!(
                "Replaced {count} occurrence(s) in {}{warn}\n{diff}",
                path.display()
            ),
            ctx.max_output,
        ))
    }
}

// ---- bash ----

pub struct BashTool;

/// Arguments shared by the shell tools (`bash`, `powershell`).
#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// The JSON-Schema shared by the shell tools; only the command description
/// differs.
fn shell_parameters(command_desc: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": command_desc},
            "timeout_ms": {"type": "integer", "description": "Timeout in ms (default 120000)."}
        },
        "required": ["command"]
    })
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command via `bash -c` in the working directory. Use for build, test, \
         git, and anything without a dedicated tool. Output is captured and length-bounded. \
         Each call starts fresh in the working directory — `cd` does NOT persist between \
         calls; chain it in one command (`cd sub && …`) or use paths from the cwd. \
         Git: stage explicit paths (`git add <file> …`); blanket staging, force-push, \
         hook-skipping, and destructive commands are rejected."
    }
    fn parameters(&self) -> serde_json::Value {
        shell_parameters("Shell command to run.")
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ShellArgs = serde_json::from_value(args).context("invalid bash args")?;
        if let Some(msg) = crate::check_guardrails(&a.command, &ctx.guardrails) {
            bail!("command blocked: {msg}");
        }
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
    // If this future is cancelled (turn interrupt) the process must not linger.
    cmd.kill_on_drop(true);
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
    let timed = tokio::time::timeout(timeout, collect).await;
    let status = match timed {
        Ok(status) => Some(status?),
        Err(_) => {
            // Timed out: kill the process (it would otherwise keep running
            // orphaned) and hand the model whatever it printed so far.
            let _ = child.kill().await;
            out.push_str(&format!(
                "[command timed out after {}ms; process killed — raise timeout_ms or run a \
                 narrower command]\n",
                timeout.as_millis()
            ));
            None
        }
    };
    if let Some(status) = status
        && !status.success()
    {
        out.push_str(&format!("[exit status: {status}]\n"));
    }
    let out = out.trim_end();
    if out.is_empty() {
        return Ok("(no output)".to_string());
    }
    // Head+tail truncation: build/test output puts the failures at the end —
    // plain head-only truncation would cut exactly what the model needs.
    Ok(truncate_middle(out, ctx.max_output))
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
        shell_parameters("PowerShell command to run.")
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ShellArgs = serde_json::from_value(args).context("invalid powershell args")?;
        if let Some(msg) = crate::check_guardrails(&a.command, &ctx.guardrails) {
            bail!("command blocked: {msg}");
        }
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &a.command])
            .current_dir(&ctx.cwd);
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_BASH_TIMEOUT_MS));
        run_streamed_command(cmd, timeout, ctx).await
    }
}

// ---- grep ----

/// Search backend, chosen once by availability.
#[derive(Clone, Copy)]
enum GrepBackend {
    Rg,
    Grep,
    Builtin,
}

pub struct GrepTool {
    backend: GrepBackend,
}

impl GrepTool {
    /// Pick a search backend: ripgrep, then POSIX `grep`, then a built-in walker
    /// (so search works even on a machine with neither installed).
    pub fn detect() -> Self {
        let backend = if which::which("rg").is_ok() {
            GrepBackend::Rg
        } else if which::which("grep").is_ok() {
            GrepBackend::Grep
        } else {
            GrepBackend::Builtin
        };
        Self { backend }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    context: Option<usize>,
}

impl GrepArgs {
    /// Context lines per match side, clamped to something sane.
    fn context(&self) -> usize {
        self.context.unwrap_or(0).min(GREP_MAX_CONTEXT)
    }

    /// Match cap: with context each match is ~2·n+1 lines, so the budget
    /// shrinks accordingly.
    fn max_matches(&self) -> usize {
        if self.context() == 0 {
            GREP_MAX_MATCHES
        } else {
            GREP_MAX_MATCHES_WITH_CONTEXT
        }
    }
}

/// Max matches a single grep call returns; beyond this the result ends with a
/// "narrow the pattern" nudge instead of flooding the context.
const GREP_MAX_MATCHES: usize = 200;
/// Lower cap when `context` is requested (each match is a whole window).
const GREP_MAX_MATCHES_WITH_CONTEXT: usize = 50;
/// Upper bound on `context` lines per side.
const GREP_MAX_CONTEXT: usize = 10;

#[async_trait]
impl Tool for GrepTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents (via ripgrep, grep, or a built-in walker — whichever is available). \
         Returns `path:line:match`. Optionally scope to a `path` and/or filter files with a \
         `glob` (e.g. '*.rs'). Set `context` to 2–3 to see the lines around each match \
         instead of making a follow-up read_file call."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default cwd)."},
                "glob": {"type": "string", "description": "Glob to filter files, e.g. '*.rs'."},
                "context": {"type": "integer", "description": "Lines of context around each match (0-10, default 0)."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GrepArgs = serde_json::from_value(args).context("invalid grep args")?;
        match self.backend {
            GrepBackend::Rg => grep_ripgrep(&a, ctx).await,
            GrepBackend::Grep => grep_posix(&a, ctx).await,
            GrepBackend::Builtin => grep_builtin(&a, ctx),
        }
    }
}

async fn grep_ripgrep(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .current_dir(&ctx.cwd);
    if a.context() > 0 {
        cmd.arg("-C").arg(a.context().to_string());
    }
    if let Some(g) = &a.glob {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(&a.pattern);
    if let Some(p) = &a.path {
        cmd.arg(p);
    }
    run_search_cmd(cmd, "ripgrep", a.max_matches(), ctx).await
}

async fn grep_posix(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    let mut cmd = tokio::process::Command::new("grep");
    cmd.arg("-rnE").arg("--color=never").current_dir(&ctx.cwd);
    if a.context() > 0 {
        cmd.arg("-C").arg(a.context().to_string());
    }
    if let Some(g) = &a.glob {
        cmd.arg(format!("--include={g}"));
    }
    cmd.arg("--").arg(&a.pattern);
    cmd.arg(a.path.as_deref().unwrap_or("."));
    run_search_cmd(cmd, "grep", a.max_matches(), ctx).await
}

/// Run a configured search command: empty stdout means "(no matches)" (search
/// tools exit non-zero on no match) unless stderr reports a real error;
/// otherwise the truncated stdout. Shared postlude of the rg/grep backends.
async fn run_search_cmd(
    mut cmd: tokio::process::Command,
    tool: &str,
    max_matches: usize,
    ctx: &ToolContext,
) -> Result<String> {
    let output = cmd
        .output()
        .await
        .with_context(|| format!("running {tool}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            bail!("{tool}: {}", stderr.trim());
        }
        return Ok("(no matches)".to_string());
    }
    // Cap by match count first (with a "narrow the pattern" nudge), then by
    // bytes as the backstop.
    Ok(truncate(&cap_matches(&stdout, max_matches), ctx.max_output))
}

/// Pure-Rust search fallback: walk the tree (honoring `.gitignore`) and match
/// each line with a regex. Used when neither ripgrep nor grep is installed.
fn grep_builtin(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    let re =
        regex::Regex::new(&a.pattern).with_context(|| format!("invalid regex: {}", a.pattern))?;
    let root = a
        .path
        .as_ref()
        .map(|p| ctx.resolve(p))
        .unwrap_or_else(|| ctx.cwd.clone());
    let glob_pat = a
        .glob
        .as_ref()
        .map(|g| glob::Pattern::new(g))
        .transpose()
        .context("invalid glob")?;

    let mut out = String::new();
    let mut matches = 0usize;
    let walker = ignore::WalkBuilder::new(&root)
        .max_depth(Some(20))
        .hidden(true)
        .build();
    'walk: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(gp) = &glob_pat {
            let name = path.file_name().map(|n| n.to_string_lossy());
            let rel = path.strip_prefix(&root).unwrap_or(path);
            let hit = name.as_deref().is_some_and(|n| gp.matches(n)) || gp.matches_path(rel);
            if !hit {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // skip binary / non-UTF-8 files
        };
        let disp = path.strip_prefix(&ctx.cwd).unwrap_or(path);
        let n_ctx = a.context();
        let max_matches = a.max_matches();
        if n_ctx == 0 {
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    matches += 1;
                    if matches > max_matches {
                        out.push_str(
                            "… [match limit reached — narrow the pattern or scope with path/glob]",
                        );
                        break 'walk;
                    }
                    out.push_str(&format!("{}:{}:{}\n", disp.display(), i + 1, line));
                    if out.len() > ctx.max_output {
                        break 'walk;
                    }
                }
            }
            continue;
        }
        // Context mode: collect this file's hits (bounded by the match cap),
        // then emit merged ±n windows — matches as `path:NN:line`, context as
        // `path-NN-line`, `--` between disjoint groups (grep/rg -C format).
        let lines: Vec<&str> = text.lines().collect();
        let mut hits: Vec<usize> = Vec::new();
        let mut capped = false;
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                if matches >= max_matches {
                    capped = true;
                    break;
                }
                matches += 1;
                hits.push(i);
            }
        }
        emit_context_windows(&mut out, &disp.display().to_string(), &lines, &hits, n_ctx);
        if capped {
            out.push_str("… [match limit reached — narrow the pattern or scope with path/glob]");
            break 'walk;
        }
        if out.len() > ctx.max_output {
            break 'walk;
        }
    }
    if out.is_empty() {
        Ok("(no matches)".to_string())
    } else {
        Ok(truncate(out.trim_end(), ctx.max_output))
    }
}

/// Append merged ±`n_ctx` windows around `hits` (0-based line indexes) in
/// grep `-C` format: `path:NN:line` for matches, `path-NN-line` for context,
/// `--` between disjoint groups (including the boundary to earlier output).
fn emit_context_windows(
    out: &mut String,
    disp: &str,
    lines: &[&str],
    hits: &[usize],
    n_ctx: usize,
) {
    if hits.is_empty() {
        return;
    }
    // Merge overlapping/adjacent windows.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &h in hits {
        let start = h.saturating_sub(n_ctx);
        let end = (h + n_ctx).min(lines.len().saturating_sub(1));
        match ranges.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }
    let hit_set: std::collections::HashSet<usize> = hits.iter().copied().collect();
    for (start, end) in ranges {
        if !out.is_empty() {
            out.push_str("--\n");
        }
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            let sep = if hit_set.contains(&i) { ':' } else { '-' };
            out.push_str(&format!("{disp}{sep}{}{sep}{line}\n", i + 1));
        }
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
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (supports `**`), relative to cwd. Returns matching \
         paths. Also the way to list a directory: pattern `src/*` lists src's contents."
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
        // Escape the cwd prefix: only the user's pattern is glob syntax. A cwd
        // containing `[`, `*`, or `?` must match literally.
        let cwd_escaped = glob::Pattern::escape(&ctx.cwd.to_string_lossy());
        let pat = std::path::Path::new(&cwd_escaped)
            .join(&a.pattern)
            .to_string_lossy()
            .to_string();
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
        let items = parse_todos(args).context("invalid todo_write args")?;
        let rendered = render_todos(&items);
        // A poisoned lock must not silently report success with a stale list.
        *ctx.todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = items;
        Ok(rendered)
    }
}

/// Forgivingly extract the todo list from `todo_write` arguments. The schema is
/// the standard `{"todos": [{content, status}, …]}`, but smaller models often
/// echo the JSON-Schema shape into the value or drop/rename the wrapper, so we
/// also accept `{"todos": {"items": […]}}` (the schema-echo mistake), a bare
/// `{"items": […]}` / `{"tasks": […]}`, and a top-level array.
fn parse_todos(args: serde_json::Value) -> Result<Vec<TodoItem>> {
    use serde_json::Value;
    let arr = match args {
        Value::Array(a) => a,
        Value::Object(mut m) => {
            let v = m
                .remove("todos")
                .or_else(|| m.remove("items"))
                .or_else(|| m.remove("tasks"))
                .ok_or_else(|| anyhow!("expected a `todos` array of {{content, status}} items"))?;
            match v {
                Value::Array(a) => a,
                // `{"todos": {"items": […]}}` — the model copied the schema's
                // `items` keyword instead of emitting a bare array.
                Value::Object(mut inner) => {
                    match inner.remove("items").or_else(|| inner.remove("todos")) {
                        Some(Value::Array(a)) => a,
                        _ => bail!("`todos` must be an array of {{content, status}} items"),
                    }
                }
                // A single item object instead of a one-element array.
                other => vec![other],
            }
        }
        _ => bail!("expected an object with a `todos` array"),
    };
    arr.into_iter().map(parse_item).collect()
}

/// Parse one todo item, tolerating `task`/`text`/`title` aliases for the content
/// and a range of status spellings (see [`normalize_status`]).
fn parse_item(v: serde_json::Value) -> Result<TodoItem> {
    use serde_json::Value;
    let Value::Object(mut m) = v else {
        bail!("each todo must be an object with a `content` string");
    };
    let content = m
        .remove("content")
        .or_else(|| m.remove("task"))
        .or_else(|| m.remove("text"))
        .or_else(|| m.remove("title"))
        .and_then(|c| match c {
            Value::String(s) => Some(s),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("each todo needs a non-empty `content` string"))?;
    let status = m
        .remove("status")
        .or_else(|| m.remove("state"))
        .and_then(|s| s.as_str().map(normalize_status))
        .unwrap_or_else(|| "pending".to_string());
    Ok(TodoItem { content, status })
}

/// Map a free-form status string onto one of `pending | in_progress | completed`.
/// Unknown values fall back to `pending`, so a bad status never fails the call.
fn normalize_status(s: &str) -> String {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "completed" | "complete" | "done" | "finished" | "x" | "[x]" => "completed",
        "in_progress" | "inprogress" | "doing" | "active" | "current" | "wip" | "started"
        | "ongoing" => "in_progress",
        _ => "pending",
    }
    .to_string()
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

    // ---- grep (built-in fallback) ----

    #[test]
    fn grep_builtin_matches_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nlet x = 1;\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "foo in text\n").unwrap();
        let c = ctx(dir.path().to_path_buf());

        // Matches across files.
        let out = super::grep_builtin(
            &GrepArgs {
                pattern: "foo".into(),
                path: None,
                glob: None,
                context: None,
            },
            &c,
        )
        .unwrap();
        assert!(out.contains("a.rs:1:fn foo() {}"), "{out}");
        assert!(out.contains("b.txt:1:foo in text"), "{out}");

        // Glob restricts to *.rs.
        let out = super::grep_builtin(
            &GrepArgs {
                pattern: "foo".into(),
                path: None,
                glob: Some("*.rs".into()),
                context: None,
            },
            &c,
        )
        .unwrap();
        assert!(out.contains("a.rs"), "{out}");
        assert!(!out.contains("b.txt"), "glob should exclude b.txt: {out}");

        // No matches.
        let out = super::grep_builtin(
            &GrepArgs {
                pattern: "zzz_nope".into(),
                path: None,
                glob: None,
                context: None,
            },
            &c,
        )
        .unwrap();
        assert_eq!(out, "(no matches)");
    }

    #[tokio::test]
    async fn grep_builtin_context_windows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.txt"),
            "l1\nl2\nl3 hit\nl4\nl5\nl6\nl7\nl8 hit\nl9\nl10\nl11\nl12\nl13 hit\nl14\n",
        )
        .unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = super::grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(1),
            },
            &c,
        )
        .unwrap();
        // Matches use `:`; context lines use `-`; disjoint groups separated
        // by `--`. Lines 3 and 8 don't overlap at ±1 → two groups; 13 makes
        // a third.
        assert!(out.contains("f.txt:3:l3 hit"), "{out}");
        assert!(out.contains("f.txt-2-l2"), "{out}");
        assert!(out.contains("f.txt-4-l4"), "{out}");
        assert!(out.contains("f.txt:8:l8 hit"), "{out}");
        assert_eq!(out.matches("--\n").count(), 2, "{out}");
        // Overlapping windows merge: context 3 joins hits 3 and 8 into one
        // group (and 13 stays separate: 8+3=11 < 13-3=10? no — 11 >= 10-1,
        // adjacent-merge joins them too, so exactly one separator drops).
        let out = super::grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(3),
            },
            &c,
        )
        .unwrap();
        assert_eq!(out.matches("--\n").count(), 0, "{out}");
        // No duplicate lines from the merge.
        assert_eq!(out.matches("l5").count(), 1, "{out}");
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
    async fn edit_and_overwrite_require_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "content").unwrap();
        let c = ctx(dir.path().to_path_buf());
        // Blind edit and blind overwrite both refuse.
        let err = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "content", "new_string": "x"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read_file first"), "{err}");
        let err = WriteTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "content": "x"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read_file first"), "{err}");
        // A read (relative path — canonicalization must unify spellings)
        // unlocks the edit.
        ReadTool
            .execute(serde_json::json!({"path": "f.txt"}), &c)
            .await
            .unwrap();
        EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "content", "new_string": "updated"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
    }

    #[tokio::test]
    async fn model_authored_writes_are_editable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let c = ctx(dir.path().to_path_buf());
        // Creating a new file needs no read; the model knows what it wrote,
        // so an immediate edit (and overwrite) is allowed.
        WriteTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "content": "alpha beta"}),
                &c,
            )
            .await
            .unwrap();
        EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "beta", "new_string": "gamma"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha gamma");
    }

    #[tokio::test]
    async fn bash_guardrail_blocks_command() {
        if which::which("bash").is_err() {
            return; // no bash on this machine
        }
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let err = BashTool
            .execute(serde_json::json!({"command": "git add -A"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("command blocked"), "{err}");
        // Harmless commands still run. Unix-only: on Windows CI `bash` on
        // PATH is the WSL stub, which errors without a distro installed.
        #[cfg(unix)]
        {
            let out = BashTool
                .execute(serde_json::json!({"command": "echo ok"}), &c)
                .await
                .unwrap();
            assert!(out.contains("ok"));
        }
    }

    #[tokio::test]
    async fn edit_whitespace_near_match_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn main() {\n    let x = 1;\n}\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        c.mark_read(&path);
        // Same tokens, wrong indentation (tab instead of 4 spaces).
        let err = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "\tlet x = 1;", "new_string": "\tlet x = 2;"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("whitespace/indentation"),
            "expected the near-match hint, got: {err}"
        );
        // Genuinely absent text keeps the generic stale-file error.
        let err = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "let y = 9;", "new_string": "z"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("may have changed"), "{err}");
    }

    #[tokio::test]
    async fn mutations_outside_cwd_refused() {
        // Tempdirs are inside the always-allowed temp tree, so the "outside"
        // target must be a non-temp path. The gate fires before any I/O, so
        // it needn't exist (and /etc isn't writable anyway — belt & braces).
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let target = "/etc/hrdr-gate-test.txt";
        let err = WriteTool
            .execute(serde_json::json!({"path": target, "content": "pwned"}), &c)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the working directory"),
            "{err}"
        );
        let err = EditTool
            .execute(
                serde_json::json!({"path": target, "old_string": "a", "new_string": "b"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the working directory"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_diff_reflects_post_hook_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let mut c = ctx(dir.path().to_path_buf());
        c.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "edit".to_string(),
            glob: None,
            run: "printf 'hooked\\n' >> {path}".to_string(),
            timeout_ms: crate::DEFAULT_HOOK_TIMEOUT_MS,
        }]);
        c.mark_read(&path);
        let out = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "hello", "new_string": "hi"}),
                &c,
            )
            .await
            .unwrap();
        // The hook ran, and the diff shows its effect too (post-hook state).
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\nhooked\n");
        assert!(out.contains("+hooked"), "diff missing hook effect:\n{out}");
        // A failing hook adds a warning but the edit still succeeds.
        c.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "edit".to_string(),
            glob: None,
            run: "exit 7".to_string(),
            timeout_ms: crate::DEFAULT_HOOK_TIMEOUT_MS,
        }]);
        let out = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "hi", "new_string": "hey"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("[hook `exit 7` failed"), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hey\nhooked\n");
    }

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
        c.mark_read(&path); // edits require a prior read
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
        c.mark_read(&path);
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
        c.mark_read(&path);
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
        c.mark_read(&path);
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

    #[test]
    fn parse_todos_accepts_schema_echo_and_variants() {
        let want = |items: &[TodoItem]| {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].content, "a");
            assert_eq!(items[0].status, "in_progress");
            assert_eq!(items[1].content, "b");
            assert_eq!(items[1].status, "completed");
        };
        let items = [
            json!({"content": "a", "status": "in_progress"}),
            json!({"content": "b", "status": "completed"}),
        ];
        // Correct shape.
        want(&parse_todos(json!({ "todos": items })).unwrap());
        // The schema-echo mistake: `{"todos": {"items": [...]}}`.
        want(&parse_todos(json!({ "todos": { "items": items } })).unwrap());
        // Dropped/renamed wrapper key, and a bare top-level array.
        want(&parse_todos(json!({ "items": items })).unwrap());
        want(&parse_todos(json!({ "tasks": items })).unwrap());
        want(&parse_todos(json!(items)).unwrap());
    }

    #[test]
    fn parse_todos_tolerates_status_synonyms_and_content_aliases() {
        let items = parse_todos(json!({
            "todos": [
                {"content": "x", "status": "DONE"},
                {"task": "y", "state": "doing"},   // `task` alias, `state` alias
                {"text": "z"},                       // no status → pending
                {"title": "w", "status": "wat"},    // unknown status → pending
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(
            (items[0].content.as_str(), items[0].status.as_str()),
            ("x", "completed")
        );
        assert_eq!(
            (items[1].content.as_str(), items[1].status.as_str()),
            ("y", "in_progress")
        );
        assert_eq!(
            (items[2].content.as_str(), items[2].status.as_str()),
            ("z", "pending")
        );
        assert_eq!(
            (items[3].content.as_str(), items[3].status.as_str()),
            ("w", "pending")
        );
    }

    #[test]
    fn parse_todos_rejects_itemless_content() {
        // An item with no usable content string is an error (not silently kept).
        assert!(parse_todos(json!({ "todos": [{"status": "pending"}] })).is_err());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_process_and_keeps_partial_output() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = BashTool
            .execute(
                serde_json::json!({"command": "echo early; sleep 30", "timeout_ms": 300}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("early"), "partial output missing: {out}");
        assert!(out.contains("timed out"), "timeout marker missing: {out}");
    }
}
