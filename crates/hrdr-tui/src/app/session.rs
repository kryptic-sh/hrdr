//! Session persistence, restore, and transcript rebuild.

use hrdr_agent::Message;

use super::*;

impl super::App {
    /// On startup, resume the most recent saved session for the current
    /// directory (if any). No match → leave the fresh session as-is.
    pub(super) fn auto_resume_latest(&mut self) {
        let cwd = self.current_cwd();
        // Shared lookup (skips empty/system-prompt-only sessions).
        let Some((id, session)) = hrdr_app::latest_session_for_cwd(&cwd) else {
            return; // nothing saved here yet — start fresh
        };
        self.with_agent(|a| {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        });
        self.model = session.model.clone();
        self.provider = session.provider.clone();
        self.rebuild_transcript(&session.messages);
        // Restore saved TODOs.
        if let Ok(mut todos) = self.todos.lock() {
            *todos = session.todos.clone();
        }
        self.session_id = Some(id);
        self.session_label = Some(session.name.clone());
        self.push_entry(Entry::System(format!(
            "resumed most recent session '{}' ({} messages) — /clear to start fresh",
            session.name,
            session.messages.len()
        )));
    }
    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, and `/rename`.
    pub(super) fn autosave(&mut self) {
        let snap = self
            .agent
            .try_lock()
            .ok()
            .map(|a| (a.messages_owned(), a.cwd()));
        let Some((msgs, cwd)) = snap else {
            return;
        };
        // Snapshot TODOs from the shared list.
        let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
        let outcome = hrdr_app::save_session(
            self.session_id.as_deref(),
            self.session_label.as_deref(),
            &self.model,
            self.provider.as_deref(),
            &self.base_url,
            &cwd.display().to_string(),
            msgs,
            todos,
        );
        if let Some(o) = outcome {
            // Notify once, when the session is first created.
            if o.first_save {
                self.push_entry(Entry::System(hrdr_app::session_saved_notice(&o.id)));
            }
            self.session_id = Some(o.id);
        }
    }
    /// Restore a resolved session (the shared `/resume` command calls this via
    /// [`hrdr_app::CommandHost::resume`]): swap in its messages/model, rebuild the
    /// transcript, adopt its id/name, and follow its working directory.
    pub(super) fn apply_session(&mut self, id: String, session: hrdr_agent::Session) {
        // A running turn holds the agent mutex: the message swap below would
        // silently no-op while the transcript + session id still switched, and
        // the in-flight turn's autosave would then overwrite the resumed
        // session's file with the old conversation.
        if self.running {
            // Defense in depth: the shared dispatcher already guards /resume,
            // but auto-resume/other callers reach this directly.
            self.system(hrdr_app::RESUME_BUSY_MSG);
            return;
        }
        let plan = hrdr_app::resume_plan(
            &session,
            std::path::Path::new(&self.current_cwd()),
            &self.base_url,
        );
        self.with_agent(|a| {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        });
        self.model = session.model.clone();
        self.provider = session.provider.clone();
        self.rebuild_transcript(&session.messages);
        // Restore saved TODOs.
        if let Ok(mut todos) = self.todos.lock() {
            *todos = session.todos.clone();
        }
        self.session_id = Some(id.clone());
        self.session_label = Some(session.name.clone());
        self.scroll_offset = 0;
        // Switch hrdr's tools to the session's working directory (in-process
        // only — the parent shell is untouched).
        if let Some(target) = plan.new_cwd {
            self.apply_cwd(target);
        }
        for line in plan.lines {
            self.system(line);
        }
    }
    /// Rebuild the display transcript from a restored message history (the
    /// entry construction is shared with the GUI via
    /// [`hrdr_app::messages_to_entries`]).
    fn rebuild_transcript(&mut self, msgs: &[Message]) {
        self.clear_transcript();
        for e in hrdr_app::messages_to_entries(msgs) {
            self.push_entry(e);
        }
    }
}
