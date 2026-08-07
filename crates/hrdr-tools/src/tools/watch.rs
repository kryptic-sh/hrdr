//! `watch` — non-blocking background watch with result delivery.
//!
//! The release procedure requires watching the tag's CI run to completion and
//! confirming it published, but hrdr has no non-blocking primitive for it: a
//! blocking `sleep`-poll loop or `gh run watch` is the wrong shape (there is no
//! polling tool), and ending the turn stalls the flow until a human notices.
//! `watch` is the answer — the same spawn-return-deliver contract `task`
//! already has: call it, get an id immediately, end the turn, be woken when the
//! condition flips.
//!
//! It re-runs a shell command until it exits 0, then delivers the result into
//! the conversation exactly like a finished background sub-agent's report. The
//! CI case is one consumer (`gh run view <id> --json status -q .status |
//! grep -qx completed`); watching a deploy's health endpoint or a build
//! artifact appearing is the same primitive. The watcher reports "the run
//! finished", never "the release is published" — the confirmation (enumerate
//! the jobs, check artifacts) stays a model step on wake.

use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::tools::shell::{Shell, reject_timeout_ms, run_streamed_command};
use crate::{BackgroundKind, BackgroundTask, Tool, ToolContext};

/// How often the check re-runs, by default.
pub const DEFAULT_WATCH_INTERVAL_SECS: u64 = 30;
/// The floor on `interval_secs`: below this the check would hammer whatever it
/// is watching (a CI API, a health endpoint) and burn a subprocess per poll.
pub const MIN_WATCH_INTERVAL_SECS: u64 = 15;
/// How long a watch runs before it gives up, by default.
///
/// 30 minutes, not the 10 a first draft chose: the motivating use case is
/// watching a release tag's CI run, and the runs that prompted this tool take
/// 12–19 minutes — a 10-minute default would time the flagship flow out
/// mid-run and force a re-watch with an explicit ceiling.
pub const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 1800;
/// The ceiling on `timeout_secs`: any longer and the watch holds its registry
/// entry — and the poller's clone of the call's stream channel — past any
/// sensible turn.
pub const MAX_WATCH_TIMEOUT_SECS: u64 = 3600;
/// Deadline for **one** check run. A stuck `gh` must not hold a poll slot for
/// the standard 5-minute tool timeout; the check re-runs next interval anyway.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Ring-buffer cap on the entry `log` shown in the background panel. Unbounded,
/// 240 polls × 5 KB of output would be ~1.2 MB held in the registry and
/// rendered every frame; the final result carries the last output verbatim.
pub(crate) const WATCH_LOG_MAX_BYTES: usize = 16 * 1024;
/// Cap on the delivered result — hrdr-tools' own copy of the 24 KB
/// `BACKGROUND_REPORT_MAX_BYTES` hrdr-agent applies to sub-agent reports (that
/// constant is `pub(crate)` there and not importable from here).
pub(crate) const WATCH_REPORT_MAX_BYTES: usize = 24_000;

pub struct WatchTool {
    shell: Shell,
}

impl WatchTool {
    pub fn new(shell: Shell) -> Self {
        Self { shell }
    }
}

