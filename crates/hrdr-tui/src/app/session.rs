//! Session persistence and restore.
//!
//! The app's [`hrdr_app::SessionState`] *is* the session file's payload, so
//! saving is "refresh the mirrors, serialize" and resuming is "assign". There is
//! no conversion layer, and nothing to keep in sync by hand.

use super::*;

impl super::App {
    /// Point the shared sub-agent transcript cell at the current session's dir.
    /// Called after the session id is assigned; sub-agents spawned before this
    /// (a brand-new session's first turn) are simply not persisted.
    pub(super) fn refresh_subagent_dir(&self) {
        if let Some(id) = &self.state().id {
            let dir = hrdr_app::subagent_transcript_dir(&self.current_cwd(), id);
            if let Ok(mut cell) = self.subagent_dir.lock() {
                *cell = Some(dir);
            }
        }
    }

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
        self.push_entry(Entry::notice(format!(
            "resumed most recent session '{name}' ({messages} messages) — /new to start fresh"
        )));
    }

    /// Mid-turn durability: the agent just committed a tool round and sent a
    /// history snapshot ([`hrdr_agent::AgentEvent::History`]). The turn task
    /// holds the agent lock, so [`Self::autosave`]'s try_lock read would skip —
    /// adopt the snapshot it sent and persist that instead. With this, a crash
    /// mid-turn loses at most the round in flight.
    pub(super) fn persist_mid_turn(&mut self, messages: Vec<hrdr_agent::Message>) {
        let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
        // `state.cwd` is only synced by the turn-end autosave; on the very
        // first turn it is still empty, which would file the session under the
        // wrong cwd slug.
        let cwd = self.current_cwd();
        let state = self.state_mut();
        state.messages = messages;
        state.todos = todos;
        state.cwd = cwd;
        if let Some(o) = hrdr_app::save_session(self.state()) {
            if o.first_save {
                self.push_entry(Entry::notice(hrdr_app::session_saved_notice(&o.id)));
            }
            self.state_mut().id = Some(o.id);
            self.refresh_subagent_dir();
        }
    }

    /// Claim this session's id — and with it the sub-agent transcript dir —
    /// *before* the turn runs, when it does not have one yet.
    ///
    /// The id is otherwise assigned only when the agent emits its first `History`
    /// event, and that lands **after** the round's tool batch has already
    /// executed. So on a brand-new session the first delegated `task` spawned
    /// while the transcript dir cell was still empty and its transcript was
    /// silently dropped — precisely the crash the transcript exists to survive.
    ///
    /// The id must be *reserved*, not merely computed: [`unique_session_id`]
    /// establishes uniqueness by looking for an existing file, so a second hrdr
    /// started in the same cwd would mint the same id until one of them writes.
    /// Saving here also means a crash during the very first turn no longer loses
    /// the user's message.
    ///
    /// `sent` is the prepared outgoing message — the same text the agent is about
    /// to push — so the mirror we save matches the history the agent will build.
    ///
    /// [`unique_session_id`]: hrdr_app::unique_session_id
    pub(crate) fn reserve_session_id(&mut self, sent: &str) {
        if self.state().id.is_some() {
            return;
        }
        // An *empty* turn carries no message of its own: it exists to hand the agent
        // something already in its history — a `!command`'s output, or a finished
        // background task. Seeding the mirror with an empty user message would create
        // a session whose first turn is blank, named after nothing (`session.json`).
        // The turn still runs; its autosave names the session from the agent's real
        // history once the note is in it.
        if sent.trim().is_empty() {
            return;
        }
        // `save_session` skips a conversation with no user message, and the agent
        // does not push this one until the turn starts — so seed the mirror. The
        // next autosave replaces it with the agent's own history.
        self.state_mut()
            .messages
            .push(hrdr_agent::Message::user(sent));
        if let Some(o) = hrdr_app::save_session(self.state()) {
            // Stay silent here: the notice belongs *after* the turn, not ahead of
            // the reply. Hand it to the first autosave, which would otherwise see
            // an id already set and conclude this was not a first save.
            self.session_notice_pending = o.first_save;
            self.state_mut().id = Some(o.id);
            self.refresh_subagent_dir();
        }
    }

    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, `/rename`, a cancelled turn, and right before the app quits —
    /// so the visible user message + any partial reply from a turn that never
    /// finished isn't lost.
    pub(crate) fn autosave(&mut self) {
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
        self.state_mut().sync_from(msgs, todos, cwd);

        if let Some(o) = hrdr_app::save_session(self.state()) {
            // Notify once, when the session is first created — including when
            // `reserve_session_id` created it at turn start and deferred the
            // notice to here (it sees `first_save` as false by then).
            if o.first_save || std::mem::take(&mut self.session_notice_pending) {
                self.push_entry(Entry::notice(hrdr_app::session_saved_notice(&o.id)));
            }
            self.state_mut().id = Some(o.id);
            self.refresh_subagent_dir();
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
        if self.running() {
            // Defense in depth: the shared dispatcher already guards /resume,
            // but auto-resume/other callers reach this directly.
            self.system(hrdr_app::RESUME_BUSY_MSG);
            return;
        }
        let plan = hrdr_app::resume_plan(
            &session.state,
            std::path::Path::new(&self.current_cwd()),
            &self.state().base_url,
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
    /// Two fields are not simply overwritten:
    ///
    /// * `context_window` — a saved one is a stand-in until the endpoint is
    ///   re-probed, so it never clobbers a window this process already knows.
    /// * `model` / `provider` — the session supplies them only when this process
    ///   didn't pin them with a flag or an env var. Precedence, highest first:
    ///   **flag > env > session > config**. Applies to `/resume` as well as to
    ///   startup auto-resume: a pinned model never switches out from under you.
    ///
    /// And when the session *does* supply the provider, the agent is **repointed to
    /// it** ([`hrdr_app::restore_session_provider`]) — endpoint, key and model
    /// together.
    ///
    /// Regression: the endpoint used to be treated as the process's, and a resume
    /// only printed "note: session endpoint was X". So a session saved on one
    /// provider, resumed in a process configured for another, adopted the session's
    /// model *name* and provider *label* into the status bar while the agent kept
    /// talking to the launch endpoint — where that model does not exist and the key
    /// is not valid. The bar said one thing; the socket did another. A conversation's
    /// provider is part of the conversation.
    fn adopt_state(&mut self, state: hrdr_app::SessionState, id: Option<String>) {
        let probed_window = self.state().usage.context_window;
        let base_url = std::mem::take(&mut self.state_mut().base_url);
        // ONE pin, for one identity. A flag/env (`--model` / `$HRDR_MODEL`) pins what
        // this process runs on, and a resumed session does not unpin it — but it pins
        // the identity WHOLE. Pinning one half and adopting the other is how a
        // session's model used to arrive on the launch provider (or the launch model
        // on the session's provider), which is precisely the pair that never agreed.
        let pinned = self.cfg.model_pinned.then(|| self.state().model.clone());
        // The identity in force right now — the provider an OLD session file (one
        // that named a model but no provider) means by "this model".
        let in_force = self.state().model.clone();

        // The state *is* the main pane's — transcript, counters and all — so
        // adopting a session is one assignment. There is nothing left to hand back.
        *self.state_mut() = state.restored();
        let state = self.state_mut();
        state.id = id;
        state.base_url = base_url;
        state.usage.context_window = probed_window.or(state.usage.context_window);
        // A pre-`provider://model` session file: its model, on the provider we are on.
        if state.provider_unset {
            state.model = hrdr_agent::ModelSpec::ModelOnly(state.model.model().to_string())
                .apply(&in_force)
                .expect("a bare model id always resolves");
            state.provider_unset = false;
        }
        if let Some(model) = pinned {
            state.model = model;
        }
        self.refresh_subagent_dir();
        // The pane is rebuilt from the registry every frame, main agent included —
        // so a resumed session's model/endpoint/counters have to land there too, or
        // the next draw quietly restores the ones we just replaced.
        self.publish_main_agent();

        // The resumed session's spend is seeded into the agent's own counter, so it
        // counts on from there — rather than the frontend keeping a second tally and
        // adding it to the agent's on the way to the screen.
        let (messages, todos, spent) = {
            let s = self.state();
            (s.messages.clone(), s.todos.clone(), s.usage.cost_usd)
        };
        self.with_agent(|a| {
            a.set_messages(messages);
            a.set_session_cost(spent);
        });

        // The conversation's IDENTITY comes back with it — provider and model
        // together, which is the only way either of them means anything: resuming a
        // conversation and then talking to a different provider's endpoint is not the
        // same conversation. The agent is switched with it, so the thing doing the
        // talking is the thing being displayed. (The model alone used to be handed
        // over here, leaving the agent on the launch endpoint.)
        //
        // Two things stop it. A **pinned** identity (`--model`, or `$HRDR_MODEL`) is
        // this process's decision, already applied at launch — re-resolving it would
        // throw away an endpoint the user chose (a `--base-url` override) for the
        // provider's canonical one. And an identity the agent is **already on** needs
        // no switch, for the same reason.
        let (reference, window) = {
            let s = self.state();
            (s.model.clone(), s.usage.context_window)
        };
        let current = self.live_subagents.with(|v| {
            v.iter()
                .find(|e| e.key == hrdr_agent::MAIN_KEY)
                .map(|e| (e.provider.clone().unwrap_or_default(), e.model.clone()))
        });
        let switchable = !self.cfg.model_pinned;
        let unchanged = current.as_ref()
            == Some(&(
                reference.provider().to_string(),
                reference.model().to_string(),
            ));
        if switchable && !unchanged && !reference.model().is_empty() {
            let name = reference.provider().to_string();
            let model = reference.model().to_string();
            let mut host = commands::TuiHost { app: self };
            if let Err(e) = hrdr_app::restore_session_provider(&mut host, &name, model, window) {
                self.system(format!(
                    "this session ran on provider '{name}', which isn't usable here ({e}) — \
                     staying on the current endpoint; /model to switch"
                ));
            }
        }

        if let Ok(mut t) = self.todos.lock() {
            *t = todos;
        }
        crate::ui::clear_transcript_cache();
    }
}
