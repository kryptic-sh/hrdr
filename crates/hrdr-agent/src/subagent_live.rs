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

/// The session's own agent, in the same registry as every delegated one.
///
/// The main agent is not a different *kind* of thing — it is the agent that
/// happens to have been there first. Registering it here is what lets a frontend
/// build its view the same way for all of them: replay the agent's record. Keys
/// from [`LiveAgents::next_key`] start at 1, so this never collides.
pub const MAIN_KEY: u64 = 0;

/// An agent's own record of what it has emitted, in order. Shared between the
/// running agent (which appends) and a frontend (which replays).
///
/// Consumed events are dropped once every reader has seen them
/// ([`LiveAgents::compact`]), so a long session does not retain every token delta
/// it ever streamed — `base` keeps cursors stable across that.
#[derive(Default)]
pub struct Events {
    events: std::collections::VecDeque<crate::AgentEvent>,
    /// Absolute index of `events[0]` — cursors are absolute, so compaction is
    /// invisible to a reader.
    base: usize,
}

impl Events {
    pub fn push(&mut self, ev: crate::AgentEvent) {
        self.events.push_back(ev);
    }

    /// Everything from absolute index `from`, and the cursor to resume at. A
    /// cursor behind what has been compacted away yields what is still held —
    /// never a panic, and never a replay of something already folded in.
    pub fn since(&self, from: usize) -> (Vec<crate::AgentEvent>, usize) {
        let start = from.saturating_sub(self.base).min(self.events.len());
        let tail = self.events.iter().skip(start).cloned().collect();
        (tail, self.base + self.events.len())
    }

    /// Drop everything before `upto` — the reader has folded it into its view.
    pub fn compact(&mut self, upto: usize) {
        let drop = upto.saturating_sub(self.base).min(self.events.len());
        self.events.drain(..drop);
        self.base += drop;
    }

