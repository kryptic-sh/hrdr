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

use crate::{Tool, ToolContext, truncate};

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
    "--exec",
    // `git diff --output=FILE` writes to the filesystem.
    "--output",
    // Reach a remote, running its side's program.
    "--upload-pack",
    "--receive-pack",
];

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

/// Whether `arg` is `flag`, or `flag=value`.
fn matches_flag(arg: &str, flag: &str) -> bool {
    arg == flag || (arg.starts_with(flag) && arg.as_bytes().get(flag.len()) == Some(&b'='))
}

/// The refused flag in `args` for `sub`, if any.
fn forbidden_flag<'a>(sub: &str, args: &'a [String]) -> Option<&'a str> {
    let extra: &[&str] = if sub == "branch" {
        FORBIDDEN_BRANCH
    } else {
        &[]
    };
    args.iter().map(String::as_str).find(|arg| {
        FORBIDDEN_ANY
            .iter()
            .chain(extra)
            .any(|f| matches_flag(arg, f))
    })
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

        let out = tokio::process::Command::new("git")
            .arg(sub)
            .args(&a.args)
            .current_dir(&ctx.cwd)
            // A pager would hang waiting for a terminal that isn't there.
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .await
            .context("running git (is it installed?)")?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let msg = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!("git {sub} failed: {msg}");
        }
        let body = if stdout.trim().is_empty() {
            "(no output)".to_string()
        } else {
            stdout.into_owned()
        };
        Ok(truncate(&body, ctx.max_output))
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
}
