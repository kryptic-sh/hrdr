//! `replace`: one substitution applied across many files, under the guards.
//!
//! The alternative a model reaches for is `bash sed -i`, which is the single
//! worst mutation path available to it: silent about what it changed — a bad
//! regex corrupts the tree and the model reports success.
//!
//! This tool walks the project respecting `.gitignore`, matches a **literal**
//! string by default (a regex only when asked), and returns a unified diff per
//! file so the change is visible in the transcript. `dry_run: true` reports what
//! *would* change without writing.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::edit::MAX_EDIT_OUTPUT_BYTES;
use super::mutation::apply_file_change;
use super::write::unified_diff;

/// Refuse a sweep wider than this many files: past it, the model is almost
/// certainly matching something it didn't mean to, and a diff that large is
/// unreviewable anyway.
const MAX_FILES: usize = 200;

/// Files above this size are skipped — they're generated or vendored, and a
/// substitution across them is never what was intended.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct ReplaceTool;

#[derive(Deserialize)]
struct ReplaceArgs {
    find: String,
    replace: String,
    /// Restrict to paths matching this glob (e.g. `src/**/*.rs`).
    #[serde(default)]
    glob: Option<String>,
    /// Directory to search under; defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
    /// Treat `find` as a regular expression (captures usable as `$1` in
    /// `replace`). Default false: a literal string.
    #[serde(default)]
    regex: bool,
    /// Report what would change, write nothing. Default false.
    #[serde(default)]
    dry_run: bool,
}

