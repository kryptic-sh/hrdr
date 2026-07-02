//! Shared slash-command layer. The command *implementations* live here, behind
//! the [`CommandHost`] trait, so both frontends (TUI, GUI) drive the exact same
//! logic and gain new commands for free — a frontend just implements the host
//! capabilities (emit a line, access the agent, clipboard, sessions, …) and
//! calls [`dispatch`]. Frontend-coupled commands (scrolling, find/goto, expand,
//! theme/timestamps, editor) stay in the frontends and are handled before
//! delegating here.
//!
//! Async work (network, subprocess, filesystem, agent lock) is expressed as a
//! [`LineFuture`] the host spawns; its returned string (if non-empty) is shown
//! as a system line. This keeps the layer uniform across the sync-polled TUI and
//! the async-locked GUI, which both hold the agent as `Arc<tokio::sync::Mutex>`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use hrdr_agent::{Agent, Message, MessageRole, Session};
use tokio::sync::Mutex;

/// A future producing a system line to display (empty = show nothing). The host
/// spawns it on its runtime and pipes the result to its transcript.
pub type LineFuture = Pin<Box<dyn Future<Output = String> + Send>>;

/// What `/expand` should do to tool output (parsed by the shared dispatcher;
/// applied by the frontend, which owns the expansion state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    /// Show every tool result in full.
    All,
    /// Collapse everything.
    Off,
    /// Toggle the most recent tool result.
    ToggleLast,
}

/// The capabilities a frontend exposes so the shared commands can drive it.
pub trait CommandHost {
    /// Emit a system line immediately (on the UI thread).
    fn info(&mut self, line: String);
    /// Spawn `fut`; when it resolves, show its non-empty string as a system line.
    fn spawn_line(&self, fut: LineFuture);
    /// The shared agent handle (for async reads/mutations).
    fn agent(&self) -> Arc<Mutex<Agent>>;
    /// Working directory the tools operate in.
    fn cwd(&self) -> PathBuf;
    /// Current endpoint base URL (recorded into saved sessions).
    fn base_url(&self) -> String;

    /// The displayed model name.
    fn model(&self) -> String;
    /// Update the displayed model (the agent is switched separately by dispatch).
    fn set_model(&mut self, model: String);

    /// Whether `<think>` reasoning is shown.
    fn show_thinking(&self) -> bool;
    /// Toggle reasoning display (persisting if the frontend supports it).
    fn set_show_thinking(&mut self, on: bool);

    /// Reset to a fresh conversation (clear transcript + agent history + session).
    fn clear_conversation(&mut self);

    /// The active session's file id, if one has been assigned.
    fn session_id(&self) -> Option<String>;
    /// Override the session's display name (used by `/rename`).
    fn set_session_label(&mut self, name: String);
    /// Persist the current conversation (assigning/reusing the session id).
    fn autosave(&mut self);
    /// Restore a resolved session (rebuild the transcript, adopt id/model/label).
    fn resume(&mut self, id: String, session: Session);

    /// Copy `text` to the OS clipboard, returning a status line.
    fn copy_to_clipboard(&mut self, text: &str, label: &str) -> String;
    /// The most recent assistant reply, if any.
    fn last_reply(&self) -> Option<String>;
    /// The whole transcript as plain text (for `/copy all`).
    fn transcript_text(&self) -> String;
    /// The Nth (1-based) user/assistant message's text (for `/copy msg N[-M]`).
    fn nth_message_text(&self, n: usize) -> Option<String>;
    /// The most recent fenced code block (for `/copy code`). Default: from the
    /// last reply only; frontends may search further back.
    fn last_code_block(&self) -> Option<String> {
        self.last_reply()
            .as_deref()
            .and_then(crate::last_fenced_block)
    }

    /// Like [`spawn_line`](Self::spawn_line), but the resolved string is a
    /// unified diff (or a status/error line) — frontends with diff-aware
    /// rendering override to route real diffs accordingly.
    fn spawn_diff(&self, fut: LineFuture) {
        self.spawn_line(fut);
    }

