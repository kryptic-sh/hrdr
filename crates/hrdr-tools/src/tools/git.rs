//! `git`: a read-only window onto the repository.
//!
//! Everything here is reachable through `bash`, but only agents that *have* a
//! shell — and a shell answer arrives as unstructured text the model must parse
//! around. Exposing the read-only subcommands as their own tool means
//! `explore` and `review`, which have no shell at all, can finally look at
//! history, blame and the working diff.
//!
//! The subcommand is an **allow-list**, not a filter: `git` runs
//! `git <subcommand> …` directly, never through a shell, so there is no
//! quoting or `;`-injection surface. Nothing here can mutate the repository —
//! no `commit`, `checkout`, `reset`, `push`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

/// The subcommands this tool will run. All are read-only: none writes to the
/// index, the working tree, the object store, or a remote.
const ALLOWED: &[&str] = &[
    "status", "diff", "log", "show", "blame", "branch", "describe", "remote", "shortlog",
];

/// Flags refused for **every** subcommand: each can run a program of the
/// caller's choosing, or write a file, turning a read-only command into an
/// arbitrary one.
const FORBIDDEN_ANY: &[&str] = &[
    // `-c core.pager=sh -c evil` / `--config-env=…` inject config for the run.
    "-c",
    "--config-env",
    // Run an external program as part of the diff.
    "--ext-diff",
    "--textconv",
    "--exec",
    // `git diff --output=FILE` writes to the filesystem.
    "--output",
    // Reach a remote, running its side's program.
    "--upload-pack",
    "--receive-pack",
];

/// Flags refused for `diff`/`blame` specifically: `--no-index` turns `diff`
/// into a generic two-arbitrary-paths file comparator (reads anything on
/// disk, not just tracked repo content); `--contents` feeds `blame` a file
/// from *outside* the repo to attribute against the history — both are file
/// read escapes, not repository inspection.
const FORBIDDEN_DIFF_BLAME: &[&str] = &["--no-index", "--contents"];

/// Flags refused for `branch` specifically: the subcommand reads by default but
/// deletes, renames or copies with these. (`-M`/`-m` mean *move detection* on
/// `blame`/`diff`, which is harmless — hence the per-subcommand list.)
const FORBIDDEN_BRANCH: &[&str] = &[
    "-d",
    "-D",
    "--delete",
    "-m",
    "-M",
    "--move",
    "-c",
    "-C",
    "--copy",
    "--force",
    "-f",
    "--edit-description",
    "--set-upstream-to",
    "-u",
    "--unset-upstream",
];

/// Single-character flags that, bundled into one dash-prefixed short-flag
/// group (e.g. `-fD`, a `git` convention this parser must not be fooled by),
/// make the subcommand unsafe. Checked against every letter of every `-xyz`
/// style argument, not just whole-flag matches.
const FORBIDDEN_BRANCH_SHORT_CHARS: &[char] = &['d', 'D', 'm', 'M', 'c', 'C', 'f', 'u'];

/// Whether `arg` is `flag`, or `flag=value`.
fn matches_flag(arg: &str, flag: &str) -> bool {
    arg == flag || (arg.starts_with(flag) && arg.as_bytes().get(flag.len()) == Some(&b'='))
}

/// Whether `arg` is a bundled short-flag group (`-fD`, `-Dx`, …) that contains
/// one of `chars` — so `-fD` is caught as containing `-D` even though it isn't
/// a whole-argument match. Long options (`--foo`) and the bare `-` are never
/// bundles.
fn bundled_short_flag_contains(arg: &str, chars: &[char]) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('-') {
        return false; // "-" alone, or a long "--flag"
    }
    rest.chars().any(|c| chars.contains(&c))
}

