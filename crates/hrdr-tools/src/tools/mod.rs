//! The built-in tool set (read, write, edit, shell, grep, find, ls, todo, fetch, search).

pub(crate) mod edit;
pub(crate) mod find;
pub(crate) mod grep;
pub(crate) mod ls;
pub(crate) mod mutation;
pub(crate) mod read;
pub(crate) mod replace;
pub(crate) mod secret_diff;
pub(crate) mod shell;
pub(crate) mod todo;
pub(crate) mod tree;
pub(crate) mod verify;
pub(crate) mod write;

/// Hard cap on a rendered source line, so one minified file can't blow context.
pub(crate) const MAX_LINE: usize = 2_000;
pub(crate) const DEFAULT_READ_LIMIT: usize = 2_000;
/// The `read` output budget as a multiple of the shared `max_output` cap. The
/// shared cap is sized to tame *unbounded* tool output (a `cargo build` wall, a
/// huge grep) which spills to a file; a file `read` is different — the model
/// asked for the content, usually needs the whole file, and often is reading an
/// overflow a `shell`/`grep`/`git` call just spilled — so reads get far more
/// room. `full: true` ignores the budget entirely (bounded only by the 50 MB
/// load cap).
pub(crate) const READ_BUDGET_FACTOR: usize = 20;
/// Hard cap on the file size `read` will load into memory. Past this, even
/// `offset`/`limit` paging isn't worth it — `read_to_string` would buffer the
/// whole file first regardless of how few lines are requested, so a
/// multi-gigabyte (or special/device) file would stall or OOM the process
/// before a single line comes back. Generous enough for any real source file.
pub(crate) const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
/// **Naming for every duration constant in this workspace**, enforced by
/// `hrdr-test-support`'s `duration_constants_name_their_unit` test rather than
/// left to memory:
///
/// * A `Duration` constant carries **no unit suffix** — the type already says it
///   — and ends in what it bounds: `_TIMEOUT`, `_INTERVAL`, `_TTL`, `_DEBOUNCE`,
///   `_POLL`. (`HTTP_TIMEOUT`, `CALL_TIMEOUT`, `CACHE_TTL`.)
/// * A **scalar** constant (`u64`/`usize`) must end in its unit: `_SECS`, `_MS`,
///   or the non-time unit it counts (`_TURNS`, `_BYTES`, `_LINES`). A bare number
///   whose unit lives only in a doc comment is how `100` meaning a tenth of a
///   second becomes a minute and a half.
/// * An optional leading `DEFAULT_` / `MAX_` / `MIN_` says which bound it is, and
///   the scope comes next when the constant is exported (`DEFAULT_TOOL_…`,
///   `DEFAULT_HOOK_…`); a module-private one takes the module as its scope
///   (`NAV_TIMEOUT_SECS` in `lsp`).
///
/// Model- and config-facing durations are seconds, always, so a name ending
/// `_MS` marks something deliberately sub-second: a debounce or a probe poll,
/// never a timeout.
///
/// ---
///
/// How long **any** tool call gets before it is cut off, unless the model asks
/// for more with `timeout_secs`. Enforced once, for every tool, in
/// [`crate::ToolRegistry::execute`].
///
/// Five minutes, because the calls worth waiting on are the slow ones: a cold
/// `cargo build`, a full test suite, an `npm install` on a fresh tree, a search
/// across a huge tree. The old two-minute shell default killed those *just* often
/// enough to be maddening — and a killed build teaches the model nothing except to
/// try a narrower command, so the work is redone rather than finished. Something
/// genuinely wedged is still caught; it just gets a realistic amount of rope first.
///
/// It was `DEFAULT_SHELL_TIMEOUT_SECS` while only `shell` had a deadline at all.
/// `grep` and `git` had none, so a pathological pattern or a git command on a cold
/// mount could hang a turn indefinitely — the name changed with the scope.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 300;
/// Hard cap on a single output line accumulated from the shell; prevents
/// a minified-file line from blowing the per-turn context.
pub(crate) const BASH_LINE_CAP: usize = 8_192;
/// What a search tool reports when nothing matched.
pub(crate) const NO_MATCHES: &str = "(no matches)";