    /// Whether a turn is currently running — the busy-guard for commands that
    /// mutate turn-coupled state (`/retry`, `/undo`, `/compact`, `/cwd`, …).
    fn is_busy(&self) -> bool;
    /// Launch a model turn with `prompt`. `show_as_user` displays it as a user
    /// message (`/retry`); `false` keeps it out of the transcript (`/init`).
    fn send_prompt(&mut self, prompt: String, show_as_user: bool);
    /// Replace the input buffer (`/undo` puts the rewound message back).
    fn set_input(&mut self, text: String);
    /// Prepend text to the input buffer (`/add` attaches a file block).
    fn prepend_input(&mut self, text: String);
    /// Insert text at the input cursor (`/paste`).
    fn insert_input(&mut self, text: String);
    /// Read the OS clipboard as text (`/paste`). `None` = unavailable.
    fn read_clipboard(&self) -> Option<String> {
        None
    }
    /// Apply an `/expand` mode to the tool-output display, returning the
    /// status line to show (the expansion state lives in the frontend).
    fn set_tool_expansion(&mut self, mode: ExpandMode) -> String;
    /// Rewind the last user turn: pop it (and the reply) from the agent
    /// history *and* the display transcript, returning the user's text.
    /// `None` when there's nothing to rewind (or the agent is locked).
    fn rewind_last_turn(&mut self) -> Option<String>;

    /// Persist one setting to the user config file. Default writes directly;
    /// the TUI overrides to also suppress its config hot-reload.
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        let _ = hrdr_agent::persist_setting(key, value);
    }
    /// The reasoning-effort label shown in status chrome.
    fn effort(&self) -> Option<String> {
        None
    }
    /// The session's display-name override (`/rename`), for `/info`.
    fn session_label(&self) -> Option<String> {
        None
    }
    /// The latest model call's `(prompt, completion)` token usage.
    fn context_usage(&self) -> Option<(u32, u32)> {
        None
    }
    /// The model's context window in tokens, if known.
    fn context_window(&self) -> Option<u32> {
        None
    }
    /// Session-cumulative `(input, output)` token counters.
    fn session_tokens(&self) -> (usize, usize) {
        (0, 0)
    }
    /// Update the effort label (persistence is dispatch's job).
    fn set_effort(&mut self, label: String) {
        let _ = label;
    }

    /// Called after `/cwd` switched the working directory (update dir/branch
    /// displays and invalidate any `@file` index).
    fn cwd_changed(&mut self, new: &Path) {
        let _ = new;
    }
    /// Called when files changed on disk outside a turn (`/revert`), so the
    /// `@file` completion index can be invalidated.
    fn files_changed(&mut self) {}

    /// Mark the in-flight turn as an `/init` run, so the frontend reloads
    /// `AGENTS.md` into the system prompt when it completes (via
    /// [`reload_project_docs`]).
    fn mark_init_turn(&mut self) {}

    /// Start conversation compaction on a background task. Frontends run
    /// [`run_compaction`] and, when it lands, show [`compaction_message`],
    /// reset their stale context usage, autosave on success, and resume any
    /// queued sends — same semantics in both.
    fn compact(&mut self, instructions: Option<String>);

    /// Current per-message timestamp style (frontends with timestamp rendering
    /// override the pair).
    fn timestamp_style(&self) -> crate::TimestampStyle {
        crate::TimestampStyle::Relative
    }
    /// Apply a timestamp style (persistence is dispatch's job).
    fn set_timestamp_style(&mut self, style: crate::TimestampStyle) {
        let _ = style;
    }
    /// Turns a completed TODO stays visible before pruning.
    fn todo_ttl(&self) -> u64 {
        crate::DEFAULT_TODO_TTL
    }
    /// Apply a TODO lifetime (persistence is dispatch's job).
    fn set_todo_ttl(&mut self, turns: u64) {
        let _ = turns;
    }

    /// Apply a theme (a path to an hjkl theme TOML; `None` = the bundled
    /// default). Persistence is dispatch's job.
    fn set_theme(&mut self, path: Option<String>) {
        let _ = path;
    }
    /// Remove one setting from the user config file (`/theme` reset). Default
    /// writes directly; the TUI overrides to suppress its hot-reload.
    fn unpersist_setting(&mut self, key: &str) {
        let _ = hrdr_agent::remove_setting(key);
    }

    /// Open `path` in an editor (`/edit`). The default launches the OS default
    /// handler (`xdg-open` / `open` / `start`) — right for GUI frontends; the
    /// TUI overrides to suspend the terminal and run `$EDITOR` instead.
    fn open_editor(&mut self, path: PathBuf) {
        let line = match open_system_handler(&path) {
            Ok(()) => format!("opened {} in the system editor", path.display()),
            Err(e) => format!("couldn't open {}: {e}", path.display()),
        };
        self.info(line);
    }

    /// Current status-bar mode (frontends with a status bar override the pair).
    fn statusbar_mode(&self) -> crate::StatusBarMode {
        crate::StatusBarMode::Truncate
    }
    /// Apply a status-bar mode (persistence is dispatch's job).
    fn set_statusbar_mode(&mut self, mode: crate::StatusBarMode) {
        let _ = mode;
    }

    /// Re-read config and apply the live-changeable settings (frontends decide
    /// what that covers and emit their own status lines).
    fn reload_config(&mut self) {
        self.info("reload isn't supported here".to_string());
    }

    /// Resolve a provider preset by name (built-ins + `[providers.<name>]`
    /// from config). `None` = unknown / no provider support.
    fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
        let _ = name;
        None
    }
    /// Update the displayed endpoint after a `/provider` switch.
    fn set_base_url(&mut self, url: String) {
        let _ = url;
    }
    /// Update the displayed context window after a `/provider` switch.
    fn set_context_window(&mut self, tokens: Option<u32>) {
        let _ = tokens;
    }

    /// Whether this frontend supports `cmd` (used to filter `/help`). Default
    /// matches the GUI: everything not in [`crate::TUI_ONLY_COMMANDS`].
    fn supports_command(&self, cmd: &str) -> bool {
        !crate::is_tui_only(cmd)
    }

    /// Frontend-specific keybinding tips appended to `/help`.
    fn help_tips(&self) -> Option<String> {
        None
    }
}

