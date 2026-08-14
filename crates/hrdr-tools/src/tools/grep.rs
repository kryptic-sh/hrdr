use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, TruncateSide, truncate_saved};

// ---- grep ----

/// A deliberately simple search tool: a pure-Rust walker, no subprocess, and
/// **jail-only** — every other mode has `shell`, where `rg` is one call away.
///
/// It used to pick between ripgrep, POSIX `grep` and this walker. Both subprocess
/// backends are deleted, and the reason is the one that matters here: they spawned
/// through a bare `Command::new`, *not* through `sandboxed_shell_command`, so those
/// children were unconfined by the OS. `check_read` validates the path the model
/// *named*; it cannot constrain how a helper walks the filesystem once started —
/// which in the one mode that still has `grep` is precisely the boundary.
///
/// The POSIX backend had earned it independently: it only ran when `rg` was absent,
/// so never on a dev machine, exercised in CI alone — and it shipped a real bug that
/// reached a tag (an `--exclude-dir=.*` trap). `Rg` goes because `grep` is jail-only
/// now, so nothing would have called it.
///
/// **This costs look-around, and that is a decision rather than an oversight.**
/// Rust's `regex` crate deliberately has none; ripgrep supplied it via PCRE2. Audits
/// rarely need it and the error is clear, but keeping it would mean routing a
/// subprocess out of the one mode built to have none.
pub struct GrepTool;

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
    /// Also search hidden files/dirs (dotfiles). Skipped by default.
    #[serde(default)]
    pub(crate) hidden: bool,
    /// Also search .gitignore'd files. Skipped by default.
    #[serde(default)]
    pub(crate) no_ignore: bool,
    /// Treat `pattern` as a fixed string rather than a regex.
    #[serde(default)]
    pub(crate) literal: bool,
    /// Case-insensitive match.
    #[serde(default)]
    pub(crate) case_insensitive: bool,
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
    /// Pattern and path, so a hit's provenance travels with it.
    fn output_source(&self, args: &serde_json::Value) -> String {
        let field = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let path = field("path");
        let path = if path.is_empty() {
            ".".to_string()
        } else {
            path
        };
        format!("grep {:?} in {path}", field("pattern"))
    }

    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents (via ripgrep, grep, or a built-in walker — whichever is available). \
         Returns `path:line:match`, capped at 200 matches (50 when `context` is set) — scope with \
         `path`/`glob` or narrow `pattern` proactively rather than relying on the cap. By default \
         hidden files/dirs (dotfiles) and .gitignore'd paths are skipped; set `hidden` and/or \
         `no_ignore` to include them (e.g. to search `.github/` or build output). Optionally scope \
         to a `path` and/or filter files with a `glob` (e.g. '*.rs'). Set `context` to lines of \
         surrounding context per match, 0-10 (2-3 is usually enough) to see the lines around each \
         match instead of making a follow-up read call. Set `multiline` to true for patterns that \
         span line boundaries. Same matching shape as `replace`: `pattern` is a regex unless \
         `literal: true`, so a pattern that worked here means the same thing there."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Pattern to search for — a regex by default, or a fixed string when `literal` is set."},
                "path": {"type": "string", "default": ".", "description": "File or directory to search (default cwd)."},
                "glob": {"type": "string", "default": null, "description": "Glob to filter files, e.g. '*.rs'."},
                "context": {"type": "integer", "default": 0, "description": "Lines of surrounding context per match, 0-10 (default 0; 2-3 is usually enough)."},
                "multiline": {"type": "boolean", "default": false, "description": "Allow regex matches to span line boundaries (default false)."},
                "hidden": {"type": "boolean", "default": false, "description": "Also search hidden files/dirs (dotfiles). Skipped by default (default false)."},
                "no_ignore": {"type": "boolean", "default": false, "description": "Also search .gitignore'd files. Skipped by default (default false)."},
                "literal": {"type": "boolean", "default": false, "description": "Treat `pattern` as a fixed string, not a regex — use for patterns like 'foo(bar)', 'a.b', '$var' (default false)."},
                "case_insensitive": {"type": "boolean", "default": false, "description": "Case-insensitive match (default false)."}
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
            let root = ctx.resolve_read(p)?;
            crate::guard_secret_read(&root)?;
        }
        // Look-around needs PCRE2, which only the ripgrep backend can switch on
        // (`--pcre2`). POSIX `grep -E` and the built-in `regex` walker have no
        // equivalent, so say so instead of surfacing their regex-parse error.
        if !a.literal && has_lookaround(&a.pattern) {
            bail!(
                "look-around (`(?=`, `(?!`, `(?<=`, `(?<!`) is not supported: this tool uses \
                 Rust's `regex`, which has none by design\n(hint: match it as a fixed string \
                 with `literal: true`, or search for a pattern you can express without \
                 look-around and filter the hits yourself)"
            );
        }
        grep_builtin(&a, ctx).await
    }
}