#[derive(Deserialize)]
struct WatchArgs {
    check: String,
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
        "Watch for an external end state: re-run a shell command in the background until it exits 0, \
         and wake me when it does. For a CI run finishing, a deploy's health endpoint coming up, a \
         build artifact appearing — things that complete on their own schedule. The call returns an \
         id immediately; END YOUR TURN, and the result is delivered to you like a finished background \
         task when the condition flips. The check runs under the same guardrails and sandbox as \
         `shell`; a check blocked by a guardrail stops the watch. Cancel a watch with `task_cancel \
         <id>`. Not a general repeat-a-command tool — use `shell` when you want a command's output."
    }

    /// The check command may mutate the tree (like any `shell` command); it is
    /// never an observation.
    fn read_only(&self) -> bool {
        false
    }

    /// Each watch is self-contained (its own registry entry, its own poller);
    /// two watches never need to observe each other in order.
    fn concurrent(&self) -> bool {
        true
    }

    /// The whole job is to ask the same check until the answer changes — the
    /// exact case `RepeatGuard` must not nudge on.
    fn repeatable(&self) -> bool {
        true
    }

    /// Self-managed, like `shell`: the watch's `timeout_secs` is the whole-watch
    /// ceiling the handler enforces, not a per-call deadline for the dispatcher
    /// to apply. Letting the dispatcher cut the call off would destroy the
    /// poller and throw away the partial result.
    fn timeout_secs(&self) -> Option<u64> {
        None
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "check": {
                    "type": "string",
                    "description": "A shell command; exit 0 = the condition is met. Runs under the \
                                    same guardrails and sandbox as `shell` (e.g. `gh run view 123 \
                                    --json status -q .status | grep -qx completed`)."
                },
                "interval_secs": {
                    "type": "integer",
                    "default": DEFAULT_WATCH_INTERVAL_SECS,
                    "minimum": MIN_WATCH_INTERVAL_SECS,
                    "description": format!(
                        "How often to re-run the check, in seconds (default \
                         {DEFAULT_WATCH_INTERVAL_SECS}, minimum {MIN_WATCH_INTERVAL_SECS})."
                    ),
                },
                "timeout_secs": {
                    "type": "integer",
                    "default": DEFAULT_WATCH_TIMEOUT_SECS,
                    "maximum": MAX_WATCH_TIMEOUT_SECS,
                    "description": format!(
                        "Whole-watch ceiling, in seconds (default {DEFAULT_WATCH_TIMEOUT_SECS}, \
                         maximum {MAX_WATCH_TIMEOUT_SECS}). The watch ends with a timeout result \
                         if the check never exits 0."
                    ),
                }
            },
            "required": ["check"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        reject_timeout_ms(&args)?;
        let a: WatchArgs = crate::tool_args("watch", args)?;
        let check = a.check.trim().to_string();
        if check.is_empty() {
            bail!(
                "watch needs a non-empty `check` — the shell command that exits 0 when the condition is met"
            );
        }
        let interval = a.interval_secs.unwrap_or(DEFAULT_WATCH_INTERVAL_SECS);
        // The floor is a test-speed valve, same shape as the shared timeout
        // floor (`floored_timeout_secs`): `enforce_timeout_floor` is false only
        // in tests, which need a sub-floor interval to exercise the flip/timeout
        // paths without a 30-second sleep per poll.
        if interval < MIN_WATCH_INTERVAL_SECS && ctx.enforce_timeout_floor {
            bail!(
                "interval_secs={interval} is below the {MIN_WATCH_INTERVAL_SECS}s floor — the \
                 check would hammer the thing it is watching"
            );
        }
        let timeout = a.timeout_secs.unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS);
        if timeout > MAX_WATCH_TIMEOUT_SECS {
            bail!(
                "timeout_secs={timeout} exceeds the {MAX_WATCH_TIMEOUT_SECS}s ceiling — the watch \
                 would hold its registry entry too long"
            );
        }
        // Mint the id from the shared counter so task and watch ids never
        // collide: `drain_background`/`task_cancel`/the TUI wake all match on
        // `id` alone.
        let id = BackgroundTask::next_id();
        // The label doubles as the delivery Notice's header, so newlines from a
        // multi-line check (or `truncate`'s "output truncated" marker) must not
        // land raw in it.
        let label = format!(
            "watch: {}",
            crate::truncate(&check, 60).replace(['\n', '\r'], " ")
        );
        // Push BEFORE spawning, the same order `spawn_background` uses, so the
        // id is addressable (`task_cancel N`, the panel) as soon as the call
        // returns — the poller races nothing.
        {
            let mut v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.push(BackgroundTask {
                id,
                kind: BackgroundKind::Watch,
                tool_id: ctx.call_id.clone(),
                label: label.clone(),
                log: String::new(),
                done: false,
                result: None,
                delivered: false,
                cancelled: false,
            });
        }
        // The poller's own context, with the stream nulled: `run_streamed_command`
        // emits each check's output line-by-line, and the poller's clone still
        // holds THIS call's stream channel — kept alive by the poller's tx for up
        // to `timeout_secs`. The progress the panel shows comes from the entry
        // `log`; nothing should stream into a call that already returned.
        let mut poller_ctx = ctx.clone();
        poller_ctx.stream = None;
        let shell = self.shell;
        let check_for_poller = check.clone();
        tokio::spawn(async move {
            poll_watch(id, shell, check_for_poller, interval, timeout, &poller_ctx).await;
        });
        let timeout_display = if timeout.is_multiple_of(60) {
            format!("{} min", timeout / 60)
        } else {
            format!("{timeout}s")
        };
        Ok(format!(
            "watching #{id} — polls `{check}` every {interval}s, up to {timeout_display}; I'll be \
             woken when it completes. End your turn. Cancel with `task_cancel {id}`."
        ))
    }
}