/// The refused flag in `args` for `sub`, if any.
fn forbidden_flag<'a>(sub: &str, args: &'a [String]) -> Option<&'a str> {
    let extra: &[&str] = match sub {
        "branch" => FORBIDDEN_BRANCH,
        "diff" | "blame" => FORBIDDEN_DIFF_BLAME,
        _ => &[],
    };
    args.iter().map(String::as_str).find(|arg| {
        FORBIDDEN_ANY
            .iter()
            .chain(extra)
            .any(|f| matches_flag(arg, f))
            || (sub == "branch" && bundled_short_flag_contains(arg, FORBIDDEN_BRANCH_SHORT_CHARS))
    })
}

/// Whether a `diff`/`blame` path argument reads outside the workspace, resolved
/// the same way every other tool confines paths: turn it into a full path and
/// check it still sits under `cwd`.
///
/// Two steps, because neither alone is enough:
/// 1. A lexical `..` check catches an out-of-tree target even when it doesn't
///    exist (so it behaves identically on every OS / CI runner, where e.g.
///    `/etc/passwd` is absent). `Path::components` normalises `/` and `\`.
/// 2. Otherwise resolve the arg against `cwd` ([`resolve_under`] handles both
///    Unix- and Windows-absolute spellings) and canonicalize both sides
///    ([`canonicalize_nearest`] resolves symlinks and the nearest existing
///    ancestor), then require the result to stay under `cwd`.
///
/// This replaces a hand-rolled `is_absolute` heuristic that missed Unix paths
/// on Windows, and it additionally catches symlink escapes. A git revision or
/// ref (`HEAD`, `main`, `a..b`) resolves to a non-existent path *inside* `cwd`,
/// so it passes untouched. Flags (`-`-prefixed) are skipped by the caller.
fn escapes_workspace(arg: &str, cwd: &std::path::Path) -> bool {
    use std::path::Component;
    if std::path::Path::new(arg)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return true;
    }
    let resolved = crate::canonicalize_nearest(&crate::resolve_under(cwd, arg));
    !resolved.starts_with(crate::canonicalize_nearest(cwd))
}

/// For `diff`/`blame`: reject any non-flag argument that resolves outside the
/// workspace — combined with the `--no-index`/`--contents` flag bans, this keeps
/// both subcommands reading only content under the cwd.
fn escaping_path_arg<'a>(sub: &str, args: &'a [String], cwd: &std::path::Path) -> Option<&'a str> {
    if sub != "diff" && sub != "blame" {
        return None;
    }
    args.iter()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-') && escapes_workspace(arg, cwd))
}

/// For `remote`, accept only complete local read-only grammar. Options cannot
/// prefix another subcommand, and `show` must disable network queries.
fn check_remote_args(args: &[String]) -> Result<(), &'static str> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["-v"] | ["--verbose"] => Ok(()),
        ["get-url", name] if !name.starts_with('-') => Ok(()),
        ["get-url", "--all", name] if !name.starts_with('-') => Ok(()),
        ["show", "-n"] | ["show", "--no-query"] => Ok(()),
        ["show", "-n", name] | ["show", "--no-query", name] if !name.starts_with('-') => Ok(()),
        _ => Err(
            "`git remote` only allows the read-only forms: no args, -v, get-url [--all] \
             <name>, show -n [name] — mutating and networking forms are refused",
        ),
    }
}

/// For `branch`: refuse a bare `git branch <name>` (creates a branch) — only
/// the listing forms (no args, or args that are all flags) are read-only.
fn check_branch_args(args: &[String]) -> Result<(), &'static str> {
    if args.iter().any(|a| !a.starts_with('-')) {
        return Err(
            "`git branch <name>` creates a branch — this tool only lists branches \
             (no args, or flags like -a/-r/-v)",
        );
    }
    Ok(())
}