#[async_trait]
impl Tool for ReplaceTool {
    fn name(&self) -> &'static str {
        "replace"
    }
    fn description(&self) -> &'static str {
        "Replace text across many files at once — a project-wide textual substitution. To \
         rename a *code symbol*, prefer the `rename` tool instead: it's scope-aware via the \
         language server, where a textual replace also hits comments, strings, and \
         substrings of unrelated names. `find` is a literal string unless `regex` is true. \
         Narrow the sweep with `glob` (e.g. \"src/**/*.rs\") and/or `path`. Files over 2 MiB \
         are skipped. Returns a unified diff of every file changed. Use `dry_run: true` to \
         preview first. Prefer this over `bash sed -i`: it shows you exactly what it changed."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "find": {"type": "string", "description": "Text to find. A literal string unless `regex` is true."},
                "replace": {"type": "string", "description": "Replacement text. With `regex`, $1/$2 refer to capture groups; brace them (${1}) when a letter, digit or underscore follows, or the group name swallows it."},
                "glob": {"type": "string", "description": "Only files matching this glob, e.g. \"src/**/*.rs\"."},
                "path": {"type": "string", "description": "Directory to search under. Defaults to the working directory."},
                "regex": {"type": "boolean", "description": "Treat `find` as a regular expression. Default false."},
                "dry_run": {"type": "boolean", "description": "Report the diff without writing. Default false."}
            },
            "required": ["find", "replace"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ReplaceArgs = crate::tool_args("replace", args)?;
        if a.find.is_empty() {
            bail!("`find` is empty — that would match at every position in every file");
        }
        let root = match &a.path {
            Some(p) => ctx.resolve(p),
            None => ctx.cwd.clone(),
        };

        let re = if a.regex {
            regex::Regex::new(&a.find).with_context(|| format!("invalid regex: {}", a.find))?
        } else {
            regex::Regex::new(&regex::escape(&a.find)).expect("an escaped literal is valid")
        };
        let pattern = a
            .glob
            .as_deref()
            .map(|g| glob::Pattern::new(g).with_context(|| format!("invalid glob: {g}")))
            .transpose()?;

        let (candidates, oversized) = collect_files(&root, pattern.as_ref(), ctx)?;

        // Phase 1 — plan. Every file the sweep would rewrite is checked before
        // any of them is written, so a file this agent may not touch aborts the
        // whole sweep rather than leaving it half applied. `MAX_FILES` bounds
        // the files that actually *match* — not every candidate the walk
        // turns up — so a large repo with few hits still succeeds. The diff is
        // *not* built here for a real run: a post-edit hook can rewrite the
        // file again, and the diff must reflect what actually lands on disk —
        // see phase 2.
        let mut planned = Vec::new();
        let mut total = 0usize;
        for path in candidates {
            let Ok(before) = tokio::fs::read_to_string(&path).await else {
                continue; // binary or unreadable: not ours to rewrite
            };
            let hits = re.find_iter(&before).count();
            if hits == 0 {
                continue;
            }
            if planned.len() >= MAX_FILES {
                bail!(
                    "more than {MAX_FILES} files contain {:?} — narrow the sweep with `glob` \
                     or `path`",
                    a.find
                );
            }
            // Only now is the file a mutation target, so only now must it satisfy
            // this agent's extension allow-list.
            //
            // Bound output size before it can OOM: `find="e"`, `replace=50KB`
            // could expand even a single sub-2 MB file into gigabytes. The two
            // modes are bounded differently because only one admits an exact
            // pre-projection:
            //   * LITERAL — each hit grows the output by exactly
            //     `replace.len() - find.len()`, so the projection below is exact
            //     and can refuse before allocating anything.
            //   * REGEX — the template's capture references (`$1`, `${name}`,
            //     `$0`) expand to matched text of unknown size, so no pre-hoc
            //     estimate off `replace.len()` is safe (it under-counts and would
            //     let a `$1$1$1…` template OOM). It is bounded *incrementally*
            //     while the output is built (`bounded_regex_replace`), aborting
            //     the moment the real output crosses the ceiling.
            let after = if a.regex {
                match bounded_regex_replace(&re, &a.replace, &before, MAX_EDIT_OUTPUT_BYTES) {
                    Ok(after) => after,
                    Err(len) => bail!(
                        "replacing {:?} in {} would produce ~{len}+ bytes; narrow `find` or \
                         the sweep",
                        a.find,
                        path.strip_prefix(&ctx.cwd).unwrap_or(&path).display()
                    ),
                }
            } else {
                if a.replace.len() > a.find.len() {
                    let projected = before
                        .len()
                        .saturating_add(hits.saturating_mul(a.replace.len() - a.find.len()));
                    if projected > MAX_EDIT_OUTPUT_BYTES {
                        bail!(
                            "replacing {:?} in {} would produce ~{projected} bytes; narrow `find` \
                             or the sweep",
                            a.find,
                            path.strip_prefix(&ctx.cwd).unwrap_or(&path).display()
                        );
                    }
                }
                before.replace(&a.find, &a.replace)
            };
            if after == before {
                continue;
            }
            total += hits;
            let rel = path
                .strip_prefix(&ctx.cwd)
                .unwrap_or(&path)
                .display()
                .to_string();
            planned.push((path, before, after, rel));
        }

        // Phase 2 — write. For a real run, the diff and hook/diagnostic notes
        // are taken from `apply_file_change`'s return: the post-hook content
        // actually written to disk, not the in-memory substitution — a
        // formatter hook can rewrite the file again after this tool's own
        // write.
        let mut changed = Vec::new();
        let mut diffs = String::new();
        let mut notes = String::new();
        for (path, before, after, rel) in planned {
            if a.dry_run {
                diffs.push_str(&unified_diff(&rel, &before, &after));
            } else {
                let fc = apply_file_change(ctx, &path, "replace", &after).await?;
                ctx.mark_read(&path);
                for note in &fc.notes {
                    notes.push_str(&format!("[{rel}] {note}\n"));
                }
                diffs.push_str(&unified_diff(&rel, &before, &fc.content_after));
            }
            changed.push(rel);
        }

        let skip_note = (!oversized.is_empty()).then(|| {
            format!(
                "{} file{} over 2 MiB skipped: {}",
                oversized.len(),
                if oversized.len() == 1 { "" } else { "s" },
                oversized.join(", ")
            )
        });

        if changed.is_empty() {
            let mut out = format!("No file contains {:?} — nothing changed.", a.find);
            if let Some(note) = &skip_note {
                out.push('\n');
                out.push_str(note);
            }
            return Ok(out);
        }
        let verb = if a.dry_run {
            "Would replace"
        } else {
            "Replaced"
        };
        let mut header = format!(
            "{verb} {total} occurrence{} across {} file{}:\n{}",
            if total == 1 { "" } else { "s" },
            changed.len(),
            if changed.len() == 1 { "" } else { "s" },
            changed.join("\n")
        );
        // Notes (formatter-hook failures, build-breaking LSP diagnostics) go
        // right after the file list and before the diffs — a long diff must
        // not bury a "this now fails to build" warning.
        if !notes.is_empty() {
            header.push('\n');
            header.push_str(notes.trim_end_matches('\n'));
        }
        // A file over MAX_FILE_BYTES is silently absent from every count above
        // (it never became a candidate) — call that out explicitly, or a
        // sweep that missed a large file looks identical to one that found
        // no match in it.
        if let Some(note) = &skip_note {
            header.push('\n');
            header.push_str(note);
        }
        Ok(truncate(&format!("{header}\n\n{diffs}"), ctx.max_output))
    }
}

