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

    /// Commit a prepared update handoff, then let its prepared owner take over.
    ///
    /// The coordinator is prepared before this method mutates the app, and is
    /// consumed exactly once only after the durable commit and input gate.
    pub(crate) async fn commit_admitted_update(
        &mut self,
        admitted: AdmittedUpdate,
    ) -> anyhow::Result<bool> {
        if !self.handoff_quiescent() {
            return Ok(false);
        }
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

        let AdmittedUpdate {
            operation_id,
            coordinator,
        } = admitted;
        let Some(result) =
            self.with_agent(|agent| agent.append_successful_update(operation_id.clone()))
        else {
            return Ok(false);
        };
        result?;
        self.autosave();
        self.await_saves().await;
        if !self.handoff_quiescent() {
            return Ok(false);
        }

        let payload = update_handoff_payload(&operation_id)?;
        let lock = self
            .active_lock
            .as_ref()
            .expect("quiescent handoff retains a backed session lock");
        let journal = hrdr_agent::HandoffJournal::open(&self.current_cwd(), &id, lock)?;
        let epoch = hrdr_tools::unix_now();
        journal.append(epoch, hrdr_agent::HandoffState::Prepare, &payload)?;
        journal.append(epoch, hrdr_agent::HandoffState::Commit, &payload)?;
        self.handoff_gate = true;

        if let Err(launch_error) = coordinator.launch() {
            if let Err(abort_error) =
                journal.append(epoch, hrdr_agent::HandoffState::Abort, &payload)
            {
                return Err(launch_error.context(format!(
                    "prepared update launch failed and recording handoff abort failed: {abort_error}"
                )));
            }
            self.handoff_gate = false;
            return Err(launch_error.context("prepared update launch failed; handoff aborted"));
        }

        self.handoff_exit = true;
        self.should_quit = true;
        Ok(true)
    }

    /// The TUI half of the handoff gate. This runs entirely on the UI task; the
    /// registry supplies its own locked snapshot for agent-owned runtime state.
    fn handoff_quiescent(&self) -> bool {
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