/// A non-flag operand that would dump a **secret file's whole content**:
/// `git show <rev>:<secret>` (the raw file at a revision) or `git blame <secret>`
/// (every line of the file). The read/grep tools refuse these files via
/// [`crate::secret_file_reason`]; the git tool must too, or a read-only
/// `explore`/`review` sub-agent (which has `git` but no shell) could read a
/// credential out of history. Diffs that merely *touch* a secret are redacted in
/// the output instead (see [`redact_secret_diffs`]); this refuses only the forms
/// that reveal the entire file.
fn secret_content_operand<'a>(sub: &str, args: &'a [String]) -> Option<(&'a str, &'static str)> {
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        // `<tree-ish>:<path>` — the part after the last `:` names a path in the
        // tree (`rsplit` so a stage prefix like `:0:.env` still resolves to
        // `.env`). `git show HEAD:.env` dumps that file's content verbatim.
        if let Some((_, path)) = arg.rsplit_once(':')
            && let Some(reason) = crate::secret_file_reason(std::path::Path::new(path))
        {
            return Some((arg, reason));
        }
        // `git blame <path>` prints every line of the file, annotated.
        if sub == "blame"
            && let Some(reason) = crate::secret_file_reason(std::path::Path::new(arg))
        {
            return Some((arg, reason));
        }
    }
    None
}

