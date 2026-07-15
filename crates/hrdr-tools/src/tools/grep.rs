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
    #[serde(default)]
    pub(crate) multiline: bool,
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
         instead of making a follow-up read call. Set `multiline` to true for patterns that \
         span line boundaries."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default cwd)."},
                "glob": {"type": "string", "description": "Glob to filter files, e.g. '*.rs'."},
                "context": {"type": "integer", "description": "Lines of context around each match (0-10, default 0)."},
                "multiline": {"type": "boolean", "description": "Allow regex matches to span line boundaries (default false)."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GrepArgs = crate::tool_args("grep", args)?;
        // A search scoped at an explicit path must stay inside the project and
        // off the credential deny-list: grep reads file *contents*, so an
        // out-of-cwd or secret root is an exfiltration vector like `read`. With
        // no path it searches cwd, which is confined by construction.
        if let Some(p) = &a.path {
            let root = ctx.resolve(p);
            crate::guard_secret_read(&root)?;
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
        .arg("--with-filename")
        .arg("--no-heading")
        .arg("--color=never")
        .current_dir(&ctx.cwd);
    if a.multiline {
        cmd.arg("--multiline");
    }
    if a.context() > 0 {
        cmd.arg("-C").arg(a.context().to_string());
    }
    if let Some(g) = &a.glob {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(&a.pattern);
    // Always pass an explicit path. With none, ripgrep reads STDIN when it isn't
    // a TTY — and under a nulled/redirected stdin every unscoped search would
    // silently return "(no matches)" or hang. Default to cwd, matching the POSIX
    // backend below.
    cmd.arg(a.path.as_deref().unwrap_or("."));
    run_search_cmd(cmd, "ripgrep", a.max_matches(), ctx).await
}

async fn grep_posix(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    // POSIX grep cannot match across records. Use the same portable in-process
    // implementation as the no-binary fallback for multiline requests.
    if a.multiline {
        return grep_builtin(a, ctx);
    }
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
    cmd: tokio::process::Command,
    tool: &str,
    max_matches: usize,
    ctx: &ToolContext,
) -> Result<String> {
    // `output()` would buffer the *entire* stdout before any cap ran — an
    // unscoped `grep .` across a monorepo can be hundreds of MB — and would not
    // kill the child on Esc. `run_capped_output` nulls stdin, sets
    // `kill_on_drop`, and stops accumulating stdout past a generous ceiling
    // (5× the byte budget `truncate_saved` trims to below, the same headroom the
    // shell tool keeps in memory) so a 10 GB output is cut early. Anything that
    // fits under the ceiling is byte-for-byte what `output()` produced.
    let cap = ctx.max_output.saturating_mul(5).max(ctx.max_output);
    let (_status, stdout_bytes, stderr_bytes) = super::run_capped_output(cmd, cap, cap)
        .await
        .with_context(|| format!("running {tool}"))?;
    let raw = String::from_utf8_lossy(&stdout_bytes);
    if raw.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
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
    if a.multiline {
        return grep_builtin_multiline(a, ctx);
    }
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

/// Cross-line variant of the built-in walker. Every line touched by a match is
/// emitted as a match line. POSIX grep uses this path too because its executable
/// has no portable cross-record matching mode.
fn grep_builtin_multiline(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
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

    'walk: for entry in ignore::WalkBuilder::new(&root)
        .max_depth(Some(20))
        .hidden(true)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if crate::secret_file_reason(&crate::canonicalize_nearest(path)).is_some() {
            continue;
        }
        if let Some(gp) = &glob_pat {
            let name = path.file_name().map(|n| n.to_string_lossy());
            let rel = path.strip_prefix(&root).unwrap_or(path);
            if !name.as_deref().is_some_and(|n| gp.matches(n)) && !gp.matches_path(rel) {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }
        let mut matched_lines = HashSet::new();
        let mut capped = false;
        for hit in re.find_iter(&text) {
            if matches >= a.max_matches() {
                capped = true;
                break;
            }
            matches += 1;
            let start = text[..hit.start()].bytes().filter(|b| *b == b'\n').count();
            let last_byte = hit.end().saturating_sub(1).max(hit.start());
            let end = text[..last_byte].bytes().filter(|b| *b == b'\n').count();
            for line in start..=end.min(lines.len().saturating_sub(1)) {
                matched_lines.insert(line);
                if matched_lines.len() >= ctx.max_output_lines {
                    capped = true;
                    break;
                }
            }
            if capped {
                break;
            }
        }
        if !matched_lines.is_empty() {
            let mut hits: Vec<usize> = matched_lines.into_iter().collect();
            hits.sort_unstable();
            let disp = path.strip_prefix(&ctx.cwd).unwrap_or(path);
            if a.context() == 0 {
                for i in hits {
                    out.push_str(&format!("{}:{}:{}\n", disp.display(), i + 1, lines[i]));
                }
            } else {
                emit_context_windows(
                    &mut out,
                    &disp.display().to_string(),
                    &lines,
                    &hits,
                    a.context(),
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn multiline_args(pattern: &str) -> GrepArgs {
        GrepArgs {
            pattern: pattern.to_string(),
            path: Some("sample.txt".to_string()),
            glob: None,
            context: None,
            multiline: true,
        }
    }

    #[test]
    fn multiline_defaults_to_false_and_is_in_schema() {
        let args: GrepArgs = serde_json::from_value(json!({ "pattern": "x" })).unwrap();
        assert!(!args.multiline);
        let schema = GrepTool::detect().parameters();
        assert_eq!(
            schema["properties"]["multiline"]["type"],
            serde_json::Value::String("boolean".into())
        );
    }

    #[test]
    fn builtin_multiline_matches_across_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = grep_builtin(&multiline_args("foo\\nbar"), &ctx).unwrap();
        assert!(out.contains("sample.txt:2:foo"), "{out}");
        assert!(out.contains("sample.txt:3:bar"), "{out}");
    }

    #[test]
    fn builtin_without_multiline_does_not_cross_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "foo\nbar\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut args = multiline_args("foo\\nbar");
        args.multiline = false;
        assert_eq!(grep_builtin(&args, &ctx).unwrap(), "(no matches)");
    }

    #[test]
    fn builtin_multiline_zero_width_match_on_empty_file_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "").unwrap();
        let ctx = ToolContext::new(dir.path());
        assert_eq!(
            grep_builtin(&multiline_args("^"), &ctx).unwrap(),
            "(no matches)"
        );
    }

    #[test]
    fn builtin_multiline_spanning_match_respects_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let text = (0..100).map(|i| format!("line{i}\n")).collect::<String>();
        std::fs::write(dir.path().join("sample.txt"), text).unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.max_output_lines = 5;
        let out = grep_builtin(&multiline_args("(?s).*"), &ctx).unwrap();
        assert!(out.lines().count() <= 7, "{out}");
        assert!(out.contains("full output"), "{out}");
    }

    /// An unscoped search (no `path`) must search the working tree, not stdin.
    /// ripgrep with no path argument reads STDIN when it isn't a TTY, so under a
    /// nulled/redirected stdin an unscoped grep used to silently return
    /// "(no matches)" — the model would wrongly conclude the symbol is absent.
    /// Passing an explicit `.` fixes it and aligns rg with the POSIX backend.
    #[tokio::test]
    async fn ripgrep_without_path_searches_the_tree_not_stdin() {
        if which::which("rg").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn needle() {}\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let a = GrepArgs {
            pattern: "needle".to_string(),
            path: None,
            glob: None,
            context: None,
            multiline: false,
        };
        let out = grep_ripgrep(&a, &ctx).await.unwrap();
        assert!(out.contains("code.rs:1:fn needle"), "{out}");
    }

    #[tokio::test]
    async fn ripgrep_multiline_matches_across_line_boundary() {
        if which::which("rg").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = grep_ripgrep(&multiline_args("foo\\nbar"), &ctx)
            .await
            .unwrap();
        assert!(out.contains("sample.txt:2:foo"), "{out}");
        assert!(out.contains("sample.txt:3:bar"), "{out}");
    }

    #[tokio::test]
    async fn posix_backend_multiline_matches_across_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = grep_posix(&multiline_args("foo\\nbar"), &ctx)
            .await
            .unwrap();
        assert!(out.contains("sample.txt:2:foo"), "{out}");
        assert!(out.contains("sample.txt:3:bar"), "{out}");
    }

    #[test]
    fn builtin_multiline_preserves_context_and_glob_filtering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        std::fs::write(dir.path().join("sample.rs"), "foo\nbar\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut args = multiline_args("foo\\nbar");
        args.glob = Some("*.txt".into());
        args.context = Some(1);
        let out = grep_builtin(&args, &ctx).unwrap();
        assert!(out.contains("sample.txt-1-before"), "{out}");
        assert!(out.contains("sample.txt-4-after"), "{out}");
        assert!(!out.contains("sample.rs"), "{out}");
    }

    /// A search scoped at an explicit path outside the project is refused
    /// before any backend runs — grep reads file contents, so an out-of-cwd
    /// root is an exfiltration vector. Backend-independent: the guard lives in
    /// `execute`, so `GrepTool::detect()`'s chosen backend doesn't matter.
    #[tokio::test]
    async fn grep_allows_a_path_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.txt"), "needle here").unwrap();

        let ctx = ToolContext::new(cwd.path());
        let out = GrepTool::detect()
            .execute(
                serde_json::json!({
                    "pattern": "needle",
                    "path": outside.path().to_str().unwrap(),
                }),
                &ctx,
            )
            .await
            .expect("grepping outside cwd is allowed");
        assert!(out.contains("needle"), "got: {out}");
    }

    /// With `context > 0`, a `.env` line adjacent to a match must not leak via
    /// a `-C` context line (`path-NN-content`) — the secret filter used to
    /// only recognise `path:NN:` match lines, so the context form rode along
    /// unfiltered. Exercises the real POSIX-`grep` backend, not just the
    /// builtin walker (which drops secret files entirely at the walk level).
    #[tokio::test]
    async fn context_lines_do_not_leak_env_secrets_via_posix_grep() {
        if which::which("grep").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "BEFORE=1\nAPI_KEY=supersecret\nAFTER=1\n",
        )
        .unwrap();
        let ctx = ToolContext::new(dir.path());
        let a = GrepArgs {
            pattern: "API_KEY".to_string(),
            path: None,
            glob: None,
            context: Some(2),
            multiline: false,
        };
        let out = grep_posix(&a, &ctx).await.unwrap();
        assert!(!out.contains("supersecret"), "{out}");
        assert!(!out.contains(".env"), "{out}");
    }

    /// Same guarantee for the pure-Rust builtin fallback (used when neither
    /// `rg` nor `grep` is installed): it already skips secret files at the
    /// walk level, but pin it here too so a refactor can't silently regress.
    #[test]
    fn context_lines_do_not_leak_env_secrets_via_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "BEFORE=1\nAPI_KEY=supersecret\nAFTER=1\n",
        )
        .unwrap();
        let ctx = ToolContext::new(dir.path());
        let a = GrepArgs {
            pattern: "API_KEY".to_string(),
            path: None,
            glob: None,
            context: Some(2),
            multiline: false,
        };
        let out = grep_builtin(&a, &ctx).unwrap();
        assert!(!out.contains("supersecret"), "{out}");
        assert_eq!(out, "(no matches)");
    }
}
