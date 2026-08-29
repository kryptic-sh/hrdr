//! `replace`: one substitution applied across many files, under the guards.
//!
//! The alternative a model reaches for is `bash sed -i`, which is the single
//! worst mutation path available to it: silent about what it changed — a bad
//! regex corrupts the tree and the model reports success.
//!
//! This tool walks the project respecting `.gitignore`, matches `pattern` as a
//! **regex** by default (`literal: true` for exact text) — the same matching
//! shape as `grep`, so a pattern that worked there means the same thing here —
//! and returns a unified diff per file so the change is visible in the
//! transcript. `dry_run: true` reports what *would* change without writing.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

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
    /// A regular expression (captures usable as `$1` in `replace`) unless
    /// `literal` is set — the same shape as `grep`'s `pattern`.
    pattern: String,
    replace: String,
    /// Restrict to paths matching this glob (e.g. `src/**/*.rs`).
    #[serde(default)]
    glob: Option<String>,
    /// Directory to search under; defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
    /// Treat `pattern` as a fixed string rather than a regex — and the
    /// replacement as fixed text, with no `$1` expansion. Default false.
    #[serde(default)]
    literal: bool,
    /// Report what would change, write nothing. Default false.
    #[serde(default)]
    dry_run: bool,
}

/// The pre-1.0 rename of `find` → `pattern` inverted the matching polarity as
/// well (literal-by-default → regex-by-default), so silently accepting the old
/// field would flip what a call *means* rather than merely failing: `a.b` used
/// to match a dot, and now matches any character. Both dead fields are rejected
/// with the shape they became.
fn reject_removed_args(args: &serde_json::Value) -> Result<()> {
    let Some(obj) = args.as_object() else {
        return Ok(());
    };
    if obj.contains_key("find") {
        bail!("`find` is now `pattern` (a regex by default; `literal: true` for exact text)");
    }
    if obj.contains_key("regex") {
        bail!(
            "`regex` is gone — patterns are regex by default; use `literal: true` for exact text"
        );
    }
    Ok(())
}

