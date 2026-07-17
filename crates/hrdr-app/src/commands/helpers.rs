use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::SessionState;
use hrdr_agent::Agent;
use tokio::sync::Mutex;

pub fn busy_guard(action: &str) -> String {
    format!("can't {action} while a turn is running")
}

pub fn busy_generic() -> String {
    "busy — try again after the current turn".to_string()
}

/// `/expand` status lines (both frontends show the same wording).
pub mod expand_msg {
    pub const ALL: &str = "tool output expanded (all)";
    pub const OFF: &str = "tool output collapsed";
    pub const LAST_COLLAPSED: &str = "collapsed last tool output";
    pub const LAST_EXPANDED: &str = "expanded last tool output";
    pub const NONE: &str = "no tool output to expand";
}

/// `/reload` + hot-reload status lines (both frontends).
pub const RELOAD_MANUAL_MSG: &str = "reloaded config (theme, effort, toggles)";

/// Hot-reload notice, naming the config file that changed (home collapsed to
/// `~`). Falls back to the bare notice when there's no resolvable config path
/// (no `HOME` / `XDG_CONFIG_HOME`).
pub fn reload_hot_message() -> String {
    match hrdr_agent::config_file_path() {
        Some(p) => format!("config reloaded ({})", crate::display_dir(&p)),
        None => "config reloaded".to_string(),
    }
}
/// Invalid config file on reload: keep the current settings and warn.
pub fn reload_invalid_message(e: &dyn std::fmt::Display) -> String {
    format!("config invalid — keeping current settings: {e}")
}

/// Startup notice when `AGENTS.md` was gathered into the system prompt.
pub const PROJECT_DOCS_LOADED_MSG: &str = "loaded project instructions from AGENTS.md";

/// Shown by `/new` when the `AGENTS.md` it just re-read differs from the one that
/// was in the prompt — the only point at which project docs are re-seeded.
///
/// A running conversation is never re-seeded: the agent that edited the file has
/// the change in its context already, and another session that wants it starts a
/// new conversation.
pub const PROJECT_DOCS_RELOADED_MSG: &str =
    "AGENTS.md changed on disk — reloaded into the system prompt";

/// Startup notice for non-fatal config problems the TUI should surface: the
/// agent-side env-override warnings and the UI-side enum warnings, combined into
/// one block (each dropped-and-defaulted value on its own line). `None` when the
/// config is clean.
///
/// Hard config errors do NOT come through here — `main` prints and exits on
/// those before any frontend starts (see `hrdr_agent::ConfigDiagnostics`), so by
/// the time the TUI is drawing, only warnings remain.
pub fn startup_config_warning() -> Option<String> {
    let (_, agent) = hrdr_agent::AgentConfig::load_diagnosed();
    let (_, ui_warnings) = crate::UiConfig::load_diagnosed();
    let mut lines: Vec<String> = agent.warnings;
    lines.extend(ui_warnings);
    if lines.is_empty() {
        return None;
    }
    Some(format!("configuration warnings:\n  {}", lines.join("\n  ")))
}

/// Guard shown when `/resume` is attempted mid-turn (the running turn holds
/// the agent mutex: the message swap would silently no-op while the transcript
/// and session id switched, and the turn's autosave would then overwrite the
/// resumed session's file with the old conversation).
pub const RESUME_BUSY_MSG: &str = "a turn is running — interrupt it before /resume";

/// What restoring a session changes beyond the host's own state swap: the
/// working directory to adopt (if any) and the system lines to show, in order.
pub struct ResumePlan {
    /// The session's cwd when it exists and differs from the current one.
    pub new_cwd: Option<PathBuf>,
    /// Notices: the "resumed …" line, then cwd / missing-cwd / endpoint notes.
    pub lines: Vec<String>,
}

