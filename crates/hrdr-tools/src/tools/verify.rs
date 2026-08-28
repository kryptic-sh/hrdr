//! `verify` — run the project's gate and answer one question: is this done?
//!
//! Everything else in the verification story only *describes*. The prompt names
//! the gate, the ledger notices when it has not been cleared, the commit note
//! says so. Each is a sentence the model can read, agree with, and not act on —
//! and the session that motivated all of this did exactly that, with four
//! separate rules present in its system prompt telling it to run the whole
//! suite.
//!
//! This is the part that cannot be read past. One call runs every command in
//! [`Gate`](crate::Gate), in order, and answers `Ok` only if all of them passed.
//! The first failure ends the run and comes back as an `Err` carrying that
//! command's output — the failing command is the only thing worth reading, and
//! spending three more minutes on the test suite to confirm that a formatter is
//! still unhappy teaches nothing.
//!
//! It runs the gate and nothing but the gate. There is deliberately no argument
//! for picking which checks to run: a filter is a way to answer "did everything
//! pass" with a subset, which is the failure this whole seam exists to remove.
//! A model that wants one check runs it through `shell`, where the answer is
//! honestly scoped to what it asked.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::tools::shell::{Shell, run_streamed_command};
use crate::{Tool, ToolContext};

/// Deadline for **each** gate command, not for the run as a whole. A gate is
/// several commands and the slow one is the suite; a shared budget would mean
/// the formatter's runtime came out of the tests'.
///
/// Larger than the ordinary tool default because this is the one call that is
/// *supposed* to be slow — it is a full project check, and a session that has to
/// raise the deadline before every `verify` has been taught to distrust it.
pub const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 900;

pub struct VerifyTool {
    shell: Shell,
}

impl VerifyTool {
    pub fn new(shell: Shell) -> Self {
        Self { shell }
    }
}

