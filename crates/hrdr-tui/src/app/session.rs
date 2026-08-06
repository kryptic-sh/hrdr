//! Session persistence and restore.
//!
//! The app's [`hrdr_app::SessionState`] *is* the session file's payload, so
//! saving is "refresh the mirrors, serialize" and resuming is "assign". There is
//! no conversion layer, and nothing to keep in sync by hand.

use super::*;

impl super::App {
    fn record_session_save(
        &mut self,
        result: anyhow::Result<Option<hrdr_app::SaveOutcome>>,
    ) -> Option<hrdr_app::SaveOutcome> {
        match result {
            Ok(outcome) => {
                self.session_save_error = None;
                outcome
            }
            Err(error) => {
                let error = error.to_string();
                self.note_save_error(error);
                None
            }
        }
    }

    /// Surface a session-save failure once per distinct error: warn via the
    /// toast stack and remember the error so a retry failing the same way
    /// stays silent (the warning is already on screen).
    fn note_save_error(&mut self, error: String) {
        if self.session_save_error.as_deref() != Some(&error) {
            self.toasts.warn(format!(
                "session autosave failed — conversation is not safely stored: {error}"
            ));
            self.session_save_error = Some(error);
        }
    }

    /// Point the shared sub-agent transcript cell at the current session's dir,
    /// and attach the MAIN agent's own durable transcript writer at its sibling
    /// `<id>.jsonl`. Called after the session id is assigned; anything spawned or
    /// emitted before this (a brand-new session's first turn) is simply not
    /// persisted — matching the documented pre-first-save behavior.
    ///
    /// Attaching is idempotent (a no-op once the writer is open), so calling this
    /// on every save is cheap; a session *switch* detaches first
    /// (`detach_transcript`, in `clear_all` and `adopt_state`) so the writer then
    /// re-opens against the new session's file.
    pub(super) fn refresh_subagent_dir(&self) {
        if let Some(id) = &self.state().id {
            let cwd = self.current_cwd();
            let dir = hrdr_app::child_transcript_dir(&cwd, id);
            if let Ok(mut cell) = self.subagent_dir.lock() {
                *cell = Some(dir);
            }
            let jsonl = hrdr_app::session_transcript_path(&cwd, id);
            self.registry
                .attach_transcript(hrdr_agent::MAIN_KEY, &jsonl);
        }
    }

    /// On startup, resume the most recent saved session for the current
    /// directory (if any). No match → leave the fresh session as-is.
    pub(super) fn auto_resume_latest(&mut self) {
        let cwd = self.current_cwd();
        // Shared lookup, taking the session's open-lock (skips empty/system-only).
        //
        // Anything other than a resumable, ownable session — nothing saved here
        // yet, a corrupt newest file, OR (the `Err`) a session already open in
        // another hrdr window — falls through to a fresh start. Auto-resume never
        // hard-errors on a busy candidate: a jarring startup error is the wrong
        // UX; only an explicit `/resume` refuses.
        if let Ok(Some((id, session, lock))) = hrdr_app::open_latest_session_for_cwd(&cwd) {
            self.active_lock = Some(lock);
            self.auto_resume_state(session.state, id);
        }
    }

    /// Open `path`'s session under its open-lock and swap it in as the active
    /// session — the shared body of an **explicit** resume (the `/resume` picker
    /// and the `/resume <arg>` text path). `id` is the file id, shown in messages.
    ///
    /// Ordering is acquire-new-before-release-old: [`hrdr_app::Session::open_path`]
    /// takes the new lock first, so a session held open elsewhere (`Busy`) leaves
    /// the current session and its lock untouched. On success the old lock is
    /// dropped as the new one is stored.
    pub(super) fn resume_locked_path(&mut self, id: String, path: &std::path::Path) {
        match hrdr_app::Session::open_path(path) {
            Ok((session, lock)) => {
                // A running turn holds the agent lock; `apply_session` would
                // reject the swap. Drop the freshly-taken lock and keep the
                // current session rather than releasing its lock for nothing.
                if self.running() {
                    drop(lock);
                    self.system(hrdr_app::RESUME_BUSY_MSG);
                    return;
                }
                self.active_lock = Some(lock); // releases the previous session's lock
                self.pending_fork = None; // a normal resume clears any stale offer
                self.apply_session(id, session);
            }
            Err(hrdr_app::OpenError::Busy { pid, .. }) => {
                // Can't take the session (a live instance owns it) — but the user
                // can open a forked copy instead. Arm the offer for the next key.
                self.pending_fork = Some((id.clone(), path.to_path_buf()));
                self.system(format!(
                    "session {id} is open in another hrdr window (pid {pid}) — \
                     press f to open a copy, or Esc to cancel"
                ));
            }
            Err(hrdr_app::OpenError::Load(e)) => {
                self.system(format!("can't load session {id}: {e}"));
            }
        }
    }

