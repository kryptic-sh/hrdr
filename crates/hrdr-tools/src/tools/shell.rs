use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::{Tool, ToolContext};

use super::{BASH_LINE_CAP, DEFAULT_SHELL_TIMEOUT_MS};

// ---- shell ----

/// The shell interpreter this session runs commands through, resolved once from
/// `PATH`: `bash`, then POSIX `sh`. hrdr targets UNIX workflows, so there is no
/// PowerShell path; on Windows this means WSL or Git Bash. The model is told
/// which one it has (see [`ShellTool::description`]) so it can avoid bashisms
/// when only `sh` is present.
#[derive(Clone, Copy)]
enum ShellKind {
    Bash,
    Posix,
}

impl ShellKind {
    /// The interpreter program to invoke as `<program> -c <command>`.
    fn program(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Posix => "sh",
        }
    }
}

/// The single, platform-agnostic `shell` tool. It runs whatever shell was
/// auto-detected (`bash` or POSIX `sh`); its name is always `shell`, and its
/// description names the actual interpreter in use.
pub struct ShellTool {
    kind: ShellKind,
}

impl ShellTool {
    /// A `shell` tool backed by `bash`.
    pub fn bash() -> Self {
        Self {
            kind: ShellKind::Bash,
        }
    }
    /// A `shell` tool backed by POSIX `sh`.
    pub fn posix() -> Self {
        Self {
            kind: ShellKind::Posix,
        }
    }
}

const BASH_DESC: &str = "Run a shell command via `bash -c` in the working directory. Use for build, test, \
     git, and anything without a dedicated tool. Output is captured and length-bounded. \
     Each call starts fresh in the working directory (you are already there — no need \
     to `cd` to it). If you need to change dir, chain it (`cd sub && …`) or use paths \
     from the cwd; `cd` does NOT persist between calls. \
     Git: stage explicit paths (`git add <file> …`); blanket staging, force-push, \
     hook-skipping, and destructive commands are rejected.";

const SH_DESC: &str = "Run a shell command via `sh -c` — this session's shell is POSIX `sh`, NOT bash, so \
     avoid bash-only syntax (`[[ … ]]`, arrays, `source`, `set -o pipefail`, `<(…)`). \
     Use for build, test, git, and anything without a dedicated tool. Output is captured \
     and length-bounded. Each call starts fresh in the working directory (you are already \
     there — no need to `cd` to it). If you need to change dir, chain it (`cd sub && …`) \
     or use paths from the cwd; `cd` does NOT persist between calls. Git: stage explicit \
     paths (`git add <file> …`); blanket staging, force-push, hook-skipping, and \
     destructive commands are rejected.";

/// Arguments for the `shell` tool.
#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// The JSON-Schema for the `shell` tool; only the command description differs by
/// shell.
fn shell_parameters(command_desc: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": command_desc},
            "timeout_ms": {
                "type": "integer",
                "description": "How long to let the command run, in milliseconds. \
                                Default 300000 (5 minutes). Raise it for something you \
                                expect to be slow — a cold build, a full test suite, a \
                                dependency install — rather than letting it be killed \
                                and starting over."
            }
        },
        "required": ["command"]
    })
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn description(&self) -> &'static str {
        match self.kind {
            ShellKind::Bash => BASH_DESC,
            ShellKind::Posix => SH_DESC,
        }
    }
    fn shell_program(&self) -> Option<&'static str> {
        Some(self.kind.program())
    }
    fn parameters(&self) -> serde_json::Value {
        shell_parameters("Shell command to run.")
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ShellArgs = crate::tool_args("shell", args)?;
        if let Some(msg) = crate::check_guardrails(&a.command, &ctx.guardrails) {
            bail!("command blocked: {msg}");
        }
        let mut cmd = tokio::process::Command::new(self.kind.program());
        cmd.arg("-c").arg(&a.command).current_dir(&ctx.cwd);
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS));
        run_streamed_command(cmd, timeout, ctx).await
    }
}

