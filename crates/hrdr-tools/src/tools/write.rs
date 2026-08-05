use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

use super::MAX_LINE;

// ---- write ----

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    // Same path-name synonyms `read` accepts (see `ReadArgs`): a model trained
    // on `file_path`/`file` shouldn't lose the call to a "missing field `path`".
    // Only the one path field exists here, so the aliases are unambiguous.
    #[serde(
        alias = "file_path",
        alias = "filepath",
        alias = "file",
        alias = "filename",
        alias = "file_name",
        alias = "path_to_file"
    )]
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Create a new file or fully rewrite an existing one with `content`. Parent \
         directories are created as needed. Overwriting an existing file requires a \
         complete, fresh read first — a partial read (paged, or clipped by a long line) or \
         a stale one (the file changed on disk since) is refused; re-read after any \
         external change. Prefer `edit` for changing part of an existing file."
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
        let a: WriteArgs = crate::tool_args("write", args)?;
        let path = ctx.resolve_write(&a.path)?;
        if let Some(reason) = crate::secret_file_reason(&crate::canonicalize_nearest(&path)) {
            bail!(
                "refusing to write to {}: {reason} — secret/credential files are off-limits to \
                 the write/edit tools; if the user genuinely needs this, they must provide it",
                path.display()
            );
        }
        let existed = super::path_exists(&path).await;
        if existed {
            // A `write` replaces the whole file, so the model must have seen the
            // whole current content: not unread, not a partial page, and not a
            // version that has since changed on disk.
            match ctx.read_state(&path) {
                crate::ReadState::Unread => bail!(
                    "{} exists but you haven't read it — call read first so the rewrite \
                     starts from its real content (or use edit for a partial change)",
                    path.display()
                ),
                crate::ReadState::Partial => bail!(
                    "you've only read part of {} — a write replaces the whole file, so read \
                     it in full first (no offset/limit, or page to the end) or the unread \
                     lines will be lost; use edit for a partial change. If this file has a \
                     line over {MAX_LINE} bytes, a normal read clips that line every time and \
                     can never mark it fully read — read it once with `full: true` (whole \
                     file, no clipping) to unblock the rewrite, or use `edit`/`shell`",
                    path.display()
                ),
                crate::ReadState::Stale => bail!(
                    "{} changed on disk since you read it — re-read it before overwriting, \
                     or the edit made in the meantime (an editor save, a formatter) is lost",
                    path.display()
                ),
                crate::ReadState::Fresh => {}
            }
        }
        let old = if existed {
            tokio::fs::read_to_string(&path).await.unwrap_or_default()
        } else {
            String::new()
        };
        super::ensure_parent_dir(&path).await?;
        let bytes = a.content.len();
        let fc = super::mutation::apply_file_change(ctx, &path, "write", &a.content).await?;
        ctx.mark_read(&path); // the model authored (or just saw) this content
        let warn = fc.formatted_notes();
        if existed {
            let diff = unified_diff(&path.display().to_string(), &old, &fc.content_after);
            let body = if diff.is_empty() {
                "(no changes)".to_string()
            } else {
                diff
            };
            Ok(format!(
                "Wrote {bytes} bytes to {}{warn}\n{body}",
                path.display()
            ))
        } else {
            Ok(format!(
                "Created {} ({} lines){warn}",
                path.display(),
                fc.content_after.lines().count()
            ))
        }
    }
}

/// `a/<path>` and `b/<path>` diff headers with exactly one separator — an
/// absolute `path` must not produce `a//home/…`, which is not a path anything
/// can act on and reads as a rendering bug.
fn diff_headers(path: &str) -> (String, String) {
    let joined = path.trim_start_matches('/');
    (format!("a/{joined}"), format!("b/{joined}"))
}

