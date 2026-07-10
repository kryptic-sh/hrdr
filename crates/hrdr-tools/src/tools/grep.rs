use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, TruncateSide, cap_matches, truncate_saved};

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
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) glob: Option<String>,
    #[serde(default)]
    pub(crate) context: Option<usize>,
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
         instead of making a follow-up read call."
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
        let a: GrepArgs = crate::tool_args("grep", args)?;
        // Refuse to scope a search directly at a credential/secret file — grep
        // reads file *contents*, so it's an exfiltration vector like `read`.
        if let Some(p) = &a.path {
            crate::guard_secret_read(&ctx.resolve(p))?;
        }
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
    let raw = String::from_utf8_lossy(&output.stdout);
    if raw.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            bail!("{tool}: {}", stderr.trim());
        }
        return Ok("(no matches)".to_string());
    }
    // Drop any match lines that name a secret file (e.g. a `.env` in the tree),
    // so a broad `grep KEY .` can't surface credentials the `read` deny-list
    // would refuse. Best-effort: covers `path:NN:…` match lines.
    let stdout: String = raw
        .lines()
        .filter(|l| !crate::grep_line_is_secret(l, &ctx.cwd))
        .collect::<Vec<_>>()
        .join("\n");
    if stdout.is_empty() {
        return Ok("(no matches)".to_string());
    }
    // Cap by match count first (with a "narrow the pattern" nudge), then by
    // bytes as the backstop.
    Ok(truncate_saved(
        &cap_matches(&stdout, max_matches),
        ctx.max_output,
        ctx.max_output_lines,
        TruncateSide::Head,
        "grep",
    ))
}

/// Pure-Rust search fallback: walk the tree (honoring `.gitignore`) and match
/// each line with a regex. Used when neither ripgrep nor grep is installed.
pub(crate) fn grep_builtin(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
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
        if crate::secret_file_reason(&crate::canonicalize_nearest(path)).is_some() {
            continue; // never read credential/secret files (see deny-list)
        }
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
        Ok(truncate_saved(
            out.trim_end(),
            ctx.max_output,
            ctx.max_output_lines,
            TruncateSide::Head,
            "grep",
        ))
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
    let hit_set: HashSet<usize> = hits.iter().copied().collect();
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
