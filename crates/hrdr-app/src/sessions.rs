//! Session listing + auto-save shared by hrdr's frontends. The listing text
//! (the `/resume` picker's text fallback) and the auto-save policy (when to
//! write, how the file id is assigned) are representation-independent, so a
//! frontend's session listing + continuous auto-save build them from here.
//! Resuming itself is per-frontend (it swaps in the saved
//! [`crate::SessionState`]), so that stays in the frontends.

use crate::{SaveOutcome, Session, SessionState, save_session};

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

/// The most recent resumable session for `cwd`, **opened under its open-lock**
/// so the resumed session is owned exclusively — the locked counterpart to
/// [`latest_session_for_cwd`], used by startup auto-resume.
///
/// Selection matches [`latest_session_for_cwd`] (newest for the cwd, more than a
/// bare system prompt). Returns:
/// * `Ok(Some((id, session, lock)))` — resume this and hold the guard;
/// * `Ok(None)` — nothing worth resuming here (no candidate, corrupt newest, or
///   content too thin);
/// * `Err(SessionBusy)` — the newest session is already open in another live
///   instance. Startup auto-resume treats this the same as `None` (start fresh)
///   rather than surfacing a jarring error; only an explicit `/resume` refuses.
pub fn open_latest_session_for_cwd(
    cwd: &str,
) -> Result<Option<(String, Session, crate::SessionLock)>, crate::SessionBusy> {
    let cur = hrdr_agent::cwd_slug(cwd);
    let Some(meta) = crate::list_sessions()
        .into_iter()
        .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)
    else {
        return Ok(None);
    };
    match Session::open_path(&meta.path) {
        Ok((session, lock)) => {
            if session.state.messages.len() > 1 {
                Ok(Some((meta.id, session, lock)))
            } else {
                // Not worth resuming — release the lock we just took.
                drop(lock);
                Ok(None)
            }
        }
        Err(crate::OpenError::Busy { pid, started }) => Err(crate::SessionBusy { pid, started }),
        // A corrupt/unreadable newest session is skipped, exactly as
        // `latest_session_for_cwd`'s `.ok()?` did.
        Err(crate::OpenError::Load(_)) => Ok(None),
    }
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
    use hrdr_agent::Message;
    use std::sync::Mutex;

    /// Global lock so env-var-dependent session tests don't race on HOME / XDG
    /// vars (`std::env::set_var` is not thread-safe in Rust tests). A local copy
    /// of the helper that lived in `session.rs` before its move to hrdr-agent —
    /// duplicated here (rather than shared through a re-export) because a
    /// `#[cfg(test)]` module in one crate is invisible to another crate's tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set XDG_DATA_HOME to an isolated temp dir for the duration of `f`.
    fn with_test_env(f: impl FnOnce(&tempfile::TempDir)) {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", tmp.path());
        }
        f(&tmp);
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    /// A saveable state: one user message, named, rooted at `cwd`.
    fn state(name: &str, cwd: &str) -> SessionState {
        SessionState {
            name: name.to_string(),
            model: "local://model".parse().unwrap(),
            base_url: "http://x/v1".to_string(),
            cwd: cwd.to_string(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        }
    }

    /// `session_diagnostics` returns only the corrupt entries. Relocated here
    /// from `session.rs`'s test module (which moved to hrdr-agent) because it
    /// exercises `session_diagnostics`, which stays in hrdr-app.
    #[test]
    fn session_diagnostics_returns_only_corrupt_files() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();

            Session::new(state("valid", &cwd)).save("valid").unwrap();
            // Derive the session directory from a public path helper (the private
            // `session_dir` used before the move now lives in hrdr-agent).
            let dir = crate::session_file_path(&cwd, "valid")
                .parent()
                .unwrap()
                .to_path_buf();
            std::fs::write(dir.join("broken.json"), "{{{").unwrap();

            let diags = session_diagnostics();
            assert_eq!(diags.len(), 1);
            assert!(diags[0].0.ends_with("broken.json"), "path: {}", diags[0].0);
        });
    }

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