/// Does this look like someone meant exact text but wrote it into a regex
/// field? Used only to decide whether to append the `literal: true` nudge to an
/// error or a no-match report — never to change matching.
fn has_regex_metachars(s: &str) -> bool {
    s.contains([
        '.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\',
    ])
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
         substrings of unrelated names. Same matching shape as `grep`: `pattern` is a regex \
         unless `literal: true` — set literal for exact text containing `.` `(` `*` etc. \
         `replace` may use `$1` capture groups unless `literal`. Narrow the sweep with `glob` \
         (e.g. \"src/**/*.rs\") and/or `path`. Files over 2 MiB are skipped. Returns a unified \
         diff of every file changed. Use `dry_run: true` to preview first. Prefer this over \
         `bash sed -i`: it shows you exactly what it changed."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Text to match — same matching shape as `grep`: a regex unless `literal` is true. Set `literal` for exact text containing `.` `(` `*` etc."},
                "replace": {"type": "string", "description": "Replacement text. May use $1/$2 capture groups unless `literal` is true; brace them (${1}) when a letter, digit or underscore follows, or the group name swallows it. With `literal`, it is inserted verbatim ($1 stays $1)."},
                "glob": {"type": "string", "default": null, "description": "Only files matching this glob, e.g. \"src/**/*.rs\"."},
                "path": {"type": "string", "default": ".", "description": "Directory to search under. Defaults to the working directory."},
                "literal": {"type": "boolean", "default": false, "description": "Treat `pattern` as a fixed string, not a regex — use for exact text like 'foo(bar)', 'a.b', '$var'. Also disables $1 expansion in `replace`. Default false."},
                "dry_run": {"type": "boolean", "default": false, "description": "Report the diff without writing. Default false."}
            },
            "required": ["pattern", "replace"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        reject_removed_args(&args)?;
        let a: ReplaceArgs = crate::tool_args("replace", args)?;
        if a.pattern.is_empty() {
            bail!("`pattern` is empty — that would match at every position in every file");
        }
        // `replace` rewrites every matching file under `root`, so the scope
        // argument is a write, not a read: a root outside the writable set is
        // refused before the sweep collects a single candidate.
        let root = match &a.path {
            Some(p) => ctx.resolve_write(p)?,
            None => ctx.cwd.clone(),
        };

        let re = if a.literal {
            regex::Regex::new(&regex::escape(&a.pattern)).expect("an escaped literal is valid")
        } else {
            // A literal-intent string (`foo.bar(x)`) is now compiled as a regex,
            // so the most likely cause of a compile failure is exactly that
            // mistake — say so in the error rather than leaving the model to
            // debug a regex it never meant to write.
            regex::Regex::new(&a.pattern).with_context(|| {
                format!(
                    "invalid regex: {} — if you meant exact text, pass `literal: true`",
                    a.pattern
                )
            })?
        };
        // Named for what it is — `pattern` alone now means the *match* pattern.
        let glob_pattern = a
            .glob
            .as_deref()
            .map(|g| glob::Pattern::new(g).with_context(|| format!("invalid glob: {g}")))
            .transpose()?;

        let (candidates, oversized) = collect_files(&root, glob_pattern.as_ref(), ctx)?;

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
                    "more than {MAX_FILES} files match {:?} — narrow the sweep with `glob` \
                     or `path`",
                    a.pattern
                );
            }
            // Only now is the file a mutation target, so only now must it satisfy
            // this agent's extension allow-list.
            //
            // Bound output size before it can OOM: `pattern="e"`, `replace=50KB`
            // could expand even a single sub-2 MB file into gigabytes. The two
            // modes are bounded differently because only one admits an exact
            // pre-projection:
            //   * LITERAL — each hit grows the output by exactly
            //     `replace.len() - pattern.len()`, so the projection below is
            //     exact and can refuse before allocating anything.
            //   * REGEX — the template's capture references (`$1`, `${name}`,
            //     `$0`) expand to matched text of unknown size, so no pre-hoc
            //     estimate off `replace.len()` is safe (it under-counts and would
            //     let a `$1$1$1…` template OOM). It is bounded *incrementally*
            //     while the output is built (`bounded_regex_replace`), aborting
            //     the moment the real output crosses the ceiling.
            let after = if a.literal {
                if a.replace.len() > a.pattern.len() {
                    let projected = before
                        .len()
                        .saturating_add(hits.saturating_mul(a.replace.len() - a.pattern.len()));
                    if projected > MAX_EDIT_OUTPUT_BYTES {
                        bail!(
                            "replacing {:?} in {} would produce ~{projected} bytes; narrow \
                             `pattern` or the sweep",
                            a.pattern,
                            super::rel_display(&path, &ctx.cwd)
                        );
                    }
                }
                before.replace(&a.pattern, &a.replace)
            } else {
                match bounded_regex_replace(&re, &a.replace, &before, MAX_EDIT_OUTPUT_BYTES) {
                    Ok(after) => after,
                    Err(len) => bail!(
                        "replacing {:?} in {} would produce ~{len}+ bytes; narrow `pattern` or \
                         the sweep",
                        a.pattern,
                        super::rel_display(&path, &ctx.cwd)
                    ),
                }
            };
            if after == before {
                continue;
            }
            total += hits;
            let rel = super::rel_display(&path, &ctx.cwd).to_string();
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
                // Refresh the read baseline for every file this rewrote: the diff
                // below shows the model the post-hook content, so it *has* seen
                // the current file and a following `edit`/`write` must not be
                // refused as stale (the guard exists for content the model hasn't
                // seen). Recorded after the hooks, so the signature is the one on
                // disk, not this tool's pre-hook substitution.
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
            let mut out = format!("No file matches {:?} — nothing changed.", a.pattern);
            // A pattern full of metacharacters that compiled but matched nothing
            // is the signature of literal intent written into a regex field
            // (`foo.bar(x)` compiles fine, and matches nothing that isn't also
            // regex-shaped) — so nudge, but only when there is something to
            // nudge about.
            if !a.literal && has_regex_metachars(&a.pattern) {
                out.push_str(" If you meant exact text, pass `literal: true`.");
            }
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
        // The full diffs ride back uncapped — they are what the transcript
        // shows the user; the agent abbreviates the model's copy.
        Ok(format!("{header}\n\n{diffs}"))
    }
}

