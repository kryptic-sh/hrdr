//! User-configured shell hooks, in two families:
//!
//! * **File hooks** ([`Hook`], [`run_file_hooks`]) — run after `edit`/`write`
//!   mutate a matching file. Formatters, mostly (`cargo fmt`,
//!   `prettier --write`). Like the guardrails, a config rule the model can't
//!   forget — but hooks are operator-configured and intentionally bypass the
//!   command guardrails. The mutating tool re-reads the file *after* hooks
//!   run, so the diff the model sees (and the text its next `old_string`
//!   must match) is the post-hook content.
//! * **Lifecycle hooks** ([`EventHook`], [`run_event_hooks`]) — run on agent
//!   events (`pre_tool`, `post_tool`, `user_prompt`, `turn_end`,
//!   `session_start`, `session_end`). Each hook gets one JSON object on
//!   stdin describing the event. Exit 0 proceeds (stdout of a `user_prompt`
//!   hook is injected as context); **exit 2 blocks** the tool call / prompt
//!   with the hook's stderr as the reason; any other failure is a
//!   non-blocking warning.

use std::path::Path;
use std::time::Duration;

/// One configured hook: run `run` (with `{path}` substituted) after tool `on`
/// successfully mutates a file matching `glob`.
#[derive(Debug, Clone)]
pub struct Hook {
    /// Tool that triggers it: `edit` or `write` (`*` for both).
    pub on: String,
    /// File filter, matched against the file name and the cwd-relative path;
    /// `None` matches everything.
    pub glob: Option<glob::Pattern>,
    /// Shell command template; every `{path}` becomes the (quoted) file path.
    pub run: String,
    /// Kill the hook after this long (default [`DEFAULT_HOOK_TIMEOUT_MS`]).
    pub timeout_ms: u64,
}

/// Default per-hook timeout: formatters are fast; anything slower is stuck.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 30_000;

impl Hook {
    /// Whether this hook applies to `tool` mutating `path` (relative to `cwd`).
    fn matches(&self, tool: &str, path: &Path, cwd: &Path) -> bool {
        if self.on != "*" && self.on != tool {
            return false;
        }
        let Some(pat) = &self.glob else {
            return true;
        };
        let name_hit = path
            .file_name()
            .map(|n| pat.matches(&n.to_string_lossy()))
            .unwrap_or(false);
        let rel = path.strip_prefix(cwd).unwrap_or(path);
        name_hit || pat.matches_path(rel)
    }
}

/// Substitute `{path}` with the shell-quoted file path.
fn render_command(template: &str, path: &Path) -> String {
    let quoted = if cfg!(windows) {
        format!("\"{}\"", path.display().to_string().replace('"', "\"\""))
    } else {
        // POSIX single-quote escaping: ' -> '\''.
        format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
    };
    template.replace("{path}", &quoted)
}

/// Run every hook matching (`tool`, `path`) sequentially, returning one
/// warning line per hook that failed or timed out (empty = all quiet).
/// Success output is discarded — the caller re-reads the file and diffs, so
/// the model sees the effect, not the chatter.
pub async fn run_file_hooks(hooks: &[Hook], tool: &str, path: &Path, cwd: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for hook in hooks.iter().filter(|h| h.matches(tool, path, cwd)) {
        let cmd_line = render_command(&hook.run, path);
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &cmd_line]);
            c
        } else {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(&cmd_line);
            c
        };
        cmd.current_dir(cwd);
        // A file hook (a formatter, mostly) never reads stdin; leaving it
        // inherited would let it block on the TUI's terminal. Null it — the
        // lifecycle hooks below deliberately pipe stdin to feed their payload.
        // Pipe stdout/stderr: `wait_with_output()` only captures piped streams,
        // and inherited ones would print onto the TUI's alternate screen and
        // leave `out`'s failure `detail` empty. (`Command::output()`, which this
        // spawn replaced, set these implicitly.)
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        // Own process group / job object, so a hook that forks something
        // (a formatter shelling out) can be killed in full on timeout.
        crate::proc::configure(&mut cmd);
        let timeout = Duration::from_millis(hook.timeout_ms);
        // `Ok(Ok(out))` / `Ok(Err(spawn_err))` / `Err(Elapsed)` — same shape
        // `tokio::time::timeout(timeout, cmd.output()).await` produced, so the
        // match below is unchanged; spawning is just pulled out in front so we
        // can hold the pid/group needed to kill the whole tree on timeout.
        let ran = match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                let group = crate::proc::ProcessGroup::attach(&child);
                let ran = tokio::time::timeout(timeout, child.wait_with_output()).await;
                if ran.is_err()
                    && let Ok(group) = &group
                {
                    // The timer won: kill the whole tree, not just the
                    // direct child — `kill_on_drop` (already set above)
                    // reaps only the pid tokio spawned.
                    group.kill(pid);
                }
                ran
            }
            Err(e) => Ok(Err(e)),
        };
        match ran {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => {
                let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if detail.is_empty() {
                    detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
                }
                let detail = crate::truncate_inline(&detail, 300);
                notes.push(format!(
                    "[hook `{}` failed ({}){}]",
                    hook.run,
                    out.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
            }
            Ok(Err(e)) => notes.push(format!("[hook `{}` couldn't run: {e}]", hook.run)),
            Err(_) => notes.push(format!(
                "[hook `{}` timed out after {}ms; killed]",
                hook.run, hook.timeout_ms
            )),
        }
    }
    notes
}

