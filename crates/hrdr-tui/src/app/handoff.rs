//! Internal checkpoint coordination for a quiescent saved TUI session.

use super::*;

#[allow(dead_code)]
const HANDOFF_PAYLOAD: &[u8] = br#"{"version":1,"runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#;

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
        journal.append(epoch, hrdr_agent::HandoffState::Prepare, HANDOFF_PAYLOAD)?;
        journal.append(epoch, hrdr_agent::HandoffState::Commit, HANDOFF_PAYLOAD)?;
        self.handoff_gate = true;
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
