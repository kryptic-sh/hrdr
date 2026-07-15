//! `replace`: one substitution applied across many files, under the guards.
//!
//! The alternative a model reaches for is `bash sed -i`, which is the single
//! worst mutation path available to it: not held to the `write_ext` allow-list,
//! and silent about what it changed — a bad regex corrupts the tree and the
//! model reports success.
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
        "Replace text across many files at once — the safe way to do a project-wide rename. \
         `find` is a literal string unless `regex` is true. Narrow the sweep with `glob` \
         (e.g. \"src/**/*.rs\") and/or `path`. Returns a unified diff of every file changed. \
         Use `dry_run: true` to preview first. Prefer this over `bash sed -i`: it shows you \
         exactly what it changed."
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

        let candidates = collect_files(&root, pattern.as_ref(), ctx)?;

        // Phase 1 — plan. Every file the sweep would rewrite is checked before
        // any of them is written, so a file this agent may not touch aborts the
        // whole sweep rather than leaving it half applied. `MAX_FILES` bounds
        // the files that actually *match* — not every candidate the walk
        // turns up — so a large repo with few hits still succeeds.
        let mut planned = Vec::new();
        let mut diffs = String::new();
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
            ctx.ensure_writable_ext(&path)?;
            // A literal substitution's output size is exactly computable from the
            // hit count, so bound it before allocating: `find="e"`, `replace=50KB`
            // could expand even a single sub-2 MB file into gigabytes. (A regex
            // replacement's size depends on per-match captures, so it isn't
            // guarded here — its input is already capped at `MAX_FILE_BYTES`.)
            if !a.regex && a.replace.len() > a.find.len() {
                let projected = before
                    .len()
                    .saturating_add(hits.saturating_mul(a.replace.len() - a.find.len()));
                if projected > MAX_EDIT_OUTPUT_BYTES {
                    bail!(
                        "replacing {:?} in {} would produce ~{projected} bytes; narrow `find` or \
                         the sweep",
                        a.find,
                        path.strip_prefix(&ctx.cwd).unwrap_or(&path).display()
                    );
                }
            }
            let after = if a.regex {
                re.replace_all(&before, a.replace.as_str()).into_owned()
            } else {
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
            diffs.push_str(&unified_diff(&rel, &before, &after));
            planned.push((path, after, rel));
        }

        // Phase 2 — write.
        let mut changed = Vec::new();
        for (path, after, rel) in planned {
            if !a.dry_run {
                apply_file_change(ctx, &path, "replace", &after).await?;
                ctx.mark_read(&path);
            }
            changed.push(rel);
        }

        if changed.is_empty() {
            return Ok(format!("No file contains {:?} — nothing changed.", a.find));
        }
        let verb = if a.dry_run {
            "Would replace"
        } else {
            "Replaced"
        };
        Ok(truncate(
            &format!(
                "{verb} {total} occurrence{} across {} file{}:\n{}\n\n{diffs}",
                if total == 1 { "" } else { "s" },
                changed.len(),
                if changed.len() == 1 { "" } else { "s" },
                changed.join("\n")
            ),
            ctx.max_output,
        ))
    }
}

/// Every text-sized, non-secret file under `root` that `pattern` admits,
/// honouring `.gitignore` — the same walker `find` uses, so the two agree on
/// what "the project" is. This is the sweep's *candidate* set, before any
/// content is inspected — [`MAX_FILES`] is enforced against files that
/// actually match `find` (see the caller), not against how many candidates
/// this turns up, so a large repo with few hits still succeeds.
fn collect_files(
    root: &std::path::Path,
    pattern: Option<&glob::Pattern>,
    ctx: &ToolContext,
) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
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
            continue;
        }
        out.push(path);
    }
    out.sort();
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

    /// The `write_ext` allow-list applies to each file the sweep would rewrite —
    /// a `plan` sub-agent cannot rename a symbol across `.rs` files.
    #[tokio::test]
    async fn the_write_ext_allow_list_applies_to_every_file_touched() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.write_allow_ext = Some(vec!["md".into()]);
        write(&dir.path().join("a.md"), "old\n").await;
        write(&dir.path().join("b.rs"), "old\n").await;

        let err = ReplaceTool
            .execute(json!({"find": "old", "replace": "new"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only modify"), "{err}");
        // Refused before *anything* was written — the allowed `.md` included.
        // The sweep is all-or-nothing: phase 1 checks every target, phase 2
        // writes. A half-applied rename across a project is worse than none.
        assert_eq!(
            read(&dir.path().join("a.md")).await,
            "old\n",
            "the allowed file must not be half-applied"
        );
        assert_eq!(read(&dir.path().join("b.rs")).await, "old\n");

        // Scoped to what it may touch, it succeeds.
        ReplaceTool
            .execute(
                json!({"find": "old", "replace": "new", "glob": "*.md"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(read(&dir.path().join("a.md")).await, "new\n");
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
}