// ── Lifecycle hooks ─────────────────────────────────────────────────────────

/// The agent events a lifecycle hook can attach to (config `event = "…"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Before a tool call executes. Exit 2 blocks the call.
    PreTool,
    /// After a tool call finishes (its result is in the payload).
    PostTool,
    /// When a user message is submitted, before the turn starts. Exit 2
    /// blocks the message; stdout is injected as extra context for the model.
    UserPrompt,
    /// After a turn completes.
    TurnEnd,
    /// When a session opens.
    SessionStart,
    /// When a session ends (app quit).
    SessionEnd,
}

impl HookEvent {
    /// Parse a config `event` string. `None` for an unknown name (skipped,
    /// lenient like the rest of config parsing).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "pre_tool" => Self::PreTool,
            "post_tool" => Self::PostTool,
            "user_prompt" => Self::UserPrompt,
            "turn_end" => Self::TurnEnd,
            "session_start" => Self::SessionStart,
            "session_end" => Self::SessionEnd,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::UserPrompt => "user_prompt",
            Self::TurnEnd => "turn_end",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
        }
    }
}

/// One configured lifecycle hook: run `run` when `event` fires (for the tool
/// events, only when the tool's name matches `on`; `*` matches every tool).
#[derive(Debug, Clone)]
pub struct EventHook {
    pub event: HookEvent,
    /// Tool-name filter for `pre_tool`/`post_tool` (`*` = any tool). Ignored
    /// by the non-tool events.
    pub on: String,
    /// Shell command; receives the event payload as JSON on stdin, plus
    /// `HRDR_HOOK_EVENT` / `HRDR_HOOK_TOOL` in the environment.
    pub run: String,
    /// Kill the hook after this long (default [`DEFAULT_HOOK_TIMEOUT_MS`]).
    pub timeout_ms: u64,
}

/// What a round of lifecycle hooks decided.
#[derive(Debug, Default)]
pub struct HookOutcome {
    /// `Some(reason)` when a hook exited 2: block the tool call / prompt.
    /// The first blocking hook wins; later hooks don't run.
    pub block: Option<String>,
    /// One warning per hook that failed (nonzero exit other than 2, couldn't
    /// spawn, or timed out) — non-blocking, surfaced to the model/user.
    pub notes: Vec<String>,
    /// Successful hooks' stdout (trimmed, non-empty only) — `user_prompt`
    /// hooks inject these as context for the model.
    pub context: Vec<String>,
}