/// Handle a `/…` command shared by both frontends. Returns `true` if it was a
/// recognized command (and thus shouldn't be sent to the model). Unknown input
/// returns `false` so the caller can pass it to the model or handle it locally.
pub fn dispatch(host: &mut dyn CommandHost, input: &str) -> bool {
    let Some(rest) = input.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = crate::resolve_alias(parts.next().unwrap_or(""));
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "help" => {
            let mut s = crate::help_body_for(|name| host.supports_command(name));
            if let Some(tips) = host.help_tips() {
                s.push_str("\n\n");
                s.push_str(&tips);
            }
            host.info(s);
        }
        "clear" => {
            host.clear_conversation();
            host.info("conversation cleared".to_string());
        }
        "model" => {
            if arg.is_empty() {
                host.info(format!("model: {}", host.model()));
            } else {
                host.set_model(arg.clone());
                let agent = host.agent();
                let name = arg.clone();
                host.spawn_line(Box::pin(async move {
                    agent.lock().await.set_model(name);
                    String::new()
                }));
                host.info(format!("model → {arg}"));
            }
        }
        "models" => {
            let agent = host.agent();
            host.info("fetching models…".to_string());
            host.spawn_line(Box::pin(async move {
                let client = agent.lock().await.client();
                match client.list_models().await {
                    Ok(m) if !m.is_empty() => format!("models:\n  {}", m.join("\n  ")),
                    Ok(_) => "endpoint reported no models".to_string(),
                    Err(e) => format!("models error: {e}"),
                }
            }));
        }
        "tools" => {
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                let tools = agent.lock().await.tools();
                let mut msg = format!("{} tools:", tools.len());
                for (name, desc) in tools {
                    msg.push_str(&format!("\n  {name:<12}{desc}"));
                }
                msg
            }));
        }
        "info" => {
            let agent = host.agent();
            let model = host.model();
            let base_url = host.base_url();
            let cwd = host.cwd();
            let session = match (host.session_id(), host.session_label()) {
                (Some(id), Some(name)) => format!("{id}  (name: {name})"),
                (Some(id), None) => id,
                (None, _) => "(unsaved — send a message to start one)".to_string(),
            };
            let ctx = match (host.context_usage(), host.context_window()) {
                (Some((p, _)), Some(w)) => format!("{p} / {w}"),
                (Some((p, _)), None) => p.to_string(),
                _ => "—".to_string(),
            };
            let (tokens_in, tokens_out) = host.session_tokens();
            let effort = host.effort().unwrap_or_else(|| "—".to_string());
            host.spawn_line(Box::pin(async move {
                let temp = agent.lock().await.temperature();
                let dir = crate::display_dir(&cwd);
                let branch = crate::git_branch(&cwd).unwrap_or_else(|| "—".to_string());
                format!(
                    "session: {session}\nmodel: {model}\nendpoint: {base_url}\ncwd: {dir} \
                     ({branch})\ncontext: {ctx}\ntokens: ↑{tokens_in} ↓{tokens_out}\n\
                     temperature: {}\neffort: {effort}",
                    temp.map(|t| t.to_string())
                        .unwrap_or_else(|| "default".to_string())
                )
            }));
        }
        "copy" => {
            let lower = arg.to_ascii_lowercase();
            let toks: Vec<&str> = lower.split_whitespace().collect();
            let (text, label) = match toks.as_slice() {
                [] | ["reply"] | ["last"] => (host.last_reply(), "last reply".to_string()),
                ["code"] => (host.last_code_block(), "last code block".to_string()),
                ["all"] | ["transcript"] => {
                    (Some(host.transcript_text()), "transcript".to_string())
                }
                ["msg", spec] | ["message", spec] | ["m", spec] => {
                    let Some((a, b)) = crate::parse_msg_range(spec) else {
                        host.info("usage: /copy msg <N> or <N-M>".to_string());
                        return true;
                    };
                    let parts: Vec<String> =
                        (a..=b).filter_map(|n| host.nth_message_text(n)).collect();
                    let label = if a == b {
                        format!("message #{a}")
                    } else {
                        format!("messages #{a}-{b}")
                    };
                    ((!parts.is_empty()).then(|| parts.join("\n\n")), label)
                }
                _ => {
                    host.info("usage: /copy [code | all | msg N[-M]]".to_string());
                    return true;
                }
            };
            let line = match text {
                Some(t) if !t.is_empty() => host.copy_to_clipboard(&t, &label),
                _ => format!("nothing to copy ({label})"),
            };
            host.info(line);
        }
        "export" => {
            let agent = host.agent();
            let cwd = host.cwd();
            let arg = arg.clone();
            host.spawn_line(Box::pin(async move {
                let msgs = agent.lock().await.messages_owned();
                match export_conversation(&msgs, &cwd, &arg) {
                    Ok((path, lines)) => {
                        format!("exported transcript to {} ({lines} lines)", path.display())
                    }
                    Err(e) => format!("export failed: {e}"),
                }
            }));
        }
        "rename" => {
            if arg.is_empty() {
                host.info("usage: /rename <name>".to_string());
                return true;
            }
            host.set_session_label(arg.clone());
            host.autosave();
            host.info(format!("session renamed → {arg}"));
        }
        "diff" => {
            let cwd = host.cwd();
            host.spawn_diff(Box::pin(async move {
                match git_working_diff(&cwd).await {
                    Ok(d) if d.trim().is_empty() => "git diff: no changes".to_string(),
                    Ok(d) => d,
                    Err(e) => format!("git diff failed: {e}"),
                }
            }));
        }
        "thinking" | "reasoning" | "think" => {
            let on = if arg.is_empty() {
                !host.show_thinking()
            } else if let Some(b) = hrdr_agent::parse_env_bool(&arg) {
                b
            } else {
                host.info("usage: /thinking [on | off]".to_string());
                return true;
            };
            host.set_show_thinking(on);
            host.info(
                if on {
                    "thinking shown"
                } else {
                    "thinking hidden"
                }
                .to_string(),
            );
        }
        "temp" | "temperature" => {
            if arg.is_empty() {
                let agent = host.agent();
                host.spawn_line(Box::pin(async move {
                    let t = agent.lock().await.temperature();
                    format!(
                        "temperature: {}",
                        t.map(|t| t.to_string()).unwrap_or_else(|| "default".into())
                    )
                }));
            } else {
                match arg.parse::<f32>() {
                    Ok(t) => {
                        let agent = host.agent();
                        host.spawn_line(Box::pin(async move {
                            agent.lock().await.set_temperature(Some(t));
                            String::new()
                        }));
                        host.persist_setting(
                            "temperature",
                            hrdr_agent::ConfigValue::Float(t as f64),
                        );
                        host.info(format!("temperature → {t}"));
                    }
                    Err(_) => host.info("usage: /temp <number>".to_string()),
                }
            }
        }
        "effort" => {
            if arg.is_empty() {
                host.info(format!(
                    "effort: {}",
                    host.effort().unwrap_or_else(|| "—".into())
                ));
            } else {
                host.set_effort(arg.clone());
                host.persist_setting("effort", hrdr_agent::ConfigValue::Str(&arg));
                host.info(format!("effort → {arg}"));
            }
        }
        "cwd" => {
            let cur = host.cwd();
            if arg.is_empty() {
                host.info(format!("cwd: {}", cur.display()));
                return true;
            }
            if host.is_busy() {
                host.info("busy — try again after the current turn".to_string());
                return true;
            }
            let new = crate::resolve_under(&cur, &arg);
            if !new.is_dir() {
                host.info(format!("not a directory: {}", new.display()));
                return true;
            }
            let new = new.canonicalize().unwrap_or(new);
            let agent = host.agent();
            let target = new.clone();
            host.spawn_line(Box::pin(async move {
                agent.lock().await.set_cwd(target);
                String::new()
            }));
            host.cwd_changed(&new);
            host.info(format!("cwd → {}", new.display()));
        }
        "expand" => {
            let mode = match arg.to_ascii_lowercase().as_str() {
                "all" | "on" => ExpandMode::All,
                "off" | "none" | "collapse" => ExpandMode::Off,
                "" => ExpandMode::ToggleLast,
                _ => {
                    host.info("usage: /expand [all | off]".to_string());
                    return true;
                }
            };
            let status = host.set_tool_expansion(mode);
            host.info(status);
        }
        "add" => {
            if arg.is_empty() {
                host.info("usage: /add <file>".to_string());
                return true;
            }
            let path = crate::resolve_under(&host.cwd(), &arg);
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let n = content.lines().count();
                    host.prepend_input(format!("`{arg}`:\n```\n{content}\n```\n\n"));
                    host.info(format!("added {arg} ({n} lines) to the input"));
                }
                Err(e) => host.info(format!("can't read {arg}: {e}")),
            }
        }
        "paste" => {
            let Some(text) = host.read_clipboard().filter(|t| !t.is_empty()) else {
                host.info("clipboard unavailable or empty".to_string());
                return true;
            };
            // A single-line path to an existing file → attach as `@path`.
            let trimmed = text.trim();
            if !trimmed.is_empty()
                && !trimmed.contains('\n')
                && crate::resolve_under(&host.cwd(), trimmed).is_file()
            {
                host.insert_input(format!("@{trimmed} "));
                host.info(format!("attached @{trimmed} from clipboard"));
            } else {
                host.insert_input(text);
            }
        }
        "revert" => {
            if host.is_busy() {
                host.info("can't revert while a turn is running".to_string());
                return true;
            }
            host.files_changed(); // files may change; invalidate @-completion
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                let Some(cp) = agent.lock().await.checkpoints() else {
                    return "checkpoints are off (auto-disabled in git repos — use git, or set \
                            checkpoints = on)"
                        .to_string();
                };
                let result = match cp.lock() {
                    Ok(mut c) => c.revert_last(),
                    Err(_) => return "checkpoint store busy".to_string(),
                };
                match result {
                    Ok(files) if files.is_empty() => "nothing to revert".to_string(),
                    Ok(files) => {
                        let names = files
                            .iter()
                            .map(|p| {
                                p.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| p.display().to_string())
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("reverted {} file(s): {names}", files.len())
                    }
                    Err(e) => format!("revert failed: {e}"),
                }
            }));
        }
        "checkpoints" => {
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                let Some(cp) = agent.lock().await.checkpoints() else {
                    return "checkpoints are off (auto-disabled in git repos — use git, or set \
                            checkpoints = on)"
                        .to_string();
                };
                let infos = match cp.lock() {
                    Ok(c) => c.list(),
                    Err(_) => return "checkpoint store busy".to_string(),
                };
                if infos.is_empty() {
                    return "no file checkpoints yet".to_string();
                }
                let mut s =
                    String::from("file checkpoints (newest first; /revert undoes the latest):");
                for info in infos.iter().take(20) {
                    let names = info
                        .files
                        .iter()
                        .map(|f| {
                            std::path::Path::new(f)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| f.clone())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    s.push_str(&format!(
                        "\n  turn {} · {} file(s): {names}",
                        info.turn,
                        info.files.len()
                    ));
                }
                s
            }));
        }
        "retry" => {
            if host.is_busy() {
                host.info("can't retry while a turn is running".to_string());
                return true;
            }
            // Optional model switch for this retry (and subsequent turns).
            if !arg.is_empty() {
                host.set_model(arg.clone());
                let agent = host.agent();
                let name = arg.clone();
                host.spawn_line(Box::pin(async move {
                    agent.lock().await.set_model(name);
                    String::new()
                }));
                host.info(format!("model → {arg}"));
            }
            match host.rewind_last_turn() {
                Some(text) => host.send_prompt(text, true),
                None => host.info("nothing to retry".to_string()),
            }
        }
        "undo" => {
            if host.is_busy() {
                host.info("can't undo while a turn is running".to_string());
                return true;
            }
            match host.rewind_last_turn() {
                Some(text) => {
                    host.set_input(text);
                    host.autosave();
                    host.info("undid last turn — edit and resend".to_string());
                }
                None => host.info("nothing to undo".to_string()),
            }
        }
        "compact" => {
            if host.is_busy() {
                host.info("can't compact while a turn is running".to_string());
                return true;
            }
            host.compact((!arg.is_empty()).then(|| arg.clone()));
        }
        "init" => {
            if host.is_busy() {
                host.info("can't /init while a turn is running".to_string());
                return true;
            }
            host.info("/init — exploring the project to write AGENTS.md…".to_string());
            host.mark_init_turn();
            host.send_prompt(INIT_PROMPT.to_string(), false);
        }
        "timestamps" | "ts" => {
            use crate::TimestampStyle;
            let style = match arg.to_ascii_lowercase().as_str() {
                // No arg toggles between off and relative.
                "" => {
                    if host.timestamp_style() == TimestampStyle::None {
                        TimestampStyle::Relative
                    } else {
                        TimestampStyle::None
                    }
                }
                "none" | "off" | "hidden" => TimestampStyle::None,
                "relative" | "rel" | "on" => TimestampStyle::Relative,
                "exact" | "absolute" | "abs" => TimestampStyle::Exact,
                _ => {
                    host.info("usage: /timestamps [none | relative | exact]".to_string());
                    return true;
                }
            };
            host.set_timestamp_style(style);
            host.persist_setting(
                "timestamps",
                hrdr_agent::ConfigValue::Str(style.as_config_str()),
            );
            host.info(
                match style {
                    TimestampStyle::None => "timestamps: off",
                    TimestampStyle::Relative => "timestamps: relative",
                    TimestampStyle::Exact => "timestamps: exact (HH:MM)",
                }
                .to_string(),
            );
        }
        "todo-ttl" | "todottl" | "todos" => {
            if arg.is_empty() {
                let ttl = host.todo_ttl();
                host.info(format!(
                    "todo-ttl: {ttl} turn{}",
                    if ttl == 1 { "" } else { "s" }
                ));
                return true;
            }
            match arg.parse::<u64>() {
                Ok(n) => {
                    host.set_todo_ttl(n);
                    host.persist_setting("todo_ttl", hrdr_agent::ConfigValue::Int(n as i64));
                    host.info(format!(
                        "todo-ttl → {n} turn{}",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                Err(_) => {
                    host.info("usage: /todo-ttl <turns> (a whole number, e.g. 5)".to_string())
                }
            }
        }
        "theme" => {
            let path = (!arg.is_empty()).then(|| arg.clone());
            host.set_theme(path.clone());
            match path {
                Some(p) => {
                    host.persist_setting("theme", hrdr_agent::ConfigValue::Str(&p));
                    host.info(format!("theme → {p}"));
                }
                None => {
                    host.unpersist_setting("theme");
                    host.info("theme reset to default".to_string());
                }
            }
        }
        "edit" => {
            if arg.is_empty() {
                host.info("usage: /edit <file>".to_string());
                return true;
            }
            let path = crate::resolve_under(&host.cwd(), &arg);
            if !path.exists() {
                host.info(format!("file not found: {}", path.display()));
                return true;
            }
            host.open_editor(path);
        }
        "statusbar" => {
            use crate::StatusBarMode;
            let mode = match arg.to_ascii_lowercase().as_str() {
                // No arg cycles truncate → wrap → none.
                "" => match host.statusbar_mode() {
                    StatusBarMode::Truncate => StatusBarMode::Wrap,
                    StatusBarMode::Wrap => StatusBarMode::None,
                    StatusBarMode::None => StatusBarMode::Truncate,
                },
                "none" | "off" | "hidden" => StatusBarMode::None,
                "truncate" | "trunc" => StatusBarMode::Truncate,
                "wrap" => StatusBarMode::Wrap,
                _ => {
                    host.info("usage: /statusbar [none | truncate | wrap]".to_string());
                    return true;
                }
            };
            host.set_statusbar_mode(mode);
            host.persist_setting(
                "statusbar",
                hrdr_agent::ConfigValue::Str(mode.as_config_str()),
            );
            host.info(
                match mode {
                    StatusBarMode::None => "status bar: hidden",
                    StatusBarMode::Truncate => "status bar: truncate",
                    StatusBarMode::Wrap => "status bar: wrap",
                }
                .to_string(),
            );
        }
        "reload" => host.reload_config(),
        "provider" => {
            if arg.is_empty() {
                host.info("usage: /provider <name>".to_string());
                return true;
            }
            let Some(p) = host.resolve_provider(&arg) else {
                host.info(format!("unknown provider '{arg}'"));
                return true;
            };
            if host.is_busy() {
                host.info("busy — try again after the current turn".to_string());
                return true;
            }
            let key = p
                .api_key
                .clone()
                .or_else(|| p.key_env.as_ref().and_then(|e| std::env::var(e).ok()));
            let agent = host.agent();
            let (url, m) = (p.base_url.clone(), p.model.clone());
            host.spawn_line(Box::pin(async move {
                let mut a = agent.lock().await;
                a.set_endpoint(url, key);
                if let Some(m) = m {
                    a.set_model(m);
                }
                String::new()
            }));
            if let Some(m) = &p.model {
                host.set_model(m.clone());
            }
            if p.context_window.is_some() {
                host.set_context_window(p.context_window);
            }
            host.set_base_url(p.base_url.clone());
            host.info(format!("provider → {arg} ({})", p.base_url));
            if !p.remote {
                host.info(
                    "note: a running backend isn't restarted; relaunch hrdr for a local backend"
                        .to_string(),
                );
            }
        }
        "sessions" => {
            let all = crate::sessions_all_flag(&arg);
            host.info(crate::session_list_text(
                all,
                &host.cwd().display().to_string(),
            ));
        }
        "resume" | "load" => {
            if arg.is_empty() {
                host.info("usage: /resume <id or name> (see /sessions)".to_string());
                return true;
            }
            match hrdr_agent::resolve_session(&host.cwd().display().to_string(), &arg) {
                Some((id, session)) => host.resume(id, session),
                None => host.info(format!("no session matching '{arg}' (see /sessions)")),
            }
        }
        _ => return false,
    }
    true
}

/// Launch the OS default handler for `path` (`xdg-open` on Linux/BSD, `open`
/// on macOS, `start` on Windows), detached — the child outlives the call.
pub fn open_system_handler(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Instruction sent to the model by `/init` to author an `AGENTS.md`.
pub const INIT_PROMPT: &str = "\
Analyze this codebase and create an AGENTS.md file at the repository root to guide \
AI coding agents working here (the open standard at https://agents.md).

Do this:
1. Explore the project with your tools — read the README(s), the build/manifest \
   files (Cargo.toml, package.json, pyproject.toml, go.mod, Makefile, etc.), CI \
   config, and skim the source layout with glob/grep/read_file to understand how \
   it's organized.
2. If an AGENTS.md (or CLAUDE.md / .cursorrules / similar) already exists, read it \
   and improve it instead of discarding useful content.
3. Write AGENTS.md (use the write_file tool) with concise, repo-specific sections:
   - Project overview: what it is and does.
   - Setup / build / run: the actual commands for THIS repo.
   - Testing: how to run the test suite and a single test.
   - Code style & conventions: formatting, linting, naming — inferred from config \
     and existing code.
   - Architecture / layout: key directories and how they fit together.
   - Gotchas or rules an agent must follow.

Prefer real commands, paths, and specifics over generic advice. Keep it tight. \
When finished, give a one-line summary of what you wrote.";

// ---- representation-independent command cores ----

/// Probe the endpoint (list its models) and return a warning line when it
/// looks unreachable or doesn't advertise `model`; `None` when healthy. The
/// startup health-check core — both frontends spawn it and surface the
/// warning as a system line before the first turn.
pub async fn endpoint_health_warning(
    agent: Arc<Mutex<Agent>>,
    model: String,
    base_url: String,
) -> Option<String> {
    let client = agent.lock().await.client();
    match client.list_models().await {
        Err(e) => Some(format!("⚠ endpoint {base_url} looks unreachable: {e}")),
        Ok(models) => {
            if model != "default" && !models.is_empty() && !models.iter().any(|m| m == &model) {
                let sample = models
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "⚠ model '{model}' not found at {base_url}; available: {sample}"
                ))
            } else {
                None
            }
        }
    }
}