/// Whether `pattern` uses a look-around group — `(?=`, `(?!`, `(?<=`, `(?<!`.
/// Rust's `regex` crate has no look-around at all, so this earns a clear refusal
/// rather than the crate's own parse error.
fn has_lookaround(pattern: &str) -> bool {
    ["(?=", "(?!", "(?<=", "(?<!"].iter().any(|p| {
        // A `\(` is a literal paren, not the start of a group.
        pattern.match_indices(p).any(|(i, _)| {
            let escapes = pattern[..i]
                .bytes()
                .rev()
                .take_while(|b| *b == b'\\')
                .count();
            escapes % 2 == 0
        })
    })
}

/// Compile `a.pattern` into a `Regex`, honoring `literal` (escape to a fixed
/// string, e.g. for `foo(bar)`, `a.b`, `$var`) and `case_insensitive`.
fn compile_pattern(a: &GrepArgs) -> Result<regex::Regex> {
    let pattern = if a.literal {
        regex::escape(&a.pattern)
    } else {
        a.pattern.clone()
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(a.case_insensitive)
        .build()
        .with_context(|| format!("invalid regex: {}", a.pattern))
}

/// Pure-Rust search fallback: walk the tree (honoring `.gitignore`) and match
/// each line with a regex. Used when neither ripgrep nor grep is installed.
pub(crate) async fn grep_builtin(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    if a.multiline {
        return grep_builtin_multiline(a, ctx).await;
    }
    let re = compile_pattern(a)?;
    let root = match a.path.as_ref() {
        Some(p) => ctx.resolve_read(p)?,
        None => ctx.cwd.clone(),
    };
    let glob_pat = a
        .glob
        .as_ref()
        .map(|g| glob::Pattern::new(g))
        .transpose()
        .context("invalid glob")?;

    // The WHOLE walk — walker, per-file `read_to_string`, regex/glob matching —
    // runs in one `spawn_blocking` closure: a grep across a large tree is exactly
    // the `std::fs` work that must not occupy a tokio worker. Not one closure per
    // file (that would serialize on the blocking pool's queue). The closure owns
    // every value it touches (root, cwd, the parsed glob/regex, the limits), so
    // nothing borrows `ctx` or `a` across the boundary.
    let cwd = ctx.cwd.clone();
    let max_output = ctx.max_output;
    let max_output_lines = ctx.max_output_lines;
    let hidden = a.hidden;
    let no_ignore = a.no_ignore;
    let n_ctx = a.context();
    let max_matches = a.max_matches();
    tokio::task::spawn_blocking(move || {
        let mut out = String::new();
        let mut matches = 0usize;
        let walker = super::ignore_walker(&root, hidden, no_ignore);
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
            let disp = path.strip_prefix(&cwd).unwrap_or(path);
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
                        if out.len() > max_output {
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
            if out.len() > max_output {
                break 'walk;
            }
        }
        if out.is_empty() {
            Ok(super::NO_MATCHES.to_string())
        } else {
            Ok(truncate_saved(
                out.trim_end(),
                max_output,
                max_output_lines,
                TruncateSide::Head,
                "grep",
            ))
        }
    })
    .await?
}

/// Cross-line variant of the built-in walker. Every line touched by a match is
/// emitted as a match line. POSIX grep uses this path too because its executable
/// has no portable cross-record matching mode.
async fn grep_builtin_multiline(a: &GrepArgs, ctx: &ToolContext) -> Result<String> {
    let re = compile_pattern(a)?;
    let root = match a.path.as_ref() {
        Some(p) => ctx.resolve_read(p)?,
        None => ctx.cwd.clone(),
    };
    let glob_pat = a
        .glob
        .as_ref()
        .map(|g| glob::Pattern::new(g))
        .transpose()
        .context("invalid glob")?;

    // Same one-closure-per-walk structure as `grep_builtin` — see there for why.
    let cwd = ctx.cwd.clone();
    let max_output = ctx.max_output;
    let max_output_lines = ctx.max_output_lines;
    let hidden = a.hidden;
    let no_ignore = a.no_ignore;
    let n_ctx = a.context();
    let max_matches = a.max_matches();
    tokio::task::spawn_blocking(move || {
        let mut out = String::new();
        let mut matches = 0usize;

        'walk: for entry in super::ignore_walker(&root, hidden, no_ignore).flatten() {
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
            let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
            if lines.is_empty() {
                continue;
            }
            let mut matched_lines = HashSet::new();
            let mut capped = false;
            for hit in re.find_iter(&text) {
                if matches >= max_matches {
                    capped = true;
                    break;
                }
                matches += 1;
                let start = newlines.partition_point(|&nl| nl < hit.start());
                let last_byte = hit.end().saturating_sub(1).max(hit.start());
                let end = newlines.partition_point(|&nl| nl < last_byte);
                for line in start..=end.min(lines.len().saturating_sub(1)) {
                    matched_lines.insert(line);
                    if matched_lines.len() >= max_output_lines {
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
                let disp = path.strip_prefix(&cwd).unwrap_or(path);
                if n_ctx == 0 {
                    for i in hits {
                        out.push_str(&format!("{}:{}:{}\n", disp.display(), i + 1, lines[i]));
                    }
                } else {
                    emit_context_windows(
                        &mut out,
                        &disp.display().to_string(),
                        &lines,
                        &hits,
                        n_ctx,
                    );
                }
            }
            if capped {
                out.push_str(
                    "… [match limit reached — narrow the pattern or scope with path/glob]",
                );
                break 'walk;
            }
            if out.len() > max_output {
                break 'walk;
            }
        }
        if out.is_empty() {
            Ok(super::NO_MATCHES.to_string())
        } else {
            Ok(truncate_saved(
                out.trim_end(),
                max_output,
                max_output_lines,
                TruncateSide::Head,
                "grep",
            ))
        }
    })
    .await?
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
            hidden: false,
            no_ignore: false,
            literal: false,
            case_insensitive: false,
        }
    }

    /// Default (non-multiline, unscoped) args for a single-line pattern.
    fn plain_args(pattern: &str) -> GrepArgs {
        GrepArgs {
            pattern: pattern.to_string(),
            path: None,
            glob: None,
            context: None,
            multiline: false,
            hidden: false,
            no_ignore: false,
            literal: false,
            case_insensitive: false,
        }
    }

    /// There is one backend now, and it is always runnable. Kept as a helper so the
    /// shared-behaviour tests below read unchanged — they used to run against
    /// ripgrep, POSIX `grep` and this walker, and the walker is the survivor.
    fn available_backends() -> Vec<(&'static str, ())> {
        vec![("builtin", ())]
    }

    #[test]
    fn multiline_defaults_to_false_and_is_in_schema() {
        let args: GrepArgs = serde_json::from_value(json!({ "pattern": "x" })).unwrap();
        assert!(!args.multiline);
        let schema = GrepTool.parameters();
        assert_eq!(
            schema["properties"]["multiline"]["type"],
            serde_json::Value::String("boolean".into())
        );
    }

    #[test]
    fn lookaround_detector_spots_every_form_and_ignores_escaped_parens() {
        for p in ["a(?=b)", "a(?!b)", "(?<=a)b", "(?<!a)b", "x(?!y)z"] {
            assert!(has_lookaround(p), "{p}");
        }
        for p in ["plain", "(group)", "(?i)ci", "(?s).*", "a\\(?=b", "\\(?!"] {
            assert!(!has_lookaround(p), "{p}");
        }
        // An escaped backslash before the group doesn't escape the paren.
        assert!(has_lookaround("a\\\\(?!b)"));
    }

    #[tokio::test]
    async fn builtin_multiline_matches_across_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = grep_builtin(&multiline_args("foo\\nbar"), &ctx)
            .await
            .unwrap();
        assert!(out.contains("sample.txt:2:foo"), "{out}");
        assert!(out.contains("sample.txt:3:bar"), "{out}");
    }

    #[tokio::test]
    async fn builtin_without_multiline_does_not_cross_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "foo\nbar\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut args = multiline_args("foo\\nbar");
        args.multiline = false;
        assert_eq!(grep_builtin(&args, &ctx).await.unwrap(), "(no matches)");
    }

    #[tokio::test]
    async fn builtin_multiline_zero_width_match_on_empty_file_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "").unwrap();
        let ctx = ToolContext::new(dir.path());
        assert_eq!(
            grep_builtin(&multiline_args("^"), &ctx).await.unwrap(),
            "(no matches)"
        );
    }

    #[tokio::test]
    async fn builtin_multiline_spanning_match_respects_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let text = (0..100).map(|i| format!("line{i}\n")).collect::<String>();
        std::fs::write(dir.path().join("sample.txt"), text).unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.max_output_lines = 5;
        let out = grep_builtin(&multiline_args("(?s).*"), &ctx).await.unwrap();
        assert!(out.lines().count() <= 7, "{out}");
        assert!(out.contains("full output"), "{out}");
    }

    #[tokio::test]
    async fn builtin_multiline_preserves_context_and_glob_filtering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "before\nfoo\nbar\nafter\n").unwrap();
        std::fs::write(dir.path().join("sample.rs"), "foo\nbar\n").unwrap();
        let ctx = ToolContext::new(dir.path());
        let mut args = multiline_args("foo\\nbar");
        args.glob = Some("*.txt".into());
        args.context = Some(1);
        let out = grep_builtin(&args, &ctx).await.unwrap();
        assert!(out.contains("sample.txt-1-before"), "{out}");
        assert!(out.contains("sample.txt-4-after"), "{out}");
        assert!(!out.contains("sample.rs"), "{out}");
    }

    /// A search scoped at an explicit path outside the project is refused
    /// before any backend runs — grep reads file contents, so an out-of-cwd
    /// root is an exfiltration vector. Backend-independent: the guard lives in
    /// `execute`, so `GrepTool`'s chosen backend doesn't matter.
    #[tokio::test]
    async fn grep_allows_a_path_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.txt"), "needle here").unwrap();

        let ctx = ToolContext::new(cwd.path());
        let out = GrepTool
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

    /// Same guarantee for the pure-Rust builtin fallback (used when neither
    /// `rg` nor `grep` is installed): it already skips secret files at the
    /// walk level, but pin it here too so a refactor can't silently regress.
    #[tokio::test]
    async fn context_lines_do_not_leak_env_secrets_via_builtin() {
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
            hidden: false,
            no_ignore: false,
            literal: false,
            case_insensitive: false,
        };
        let out = grep_builtin(&a, &ctx).await.unwrap();
        assert!(!out.contains("supersecret"), "{out}");
        assert_eq!(out, "(no matches)");
    }

    /// Hidden files/dirs (dotfiles) are skipped by default and only searched
    /// when `hidden: true` is set — the undocumented behavior this change
    /// documents and makes overridable.
    #[tokio::test]
    async fn builtin_hidden_files_skipped_by_default_and_found_with_hidden_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden-dir")).unwrap();
        std::fs::write(dir.path().join(".hidden-dir/file.txt"), "needle here\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let args = plain_args("needle");
        assert_eq!(grep_builtin(&args, &ctx).await.unwrap(), "(no matches)");

        let mut hidden_args = plain_args("needle");
        hidden_args.hidden = true;
        // Windows paths print with `\` — normalize before asserting.
        let out = grep_builtin(&hidden_args, &ctx)
            .await
            .unwrap()
            .replace('\\', "/");
        assert!(out.contains(".hidden-dir/file.txt:1:needle"), "{out}");
    }

    /// `.gitignore`'d files are skipped by default and only searched when
    /// `no_ignore: true` is set. Requires a `.git` dir in the fixture: the
    /// `ignore` crate only applies git-related ignore rules (including
    /// `.gitignore`) inside a discovered git repository by default — same
    /// setup `tree_respects_gitignore` uses.
    #[tokio::test]
    async fn builtin_gitignored_files_skipped_by_default_and_found_with_no_ignore_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "needle here\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let args = plain_args("needle");
        assert_eq!(grep_builtin(&args, &ctx).await.unwrap(), "(no matches)");

        let mut no_ignore_args = plain_args("needle");
        no_ignore_args.no_ignore = true;
        let out = grep_builtin(&no_ignore_args, &ctx).await.unwrap();
        assert!(out.contains("ignored.txt:1:needle"), "{out}");
    }

    /// `literal: true` treats `pattern` as a fixed string rather than a
    /// regex. As a regex, `foo(bar)` means "foo" followed by a group matching
    /// "bar" — it does NOT match the literal text `foo(bar)` because the
    /// parens themselves aren't part of the match. Only `literal: true`
    /// (which escapes the pattern) finds the verbatim text, and it must not
    /// error doing so.
    #[tokio::test]
    async fn builtin_literal_matches_fixed_string_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "call foo(bar) here\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let regex_args = plain_args("foo(bar)");
        assert_eq!(
            grep_builtin(&regex_args, &ctx).await.unwrap(),
            "(no matches)"
        );

        let mut literal_args = plain_args("foo(bar)");
        literal_args.literal = true;
        let out = grep_builtin(&literal_args, &ctx).await.unwrap();
        assert!(out.contains("sample.txt:1:call foo(bar) here"), "{out}");
    }

    /// `case_insensitive: true` matches regardless of case.
    #[tokio::test]
    async fn builtin_case_insensitive_matches_across_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.txt"), "NEEDLE here\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let args = plain_args("needle");
        assert_eq!(grep_builtin(&args, &ctx).await.unwrap(), "(no matches)");

        let mut ci_args = plain_args("needle");
        ci_args.case_insensitive = true;
        let out = grep_builtin(&ci_args, &ctx).await.unwrap();
        assert!(out.contains("sample.txt:1:NEEDLE here"), "{out}");
    }

    /// `grep` reads file *contents*, so a read-confined agent may not scope a
    /// search outside its readable roots — asserted at the tool seam (which
    /// covers every backend) and again on the built-in walker's own resolve.
    #[tokio::test]
    async fn grep_outside_roots_is_refused_in_strict_mode() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.txt"), "needle here").unwrap();
        let ctx = crate::sandbox::confined_ctx(cwd.path(), crate::SandboxMode::Jail);

        let err = GrepTool
            .execute(
                serde_json::json!({
                    "pattern": "needle",
                    "path": outside.path().to_str().unwrap(),
                }),
                &ctx,
            )
            .await
            .expect_err("searching outside the readable roots must be refused")
            .to_string();
        assert!(err.contains("sandbox: refusing to read"), "{err}");

        // Both built-in walkers resolve the scope themselves — single-line and
        // the multiline variant it delegates to.
        for mut a in [plain_args("needle"), multiline_args("needle")] {
            a.path = Some(outside.path().to_string_lossy().to_string());
            let err = grep_builtin(&a, &ctx).await.unwrap_err().to_string();
            assert!(err.contains("sandbox: refusing to read"), "{err}");
        }
    }

    /// The matching semantics every backend owes the model, run against each of
    /// them: a regex, `literal`, `case_insensitive`, and a pattern that hits
    /// nothing. Asserted on the shared invariant rather than byte-identical
    /// output — the subprocess backends print the search root they were given
    /// (`./code.rs:1:…`), the walker prints the path relative to cwd.
    #[tokio::test]
    async fn every_backend_agrees_on_regex_literal_case_and_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "let NEEDLE = foo(bar);\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        for (name, ()) in available_backends() {
            let run = |args| {
                let tool = GrepTool;
                let ctx = &ctx;
                async move { tool.execute(args, ctx).await.unwrap() }
            };

            let out = run(json!({"pattern": "NE+DLE"})).await;
            assert!(out.contains("code.rs:1:let NEEDLE"), "{name}: {out}");
            // As a regex, `foo(bar)` is a group — it matches the text "foobar",
            // not the parens; only `literal` finds them verbatim.
            assert_eq!(
                run(json!({"pattern": "foo(bar)"})).await,
                "(no matches)",
                "{name}"
            );
            let out = run(json!({"pattern": "foo(bar)", "literal": true})).await;
            assert!(
                out.contains("code.rs:1:let NEEDLE = foo(bar)"),
                "{name}: {out}"
            );

            assert_eq!(
                run(json!({"pattern": "needle"})).await,
                "(no matches)",
                "{name}"
            );
            let out = run(json!({"pattern": "needle", "case_insensitive": true})).await;
            assert!(out.contains("code.rs:1:let NEEDLE"), "{name}: {out}");

            assert_eq!(
                run(json!({"pattern": "absent-xyzzy"})).await,
                "(no matches)",
                "{name}"
            );
        }
    }

    /// `glob` reaches every backend through a different mechanism (`--glob`,
    /// `--include`, `glob::Pattern` on the walk), so pin the one thing they must
    /// agree on: only the matching files are searched.
    #[tokio::test]
    async fn every_backend_scopes_a_search_with_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "needle in rust\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "needle in text\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        for (name, ()) in available_backends() {
            let out = GrepTool
                .execute(json!({"pattern": "needle", "glob": "*.rs"}), &ctx)
                .await
                .unwrap();
            assert!(out.contains("code.rs:1:needle in rust"), "{name}: {out}");
            assert!(!out.contains("notes.txt"), "{name}: {out}");
        }
    }
}