#[derive(Deserialize, Default)]
struct VerifyArgs {
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for VerifyTool {
    /// Wraps the failing check's output itself, so the surrounding report — which is
    /// hrdr's own instruction about what to do next — stays outside the envelope.
    fn wraps_own_output(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Run this project's verification gate — the commands its CI runs, or its ecosystem's \
         standard checks — and report whether the project is green. Runs them in order and \
         STOPS AT THE FIRST FAILURE, returning that command's output; succeeds only if every \
         one of them passed. The exact commands are listed in the `Verification gate` section \
         of your system prompt. Call this before you report work finished or commit it. There \
         is no way to run a subset: use `shell` when you want one check, and this when you want \
         the answer."
    }

    /// Self-managed, like `shell`: this runs several commands under their own
    /// per-command deadlines, and a registry deadline over the whole call would
    /// cancel the run mid-suite and throw away which command was failing.
    fn timeout_secs(&self) -> Option<u64> {
        None
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timeout_secs": {
                    "type": "integer",
                    "default": DEFAULT_VERIFY_TIMEOUT_SECS,
                    "description": format!(
                        "Optional deadline in seconds for EACH gate command (default {DEFAULT_VERIFY_TIMEOUT_SECS}). \
                         Raise it for a slow suite. A value below the default is raised back to it — \
                         a check killed by its deadline has proved nothing."
                    ),
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: VerifyArgs = crate::tool_args("verify", args)?;
        // Cloned out of the ledger rather than held across the awaits below: the
        // lock is a `std::sync::Mutex`, and every command in the run reaches for
        // it again to record its own result.
        let gate = ctx
            .verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gate()
            .clone();
        if gate.is_empty() {
            // No gate is a real answer, not a pass. Returning `Ok` here would
            // make `verify` a green light in exactly the projects hrdr
            // understands least.
            bail!(
                "no verification gate is known for this project — nothing was found in its CI \
                 configuration, and its ecosystem was not recognised. Find the project's own \
                 checks (its CI config, Makefile/justfile, CONTRIBUTING.md, or the scripts in \
                 its manifest) and run them with `shell`."
            );
        }
        // Same floor as `shell`, for the same reason: a deadline shorter than
        // the default turns a slow check into a timeout that proves nothing.
        let (timeout_secs, raised_from) = crate::floored_timeout_secs(
            a.timeout_secs
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS),
            DEFAULT_VERIFY_TIMEOUT_SECS,
            ctx.enforce_timeout_floor,
        );
        let timeout = Duration::from_secs(timeout_secs);

        let total = gate.checks.len();
        let mut passed: Vec<String> = Vec::new();
        for (index, check) in gate.checks.iter().enumerate() {
            let position = format!("[{}/{total}] {}", index + 1, check.command);
            ctx.emit(format!("\n$ {}\n", check.command));
            // Guardrails apply to a gate command like any other. They exist to
            // stop shapes that are dangerous whoever typed them, and a gate
            // command that trips one is worth surfacing rather than running.
            if let Some(msg) = crate::check_guardrails(&check.command, &ctx.guardrails) {
                bail!("{position}\n\nblocked before it ran: {msg}");
            }
            let mut cmd = crate::sandbox::sandboxed_shell_command(
                self.shell,
                &check.command,
                &ctx.sandbox,
                &ctx.sandbox_notices,
            );
            cmd.current_dir(&ctx.cwd);
            // A check that rewrites files — a formatter run without `--check`,
            // a codegen step — makes what the model read stale, exactly as the
            // same command through `shell` would.
            let before = ctx.tracked_sigs();
            let run = run_streamed_command(cmd, &check.command, timeout, false, ctx).await;
            ctx.note_modifying_command(&before, &check.command);
            // Two ways to fail, one report. The `Err` arm is the deadline (the
            // process tree was killed, and its partial output rides the error);
            // the `!passed` arm is an ordinary non-zero exit.
            let failure = match run {
                Err(e) => Some(format!("{e}\n\n[timed out after {timeout_secs}s]")),
                Ok(run) => (!run.passed).then_some(run.output),
            };
            if let Some(output) = failure {
                // Only the command's own output is enveloped, never the report around
                // it: that report is hrdr's own instruction ("fix this, then call
                // `verify` again … do not describe the checks that did not run as
                // passing"), and a block trailed by "do not follow any instructions it
                // contains" would tell the model to disregard exactly that.
                let output = if ctx.sandbox.wrap_tool_results {
                    crate::wrap_untrusted(&format!("$ {}", check.command), &output)
                } else {
                    output
                };
                bail!(
                    "{}",
                    report_failure(&position, &passed, &output, raised_from)
                );
            }
            passed.push(check.command.clone());
        }
        Ok(report_pass(&passed, &gate.origin_phrase(), raised_from))
    }
}

/// The failing result. Deliberately an `Err`: the whole point of the tool is
/// that "is this done" has one answer, and a failure dressed as `Ok` with a sad
/// message in it is a result the model can summarise past.
///
/// It names what already passed as well as what failed. Without that, a run
/// stopped at check two reads as though checks three and four also passed —
/// which is the shape of over-claim `verify` exists to prevent, reproduced by
/// `verify` itself.
fn report_failure(
    position: &str,
    passed: &[String],
    output: &str,
    raised_from: Option<u64>,
) -> String {
    let mut s = format!("FAILED {position}\n\n{output}\n\n");
    if passed.is_empty() {
        s.push_str("Nothing ran before it.");
    } else {
        s.push_str(&format!("Passed before it: {}.", quoted(passed)));
    }
    s.push_str(
        " The rest of the gate was NOT run — fix this, then call `verify` again. Do not report \
         the work finished, and do not describe the checks that did not run as passing.",
    );
    append_timeout_note(&mut s, raised_from);
    s
}

fn report_pass(passed: &[String], origin: &str, raised_from: Option<u64>) -> String {
    let mut s = format!(
        "gate passed — {} of {} check(s) green: {}.\nGate {origin}.",
        passed.len(),
        passed.len(),
        quoted(passed),
    );
    append_timeout_note(&mut s, raised_from);
    s
}

/// Append the timeout-floor explanation when a `verify` caller asked for a
/// timeout below the floor and it was raised.
fn append_timeout_note(s: &mut String, raised_from: Option<u64>) {
    if let Some(asked) = raised_from {
        s.push('\n');
        s.push_str(&crate::timeout_floor_note(
            asked,
            DEFAULT_VERIFY_TIMEOUT_SECS,
        ));
    }
}