    /// Open a forked copy of the session at `path` (the busy-`/resume` escape
    /// hatch): copy the source's current snapshot into a fresh, independently
    /// locked session and swap it in exactly like a successful resume — set
    /// [`Self::active_lock`] to the fork's guard (dropping the old one) and
    /// [`Self::apply_session`]. `source_id` names the busy original in messages.
    ///
    /// Gated on no running turn, matching [`Self::resume_locked_path`]: a running
    /// turn holds the agent lock and `apply_session` would reject the swap, so
    /// refuse up front rather than mint a fork we can't use.
    pub(super) fn fork_session(&mut self, source_id: String, path: &std::path::Path) {
        if self.running() {
            self.system(hrdr_app::RESUME_BUSY_MSG);
            return;
        }
        match hrdr_app::Session::fork(&self.current_cwd(), path) {
            Ok((new_id, session, lock)) => {
                self.active_lock = Some(lock); // releases the previous session's lock
                self.apply_session(new_id.clone(), session);
                self.system(format!(
                    "session {source_id} is open elsewhere — opened a copy as {new_id}"
                ));
            }
            Err(e) => {
                self.system(format!("couldn't fork session {source_id}: {e:#}"));
            }
        }
    }

    /// The state-swap half of [`Self::auto_resume_latest`], split out so it can
    /// be driven without a session file on disk.
    pub(super) fn auto_resume_state(&mut self, state: hrdr_app::SessionState, id: String) {
        let name = state.name.clone();
        let messages = state.messages.len();
        self.adopt_state(state, Some(id));
        self.system(format!(
            "resumed most recent session '{name}' ({messages} messages) — /new to start fresh"
        ));
    }

