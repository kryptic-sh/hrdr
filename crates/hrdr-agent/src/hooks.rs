//! Lifecycle event hooks — extracted from [`Agent`] into its own module to keep
//! `lib.rs` manageable.

use crate::{Agent, AgentEvent};

impl Agent {
    /// Whether any lifecycle hook is registered for `event` — the cheap check
    /// that keeps the hookless common path free of payload building.
    pub(crate) fn has_event_hooks(&self, event: hrdr_tools::HookEvent) -> bool {
        self.event_hooks.iter().any(|h| h.event == event)
    }

    /// Run every registered hook for `event` with the shared
    /// `{"event", "cwd", "model"}` payload, returning the failure notes (blocked
    /// output first, then stderr/notes). Keeps the hookless early-out, so the
    /// common path never builds the payload.
    async fn run_hooks(&self, event: hrdr_tools::HookEvent) -> Vec<String> {
        if !self.has_event_hooks(event) {
            return Vec::new();
        }
        let payload = serde_json::json!({
            "event": event.as_str(),
            "cwd": self.ctx.cwd.display().to_string(),
            "model": self.client.model,
        });
        let out =
            hrdr_tools::run_event_hooks(&self.event_hooks, event, None, &payload, &self.ctx.cwd)
                .await;
        out.notes.into_iter().chain(out.block).collect()
    }

    /// Run the `turn_end` hooks (both turn exits call this just before
    /// `TurnDone`). Failures surface as notices; nothing here can block.
    pub(crate) async fn fire_turn_end_hooks<F: FnMut(AgentEvent)>(&self, on_event: &mut F) {
        for note in self.run_hooks(hrdr_tools::HookEvent::TurnEnd).await {
            on_event(AgentEvent::Notice(note));
        }
    }

    /// Run the `session_start`/`session_end` hooks — driven by the frontend
    /// (the agent doesn't know when a session opens or the app quits). Returns
    /// the failure notes for the frontend to display.
    pub async fn run_session_hooks(&self, event: hrdr_tools::HookEvent) -> Vec<String> {
        self.run_hooks(event).await
    }
}