/// Run every hook registered for `event` (and matching `tool`, for the tool
/// events) sequentially, feeding each `payload` as JSON on stdin.
pub async fn run_event_hooks(
    hooks: &[EventHook],
    event: HookEvent,
    tool: Option<&str>,
    payload: &serde_json::Value,
    cwd: &Path,
) -> HookOutcome {
    let mut out = HookOutcome::default();
    let matching = hooks.iter().filter(|h| {
        h.event == event
            && match tool {
                Some(t) => h.on == "*" || h.on == t,
                None => true,
            }
    });
    let payload = payload.to_string();
    for hook in matching {
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &hook.run]);
            c
        } else {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(&hook.run);
            c
        };
        cmd.current_dir(cwd)
            .env("HRDR_HOOK_EVENT", event.as_str())
            .env("HRDR_HOOK_TOOL", tool.unwrap_or(""))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group / job object, so a hung hook that forked
        // something (a background watcher, say) is fully killed on timeout,
        // not just the hook's own shell.
        crate::proc::configure(&mut cmd);
        let timeout = Duration::from_millis(hook.timeout_ms);
        // `Ok(Ok(out))` / `Ok(Err(spawn_err))` / `Err(Elapsed)` — same shape
        // as before; spawning is pulled out in front of the timed race so we
        // can hold the pid/group needed to kill the whole tree on timeout.
        let ran = match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                let group = crate::proc::ProcessGroup::attach(&child);
                let ran = tokio::time::timeout(timeout, async {
                    if let Some(mut stdin) = child.stdin.take() {
                        use tokio::io::AsyncWriteExt;
                        // A hook that never reads stdin is fine — the write
                        // fails when the pipe closes and we move on to
                        // waiting.
                        let _ = stdin.write_all(payload.as_bytes()).await;
                    }
                    child.wait_with_output().await
                })
                .await;
                if ran.is_err()
                    && let Ok(group) = &group
                {
                    // The timer won: kill the whole tree, not just the
                    // direct child — `kill_on_drop` (already set above)
                    // reaps only the pid tokio spawned.
                    group.kill(pid);
                }
                ran
            }
            Err(e) => Ok(Err(e)),
        };
        match ran {
            Ok(Ok(o)) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !stdout.is_empty() {
                    out.context.push(crate::truncate_inline(&stdout, 10_000));
                }
            }
            Ok(Ok(o)) if o.status.code() == Some(2) => {
                let mut reason = String::from_utf8_lossy(&o.stderr).trim().to_string();
                if reason.is_empty() {
                    reason = String::from_utf8_lossy(&o.stdout).trim().to_string();
                }
                if reason.is_empty() {
                    reason = format!("hook `{}` exited 2", hook.run);
                }
                out.block = Some(crate::truncate_inline(&reason, 2_000));
                break;
            }
            Ok(Ok(o)) => {
                let mut detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
                if detail.is_empty() {
                    detail = String::from_utf8_lossy(&o.stdout).trim().to_string();
                }
                let detail = crate::truncate_inline(&detail, 300);
                out.notes.push(format!(
                    "[hook `{}` failed ({}){}]",
                    hook.run,
                    o.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
            }
            Ok(Err(e)) => out
                .notes
                .push(format!("[hook `{}` couldn't run: {e}]", hook.run)),
            Err(_) => out.notes.push(format!(
                "[hook `{}` timed out after {}ms; killed]",
                hook.run, hook.timeout_ms
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(on: &str, glob: Option<&str>, run: &str) -> Hook {
        Hook {
            on: on.to_string(),
            glob: glob.map(|g| glob::Pattern::new(g).unwrap()),
            run: run.to_string(),
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }

    #[test]
    fn matching_by_tool_and_glob() {
        let cwd = Path::new("/proj");
        let h = hook("edit", Some("*.rs"), "true");
        assert!(h.matches("edit", Path::new("/proj/src/main.rs"), cwd));
        assert!(!h.matches("write", Path::new("/proj/src/main.rs"), cwd));
        assert!(!h.matches("edit", Path::new("/proj/README.md"), cwd));
        // `*` tool matches both; no glob matches every file.
        let any = hook("*", None, "true");
        assert!(any.matches("edit", Path::new("/proj/x"), cwd));
        assert!(any.matches("write", Path::new("/proj/x"), cwd));
        // Path-shaped globs match against the cwd-relative path.
        let nested = hook("edit", Some("src/**/*.rs"), "true");
        assert!(nested.matches("edit", Path::new("/proj/src/a/b.rs"), cwd));
        assert!(!nested.matches("edit", Path::new("/proj/tests/a.rs"), cwd));
    }

    #[test]
    fn command_rendering_quotes_path() {
        let cmd = render_command("fmt {path} && check {path}", Path::new("/tmp/a b.rs"));
        if cfg!(windows) {
            assert_eq!(cmd, "fmt \"/tmp/a b.rs\" && check \"/tmp/a b.rs\"");
        } else {
            assert_eq!(cmd, "fmt '/tmp/a b.rs' && check '/tmp/a b.rs'");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_run_fail_and_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        // A hook that mutates the file runs quietly…
        let ok = hook("edit", None, "printf y >> {path}");
        let notes = run_file_hooks(&[ok], "edit", &file, dir.path()).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xy");
        // …a failing hook reports, with its stderr…
        let bad = hook("edit", None, "echo broken >&2; exit 3");
        let notes = run_file_hooks(&[bad], "edit", &file, dir.path()).await;
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("failed") && notes[0].contains("broken"),
            "{}",
            notes[0]
        );
        // …and a hung hook is killed at its timeout.
        let mut slow = hook("edit", None, "sleep 5");
        slow.timeout_ms = 100;
        let notes = run_file_hooks(&[slow], "edit", &file, dir.path()).await;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("timed out"), "{}", notes[0]);
    }

    #[cfg(unix)] // only the unix-gated integration test below builds these
    fn event_hook(event: HookEvent, on: &str, run: &str) -> EventHook {
        EventHook {
            event,
            on: on.to_string(),
            run: run.to_string(),
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }

    #[test]
    fn event_names_round_trip() {
        for name in [
            "pre_tool",
            "post_tool",
            "user_prompt",
            "turn_end",
            "session_start",
            "session_end",
        ] {
            assert_eq!(HookEvent::parse(name).unwrap().as_str(), name);
        }
        assert!(HookEvent::parse("no_such_event").is_none());
        assert!(
            HookEvent::parse("file").is_none(),
            "file hooks aren't events"
        );
    }

    /// The whole contract in one pass: matching by event + tool, the JSON
    /// payload on stdin, exit 2 blocking with stderr as the reason (and
    /// stopping later hooks), other failures becoming notes, and stdout of a
    /// clean hook landing in `context`.
    #[cfg(unix)]
    #[tokio::test]
    async fn event_hooks_block_note_and_inject() {
        let dir = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({"event": "pre_tool", "tool": "bash"});

        // Only hooks for this event + tool run.
        let hooks = vec![
            event_hook(HookEvent::PostTool, "*", "exit 2"), // wrong event
            event_hook(HookEvent::PreTool, "edit", "exit 2"), // wrong tool
            event_hook(HookEvent::PreTool, "bash", "cat > /dev/null; echo saw-it"),
        ];
        let out = run_event_hooks(
            &hooks,
            HookEvent::PreTool,
            Some("bash"),
            &payload,
            dir.path(),
        )
        .await;
        assert!(out.block.is_none());
        assert!(out.notes.is_empty(), "{:?}", out.notes);
        assert_eq!(out.context, vec!["saw-it".to_string()]);

        // The payload arrives on stdin.
        let hooks = vec![event_hook(HookEvent::PreTool, "*", "grep -o pre_tool")];
        let out = run_event_hooks(
            &hooks,
            HookEvent::PreTool,
            Some("bash"),
            &payload,
            dir.path(),
        )
        .await;
        assert_eq!(out.context, vec!["pre_tool".to_string()]);

        // Exit 2 blocks with stderr as the reason and stops the chain.
        let hooks = vec![
            event_hook(HookEvent::PreTool, "*", "echo nope >&2; exit 2"),
            event_hook(HookEvent::PreTool, "*", "echo never-runs"),
        ];
        let out = run_event_hooks(
            &hooks,
            HookEvent::PreTool,
            Some("bash"),
            &payload,
            dir.path(),
        )
        .await;
        assert_eq!(out.block.as_deref(), Some("nope"));
        assert!(out.context.is_empty(), "the chain stopped at the block");

        // Any other failure is a non-blocking note.
        let hooks = vec![event_hook(
            HookEvent::TurnEnd,
            "*",
            "echo broken >&2; exit 1",
        )];
        let out = run_event_hooks(&hooks, HookEvent::TurnEnd, None, &payload, dir.path()).await;
        assert!(out.block.is_none());
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("broken"), "{}", out.notes[0]);
    }
}