/// Every text-sized, non-secret file under `root` that `pattern` admits,
/// honouring `.gitignore` — the same walker `find` uses, so the two agree on
/// what "the project" is. This is the sweep's *candidate* set, before any
/// content is inspected — [`MAX_FILES`] is enforced against files that
/// actually match `find` (see the caller), not against how many candidates
/// this turns up, so a large repo with few hits still succeeds.
///
/// The second element is the project-relative paths of files that were
/// skipped for being over [`MAX_FILE_BYTES`] — never inspected, so a
/// substitution that should have landed there is silently absent unless the
/// caller reports this list back.
fn collect_files(
    root: &std::path::Path,
    pattern: Option<&glob::Pattern>,
    ctx: &ToolContext,
) -> Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    let mut out = Vec::new();
    let mut oversized = Vec::new();
    for entry in ignore::WalkBuilder::new(root).hidden(false).build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        // Never a rewrite (or diff-disclosure) target: mirrors the `read`/
        // `grep` deny-list so a broad `replace` can't touch a `.env` etc.
        if crate::secret_file_reason(&crate::canonicalize_nearest(&path)).is_some() {
            continue;
        }
        if let Some(p) = pattern {
            // Match the project-relative path, so `src/**/*.rs` means what it
            // looks like rather than depending on the absolute prefix.
            let rel = path.strip_prefix(&ctx.cwd).unwrap_or(&path);
            if !p.matches_path(rel) {
                continue;
            }
        }
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            let rel = path.strip_prefix(&ctx.cwd).unwrap_or(&path);
            oversized.push(rel.display().to_string());
            continue;
        }
        out.push(path);
    }
    out.sort();
    oversized.sort();
    Ok((out, oversized))
}

