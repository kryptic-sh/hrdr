use std::sync::Arc;

use hrdr_agent::Agent;
use tokio::sync::Mutex;

/// The shared compaction core (`/compact` and threshold auto-compaction):
/// lock the agent and summarize. `Ok((before, after))` with `before == after`
/// means there was nothing to compact.
///
/// **Session-scoped.** Compaction manages the conversation the *user* owns and
/// returns to. It is refused for a delegated sub-agent: that history is
/// short-lived, nobody resumes it, and summarising it mid-task only costs a model
/// call and loses fidelity the parent is waiting on. This is the structural guard
/// — a frontend that lets you drive a sub-agent pane goes through here too, so it
/// cannot compact one by accident.
///
/// Overflow recovery is a different thing and still applies to sub-agents: the
/// agent compacts itself inside `connect_and_drain` when a request would
/// otherwise fail outright, rescuing the task rather than losing it.
pub async fn run_compaction(
    agent: Arc<Mutex<Agent>>,
    instructions: Option<String>,
) -> Result<(usize, usize), String> {
    let mut a = agent.lock().await;
    if a.is_subagent() {
        return Err(
            "a sub-agent's conversation is not compacted — it is transient, \
                    and its context is the parent's to manage"
                .to_string(),
        );
    }
    a.compact(instructions.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// The system line a finished compaction shows — identical in both frontends.
pub fn compaction_message(res: &Result<(usize, usize), String>) -> String {
    match res {
        Ok((before, after)) if before == after => "nothing to compact yet".to_string(),
        Ok((before, after)) => format!(
            "compacted: {before} → {after} messages (summary kept; scrollback above is \
             preserved for you)"
        ),
        Err(e) => format!("[compact failed] {e}"),
    }
}

/// The context-usage token count at which auto-compaction fires. Re-exported from
/// `hrdr-agent`, which owns the math — the agent compacts itself on the same
/// threshold, and two copies would drift.
pub use hrdr_agent::{compaction_trigger, should_auto_compact};