/// A unified diff of `old` → `new` for `path`, or empty if unchanged.
pub(crate) fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let (a, b) = diff_headers(path);
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&a, &b)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `read` path-name synonyms work here too — a call spelled `file` (or
    /// `file_path`) must not die on a "missing field `path`".
    #[test]
    fn write_args_accept_path_aliases() {
        for key in ["file", "file_path", "filename", "path"] {
            let a: WriteArgs = serde_json::from_value(json!({key: "x", "content": "c"}))
                .unwrap_or_else(|e| panic!("alias {key:?}: {e}"));
            assert_eq!(a.path, "x");
        }
    }

    /// A diff header for an absolute path joins to exactly one slash: `a//home/x`
    /// is not a path anything can act on, and it appeared in every single
    /// edit/write result (models pass absolute paths).
    #[test]
    fn a_diff_header_never_doubles_the_slash() {
        let diff = unified_diff("/home/u/p/f.rs", "one\n", "two\n");
        assert!(diff.contains("--- a/home/u/p/f.rs"), "{diff}");
        assert!(diff.contains("+++ b/home/u/p/f.rs"), "{diff}");
        assert!(!diff.contains("a//"), "no doubled slash: {diff}");
        assert!(!diff.contains("b//"), "no doubled slash: {diff}");
        // A relative path is unchanged.
        assert!(unified_diff("src/f.rs", "one\n", "two\n").contains("--- a/src/f.rs"));
    }

    /// The mutation result is the FULL diff — no cap, no summary: the model
    /// verifies its own edit from it, and a mistake it cannot see costs more
    /// rounds than the tokens a summary would save.
    #[test]
    fn a_mutation_result_carries_the_full_diff() {
        // Every line of a 60-line file rewritten: one hunk, 60 added, 60 removed.
        let old = (1..=60).map(|n| format!("line {n}\n")).collect::<String>();
        let new = (1..=60).map(|n| format!("LINE {n}\n")).collect::<String>();
        let diff = unified_diff("/tmp/f.txt", &old, &new);
        assert!(diff.lines().count() > 40, "the fixture is big: {diff}");
        let result = format!("Replaced 60 occurrence(s) in /tmp/f.txt\n{diff}");
        assert!(
            result.contains("-line 1\n") && result.contains("+LINE 1"),
            "the whole diff rides back: {result}"
        );
        assert!(
            !result.contains("diff omitted"),
            "nothing is collapsed: {result}"
        );
        assert!(result.contains("--- a/tmp/f.txt"), "{result}");

        // An unchanged file still renders nothing at all.
        assert!(unified_diff("/tmp/f.txt", &old, &old).is_empty());
    }

    /// The diff rides back in full even when a hook failed: a hook warning
    /// (and, by the same path, an LSP-diagnostics block) still sits with the
    /// result — an edit must not bury a "this no longer builds".
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_result_carries_the_full_diff_and_the_hook_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.txt");
        std::fs::write(
            &path,
            (1..=60).map(|n| format!("old {n}\n")).collect::<String>(),
        )
        .unwrap();
        let mut c = ToolContext::new(dir.path());
        c.hooks = std::sync::Arc::new(vec![crate::Hook {
            on: "edit".to_string(),
            glob: None,
            run: "exit 7".to_string(),
            timeout_secs: crate::DEFAULT_HOOK_TIMEOUT_SECS,
        }]);
        c.mark_read(&path);

        let out = crate::EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "old ",
                    "new_string": "new ",
                    "replace_all": true,
                }),
                &c,
            )
            .await
            .unwrap();

        assert!(out.contains("Replaced 60 occurrence(s)"), "{out}");
        assert!(
            out.contains("[hook `exit 7` failed"),
            "hook note lost: {out}"
        );
        assert!(
            out.contains("-old 1") && out.contains("+new 1"),
            "the FULL diff rides back in the result — the user sees it: {out}"
        );
        assert!(
            !out.contains("diff omitted"),
            "nothing is collapsed any more: {out}"
        );
        assert!(!out.contains("a//"), "no doubled slash: {out}");
    }

    /// A file with a line over `MAX_LINE` bytes is `Partial` after a normal read
    /// (which clips that line every time), so `write` refuses — but the refusal
    /// now points at `read` with `full: true`, and reading it that way marks the
    /// file fully read and unblocks the rewrite. Previously this was a dead end
    /// that only `edit`/`shell` (or the delete-then-recreate hole) could work
    /// around.
    #[tokio::test]
    async fn an_over_long_line_file_is_rewritable_after_a_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big_line.txt");
        // One line over MAX_LINE *and* the whole file over the default output
        // budget (5120), so a normal read is defeated on both counts.
        let long_line = "x".repeat(MAX_LINE + 5000);
        std::fs::write(&path, format!("{long_line}\n")).unwrap();
        let ctx = ToolContext::new(dir.path());

        // A normal read clips the over-long line → Partial → write refuses,
        // pointing at the `full: true` escape hatch (and edit/shell).
        crate::ReadTool
            .execute(serde_json::json!({"path": "big_line.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);
        let err = WriteTool
            .execute(
                serde_json::json!({"path": "big_line.txt", "content": "replacement\n"}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("full: true"),
            "the refusal points at the escape hatch: {err}"
        );

        // Reading it in full (no clipping) marks it fully read...
        crate::ReadTool
            .execute(
                serde_json::json!({"path": "big_line.txt", "full": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Fresh);
        // ...so the rewrite now goes through instead of dead-ending.
        WriteTool
            .execute(
                serde_json::json!({"path": "big_line.txt", "content": "replacement\n"}),
                &ctx,
            )
            .await
            .expect("write succeeds after a full read");
    }

    /// The software path-guard: a write outside the policy's writable roots is
    /// refused, and the refusal *names the roots* so the model can retarget
    /// instead of retrying blind.
    #[tokio::test]
    async fn write_outside_roots_is_refused_and_names_the_roots() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ctx = crate::sandbox::confined_ctx(cwd.path(), crate::SandboxMode::Write);

        let target = outside.path().join("escaped.txt");
        let err = WriteTool
            .execute(
                serde_json::json!({"path": target.to_str().unwrap(), "content": "x"}),
                &ctx,
            )
            .await
            .expect_err("a write outside the roots must be refused")
            .to_string();
        assert!(err.contains("sandbox: refusing to write"), "{err}");
        assert!(err.contains("You may write only under"), "{err}");
        assert!(
            err.contains(
                &crate::canonicalize_nearest(cwd.path())
                    .display()
                    .to_string()
            ),
            "the refusal must name the writable root: {err}"
        );
        assert!(!target.exists(), "nothing may be written");

        WriteTool
            .execute(
                serde_json::json!({"path": "inside.txt", "content": "x"}),
                &ctx,
            )
            .await
            .expect("a write under the cwd root is allowed");
        assert!(cwd.path().join("inside.txt").exists());
    }

    /// The scratch and tool-output dirs are writable roots by construction —
    /// drop either and overflow-spill or throwaway scratch work breaks.
    #[tokio::test]
    async fn scratch_and_tool_output_stay_writable_under_write_mode() {
        let cwd = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(cwd.path().to_path_buf());
        ctx.sandbox = std::sync::Arc::new(crate::SandboxPolicy::for_agent(
            crate::SandboxMode::Write,
            cwd.path(),
            &[],
        ));

        for dir in [
            crate::sandbox::session_scratch_dir().to_path_buf(),
            crate::tool_output_dir(),
        ] {
            let target = dir.join("hrdr-guard-probe.txt");
            WriteTool
                .execute(
                    serde_json::json!({"path": target.to_str().unwrap(), "content": "x"}),
                    &ctx,
                )
                .await
                .unwrap_or_else(|e| panic!("{} should be writable: {e}", target.display()));
            let _ = std::fs::remove_file(&target);
        }
    }

    /// Mode `None` is the shipped default until the policy is wired into
    /// `Agent::new`: nothing is confined, so this pins "no observable change".
    #[tokio::test]
    async fn mode_none_changes_nothing() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());
        assert_eq!(ctx.sandbox.mode, crate::SandboxMode::None);

        let target = outside.path().join("free.txt");
        WriteTool
            .execute(
                serde_json::json!({"path": target.to_str().unwrap(), "content": "x"}),
                &ctx,
            )
            .await
            .expect("an unconfined context writes anywhere");
        assert!(target.exists());
    }
}
