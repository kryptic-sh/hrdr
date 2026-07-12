//! Live sub-agents: the delegated agents a frontend can *address* — steer them
//! mid-turn, watch their output, or drive another turn on one after its task has
//! landed.
//!
//! A delegated sub-agent used to be unreachable: `SubagentTool` built it inside
//! its spawned task, handed `run` a throwaway steering queue, and dropped the
//! whole `Agent` when the task ended. Its output survived only as a flat log
//! string. This registry retains the `Agent` itself, along with the very steering
//! queue its `run` is draining, so the frontend can treat a sub-agent the way it
//! treats the main one.
//!
//! **Retention.** A sub-agent is kept while it is running, while its result is
//! still owed to the main agent, or while a frontend has [`pinned`] it (because
//! the user is looking at it). Once it is finished, delivered, and unpinned, it
//! is pruned — see [`LiveSubagents::prune`]. The prune runs inside the agent, so
//! a frontend that never pins (the headless CLI, a test) cannot leak agents by
//! simply not participating.
//!
//! [`pinned`]: LiveSubagent::pinned

use std::sync::{Arc, Mutex};

use crate::{Agent, SteeringQueue};

/// Monotonic key source for live sub-agents. Distinct from `BG_SEQ` (which
/// numbers *background* runs, and which the model sees as `task#N`): this keys
/// every sub-agent, blocking or background, so a frontend has one identity space
/// for its panes.
static LIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How a sub-agent was delegated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentKind {
    /// The `task` call blocks on it; its answer becomes the tool result.
    Blocking,
    /// Detached (`background: true`); its answer is delivered later.
    Background,
}

/// A delegated sub-agent the frontend can address.
///
/// Deliberately not `Debug`: it holds an `Agent`, whose config carries an API
/// key.
pub struct LiveSubagent {
    /// Frontend-facing identity, unique across blocking and background runs.
    pub key: u64,
    /// The background run id (`task#N`) the model sees, when this is a background
    /// run. `None` for a blocking sub-agent, which the model never names.
    pub bg_id: Option<u64>,
    /// The `task` tool call that spawned it, when there was one.
    pub tool_id: Option<String>,
    pub label: String,
    pub model: String,
    /// Provider this sub-agent runs on, when it names one. Shown on the parent's
    /// `task` block, which reports *what was delegated to* rather than replaying
    /// the sub-agent's output.
    pub provider: Option<String>,
    pub kind: SubagentKind,
    /// The sub-agent itself, retained so a frontend can drive a further turn on
    /// it once its delegated task has landed.
    pub agent: Arc<tokio::sync::Mutex<Agent>>,
    /// The steering queue its `run` is draining. Push here to inject a message
    /// into the turn already in flight.
    pub steering: SteeringQueue,
    /// A turn is in flight on this sub-agent (its delegated task, or one the user
    /// drove from its view).
    pub running: bool,
    /// Its delegated task has finished.
    pub done: bool,
    /// Its result has reached the main agent (a blocking tool result, or a
    /// delivered background answer). Until then it is owed and must be kept.
    pub delivered: bool,
    /// A frontend is displaying it, so it must be kept even once finished and
    /// delivered. The frontend clears this when it stops showing it.
    pub pinned: bool,
}

impl LiveSubagent {
    /// Whether this entry may be dropped: its work is done, the main agent has
    /// its result, and nobody is looking at it.
    fn disposable(&self) -> bool {
        !self.running && self.done && self.delivered && !self.pinned
    }
}

/// The set of live sub-agents, shared between the agent and its frontend.
#[derive(Clone, Default)]
pub struct LiveSubagents(Arc<Mutex<Vec<LiveSubagent>>>);

