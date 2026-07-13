//! The built-in tool set (read, write, edit, patch, shell, grep, find, ls, todo, fetch, search).

pub(crate) mod edit;
pub(crate) mod fileops;
pub(crate) mod find;
pub(crate) mod git;
pub(crate) mod grep;
pub(crate) mod ls;
pub(crate) mod lsp_nav;
pub(crate) mod mutation;
pub(crate) mod read;
pub(crate) mod replace;
pub(crate) mod shell;
pub(crate) mod todo;
pub(crate) mod tree;
pub(crate) mod watch;
pub(crate) mod write;

/// Hard cap on a rendered source line, so one minified file can't blow context.
pub(crate) const MAX_LINE: usize = 2_000;
pub(crate) const DEFAULT_READ_LIMIT: usize = 2_000;
/// Hard cap on the file size `read` will load into memory. Past this, even
/// `offset`/`limit` paging isn't worth it — `read_to_string` would buffer the
/// whole file first regardless of how few lines are requested, so a
/// multi-gigabyte (or special/device) file would stall or OOM the process
/// before a single line comes back. Generous enough for any real source file.
pub(crate) const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
/// How long a shell command gets before it is killed, unless the model asks for
/// more with `timeout_ms`. Shared by `bash` and `powershell`.
///
/// Five minutes, because the commands worth running are the slow ones: a cold
/// `cargo build`, a full test suite, an `npm install` on a fresh tree. The old
/// two-minute default killed those *just* often enough to be maddening — and a
/// killed build teaches the model nothing except to try a narrower command, so the
/// work is redone rather than finished. A command that hangs is still caught; it
/// just gets a realistic amount of rope first.
pub(crate) const DEFAULT_SHELL_TIMEOUT_MS: u64 = 300_000;
/// Hard cap on a single output line accumulated from bash/powershell; prevents
/// a minified-file line from blowing the per-turn context.
pub(crate) const BASH_LINE_CAP: usize = 8_192;

/// How long `watch` sleeps between checks when the model doesn't say.
///
/// Ten seconds is the compromise: fast enough that a build finishing is noticed
/// while the agent still has the context to act on it, slow enough not to hammer
/// someone else's API (`gh` is rate-limited) for the half-hour a CI run can take.
pub(crate) const DEFAULT_WATCH_INTERVAL_SECS: u64 = 10;
/// How long `watch` waits in total before giving up. Far longer than a shell
/// command's timeout, because waiting is the *point* — but bounded, so a condition
/// that will never hold ends the call rather than the turn.
pub(crate) const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 30 * 60;
/// The most a model can ask `watch` to wait. Six hours is longer than any CI run
/// worth waiting on; past that, the agent should hand the job back to the user
/// rather than sit on a tool call.
pub(crate) const MAX_WATCH_TIMEOUT_SECS: u64 = 6 * 60 * 60;
/// How long any single `watch` check gets before it is killed and read as "not
/// yet". A check is a question, not a job — one that hangs (a network call with no
/// timeout of its own) must not wedge the watch.
pub(crate) const WATCH_CHECK_TIMEOUT_SECS: u64 = 120;