/// Create `path`'s parent directory (and any missing ancestors), so a write to a
/// path in a directory that doesn't exist yet succeeds. A path with no parent
/// (a bare root) is a no-op.
pub(crate) async fn ensure_parent_dir(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

/// Whether `path` exists, reading any error (a permission-denied ancestor, a
/// broken symlink) as "no" — the callers all want a plain yes/no.
pub(crate) async fn path_exists(path: &std::path::Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Build the ignore-aware `ignore::Walk` shared by the search tools (`grep`'s
/// two built-in walker variants and `find`), honoring `hidden` (dotfiles) and
/// `no_ignore` (`.gitignore`, `.ignore`, git global/local excludes). Both flags
/// default to skipping — matching ripgrep's defaults — and are passed through
/// per call, so `hidden: true` means "also walk dotfiles" and `no_ignore: true`
/// means "also walk ignored paths".
pub(crate) fn ignore_walker(root: &std::path::Path, hidden: bool, no_ignore: bool) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .max_depth(Some(20))
        .hidden(!hidden)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_global(!no_ignore)
        .git_exclude(!no_ignore)
        .parents(!no_ignore)
        .build()
}

/// `path` displayed relative to `cwd`, falling back to the full path when it
/// lies outside — the short form the tools show the model.
pub(crate) fn rel_display<'a>(
    path: &'a std::path::Path,
    cwd: &std::path::Path,
) -> std::path::Display<'a> {
    path.strip_prefix(cwd).unwrap_or(path).display()
}

pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use replace::ReplaceTool;
pub use secret_diff::redact_secret_diffs;
pub use shell::{CommandRun, Shell, ShellTool, available_shell_tools, run_user_command};
pub use todo::TodoTool;
pub use tree::TreeTool;
pub use verify::{DEFAULT_VERIFY_TIMEOUT_SECS, VerifyTool};
pub use write::WriteTool;
pub use write::abbreviate_mutation_result;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use crate::{TodoItem, Tool, ToolContext};

    use super::grep::{GrepArgs, grep_builtin};
    use super::todo::parse_todos;
    use super::*;

    fn ctx(cwd: PathBuf) -> ToolContext {
        ToolContext::new(cwd)
    }

    // ---- read → write contract: partial-read (#8) + stale-file (#4) guards ----

    /// A full read of a file lets a `write` overwrite it; a paged (`limit`) read
    /// of the same file does not — the unread tail would be silently dropped.
    #[tokio::test]
    async fn write_requires_a_complete_read_not_a_partial_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let body = (1..=50)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, &body).unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        // Partial read (first line only) → write refused, file untouched.
        ReadTool
            .execute(json!({"path": p, "limit": 1}), &c)
            .await
            .unwrap();
        let err = WriteTool
            .execute(json!({"path": p, "content": "rewritten\n"}), &c)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("only read part of"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        // A full read lifts the gate.
        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        WriteTool
            .execute(json!({"path": p, "content": "rewritten\n"}), &c)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rewritten\n");
    }

    /// A `write` is refused when the file changed on disk since the model read it
    /// — the change (a user's editor save, a formatter) must not be clobbered.
    #[tokio::test]
    async fn write_refused_when_file_changed_on_disk_since_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, "v1\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        // Someone else edits it (different length → detected regardless of mtime).
        std::fs::write(&path, "v2 with the user's own change\n").unwrap();

        let err = WriteTool
            .execute(json!({"path": p, "content": "model rewrite\n"}), &c)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "v2 with the user's own change\n"
        );
    }

    /// `edit` matches against the file's live content, so a partial read is
    /// enough — but a stale read (file changed on disk since) whose `old_string`
    /// no longer exists in the new content is still refused. A stale read whose
    /// anchor *does* still match is applied — see
    /// `edit_over_a_stale_read_applies_when_the_anchor_still_matches`.
    #[tokio::test]
    async fn edit_accepts_a_partial_read_but_refuses_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        // Partial read → edit still allowed (it re-reads the file to match).
        ReadTool
            .execute(json!({"path": p, "limit": 1}), &c)
            .await
            .unwrap();
        EditTool
            .execute(
                json!({"path": p, "old_string": "fn b() {}", "new_string": "fn b() { todo!() }"}),
                &c,
            )
            .await
            .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("todo!()"));

        // The file changes underneath, taking the anchor with it; the next edit is
        // refused as stale.
        std::fs::write(&path, "totally different and longer content here\n").unwrap();
        let err = EditTool
            .execute(
                json!({"path": p, "old_string": "fn b() { todo!() }", "new_string": "fn b() {}"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
    }

    /// The formatter case, which used to be the top source of `edit` failures: a
    /// hook/`cargo fmt` rewrites the file between read and edit, but the model's
    /// `old_string` still matches the *current* content uniquely — so the anchor
    /// is live and the edit is applied, with a note saying the diff is against
    /// the changed file. A following edit works too: the baseline moved to the
    /// post-edit content, so the model isn't pushed into a re-read either way.
    #[tokio::test]
    async fn edit_over_a_stale_read_applies_when_the_anchor_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn a() {}\nfn keep() {}\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        // A formatter reflows an untouched region (different length → stale
        // regardless of mtime granularity) but leaves the anchor intact.
        std::fs::write(&path, "fn a() {\n    // reformatted\n}\nfn keep() {}\n").unwrap();

        let out = EditTool
            .execute(
                json!({"path": p, "old_string": "fn keep() {}", "new_string": "fn kept() {}"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert!(
            out.contains("had changed on disk") && out.contains("anchor still matched"),
            "the result must say the edit was applied over a changed file: {out}"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "fn a() {\n    // reformatted\n}\nfn kept() {}\n");

        // The baseline followed the edit: no re-read needed for the next one.
        EditTool
            .execute(
                json!({"path": p, "old_string": "reformatted", "new_string": "formatted"}),
                &c,
            )
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("formatted"),
            "the follow-up edit must not be refused as stale"
        );
    }

    /// A stale read whose anchor became *ambiguous* is refused, not applied: two
    /// matches means the model can't know which one it is aiming at, which is the
    /// same reason a fresh non-unique edit is refused.
    #[tokio::test]
    async fn edit_over_a_stale_read_refuses_an_ambiguous_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.rs");
        std::fs::write(&path, "fn one() {}\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        // Something duplicates the anchor underneath us.
        std::fs::write(&path, "fn one() {}\nfn one() {}\n").unwrap();

        let err = EditTool
            .execute(
                json!({"path": p, "old_string": "fn one() {}", "new_string": "fn two() {}"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn one() {}\nfn one() {}\n"
        );
    }

    /// `replace` shows the model the new content of every file it rewrote, so the
    /// stale guard has nothing left to protect: an `edit` to one of those files
    /// goes through without an intervening re-read.
    #[tokio::test]
    async fn edit_after_replace_needs_no_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "let old_name = 1;\nlet other = 2;\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        ReplaceTool
            .execute(
                json!({"pattern": "old_name", "replace": "new_name", "path": dir.path().to_str().unwrap()}),
                &c,
            )
            .await
            .unwrap();

        let out = EditTool
            .execute(
                json!({"path": p, "old_string": "let other = 2;", "new_string": "let other = 3;"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert!(
            !out.contains("had changed on disk"),
            "`replace` refreshed the baseline, so this edit isn't a stale one: {out}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "let new_name = 1;\nlet other = 3;\n"
        );
    }

    /// When a `shell` command is what changed a tracked file, the staleness
    /// refusal names it — "modified by `…`" — so the model re-reads instead of
    /// suspecting its own bookkeeping (the observed failure mode: distrusting
    /// `edit` and rewriting whole files with `write`).
    #[cfg(unix)]
    #[tokio::test]
    async fn stale_edit_error_names_the_shell_command_that_changed_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fmt_me.rs");
        std::fs::write(&path, "fn a(){}\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        ShellTool::new(Shell::Bash)
            .execute(
                json!({"command": "printf 'fn a() {}\\nfn b() {}\\n' > fmt_me.rs # pretend-fmt"}),
                &c,
            )
            .await
            .unwrap();

        // Anchor gone (the command rewrote that line) → still refused, but now
        // the refusal says what did it.
        let err = EditTool
            .execute(
                json!({"path": p, "old_string": "fn a(){}", "new_string": "fn a() { todo!() }"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert!(
            err.contains("modified by") && err.contains("pretend-fmt"),
            "the culprit command must be named: {err}"
        );

        // A re-read clears the attribution: a later, unexplained change must not
        // be blamed on that command.
        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        std::fs::write(&path, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let err = EditTool
            .execute(
                json!({"path": p, "old_string": "fn zzz() {}", "new_string": "x"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert!(
            !err.contains("modified by"),
            "stale attribution kept: {err}"
        );
    }

    /// Two edits in a row are fine: the first re-records the file post-edit, so
    /// the second doesn't mistake its own change for a stale-on-disk mismatch.
    #[tokio::test]
    async fn consecutive_edits_do_not_trip_the_stale_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha beta gamma\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let p = path.to_str().unwrap();

        ReadTool.execute(json!({"path": p}), &c).await.unwrap();
        EditTool
            .execute(
                json!({"path": p, "old_string": "alpha", "new_string": "ALPHA"}),
                &c,
            )
            .await
            .unwrap();
        EditTool
            .execute(
                json!({"path": p, "old_string": "gamma", "new_string": "GAMMA"}),
                &c,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ALPHA beta GAMMA\n"
        );
    }

    // ---- read deny-list (credential exfiltration guard) ----

    #[tokio::test]
    async fn read_rejects_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate an auth store at <cwd>/.config/hrdr/auth.json.
        let auth = dir.path().join(".config/hrdr/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(
            &auth,
            "{\"openrouter\":{\"type\":\"key\",\"key\":\"secret\"}}\n",
        )
        .unwrap();
        let c = ctx(dir.path().to_path_buf());

        let err = ReadTool
            .execute(json!({ "path": ".config/hrdr/auth.json" }), &c)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to read"), "{err}");
        assert!(err.contains("credential store"), "{err}");
    }

    #[tokio::test]
    async fn read_allows_normal_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello world\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = ReadTool
            .execute(json!({ "path": "notes.txt" }), &c)
            .await
            .unwrap();
        assert!(out.contains("hello world"), "{out}");
    }

    /// A `.env` *inside* the project is still refused by the secret deny-list —
    /// the case cwd-confinement doesn't cover (an in-project credential file).
    /// A `../.env` escape to the parent is caught earlier by confinement (see
    /// the per-tool `*_refuses_a_path_outside_cwd` tests).
    #[tokio::test]
    async fn read_rejects_in_project_dotenv_via_secret_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "TOKEN=abc\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let err = ReadTool
            .execute(json!({ "path": ".env" }), &c)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to read"), "{err}");
        assert!(err.contains(".env"), "{err}");
    }

    #[tokio::test]
    async fn read_rejects_private_key_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa.pem"), "-----BEGIN-----\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        assert!(
            ReadTool
                .execute(json!({ "path": "id_rsa.pem" }), &c)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn grep_builtin_skips_secret_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "let token = 1;\n").unwrap();
        // A non-hidden private key (the walker already skips dotfiles, so use a
        // `.pem` to prove the deny-list — not the hidden filter — excludes it).
        std::fs::write(dir.path().join("server.pem"), "token = SECRET\n").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = grep_builtin(
            &GrepArgs {
                pattern: "token".into(),
                path: None,
                glob: None,
                context: None,
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
        .unwrap();
        assert!(out.contains("code.rs"), "{out}");
        assert!(
            !out.contains("server.pem"),
            "secret file must not be searched: {out}"
        );
    }

    // ---- grep (built-in fallback) ----

    #[tokio::test]
    async fn grep_builtin_matches_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nlet x = 1;\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "foo in text\n").unwrap();
        let c = ctx(dir.path().to_path_buf());

        // Matches across files.
        let out = grep_builtin(
            &GrepArgs {
                pattern: "foo".into(),
                path: None,
                glob: None,
                context: None,
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
        .unwrap();
        assert!(out.contains("a.rs:1:fn foo() {}"), "{out}");
        assert!(out.contains("b.txt:1:foo in text"), "{out}");

        // Glob restricts to *.rs.
        let out = grep_builtin(
            &GrepArgs {
                pattern: "foo".into(),
                path: None,
                glob: Some("*.rs".into()),
                context: None,
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
        .unwrap();
        assert!(out.contains("a.rs"), "{out}");
        assert!(!out.contains("b.txt"), "glob should exclude b.txt: {out}");

        // No matches.
        let out = grep_builtin(
            &GrepArgs {
                pattern: "zzz_nope".into(),
                path: None,
                glob: None,
                context: None,
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
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
        let out = grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(1),
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
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
        let out = grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(3),
                multiline: false,
                hidden: false,
                no_ignore: false,
                literal: false,
                case_insensitive: false,
            },
            &c,
        )
        .await
        .unwrap();
        assert_eq!(out.matches("--\n").count(), 0, "{out}");
        // No duplicate lines from the merge.
        assert_eq!(out.matches("l5").count(), 1, "{out}");
    }

    // ---- read ----

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
        assert!(out.contains("     1: alpha"), "line 1 not found: {out}");
        assert!(out.contains("     2: beta"), "line 2 not found: {out}");
        assert!(out.contains("     3: gamma"), "line 3 not found: {out}");
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
        assert!(!out.contains("     1: "), "line 1 should be skipped");
        assert!(out.contains("     2: 2"), "line 2 missing: {out}");
        assert!(out.contains("     3: 3"), "line 3 missing: {out}");
        assert!(!out.contains("     4: "), "line 4 should be skipped");
    }

    /// A file over the size cap is refused via a fast `metadata` stat, before
    /// `read_to_string` would load it whole. Uses a sparse file (`set_len`,
    /// no actual disk written) to hit the cap without allocating 50+ MiB in
    /// the test itself.
    #[tokio::test]
    async fn read_refuses_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.bin");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_READ_BYTES + 1).unwrap(); // sparse: no real bytes written
        drop(f);
        let c = ctx(dir.path().to_path_buf());
        let err = ReadTool
            .execute(serde_json::json!({"path": path.to_str().unwrap()}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    /// A file comfortably under the cap is unaffected by the new pre-check.
    #[tokio::test]
    async fn read_allows_files_well_under_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big_ish.txt");
        // A few MB — nowhere near the 50 MiB cap, but big enough to prove the
        // new metadata pre-check doesn't interfere with normal reads.
        std::fs::write(&path, "x".repeat(2 * 1024 * 1024)).unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = ReadTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "limit": 1}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains('x'), "{out}");
    }

    // ---- write ----

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
        assert!(err.to_string().contains("read first"), "{err}");
        let err = WriteTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "content": "x"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read first"), "{err}");
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
        let err = ShellTool::new(Shell::Bash)
            .execute(serde_json::json!({"command": "git add -A"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("command blocked"), "{err}");
        // Harmless commands still run. Unix-only: on Windows CI `bash` on
        // PATH is the WSL stub, which errors without a distro installed.
        #[cfg(unix)]
        {
            let out = ShellTool::new(Shell::Bash)
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
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
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
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
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

    /// An empty `old_string` would match at every position — with
    /// `replace_all` that corrupts the file (every gap gets `new_string`
    /// inserted). Refused up front instead.
    #[tokio::test]
    async fn edit_rejects_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "abc").unwrap();
        let c = ctx(dir.path().to_path_buf());
        c.mark_read(&path);
        let err = EditTool
            .execute(
                serde_json::json!({"path": path.to_str().unwrap(), "old_string": "", "new_string": "X", "replace_all": true}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        // The file must be untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
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
        let out = FindTool
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
        let out = FindTool
            .execute(serde_json::json!({"pattern": "*.nonexistent"}), &c)
            .await
            .unwrap();
        assert_eq!(out, "(no matches)");
    }

    /// `find` must skip files inside a `.gitignore`-listed directory (e.g.
    /// `target/`) while still returning files in non-ignored directories.
    /// A minimal `.git/` directory is created so the `ignore` crate can anchor
    /// the repository root and apply the `.gitignore` rules.
    #[tokio::test]
    async fn find_skips_gitignored_directories() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal .git directory so the `ignore` crate recognises the repo
        // root and applies `.gitignore` rules (it needs the git root anchor).
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        // A `.gitignore` that excludes `target/`.
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        // File inside the gitignored dir — must NOT appear in results.
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/ignored.rs"), "").unwrap();
        // Normal file that should appear.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = FindTool
            .execute(serde_json::json!({"pattern": "**/*.rs"}), &c)
            .await
            .unwrap();
        assert!(
            out.contains("main.rs"),
            "normal file missing from find results: {out}"
        );
        assert!(
            !out.contains("ignored.rs"),
            "gitignored file should not appear in find results: {out}"
        );
    }

    // ---- todo ----

    #[tokio::test]
    async fn todo_write_render_marks() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = TodoTool
            .execute(
                serde_json::json!({
                    "todos": [
                        {"content": "pending task",  "status": "pending"},
                        {"content": "active task",   "status": "in_progress"},
                        // `evidence` because a completion without it is refused
                        // — a separate guard from the mark rendering under test.
                        {"content": "done task",     "status": "completed", "evidence": "ran it"},
                        {"content": "stopped task",  "status": "cancelled"}
                    ]
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("  pending task"), "pending: {out}");
        assert!(out.contains("⠋ active task"), "in_progress: {out}");
        assert!(out.contains("✓ done task"), "completed: {out}");
        assert!(out.contains("✗ stopped task"), "cancelled: {out}");
    }

    /// The gate has to hold through the tool, not just in the helper: a refused
    /// call must also leave the previous list untouched, or a rejected tick
    /// still loses the state the model was tracking.
    #[tokio::test]
    async fn the_todo_tool_refuses_a_bare_completion_and_keeps_the_old_list() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let list = |status: &str, evidence: Option<&str>| {
            let mut item = serde_json::Map::new();
            item.insert("content".into(), json!("fix it"));
            item.insert("status".into(), json!(status));
            if let Some(e) = evidence {
                item.insert("evidence".into(), json!(e));
            }
            json!({ "todos": [serde_json::Value::Object(item)] })
        };

        TodoTool
            .execute(list("in_progress", None), &c)
            .await
            .unwrap();
        let err = TodoTool
            .execute(list("completed", None), &c)
            .await
            .expect_err("a completion with no evidence must be refused");
        assert!(err.to_string().contains("`fix it`"), "{err}");
        // The refusal did not eat the list it was sent to replace.
        assert_eq!(c.todos.lock().unwrap()[0].status, "in_progress");

        let out = TodoTool
            .execute(
                list("completed", Some("cargo test -p hrdr-tools: 155 passed")),
                &c,
            )
            .await
            .unwrap();
        assert!(
            out.contains("↳ cargo test -p hrdr-tools: 155 passed"),
            "{out}"
        );
        assert_eq!(c.todos.lock().unwrap()[0].status, "completed");
    }

    /// The nudge has to reach the model through the same result the edit does.
    #[tokio::test]
    async fn a_source_edit_with_no_test_carries_the_nudge_and_a_test_edit_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let src = dir.path().join("a.rs");
        tokio::fs::write(&src, "fn f() {}\n").await.unwrap();

        let fc = super::mutation::apply_file_change(&c, &src, "edit", "fn f() { g(); }\n")
            .await
            .unwrap();
        assert!(
            fc.notes.iter().any(|n| n == crate::TEST_NUDGE_NOTE),
            "{:?}",
            fc.notes,
        );

        // Adding a test anywhere latches it off for the rest of the session.
        let other = dir.path().join("b.rs");
        tokio::fs::write(&other, "fn h() {}\n").await.unwrap();
        super::mutation::apply_file_change(&c, &other, "edit", "fn h() {}\n#[test]\nfn t() {}\n")
            .await
            .unwrap();
        let third = dir.path().join("c.rs");
        tokio::fs::write(&third, "fn i() {}\n").await.unwrap();
        let fc = super::mutation::apply_file_change(&c, &third, "edit", "fn i() { j(); }\n")
            .await
            .unwrap();
        assert!(fc.notes.is_empty(), "{:?}", fc.notes);
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
                {"content": "v", "status": "canceled"}, // US spelling → cancelled
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 5);
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
        assert_eq!(
            (items[4].content.as_str(), items[4].status.as_str()),
            ("v", "cancelled")
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
        let out = ShellTool::new(Shell::Bash)
            .execute(serde_json::json!({"command": "echo hello_hrdr"}), &c)
            .await
            .unwrap();
        assert!(out.contains("hello_hrdr"), "echo output missing: {out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_exit_nonzero_includes_status() {
        let c = ctx(std::path::PathBuf::from("."));
        let out = ShellTool::new(Shell::Bash)
            .execute(serde_json::json!({"command": "exit 42"}), &c)
            .await
            .unwrap();
        assert!(out.contains("exit status"), "status marker missing: {out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_process_and_keeps_partial_output() {
        let mut c = ctx(std::path::PathBuf::from("."));
        c.enforce_timeout_floor = false;
        let out = ShellTool::new(Shell::Bash)
            .execute(
                serde_json::json!({"command": "echo early; sleep 30", "timeout_secs": 1}),
                &c,
            )
            .await
            .expect_err("a killed command is not a successful one")
            .to_string();
        assert!(out.contains("early"), "partial output missing: {out}");
        assert!(out.contains("timed out"), "timeout marker missing: {out}");
    }

    // ---- verification ledger ----

    /// The ledger reaches the model through the real tool path, end to end: a
    /// source edit, a check that is not a whole-tree *pass*, then a commit — and
    /// the commit's own result carries the note naming both. Then the ordering
    /// that is the whole point: another edit, a whole-tree pass *after* it, and
    /// the next commit is silent.
    ///
    /// `cargo` is shadowed by a shell function so this asserts on the ledger
    /// rather than spawning the real suite; the ledger classifies the
    /// command text, which is exactly what a real session hands it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_after_an_unverified_edit_carries_the_verification_note() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let bash = ShellTool::new(Shell::Bash);
        let run = async |command: &str| {
            bash.execute(serde_json::json!({ "command": command }), &c)
                .await
        };

        // A real repo, so the commit really commits.
        run("git init -q . && git config user.email a@example.com \
             && git config user.name a && git config commit.gpgsign false")
        .await
        .unwrap();
        // A source edit through the real mutation path.
        WriteTool
            .execute(
                serde_json::json!({"path": "lib.rs", "content": "fn a() {}\n"}),
                &c,
            )
            .await
            .unwrap();
        // A whole-tree run that failed. `false` short-circuits the `&&`, so no
        // cargo is spawned — and a red tree settles nothing anyway.
        let out = run("false && cargo test --workspace").await.unwrap();
        assert!(
            out.contains("exit status"),
            "the run must have failed: {out}"
        );

        let out = run("git add lib.rs && git commit -q -m wip").await.unwrap();
        assert!(out.contains("[verify]"), "no note on the commit: {out}");
        assert!(out.contains("test"), "the owed kind is named: {out}");
        assert!(
            out.contains("cargo test --workspace") && out.contains("failed"),
            "the note must name what was actually run: {out}"
        );

        // Now do it properly: edit, then a passing whole-tree run *after* it.
        WriteTool
            .execute(
                serde_json::json!({"path": "lib.rs", "content": "fn a() { b(); }\n"}),
                &c,
            )
            .await
            .unwrap();
        run("cargo() { return 0; }; cargo test --workspace")
            .await
            .unwrap();
        let out = run("git add lib.rs && git commit -q -m done")
            .await
            .unwrap();
        assert!(
            !out.contains("[verify]"),
            "a whole-tree pass at the current epoch owes nothing: {out}"
        );
    }

    /// Small output that never crosses either cap must not mention (or need)
    /// an overflow file at all — the overflow file is created only once
    /// output actually exceeds the caps, not eagerly on the first line.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_small_output_has_no_overflow_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = ShellTool::new(Shell::Bash)
            .execute(serde_json::json!({"command": "echo tiny"}), &c)
            .await
            .unwrap();
        assert!(out.contains("tiny"));
        assert!(
            !out.contains("full output") && !out.contains("saved to"),
            "small output should not reference an overflow file: {out}"
        );
    }

    /// Verify that run_streamed_command caps in-memory usage and produces a
    /// truncation marker when output exceeds max_output.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_output_bounded_and_marker_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ToolContext::new(dir.path());
        // Tiny output cap so even a small command overflows.
        c.max_output = 200;
        c.max_output_lines = 10;

        // Generate 50 lines of ~20 chars each (well above both caps).
        let result = ShellTool::new(Shell::Bash)
            .execute(
                serde_json::json!({"command": "for i in $(seq 1 50); do echo \"line $i: some padding text here\"; done"}),
                &c,
            )
            .await
            .unwrap();

        // The result must be within a reasonable bound (not the full 50*~25 = 1250 bytes).
        assert!(
            result.len() < 2000,
            "result should be bounded, got {} bytes",
            result.len()
        );
        // Must contain the truncation pointer so the model knows where to look.
        assert!(
            result.contains("full output") || result.contains("truncated"),
            "marker missing from: {result}"
        );
        // Must start with some actual output (head preserved).
        assert!(result.contains("line 1"), "head not preserved: {result}");
    }

    /// A single newline-less run far larger than the caps (`tr '\0' a </dev/zero`)
    /// must come back *bounded* rather than hang or OOM: the per-line read is
    /// capped as it streams, so the result is a small, marked truncation — not a
    /// gigabyte buffered whole and only then trimmed.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_newlineless_run_is_bounded_not_hung() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ToolContext::new(dir.path());
        c.max_output = 200;
        c.max_output_lines = 10;

        // 2 MiB of 'a' with no newline at all.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ShellTool::new(Shell::Bash).execute(
                serde_json::json!({
                    "command": "head -c 2097152 /dev/zero | tr '\\0' 'a'"
                }),
                &c,
            ),
        )
        .await
        .expect("a newline-less run must not hang")
        .unwrap();

        // Bounded: the single line was capped at BASH_LINE_CAP, nowhere near 2 MiB.
        assert!(
            result.len() <= BASH_LINE_CAP + 4096,
            "output should be bounded, got {} bytes",
            result.len()
        );
        // Over the (tiny) display cap, so the truncation pointer is present.
        assert!(
            result.contains("full output") || result.contains("truncated"),
            "marker missing from bounded output: {}",
            &result[..result.len().min(200)]
        );
    }
}