    /// Mid-turn durability: the agent just committed a tool round and sent a
    /// history snapshot ([`hrdr_agent::AgentEvent::History`]). The turn task
    /// holds the agent lock, so [`Self::autosave`]'s try_lock read would skip —
    /// adopt the snapshot it sent and persist that instead. With this, a crash
    /// mid-turn loses at most the round in flight.
    ///
    /// The state mutation stays on the UI thread; the serialize + atomic write
    /// move to a spawned task once the session has an id (the mint — id +
    /// open-lock — is synchronous and only ever runs here on the UI thread).
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
        if self.state().id.is_some() {
            // The id (and its open-lock) was minted synchronously — at turn
            // start by `reserve_session_id`, or by the first sync save below.
            // Only the write goes off-thread.
            self.enqueue_save();
            return;
        }
        // No id yet (near-unreachable — `reserve_session_id` runs at turn
        // start): mint + write synchronously, exactly as before.
        let saved = hrdr_app::save_session(self.state());
        if let Some(mut o) = self.record_session_save(saved) {
            // On the first save this session's id is minted and its open-lock is
            // taken — hold it. `None` on every later save, so this never clobbers.
            if let Some(lock) = o.open_lock.take() {
                self.active_lock = Some(lock);
            }
            if o.first_save {
                self.system(hrdr_app::session_saved_notice(&o.id));
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
        // `state.cwd` is only synced by the turn-end autosave; on the very first
        // turn it is still empty, which would mint the id — and file the first
        // save — under the wrong cwd slug. Set it from the agent before minting.
        let cwd = self.current_cwd();
        self.state_mut().cwd = cwd;
        // `save_session` skips a conversation with no user message, and the agent
        // does not push this one until the turn starts — so seed the mirror. The
        // next autosave replaces it with the agent's own history.
        self.state_mut()
            .messages
            .push(hrdr_agent::Message::user(sent));
        // Mint the id + open-lock synchronously — cheap — but defer the serialize
        // and the two-fsync atomic write to the off-thread save task
        // (`enqueue_save` below). Running the full `save_session` here put the
        // whole write on the UI thread, freezing the first Enter for the duration
        // of the disk I/O. The id must still be claimed now, not merely computed:
        // [`unique_session_id`] establishes uniqueness by looking for an existing
        // file, so a second hrdr started in the same cwd would mint the same id
        // until one of them writes — and the id is what names the sub-agent
        // transcript dir the turn is about to use.
        //
        // [`unique_session_id`]: hrdr_app::unique_session_id
        match hrdr_app::mint_session(self.state()) {
            Ok(Some(mut o)) => {
                // Hold the freshly-minted session's open-lock (see `autosave`).
                if let Some(lock) = o.open_lock.take() {
                    self.active_lock = Some(lock);
                }
                // Stay silent here: the notice belongs *after* the turn, not ahead of
                // the reply. Hand it to the first autosave, which would otherwise see
                // an id already set and conclude this was not a first save.
                self.session_notice_pending = o.first_save;
                self.state_mut().id = Some(o.id);
                self.refresh_subagent_dir();
                // The reservation must live until the first write lands; the save
                // task takes it (see `enqueue_save`).
                self.pending_reservation = o.reservation;
            }
            Ok(None) => {
                // Not saveable — near-unreachable: we just pushed a user message.
            }
            Err(error) => {
                // Mint failed (a filesystem error): surface it exactly like the
                // sync path did and leave the id unset — the first autosave
                // retries the mint + write.
                let error = format!("{error:#}");
                self.note_save_error(error);
            }
        }
        // The write itself goes off-thread; without a minted id there is nothing
        // to write yet and the next autosave retries.
        if self.state().id.is_some() {
            self.enqueue_save();
        }
    }

    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/rename`, a
    /// cancelled turn, and right before the app quits — so the visible user
    /// message + any partial reply from a turn that never finished isn't lost.
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

        if self.state().id.is_some() {
            // The mint happened synchronously (at turn start, or on the very
            // first save); only the write goes off-thread. Hand the notice the
            // mint deferred over at the same point as always — here, turn end —
            // so it lands once the first save of the session succeeds.
            if std::mem::take(&mut self.session_notice_pending) {
                let id = self.state().id.clone().unwrap_or_default();
                self.system(hrdr_app::session_saved_notice(&id));
            }
            self.enqueue_save();
            return;
        }
        // No id yet: mint + write synchronously, unchanged.
        let saved = hrdr_app::save_session(self.state());
        if let Some(mut o) = self.record_session_save(saved) {
            // Hold the freshly-minted session's open-lock, if this was the mint.
            if let Some(lock) = o.open_lock.take() {
                self.active_lock = Some(lock);
            }
            // Notify once, when the session is first created — including when
            // `reserve_session_id` created it at turn start and deferred the
            // notice to here (it sees `first_save` as false by then).
            if o.first_save || std::mem::take(&mut self.session_notice_pending) {
                self.system(hrdr_app::session_saved_notice(&o.id));
            }
            self.state_mut().id = Some(o.id);
            self.refresh_subagent_dir();
        }
    }

    /// Queue a save of the current session state. Called only once the session
    /// has an id — the mint stays synchronous, only the write is off-thread.
    ///
    /// The snapshot is captured HERE on the UI thread, so a `/rename` or
    /// `/clear` after this point cannot corrupt an in-flight write; the next
    /// enqueue supersedes it (latest-wins: at most one save task runs, and a
    /// newer snapshot is always what it writes next).
    fn enqueue_save(&mut self) {
        let snapshot = self.state().clone();
        if self.save_in_flight {
            self.pending_save = Some(snapshot);
            return;
        }
        // A pending first-save reservation rides into the task: it must stay
        // alive until the write attempt ends (its drop removes the `.id.lock`
        // a failed first write would otherwise leave behind).
        let reservation = self.pending_reservation.take();
        self.spawn_save(snapshot, reservation);
    }

    /// Spawn the serialize + atomic-write for `snapshot`. The id is captured
    /// from the CURRENT state at spawn time — the pending snapshot belongs to
    /// whatever session is current, and `/clear` or `/resume` since it was
    /// captured leaves the pipeline stale (see the guard in
    /// [`Self::promote_pending_save`]).
    fn spawn_save(
        &mut self,
        snapshot: hrdr_app::SessionState,
        reservation: Option<hrdr_agent::Reservation>,
    ) {
        let id = self
            .state()
            .id
            .clone()
            .expect("a save is only spawned after the id exists");
        self.save_in_flight = true;
        let tx = self.tx.clone();
        let save_done = self.save_done.clone();
        tokio::spawn(async move {
            // `_reservation` is dropped when the task ends — after the write
            // attempt, whatever its outcome. On success `Session::save` already
            // removed the lock; on failure the drop cleans it up.
            let _reservation = reservation;
            let res = hrdr_app::Session::new(snapshot.persisted()).save(&id);
            let _ = tx
                .send(TurnMsg::SaveDone(
                    res.map(|p| p.display().to_string())
                        .map_err(|e| e.to_string()),
                ))
                .await;
            save_done.notify_one();
        });
    }

    /// An off-thread save finished: clear the in-flight flag, surface a failure
    /// exactly as the sync path does, and write the newest pending snapshot
    /// next. The snapshot captured at enqueue time is what was written, so a
    /// `/rename`/`/clear` that landed since cannot have interleaved with it.
    pub(super) fn on_save_done(&mut self, result: Result<String, String>) {
        self.save_in_flight = false;
        match result {
            Ok(_) => self.session_save_error = None,
            Err(error) => {
                self.note_save_error(error);
            }
        }
        self.promote_pending_save();
    }

    /// Write the pending snapshot (if any) next. The snapshot must belong to
    /// the CURRENT session: `/clear` or `/resume` since it was captured resets
    /// the id, and the old session's snapshot must never be written under the
    /// new one's filename — drop it instead (the session it belonged to was
    /// deliberately discarded).
    fn promote_pending_save(&mut self) {
        if let Some(next) = self.pending_save.take()
            && next.id.as_deref() == self.state().id.as_deref()
        {
            let reservation = self.pending_reservation.take();
            self.spawn_save(next, reservation);
        }
    }

    /// Wait for every save the coalescer has queued (in-flight or pending) to
    /// land. The quit path calls this after its final `autosave`: the process
    /// must not exit before the last snapshot reaches disk.
    ///
    /// Each save posts its `SaveDone` to the channel and then notifies; on the
    /// quit path the channel is never drained (the loop has already stopped
    /// selecting on it), so `on_save_done` never runs — reflect each
    /// completion here instead: clear the in-flight flag and promote the
    /// pending snapshot, exactly as `on_save_done` would. The write lands
    /// BEFORE the notification, so waking is the durability signal.
    pub(crate) async fn await_saves(&mut self) {
        while self.save_in_flight || self.pending_save.is_some() {
            self.save_done.notified().await;
            self.save_in_flight = false;
            self.promote_pending_save();
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
    /// * `model` / `provider` — the session's identity WINS. A conversation carries
    ///   the model and the provider it ran on, and resuming it brings both back.
    ///   `--model` / `$HRDR_MODEL` / config.toml settle the identity a **new** session
    ///   starts on — they are the default, not a pin, and a session that already has
    ///   an identity (resumed, or picked with `/model`) is not overridden by them.
    ///   Applies to `/resume` as well as to startup auto-resume.
    ///
    /// And when the session supplies the provider, the agent is **repointed to it**
    /// ([`hrdr_app::restore_session_provider`]) — endpoint, key and model together.
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
        // The identity in force right now — the provider an OLD session file (one
        // that named a model but no provider) means by "this model".
        let in_force = self.state().model.clone();

        // The state *is* the main pane's — transcript, counters and all — so
        // adopting a session is one assignment. There is nothing left to hand back.
        *self.state_mut() = state.restored();
        // A first-save reservation pending for the session we just left is
        // stale: `promote_pending_save`'s id guard drops that snapshot, so its
        // `.id.lock` must go too (dropping the reservation removes it).
        self.pending_reservation = None;
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
        // Drop the outgoing session's transcript writer before pointing the dirs
        // at the incoming one: `refresh_subagent_dir` then re-attaches against the
        // adopted id (append mode — a resume continues that session's jsonl).
        // Without this a resumed/switched session would append onto the file we
        // just left.
        self.registry.detach_transcript(hrdr_agent::MAIN_KEY);
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
        // One thing stops it: an identity the agent is **already on** needs no switch.
        let (reference, window) = {
            let s = self.state();
            (s.model.clone(), s.usage.context_window)
        };
        let current = self.registry.with(|v| {
            v.iter()
                .find(|e| e.key == hrdr_agent::MAIN_KEY)
                .map(|e| (e.provider.clone().unwrap_or_default(), e.model.clone()))
        });
        let unchanged = current.as_ref()
            == Some(&(
                reference.provider().to_string(),
                reference.model().to_string(),
            ));
        if !unchanged && !reference.model().is_empty() {
            let name = reference.provider().to_string();
            let model = reference.model().to_string();
            // The provider the AGENT is on — the one whose endpoint (relocated or not)
            // it is currently talking to, and the one a switch moves it off.
            let from = current
                .as_ref()
                .map(|(p, _)| p.clone())
                .unwrap_or_else(|| in_force.provider().to_string());
            let mut host = commands::TuiHost { app: self };
            if let Err(e) = hrdr_app::restore_session_provider(&mut host, &name, model, window) {
                self.system(format!(
                    "this session ran on provider '{name}', which isn't usable here ({e}) — \
                     staying on the current endpoint; /model to switch"
                ));
            } else if from != name {
                // A change of PROVIDER moves the endpoint — the agent's rule, and the
                // chrome's, so the bar names the endpoint the agent is talking to. (The
                // switch itself posts no endpoint here: it repoints an identity the
                // pane already shows.)
                let now = self
                    .cfg
                    .resolve_provider(&name)
                    .map(|p| p.base_url)
                    .unwrap_or_default();
                if !now.is_empty() {
                    self.set_active_base_url(now);
                }
            }
        }

        if let Ok(mut t) = self.todos.lock() {
            *t = todos;
        }
        // A resumed session is a different transcript — every index-based view
        // state (opened thoughts) from the session we left is meaningless here.
        self.thinking_open.clear();
        crate::ui::clear_transcript_cache();
    }
}
