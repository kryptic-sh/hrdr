//! Session listing + auto-save shared by hrdr's frontends. The listing text
//! (which sessions to show for the current cwd, or all, and how to render each
//! row) and the auto-save policy (when to write, how the file id is assigned)
//! are representation-independent, so both the TUI's `/sessions` + continuous
//! auto-save and the GUI's build them identically. Resuming itself is
//! per-frontend (it rebuilds each one's own transcript representation), so that
//! stays in the frontends.

use hrdr_agent::{Message, MessageRole};

/// The result of an auto-save: the session's file id, and whether this call was
/// the one that first assigned it (so the frontend can notify once).
pub struct SaveOutcome {
    pub id: String,
    pub first_save: bool,
}

/// Persist a conversation as a session (best-effort; filesystem errors are
/// swallowed). Returns `None` when there's nothing worth saving yet (no user
/// message). Otherwise it builds and writes the session under `cwd`, assigning a
/// stable file id from the name when `existing_id` is `None` (via
/// [`hrdr_agent::unique_session_id`]), and returns that id plus whether it was
/// newly assigned. `label` overrides the auto-derived name (the first user line);
/// pass `None` to use the derived one.
#[allow(clippy::too_many_arguments)]
pub fn save_session(
    existing_id: Option<&str>,
    label: Option<&str>,
    model: &str,
    provider: Option<&str>,
    base_url: &str,
    cwd: &str,
    messages: Vec<Message>,
    todos: Vec<hrdr_tools::TodoItem>,
) -> Option<SaveOutcome> {
    // Non-empty == has at least one user message.
    if !messages.iter().any(|m| m.role == MessageRole::User) {
        return None;
    }
    let name = label
        .map(str::to_string)
        .unwrap_or_else(|| crate::session_name_from(&messages));
    let (id, first_save) = match existing_id {
        Some(id) => (id.to_string(), false),
        None => (hrdr_agent::unique_session_id(cwd, &name), true),
    };
    let _ = hrdr_agent::Session::new(
        &name,
        model,
        provider.map(|s| s.to_string()),
        base_url,
        cwd,
        messages,
        todos,
    )
    .save(&id);
    Some(SaveOutcome { id, first_save })
}

/// Async wrapper over [`save_session`]: snapshot the agent's conversation (and
/// its cwd) under the lock, then persist. The shared core for every auto-save
/// that runs off the UI thread (the GUI's turn-end save and its
/// `CommandHost::autosave`); the TUI's synchronous `try_lock` autosave keeps
/// its own snapshot but funnels into the same [`save_session`].
pub async fn save_agent_session(
    agent: std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>>,
    existing_id: Option<String>,
    label: Option<String>,
    model: String,
    provider: Option<String>,
    base_url: String,
) -> Option<SaveOutcome> {
    let (msgs, cwd, todos) = {
        let a = agent.lock().await;
        let todos = a.todos().lock().map(|t| t.clone()).unwrap_or_default();
        (a.messages_owned(), a.cwd().display().to_string(), todos)
    };
    save_session(
        existing_id.as_deref(),
        label.as_deref(),
        &model,
        provider.as_deref(),
        &base_url,
        &cwd,
        msgs,
        todos,
    )
}

/// The most recent saved session for `cwd` that has actual conversation
/// content (more than just the system prompt) — the startup auto-resume
/// lookup shared by both frontends. `None` = nothing to resume, start fresh.
pub fn latest_session_for_cwd(cwd: &str) -> Option<(String, hrdr_agent::Session)> {
    let cur = hrdr_agent::cwd_slug(cwd);
    let meta = hrdr_agent::list_sessions()
        .into_iter()
        .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)?;
    let session = hrdr_agent::Session::load_path(&meta.path).ok()?;
    (session.messages.len() > 1).then_some((meta.id, session))
}

/// The `/sessions` listing as a display string. With `all`, every directory's
/// sessions are shown (each row tagged with its cwd); otherwise only those whose
/// cwd matches `cwd`. Returns a friendly empty-state message when there are none.
pub fn session_list_text(all: bool, cwd: &str) -> String {
    let cur = hrdr_agent::cwd_slug(cwd);
    let sessions: Vec<_> = hrdr_agent::list_sessions()
        .into_iter()
        .filter(|m| all || hrdr_agent::cwd_slug(&m.cwd) == cur)
        .collect();
    if sessions.is_empty() {
        return if all {
            format!(
                "no saved sessions in {}",
                hrdr_agent::sessions_dir().display()
            )
        } else {
            "no saved sessions for this directory (try /sessions --all)".to_string()
        };
    }
    let mut s = if all {
        String::from("all sessions (resume by id or name):")
    } else {
        String::from("sessions here (resume by id or name; /sessions --all for every dir):")
    };
    for m in sessions {
        if all {
            s.push_str(&format!("\n  {} — {}  [{}]", m.id, m.name, m.cwd));
        } else {
            s.push_str(&format!("\n  {} — {}", m.id, m.name));
        }
    }
    s
}

/// Whether a `/sessions` argument requests every directory's sessions.
pub fn sessions_all_flag(arg: &str) -> bool {
    matches!(arg.trim(), "--all" | "-a" | "all")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_agent::Message;

    #[test]
    fn save_session_skips_conversations_with_no_user_message() {
        // System/assistant-only histories aren't worth persisting → no id, and
        // (importantly) no file is written.
        assert!(save_session(None, None, "m", None, "", "/tmp/x", vec![], vec![]).is_none());
        let assistant_only = vec![Message::assistant("hi there")];
        assert!(
            save_session(None, None, "m", None, "", "/tmp/x", assistant_only, vec![]).is_none()
        );
    }

    #[test]
    fn sessions_all_flag_recognizes_variants() {
        for a in ["--all", "-a", "all", "  all  "] {
            assert!(sessions_all_flag(a), "{a:?}");
        }
        for a in ["", "here", "--foo"] {
            assert!(!sessions_all_flag(a), "{a:?}");
        }
    }
}