/// Re-read `AGENTS.md` into the system prompt (used by `/reload` and after an
/// `/init` turn writes the file). Returns the standard system line when
/// project docs were loaded, `None` when there are none.
pub async fn reload_project_docs(agent: Arc<Mutex<Agent>>) -> Option<String> {
    let mut a = agent.lock().await;
    let cwd = a.cwd();
    a.set_cwd(cwd); // re-runs the AGENTS.md gather for the (unchanged) cwd
    a.project_docs()
        .is_some()
        .then(|| "loaded AGENTS.md into the system prompt".to_string())
}

/// The shared compaction core (`/compact` and threshold auto-compaction):
/// lock the agent and summarize. `Ok((before, after))` with `before == after`
/// means there was nothing to compact.
pub async fn run_compaction(
    agent: Arc<Mutex<Agent>>,
    instructions: Option<String>,
) -> Result<(usize, usize), String> {
    let mut a = agent.lock().await;
    a.compact(instructions.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// The system line a finished compaction shows — identical in both frontends.
pub fn compaction_message(res: &Result<(usize, usize), String>) -> String {
    match res {
        Ok((before, after)) if before == after => "nothing to compact yet".to_string(),
        Ok((before, after)) => format!(
            "compacted: {before} → {after} messages (summary kept; scrollback above is \
             preserved for you)"
        ),
        Err(e) => format!("[compact failed] {e}"),
    }
}

/// Whether the context usage warrants a proactive compaction before more work
/// — the `auto_compact` threshold check, shared by both frontends.
/// `last_prompt_tokens` is the latest model call's prompt size.
pub fn should_auto_compact(
    last_prompt_tokens: Option<u32>,
    context_window: Option<u32>,
    ratio: f64,
) -> bool {
    if ratio <= 0.0 || ratio > 1.0 {
        return false;
    }
    let (Some(prompt), Some(window)) = (last_prompt_tokens, context_window) else {
        return false;
    };
    window > 0 && f64::from(prompt) >= f64::from(window) * ratio
}

/// The working-tree `git diff` for `cwd` (stdout on success, stderr message on
/// failure). Shared by `/diff`.
pub async fn git_working_diff(cwd: &Path) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .arg("diff")
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Write the conversation to a file per a `/export [--json] [file]` argument,
/// returning the path written and its line count. With no file, a timestamped
/// `hrdr-transcript-<date>.{md,json}` in `cwd` is used.
pub fn export_conversation(
    msgs: &[Message],
    cwd: &Path,
    arg: &str,
) -> Result<(PathBuf, usize), String> {
    let mut json = false;
    let mut file: Option<&str> = None;
    for tok in arg.split_whitespace() {
        if tok == "--json" {
            json = true;
        } else if file.is_none() {
            file = Some(tok);
        }
    }
    let path = match file {
        Some(f) => crate::resolve_under(cwd, f),
        None => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let ext = if json { "json" } else { "md" };
            cwd.join(format!("hrdr-transcript-{stamp}.{ext}"))
        }
    };
    let content = if json {
        conversation_to_json(msgs)
    } else {
        conversation_to_markdown(msgs)
    };
    std::fs::write(&path, &content).map_err(|e| e.to_string())?;
    Ok((path, content.lines().count()))
}

/// The conversation's user/assistant turns as Markdown.
pub fn conversation_to_markdown(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m.role {
            MessageRole::User => {
                if let Some(c) = &m.content {
                    out.push_str(&format!("## User\n{c}\n\n"));
                }
            }
            MessageRole::Assistant => {
                if let Some(c) = &m.content
                    && !c.is_empty()
                {
                    out.push_str(&format!("## Assistant\n{c}\n\n"));
                }
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// The conversation's user/assistant turns as a JSON array of `{n, role, content}`.
pub fn conversation_to_json(msgs: &[Message]) -> String {
    let mut arr = Vec::new();
    let mut num = 0;
    for m in msgs {
        let (role, content) = match m.role {
            MessageRole::User => ("user", m.content.as_deref()),
            MessageRole::Assistant => ("assistant", m.content.as_deref()),
            _ => continue,
        };
        let Some(content) = content.filter(|c| !c.is_empty()) else {
            continue;
        };
        num += 1;
        arr.push(serde_json::json!({ "n": num, "role": role, "content": content }));
    }
    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_compact_threshold_and_messages() {
        // Below / at threshold, disabled ratio, missing inputs.
        assert!(should_auto_compact(Some(850), Some(1000), 0.85));
        assert!(should_auto_compact(Some(900), Some(1000), 0.85));
        assert!(!should_auto_compact(Some(840), Some(1000), 0.85));
        assert!(!should_auto_compact(Some(999), Some(1000), 0.0)); // disabled
        assert!(!should_auto_compact(None, Some(1000), 0.85));
        assert!(!should_auto_compact(Some(999), None, 0.85));
        // Message formatting covers the three outcomes.
        assert_eq!(compaction_message(&Ok((2, 2))), "nothing to compact yet");
        assert!(compaction_message(&Ok((10, 2))).contains("compacted: 10 → 2"));
        assert!(compaction_message(&Err("boom".into())).contains("[compact failed] boom"));
    }

    #[test]
    fn conversation_export_covers_only_user_assistant() {
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::assistant(""), // empty assistant (tool-call turn) skipped
        ];
        let md = conversation_to_markdown(&msgs);
        assert!(md.contains("## User\nhello"));
        assert!(md.contains("## Assistant\nhi there"));
        assert!(!md.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&conversation_to_json(&msgs)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
    }
}
