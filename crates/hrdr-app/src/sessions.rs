//! Session listing + auto-save shared by hrdr's frontends. The listing text
//! (which sessions to show for the current cwd, or all, and how to render each
//! row) and the auto-save policy (when to write, how the file id is assigned)
//! are representation-independent, so both the TUI's `/sessions` + continuous
//! auto-save and the GUI's build them identically. Resuming itself is
//! per-frontend (it rebuilds each one's own transcript representation), so that
//! stays in the frontends.

use crate::{Session, SessionState};

/// The result of an auto-save: the session's file id, and whether this call was
/// the one that first assigned it (so the frontend can notify once).
pub struct SaveOutcome {
    pub id: String,
    pub first_save: bool,
}

/// Persist a conversation as a session (best-effort; filesystem errors are
/// swallowed). Returns `None` when there's nothing worth saving yet (no user
/// message).
///
/// `state` is the frontend's whole session state; it is written verbatim. The
/// file id comes from `state.id` when the session already has one, otherwise a
/// fresh collision-free id is derived from its name (see
/// [`crate::unique_session_id`]) and reported back as `first_save`.
pub fn save_session(state: &SessionState) -> Option<SaveOutcome> {
    if !state.is_saveable() {
        return None;
    }
    let (id, first_save) = match &state.id {
        Some(id) => (id.clone(), false),
        None => (crate::unique_session_id(&state.cwd, &state.name), true),
    };
    let _ = Session::new(state.clone()).save(&id);
    Some(SaveOutcome { id, first_save })
}

/// Async wrapper over [`save_session`]: refresh the state's mirrors of the
/// agent-owned data (chat messages, TODOs, cwd) under the lock, then persist.
/// The shared core for every auto-save that runs off the UI thread.
pub async fn save_agent_session(
    agent: std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>>,
    mut state: SessionState,
) -> Option<SaveOutcome> {
    let (msgs, cwd, todos) = {
        let a = agent.lock().await;
        let todos = a.todos().lock().map(|t| t.clone()).unwrap_or_default();
        (a.messages_owned(), a.cwd().display().to_string(), todos)
    };
    state.sync_from(msgs, todos, cwd);
    save_session(&state)
}

/// The most recent saved session for `cwd` that has actual conversation
/// content (more than just the system prompt) — the startup auto-resume
/// lookup shared by both frontends. `None` = nothing to resume, start fresh.
pub fn latest_session_for_cwd(cwd: &str) -> Option<(String, Session)> {
    let cur = hrdr_agent::cwd_slug(cwd);
    let meta = crate::list_sessions()
        .into_iter()
        .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)?;
    let session = Session::load_path(&meta.path).ok()?;
    (session.state.messages.len() > 1).then_some((meta.id, session))
}

/// The `/sessions` listing as a display string. With `all`, every directory's
/// sessions are shown (each row tagged with its cwd); otherwise only those whose
/// cwd matches `cwd`. Returns a friendly empty-state message when there are none.
pub fn session_list_text(all: bool, cwd: &str) -> String {
    let cur = hrdr_agent::cwd_slug(cwd);
    let sessions: Vec<_> = crate::list_sessions()
        .into_iter()
        .filter(|m| all || hrdr_agent::cwd_slug(&m.cwd) == cur)
        .collect();
    if sessions.is_empty() {
        return if all {
            format!("no saved sessions in {}", crate::sessions_dir().display())
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

    /// System/assistant-only histories aren't worth persisting → no id, and
    /// (importantly) no file is written.
    #[test]
    fn save_session_skips_conversations_with_no_user_message() {
        assert!(save_session(&SessionState::default()).is_none());
        let assistant_only = SessionState {
            messages: vec![hrdr_agent::Message::assistant("hi there")],
            ..Default::default()
        };
        assert!(save_session(&assistant_only).is_none());
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
