//! Session listing + auto-save shared by hrdr's frontends. The listing text
//! (the `/resume` picker's text fallback) and the auto-save policy (when to
//! write, how the file id is assigned) are representation-independent, so a
//! frontend's session listing + continuous auto-save build them from here.
//! Resuming itself is per-frontend (it swaps in the saved
//! [`crate::SessionState`]), so that stays in the frontends.

use crate::{Session, SessionState};

/// The result of an auto-save: the session's file id, and whether this call was
/// the one that first assigned it (so the frontend can notify once).
pub struct SaveOutcome {
    pub id: String,
    pub first_save: bool,
}

/// Persist a conversation as a session. Returns `Ok(None)` when there's nothing
/// worth saving yet (no user message); filesystem failures are returned so a
/// frontend never claims an unsaved conversation is durable.
///
/// `state` is the frontend's whole session state, written as-is apart from the
/// ephemeral session-chrome notices ([`SessionState::persisted`]). The file id
/// comes from `state.id` when the session already has one, otherwise a fresh
/// collision-free id is derived from its name (see [`crate::unique_session_id`])
/// and reported back as `first_save`.
pub fn save_session(state: &SessionState) -> anyhow::Result<Option<SaveOutcome>> {
    if !state.is_saveable() {
        return Ok(None);
    }
    let (id, first_save, _reservation) = if let Some(id) = &state.id {
        (id.clone(), false, None)
    } else {
        let (id, res) = crate::unique_session_id(&state.cwd, &state.name);
        (id, true, Some(res))
    };
    Session::new(state.persisted()).save(&id)?;
    // `_reservation` is dropped here. If `save` failed above, the drop
    // removes the lock file that `unique_session_id` created — no stale
    // lock is left behind. If `save` succeeded, `save()` already removed
    // the lock; the second `remove_file` in `Reservation::drop` is benign.
    Ok(Some(SaveOutcome { id, first_save }))
}

/// Async wrapper over [`save_session`]: refresh the state's mirrors of the
/// agent-owned data (chat messages, TODOs, cwd) under the lock, then persist.
/// The shared core for every auto-save that runs off the UI thread.
pub async fn save_agent_session(
    agent: std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>>,
    mut state: SessionState,
) -> anyhow::Result<Option<SaveOutcome>> {
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

/// Every saved session as a display string, newest first, each row tagged with
/// its cwd — the text fallback for frontends without the `/resume` picker
/// modal. Returns a friendly empty-state message when there are none.
///
/// Corrupt/unreadable sessions are shown with an `[error]` tag in place of
/// the name and cwd, so they are visible rather than silently skipped.
pub fn session_list_text() -> String {
    let sessions = crate::list_sessions();
    if sessions.is_empty() {
        return format!("no saved sessions in {}", crate::sessions_dir().display());
    }
    let mut s = String::from("saved sessions (newest first; resume by id or name):");
    let mut corrupt = 0;
    for m in sessions {
        if let Some(err) = &m.error {
            corrupt += 1;
            s.push_str(&format!("\n  {} — [unreadable: {err}]", m.id));
        } else {
            s.push_str(&format!("\n  {} — {}  [{}]", m.id, m.name, m.cwd));
        }
    }
    if corrupt > 0 {
        s.push_str(&format!(
            "\n{corrupt} corrupt/unreadable session file(s) — /doctor for details"
        ));
    }
    s
}

/// Return diagnostic information about every corrupt/unreadable session file
/// found in the sessions directory. Used by `/doctor` to report session health.
pub fn session_diagnostics() -> Vec<(String, String)> {
    crate::list_sessions()
        .into_iter()
        .filter_map(|m| m.error.map(|err| (m.path.display().to_string(), err)))
        .collect()
}

/// Case-insensitive fuzzy filter over session rows for the `/resume` picker:
/// the query's characters must appear in order somewhere within
/// `"id name cwd"`. Returns matching indices in input order (callers pass
/// [`crate::list_sessions`]'s newest-first list); an empty query matches
/// everything.
pub fn filter_sessions(sessions: &[crate::SessionMeta], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..sessions.len()).collect();
    }
    sessions
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let hay = format!("{} {} {}", m.id, m.name, m.cwd).to_lowercase();
            crate::is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// System/assistant-only histories aren't worth persisting → no id, and
    /// (importantly) no file is written.
    #[test]
    fn save_session_skips_conversations_with_no_user_message() {
        assert!(save_session(&SessionState::default()).unwrap().is_none());
        let assistant_only = SessionState {
            messages: vec![hrdr_agent::Message::assistant("hi there")],
            ..Default::default()
        };
        assert!(save_session(&assistant_only).unwrap().is_none());
    }

    #[test]
    fn filter_sessions_matches_id_name_and_cwd() {
        let meta = |id: &str, name: &str, cwd: &str| crate::SessionMeta {
            id: id.to_string(),
            name: name.to_string(),
            cwd: cwd.to_string(),
            updated: 0,
            path: std::path::PathBuf::new(),
            error: None,
        };
        let sessions = vec![
            meta("fix-auth", "Fix the auth bug", "/home/u/proj-a"),
            meta("tui-work", "TUI polish", "/home/u/proj-b"),
        ];
        // Empty query keeps everything in input (newest-first) order.
        assert_eq!(filter_sessions(&sessions, ""), vec![0, 1]);
        // Matches on id, name (case-insensitive), and cwd.
        assert_eq!(filter_sessions(&sessions, "auth"), vec![0]);
        assert_eq!(filter_sessions(&sessions, "POLISH"), vec![1]);
        assert_eq!(filter_sessions(&sessions, "proj-b"), vec![1]);
        // Fuzzy subsequence across the combined text.
        assert_eq!(filter_sessions(&sessions, "fx bug"), vec![0]);
        assert!(filter_sessions(&sessions, "zzz").is_empty());
    }
}
