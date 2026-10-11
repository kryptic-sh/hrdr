//! Internal checkpoint coordination for a quiescent saved TUI session.

use super::*;

const IDLE_HANDOFF_PAYLOAD: &[u8] = br#"{"version":1,"intent":"idle","operation_id":null,"runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#;

/// Prepared external work that can take ownership after a handoff commits.
///
/// Implementations are supplied by the frontend that prepared the update; the TUI
/// neither knows nor starts an apps coordinator itself.
pub trait PreparedUpdateCoordinator: Send {
    fn launch(self: Box<Self>) -> anyhow::Result<()>;
}

/// A one-shot update request held by the TUI until its current turn is over.
pub trait DeferredUpdate: Send {
    fn prepare(self: Box<Self>, session_id: &str) -> anyhow::Result<AdmittedUpdate>;
}

/// Sender exposed only to the frontend-owned update tool.
#[derive(Clone)]
pub struct UpdateRequestSender {
    tx: tokio::sync::mpsc::UnboundedSender<Box<dyn DeferredUpdate>>,
    sent: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl UpdateRequestSender {
    pub fn send(&self, request: Box<dyn DeferredUpdate>) -> anyhow::Result<()> {
        if self
            .sent
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            anyhow::bail!("an update is already pending");
        }
        if self.tx.send(request).is_err() {
            self.sent.store(false, std::sync::atomic::Ordering::Release);
            anyhow::bail!("update request is unavailable");
        }
        Ok(())
    }
}

pub struct UpdateRequestReceiver {
    rx: tokio::sync::mpsc::UnboundedReceiver<Box<dyn DeferredUpdate>>,
    sent: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl UpdateRequestReceiver {
    pub(crate) fn try_recv(
        &mut self,
    ) -> Result<Box<dyn DeferredUpdate>, tokio::sync::mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    /// Release the request slot after a terminal admission or preparation failure.
    pub(crate) fn release(&self) {
        self.sent.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub fn update_request_bridge() -> (UpdateRequestSender, UpdateRequestReceiver) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        UpdateRequestSender {
            tx,
            sent: sent.clone(),
        },
        UpdateRequestReceiver { rx, sent },
    )
}

/// An update admitted by a frontend after it has prepared its coordinator.
pub struct AdmittedUpdate {
    operation_id: hrdr_agent::UpdateOperationId,
    coordinator: Box<dyn PreparedUpdateCoordinator>,
}

impl AdmittedUpdate {
    pub fn new(
        operation_id: hrdr_agent::UpdateOperationId,
        coordinator: impl PreparedUpdateCoordinator + 'static,
    ) -> Self {
        Self {
            operation_id,
            coordinator: Box::new(coordinator),
        }
    }
}

fn update_handoff_payload(operation_id: &hrdr_agent::UpdateOperationId) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::json!({
        "version": 1,
        "intent": "update_continuation",
        "operation_id": operation_id,
        "runtime_queue": false,
        "runtime_tasks": false,
        "runtime_tools": false,
        "runtime_turn": false,
        "runtime_compaction": false,
        "runtime_watch": false,
    })
    .to_string()
    .into_bytes())
}

pub(crate) fn validate_handoff_payload(
    payload: &[u8],
    session: &hrdr_agent::Session,
) -> anyhow::Result<hrdr_agent::HandoffIntent> {
    hrdr_agent::HandoffPayload::validate_snapshot(payload, &session.state.messages)
}

#[allow(dead_code)]
impl App {
    /// Persist and commit a handoff checkpoint only while no runtime work remains.
    ///
    /// This is deliberately internal: it only records a recoverable boundary and
    /// freezes input after commit. It does not install, restart, launch recovery,
    /// or expose a user command.
    pub(crate) async fn commit_handoff_checkpoint(&mut self) -> anyhow::Result<bool> {
        if !self.handoff_quiescent() {
            return Ok(false);
        }

        self.autosave();
        self.await_saves().await;
        if !self.handoff_quiescent() {
            return Ok(false);
        }

        let id = match self.state().id.clone() {
            Some(id) => id,
            None => return Ok(false),
        };
        let Some(lock) = self.active_lock.as_ref() else {
            return Ok(false);
        };
        if !lock.is_backed() {
            return Ok(false);
        }

        let journal = hrdr_agent::HandoffJournal::open(&self.current_cwd(), &id, lock)?;
        let epoch = hrdr_tools::unix_now();
        journal.append(
            epoch,
            hrdr_agent::HandoffState::Prepare,
            IDLE_HANDOFF_PAYLOAD,
        )?;
        journal.append(
            epoch,
            hrdr_agent::HandoffState::Commit,
            IDLE_HANDOFF_PAYLOAD,
        )?;
        self.handoff_gate = true;
        Ok(true)
    }

    fn append_handoff_record(
        &self,
        id: &str,
        epoch: u64,
        state: hrdr_agent::HandoffState,
        payload: &[u8],
    ) -> anyhow::Result<()> {
        let lock = self
            .active_lock
            .as_ref()
            .expect("quiescent handoff retains a backed session lock");
        hrdr_agent::HandoffJournal::open(&self.current_cwd(), id, lock)?
            .append(epoch, state, payload)
    }

    /// Commit a prepared update handoff, then let its prepared owner take over.
    ///
    /// The coordinator is prepared before this method mutates the app, and is
    /// consumed exactly once only after the durable commit and input gate.
    /// Commit a prepared update supplied by an internal caller.
    ///
    /// Kept for existing handoff callers; the deferred path retains its update in
    /// `admitted_update` and uses `commit_stored_admitted_update` so an `Ok(false)`
    /// cannot consume the one-shot coordinator.
    pub(crate) async fn commit_admitted_update(
        &mut self,
        admitted: AdmittedUpdate,
    ) -> anyhow::Result<bool> {
        if self.admitted_update.is_some() {
            return Ok(false);
        }
        self.admitted_update = Some(admitted);
        self.commit_stored_admitted_update()
    }