pub use edit::EditTool;
pub use fileops::{CopyTool, DeleteTool, MoveTool};
pub use find::FindTool;
pub use git::GitTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use lsp_nav::{DefinitionTool, ReferencesTool, RenameTool};
pub use read::ReadTool;
pub use replace::ReplaceTool;
pub use shell::{BashTool, PowerShellTool, available_shell_tools, user_shell};
pub use todo::TodoTool;
pub use tree::TreeTool;
pub use watch::WatchTool;
pub use write::WriteTool;

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

    // ---- read deny-list (credential exfiltration guard) ----

    #[tokio::test]
    async fn read_rejects_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate an auth store at <cwd>/.config/hrdr/auth.toml.
        let auth = dir.path().join(".config/hrdr/auth.toml");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, "api_key = \"secret\"\n").unwrap();
        let c = ctx(dir.path().to_path_buf());

        let err = ReadTool
            .execute(json!({ "path": ".config/hrdr/auth.toml" }), &c)
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

    #[test]
    fn grep_builtin_skips_secret_files() {
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
            },
            &c,
        )
        .unwrap();
        assert!(out.contains("code.rs"), "{out}");
        assert!(
            !out.contains("server.pem"),
            "secret file must not be searched: {out}"
        );
    }

    // ---- grep (built-in fallback) ----

    #[test]
    fn grep_builtin_matches_and_filters() {
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
            },
            &c,
        )
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
            },
            &c,
        )
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
        let out = grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(1),
                multiline: false,
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
        let out = grep_builtin(
            &GrepArgs {
                pattern: "hit".into(),
                path: None,
                glob: None,
                context: Some(3),
                multiline: false,
            },
            &c,
        )
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

    #[cfg(unix)]
    #[tokio::test]
    async fn mutations_refuse_symlink_path_components() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("linked")).unwrap();
        let c = ctx(dir.path().to_path_buf());

        let err = WriteTool
            .execute(
                serde_json::json!({"path": "linked/file.txt", "content": "blocked"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink component"), "{err}");
        assert!(!dir.path().join("real/file.txt").exists());
    }

    /// The symlink guard stops at the project root **however that root is spelled**.
    ///
    /// The stop used to be raw path equality (`candidate == self.cwd`), which is
    /// textual: `/var/folders/…` and `/private/var/folders/…` are the same directory
    /// and different `Path`s. So a project reached through a symlink — a home dir on
    /// a symlinked volume, a worktree behind a link, every tempdir on macOS, where
    /// `/var` *is* a symlink to `/private/var` — could sail past the stop, meet the
    /// symlink above the root, and refuse a write that was always legitimate.
    ///
    /// The symlink here is *above* the working directory: it is the user's
    /// filesystem, not something the project planted, and it must not block writes
    /// inside the project.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_project_reached_through_a_symlink_is_still_writable() {
        let dir = tempfile::tempdir().unwrap();
        // An ancestor symlink *above* the project root: <tmp>/alias -> <tmp>/actual.
        // This is the shape of macOS's own filesystem, where the temp dir (and so
        // every tempdir, and many a project) sits under `/var` — which is a symlink
        // to `/private/var`. It is the user's filesystem, not the project's.
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(actual.join("proj")).unwrap();
        std::os::unix::fs::symlink(&actual, dir.path().join("alias")).unwrap();

        // hrdr holds the *resolved* root, and is handed a path spelled through the
        // alias — which is what happens whenever a caller resolves one and not the
        // other. Raw path equality (`candidate == self.cwd`) never matches here, so
        // the old walk sailed past the root, met the symlink above it, and refused a
        // write inside the project. Canonical comparison stops where it should.
        let c = ctx(actual.join("proj"));
        let through_alias = dir.path().join("alias/proj/notes.txt");

        c.ensure_no_symlink_components(&through_alias)
            .expect("a project reached through an ancestor symlink is still the project");

        // And the whole tool path agrees.
        let c = ctx(dir.path().join("alias/proj"));
        WriteTool
            .execute(
                serde_json::json!({"path": "notes.txt", "content": "allowed"}),
                &c,
            )
            .await
            .expect("a project behind a symlink is still a project");
        assert_eq!(
            std::fs::read_to_string(actual.join("proj/notes.txt")).unwrap(),
            "allowed"
        );
    }

    /// …and the guard still refuses a symlink *inside* the project, even when the
    /// project root is itself reached through one. The exemption is for what sits
    /// above the root, not for the repo's own contents — a checked-in symlink
    /// pointing at `/etc` is exactly what this is here to stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_inside_a_symlinked_project_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("inner")).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        // A symlink the *project* contains, below the (symlinked) root.
        std::os::unix::fs::symlink("inner", real.join("evil")).unwrap();

        let c = ctx(dir.path().join("link"));
        let err = WriteTool
            .execute(
                serde_json::json!({"path": "evil/file.txt", "content": "blocked"}),
                &c,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink component"), "{err}");
        assert!(!real.join("inner/file.txt").exists());
    }

    /// Scratch under the system temp dir is writable — that is what the temp stop
    /// exists for — but the stop is *at* the temp dir, not below it.
    ///
    /// `/tmp` is world-writable, and a symlink planted in it by another local user
    /// is the oldest trick in the book. Exempting everything beneath the temp dir
    /// would have re-opened it. Components below the stop are still checked.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_below_the_temp_dir_is_still_refused() {
        let temp = std::env::temp_dir();
        let real = temp.join(format!("hrdr-test-real-{}", std::process::id()));
        let link = temp.join(format!("hrdr-test-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // cwd is a normal project; the target is scratch in the temp tree, reached
        // through a symlink that lives *inside* the temp dir.
        let project = tempfile::tempdir().unwrap();
        let c = ctx(project.path().to_path_buf());
        let through_link = link.join("payload.txt");

        let err = c
            .ensure_no_symlink_components(&through_link)
            .expect_err("a symlink below the temp dir must not be a free pass");
        assert!(err.to_string().contains("symlink component"), "{err}");

        // The real directory, named directly, is fine.
        c.ensure_no_symlink_components(&real.join("payload.txt"))
            .expect("scratch in the temp dir is allowed");

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&real).unwrap();
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

    /// Small output that never crosses either cap must not mention (or need)
    /// an overflow file at all — the overflow file is created only once
    /// output actually exceeds the caps, not eagerly on the first line.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_small_output_has_no_overflow_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path().to_path_buf());
        let out = BashTool
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
        let result = BashTool
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
}
