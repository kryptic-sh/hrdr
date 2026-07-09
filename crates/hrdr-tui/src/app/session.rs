//! Session persistence and restore.
//!
//! The app's [`hrdr_app::SessionState`] *is* the session file's payload, so
//! saving is "refresh the mirrors, serialize" and resuming is "assign". There is
//! no conversion layer, and nothing to keep in sync by hand.

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
        self.auto_resume_state(session.state, id);
    }

    /// The state-swap half of [`Self::auto_resume_latest`], split out so it can
    /// be driven without a session file on disk.
    pub(super) fn auto_resume_state(&mut self, state: hrdr_app::SessionState, id: String) {
        let name = state.name.clone();
        let messages = state.messages.len();
        self.adopt_state(state, Some(id));
        self.push_entry(Entry::system(format!(
            "resumed most recent session '{name}' ({messages} messages) — /clear to start fresh"
        )));
    }

    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, and `/rename`.
    pub(super) fn autosave(&mut self) {
        // A running turn holds the agent lock; skip this save rather than block
        // the UI thread (the next one will catch up).
        let Some((msgs, cwd)) = self
            .agent
            .try_lock()
            .ok()
            .map(|a| (a.messages_owned(), a.cwd().display().to_string()))
        else {
            return;
        };
        let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
        self.state.sync_from(msgs, todos, cwd);

        if let Some(o) = hrdr_app::save_session(&self.state) {
            // Notify once, when the session is first created.
            if o.first_save {
                self.push_entry(Entry::system(hrdr_app::session_saved_notice(&o.id)));
            }
            self.state.id = Some(o.id);
        }
    }

    /// Restore a resolved session (the shared `/resume` command calls this via
    /// [`hrdr_app::CommandHost::resume`]): adopt its state and follow its
    /// working directory.
    pub(super) fn apply_session(&mut self, id: String, session: hrdr_app::Session) {
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
            &session.state,
            std::path::Path::new(&self.current_cwd()),
            &self.state.base_url,
        );
        self.adopt_state(session.state, Some(id));
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

    /// Swap in a loaded session's state wholesale, pushing the parts whose
    /// runtime owners live elsewhere (chat history → the agent, TODOs → the
    /// shared list) back out to them.
    ///
    /// Three fields are not simply overwritten:
    ///
    /// * `base_url` — the endpoint belongs to this process (`--base-url`,
    ///   `/provider`), not to the saved conversation. The resume notice reports
    ///   a mismatch rather than silently switching endpoints.
    /// * `context_window` — a saved one is a stand-in until the endpoint is
    ///   re-probed, so it never clobbers a window this process already knows.
    /// * `model` / `provider` — the session supplies them only when this process
    ///   didn't pin them with a flag or an env var. Precedence, highest first:
    ///   **flag > env > session > config**. Applies to `/resume` as well as to
    ///   startup auto-resume: a pinned model never switches out from under you.
    fn adopt_state(&mut self, state: hrdr_app::SessionState, id: Option<String>) {
        let probed_window = self.state.usage.context_window;
        let base_url = std::mem::take(&mut self.state.base_url);
        let pinned_model = self.cfg.model_pinned.then(|| self.state.model.clone());
        let pinned_provider = self
            .cfg
            .provider_pinned
            .then(|| self.state.provider.clone());

        self.state = state.restored();
        self.state.id = id;
        self.state.base_url = base_url;
        self.state.usage.context_window = probed_window.or(self.state.usage.context_window);
        if let Some(model) = pinned_model {
            self.state.model = model;
        }
        if let Some(provider) = pinned_provider {
            self.state.provider = provider;
        }

        self.with_agent(|a| {
            a.set_messages(self.state.messages.clone());
            a.set_model(self.state.model.clone());
        });
        if let Ok(mut todos) = self.todos.lock() {
            *todos = self.state.todos.clone();
        }
        crate::ui::clear_transcript_cache();
    }
}
