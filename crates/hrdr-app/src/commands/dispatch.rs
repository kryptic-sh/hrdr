use std::path::Path;

use super::conversation::export_conversation;
use super::helpers::{RESUME_BUSY_MSG, busy_generic, busy_guard, git_working_diff};
use super::host::CommandHost;
use super::model::{endpoint_health_warning, switch_model};
use super::types::ExpandMode;

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
        // `/new`, `/clear`, `/reset` — optionally naming the fresh session, so it
        // saves under that name instead of one derived from its first message.
        "new" => {
            host.clear_conversation();
            if arg.is_empty() {
                host.info("conversation cleared".to_string());
            } else {
                host.set_session_label(arg.clone());
                host.info(format!("new session '{arg}'"));
            }
        }
        "model" => {
            // Always the interactive picker (a frontend that supports it; the
            // default lists models as text). Switching provider + model by
            // name still works via the picker's fuzzy filter.
            host.begin_model_selector();
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
        "prompt" | "system" => {
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                match agent.lock().await.system_prompt() {
                    Some(p) => format!("system prompt ({} chars):\n{p}", p.chars().count()),
                    None => "no system prompt is set".to_string(),
                }
            }));
        }
        "guardrails" | "rails" => {
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                let specs = agent.lock().await.guardrail_specs();
                let mut msg = format!(
                    "{} guardrails (blocked shell commands; add more via [[guardrails]] in config):",
                    specs.len()
                );
                for (pattern, message) in specs {
                    msg.push_str(&format!("\n  {pattern}\n    → {message}"));
                }
                msg
            }));
        }
        "status" => {
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
            let cost = host.session_cost();
            let cost_line = if cost > 0.0 {
                format!("\ncost: {} (est.)", crate::fmt_cost(cost))
            } else {
                String::new()
            };
            let effort = host.effort().unwrap_or_else(|| "—".to_string());
            host.spawn_line(Box::pin(async move {
                let (temp, messages, cache) = {
                    let a = agent.lock().await;
                    (a.temperature(), a.message_count(), a.prompt_cache_active())
                };
                let dir = crate::display_dir(&cwd);
                let branch = crate::git_branch(&cwd).unwrap_or_else(|| "—".to_string());
                format!(
                    "session: {session}\nmodel: {model}\nendpoint: {base_url}\ncwd: {dir} \
                     ({branch})\ncontext: {ctx}\ntokens: ↑{tokens_in} ↓{tokens_out}{cost_line}\n\
                     temperature: {}\neffort: {effort}\nprompt cache: {}\nmessages: {messages}",
                    temp.map(|t| t.to_string())
                        .unwrap_or_else(|| "default".to_string()),
                    if cache { "on" } else { "off" }
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
            // Always the interactive picker (a frontend that supports it; the
            // default lists the model's levels as text). It offers the levels
            // the current model actually accepts, per the models.dev catalog.
            host.begin_effort_selector();
        }
        "cwd" => {
            let cur = host.cwd();
            if arg.is_empty() {
                host.info(format!("cwd: {}", cur.display()));
                return true;
            }
            if host.is_busy() {
                host.info(busy_generic());
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
            let content = match hrdr_tools::read_attach_file(&arg, &host.cwd()) {
                Ok(content) => content,
                Err(e) => {
                    host.info(format!("can't add {arg}: {e}"));
                    return true;
                }
            };
            // `expand_mentions` (`@file`) caps attached content at
            // `MAX_ATTACH_BYTES` and truncates; `/add` names one file
            // explicitly, so silently truncating it would be more confusing
            // than useful — reject it with a clear error instead.
            if content.len() > crate::MAX_ATTACH_BYTES {
                host.info(format!(
                    "{arg} is {} KiB, over the {} KiB /add limit — too large to attach",
                    content.len() / 1024,
                    crate::MAX_ATTACH_BYTES / 1024,
                ));
                return true;
            }
            let n = content.lines().count();
            host.prepend_input(format!("`{arg}`:\n```\n{content}\n```\n\n"));
            host.info(format!("added {arg} ({n} lines) to the input"));
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
                host.info(busy_guard("revert"));
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
                host.info(busy_guard("retry"));
                return true;
            }
            // Optional model switch for this retry (and subsequent turns).
            if !arg.is_empty() {
                switch_model(host, arg.clone());
                host.info(format!("model → {arg}"));
            }
            match host.rewind_last_turn() {
                Some(text) => host.send_prompt(text, true),
                None => host.info("nothing to retry".to_string()),
            }
        }
        "undo" => {
            if host.is_busy() {
                host.info(busy_guard("undo"));
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
                host.info(busy_guard("compact"));
                return true;
            }
            host.compact((!arg.is_empty()).then(|| arg.clone()));
        }
        "init" => {
            if host.is_busy() {
                host.info(busy_guard("/init"));
                return true;
            }
            host.info("/init — exploring the project to write AGENTS.md…".to_string());
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
            if arg.is_empty() {
                // No argument: open the interactive picker (a frontend that
                // supports it; the default lists the themes as text).
                host.begin_theme_selector();
                return true;
            }
            let path = (!matches!(arg.as_str(), "reset" | "default")).then(|| arg.clone());
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
        "skills" => {
            // Interactive picker where supported; the default host lists the
            // skills as text (see CommandHost::begin_skill_selector).
            host.begin_skill_selector();
        }
        "login" => host.begin_login(),
        "resume" | "load" => {
            if arg.is_empty() {
                // No argument: open the interactive session picker (a frontend
                // that supports it; the default lists sessions as text).
                host.begin_session_selector();
                return true;
            }
            if host.is_busy() {
                host.info(RESUME_BUSY_MSG.to_string());
                return true;
            }
            match crate::resolve_session(&host.cwd().display().to_string(), &arg) {
                Some((id, session)) => host.resume(id, session),
                None => host.info(format!("no session matching '{arg}' (see /resume)")),
            }
        }
        "cost" => {
            let (tokens_in, tokens_out) = host.session_tokens();
            let mut line = format!("session tokens: ↑{tokens_in} input · ↓{tokens_out} output");
            let cost = host.session_cost();
            if cost > 0.0 {
                line.push_str(&format!(" · est. {}", crate::fmt_cost(cost)));
            }
            host.info(line);
        }
        "doctor" => {
            let agent = host.agent();
            let model = host.model();
            let base_url = host.base_url();
            let cwd = host.cwd();
            let ctx_win = host.context_window();
            let in_git = hrdr_agent::in_git_repo(&cwd);
            let branch = crate::git_branch(&cwd).unwrap_or_else(|| "—".to_string());
            let config_path = hrdr_agent::config_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            let auth_path = hrdr_agent::auth_file_path()
                .map(|p| {
                    let exists = p.exists();
                    format!(
                        "{} ({})",
                        p.display(),
                        if exists { "found" } else { "not found" }
                    )
                })
                .unwrap_or_else(|| "—".to_string());
            let ctx_win_str = ctx_win.map_or_else(|| "—".to_string(), |w| w.to_string());
            host.info(format!(
                "model: {model}\nendpoint: {base_url}\ncontext window: {ctx_win_str}\n\
                 cwd: {} ({in_git})\nbranch: {branch}\nconfig: {config_path}\n\
                 auth: {auth_path}\nprobing endpoint…",
                crate::display_dir(&cwd),
                in_git = if in_git { "git repo" } else { "not a git repo" },
            ));
            host.spawn_line(Box::pin(async move {
                let ep = endpoint_health_warning(agent.clone(), model, base_url).await;
                let mut out = match ep {
                    Some(w) => w,
                    None => "✓ endpoint healthy".to_string(),
                };
                out.push('\n');
                out.push_str(&lsp_status_text(&agent).await);
                out
            }));
        }
        _ => return false,
    }
    true
}

/// The `/doctor` LSP block: whether post-edit diagnostics are enabled, and one
/// line per configured server with its lifecycle status.
async fn lsp_status_text(agent: &std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>>) -> String {
    match agent.lock().await.lsp_statuses().await {
        None => "lsp: disabled".to_string(),
        Some((wait_ms, reports)) => {
            let mut out = format!("lsp: enabled (wait {wait_ms}ms)");
            for r in reports {
                out.push_str(&format!(
                    "\n  {} (.{}): {}",
                    r.command,
                    r.extensions.join("/."),
                    r.status.label()
                ));
            }
            out
        }
    }
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
   config, and skim the source layout with find/ls/grep/read to understand how \
   it's organized.
2. If an AGENTS.md (or CLAUDE.md / .cursorrules / similar) already exists, read it \
   and improve it instead of discarding useful content.
3. Write AGENTS.md (use the write tool) with concise, repo-specific sections:
   - Project overview: what it is and does.
   - Setup / build / run: the actual commands for THIS repo.
   - Testing: how to run the test suite and a single test.
   - Code style & conventions: formatting, linting, naming — inferred from config \
     and existing code.
   - Architecture / layout: key directories and how they fit together.
   - Gotchas or rules an agent must follow.

Prefer real commands, paths, and specifics over generic advice. Keep it tight. \
When finished, give a one-line summary of what you wrote.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use crate::commands::types::LineKind;
    use hrdr_agent::{Agent, AgentConfig};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Minimal `CommandHost` mock. Only the handful of methods the commands
    /// under test actually touch are meaningfully implemented; everything
    /// else is a harmless stub — proving (by never being hit) that these
    /// tests don't accidentally exercise more than they mean to.
    struct TestHost {
        cwd: std::path::PathBuf,
        agent: Arc<Mutex<Agent>>,
        info_log: Vec<String>,
        busy: bool,
        model: hrdr_agent::ModelRef,
        input: String,
    }

    impl TestHost {
        fn new(cwd: std::path::PathBuf) -> Self {
            // Dispatching a command runs the real code: `/model` reads the last-used
            // store, an agent refreshes the models.dev cache. Not from the developer's
            // `$HOME` — the sandbox ctor moved it before this binary reached `main`.
            let agent = Agent::new(AgentConfig {
                cwd: cwd.clone(),
                model: "local://test-model".parse().unwrap(),
                ..Default::default()
            })
            .unwrap();
            Self {
                cwd,
                agent: Arc::new(Mutex::new(agent)),
                info_log: Vec::new(),
                busy: false,
                model: "local://test-model".parse().unwrap(),
                input: String::new(),
            }
        }
    }

    impl CommandHost for TestHost {
        fn info(&mut self, line: String) {
            self.info_log.push(line);
        }
        fn agent(&self) -> Arc<Mutex<Agent>> {
            self.agent.clone()
        }
        fn cwd(&self) -> std::path::PathBuf {
            self.cwd.clone()
        }
        fn base_url(&self) -> String {
            "http://test.invalid".to_string()
        }
        fn model_ref(&self) -> hrdr_agent::ModelRef {
            self.model.clone()
        }
        fn set_model_ref(&mut self, reference: hrdr_agent::ModelRef) {
            self.model = reference;
        }
        fn show_thinking(&self) -> bool {
            false
        }
        fn set_show_thinking(&mut self, _on: bool) {}
        fn clear_conversation(&mut self) {}
        fn session_id(&self) -> Option<String> {
            None
        }
        fn set_session_label(&mut self, _name: String) {}
        fn autosave(&mut self) {}
        fn resume(&mut self, _id: String, _session: Session) {}
        fn copy_to_clipboard(&mut self, _text: &str, _label: &str) -> String {
            String::new()
        }
        fn last_reply(&self) -> Option<String> {
            None
        }
        fn transcript_text(&self) -> String {
            String::new()
        }
        fn nth_message_text(&self, _n: usize) -> Option<String> {
            None
        }
        fn line_poster(&self) -> Box<dyn Fn(LineKind, String) + Send> {
            Box::new(|_, _| {})
        }
        fn is_busy(&self) -> bool {
            self.busy
        }
        fn send_prompt(&mut self, _prompt: String, _show_as_user: bool) {}
        fn set_input(&mut self, text: String) {
            self.input = text;
        }
        fn prepend_input(&mut self, text: String) {
            self.input = format!("{text}{}", self.input);
        }
        fn insert_input(&mut self, text: String) {
            self.input.push_str(&text);
        }
        fn set_tool_expansion(&mut self, _mode: ExpandMode) -> String {
            String::new()
        }
        fn rewind_last_turn(&mut self) -> Option<String> {
            None
        }
        fn start_compaction(&mut self, _instructions: Option<String>) {}
    }

    /// `/add` applies the same attach-size cap as `@file` mentions
    /// (`MAX_ATTACH_BYTES`), but errors clearly instead of silently
    /// truncating — the user named this one file explicitly.
    #[tokio::test]
    async fn add_rejects_a_file_over_the_attach_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("huge.txt"),
            "x".repeat(crate::MAX_ATTACH_BYTES + 1),
        )
        .unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/add huge.txt"));
        assert!(
            host.info_log.iter().any(|l| l.contains("too large")),
            "{:?}",
            host.info_log
        );
        assert!(
            host.input.is_empty(),
            "the oversized file must not be attached"
        );
    }

    #[tokio::test]
    async fn add_attaches_a_file_within_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), "hello from small").unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/add small.txt"));
        assert!(host.input.contains("hello from small"), "{:?}", host.input);
    }

    /// `/model` always opens the picker — an argument no longer switches
    /// directly (the picker's fuzzy filter covers that), so the displayed
    /// model must not change from dispatch alone.
    #[tokio::test]
    async fn model_opens_the_picker_and_ignores_arguments() {
        let mut host = TestHost::new(std::env::temp_dir());

        assert!(dispatch(&mut host, "/model other-model"));
        assert_eq!(
            host.model,
            "local://test-model".parse().unwrap(),
            "/model must not switch the model directly"
        );
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("model selector isn't available")),
            "the default host reports the picker as unavailable: {:?}",
            host.info_log
        );
    }

    /// `/add` attaches files outside the working directory (full-access default):
    /// a `..` escape and an absolute path both go through. Only secret/credential
    /// files stay off-limits (see `add_rejects_secret_file`).
    #[tokio::test]
    async fn add_allows_paths_outside_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("leak.txt");
        std::fs::write(&outside, "data").unwrap();

        // Relative `..` escape.
        let mut host = TestHost::new(root.clone());
        assert!(dispatch(&mut host, "/add ../leak.txt"));
        assert!(
            !host.input.is_empty(),
            "a path above cwd must attach, got info_log: {:?}",
            host.info_log
        );

        // Absolute path outside cwd.
        let mut host = TestHost::new(root);
        assert!(dispatch(&mut host, &format!("/add {}", outside.display())));
        assert!(!host.input.is_empty(), "info_log: {:?}", host.info_log);
    }

    /// `/add` rejects secret/credential files
    #[tokio::test]
    async fn add_rejects_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        let mut host = TestHost::new(root);

        assert!(dispatch(&mut host, "/add .env"));
        assert!(
            host.info_log.iter().any(|l| l.contains("secret")),
            "expected secret-file error, got: {:?}",
            host.info_log
        );
        assert!(host.input.is_empty());
    }

    /// `/add` accepts a valid nested file
    #[tokio::test]
    async fn add_accepts_nested_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("nested.txt"), "nested content").unwrap();
        let mut host = TestHost::new(root);

        assert!(dispatch(&mut host, "/add sub/nested.txt"));
        assert!(host.input.contains("nested content"), "{:?}", host.input);
    }
}
