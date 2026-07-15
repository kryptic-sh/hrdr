//! `watch` — wait for something outside hrdr to reach an end state.
//!
//! The shape of the problem: "tell me when CI is done". The agent cannot answer
//! that in one shell call, because the answer is not there yet — and both things it
//! does instead are bad. It polls by hand, paying a model round-trip per check
//! (`gh run view` … think … `sleep 30` … think … `gh run view` …), or it blocks a
//! shell call on `sleep 600` and learns nothing until the end.
//!
//! So: hand it a command that answers a yes/no question *with its exit code*, and
//! run that command on a loop here, where a loop is free. Exit 0 means the thing it
//! was waiting for has happened; any other code means not yet, so sleep and ask
//! again. The tool returns the output of the check that finally said yes.
//!
//! The end state is whatever the command says it is. `gh run view <id> --json status
//! -q .status | grep -q completed` is satisfied whether CI passed *or* failed —
//! which is what "watch CI" actually means. Which of the two it was is in the output
//! the agent gets back.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

use super::{
    DEFAULT_WATCH_INTERVAL_SECS, DEFAULT_WATCH_TIMEOUT_SECS, MAX_WATCH_TIMEOUT_SECS,
    WATCH_CHECK_TIMEOUT_SECS,
};

pub struct WatchTool;

#[derive(Deserialize)]
struct WatchArgs {
    command: String,
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for WatchTool {
    fn name(&self) -> &'static str {
        "watch"
    }

    fn description(&self) -> &'static str {
        "Wait for something outside hrdr to reach an end state, then report it. \
         Runs `command` on a loop until it exits 0 — exit 0 means \"what I am waiting for has \
         happened\", any other exit code means \"not yet, ask again\" — and returns that final \
         check's output. Use it instead of polling by hand (a model round-trip per check) or \
         sleeping blindly. \
         The command must *test* a condition and exit, not block: \
         `gh run view <id> --json status -q .status | grep -q completed` (satisfied whether CI \
         passed or failed — read the output to see which), `test -f build/done`, \
         `curl -sf localhost:8080/health`. \
         Checks every `interval_secs` (default 10). Gives up after `timeout_secs` (default 1800, \
         max 21600) and reports the last check's output."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The check: exits 0 when the wait is over, non-zero to keep waiting. \
                                    It runs once per interval, so it must return promptly rather than block."
                },
                "interval_secs": {
                    "type": "integer",
                    "description": "Seconds to sleep between checks (default 10). Raise it for something \
                                    slow or rate-limited (a CI API); lower it for something local."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Give up after this long (default 1800 = 30 minutes, max 21600 = 6 hours). \
                                    Longer than the shell tool's timeout because waiting is the point — but \
                                    bounded, so a condition that never holds ends the call instead of the turn."
                }
            },
            "required": ["command"]
        })
    }

    /// The tool changes nothing itself, but the command it is handed could — so it
    /// goes through the same guardrails as `bash`, and a read-only sub-agent does
    /// not get it.
    fn read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: WatchArgs = crate::tool_args("watch", args)?;
        if let Some(msg) = crate::check_guardrails(&a.command, &ctx.guardrails) {
            bail!("command blocked: {msg}");
        }
        // A zero interval is a busy-loop against someone else's API and a zero
        // timeout is a watch that never watches. Both are far likelier to be a
        // mistake than an intention, so they read as "use the default".
        let interval = Duration::from_secs(
            a.interval_secs
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_WATCH_INTERVAL_SECS),
        );
        let budget = Duration::from_secs(
            a.timeout_secs
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS)
                .min(MAX_WATCH_TIMEOUT_SECS),
        );

        let started = Instant::now();
        let mut checks = 0usize;

        loop {
            checks += 1;
            let (code, output) = run_check(&a.command, ctx).await?;

            if code == Some(0) {
                let waited = started.elapsed().as_secs();
                let body = format!(
                    "[condition met after {checks} check(s), {waited}s]\n\n{}",
                    output.trim_end()
                );
                return Ok(crate::truncate(&body, ctx.max_output));
            }

            let code = code.map_or_else(|| "killed".to_string(), |c| c.to_string());
            let elapsed = started.elapsed();
            // Sleeping again would run past the budget, so this check was the last
            // one. Say what it was still seeing: a bare "timed out" sends the agent
            // back to run the same command by hand to find out.
            if elapsed + interval >= budget {
                let waited = elapsed.as_secs();
                let tail = crate::truncate(output.trim_end(), ctx.max_output);
                bail!(
                    "watch gave up after {waited}s and {checks} check(s) — the condition never held \
                     (last exit code {code}). Last output:\n{tail}"
                );
            }

            // A watch is the one tool that is *supposed* to take minutes. A silent
            // one looks hung, so say what it just saw and when it will look again.
            ctx.emit(format!(
                "watch: check {checks} — not yet (exit {code}); next in {}s\n",
                interval.as_secs()
            ));
            tokio::time::sleep(interval).await;
        }
    }
}