    pub fn len(&self) -> usize {
        self.base + self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// An agent's event record, shared between the agent and its frontend.
pub type EventLog = Arc<Mutex<Events>>;

/// A fresh, empty event log.
pub fn event_log() -> EventLog {
    Arc::new(Mutex::new(Events::default()))
}

/// What happened to a prompt handed to an agent — see [`LiveSubagents::send_prompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// A turn was already in flight, so the prompt was injected into it.
    Steered,
    /// The agent was idle, so a fresh turn was started on it.
    StartedTurn,
}

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
    /// Endpoint it is talking to — it need not be the parent's (a sub-agent can
    /// be delegated to a different provider entirely).
    pub base_url: String,
    /// Its own token/cost counters, folded from every call it makes — by
    /// [`LiveSubagents::send_prompt`] for a turn the user drove, and by the `task`
    /// tool for the delegated run. A frontend showing this agent reads its usage
    /// from here; nothing has to be watching for the figures to be right.
    pub usage: crate::AgentUsage,
    /// **Everything this agent has emitted**, in order — the record a frontend
    /// replays to build its transcript ([`LiveSubagents::events_since`]).
    ///
    /// The agent keeps it itself because it is the agent's own history, and
    /// because the alternative does not work: a *background* sub-agent's `task`
    /// call returns the moment it is spawned, so there is no live tool call left
    /// to stream its output through — it emitted nothing to a frontend at all, and
    /// its pane stayed empty however long it worked. A blocking one streamed only
    /// flattened text through its parent's call, so its tool calls rendered as
    /// prose. Recording the events themselves fixes both, and means a frontend
    /// that attaches late still sees the whole run.
    pub events: EventLog,
    /// The clock on its current turn: how long the model has worked, its
    /// throughput, whether it is inferring or waiting on a tool. Every agent has
    /// turns, so every agent has one — a frontend showing this agent shows *its*
    /// loader, not the main agent's.
    pub turn: crate::TurnStats,
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
    ///
    /// The session's own agent is never disposable — it is the conversation.
    fn disposable(&self) -> bool {
        self.key != MAIN_KEY && !self.running && self.done && self.delivered && !self.pinned
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

    /// Register an agent.
    pub fn register(&self, entry: LiveSubagent) {
        self.with(|v| v.push(entry));
    }

    /// Register the session's own agent, so it is an entry in the registry like
    /// every delegated one — same record, same counters, same chrome.
    ///
    /// This is what makes a frontend able to stop special-casing it: it builds the
    /// main agent's view by replaying the main agent's record, exactly as it builds
    /// a sub-agent's. Idempotent.
    pub fn register_main(
        &self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        steering: SteeringQueue,
        model: String,
        provider: Option<String>,
        base_url: String,
        usage: crate::AgentUsage,
    ) {
        self.with(|v| {
            if v.iter().any(|e| e.key == MAIN_KEY) {
                return;
            }
            v.push(LiveSubagent {
                key: MAIN_KEY,
                bg_id: None,
                tool_id: None,
                label: "main".to_string(),
                model,
                provider,
                base_url,
                usage,
                events: event_log(),
                turn: crate::TurnStats::default(),
                kind: SubagentKind::Blocking,
                agent,
                steering,
                running: false,
                // The session's agent is never finished, never owed, never pruned.
                done: false,
                delivered: false,
                pinned: true,
            });
        });
    }

    /// Apply `f` to the entry with `key`, if it is still present.
    pub fn update(&self, key: u64, f: impl FnOnce(&mut LiveSubagent)) {
        self.with(|v| {
            if let Some(e) = v.iter_mut().find(|e| e.key == key) {
                f(e);
            }
        });
    }

    /// Record one of sub-agent `key`'s events against it: append it to the agent's
    /// event log, and fold it into the agent's usage counters.
    ///
    /// Every path a sub-agent's events travel calls this — its delegated run
    /// (blocking or background) and any later turn the user drives on it — so what
    /// the agent did and what it spent are recorded in one place, by the agent,
    /// with nothing required to be watching.
    pub fn record(&self, key: u64, ev: &crate::AgentEvent) {
        self.update(key, |e| {
            e.usage.record_event(ev);
            e.turn.record(ev);
            if let Ok(mut log) = e.events.lock() {
                log.push(ev.clone());
            }
        });
    }

    /// A turn is starting on agent `key`: start its clock.
    pub fn begin_turn(&self, key: u64) {
        self.update(key, |e| {
            e.running = true;
            e.turn.begin();
        });
    }

    /// Agent `key`'s turn is over: stop its clock.
    pub fn end_turn(&self, key: u64) {
        self.update(key, |e| e.turn.end());
    }

    /// A snapshot of `key`'s turn clock, for the frontend showing that agent.
    pub fn turn(&self, key: u64) -> Option<crate::TurnStats> {
        self.with(|v| v.iter().find(|e| e.key == key).map(|e| e.turn))
    }

    /// Agent `key`'s events from `from` onwards, and the new cursor.
    ///
    /// A frontend keeps a cursor per pane and folds what it hasn't seen yet, so a
    /// pane opened long after the agent started still shows the whole run — and one
    /// that was never opened costs nothing to keep up to date.
    pub fn events_since(&self, key: u64, from: usize) -> Option<(Vec<crate::AgentEvent>, usize)> {
        self.with(|v| {
            let e = v.iter().find(|e| e.key == key)?;
            let log = e.events.lock().ok()?;
            Some(log.since(from))
        })
    }

    /// Release agent `key`'s events before `upto` — its reader has folded them
    /// into its view and will never ask for them again.
    ///
    /// Without this the record is an ever-growing second copy of the transcript:
    /// one entry per streamed token delta, for the life of the session.
    pub fn compact(&self, key: u64, upto: usize) {
        self.update(key, |e| {
            if let Ok(mut log) = e.events.lock() {
                log.compact(upto);
            }
        });
    }

    /// A snapshot of `key`'s usage, for a frontend showing that agent.
    pub fn usage(&self, key: u64) -> Option<crate::AgentUsage> {
        self.with(|v| v.iter().find(|e| e.key == key).map(|e| e.usage))
    }

    /// The steering queue and agent handle for `key`, if it is still live.
    pub fn handle(&self, key: u64) -> Option<(Arc<tokio::sync::Mutex<Agent>>, SteeringQueue)> {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .map(|e| (Arc::clone(&e.agent), Arc::clone(&e.steering)))
        })
    }

    /// Send a user prompt to sub-agent `key`.
    ///
    /// The whole decision lives here, not in a frontend, because it is not a
    /// frontend's decision — it is the same rule for any agent, driven by anything:
    ///
    /// * a turn is **in flight** → the prompt is *steering*. It goes into the very
    ///   queue that agent's `run` is draining, so the model reads it before its next
    ///   request. Identical to steering the main agent.
    /// * the agent is **idle** (its delegated task already landed) → drive a **new
    ///   turn** on it. This is what retaining the agent was for: it is still alive
    ///   with its full history, so a further turn continues the conversation instead
    ///   of re-delegating from scratch.
    ///
    /// A frontend supplies only `on_event` — how to *surface* what comes back. It
    /// makes no routing decision and holds no rule of its own.
    ///
    /// `None` when the sub-agent has already been released (finished, delivered and
    /// pruned), so a caller can say so rather than swallow the prompt.
    pub fn send_prompt<F>(&self, key: u64, input: String, on_event: F) -> Option<PromptDelivery>
    where
        F: FnMut(crate::AgentEvent) + Send + 'static,
    {
        let (agent, steering, running) = self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .map(|e| (Arc::clone(&e.agent), Arc::clone(&e.steering), e.running))
        })?;

        if running {
            if let Ok(mut q) = steering.lock() {
                q.push_back(input);
            }
            return Some(PromptDelivery::Steered);
        }

        // Idle: a further turn on the agent we kept alive for exactly this.
        //
        // `run` emits `Steered` for a message it *drains* mid-turn, but nothing for
        // the input that opens a turn — so record it here. Without it the agent's
        // record shows the reply and not the question, and a pane rebuilt from that
        // record would too.
        self.record(key, &crate::AgentEvent::Steered(input.clone()));
        self.begin_turn(key);
        let live = self.clone();
        tokio::spawn(async move {
            // The guard marks it idle again on every exit — including cancellation,
            // where nothing after the await would run.
            let _guard = RunGuard::new(live.clone(), key);
            let mut on_event = on_event;
            // Recorded on the agent's own entry rather than by whoever is watching:
            // what a turn did and what it spent are facts about the agent, not
            // about the fact that someone happened to be looking at it.
            let mut on_event = move |ev: crate::AgentEvent| {
                live.record(key, &ev);
                on_event(ev);
            };
            let mut a = agent.lock().await;
            if let Err(e) = a.run(input, steering, &mut on_event).await {
                on_event(crate::AgentEvent::Notice(format!("[error] {e:#}")));
                // `run` only emits `TurnDone` on success; a frontend still needs to
                // know the turn is over.
                on_event(crate::AgentEvent::TurnDone);
            }
        });
        Some(PromptDelivery::StartedTurn)
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
            // Stop its clock too, or a cancelled agent's loader spins forever.
            e.turn.end();
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
            base_url: String::new(),
            usage: crate::AgentUsage::default(),
            events: event_log(),
            turn: crate::TurnStats::default(),
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

    /// The routing rule lives here, not in a frontend — so it is testable with no
    /// UI at all. A prompt to a *busy* agent steers the turn in flight: it goes
    /// into the very queue that agent's `run` is draining.
    #[tokio::test]
    async fn a_prompt_to_a_busy_agent_steers_the_turn_in_flight() {
        let live = LiveSubagents::new();
        live.register(entry(1)); // `entry` is running
        let steering = live.with(|v| Arc::clone(&v[0].steering));

        let delivery = live.send_prompt(1, "look at auth too".to_string(), |_| {});
        assert_eq!(delivery, Some(PromptDelivery::Steered));
        assert_eq!(
            steering.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec!["look at auth too".to_string()],
            "it reaches the queue the agent's run() drains"
        );
        assert!(
            live.with(|v| v[0].running),
            "steering does not start a second turn"
        );
    }

    /// A prompt to an *idle* agent starts a further turn on it. This is what
    /// retaining the agent was for: it is still alive with its history, so the
    /// conversation continues rather than being re-delegated from scratch.
    #[tokio::test]
    async fn a_prompt_to_an_idle_agent_starts_a_further_turn_on_it() {
        let live = LiveSubagents::new();
        live.register(entry(1));
        // Its delegated task has landed.
        live.update(1, |e| {
            e.running = false;
            e.done = true;
        });

        let delivery = live.send_prompt(1, "now summarise".to_string(), |_| {});
        assert_eq!(
            delivery,
            Some(PromptDelivery::StartedTurn),
            "an idle agent is driven, not steered into a void"
        );
        assert!(
            live.with(|v| v[0].running),
            "and it is marked busy, so the next prompt steers instead"
        );
        // The turn itself runs against an unreachable endpoint and fails; the
        // RunGuard is what returns it to idle, which the cancellation test covers.
    }

    /// A prompt to an agent that has already been released reports that, so a
    /// caller can say so rather than silently swallowing what the user typed.
    #[test]
    fn a_prompt_to_a_released_agent_is_reported_not_swallowed() {
        let live = LiveSubagents::new();
        assert!(live.send_prompt(99, "hello?".to_string(), |_| {}).is_none());
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

    /// A sub-agent records everything it emits on its own entry, and a frontend
    /// replays it from a cursor. This is what a pane is built from.
    ///
    /// Regression: nothing recorded a sub-agent's events, so a frontend could only
    /// see what it happened to be listening for. For a *background* sub-agent that
    /// was nothing at all — its `task` call returns the moment it is spawned, so
    /// there is no live tool call left to stream through — and background is the
    /// default. Its pane stayed empty however long it worked.
    ///
    /// Replaying from the agent's own record also means a pane opened late still
    /// shows the whole run, which is the normal case: you click a sub-agent's row
    /// *because* you noticed it working.
    #[test]
    fn an_agents_events_are_recorded_on_it_and_replayed_from_a_cursor() {
        let live = LiveSubagents::new();
        live.register(entry(1));

        live.record(1, &crate::AgentEvent::Text("looking".into()));
        live.record(1, &crate::AgentEvent::Text(" around".into()));

        // A frontend attaching now — long after the work started — gets all of it.
        let (events, cursor) = live.events_since(1, 0).expect("a live agent has a record");
        assert_eq!(events.len(), 2, "the whole run, not just what came after");
        assert_eq!(cursor, 2);

        // From its cursor it sees only what is new.
        live.record(1, &crate::AgentEvent::Text(" done".into()));
        let (events, cursor) = live.events_since(1, cursor).unwrap();
        assert!(
            matches!(&events[..], [crate::AgentEvent::Text(t)] if t == " done"),
            "only the unseen tail is replayed"
        );
        assert_eq!(cursor, 3);
        assert!(
            live.events_since(1, cursor).unwrap().0.is_empty(),
            "and nothing is replayed twice"
        );

        assert!(
            live.events_since(99, 0).is_none(),
            "an unknown key has none"
        );
    }

    /// A record whose reader has folded everything in is released — otherwise it is
    /// a second copy of the transcript, one entry per streamed token delta, kept for
    /// the life of the session. Cursors are absolute, so compaction is invisible to
    /// the reader.
    #[test]
    fn a_folded_in_record_is_released_without_disturbing_the_cursor() {
        let live = LiveSubagents::new();
        live.register(entry(1));
        live.record(1, &crate::AgentEvent::Text("a".into()));
        live.record(1, &crate::AgentEvent::Text("b".into()));

        let (_, cursor) = live.events_since(1, 0).unwrap();
        assert_eq!(cursor, 2);
        live.compact(1, cursor);
        assert!(
            live.with(|v| v[0].events.lock().unwrap().is_empty()),
            "what has been folded in is not kept"
        );

        // The cursor still means the same thing: nothing is replayed, and new work
        // is picked up exactly once.
        assert!(live.events_since(1, cursor).unwrap().0.is_empty());
        live.record(1, &crate::AgentEvent::Text("c".into()));
        let (events, cursor) = live.events_since(1, cursor).unwrap();
        assert!(
            matches!(&events[..], [crate::AgentEvent::Text(t)] if t == "c"),
            "the tail after a compaction is still the tail"
        );
        assert_eq!(cursor, 3, "cursors stay absolute across compaction");
    }

    /// A sub-agent's tokens are a fact about *that agent*, so they are counted on
    /// its registry entry — not in whichever frontend happens to be showing it.
    /// A status bar reading the agent you are looking at reads these.
    #[test]
    fn a_sub_agents_calls_are_counted_on_its_own_entry() {
        let live = LiveSubagents::new();
        live.register(entry(1));
        live.register(entry(2));
        live.record(
            1,
            &crate::AgentEvent::Usage {
                prompt_tokens: 120,
                completion_tokens: 30,
                cached_prompt_tokens: None,
                reasoning_tokens: None,
                cost_usd: None,
                session_cost_usd: Some(0.02),
            },
        );
        // Everything else in the stream leaves the counters alone.
        live.record(1, &crate::AgentEvent::Text("hi".into()));

        let u = live.usage(1).expect("a live sub-agent has usage");
        assert_eq!((u.tokens_in, u.tokens_out), (120, 30));
        assert_eq!(u.ctx_used(), 120, "its own context, not the parent's");
        assert_eq!(u.cost_usd, 0.02);
        assert_eq!(
            live.usage(2).unwrap().tokens_in,
            0,
            "one sub-agent's tokens are not another's"
        );
        assert!(live.usage(99).is_none());
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