/// Read one line (through `\n`) from `reader` into `buf`, but never buffer more
/// than `cap` bytes of it: once `buf` holds `cap` bytes the rest of an
/// over-long line is consumed and discarded up to its newline. This is the
/// memory bound `read_until` lacks — `read_until` would grow `buf` without
/// limit on a newline-less multi-gigabyte run (`tr '\0' a </dev/zero`, a huge
/// minified blob) and OOM the process before the [`BASH_LINE_CAP`] display cap
/// ever ran.
///
/// Returns `buf.len()` after the read: `0` means EOF with nothing buffered
/// (caller stops); any non-zero value means a line (possibly capped, possibly
/// the final newline-less tail at EOF) is ready to ingest. The trailing `\n` is
/// included when present. `overflowing` carries the "already past cap for this
/// line" state across calls so the loop stays cancel-safe (each `fill_buf`
/// await is the only suspension point, and it consumes nothing until it
/// returns), exactly as the persistent `buf` did for `read_until`.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    overflowing: &mut bool,
    cap: usize,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF: hand back whatever partial line remains
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if !*overflowing {
            let remaining = cap.saturating_sub(buf.len());
            if take <= remaining {
                buf.extend_from_slice(&available[..take]);
            } else {
                buf.extend_from_slice(&available[..remaining]);
                *overflowing = true; // drop the rest of this over-long line
            }
        }
        let ended = available[take - 1] == b'\n';
        reader.consume(take);
        if ended {
            *overflowing = false;
            return Ok(buf.len());
        }
    }
}

