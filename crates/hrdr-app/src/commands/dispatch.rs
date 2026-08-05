use std::path::Path;

use super::conversation::export_conversation;
use super::helpers::{RESUME_BUSY_MSG, busy_generic, busy_guard};
use super::host::CommandHost;
use super::model::endpoint_health_warning;
use super::types::ExpandMode;

/// Handle a `/…` command, independent of renderer. Returns `true` if it was a
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
                let s = crate::fmt_cost_maybe_partial(cost, host.session_cost_partial());
                format!("\ncost: {s} (est.)")
            } else {
                String::new()
            };
            let effort = host.effort().unwrap_or_else(|| "—".to_string());
            // What the cache actually DID this session, appended to whether it
            // is switched on — the two belong on one line, because "on" alone
            // says the breakpoints were sent, not that anything was served from
            // them. Absent when the endpoint publishes no figures.
            let observed = host
                .session_cache()
                .map(|(rate, read, written)| {
                    format!(
                        " · {:.0}% read ({}), {} written",
                        rate * 100.0,
                        crate::fmt_count(read),
                        crate::fmt_count(written)
                    )
                })
                .unwrap_or_default();
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
                     temperature: {}\neffort: {effort}\nprompt cache: {}{observed}\n\
                     messages: {messages}",
                    temp.map(|t| t.to_string())
                        .unwrap_or_else(|| "default".to_string()),
                    if cache { "on" } else { "off" }
                )
            }));
        }
        "export" => {
            let agent = host.agent();
            let cwd = host.cwd();
            let arg = arg.clone();
            host.spawn_line(Box::pin(async move {
                let msgs = agent.lock().await.messages_owned();
                // The serialization and the fs write are blocking work; run
                // them off the async worker.
                match tokio::task::spawn_blocking(move || export_conversation(&msgs, &cwd, &arg))
                    .await
                {
                    Ok(Ok((path, lines))) => {
                        format!("exported transcript to {} ({lines} lines)", path.display())
                    }
                    Ok(Err(e)) => format!("export failed: {e}"),
                    Err(e) => format!("export task failed: {e}"),
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
            } else if matches!(arg.to_ascii_lowercase().as_str(), "default" | "reset") {
                let agent = host.agent();
                host.spawn_line(Box::pin(async move {
                    agent.lock().await.set_temperature(None);
                    String::new()
                }));
                host.unpersist_setting("temperature");
                host.info("temperature → default".to_string());
            } else {
                match arg.parse::<f32>() {
                    Ok(t) if t.is_finite() && (0.0..=2.0).contains(&t) => {
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
                    _ => host.info("usage: /temp <0-2> | default".to_string()),
                }
            }
        }
        "effort" => {
            if arg.is_empty() {
                // Always the interactive picker (a frontend that supports it;
                // the default lists the model's levels as text). It offers the
                // levels the current model actually accepts, per the models.dev
                // catalog.
                host.begin_effort_selector();
            } else {
                // `/effort <name>` applies a level directly, mirroring the
                // picker's apply-selected path: match by value or label
                // (case-insensitive), or the Default row for default/reset.
                let reference = host.model_ref();
                let choices =
                    crate::effort_choices(Some(reference.provider().as_str()), reference.model());
                let arg_lower = arg.to_ascii_lowercase();
                let choice = if matches!(arg_lower.as_str(), "default" | "reset") {
                    choices.iter().find(|c| c.value.is_none())
                } else {
                    choices.iter().find(|c| {
                        c.value
                            .as_deref()
                            .is_some_and(|v| v.eq_ignore_ascii_case(&arg))
                            || c.label.eq_ignore_ascii_case(&arg)
                    })
                };
                match choice {
                    Some(c) => {
                        match &c.value {
                            Some(v) => {
                                host.persist_setting("effort", hrdr_agent::ConfigValue::Str(v))
                            }
                            None => host.unpersist_setting("effort"),
                        }
                        host.set_effort(c.value.clone());
                        host.info(match &c.value {
                            Some(v) => format!("effort → {} ({v})", c.label),
                            None => "effort → default (the model/provider default)".to_string(),
                        });
                    }
                    None => {
                        let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
                        host.info(format!(
                            "unknown effort '{arg}' — available: {}",
                            labels.join(", ")
                        ));
                    }
                }
            }
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
            // Trust is answered per directory, when hrdr opens in one, and it
            // cannot be asked again here: the TUI owns the terminal by now, and
            // the question is a security decision that must not be reduced to a
            // line of chat the model could later be talked into answering. So an
            // unanswered directory is refused rather than entered — otherwise a
            // trusted session could walk into a fresh checkout and read its
            // AGENTS.md with the tool set the *first* directory earned.
            if !hrdr_agent::trust::is_trusted(&new) {
                host.info(format!(
                    "not a trusted directory: {} — hrdr asks about a directory when it opens \
                     there, and cannot ask mid-session. Start hrdr in it to answer.",
                    new.display()
                ));
                return true;
            }
            let agent = host.agent();
            let target = new.clone();
            host.spawn_line(Box::pin(async move {
                agent.lock().await.set_cwd(target);
                String::new()
            }));
            host.cwd_changed(&new);
            host.info(format!("cwd → {}", new.display()));
        }
        "verbose" => {
            // A strict on/off toggle: a bare `/verbose` flips the current
            // state, `on`/`off` set it. On expands every tool block and shows
            // the model's thinking; off collapses them and hands the display
            // back to per-block clicking. The frontend owns the expansion
            // state, so it reads it back to decide which way a bare flip goes.
            let on = if arg.is_empty() {
                !host.tool_expansion_on()
            } else if let Some(b) = hrdr_agent::parse_env_bool(&arg) {
                b
            } else {
                host.info("usage: /verbose [on | off]".to_string());
                return true;
            };
            let status =
                host.set_tool_expansion(if on { ExpandMode::All } else { ExpandMode::Off });
            host.info(status);
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
        "compact" => {
            // A message after `/compact` steers the summary's focus. While the
            // agent is mid-turn the compaction is queued, not refused and not
            // steered: it runs after the turn ends (the frontend drains the
            // queue at turn completion), so the model is never interrupted with
            // it the way a steer would be.
            let instructions = (!arg.is_empty()).then(|| arg.clone());
            if host.is_busy() {
                host.queue_compaction(instructions);
            } else {
                host.start_compaction(instructions);
            }
        }
        "init" => {
            if host.is_busy() {
                host.info(busy_guard("/init"));
                return true;
            }
            host.info("/init — exploring the project to write AGENTS.md…".to_string());
            host.send_prompt(INIT_PROMPT.to_string(), false);
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
                    // Clamp to i64::MAX for TOML persistence so very large
                    // values don't wrap to negative on reload.
                    let clamped = n.min(i64::MAX as u64) as i64;
                    host.persist_setting("todo_ttl", hrdr_agent::ConfigValue::Int(clamped));
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
            if !arg.is_empty() {
                host.info("/skills takes no argument — skills run via `:name`".to_string());
            }
            // Interactive picker where supported; the default host lists the
            // skills as text (see CommandHost::begin_skill_selector).
            host.begin_skill_selector();
        }
        "login" => {
            if !arg.is_empty() {
                host.info("/login takes no argument".to_string());
            }
            host.begin_login();
        }
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
            // The one figure that says whether the prompt cache is working
            // across the session, rather than on whichever call was last.
            // Omitted entirely — not shown as 0% — when the endpoint publishes
            // no cache figures at all.
            if let Some((rate, read, written)) = host.session_cache() {
                line.push_str(&format!(
                    " · prompt cache: {:.0}% read ({}), {} written",
                    rate * 100.0,
                    crate::fmt_count(read),
                    crate::fmt_count(written)
                ));
            }
            let cost = host.session_cost();
            if cost > 0.0 {
                let s = crate::fmt_cost_maybe_partial(cost, host.session_cost_partial());
                line.push_str(&format!(" · est. {s}"));
            }
            host.info(line);
        }
        "doctor" => {
            let agent = host.agent();
            let model = host.model();
            let base_url = host.base_url();
            let cwd = host.cwd();
            let ctx_win = host.context_window();
            let config_path = hrdr_agent::config_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            let ctx_win_str = ctx_win.map_or_else(|| "—".to_string(), |w| w.to_string());
            // Only pure computations here: every filesystem probe (git
            // discovery, the auth-file check) runs in the spawned task below,
            // not on the UI thread.
            host.info(format!(
                "model: {model}\nendpoint: {base_url}\ncontext window: {ctx_win_str}\n\
                 cwd: {}\nconfig: {config_path}\nprobing endpoint…",
                crate::display_dir(&cwd),
            ));
            host.spawn_line(Box::pin(async move {
                // `in_git_repo` walks ancestors calling `.exists()` and
                // `git_branch` reads `.git/HEAD` up the tree — both belong
                // here, off the UI thread, along with the auth-file check.
                let git_line = if hrdr_agent::in_git_repo(&cwd) {
                    let branch = crate::git_branch(&cwd).unwrap_or_else(|| "—".to_string());
                    format!("git: on branch {branch}")
                } else {
                    "git: not a repo".to_string()
                };
                let auth_line = hrdr_agent::auth_file_path()
                    .map(|p| {
                        let exists = p.exists();
                        format!(
                            "auth: {} ({})",
                            p.display(),
                            if exists { "found" } else { "not found" }
                        )
                    })
                    .unwrap_or_else(|| "auth: —".to_string());
                let ep = endpoint_health_warning(agent.clone(), model, base_url).await;
                let mut out = match ep {
                    Some(w) => w,
                    None => "✓ endpoint healthy".to_string(),
                };
                out = format!("{git_line}\n{auth_line}\n{out}");
                out.push('\n');
                out.push_str(&lsp_status_text(&agent).await);
                // Session health: report any corrupt/unreadable files.
                let diags = crate::session_diagnostics();
                if !diags.is_empty() {
                    out.push_str(&format!("\nsessions: {} corrupt file(s)", diags.len()));
                    for (path, err) in &diags {
                        out.push_str(&format!("\n  {path}: {err}"));
                    }
                } else {
                    out.push_str("\nsessions: ✓");
                }
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
        Some((wait_secs, reports)) => {
            let mut out = format!("lsp: enabled (wait {wait_secs}ms)");
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
        // `cmd.exe` treats `&` as a command separator regardless of the
        // caller's own argument quoting (`std::process::Command` only quotes
        // for embedded whitespace/quotes, which doesn't help here), so an
        // OAuth authorize URL's `...&state=...` query string would get
        // silently truncated at the first `&` by `start`. Caret-escape it so
        // `cmd` sees a literal character instead of an operator.
        let target = windows_escape_for_cmd_start(&path.to_string_lossy());
        c.args(["/C", "start", ""]).arg(target);
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

/// Escape a string for use as the target argument to `cmd /C start ""` on
/// Windows. `cmd.exe` parses `&` as a command separator even inside an
/// argument that `std::process::Command` didn't feel the need to quote
/// (it only quotes for embedded whitespace/quotes) — so a URL like
/// `https://…/authorize?client_id=…&state=…` gets truncated at the first
/// `&`, silently dropping the rest of the query string (breaking OAuth
/// callbacks that carry `state`/`code` after it). `^` is `cmd`'s own escape
/// character; caret-escaping `&` makes `cmd` treat it as a literal character
/// instead of an operator. Pure string transform — kept testable off Windows.
#[cfg(any(test, target_os = "windows"))]
fn windows_escape_for_cmd_start(s: &str) -> String {
    s.replace('&', "^&")
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
        /// Lines posted by spawned tasks (`line_poster`), captured so async
        /// command output (export results, /doctor reports) is assertable.
        async_log: Arc<std::sync::Mutex<Vec<String>>>,
        busy: bool,
        model: hrdr_agent::ModelRef,
        input: String,
        /// `/compact` runs started (idle dispatch), in order.
        started_compactions: Vec<Option<String>>,
        /// `/compact` requests queued (busy dispatch), in order.
        queued_compactions: Vec<Option<String>>,
        /// Session prompt-cache figures, as `session_cache` reports them.
        cache: Option<(f64, usize, usize)>,
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
                async_log: Arc::new(std::sync::Mutex::new(Vec::new())),
                busy: false,
                model: "local://test-model".parse().unwrap(),
                input: String::new(),
                started_compactions: Vec::new(),
                queued_compactions: Vec::new(),
                cache: None,
            }
        }
    }

    impl CommandHost for TestHost {
        fn info(&mut self, line: String) {
            self.info_log.push(line);
        }
        fn session_cache(&self) -> Option<(f64, usize, usize)> {
            self.cache
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
        fn clear_conversation(&mut self) {}
        fn session_id(&self) -> Option<String> {
            None
        }
        fn set_session_label(&mut self, _name: String) {}
        fn autosave(&mut self) {}
        fn resume(&mut self, _id: String, _session: Session) {}
        fn line_poster(&self) -> Box<dyn Fn(LineKind, String) + Send> {
            let log = self.async_log.clone();
            Box::new(move |_, line| {
                log.lock().unwrap().push(line);
            })
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
        fn start_compaction(&mut self, instructions: Option<String>) {
            self.started_compactions.push(instructions);
        }
        fn queue_compaction(&mut self, instructions: Option<String>) {
            self.queued_compactions.push(instructions);
        }
    }

    /// `/add` applies the same attach-size cap as `@file` mentions
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

    /// `/cwd` into a directory nobody has answered for is refused, and the
    /// session stays where it was.
    ///
    /// The gate that asks runs once, in `main`, before the first agent — so
    /// without this a session that answered for one directory could walk into a
    /// fresh checkout and read its `AGENTS.md` with the tool set the first
    /// directory earned.
    ///
    /// No environment juggling: the test-support ctor already points this
    /// process's `$XDG_CACHE_HOME` at a throwaway, so the store starts empty and
    /// `trust()` below writes into that copy, not the developer's.
    #[tokio::test]
    async fn cwd_refuses_a_directory_that_was_never_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("here");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let mut host = TestHost::new(here.clone());
        assert!(dispatch(
            &mut host,
            &format!("/cwd {}", elsewhere.display())
        ));

        assert_eq!(host.cwd, here, "the session must not have moved");
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("not a trusted directory")),
            "and it must say why: {:?}",
            host.info_log
        );
    }

    /// A directory that *was* answered for is entered normally — the check is a
    /// gate, not a ban on `/cwd`.
    #[tokio::test]
    async fn cwd_enters_a_trusted_directory() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("here");
        let target = dir.path().join("target");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        hrdr_agent::trust::trust(&target).expect("record the answer");

        let mut host = TestHost::new(here.clone());
        assert!(dispatch(&mut host, &format!("/cwd {}", target.display())));

        assert!(
            host.info_log.iter().any(|l| l.contains("cwd →")),
            "a trusted directory is entered: {:?}",
            host.info_log
        );
        assert!(
            !host.info_log.iter().any(|l| l.contains("not a trusted")),
            "and is not refused: {:?}",
            host.info_log
        );
    }

    /// `/add` attaches files outside the working directory (full-access default):
    /// a `..` escape and an absolute path both go through. Only secret/credential
    /// `cmd.exe` treats `&` as a command separator even when the argument
    /// itself isn't shell-quoted, so an OAuth URL's query string
    /// (`...&state=...`) would get truncated by `cmd /C start "" <url>` on
    /// Windows. Caret-escaping `&` makes `cmd` treat it as a literal
    /// character instead of an operator, so `start` receives the whole URL.
    #[test]
    fn windows_cmd_start_escapes_ampersand_in_oauth_url() {
        let url = "https://example.com/authorize?client_id=abc&state=xyz&scope=read+write";
        assert_eq!(
            windows_escape_for_cmd_start(url),
            "https://example.com/authorize?client_id=abc^&state=xyz^&scope=read+write"
        );
    }

    /// A URL/path with no `&` is passed through unchanged.
    #[test]
    fn windows_cmd_start_leaves_url_without_ampersand_untouched() {
        let url = "https://example.com/callback?code=abc123";
        assert_eq!(windows_escape_for_cmd_start(url), url);
    }

    /// `/cost` reports what the prompt cache did across the session — the
    /// figure that says whether caching is working, as opposed to a single
    /// call's fraction, which moves for reasons that have nothing to do with
    /// the cache (a compaction shrink stage rewrites the prompt; a retry warms
    /// it).
    ///
    /// And it says nothing at all when the endpoint reported nothing. A 0%
    /// there would read as "the cache stopped working", which is the one
    /// conclusion the line exists to support and would be wrong.
    #[test]
    fn cost_reports_the_sessions_prompt_cache_rate() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/cost"));
        let silent = host.info_log.last().cloned().unwrap_or_default();
        assert!(silent.contains("session tokens:"), "{silent}");
        assert!(
            !silent.contains("prompt cache"),
            "an endpoint that reported nothing must not read as a rate: {silent}"
        );

        host.cache = Some((0.78, 120_000, 30_000));
        assert!(dispatch(&mut host, "/cost"));
        let measured = host.info_log.last().cloned().unwrap_or_default();
        assert!(
            measured.contains("prompt cache: 78% read (120.0k), 30.0k written"),
            "{measured}"
        );
    }

    // ── /temp, /export, /effort, /doctor, /login, /skills ──────────────────

    /// The tests that read or write the shared sandboxed `config.toml`
    /// (dispatch's `persist_setting`/`unpersist_setting` target the
    /// process-wide sandbox root) are serialized against each other: two of
    /// them mutating `temperature` / `effort` concurrently would race on the
    /// one file.
    static CONFIG_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// The sandboxed config file `persist_setting` writes, as text ("" when
    /// nothing has been written yet).
    fn config_contents() -> String {
        let path = hrdr_agent::config_file_path().expect("the sandbox ctor set HOME");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Yield until `cond` holds or ~1s passes, letting spawned-task effects
    /// (agent mutations, posted lines, file writes) land before asserting.
    async fn settle(cond: impl Fn() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Seed the host's agent with a user/assistant exchange so the export
    /// formats have something to render.
    async fn seed_messages(host: &TestHost) {
        let mut a = host.agent.lock().await;
        a.set_messages(vec![
            hrdr_agent::Message::user("hello"),
            hrdr_agent::Message::assistant("hi there"),
        ]);
    }

    /// `/temp` accepts only finite values in `0.0..=2.0`. A NaN, an infinity,
    /// an out-of-range number, or a parse error is refused with the usage line
    /// — and nothing lands in the agent or the config file.
    #[tokio::test]
    async fn temp_rejects_invalid_values_without_persisting() {
        let _guard = CONFIG_LOCK.lock().await;
        hrdr_agent::remove_setting("temperature").expect("clean slate");
        for bad in ["nan", "inf", "5", "-1", "1e40"] {
            let dir = tempfile::tempdir().unwrap();
            let mut host = TestHost::new(dir.path().to_path_buf());
            assert!(dispatch(&mut host, &format!("/temp {bad}")), "{bad}");
            assert!(
                host.info_log
                    .iter()
                    .any(|l| l.contains("usage: /temp <0-2> | default")),
                "/temp {bad}: {:?}",
                host.info_log
            );
            assert_eq!(
                host.agent.lock().await.temperature(),
                None,
                "/temp {bad} must not set the temperature"
            );
        }
        assert!(
            !config_contents().contains("temperature"),
            "no temperature key may land in the config: {:?}",
            config_contents()
        );
    }

    /// `/temp 0.7` applies the value to the agent (in the spawned task — yield
    /// first) and persists it to the config file.
    #[tokio::test]
    async fn temp_valid_value_persists_and_applies() {
        let _guard = CONFIG_LOCK.lock().await;
        hrdr_agent::remove_setting("temperature").expect("clean slate");
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/temp 0.7"));
        settle(|| {
            host.agent
                .try_lock()
                .is_ok_and(|a| a.temperature() == Some(0.7))
        })
        .await;
        assert_eq!(host.agent.lock().await.temperature(), Some(0.7));
        // `persist_setting` stores `t as f64` for an `f32` input, so the value
        // on disk is the widened f32 representation, not the decimal literal.
        let cfg = toml::from_str::<toml::Value>(&config_contents()).expect("valid TOML");
        assert_eq!(
            cfg.get("temperature").and_then(|v| v.as_float()),
            Some(0.7f32 as f64),
            "{:?}",
            config_contents()
        );
        hrdr_agent::remove_setting("temperature").expect("cleanup");
    }

    /// `/temp default` clears a set value: the agent goes back to `None` and
    /// the `temperature` key leaves the config file.
    #[tokio::test]
    async fn temp_default_clears_the_override() {
        let _guard = CONFIG_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        // Set a value first so the clear has something to undo.
        assert!(dispatch(&mut host, "/temp 0.5"));
        settle(|| {
            host.agent
                .try_lock()
                .is_ok_and(|a| a.temperature() == Some(0.5))
        })
        .await;

        assert!(dispatch(&mut host, "/temp default"));
        settle(|| {
            host.agent
                .try_lock()
                .is_ok_and(|a| a.temperature().is_none())
        })
        .await;
        assert!(
            !config_contents().contains("temperature"),
            "the key must be removed: {:?}",
            config_contents()
        );
    }

    /// A file named `*.json` is exported as JSON even without the flag.
    #[tokio::test]
    async fn export_writes_json_when_the_file_is_named_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        seed_messages(&host).await;

        assert!(dispatch(&mut host, "/export out.json"));
        settle(|| dir.path().join("out.json").exists()).await;
        let text = std::fs::read_to_string(dir.path().join("out.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(v.is_array(), "expected a JSON array, got: {text}");
    }

    /// A plain name is exported as Markdown.
    #[tokio::test]
    async fn export_writes_markdown_without_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        seed_messages(&host).await;

        assert!(dispatch(&mut host, "/export out.md"));
        settle(|| dir.path().join("out.md").exists()).await;
        let text = std::fs::read_to_string(dir.path().join("out.md")).unwrap();
        assert!(
            text.starts_with("## User"),
            "expected markdown, got: {text}"
        );
    }

    /// `--json` forces JSON even for a file with no extension.
    #[tokio::test]
    async fn export_flag_forces_json_even_without_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        seed_messages(&host).await;

        assert!(dispatch(&mut host, "/export --json out"));
        settle(|| dir.path().join("out").exists()).await;
        let text = std::fs::read_to_string(dir.path().join("out")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(v.is_array(), "expected a JSON array, got: {text}");
    }

    /// An existing file is refused, never overwritten, and the error names it.
    #[tokio::test]
    async fn export_refuses_to_overwrite_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        seed_messages(&host).await;
        std::fs::write(dir.path().join("out.md"), "original").unwrap();

        assert!(dispatch(&mut host, "/export out.md"));
        settle(|| !host.async_log.lock().unwrap().is_empty()).await;
        assert!(
            host.async_log
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("refusing to overwrite existing file")),
            "{:?}",
            host.async_log.lock().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.md")).unwrap(),
            "original",
            "the existing file must be left untouched"
        );
    }

    /// A second filename is refused with the usage line, and nothing is
    /// written.
    #[tokio::test]
    async fn export_rejects_extra_tokens_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        seed_messages(&host).await;

        assert!(dispatch(&mut host, "/export a.md b.md"));
        settle(|| !host.async_log.lock().unwrap().is_empty()).await;
        assert!(
            host.async_log
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("usage: /export [--json] <file>")),
            "{:?}",
            host.async_log.lock().unwrap()
        );
        assert!(!dir.path().join("a.md").exists(), "nothing may be written");
    }

    /// `/effort high` applies the level by value (the picker's own match rule)
    /// and persists it as the config default.
    #[tokio::test]
    async fn effort_applies_a_valid_value() {
        let _guard = CONFIG_LOCK.lock().await;
        hrdr_agent::remove_setting("effort").expect("clean slate");
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/effort high"));
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("effort → High (high)")),
            "{:?}",
            host.info_log
        );
        assert!(
            config_contents().contains("effort = \"high\""),
            "{:?}",
            config_contents()
        );
        hrdr_agent::remove_setting("effort").expect("cleanup");
    }

    /// `/effort default` clears the override: the `effort` key leaves the
    /// config file.
    #[tokio::test]
    async fn effort_default_clears_the_override() {
        let _guard = CONFIG_LOCK.lock().await;
        hrdr_agent::remove_setting("effort").expect("clean slate");
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/effort default"));
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("effort → default (the model/provider default)")),
            "{:?}",
            host.info_log
        );
        assert!(
            !config_contents().contains("effort"),
            "{:?}",
            config_contents()
        );
    }

    /// An unknown level is refused with a list of what IS available (the
    /// FALLBACK ladder for the local test model), and nothing is persisted.
    #[tokio::test]
    async fn effort_unknown_value_lists_the_available_levels() {
        let _guard = CONFIG_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/effort zzz"));
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("unknown effort 'zzz'")),
            "{:?}",
            host.info_log
        );
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("available: Default, High, Medium, Low, Minimal")),
            "{:?}",
            host.info_log
        );
        assert!(
            !config_contents().contains("effort"),
            "{:?}",
            config_contents()
        );
    }

    /// `/doctor` keeps the filesystem probes (git, auth file) off the UI
    /// thread: the synchronous header is pure, and the spawned report carries
    /// the git/auth lines.
    #[tokio::test]
    async fn doctor_reports_not_a_repo_from_the_spawned_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/doctor"));
        // The synchronous header must not do the git walk itself.
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("probing endpoint…")),
            "{:?}",
            host.info_log
        );
        assert!(
            !host.info_log.iter().any(|l| l.contains("branch:")),
            "the branch belongs to the spawned report, not the header: {:?}",
            host.info_log
        );
        settle(|| !host.async_log.lock().unwrap().is_empty()).await;
        let joined = host.async_log.lock().unwrap().join("\n");
        assert!(joined.contains("git: not a repo"), "{joined}");
        assert!(joined.contains("auth:"), "{joined}");
    }

    /// Inside a real git repo the spawned report names the branch.
    #[tokio::test]
    async fn doctor_reports_the_branch_inside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git must be available to run this test");
        assert!(out.status.success(), "{out:?}");
        let mut host = TestHost::new(dir.path().to_path_buf());

        assert!(dispatch(&mut host, "/doctor"));
        settle(|| !host.async_log.lock().unwrap().is_empty()).await;
        let joined = host.async_log.lock().unwrap().join("\n");
        assert!(joined.contains("git: on branch"), "{joined}");
    }

    /// `/login <arg>` says the argument is unused but still opens the wizard.
    #[tokio::test]
    async fn login_with_an_argument_still_opens_the_wizard() {
        let mut host = TestHost::new(std::env::temp_dir());
        assert!(dispatch(&mut host, "/login foo"));
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("/login takes no argument")),
            "{:?}",
            host.info_log
        );
        // The wizard still opened (the default host reports it unavailable).
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("/login isn't available")),
            "{:?}",
            host.info_log
        );
    }

    /// `/skills <arg>` says the argument is unused but still opens the picker.
    #[tokio::test]
    async fn skills_with_an_argument_still_opens_the_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        assert!(dispatch(&mut host, "/skills foo"));
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("/skills takes no argument")),
            "{:?}",
            host.info_log
        );
        // The picker still opened (the default host lists the skills as text).
        assert!(
            host.info_log
                .iter()
                .any(|l| l.contains("skills (invoke with :name")),
            "{:?}",
            host.info_log
        );
    }

    /// `/compact` with an idle agent starts a compaction, carrying the message
    /// after the command as the summary-steering instructions.
    #[tokio::test]
    async fn compact_idle_starts_with_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        assert!(dispatch(&mut host, "/compact keep the file paths"));
        assert_eq!(
            host.started_compactions,
            vec![Some("keep the file paths".to_string())]
        );
        assert!(host.queued_compactions.is_empty());
        assert!(
            host.info_log.is_empty(),
            "no busy refusal when idle: {:?}",
            host.info_log
        );
    }

    /// `/compact` while the agent is mid-turn is QUEUED, not refused and not
    /// started — it runs after the turn ends, so it never reaches the model
    /// like a steer.
    #[tokio::test]
    async fn compact_while_busy_is_queued_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        host.busy = true;
        assert!(dispatch(&mut host, "/compact drop the build details"));
        assert_eq!(
            host.queued_compactions,
            vec![Some("drop the build details".to_string())]
        );
        assert!(host.started_compactions.is_empty());
        assert!(
            host.info_log.iter().all(|l| !l.contains("busy")),
            "no busy-guard refusal — the request queues: {:?}",
            host.info_log
        );
    }

    /// A bare `/compact` with no message carries `None` instructions either way.
    #[tokio::test]
    async fn compact_without_a_message_carries_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = TestHost::new(dir.path().to_path_buf());
        assert!(dispatch(&mut host, "/compact"));
        assert_eq!(host.started_compactions, vec![None]);
        assert!(host.queued_compactions.is_empty());
    }
}