/// Every text-sized, non-secret file under `root` that `glob_pattern` admits,
/// honouring `.gitignore` — the same walker `find` uses, so the two agree on
/// what "the project" is. This is the sweep's *candidate* set, before any
/// content is inspected — [`MAX_FILES`] is enforced against files that
/// actually match `pattern` (see the caller), not against how many candidates
/// this turns up, so a large repo with few hits still succeeds.
///
/// The second element is the project-relative paths of files that were
/// skipped for being over [`MAX_FILE_BYTES`] — never inspected, so a
/// substitution that should have landed there is silently absent unless the
/// caller reports this list back.
fn collect_files(
    root: &std::path::Path,
    glob_pattern: Option<&glob::Pattern>,
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
        // `.git` is never a rewrite (or diff-disclosure) target: the walker's
        // `hidden(false)` descends into it, and the deny-list has no `.git` arm,
        // so without this the sweep could rewrite `refs/heads/main` (a 40-hex
        // SHA matches a broad literal like `a` with near-certainty), `config`
        // (remote URLs), `packed-refs` or the hooks — corrupting the repo and
        // diffing its metadata into the transcript. `find`/`grep`/`tree` stay
        // out by skipping dotfiles entirely; `replace` must keep walking
        // dotfiles (a sweep may target `.github/`), so it skips `.git` alone.
        if path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        // Never a rewrite (or diff-disclosure) target: mirrors the `read`/
        // `grep` deny-list so a broad `replace` can't touch a `.env` etc.
        if crate::secret_file_reason(&crate::canonicalize_nearest(&path)).is_some() {
            continue;
        }
        if let Some(p) = glob_pattern {
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
/// only added behaviour is the ceiling: this returns `Err(len)` (a lower bound
/// on the true output) instead of finishing the allocation once the output
/// would pass `cap`.
///
/// The ceiling is checked BEFORE each `expand`, against a per-match upper bound —
/// the gap plus the template length plus (number of `$` references × this
/// match's length), since each `$N` reference expands to at most the whole
/// match. Bounding pre-expansion is what makes a *single* pathological match
/// safe: a lone `(a+)` over 2 MiB with a template repeating `$1` thousands of
/// times would expand to gigabytes in one `expand` call, so a post-expand check
/// would already have allocated it. Here that match is refused before `expand`
/// runs, so `out` never grows more than one bounded gap past `cap`.
fn bounded_regex_replace(
    re: &regex::Regex,
    template: &str,
    input: &str,
    cap: usize,
) -> std::result::Result<String, usize> {
    // Conservative count of expansion sites: every `$` could begin a capture
    // reference. `$$` (a literal `$`) is counted too — that only over-estimates,
    // which is safe (it can never let an over-cap expansion through).
    let refs = template.matches('$').count();
    let mut out = String::new();
    let mut last_end = 0;
    for caps in re.captures_iter(input) {
        let m = caps.get(0).expect("group 0 always participates in a match");
        let gap = m.start() - last_end;
        let match_len = m.end() - m.start();
        // Upper bound on what this iteration appends, computed without expanding.
        let projected = out
            .len()
            .saturating_add(gap)
            .saturating_add(template.len())
            .saturating_add(refs.saturating_mul(match_len));
        if projected > cap {
            return Err(projected);
        }
        out.push_str(&input[last_end..m.start()]);
        caps.expand(template, &mut out);
        last_end = m.end();
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

    /// `.git` metadata is never a rewrite target: the walker's `hidden(false)`
    /// descends into it, and without the `.git`-component skip a broad literal
    /// like `a` matches the 40-hex SHA in `refs/heads/main` (p ≈ 92%) and
    /// corrupts the repo. The sweep must leave `.git` files byte-identical
    /// while still walking dotfiles (a `.github/` file below is rewritten).
    #[tokio::test]
    async fn replace_never_rewrites_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        // A real-ish repo head ref, containing `a` many times.
        let head = dir.path().join(".git/refs/heads/main");
        write(&head, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n").await;
        // A dotfile outside `.git` that the sweep SHOULD rewrite.
        write(&dir.path().join(".github/workflows/ci.yml"), "on: a\n").await;

        let out = ReplaceTool
            .execute(
                json!({"pattern": "a", "replace": "b", "literal": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.contains(".github"),
            "dotfiles outside .git still swept:\n{out}"
        );
        assert!(
            !out.lines()
                .any(|l| l.starts_with("--- a/.git/") || l.starts_with("+++ b/.git/")),
            "no .git file appears in the diff:\n{out}"
        );
        assert_eq!(
            read(&head).await,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "the branch ref must survive a sweeping `a`→`b` replace"
        );
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
            .execute(json!({"pattern":"old_name", "replace": "new_name"}), &ctx)
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

    /// `pattern` is a regex by default — the same shape as `grep` — so
    /// metacharacters are metacharacters and `$1` in `replace` expands a
    /// capture, with no flag asked for.
    #[tokio::test]
    async fn pattern_is_a_regex_by_default_with_capture_groups() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "a.c and abc\n").await;

        ReplaceTool
            .execute(
                json!({"pattern": "a.c", "replace": "X", "glob": "a.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "X and X\n",
            "`.` matched any character — regex is the default"
        );

        // Captures work without any flag. `${1}` is braced because a bare
        // `$1_v2` would name the group `1_v2`, which does not exist, and expand
        // to nothing.
        write(&dir.path().join("b.txt"), "fn foo() {}\n").await;
        ReplaceTool
            .execute(
                json!({"pattern": r"fn (\w+)\(", "replace": "fn ${1}_v2(", "glob": "b.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(read(&dir.path().join("b.txt")).await, "fn foo_v2() {}\n");
    }

    /// `literal: true` opts out of regex on both sides: metacharacters in
    /// `pattern` match themselves, and `$1` in `replace` is inserted verbatim
    /// rather than expanding a capture that doesn't exist.
    #[tokio::test]
    async fn literal_true_matches_exact_text_and_inserts_the_replacement_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "a.c and abc\n").await;

        ReplaceTool
            .execute(
                json!({"pattern": "a.c", "replace": "X", "literal": true, "glob": "a.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "X and abc\n",
            "`.` was literal"
        );

        // Exact text full of metacharacters — the case that would otherwise be
        // a regex compile error or a wrong match.
        write(&dir.path().join("b.txt"), "foo.bar(x) + 1\n").await;
        ReplaceTool
            .execute(
                json!({"pattern": "foo.bar(x)", "replace": "baz($1)", "literal": true, "glob": "b.txt"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            read(&dir.path().join("b.txt")).await,
            "baz($1) + 1\n",
            "`$1` is verbatim under `literal`, not a capture reference"
        );
    }

    /// The dead pre-1.0 fields are rejected, not silently ignored: accepting
    /// them would flip a call's meaning (literal-by-default → regex-by-default)
    /// rather than fail.
    #[tokio::test]
    async fn the_old_find_and_regex_fields_are_refused_with_the_new_shape() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "old\n").await;

        let err = ReplaceTool
            .execute(json!({"find": "old", "replace": "new"}), &ctx)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`find` is now `pattern`"), "{msg}");
        assert!(msg.contains("literal: true"), "{msg}");

        let err = ReplaceTool
            .execute(
                json!({"pattern": "old", "replace": "new", "regex": true}),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`regex` is gone"), "{msg}");
        assert!(msg.contains("literal: true"), "{msg}");

        assert_eq!(
            read(&dir.path().join("a.txt")).await,
            "old\n",
            "a rejected call writes nothing"
        );
    }

    #[tokio::test]
    async fn dry_run_reports_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(
                json!({"pattern":"old", "replace": "new", "dry_run": true}),
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
                json!({"pattern":"x", "replace": "y", "glob": "src/**/*.rs"}),
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
    async fn an_empty_pattern_and_a_bad_regex_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = ReplaceTool
            .execute(json!({"pattern":"", "replace": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");

        // Regex-by-default means a literal-intent string can now fail to
        // compile — the error must name the way out.
        let err = ReplaceTool
            .execute(json!({"pattern":"(unclosed", "replace": "x"}), &ctx)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid regex"), "{msg}");
        assert!(
            msg.contains("if you meant exact text, pass `literal: true`"),
            "the bad-regex error must carry the literal hint: {msg}"
        );
    }

    #[tokio::test]
    async fn no_match_is_reported_not_silently_successful() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "hello\n").await;
        let out = ReplaceTool
            .execute(json!({"pattern":"absent", "replace": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("No file matches"), "{out}");
        assert!(
            !out.contains("literal: true"),
            "no metacharacters, so no misleading nudge:\n{out}"
        );
    }

    /// A pattern that compiles but matches nothing, full of metacharacters, is
    /// the signature of literal intent written into a regex field — the
    /// no-match report says so rather than leaving the model to guess.
    #[tokio::test]
    async fn a_metacharacter_pattern_that_matches_nothing_suggests_literal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        write(&dir.path().join("a.txt"), "hello\n").await;
        let out = ReplaceTool
            .execute(json!({"pattern": "foo.bar(x)", "replace": "y"}), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("No file matches"), "{out}");
        assert!(
            out.contains("If you meant exact text, pass `literal: true`."),
            "{out}"
        );
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
            .execute(json!({"pattern":"old_name", "replace": "new_name"}), &ctx)
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
            .execute(json!({"pattern":"needle", "replace": "found"}), &ctx)
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
            .execute(json!({"pattern":"needle", "replace": "found"}), &ctx)
            .await
            .unwrap();

        assert!(out.starts_with("No file matches"), "{out}");
        assert!(out.contains("1 file over 2 MiB skipped: big.txt"), "{out}");
    }

    /// `MAX_FILES` bounds the files that actually *match* `pattern`, not every
    /// candidate the walk turns up — a repo with far more than `MAX_FILES`
    /// files but only a few hits must still succeed.
    #[tokio::test]
    async fn max_files_counts_matches_not_candidates() {
        let dir = tempfile::tempdir().unwrap();
        // Many more candidate files than MAX_FILES, none matching `pattern`.
        for i in 0..(MAX_FILES + 50) {
            write(&dir.path().join(format!("f{i}.txt")), "nothing here\n").await;
        }
        // A single file that actually matches.
        write(&dir.path().join("hit.txt"), "needle\n").await;
        let ctx = ToolContext::new(dir.path());

        let out = ReplaceTool
            .execute(json!({"pattern":"needle", "replace": "found"}), &ctx)
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
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(json!({"pattern":"old", "replace": "new"}), &ctx)
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
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(json!({"pattern":"old", "replace": "new"}), &ctx)
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
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
        }]);
        write(&dir.path().join("a.txt"), "old\n").await;

        let out = ReplaceTool
            .execute(
                json!({"pattern":"old", "replace": "new", "dry_run": true}),
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

    /// A SINGLE giant match must be refused BEFORE it expands, not after.
    ///
    /// This is the case a post-expand check misses: `(a+)` matches the whole
    /// input once, and a template repeating `$1` a thousand times would expand
    /// that one match to ~200 MB in a single `expand` call — allocated in full
    /// before any "did we pass the cap?" check that runs afterward could fire.
    /// The pre-expand projection refuses it up front, so `out` never holds the
    /// blow-up: the call returns an `Err` far above `cap` essentially instantly.
    #[test]
    fn bounded_regex_replace_refuses_a_single_giant_match_before_expanding() {
        let re = regex::Regex::new("(a+)").unwrap(); // one match over the whole input
        let template = "$1".repeat(1_000);
        let input = "a".repeat(200_000); // one 200k capture × 1000 refs ≈ 200 MB
        let cap = 1024;
        let err = bounded_regex_replace(&re, &template, &input, cap).unwrap_err();
        assert!(
            err > 100_000_000,
            "must report the projected blow-up ({err}) and refuse before expanding"
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
                json!({"pattern": "(a)", "replace": "$1".repeat(1_000)}),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("would produce"), "{msg}");
        assert!(msg.contains("+ bytes"), "reports a lower-bound size: {msg}");
        assert!(msg.contains("narrow `pattern`"), "{msg}");
        assert_eq!(
            read(&dir.path().join("big.txt")).await,
            input,
            "the file must be left exactly as it was — the sweep aborted"
        );
    }
}