/// Spawn a configured command, streaming its stdout/stderr line-by-line to the
/// UI sink while accumulating a length-bounded view of the output. Full output
/// is written incrementally to an overflow file so the model can read/grep it
/// even when the in-memory view is truncated. Used by the `shell` tool.
async fn run_streamed_command(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    ctx: &ToolContext,
) -> Result<String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // A model-supplied command must never read the TUI's terminal: something
    // like `sudo` prompting for a password would block on the user's keystrokes
    // for the whole timeout. Nothing here feeds the child stdin, so null it.
    cmd.stdin(Stdio::null());
    // Cancelled future → child must not linger.
    cmd.kill_on_drop(true);
    // Own process group / job object, so the timeout path below can kill the
    // whole tree the command forked, not just its own pid — `kill_on_drop`
    // and a bare `child.kill()` only ever reach the leader.
    crate::proc::configure(&mut cmd);
    let mut child = cmd.spawn().context("spawning command")?;
    let pid = child.id();
    let group = crate::proc::ProcessGroup::attach(&child).context("attaching process group")?;
    let stdout = child.stdout.take().context("capturing stdout")?;
    let stderr = child.stderr.take().context("capturing stderr")?;
    let mut out_reader = BufReader::new(stdout);
    let mut err_reader = BufReader::new(stderr);

    // In-memory budget: ~1/5 head + ~4/5 tail ring (both measured in bytes).
    // 5× max_output keeps enough context for head+tail display while staying
    // orders of magnitude below a typical huge file.
    let mem_budget = ctx.max_output.saturating_mul(5).max(ctx.max_output);
    let head_budget = mem_budget / 5;
    let tail_budget = mem_budget - head_budget;

    let mut head = String::new();
    // Tail ring: each entry is one line (with its newline). Evict from front
    // when tail_bytes would exceed the budget.
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut tail_bytes: usize = 0;
    let mut total_bytes: usize = 0;
    let mut total_lines: usize = 0;

    // Overflow file: created only once output actually exceeds the display
    // caps — most commands never touch it. Until that point every ingested
    // line lives verbatim in `head` (it only starts spilling into the `tail`
    // ring once `head` reaches `head_budget`, which is sized to exactly
    // `ctx.max_output` — the same threshold that trips the byte cap below), so
    // the moment we first cross a cap, `head` already holds the complete
    // output so far and can be dumped in one write with nothing missing.
    // Every line after that point is appended as it arrives, same as before.
    let overflow_dir = crate::tool_output_dir();
    let mut overflow_path: Option<std::path::PathBuf> = None;
    let mut overflow_file: Option<std::fs::File> = None;

    macro_rules! ingest_line {
        ($line:expr) => {{
            let line: &str = $line;
            // Stream to the UI (unchanged from current behaviour).
            ctx.emit(format!("{line}\n"));

            total_lines += 1;
            total_bytes += line.len() + 1; // +1 for the newline

            // Accumulate in-memory head + tail.
            if head.len() < head_budget {
                head.push_str(line);
                head.push('\n');
            } else {
                let entry = format!("{line}\n");
                tail_bytes += entry.len();
                tail.push_back(entry);
                // Evict oldest tail entries to stay within the tail budget.
                while tail_bytes > tail_budget {
                    if let Some(front) = tail.pop_front() {
                        tail_bytes -= front.len();
                    } else {
                        break;
                    }
                }
            }

            let over_cap = total_bytes > ctx.max_output || total_lines > ctx.max_output_lines;
            if overflow_file.is_none() {
                if over_cap {
                    // First time over a cap: open the file and seed it with
                    // everything ingested so far (verbatim in `head`) in one
                    // write, rather than having written every line from the
                    // start regardless of whether it would ever be needed.
                    let _ = std::fs::create_dir_all(&overflow_dir);
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    static COUNTER: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let p = overflow_dir.join(format!("shell-{stamp}-{seq}.txt"));
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .open(&p)
                    {
                        use std::io::Write as _;
                        let _ = f.write_all(head.as_bytes());
                        overflow_path = Some(p);
                        overflow_file = Some(f);
                    }
                }
            } else if let Some(f) = &mut overflow_file {
                // Already over the cap and the file is open: keep it in sync
                // one line at a time (it was already seeded with everything
                // up to the line that tripped `over_cap` above).
                use std::io::Write as _;
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }};
    }

    // Read stdout + stderr concurrently; read_line_capped bounds each line at
    // BASH_LINE_CAP as it reads, so a single newline-less run (minified source,
    // `tr '\0' a </dev/zero`) is cut instead of buffered whole and OOMing.
    let collect = async {
        let mut out_done = false;
        let mut err_done = false;
        let mut out_buf = Vec::<u8>::new();
        let mut err_buf = Vec::<u8>::new();
        let mut out_over = false;
        let mut err_over = false;
        loop {
            tokio::select! {
                n = read_line_capped(&mut out_reader, &mut out_buf, &mut out_over, BASH_LINE_CAP), if !out_done => {
                    match n? {
                        0 => out_done = true,
                        _ => {
                            // The buffer is already capped at BASH_LINE_CAP; strip
                            // any trailing newline / carriage-return.
                            if out_buf.last() == Some(&b'\n') { out_buf.pop(); }
                            if out_buf.last() == Some(&b'\r') { out_buf.pop(); }
                            let capped_len = out_buf.len().min(BASH_LINE_CAP);
                            let line = String::from_utf8_lossy(&out_buf[..capped_len]).into_owned();
                            ingest_line!(&line);
                            out_buf.clear();
                            out_over = false;
                        }
                    }
                }
                n = read_line_capped(&mut err_reader, &mut err_buf, &mut err_over, BASH_LINE_CAP), if !err_done => {
                    match n? {
                        0 => err_done = true,
                        _ => {
                            if err_buf.last() == Some(&b'\n') { err_buf.pop(); }
                            if err_buf.last() == Some(&b'\r') { err_buf.pop(); }
                            let capped_len = err_buf.len().min(BASH_LINE_CAP);
                            let line = String::from_utf8_lossy(&err_buf[..capped_len]).into_owned();
                            ingest_line!(&line);
                            err_buf.clear();
                            err_over = false;
                        }
                    }
                }
                else => break,
            }
        }
        let status = child.wait().await.context("waiting on command")?;
        anyhow::Ok(status)
    };

    let timed = tokio::time::timeout(timeout, collect).await;
    let status = match timed {
        Ok(inner) => Some(inner?),
        Err(_) => {
            // Kill the whole process tree, not just `child`: `bash -c "npm
            // run dev"` forks `node`, and `child.kill()` alone only reaps
            // `bash` — `node` would keep holding its port forever.
            group.kill(pid);
            let _ = child.kill().await;
            let msg = format!(
                "[command timed out after {}ms; process killed — raise timeout_ms or \
                 run a narrower command]",
                timeout.as_millis()
            );
            ingest_line!(&msg);
            None
        }
    };
    if let Some(s) = status
        && !s.success()
    {
        let msg = format!("[exit status: {s}]");
        ingest_line!(&msg);
    }

    // Flush the overflow file (drop closes it).
    drop(overflow_file);

    // Nothing produced.
    if total_lines == 0 {
        return Ok("(no output)".to_string());
    }

    // Within both display caps: return the full in-memory view (no pointer needed).
    if total_bytes <= ctx.max_output && total_lines <= ctx.max_output_lines {
        // head holds all lines in this branch.
        let out = head.trim_end();
        return Ok(out.to_string());
    }

    // Over the display cap: emit head + overflow pointer + tail, via the shared
    // `overflow_preview` so `shell`, `grep`, and `git` produce one marker format.
    //
    // `head`/`tail` above are only bounded by the roomy 5x in-memory ring
    // (`mem_budget`) — that headroom exists so nothing is dropped before we know
    // whether the run will overflow, not so the *returned* text can be that big.
    // Without a final trim here, one call could hand back ~1x (head) + ~4x (tail)
    // = ~5x max_output, silently blowing the budget every other tool enforces.
    // Re-trim head and tail to their share of the real display budget (same ~1/5
    // head, ~4/5 tail split `truncate_saved`'s `Middle` side uses).
    let tail_str: String = tail.iter().map(|s| s.as_str()).collect();
    let disp_head_bytes = ctx.max_output / 5;
    let disp_tail_bytes = ctx.max_output.saturating_sub(disp_head_bytes);
    let disp_head_lines = (ctx.max_output_lines / 5).max(1);
    let disp_tail_lines = ctx.max_output_lines.saturating_sub(disp_head_lines);
    let head_disp = cap_display(head.trim_end(), disp_head_bytes, disp_head_lines, false);
    let tail_disp = cap_display(
        tail_str.trim_start(),
        disp_tail_bytes,
        disp_tail_lines,
        true,
    );

    Ok(crate::overflow_preview(
        &head_disp,
        &tail_disp,
        overflow_path.as_deref(),
        total_lines,
        total_bytes,
    ))
}