/// Run the check once. `Ok((code, output))`, where `code` is `None` when the check
/// had to be killed for taking too long — which counts as "not yet", not as an
/// error: a check that hangs (a network call with no timeout of its own) must not
/// wedge the watch.
async fn run_check(command: &str, ctx: &ToolContext) -> Result<(Option<i32>, String)> {
    let Some((program, args)) = super::shell::user_shell() else {
        bail!("no shell available to run the watch check");
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .arg(command)
        .current_dir(&ctx.cwd)
        // A model-supplied check must never block reading the TUI's terminal
        // (e.g. a `sudo` password prompt) — nothing feeds it stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The turn was cancelled (Esc), or the watch gave up: the check must not
        // outlive the call that started it.
        .kill_on_drop(true);
    // Own process group / job object, so a check that backgrounds something
    // (`some-daemon &`) can be killed in full on timeout, not just the shell
    // running the check itself.
    crate::proc::configure(&mut cmd);

    let check_timeout = Duration::from_secs(WATCH_CHECK_TIMEOUT_SECS);
    let child = cmd.spawn()?;
    let pid = child.id();
    let group = crate::proc::ProcessGroup::attach(&child)?;
    match tokio::time::timeout(check_timeout, child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                text.push_str(&err);
            }
            Ok((out.status.code(), text))
        }
        // The leader is killed as the timed-out future (and the `Child` it
        // owns) is dropped (`kill_on_drop`) — but that alone only reaps the
        // leader. Kill the whole group explicitly so anything the check
        // backgrounded dies with it.
        Err(_) => {
            group.kill(pid);
            Ok((
                None,
                format!("[check exceeded {WATCH_CHECK_TIMEOUT_SECS}s and was killed]"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    /// A condition that already holds returns immediately, with the check's output.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_condition_already_met_returns_at_once() {
        let out = WatchTool
            .execute(json!({"command": "echo ci-green; true"}), &ctx())
            .await
            .unwrap();
        assert!(
            out.contains("ci-green"),
            "the check's output comes back: {out}"
        );
        assert!(out.contains("1 check(s)"), "and it only ran once: {out}");
    }

    /// The point of the tool: keep asking until the answer changes.
    ///
    /// The check fails twice and then succeeds — driven by a counter file, so the
    /// command really is being re-run rather than its result cached.
    #[cfg(unix)]
    #[tokio::test]
    async fn it_keeps_checking_until_the_condition_holds() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("n");
        // Append a mark each run; succeed once there are three of them.
        let command = format!(
            "printf x >> {c}; test $(wc -c < {c}) -ge 3 && echo done",
            c = counter.display()
        );

        let started = Instant::now();
        let out = WatchTool
            .execute(json!({"command": command, "interval_secs": 1}), &ctx())
            .await
            .unwrap();

        assert!(out.contains("done"), "{out}");
        assert!(out.contains("3 check(s)"), "it checked three times: {out}");
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "and it actually slept between them"
        );
    }

    /// A condition that never holds ends the *call*, not the turn — and says what it
    /// was still seeing, so the agent doesn't have to run the same command by hand
    /// to find out.
    #[cfg(unix)]
    #[tokio::test]
    async fn giving_up_reports_the_last_check() {
        let err = WatchTool
            .execute(
                json!({
                    "command": "echo still-queued; false",
                    "interval_secs": 1,
                    "timeout_secs": 2
                }),
                &ctx(),
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("gave up"), "{err}");
        assert!(err.contains("last exit code 1"), "{err}");
        assert!(
            err.contains("still-queued"),
            "the last output must come back with the timeout: {err}"
        );
    }

    /// The command goes through the same guardrails as `bash`. `watch` runs a shell
    /// command on a loop; it would be a fine way to run a blocked one.
    #[tokio::test]
    async fn the_command_is_guarded_like_any_other_shell_command() {
        let err = WatchTool
            .execute(json!({"command": "git push --force origin main"}), &ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked"), "{err}");
    }

    /// Zero is a mistake, not an instruction: a zero interval busy-loops against
    /// someone else's API, and a zero timeout is a watch that never watches. Both
    /// fall back to the defaults rather than being obeyed.
    #[cfg(unix)]
    #[tokio::test]
    async fn zero_interval_falls_back_to_the_default() {
        // With the default 10s interval and a 1s budget, one check runs and the
        // watch gives up rather than spinning.
        let started = Instant::now();
        let err = WatchTool
            .execute(
                json!({"command": "false", "interval_secs": 0, "timeout_secs": 1}),
                &ctx(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("gave up"), "{err}");
        assert!(err.contains("1 check(s)"), "no busy-loop: {err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