    pub(crate) fn commit_stored_admitted_update(&mut self) -> anyhow::Result<bool> {
        if !self.handoff_quiescent() {
            return Ok(false);
        }
        let Some(admitted) = self.admitted_update.as_ref() else {
            return Ok(false);
        };
        if !self
            .active_lock
            .as_ref()
            .is_some_and(hrdr_app::SessionLock::is_backed)
        {
            return Ok(false);
        }
        let Some(id) = self.state().id.clone() else {
            return Ok(false);
        };

        let operation_id = admitted.operation_id.clone();
        let payload = update_handoff_payload(&operation_id)?;
        let epoch = hrdr_tools::unix_now();

        let before_state = self.state().clone();
        let Some(result) = self.with_agent(|agent| {
            let before = agent.messages_owned();
            agent.append_successful_update(operation_id.clone())?;
            Ok::<_, anyhow::Error>((
                before,
                agent.messages_owned(),
                agent.cwd().display().to_string(),
            ))
        }) else {
            return Ok(false);
        };
        let (before, messages, cwd) = result?;
        let todos = self.state().todos.clone();
        let goals = self.state().goals.clone();
        let crons = self.state().crons.clone();
        self.state_mut()
            .sync_from(messages, todos, goals, crons, cwd);

        if let Err(error) = hrdr_app::Session::new(self.state().clone().persisted())
            .save(&id, self.unix_style_paths())
        {
            let _ = self.with_agent(|agent| agent.set_messages(before));
            *self.state_mut() = before_state;
            return Err(error);
        }
        if let Err(error) =
            self.append_handoff_record(&id, epoch, hrdr_agent::HandoffState::Prepare, &payload)
        {
            let _ = self.with_agent(|agent| agent.set_messages(before));
            *self.state_mut() = before_state.clone();
            let _ =
                hrdr_app::Session::new(before_state.persisted()).save(&id, self.unix_style_paths());
            return Err(error);
        }
        if let Err(error) =
            self.append_handoff_record(&id, epoch, hrdr_agent::HandoffState::Commit, &payload)
        {
            let _ = self.with_agent(|agent| agent.set_messages(before));
            *self.state_mut() = before_state;
            let _ =
                self.append_handoff_record(&id, epoch, hrdr_agent::HandoffState::Abort, &payload);
            return Err(error);
        }
        self.handoff_gate = true;

        let AdmittedUpdate { coordinator, .. } =
            self.admitted_update.take().expect("checked above");
        if let Err(launch_error) = coordinator.launch() {
            let _ = self.with_agent(|agent| agent.set_messages(before));
            *self.state_mut() = before_state;
            let restore = hrdr_app::Session::new(self.state().clone().persisted())
                .save(&id, self.unix_style_paths());
            let handoff = hrdr_agent::session_file_path(&self.current_cwd(), &id)
                .with_file_name(format!("{id}.handoff"));
            let clear = match std::fs::remove_file(&handoff) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
            self.handoff_gate = false;
            return match (restore, clear) {
                (Ok(_), Ok(())) => Err(launch_error.context("prepared update launch failed; handoff aborted")),
                (restore, clear) => Err(launch_error.context(format!(
                    "prepared update launch failed; restoring the pre-update session: {restore:?}; clearing the handoff journal: {clear:?}"
                ))),
            };
        }

        self.handoff_exit = true;
        self.should_quit = true;
        Ok(true)
    }

    /// The TUI half of the handoff gate. This runs entirely on the UI task; the
    /// registry supplies its own locked snapshot for agent-owned runtime state.
    pub(crate) fn handoff_quiescent(&self) -> bool {
        self.registry.is_handoff_quiescent()
            && self
                .background_tasks
                .lock()
                .map(|tasks| tasks.is_empty())
                .unwrap_or(false)
            && self.turn_handle.is_none()
            && self.quit_reap.is_none()
            && self.user_shell.is_none()
            && self.browser_login_task.is_none()
            && self.pending_compaction.is_none()
            && self
                .steering
                .lock()
                .map(|queue| queue.is_empty())
                .unwrap_or(false)
            && self.pending_save.is_none()
            && !self.save_in_flight
            && self.pending_reservation.is_none()
            && self
                .active_lock
                .as_ref()
                .is_some_and(|lock| lock.is_backed())
            && self.state().id.is_some()
            && self.composer_is_empty()
            && self.stash.is_empty()
            && self.pending_attachments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use hrdr_agent::{HandoffIntent, HandoffPayload};

    #[test]
    fn recovery_payload_parses_only_known_idle_and_update_intents() {
        assert_eq!(
            HandoffPayload::parse(
                br#"{"version":1,"intent":"idle","operation_id":null,"runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#
            )
            .unwrap(),
            HandoffIntent::Idle
        );
        assert_eq!(
            HandoffPayload::parse(
                br#"{"version":1,"intent":"update_continuation","operation_id":"op-1","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#
            )
            .unwrap(),
            HandoffIntent::UpdateContinuation
        );
    }

    #[test]
    fn recovery_payload_rejects_missing_unknown_and_active_fields() {
        for payload in [
            br#"{"version":1,"intent":"idle","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false}"# as &[u8],
            br#"{"version":1,"intent":"idle","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false,"extra":false}"#,
            br#"{"version":1,"intent":"idle","runtime_queue":true,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#,
            br#"{"version":1,"intent":"unknown","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#,
        ] {
            assert!(HandoffPayload::parse(payload).is_err(), "{payload:?}");
        }
    }
}