impl LiveSubagents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next frontend key.
    pub fn next_key() -> u64 {
        LIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Run `f` over the entries under the lock. A poisoned lock is recovered
    /// rather than propagated: losing the pane list must never fail a turn.
    pub fn with<R>(&self, f: impl FnOnce(&mut Vec<LiveSubagent>) -> R) -> R {
        let mut v = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut v)
    }

    /// Register a sub-agent at spawn.
    pub fn register(&self, entry: LiveSubagent) {
        self.with(|v| v.push(entry));
    }

    /// Apply `f` to the entry with `key`, if it is still present.
    pub fn update(&self, key: u64, f: impl FnOnce(&mut LiveSubagent)) {
        self.with(|v| {
            if let Some(e) = v.iter_mut().find(|e| e.key == key) {
                f(e);
            }
        });
    }

    /// The steering queue and agent handle for `key`, if it is still live.
    pub fn handle(&self, key: u64) -> Option<(Arc<tokio::sync::Mutex<Agent>>, SteeringQueue)> {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .map(|e| (Arc::clone(&e.agent), Arc::clone(&e.steering)))
        })
    }

    /// Drop every entry that is finished, delivered, and unpinned. Called by the
    /// agent at turn end, so a frontend that never pins cannot leak sub-agents.
    pub fn prune(&self) {
        self.with(|v| v.retain(|e| !e.disposable()));
    }

    /// How many entries are currently retained (tests, `/doctor`).
    pub fn len(&self) -> usize {
        self.with(|v| v.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Age out finished TODO items in place. Stamps each completed item with the
/// `turn` it was first seen finished (in `stamps`, keyed by content), then drops
/// any completed item that has been finished for `ttl` turns. Stamps for items no
/// longer present as completed are forgotten, so a re-completed item ages from
/// scratch. Pending / in-progress items are kept.
///
/// This lives beside the agent, not in a frontend, because the TODO list is the
/// agent's own state: the model reads it every turn. Ageing it only in the TUI
/// meant a headless run — and every delegated sub-agent — accumulated completed
/// items forever, growing the context they read from.
pub fn age_completed_todos(
    todos: &mut Vec<hrdr_tools::TodoItem>,
    stamps: &mut std::collections::HashMap<String, u64>,
    turn: u64,
    ttl: u64,
) {
    for t in todos.iter() {
        if t.status == "completed" {
            stamps.entry(t.content.clone()).or_insert(turn);
        }
    }
    todos.retain(|t| {
        t.status != "completed"
            || stamps
                .get(&t.content)
                .is_none_or(|&done| turn.saturating_sub(done) < ttl)
    });
    stamps.retain(|content, _| {
        todos
            .iter()
            .any(|t| t.status == "completed" && &t.content == content)
    });
}

/// Marks a sub-agent idle on **every** exit path — including task cancellation,
/// where the code after `run(...).await` simply never executes.
///
/// Without this, a cancelled turn (`/new`, Esc, quit) leaves its sub-agents stuck
/// at `running: true, done: false`: never `disposable()`, so retained forever,
/// each still holding an `Agent` — and shown to the user as a live pane they can
/// "steer", with nothing on the other end.
pub struct RunGuard {
    live: LiveSubagents,
    key: u64,
}

impl RunGuard {
    pub fn new(live: LiveSubagents, key: u64) -> Self {
        Self { live, key }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.live.update(self.key, |e| {
            e.running = false;
            e.done = true;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentConfig, steering_queue};

    fn entry(key: u64) -> LiveSubagent {
        let agent = Agent::new(AgentConfig {
            checkpoints: Some("off".to_string()),
            ..Default::default()
        })
        .unwrap();
        LiveSubagent {
            key,
            bg_id: None,
            tool_id: None,
            label: "l".to_string(),
            model: "m".to_string(),
            provider: None,
            kind: SubagentKind::Blocking,
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            steering: steering_queue(),
            running: true,
            done: false,
            delivered: false,
            pinned: false,
        }
    }

    #[test]
    fn prune_keeps_running_owed_and_pinned_entries() {
        let live = LiveSubagents::new();

        // Still running → kept.
        live.register(entry(1));
        // Finished but its result is still owed to the main agent → kept.
        live.register(entry(2));
        live.update(2, |e| {
            e.running = false;
            e.done = true;
        });
        // Finished and delivered, but the user is looking at it → kept.
        live.register(entry(3));
        live.update(3, |e| {
            e.running = false;
            e.done = true;
            e.delivered = true;
            e.pinned = true;
        });
        // Finished, delivered, unwatched → dropped.
        live.register(entry(4));
        live.update(4, |e| {
            e.running = false;
            e.done = true;
            e.delivered = true;
        });

        live.prune();
        let keys = live.with(|v| v.iter().map(|e| e.key).collect::<Vec<_>>());
        assert_eq!(keys, vec![1, 2, 3], "only the disposable entry is dropped");

        // Unpin the one being viewed: now it too is disposable.
        live.update(3, |e| e.pinned = false);
        live.prune();
        let keys = live.with(|v| v.iter().map(|e| e.key).collect::<Vec<_>>());
        assert_eq!(keys, vec![1, 2]);
    }

    #[test]
    fn keys_are_unique_across_runs() {
        let a = LiveSubagents::next_key();
        let b = LiveSubagents::next_key();
        assert_ne!(a, b);
    }

    /// The TODO list is agent state the model re-reads every turn, so ageing it is
    /// the agent's job. It used to happen only in the TUI — meaning a headless run
    /// and every delegated sub-agent kept their finished items forever and paid for
    /// them in context on every request.
    #[test]
    fn completed_todos_age_out_after_their_ttl() {
        use hrdr_tools::TodoItem;
        let todo = |content: &str, status: &str| TodoItem {
            content: content.to_string(),
            status: status.to_string(),
        };
        let mut todos = vec![
            todo("done thing", "completed"),
            todo("still going", "in_progress"),
        ];
        let mut stamps = std::collections::HashMap::new();

        // First seen finished on turn 1, with a 2-turn lifetime.
        age_completed_todos(&mut todos, &mut stamps, 1, 2);
        assert_eq!(todos.len(), 2, "a freshly finished item still shows");

        age_completed_todos(&mut todos, &mut stamps, 2, 2);
        assert_eq!(todos.len(), 2, "and lingers for its ttl");

        age_completed_todos(&mut todos, &mut stamps, 3, 2);
        assert_eq!(todos.len(), 1, "then it is aged out");
        assert_eq!(todos[0].content, "still going");
        assert!(
            stamps.is_empty(),
            "and its stamp is forgotten, so a re-completed item ages from scratch"
        );

        // Unfinished work is never aged out, however long it takes.
        age_completed_todos(&mut todos, &mut stamps, 99, 2);
        assert_eq!(todos.len(), 1, "in-progress work is not swept away");
    }

    /// A cancelled run must not strand its sub-agent. The update after `.await`
    /// never runs when the task is aborted, so the guard has to do it on drop.
    #[test]
    fn a_cancelled_run_still_releases_its_sub_agent() {
        let live = LiveSubagents::new();
        live.register(entry(1));
        {
            let _guard = RunGuard::new(live.clone(), 1);
            // ...task is aborted here: nothing after this point would have run.
        }
        let (running, done) = live.with(|v| (v[0].running, v[0].done));
        assert!(!running, "a cancelled sub-agent is not still running");
        assert!(done, "and it is finished, not stuck in flight");

        // Its answer never reached the main agent, so it is still owed and kept —
        // but once delivery is moot it becomes collectable rather than immortal.
        live.prune();
        assert_eq!(live.len(), 1, "undelivered work is still held");
        live.update(1, |e| e.delivered = true);
        live.prune();
        assert!(live.is_empty(), "and then it is released, not leaked");
    }

    #[tokio::test]
    async fn handle_exposes_the_retained_agent_and_its_steering_queue() {
        let live = LiveSubagents::new();
        live.register(entry(7));
        let (agent, steering) = live.handle(7).expect("a live sub-agent is addressable");
        // The queue is the one `run` drains, so a push is a mid-turn injection.
        steering.lock().unwrap().push_back("steer me".to_string());
        assert_eq!(steering.lock().unwrap().len(), 1);
        // And the agent itself is reachable for a further turn.
        assert!(agent.try_lock().is_ok());
        assert!(live.handle(999).is_none(), "an unknown key is not a handle");
    }
}