/// The shared `/resume` semantics both frontends apply: follow the session's
/// working directory (in-process only) and surface the same notices.
pub fn resume_plan(session: &SessionState, prev_cwd: &Path, current_base_url: &str) -> ResumePlan {
    let mut lines = vec![format!(
        "resumed '{}' ({} messages)",
        session.name,
        session.messages.len()
    )];
    let mut new_cwd = None;
    if !session.cwd.is_empty() && Path::new(&session.cwd) != prev_cwd {
        let target = PathBuf::from(&session.cwd);
        if target.is_dir() {
            lines.push(format!("cwd → {}", target.display()));
            new_cwd = Some(target);
        } else {
            lines.push(format!(
                "note: session cwd {} no longer exists; staying in {}",
                session.cwd,
                prev_cwd.display()
            ));
        }
    }
    if session.base_url != current_base_url {
        lines.push(format!(
            "note: session endpoint was {} (current: {current_base_url})",
            session.base_url
        ));
    }
    ResumePlan { new_cwd, lines }
}

/// Minimum turn duration before the finish nudge fires (the TUI's terminal
/// bell) — quick replies stay silent.
pub const BELL_MIN_SECS: f64 = 5.0;

/// Whether a finished turn warrants the nudge: the knob is on and the turn ran
/// at least [`BELL_MIN_SECS`].
pub fn should_bell(enabled: bool, elapsed_secs: Option<f64>) -> bool {
    enabled && elapsed_secs.is_some_and(|e| e >= BELL_MIN_SECS)
}

/// The cancel notice both frontends show (with the discarded-queue count).
pub fn cancel_message(dropped: usize) -> String {
    if dropped > 0 {
        format!("[cancelled · {dropped} queued message(s) discarded]")
    } else {
        "[cancelled]".to_string()
    }
}

/// The one-time notice when a session file is first created.
pub fn session_saved_notice(id: &str) -> String {
    format!("session saved as '{id}' — /resume {id}")
}

/// Copy `text` to the OS clipboard, returning the status line both frontends
/// show. `cb` is the frontend's long-lived clipboard handle (`None` when the
/// platform has none).
pub fn clipboard_copy_status(
    cb: &mut Option<hjkl_clipboard::Clipboard>,
    text: &str,
    label: &str,
) -> String {
    use hjkl_clipboard::{MimeType, Selection};
    match cb
        .as_mut()
        .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()))
    {
        Some(Ok(())) => format!("copied {label} to clipboard"),
        Some(Err(_)) => "clipboard write failed".to_string(),
        None => "clipboard unavailable".to_string(),
    }
}

/// Read the OS clipboard as text (`/paste`).
pub fn clipboard_read_text(cb: &Option<hjkl_clipboard::Clipboard>) -> Option<String> {
    use hjkl_clipboard::{MimeType, Selection};
    let bytes = cb
        .as_ref()
        .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok())?;
    Some(String::from_utf8_lossy(&bytes).to_string())
}

/// The tools' working directory: the agent's cwd when the lock is free
/// (a turn may hold it), else the process cwd.
pub fn agent_cwd(agent: &Arc<Mutex<Agent>>) -> PathBuf {
    agent
        .try_lock()
        .map(|a| a.cwd())
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// The names of sub-agents the live agent can delegate to (for `@name` mention
/// routing). Empty when the lock is held (a turn is running) or delegation is off.
pub fn agent_names(agent: &Arc<Mutex<Agent>>) -> Vec<String> {
    agent
        .try_lock()
        .map(|a| a.agent_names().to_vec())
        .unwrap_or_default()
}

/// [`crate::prepare_outgoing`] for frontends holding the shared agent handle:
/// fetches the sub-agent names ([`agent_names`]) and cwd ([`agent_cwd`]) itself.
pub fn prepare_outgoing_via(agent: &Arc<Mutex<Agent>>, input: &str) -> String {
    crate::prepare_outgoing(input, &agent_names(agent), &agent_cwd(agent))
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

#[cfg(test)]
mod reload_message_tests {
    /// The hot-reload notice names the config file that changed. The path is
    /// whatever `config_file_path()` resolves to (home collapsed to `~`), so
    /// assert on its shape rather than an absolute path.
    #[test]
    fn hot_reload_notice_names_the_config_file() {
        let msg = super::reload_hot_message();
        assert!(msg.starts_with("config reloaded"), "{msg}");
        // With no HOME/XDG the path is unresolvable and the bare notice is used.
        if hrdr_agent::config_file_path().is_some() {
            assert!(msg.contains("config.toml"), "{msg}");
            assert!(msg.ends_with(')'), "{msg}");
        }
    }
}