/// The file path a diff-section header names, if `line` starts one:
/// `diff --git a/<p> b/<p>` (prefer the `b/` destination), or a merge diff's
/// `diff --cc <p>` / `diff --combined <p>`. `None` for any other line.
fn diff_section_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        if let Some(idx) = rest.rfind(" b/") {
            return Some(rest[idx + 3..].to_string());
        }
        return rest
            .strip_prefix("a/")
            .map(|p| p.split(' ').next().unwrap_or(p).to_string());
    }
    for pre in ["diff --cc ", "diff --combined "] {
        if let Some(rest) = line.strip_prefix(pre) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Redact the hunk body of any diff section whose file is a credential/secret
/// store, keeping the section header so the model still sees *that* the file
/// changed — just not its content. Covers `diff`, `show`, and `log -p` output;
/// a no-op on plain `status`/`log`/`branch` output (no diff headers).
fn redact_secret_diffs(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(path) = diff_section_path(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        out.push_str(line);
        out.push('\n');
        if crate::secret_file_reason(std::path::Path::new(&path)).is_some() {
            out.push_str(
                "[redacted: this file is a credential/secret store — its diff is withheld]\n",
            );
            // Drop the rest of this section (up to the next `diff` header / EOF).
            while let Some(peek) = lines.peek() {
                if diff_section_path(peek).is_some() {
                    break;
                }
                lines.next();
            }
        }
    }
    out
}

pub struct GitTool;

#[derive(Deserialize)]
struct GitArgs {
    subcommand: String,
    #[serde(default)]
    args: Vec<String>,
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &'static str {
        "git"
    }

    fn read_only(&self) -> bool {
        // Nothing in ALLOWED mutates the repository, so a read-only agent may
        // use it. Keep that true if you add a subcommand.
        true
    }

    fn description(&self) -> &'static str {
        "Inspect the git repository: status, diff, log, show, blame, branch, describe, \
         remote, shortlog. Read-only — it cannot commit, checkout, reset or push. Pass the \
         subcommand's own flags in `args`, e.g. subcommand=\"log\", args=[\"-5\", \"--oneline\"], \
         or subcommand=\"diff\", args=[\"--staged\"]."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "subcommand": {
                    "type": "string",
                    "enum": ALLOWED,
                    "description": "The read-only git subcommand to run."
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments for the subcommand, one per element (not a single joined string)."
                }
            },
            "required": ["subcommand"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GitArgs = crate::tool_args("git", args)?;
        let sub = a.subcommand.trim();
        if !ALLOWED.contains(&sub) {
            bail!(
                "`git {sub}` is not available — this tool is read-only. Allowed: {}",
                ALLOWED.join(", ")
            );
        }
        if let Some(bad) = forbidden_flag(sub, &a.args) {
            bail!("`{bad}` is not allowed: it can modify the repository or run a program");
        }
        if let Some(bad) = escaping_path_arg(sub, &a.args, &ctx.cwd) {
            bail!(
                "`{bad}` is not allowed: `git {sub}` only reads paths inside the workspace \
                 (no absolute paths, no `..` escapes)"
            );
        }
        if sub == "remote"
            && let Err(msg) = check_remote_args(&a.args)
        {
            bail!(msg);
        }
        if sub == "branch"
            && let Err(msg) = check_branch_args(&a.args)
        {
            bail!(msg);
        }
        if let Some((bad, reason)) = secret_content_operand(sub, &a.args) {
            bail!(
                "`{bad}` would reveal {reason} — the git tool won't dump a \
                 credential/secret file's content"
            );
        }

        let mut cmd = tokio::process::Command::new("git");
        cmd.arg(sub)
            .args(&a.args)
            .current_dir(&ctx.cwd)
            // A pager would hang waiting for a terminal that isn't there.
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0");
        // `run_capped_output` nulls stdin, sets `kill_on_drop` (so Esc actually
        // stops a `git log -p` on a huge repo), and caps how much stdout is
        // buffered — `output()` would hold the entire diff/log in memory before
        // the byte cap below ran. 5× the display budget is generous headroom;
        // anything that fits is identical to what `output()` returned.
        let cap = ctx.max_output.saturating_mul(5).max(ctx.max_output);
        let (status, stdout_bytes, stderr_bytes, over_cap) =
            super::run_capped_output(cmd, cap, cap)
                .await
                .context("running git (is it installed?)")?;

        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        // When output overflowed the cap the child was killed, so its exit
        // status is a signal death, not a git error. Treat it as a valid (large)
        // result — it flows through the redaction + overflow-file path below,
        // exactly like a diff that fit. Only a genuine non-success (real git
        // failure) becomes an error.
        if !over_cap && !status.success() {
            let msg = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            // Cap the error text so a failure that still printed a lot of stdout
            // can't itself blow the model's context.
            let capped = crate::truncate(msg, ctx.max_output);
            bail!("git {sub} failed: {capped}");
        }
        if stdout.trim().is_empty() {
            return Ok("(no output)".to_string());
        }
        // Redact any diff hunk for a secret file before it reaches the model
        // (a `diff`/`log -p`/`show` can otherwise echo `.env`'s contents) — the
        // saved overflow file below therefore holds the redacted text too.
        let body = redact_secret_diffs(&stdout);
        // Big output (`log -p`, a wide `diff`, a long `show`) is saved whole to a
        // file the model can `grep`/`read`, same as `bash`/`grep` — so it isn't
        // byte-truncated and lost. Small output comes straight back.
        Ok(crate::truncate_saved(
            &body,
            ctx.max_output,
            ctx.max_output_lines,
            crate::TruncateSide::Head,
            "git",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A repo with one commit, so `log`/`status` have something to say.
    async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "first"]);
        dir
    }

    #[tokio::test]
    async fn runs_read_only_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());

        let log = GitTool
            .execute(json!({"subcommand": "log", "args": ["--oneline"]}), &ctx)
            .await
            .unwrap();
        assert!(log.contains("first"), "{log}");

        // An unstaged change shows up in status and diff.
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let status = GitTool
            .execute(json!({"subcommand": "status", "args": ["--short"]}), &ctx)
            .await
            .unwrap();
        assert!(status.contains("a.txt"), "{status}");
        let diff = GitTool
            .execute(json!({"subcommand": "diff"}), &ctx)
            .await
            .unwrap();
        assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
    }

    /// The subcommand is an allow-list: writing commands are refused, and so is
    /// anything that isn't a git subcommand at all.
    #[tokio::test]
    async fn refuses_writing_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        for sub in ["commit", "checkout", "reset", "push", "clean", "rm"] {
            let err = GitTool
                .execute(json!({"subcommand": sub}), &ctx)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("read-only"), "{sub}: {err}");
        }
    }

    /// Flags that would let a read-only subcommand write, or run a program, are
    /// refused even though the subcommand itself is allowed.
    #[tokio::test]
    async fn refuses_dangerous_flags_on_allowed_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        // `git branch -D main` deletes a branch.
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["-D", "main"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        // `-c` sets config for the invocation, which can point at a program.
        let err = GitTool
            .execute(
                json!({"subcommand": "log", "args": ["-c", "core.pager=sh -c evil"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// Arguments go to `git` directly, never through a shell: a `;` is just a
    /// bad argument, not a second command.
    #[tokio::test]
    async fn arguments_are_not_shell_interpreted() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let marker = dir.path().join("pwned");
        let injected = format!("; touch {}", marker.display());
        let err = GitTool
            .execute(json!({"subcommand": "log", "args": [injected]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git log failed"), "{err}");
        assert!(!marker.exists(), "the shell never saw it");
    }

    /// A failing git command surfaces git's own message rather than an empty ok.
    #[tokio::test]
    async fn a_failure_is_an_error_not_an_empty_success() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "show", "args": ["nosuchref"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git show failed"), "{err}");
    }

    /// A forbidden flag bundled into a short-flag group (`-fD`) is caught the
    /// same as a standalone `-D` or `-f` — the allow-list can't be defeated by
    /// packing flags together.
    #[tokio::test]
    async fn bundled_short_flags_are_caught() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["-fD", "main"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// `git diff --no-index` and `git blame --contents` are file-read escapes
    /// (they read arbitrary filesystem paths, not tracked repo content) and
    /// are refused even though `diff`/`blame` are otherwise allowed.
    #[tokio::test]
    async fn diff_no_index_and_blame_contents_are_refused() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "diff", "args": ["--no-index", "/etc/passwd", "a.txt"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");

        let err = GitTool
            .execute(
                json!({"subcommand": "blame", "args": ["--contents", "/etc/passwd", "a.txt"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// The resolve-then-prefix guard rejects real escapes (absolute paths, `..`)
    /// but leaves in-tree paths *and* git revisions/refs — which resolve to
    /// non-existent paths under cwd — untouched.
    #[test]
    fn escapes_workspace_allows_in_tree_and_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert!(!escapes_workspace("src/main.rs", cwd));
        assert!(!escapes_workspace("HEAD", cwd));
        assert!(!escapes_workspace("main..feature", cwd)); // a range, not a `..` path
        assert!(!escapes_workspace("v1.0", cwd));
        assert!(escapes_workspace("../secret", cwd));
        assert!(escapes_workspace("/etc/passwd", cwd));
    }

    /// `diff`/`blame` args that are absolute paths or escape the workspace via
    /// `..` are refused, even without `--no-index`/`--contents`.
    #[tokio::test]
    async fn diff_and_blame_reject_paths_outside_the_workspace() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "diff", "args": ["/etc/passwd"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");

        let err = GitTool
            .execute(
                json!({"subcommand": "blame", "args": ["../../etc/passwd"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");
    }

    /// `git remote` only allows the read-only forms — mutating/networking
    /// sub-subcommands are refused.
    #[tokio::test]
    async fn remote_only_allows_read_only_forms() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        for args in [
            vec!["add", "origin", "https://evil.example/repo.git"],
            vec!["-v", "add", "origin", "https://evil.example/repo.git"],
            vec!["--verbose", "remove", "origin"],
            vec!["remove", "origin"],
            vec!["rm", "origin"],
            vec!["set-url", "origin", "https://evil.example/repo.git"],
            vec!["rename", "origin", "up"],
            vec!["update"],
            vec!["prune"],
            vec!["set-head", "origin", "-a"],
        ] {
            let err = GitTool
                .execute(json!({"subcommand": "remote", "args": args.clone()}), &ctx)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("read-only forms"),
                "{args:?}: {err}"
            );
        }
        // The read-only forms still work.
        for args in [
            vec![],
            vec!["-v".to_string()],
            vec!["show".to_string(), "-n".to_string()],
        ] {
            GitTool
                .execute(json!({"subcommand": "remote", "args": args.clone()}), &ctx)
                .await
                .unwrap_or_default(); // no remotes configured — empty output is fine
        }
    }

    /// A bare `git branch <name>` creates a branch — refused; only listing
    /// forms (no args, or all-flag args) are allowed.
    #[tokio::test]
    async fn bare_branch_name_is_refused() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["new-branch"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only lists branches"), "{err}");
        // Listing (no args, or flags only) still works.
        GitTool
            .execute(json!({"subcommand": "branch", "args": ["-a"]}), &ctx)
            .await
            .unwrap();
    }

    /// A repo with a committed `.env`, to exercise the secret-file guards.
    async fn repo_with_secret() -> tempfile::TempDir {
        let dir = repo().await;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap()
        };
        std::fs::write(dir.path().join(".env"), "API_KEY=supersecret123\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "add env"]);
        dir
    }

    /// `git show HEAD:.env` dumps the file verbatim — refused, and the secret is
    /// not echoed in the refusal message.
    #[tokio::test]
    async fn show_of_a_secret_file_is_refused() {
        let dir = repo_with_secret().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "show", "args": ["HEAD:.env"]}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("credential") || err.contains("secret"),
            "{err}"
        );
        assert!(
            !err.contains("supersecret"),
            "the secret must not leak: {err}"
        );
    }

    /// `git blame .env` prints every line of the file — refused.
    #[tokio::test]
    async fn blame_of_a_secret_file_is_refused() {
        let dir = repo_with_secret().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "blame", "args": [".env"]}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("credential") || err.contains("secret"),
            "{err}"
        );
    }

    /// A `diff` touching both a secret and a normal file shows the normal file's
    /// change and that the secret changed, but withholds the secret's content.
    #[tokio::test]
    async fn diff_redacts_a_secret_files_hunk_but_keeps_others() {
        let dir = repo_with_secret().await;
        let ctx = ToolContext::new(dir.path());
        std::fs::write(dir.path().join(".env"), "API_KEY=rotated_secret_456\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();

        let out = GitTool
            .execute(json!({"subcommand": "diff", "args": []}), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("a.txt") && out.contains("changed"),
            "the normal file's change stays visible: {out}"
        );
        assert!(
            out.contains(".env") && out.contains("redacted"),
            "the model should see .env changed, redacted: {out}"
        );
        assert!(
            !out.contains("rotated_secret_456"),
            "the secret content must not leak: {out}"
        );
    }

    /// The guards don't over-block: `show` of a non-secret path still works.
    #[tokio::test]
    async fn show_of_a_normal_path_still_works() {
        let dir = repo_with_secret().await;
        let ctx = ToolContext::new(dir.path());
        let out = GitTool
            .execute(json!({"subcommand": "show", "args": ["HEAD:a.txt"]}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("one"), "{out}");
    }

    /// Big git output is saved whole to a file the model can `grep`/`read`, not
    /// byte-truncated and lost — the same overflow handling `bash`/`grep` get.
    #[tokio::test]
    async fn large_output_is_saved_to_a_file() {
        let dir = repo().await;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap()
        };
        // A file with more lines than the display cap, so `show` overflows.
        let big: String = (1..=(crate::DEFAULT_MAX_OUTPUT_LINES + 100))
            .map(|n| format!("line {n}\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "big"]);

        let ctx = ToolContext::new(dir.path());
        let out = GitTool
            .execute(
                json!({"subcommand": "show", "args": ["HEAD:big.txt"]}),
                &ctx,
            )
            .await
            .unwrap();
        // The model gets a pointer to the saved file, not the whole flood.
        assert!(out.contains("saved to"), "big output must be saved: {out}");
        assert!(out.contains("grep"), "{out}");
        assert!(out.len() < big.len(), "the inline output is bounded: {out}");
    }
}