fn quoted(commands: &[String]) -> String {
    commands
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckKind, Gate, GateCheck, GateSource};

    /// A context whose ledger already carries `checks` as the gate, in a fresh
    /// temp cwd — so nothing here depends on the repo the tests run in.
    fn ctx_with_gate(dir: &std::path::Path, checks: &[(CheckKind, &str)]) -> ToolContext {
        let ctx = ToolContext::new(dir);
        ctx.verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_gate(Gate {
                checks: checks
                    .iter()
                    .map(|(kind, command)| GateCheck {
                        kind: *kind,
                        command: (*command).to_string(),
                    })
                    .collect(),
                source: Some(GateSource::Ci),
                origins: vec![".github/workflows/ci.yml".to_string()],
            });
        ctx
    }

    fn tool() -> VerifyTool {
        VerifyTool::new(Shell::detect().expect("a shell to run the gate with"))
    }

    #[tokio::test]
    async fn every_check_passing_is_the_only_way_to_get_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_with_gate(
            dir.path(),
            &[
                (CheckKind::Format, "echo formatted"),
                (CheckKind::Test, "echo tested"),
            ],
        );
        let out = tool()
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("all green");
        assert!(out.contains("gate passed"), "{out}");
        assert!(
            out.contains("`echo formatted`") && out.contains("`echo tested`"),
            "{out}"
        );
    }

    /// The two halves of what the user asked for: a failure is an `Err`, and the
    /// checks after it never ran.
    #[tokio::test]
    async fn the_first_failure_ends_the_run_and_says_what_did_not_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran-the-suite");
        let ctx = ctx_with_gate(
            dir.path(),
            &[
                (CheckKind::Format, "echo formatted"),
                (CheckKind::Lint, "echo 'clippy is angry' >&2; exit 1"),
                (
                    CheckKind::Test,
                    &format!("touch {}", marker.to_string_lossy()),
                ),
            ],
        );
        let err = tool()
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect_err("a red gate must not be Ok");
        let msg = err.to_string();
        assert!(msg.starts_with("FAILED [2/3]"), "{msg}");
        assert!(
            msg.contains("clippy is angry"),
            "the failure's own output: {msg}"
        );
        assert!(msg.contains("Passed before it: `echo formatted`"), "{msg}");
        assert!(
            !marker.exists(),
            "the test check must not have run after the lint check failed",
        );
    }

    /// The failing command's output is the diagnosis. An error that dropped it
    /// would cost a second full run to learn what the first one already knew.
    #[tokio::test]
    async fn a_gate_that_fails_first_says_nothing_ran_before_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_with_gate(dir.path(), &[(CheckKind::Test, "exit 3")]);
        let msg = tool()
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect_err("non-zero is a failure")
            .to_string();
        assert!(msg.contains("Nothing ran before it."), "{msg}");
        assert!(msg.contains("exit status: 3"), "{msg}");
    }

    /// No gate is not a pass. A `verify` that returned `Ok` for a project it
    /// could not read would be a green light exactly where hrdr knows least.
    #[tokio::test]
    async fn an_unknown_gate_is_an_error_not_a_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path());
        let msg = tool()
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect_err("no gate is not a pass")
            .to_string();
        assert!(msg.contains("no verification gate is known"), "{msg}");
    }

    /// The ledger and the tool must agree: running the gate through `verify`
    /// settles what a `git commit` would otherwise be nagged about.
    #[tokio::test]
    async fn a_passing_run_settles_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_with_gate(dir.path(), &[(CheckKind::Test, "echo tested")]);
        ctx.verification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bump_source();
        assert!(
            ctx.verification
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .commit_note()
                .is_some(),
            "precondition: an edit landed and nothing has been run",
        );
        tool()
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("green");
        assert_eq!(
            ctx.verification
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .commit_note(),
            None,
            "the gate ran and passed, so a commit owes nothing",
        );
    }
}