/// The poller: run `check` once immediately, then every `interval_secs`, until
/// it exits 0, the whole-watch `timeout_secs` ceiling passes, the entry is
/// cancelled, or the entry disappears (pruned after delivery, cleared by
/// `/clear`/agent teardown, process exit).
async fn poll_watch(
    id: u64,
    shell: Shell,
    check: String,
    interval: u64,
    timeout: u64,
    ctx: &ToolContext,
) {
    let started = std::time::Instant::now();
    // `None` until a round produces output; the timeout branch falls back to a
    // plain sentence rather than inventing output that never existed.
    let mut last_output: Option<String> = None;
    loop {
        // Self-termination: re-lock the registry and look up our own entry
        // before each poll. Gone or cancelled means stop — covers `task_cancel`,
        // delivery-then-prune, `/clear` and agent teardown with no handle
        // plumbing. (An in-flight check subprocess when cancel lands runs out
        // its CHECK_TIMEOUT; worst-case stray life is one interval plus one
        // check, which the `timeout_secs ≤ MAX_WATCH_TIMEOUT_SECS` bound caps
        // absolutely.)
        let cancelled = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match v.iter().find(|t| t.id == id) {
                Some(t) => t.cancelled,
                None => return,
            }
        };
        if cancelled {
            return;
        }
        // Whole-watch ceiling, checked before each round so a check never runs
        // past it. First iteration (elapsed ≈ 0) falls through unless the model
        // asked for a sub-second timeout, which times out immediately with the
        // "never ran" fallback.
        if started.elapsed() >= Duration::from_secs(timeout) {
            let output = last_output.as_deref().unwrap_or("(no check output yet)");
            let result = format!(
                "watch #{id} timed out after {timeout}s — the check never exited 0.\n\nLast check \
                 output:\n{output}"
            );
            finish(ctx, id, result);
            return;
        }
        // Guardrails first, confinement second — a blocked command never runs,
        // sandboxed or not, exactly as in `shell`. The check can never pass
        // while blocked, so the refusal ends the watch rather than polling a
        // command that is refused every round.
        if let Some(msg) = crate::check_guardrails(&check, &ctx.guardrails) {
            let result = format!(
                "watch #{id} stopped: the check was blocked by a command guardrail before it \
                 ran: {msg}"
            );
            finish(ctx, id, result);
            return;
        }
        let mut cmd = crate::sandbox::sandboxed_shell_command(
            shell,
            &check,
            &ctx.sandbox,
            &ctx.sandbox_notices,
        );
        cmd.current_dir(&ctx.cwd);
        // Staleness pairing, like `shell`: a mutating check names itself as the
        // culprit for the read-before-edit guard.
        let before = ctx.tracked_sigs();
        let run = run_streamed_command(cmd, &check, CHECK_TIMEOUT, false, ctx).await;
        ctx.note_modifying_command(&before, &check);
        match run {
            Ok(run) => {
                last_output = Some(run.output.clone());
                append_log(ctx, id, &run.output);
                if run.passed {
                    let elapsed = started.elapsed().as_secs();
                    let result = format!(
                        "watch #{id} finished — the check exited 0.\n\nLast check output:\n{}\n\n\
                         Watched for {elapsed}s.",
                        run.output.trim_end()
                    );
                    finish(ctx, id, result);
                    return;
                }
            }
            Err(e) => {
                // A failed ROUND, never a failed watch: a check killed at its
                // own deadline (the error text says so) or one that failed to
                // spawn keeps the watch polling — only the whole-watch timeout
                // ends it.
                let note = format!("(check failed this round: {e:#})");
                last_output = Some(note.clone());
                append_log(ctx, id, &note);
            }
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

/// Append a poll's output to the entry's `log`, ring-buffered to
/// [`WATCH_LOG_MAX_BYTES`] so a long watch cannot grow the registry without
/// bound.
fn append_log(ctx: &ToolContext, id: u64, chunk: &str) {
    let mut v = ctx
        .background_tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = v.iter_mut().find(|t| t.id == id) else {
        return;
    };
    // A single chunk can itself outgrow the whole cap (a raised `max_output`
    // makes `run_streamed_command`'s per-check output larger): keep the newest
    // cap bytes of the chunk and drop the old log — the cap is a hard bound.
    if chunk.len() > WATCH_LOG_MAX_BYTES {
        let mut start = chunk.len() - WATCH_LOG_MAX_BYTES;
        while !chunk.is_char_boundary(start) {
            start += 1;
        }
        entry.log = chunk[start..].to_string();
        return;
    }
    if entry.log.len().saturating_add(chunk.len()) > WATCH_LOG_MAX_BYTES {
        // Drop from the front, on a char boundary, keeping the newest output.
        let keep = WATCH_LOG_MAX_BYTES.saturating_sub(chunk.len());
        let mut start = entry.log.len().saturating_sub(keep);
        while start < entry.log.len() && !entry.log.is_char_boundary(start) {
            start += 1;
        }
        entry.log = entry.log[start..].to_string();
    }
    entry.log.push_str(chunk);
}

/// Mark the entry done with `result` (capped by hrdr-tools' own constant, since
/// hrdr-agent's `BACKGROUND_REPORT_MAX_BYTES` is not importable from here).
/// `drain_background` delivers it and prunes the entry; if the entry is already
/// gone (cancelled and pruned, or `/clear`), there is nothing to do.
fn finish(ctx: &ToolContext, id: u64, result: String) {
    let result = crate::truncate_middle(&result, WATCH_REPORT_MAX_BYTES);
    let mut v = ctx
        .background_tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = v.iter_mut().find(|t| t.id == id) {
        entry.done = true;
        entry.result = Some(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> WatchTool {
        WatchTool::new(Shell::detect().expect("a shell to watch with"))
    }

    /// The `#N` from the acknowledgement ("watching #N — …").
    fn ack_id(ack: &str) -> u64 {
        ack.split('#')
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no id in ack: {ack}"))
    }

    /// Wait for the watch's registry entry to flip to `done`, panicking if it
    /// vanishes first (a vanished entry means the poller gave up — a bug) or
    /// the deadline passes.
    async fn wait_done(ctx: &ToolContext, id: u64, max: Duration) -> BackgroundTask {
        let deadline = std::time::Instant::now() + max;
        loop {
            // Clone the entry inside a scoped block so the registry guard drops
            // before the sleep below.
            let entry = {
                let v = ctx
                    .background_tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                v.iter().find(|t| t.id == id).cloned()
            };
            match entry {
                Some(t) if t.done => return t,
                Some(_) => {}
                None => panic!("watch entry #{id} vanished before done"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watch #{id} never finished within {max:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A check that exits 0 only after `pass_after` runs, counting into a file
    /// in `dir` — the shape the "condition flips" tests need.
    fn flip_check(dir: &std::path::Path, pass_after: u64) -> String {
        let counter = dir.join("count");
        // Shell-safe form of the path: forward slashes (a `C:\…` spelling is
        // read cwd-relative by Git Bash, so the check counts in a file the
        // test cannot see) and shell-quoted (spaces/globs would break it
        // either way).
        let normalized = counter.to_string_lossy().replace('\\', "/");
        let counter = shell_words::quote(&normalized);
        format!(
            "c=$(cat {counter} 2>/dev/null || echo 0); c=$((c+1)); echo \"$c\"; echo \"$c\" > {counter}; \
             test \"$c\" -ge {pass_after}"
        )
    }

    fn counter_value(dir: &std::path::Path) -> u64 {
        std::fs::read_to_string(dir.join("count"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The call returns the id and an end-turn ack immediately — it must not
    /// block on the first check (a `sleep 2 && exit 0` check would delay a
    /// blocking poller by two seconds).
    #[tokio::test]
    async fn watch_returns_immediately_with_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let started = std::time::Instant::now();
        let ack = tool()
            .execute(serde_json::json!({"check": "sleep 2 && exit 0"}), &ctx)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the call blocked on the first check: {:?}",
            started.elapsed()
        );
        let id = ack_id(&ack);
        assert!(ack.contains(&format!("#{id}")), "{ack}");
        // The end-turn contract is in the ack: this is a spawn-return-deliver
        // tool, and the model is told to stop working and wait for the wake.
        assert!(ack.contains("End your turn"), "{ack}");
        assert!(ack.contains("task_cancel"), "{ack}");
        let v = ctx
            .background_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = v.iter().find(|t| t.id == id).expect("entry registered");
        assert!(!entry.done, "the entry must start not-done");
        assert!(!entry.cancelled);
        assert!(entry.result.is_none());
    }

    /// `check: "true"` — a condition already met — must finish on the FIRST
    /// poll, with a success result naming the exit.
    #[tokio::test]
    async fn watch_completes_instantly_when_the_check_passes_first() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let ack = tool()
            .execute(serde_json::json!({"check": "true"}), &ctx)
            .await
            .unwrap();
        let id = ack_id(&ack);
        let entry = wait_done(&ctx, id, Duration::from_secs(10)).await;
        let result = entry.result.expect("a done watch has a result");
        assert!(result.contains("exited 0"), "{result}");
        assert!(result.contains(&format!("#{id}")), "{result}");
        // A single round: `true` produces "(no output)", and the log holds it
        // exactly once — a re-poll would have doubled it.
        assert_eq!(
            entry.log, "(no output)",
            "the check ran once: {}",
            entry.log
        );
    }

    /// A check that fails twice then passes — the poller must keep polling
    /// until the condition flips and then land the result with the last
    /// output.
    #[tokio::test]
    async fn watch_delivers_after_the_condition_flips() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.enforce_timeout_floor = false; // sub-floor interval for a fast poll
        let check = flip_check(dir.path(), 3);
        let ack = tool()
            .execute(
                serde_json::json!({"check": check, "interval_secs": 1, "timeout_secs": 30}),
                &ctx,
            )
            .await
            .unwrap();
        let id = ack_id(&ack);
        let entry = wait_done(&ctx, id, Duration::from_secs(15)).await;
        let result = entry.result.expect("a done watch has a result");
        assert!(result.contains("exited 0"), "{result}");
        // The third poll's own output (the counter reaching 3) is in the result.
        assert!(result.contains('3'), "{result}");
        assert_eq!(
            counter_value(dir.path()),
            3,
            "the check ran exactly three rounds"
        );
    }

    /// A check that never passes must end with a timeout result naming the
    /// ceiling — and must NOT report success.
    #[tokio::test]
    async fn watch_times_out_with_a_failure_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.enforce_timeout_floor = false;
        let ack = tool()
            .execute(
                serde_json::json!({"check": "false", "interval_secs": 1, "timeout_secs": 1}),
                &ctx,
            )
            .await
            .unwrap();
        let id = ack_id(&ack);
        let entry = wait_done(&ctx, id, Duration::from_secs(15)).await;
        let result = entry.result.expect("a done watch has a result");
        assert!(result.contains("timed out after 1s"), "{result}");
        // Success reads "watch #N finished"; a timeout must not use that word.
        assert!(
            !result.contains("finished"),
            "a timeout must not read as success: {result}"
        );
    }

    /// `task_cancel` writes `cancelled = true` and `done = true` and no result;
    /// the poller must stop on that exact shape and never publish a result of
    /// its own (which would be delivered).
    #[tokio::test]
    async fn watch_stops_when_its_entry_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.enforce_timeout_floor = false;
        // Never passes on its own — only a buggy poller could "finish" it.
        let check = flip_check(dir.path(), 1_000_000);
        let ack = tool()
            .execute(
                serde_json::json!({"check": check, "interval_secs": 1, "timeout_secs": 60}),
                &ctx,
            )
            .await
            .unwrap();
        let id = ack_id(&ack);
        // Let two rounds run, then cancel with the exact two-flag write
        // `task_cancel` makes.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        {
            let mut v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = v.iter_mut().find(|t| t.id == id).unwrap();
            entry.cancelled = true;
            entry.done = true;
        }
        let before = counter_value(dir.path());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            counter_value(dir.path()),
            before,
            "the poller kept polling after cancel"
        );
        let v = ctx
            .background_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = v.iter().find(|t| t.id == id).unwrap();
        assert!(
            entry.result.is_none(),
            "a cancelled watch must not publish a result (it would be delivered)"
        );
    }

    /// A guardrailed check is refused, never run — the watch ends with the
    /// refusal, and the command's own failure mode is absent from the result.
    #[tokio::test]
    async fn watch_check_runs_under_guardrails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let ack = tool()
            .execute(serde_json::json!({"check": "git add -A"}), &ctx)
            .await
            .unwrap();
        let id = ack_id(&ack);
        let entry = wait_done(&ctx, id, Duration::from_secs(10)).await;
        let result = entry.result.expect("a done watch has a result");
        assert!(
            result.contains("blocked by a command guardrail"),
            "{result}"
        );
        // Proof it never ran: in a non-git dir, a real `git add -A` fails with
        // "not a git repository".
        assert!(
            !result.contains("not a git repository"),
            "the check ran: {result}"
        );
    }

    /// Schema bounds: a missing `check` and out-of-range numbers are refused
    /// with errors that say what is wrong.
    #[tokio::test]
    async fn watch_schema_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let t = tool();

        let err = t
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("check"), "{err}");

        let err = t
            .execute(
                serde_json::json!({"check": "true", "interval_secs": 5}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("15s floor"), "{err}");

        let err = t
            .execute(
                serde_json::json!({"check": "true", "timeout_secs": 4000}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("3600s ceiling"), "{err}");

        // The old millisecond spelling is poison, exactly as in `shell`: it
        // would silently run the default timeout while the model believes it
        // asked for 30 seconds.
        let err = t
            .execute(
                serde_json::json!({"check": "true", "timeout_ms": 30_000}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`timeout_ms` is gone"), "{err}");
        assert!(err.contains("pass `timeout_secs`"), "{err}");
    }

    /// The ring buffer is a HARD bound: a single chunk larger than the whole
    /// cap (a raised `max_output` makes `run_streamed_command`'s per-check
    /// output bigger) must not blow the log past `WATCH_LOG_MAX_BYTES`.
    #[test]
    fn append_log_is_a_hard_bound_even_for_an_oversized_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        {
            let mut v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.push(BackgroundTask {
                id: 7,
                kind: BackgroundKind::Watch,
                label: "watch: x".to_string(),
                ..Default::default()
            });
        }
        // Prefix bytes, then a multi-byte char straddling the cut point, then a
        // long tail — exercises the char-boundary walk as well as the cap.
        let big = format!(
            "head{}\u{2603}{}",
            "a".repeat(WATCH_LOG_MAX_BYTES),
            "b".repeat(WATCH_LOG_MAX_BYTES)
        );
        append_log(&ctx, 7, &big);
        let v = ctx
            .background_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = v.iter().find(|t| t.id == 7).unwrap();
        assert!(
            entry.log.len() <= WATCH_LOG_MAX_BYTES,
            "log {} > cap {}",
            entry.log.len(),
            WATCH_LOG_MAX_BYTES
        );
        assert!(
            entry.log.ends_with("bbbb"),
            "the newest bytes survive: {}",
            &entry.log[entry.log.len().saturating_sub(20)..]
        );
        assert!(!entry.log.contains("head"), "the old log was dropped");
    }

    /// The optional arguments record their defaults, so a transcript can say
    /// what a call actually ran with — enforced by the crate-wide
    /// `every_optional_argument_records_its_default`, but pinned here so a
    /// schema edit that drops a default fails in this module first.
    #[test]
    fn the_defaults_are_in_the_schema() {
        let schema = tool().parameters();
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(
            props["interval_secs"]["default"],
            json!(DEFAULT_WATCH_INTERVAL_SECS)
        );
        assert_eq!(
            props["timeout_secs"]["default"],
            json!(DEFAULT_WATCH_TIMEOUT_SECS)
        );
        assert_eq!(schema["required"], json!(["check"]));
    }
}