/// [`regex::Regex::replace_all`] with an output ceiling, so a template whose
/// capture references (`$1`, `${name}`, `$0`) expand the matched text can't
/// drive an unbounded allocation and OOM the process.
///
/// Byte-for-byte identical to `replace_all` for the in-bounds case: matches are
/// taken non-overlapping and left-to-right, the gap before each match is copied
/// verbatim, and the template is expanded by the regex crate's own
/// [`regex::Captures::expand`] — the very `$`-expansion `replace_all` uses. The
/// only added behaviour is the ceiling: as soon as the accumulated output
/// passes `cap` this returns `Err(len)` with the size reached so far (a lower
/// bound on the true output) instead of finishing the allocation. The check
/// runs after each append, so the buffer never grows more than one match's
/// expansion past `cap`.
fn bounded_regex_replace(
    re: &regex::Regex,
    template: &str,
    input: &str,
    cap: usize,
) -> std::result::Result<String, usize> {
    let mut out = String::new();
    let mut last_end = 0;
    for caps in re.captures_iter(input) {
        let m = caps.get(0).expect("group 0 always participates in a match");
        out.push_str(&input[last_end..m.start()]);
        caps.expand(template, &mut out);
        last_end = m.end();
        if out.len() > cap {
            return Err(out.len());
        }
    }
    out.push_str(&input[last_end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn write(path: &std::path::Path, body: &str) {
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    async fn read(path: &std::path::Path) -> String {
        tokio::fs::read_to_string(path).await.unwrap()
    }

    #[tokio::test]
    async fn replaces_a_literal_across_files_and_reports_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(
            &dir.path().join("a.rs"),
            "let old_name = 1;\nold_name + 1\n",
        )
        .await;
        write(&dir.path().join("b.rs"), "no match here\n").await;
        write(&dir.path().join("sub/c.rs"), "old_name()\n").await;

        let out = ReplaceTool
            .execute(json!({"find": "old_name", "replace": "new_name"}), &ctx)
            .await
            .unwrap();

        assert!(
            out.contains("Replaced 3 occurrences across 2 files"),
            "{out}"
        );
        assert!(out.contains("-let old_name = 1;"), "shows a diff:\n{out}");
        assert!(out.contains("+let new_name = 1;"), "{out}");
        assert_eq!(
            read(&dir.path().join("a.rs")).await,
            "let new_name = 1;\nnew_name + 1\n"
        );
        assert_eq!(read(&dir.path().join("sub/c.rs")).await, "new_name()\n");
        assert_eq!(
            read(&dir.path().join("b.rs")).await,
            "no match here\n",
            "untouched"
        );
    }

    /// A literal `find` is not a regex: metacharacters match themselves.
    #[tokio::test]
    async fn find_is_literal_unless_regex_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "a.c and abc\n").await;

        ReplaceTool
            .execute(json!({"find": "a.c", "replace": "X"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "X and abc\n",
            "`.` was literal"
        );

        // As a regex, `.` matches any character — and captures work. `${1}` is
        // braced because a bare `$1_v2` would name the group `1_v2`, which does
        // not exist, and expand to nothing.
        write(&dir.path().join("b.txt"), "fn foo() {}\n").await;
        ReplaceTool
            .execute(json!({"find": r"fn (\w+)\(", "replace": "fn ${1}_v2(", "regex": true, "glob": "b.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read(&dir.path().join("b.txt")).await, "fn foo_v2() {}\n");
    }

    #[tokio::test]
    async fn dry_run_reports_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(
                json!({"find": "old", "replace": "new", "dry_run": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.starts_with("Would replace 1 occurrence across 1 file"),
            "{out}"
        );
        assert!(out.contains("+new"), "the diff is still shown:\n{out}");
        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "old\n",
            "nothing written"
        );
    }

    #[tokio::test]
    async fn a_glob_narrows_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("src/a.rs"), "x\n").await;
        write(&dir.path().join("docs/a.md"), "x\n").await;

        ReplaceTool
            .execute(
                json!({"find": "x", "replace": "y", "glob": "src/**/*.rs"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(read(&dir.path().join("src/a.rs")).await, "y\n");
        assert_eq!(
            read(&dir.path().join("docs/a.md")).await,
            "x\n",
            "outside the glob"
        );
    }

    #[tokio::test]
    async fn an_empty_find_and_a_bad_regex_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = ReplaceTool
            .execute(json!({"find": "", "replace": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");

        let err = ReplaceTool
            .execute(
                json!({"find": "(unclosed", "replace": "x", "regex": true}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"), "{err}");
    }

    #[tokio::test]
    async fn no_match_is_reported_not_silently_successful() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "hello\n").await;
        let out = ReplaceTool
            .execute(json!({"find": "absent", "replace": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("No file contains"), "{out}");
    }

    /// A `.env` (or other secret file) is never rewritten, even when it
    /// contains the search string — and its content never appears in the
    /// diff/summary either, mirroring the `read`/`grep` deny-list.
    #[tokio::test]
    async fn secret_files_are_never_rewritten_or_disclosed() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".env"), "API_KEY=old_name\n").await;
        write(&dir.path().join("a.txt"), "old_name\n").await;
        let ctx = ToolContext::new(dir.path());

        let out = ReplaceTool
            .execute(json!({"find": "old_name", "replace": "new_name"}), &ctx)
            .await
            .unwrap();

        assert!(out.contains("across 1 file"), "{out}");
        assert!(!out.contains("API_KEY"), "secret content leaked:\n{out}");
        assert!(!out.contains(".env"), "secret path named:\n{out}");
        assert_eq!(
            read(&dir.path().join(".env")).await,
            "API_KEY=old_name\n",
            "the secret file must be untouched"
        );
        assert_eq!(read(&dir.path().join("a.txt")).await, "new_name\n");
    }

    /// A file over `MAX_FILE_BYTES` is never inspected, so a sweep that would
    /// otherwise have matched inside it must say so — not just silently
    /// report success on the files it did touch.
    #[tokio::test]
    async fn oversized_files_are_skipped_and_named_in_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());

        // Over MAX_FILE_BYTES (2 MiB), and it contains the pattern — but must
        // never be touched or counted.
        let mut big = String::with_capacity(3 * 1024 * 1024);
        big.push_str("needle\n");
        while big.len() < 3 * 1024 * 1024 {
            big.push_str("filler filler filler filler filler filler filler filler\n");
        }
        write(&dir.path().join("big.txt"), &big).await;
        write(&dir.path().join("small.txt"), "needle\n").await;

        let out = ReplaceTool
            .execute(json!({"find": "needle", "replace": "found"}), &ctx)
            .await
            .unwrap();

        assert!(out.contains("across 1 file"), "{out}");
        assert!(
            out.contains("1 file over 2 MiB skipped: big.txt"),
            "the skip note names the file:\n{out}"
        );
        assert_eq!(read(&dir.path().join("small.txt")).await, "found\n");
        assert!(
            read(&dir.path().join("big.txt"))
                .await
                .starts_with("needle\n"),
            "the oversized file must be untouched"
        );
    }

    /// The same skip note appears even when nothing else matched — otherwise
    /// "no match" looks identical to "the only match was in a skipped file".
    #[tokio::test]
    async fn oversized_skip_note_appears_even_with_no_other_matches() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());

        let mut big = String::with_capacity(3 * 1024 * 1024);
        big.push_str("needle\n");
        while big.len() < 3 * 1024 * 1024 {
            big.push_str("filler filler filler filler filler filler filler filler\n");
        }
        write(&dir.path().join("big.txt"), &big).await;

        let out = ReplaceTool
            .execute(json!({"find": "needle", "replace": "found"}), &ctx)
            .await
            .unwrap();

        assert!(out.starts_with("No file contains"), "{out}");
        assert!(out.contains("1 file over 2 MiB skipped: big.txt"), "{out}");
    }

    /// `MAX_FILES` bounds the files that actually *match* `find`, not every
    /// candidate the walk turns up — a repo with far more than `MAX_FILES`
    /// files but only a few hits must still succeed.
    #[tokio::test]
    async fn max_files_counts_matches_not_candidates() {
        let dir = tempfile::tempdir().unwrap();
        // Many more candidate files than MAX_FILES, none containing `find`.
        for i in 0..(MAX_FILES + 50) {
            write(&dir.path().join(format!("f{i}.txt")), "nothing here\n").await;
        }
        // A single file that actually matches.
        write(&dir.path().join("hit.txt"), "needle\n").await;
        let ctx = ToolContext::new(dir.path());

        let out = ReplaceTool
            .execute(json!({"find": "needle", "replace": "found"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("across 1 file"), "{out}");
        assert_eq!(read(&dir.path().join("hit.txt")).await, "found\n");
    }

    /// A post-edit hook that further rewrites the file is reflected in the
    /// diff `replace` reports — the diff must show what actually landed on
    /// disk, not the tool's own in-memory substitution.
    #[cfg(unix)]
    #[tokio::test]
    async fn diff_reflects_post_hook_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "replace".to_string(),
            glob: None,
            run: "printf 'hooked\\n' >> {path}".to_string(),
            timeout_ms: crate::DEFAULT_HOOK_TIMEOUT_MS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(json!({"find": "old", "replace": "new"}), &ctx)
            .await
            .unwrap();

        assert_eq!(read(&dir.path().join("a.txt")).await, "new\nhooked\n");
        assert!(
            out.contains("+hooked"),
            "diff must show the post-hook content:\n{out}"
        );
    }

    /// A hook that fails is surfaced in the result, tagged with the file it
    /// belongs to — a project-wide rename that breaks the build must not
    /// report bare success.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_hook_note_is_surfaced_and_tagged_with_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "replace".to_string(),
            glob: None,
            run: "exit 7".to_string(),
            timeout_ms: crate::DEFAULT_HOOK_TIMEOUT_MS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(json!({"find": "old", "replace": "new"}), &ctx)
            .await
            .unwrap();

        assert!(
            out.contains("[a.txt] [hook `exit 7` failed"),
            "note must be tagged with its file:\n{out}"
        );
        // Placed before the diff section, not buried under it.
        let note_pos = out.find("[a.txt] [hook").unwrap();
        let diff_pos = out.find("--- a/a.txt").unwrap();
        assert!(note_pos < diff_pos, "note must precede the diff:\n{out}");
        // The file was still written despite the hook failing.
        assert_eq!(read(&dir.path().join("a.txt")).await, "new\n");
    }

    /// `dry_run` still shows the `before -> after` diff computed in memory,
    /// and runs no hooks at all — nothing is written, so there's nothing for
    /// a hook to fire on and no notes to report.
    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_shows_the_in_memory_diff_and_runs_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "replace".to_string(),
            glob: None,
            run: "printf 'hooked\\n' >> {path}".to_string(),
            timeout_ms: crate::DEFAULT_HOOK_TIMEOUT_MS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(
                json!({"find": "old", "replace": "new", "dry_run": true}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            out.starts_with("Would replace 1 occurrence across 1 file"),
            "{out}"
        );
        assert!(out.contains("-old"), "{out}");
        assert!(out.contains("+new"), "{out}");
        assert!(!out.contains("hooked"), "no hook note or effect: {out}");
        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "old\n",
            "nothing written, so the hook never ran"
        );
    }

    /// A regex template that repeats a capture (`$1$1…`) expands each match far
    /// beyond `replace.len()`, so the literal projection would under-count it.
    /// The incremental ceiling must trip on the *real* output size and stop
    /// early rather than materialising the whole blown-up string: here a full
    /// run would be ~100 MB but `cap` is 1 KiB, and the reported size is only
    /// one match's expansion past the cap — proof the walk aborted mid-stream
    /// instead of allocating the full output.
    #[test]
    fn bounded_regex_replace_aborts_early_on_capture_expansion() {
        let re = regex::Regex::new("(a)").unwrap();
        // 100_000 single-char matches, each expanding to 1_000 bytes → ~100 MB
        // if run to completion.
        let template = "$1".repeat(1_000);
        let input = "a".repeat(100_000);
        let cap = 1024;
        let err = bounded_regex_replace(&re, &template, &input, cap).unwrap_err();
        assert!(err > cap, "must report a size past the ceiling: {err}");
        assert!(
            err < cap + 2_000,
            "aborted a hair past the cap, not after the full ~100 MB blow-up: {err}"
        );
    }

    /// The bounded path is byte-for-byte identical to `replace_all` for a normal
    /// in-bounds replacement with a capture reference — the ceiling only changes
    /// behaviour when it is actually crossed. A no-match input round-trips too.
    #[test]
    fn bounded_regex_replace_matches_replace_all() {
        let re = regex::Regex::new(r"(\w+)").unwrap();
        let input = "foo bar_baz qux\nlonger line with words\n";
        // `${1}` is braced: a bare `$1_x` would name group `1_x` (nonexistent)
        // and expand to nothing — the exact gotcha `replace_all` also has.
        let template = "${1}_x";
        let expected = re.replace_all(input, template).into_owned();
        let got = bounded_regex_replace(&re, template, input, MAX_EDIT_OUTPUT_BYTES).unwrap();
        assert_eq!(got, expected);
        assert_eq!(
            got,
            "foo_x bar_baz_x qux_x\nlonger_x line_x with_x words_x\n"
        );

        let none = "!!! ??? ...";
        assert_eq!(
            bounded_regex_replace(&re, template, none, MAX_EDIT_OUTPUT_BYTES).unwrap(),
            re.replace_all(none, template).into_owned(),
            "a no-match input is returned unchanged, like replace_all"
        );
    }

    /// End-to-end: a regex replace whose template repeats a capture is refused
    /// with the size error rather than being allowed to OOM. The literal
    /// projection can't see the expansion (it only knows `replace.len()`), so
    /// this exercises the incremental ceiling wired into the tool — the full run
    /// would be ~2 GB, but the tool bails once the real output crosses
    /// `MAX_EDIT_OUTPUT_BYTES`, and leaves the file untouched.
    #[tokio::test]
    async fn regex_capture_expansion_is_refused_with_the_size_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        // Just under MAX_FILE_BYTES (2 MiB) so it is not skipped as oversized.
        let input = "a".repeat(2_000_000);
        write(&dir.path().join("big.txt"), &input).await;

        let err = ReplaceTool
            .execute(
                json!({"find": "(a)", "replace": "$1".repeat(1_000), "regex": true}),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("would produce"), "{msg}");
        assert!(msg.contains("+ bytes"), "reports a lower-bound size: {msg}");
        assert!(msg.contains("narrow `find`"), "{msg}");
        assert_eq!(
            read(&dir.path().join("big.txt")).await,
            input,
            "the file must be left exactly as it was — the sweep aborted"
        );
    }
}