/// Trim already-bounded display text down to `max_bytes` and `max_lines`,
/// keeping whole lines from the front (`from_tail = false`, for `head`) or the
/// back (`from_tail = true`, for `tail`) — the same head/tail line-collection
/// `truncate_saved`'s `Middle` side does. `head`/`tail` are already in-memory
/// strings rather than something worth round-tripping through
/// `save_overflow` again, so this just splits on `'\n'` and defers to
/// `lib.rs`'s shared `collect_lines`, which byte-caps a single line wider
/// than `max_bytes` rather than dropping it, so the preview is never empty
/// when there's anything to show.
fn cap_display(text: &str, max_bytes: usize, max_lines: usize, from_tail: bool) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    crate::collect_lines(&lines, max_lines, max_bytes, from_tail)
}

/// The shell tool for this machine as a 0-or-1 `Vec` (so the registry can
/// register it in the same presence-gated loop as its other tools). `bash`
/// first, then POSIX `sh`; empty when neither is on `PATH`.
pub fn available_shell_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    detect_shell()
        .map(|tool| vec![std::sync::Arc::new(tool) as std::sync::Arc<dyn Tool>])
        .unwrap_or_default()
}

/// Resolve the session's shell from `PATH`: `bash`, then POSIX `sh`. `None`
/// when neither exists (on Windows, install WSL or Git Bash).
fn detect_shell() -> Option<ShellTool> {
    if which::which("bash").is_ok() {
        Some(ShellTool::bash())
    } else if which::which("sh").is_ok() {
        Some(ShellTool::posix())
    } else {
        None
    }
}

/// The interpreter for a *user-typed* `!command` (the TUI's shell escape):
/// `(program, leading args)` — the same shell the `shell` tool resolves to
/// (`bash -c`, then `sh -c`). `None` when neither is on `PATH`. The command
/// string is appended as the final argument.
pub fn user_shell() -> Option<(String, Vec<String>)> {
    detect_shell().map(|s| (s.kind.program().to_string(), vec!["-c".to_string()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newline-less run far larger than the cap is bounded *as it is read* —
    /// `buf` never grows past `cap` — and the over-long line is drained through
    /// its newline so the next line comes back intact. This is the memory bound
    /// `read_until` lacked: it would have buffered the whole 1 MiB run first.
    #[tokio::test]
    async fn read_line_capped_bounds_a_newlineless_run_and_resumes() {
        // 1 MiB of 'a' with no newline, then a newline, then a short line.
        let mut data = vec![b'a'; 1 << 20];
        data.push(b'\n');
        data.extend_from_slice(b"second\n");
        let mut reader = BufReader::new(&data[..]);

        let mut buf = Vec::new();
        let mut over = false;
        let n = read_line_capped(&mut reader, &mut buf, &mut over, 64)
            .await
            .unwrap();
        assert_eq!(n, 64, "the over-long line is handed back capped");
        assert!(
            buf.len() <= 64,
            "buffer never exceeds the cap: {}",
            buf.len()
        );

        // The rest of that line was discarded up to its newline, so the next
        // read yields the following line whole (not a tail of the 1 MiB run).
        buf.clear();
        over = false;
        let n = read_line_capped(&mut reader, &mut buf, &mut over, 64)
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"second\n");

        // EOF returns 0 with nothing buffered.
        buf.clear();
        over = false;
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, &mut over, 64)
                .await
                .unwrap(),
            0
        );
    }

    /// A shell command gets five minutes unless the model says otherwise — and the
    /// schema *says so*, for both shell backends.
    ///
    /// The default has to cover the commands actually worth running: a cold
    /// `cargo build`, a full test suite, an `npm install` on a fresh tree. At two
    /// minutes those died just often enough to matter, and a killed build teaches
    /// the model nothing — it retries something narrower, and the work is redone
    /// rather than finished. A genuine hang is still caught; it just gets a
    /// realistic amount of rope first.
    ///
    /// `timeout_ms` is only useful if the model can *see* what it overrides: a
    /// default it doesn't know about is a default it won't reason about. So the
    /// number, its unit, and when to raise it all live in the description the model
    /// is handed with every request.
    #[test]
    fn a_shell_command_gets_five_minutes_by_default_and_says_so() {
        assert_eq!(
            DEFAULT_SHELL_TIMEOUT_MS, 300_000,
            "five minutes: long enough for a cold build, short enough to catch a hang"
        );

        // Both backends, through the schema each actually advertises.
        let schemas = [
            ShellTool::bash().parameters(),
            ShellTool::posix().parameters(),
        ];
        for schema in schemas {
            let desc = schema["properties"]["timeout_ms"]["description"]
                .as_str()
                .expect("timeout_ms is documented");
            assert!(
                desc.contains("300000"),
                "the model must see the default it is overriding: {desc}"
            );
            assert!(
                desc.contains("5 minutes"),
                "and in units a reader parses at a glance: {desc}"
            );
            assert!(
                desc.contains("cold build"),
                "and when raising it beats being killed: {desc}"
            );
        }
    }

    /// An unset `timeout_ms` means the default, not "no timeout" — and a set one is
    /// honoured. The parse is the only thing standing between a hung command and a
    /// wedged turn.
    #[test]
    fn timeout_ms_defaults_when_absent_and_is_honoured_when_given() {
        let default: ShellArgs = serde_json::from_value(serde_json::json!({"command": "true"}))
            .expect("command alone is valid");
        assert_eq!(default.timeout_ms, None, "absent means absent");
        assert_eq!(
            Duration::from_millis(default.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)),
            Duration::from_secs(300),
            "…and absent resolves to five minutes"
        );

        let given: ShellArgs =
            serde_json::from_value(serde_json::json!({"command": "true", "timeout_ms": 900_000}))
                .expect("an override is valid");
        assert_eq!(
            Duration::from_millis(given.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)),
            Duration::from_secs(900),
            "a model that asks for fifteen minutes gets fifteen minutes"
        );
    }

    /// The point of the whole `proc` module: a timeout must kill the entire
    /// process tree, not just the `bash` leader. `bash -c "npm run dev"`
    /// forking `node` is the motivating case — this stands a `sleep`
    /// (backgrounded, so it outlives `bash`'s own foreground sleep) in for
    /// `node` and checks it's actually dead, not just `bash`.
    ///
    /// Without the process-group kill, `child.kill()` alone reaps only
    /// `bash`; the backgrounded `sleep` — same process group, same session,
    /// no controlling terminal to notice `bash` is gone — keeps running for
    /// its full 5s, and the marker file would appear right on schedule.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree_not_just_the_leader() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-finished");
        let pid_file = dir.path().join("grandchild.pid");

        // Background a subshell that sleeps 5s and then touches `marker`
        // (standing in for a long-lived `node` server); record its pid; then
        // block in the foreground on a sleep of our own so `bash` is still
        // alive when the 300ms timeout below fires.
        let command = format!(
            "(sleep 5 && touch {m}) & echo $! > {p}; sleep 5",
            m = marker.display(),
            p = pid_file.display(),
        );

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = ShellTool::bash()
            .execute(json!({"command": command, "timeout_ms": 300}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("timed out"), "{out}");

        // Give the group-kill a moment to land, then check the grandchild
        // (background `sleep`) directly via `kill(pid, 0)` — no signal sent,
        // just a liveness probe; ESRC means it's gone.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let grandchild_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("the background job recorded its pid before bash was killed")
            .trim()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(grandchild_pid, 0) == 0 };
        assert!(
            !alive,
            "grandchild pid {grandchild_pid} survived the timeout — only the \
             `bash` leader was killed, not its process group"
        );

        // And it never got far enough to touch the marker — proof the kill
        // landed well before the grandchild's own 5s sleep would have
        // finished on its own.
        assert!(
            !marker.exists(),
            "the grandchild's sleep completed — it was never actually killed"
        );
    }

    /// Regression (MAJOR): head + hint + tail must be re-trimmed to the
    /// `max_output`/`max_output_lines` budget before returning, not just kept
    /// under the roomy 5x in-memory ring. Before the fix, a 200-byte cap could
    /// come back with a ~200-byte head *and* a ~800-byte tail (the ring's
    /// untrimmed 1x/4x split) plus the hint — up to ~5x the cap, the same
    /// contract every other tool (`truncate_saved`) honours. This pins the
    /// returned size to a tight multiple of the cap, not merely "under 2000
    /// bytes for a 200-byte cap" (10x — loose enough to pass even the bug).
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_output_is_trimmed_to_the_display_budget_not_the_5x_ring() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ToolContext::new(dir.path());
        c.max_output = 200;
        c.max_output_lines = 10;

        // 50 lines of ~33 chars each (~1650 bytes total) — comfortably over
        // both caps, and small enough to stay under the 5x in-memory ring too
        // (so this exercises the final display trim, not the ring cap).
        let result = ShellTool::bash()
            .execute(
                serde_json::json!({"command": "for i in $(seq 1 50); do echo \"line $i: some padding text here\"; done"}),
                &c,
            )
            .await
            .unwrap();

        // A generous but real bound: head (<= max_output/5) + tail
        // (<= max_output - max_output/5) + a hint line that includes an
        // overflow file path. 3x the cap is nowhere near the ~5x (1000+ byte)
        // ring the bug could return, but has headroom for the hint/path text.
        assert!(
            result.len() <= c.max_output * 3,
            "output must be trimmed to the display budget, not the 5x ring: \
             {} bytes for a {}-byte cap:\n{result}",
            result.len(),
            c.max_output
        );
        assert!(
            result.contains("full output") || result.contains("truncated"),
            "truncation marker missing: {result}"
        );
        assert!(result.contains("line 1"), "head not preserved: {result}");
    }

    /// `cap_display` keeps whole lines within both a byte and a line budget,
    /// taking from the front for `head` and the back for `tail`, and never
    /// panics or produces something absurd when a single line alone exceeds
    /// the byte budget.
    #[test]
    fn cap_display_bounds_bytes_and_lines_from_either_end() {
        let text = "one\ntwo\nthree\nfour\nfive";
        let head = cap_display(text, 100, 2, false);
        assert_eq!(head, "one\ntwo");
        let tail = cap_display(text, 100, 2, true);
        assert_eq!(tail, "four\nfive");

        // A single line wider than the byte budget is capped, not dropped.
        let one_long_line = "a".repeat(50);
        let capped = cap_display(&one_long_line, 10, 5, false);
        assert_eq!(capped.len(), 10);
    }
}
