//! The agent registry: every agent in the session — the one the user talks to
//! and every agent it delegated to — as entries of one kind, in one list.
//!
//! A frontend *addresses* an agent through here: steer it mid-turn, watch its
//! output, drive another turn on it. Which agent it is changes nothing about how
//! that works, which is the point of the registry existing. The session's own
//! agent is the entry at [`MAIN_KEY`]; a delegated one takes the next key. The
//! only asymmetries are its retention (below) and what a delegated agent's
//! *configuration* gives it — never a second code path.
//!
//! A delegated agent used to be unreachable: `SubagentTool` built it inside its
//! spawned task, handed `run` a throwaway steering queue, and dropped the whole
//! `Agent` when the task ended. Its output survived only as a flat log string.
//! The registry retains the `Agent` itself, along with the very steering queue
//! its `run` is draining, so every agent is as addressable as the first one.
//!
//! **Retention.** A delegated agent is kept while it is running, while its
//! result is still owed to its caller, or while a frontend has [`pinned`] it
//! (because the user is looking at it). Once it is finished, delivered, and
//! unpinned, it is pruned — see [`AgentRegistry::prune`]. The prune runs inside
//! the agent, so a frontend that never pins (the headless CLI, a test) cannot
//! leak agents by simply not participating. The session's own agent is never
//! pruned: [`AgentEntry::owns_session`] is what says so, in one place.
//!
//! [`pinned`]: AgentEntry::pinned

use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use tokio::task::JoinHandle;

use crate::{Agent, SteeringQueue};

/// Monotonic key source for delegated agents. Distinct from `BG_SEQ` (which
/// numbers *background* runs, and which the model sees as `task#N`): this keys
/// every agent, blocking or background, so a frontend has one identity space
/// for its panes.
static LIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The session's own agent, in the same registry as every delegated one.
///
/// It is not a different *kind* of thing — it is the agent that happens to have
/// been there first. Registering it here is what lets a frontend build its view
/// the same way for all of them: replay the agent's record. Keys from
/// [`AgentRegistry::next_key`] start at 1, so this never collides.
pub const MAIN_KEY: u64 = 0;

/// An agent's own record of what it has emitted, in order. Shared between the
/// running agent (which appends) and a frontend (which replays).
///
/// Consumed events are dropped once every reader has seen them
/// ([`Events::compact`]), so a long session does not retain every token delta
/// it ever streamed — `base` keeps cursors stable across that.
#[derive(Default)]
pub struct Events {
    events: std::collections::VecDeque<crate::AgentEvent>,
    /// Absolute index of `events[0]` — cursors are absolute, so compaction is
    /// invisible to a reader.
    base: usize,
    /// The tool calls started here and not yet ended, `(id, name)`, in start
    /// order.
    ///
    /// Kept beside the log rather than derived from it, because the log is not a
    /// durable record: a frontend that keeps up compacts each event away as it
    /// folds it ([`Events::compact`]), so by the time a turn crashes the
    /// `ToolStart` whose entry is still spinning has usually already been
    /// dropped. This index survives that, which is what lets a crashed turn
    /// close the calls it left open (see [`AgentRegistry::open_tool_ends`]).
    open_tools: Vec<(String, String)>,
}

impl Events {
    pub fn push(&mut self, ev: crate::AgentEvent) {
        match &ev {
            crate::AgentEvent::ToolStart { id, name, .. } => {
                self.open_tools.push((id.clone(), name.clone()));
            }
            crate::AgentEvent::ToolEnd { id, .. } => self.open_tools.retain(|(i, _)| i != id),
            _ => {}
        }
        self.events.push_back(ev);
    }

    /// The tool calls still running, in start order. A round can put several in
    /// flight at once — a batch of read-only calls runs concurrently — so this
    /// is a list, not a single call.
    pub fn open_tools(&self) -> &[(String, String)] {
        &self.open_tools
    }

    /// Forget what was in flight — a fresh turn is starting on this agent, and
    /// nothing from the last one is still running (see
    /// [`AgentRegistry::begin_turn`]).
    pub fn clear_open_tools(&mut self) {
        self.open_tools.clear();
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

/// What happened to a prompt handed to an agent — see [`AgentRegistry::send_prompt`].
#[derive(Debug)]
pub enum PromptDelivery {
    /// A turn was already in flight, so the prompt was injected into it.
    Steered,
    /// The agent was idle, so a fresh turn was started on it. The handle aborts
    /// that turn, so a frontend can offer cancellation on any agent's turn.
    StartedTurn(JoinHandle<()>),
}

impl PromptDelivery {
    /// Whether a fresh turn was started (rather than the prompt being steered into
    /// one already running).
    pub fn started_turn(&self) -> bool {
        matches!(self, Self::StartedTurn(_))
    }
}

/// How a turn ended, for whoever asked for it.
#[derive(Debug, Default)]
pub struct TurnOutcome {
    /// What went wrong, rendered for the user. `None` when the turn completed.
    pub error: Option<String>,
    /// The turn **panicked** rather than failing cleanly. A frontend that shares
    /// the terminal with the panic hook needs to know: the hook has already left
    /// the alt screen, so the display must be restored before drawing again.
    pub panicked: bool,
}

/// Best-effort text of a caught panic payload, for turning a crashed turn into a
/// reported error instead of a hung spinner.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// The decision `send_prompt` reaches while holding the entry lock: the entry is
/// gone, the steer was enqueued on a running turn, or the agent is idle and a
/// fresh turn must be started (with `input` handed back so the spawn happens
/// outside the lock). Carrying `input` back out keeps the running-check and the
/// enqueue atomic without holding the lock across the turn spawn.
enum SendOutcome {
    Unknown,
    Steered,
    Idle {
        agent: Arc<tokio::sync::Mutex<crate::Agent>>,
        steering: crate::SteeringQueue,
        input: crate::Steer,
    },
}

/// One agent the frontend can address — the session's own, or a delegated one.
///
/// Deliberately not `Debug`: it holds an `Agent`, whose config carries an API
/// key.
pub struct AgentEntry {
    /// Frontend-facing identity, unique across every agent in the session.
    pub key: u64,
    /// The background run id (`task#N`) the model sees, when this is a background
    /// run. `None` otherwise — a blocking or session agent the model never names.
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
    /// Reasoning effort in force for *this* agent.
    pub effort: Option<String>,
    /// Whether this agent compacts itself when its context fills, and the buffer it
    /// keeps below the window. Both are per-agent — a sub-agent on a small local
    /// model has its own — and they set where its context gauge turns red.
    pub auto_compact: bool,
    pub compaction_reserved: u32,
    /// The filesystem confinement this agent's tools enforce — what a frontend
    /// shows as the sandbox badge on the status bar. Set once from the agent's
    /// policy at registration; it is fixed for the agent's lifetime.
    pub sandbox: hrdr_tools::SandboxMode,
    /// This agent's TODO list — the live one its `todo` tool writes. Every agent has
    /// its own; a frontend showing this agent shows *its* list.
    pub todos: Arc<Mutex<Vec<hrdr_tools::TodoItem>>>,
    /// Its own token/cost counters, folded from every call it makes — by
    /// [`AgentRegistry::send_prompt`] for a turn the user drove, and by the `task`
    /// tool for the delegated run. A frontend showing this agent reads its usage
    /// from here; nothing has to be watching for the figures to be right.
    pub usage: crate::AgentUsage,
    /// **Everything this agent has emitted**, in order — the record a frontend
    /// replays to build its transcript ([`AgentRegistry::events_since`]).
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
    /// Whether the model's thinking stream is open — the last recorded event
    /// was a `Reasoning` delta. A notice arriving while it is open is deferred
    /// (see [`AgentEntry::pending_notices`]) so the thinking block is never
    /// split into two running halves.
    pub reasoning_open: bool,
    /// Notices that arrived while the thinking stream was open, in order.
    /// Written to the event log and the durable transcript together, just
    /// before whatever event closes the stream, so the record reads
    /// `[reasoning][notice][closing]` and the fold shows one complete thought
    /// with the notice below it.
    pub pending_notices: Vec<crate::AgentEvent>,
    /// The clock on its current turn: how long the model has worked, its
    /// throughput, whether it is inferring or waiting on a tool. Every agent has
    /// turns, so every agent has one — a frontend showing this agent shows *its*
    /// loader, not the main agent's.
    pub turn: crate::TurnStats,
    /// The agent itself, retained so a frontend can drive a further turn on it
    /// once its delegated task has landed.
    pub agent: Arc<tokio::sync::Mutex<Agent>>,
    /// The steering queue its `run` is draining. Push here to inject a message
    /// into the turn already in flight.
    pub steering: SteeringQueue,
    /// A turn is in flight on this agent (a delegated task, or one the user drove
    /// from its view).
    pub running: bool,
    /// It is summarizing its own context — a `/compact`, or a self-compaction it
    /// decided on when its window filled. The agent sets this: it is the one that
    /// knows, and a sub-agent compacting itself on a small local model is exactly as
    /// worth showing as the main agent doing it.
    pub compacting: bool,
    /// Its delegated task has finished.
    pub done: bool,
    /// Its result has reached the main agent (a blocking tool result, or a
    /// delivered background answer). Until then it is owed and must be kept.
    pub delivered: bool,
    /// A frontend is displaying it, so it must be kept even once finished and
    /// delivered. The frontend clears this when it stops showing it.
    pub pinned: bool,
    /// This agent's durable transcript writer (append-only jsonl), when it has
    /// one. Every event recorded against this agent — its delegated run OR a
    /// steered turn — is appended here, so the on-disk transcript is complete
    /// regardless of which path drove the turn.
    pub transcript: Option<std::sync::Arc<std::sync::Mutex<crate::transcript_log::TranscriptLog>>>,
}

impl AgentEntry {
    /// This is the agent the session belongs to. **The one asymmetry**, stated
    /// here so nobody restates it: it is never pruned and never unpinned, because
    /// it is the conversation rather than a task within it. Everything else about
    /// an agent is its configuration.
    pub fn owns_session(&self) -> bool {
        self.key == MAIN_KEY
    }

    /// Whether this entry may be dropped: its work is done, its caller has its
    /// result, and nobody is looking at it.
    fn disposable(&self) -> bool {
        !self.owns_session() && !self.running && self.done && self.delivered && !self.pinned
    }
}

/// Every agent in the session, shared between the agents and their frontend.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    entries: Arc<Mutex<Vec<AgentEntry>>>,
    /// Which turn currently owns each agent, as a monotonic counter bumped by
    /// [`Self::begin_turn`].
    ///
    /// A cancelled turn's [`RunGuard`] does not run at `abort()` — it runs
    /// whenever the runtime next polls the aborted task, which can be *after*
    /// its replacement has already started. Without this, the dying turn's
    /// guard marks the agent idle while the live one is mid-flight: the loader
    /// stops, the status bar lies, and `send_prompt` takes its idle branch and
    /// starts a second concurrent turn on the one agent. The guard carries the
    /// generation it was born with and stands down when it no longer matches.
    turns: Arc<Mutex<std::collections::HashMap<u64, u64>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next frontend key.
    pub fn next_key() -> u64 {
        LIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Run `f` over the entries under the lock. A poisoned lock is recovered
    /// rather than propagated: losing the pane list must never fail a turn.
    pub fn with<R>(&self, f: impl FnOnce(&mut Vec<AgentEntry>) -> R) -> R {
        let mut v = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut v)
    }

    /// The generation of the turn that currently owns agent `key`.
    fn turn_gen(&self, key: u64) -> u64 {
        let g = self.turns.lock().unwrap_or_else(|p| p.into_inner());
        g.get(&key).copied().unwrap_or(0)
    }

    /// Register an agent.
    pub fn register(&self, entry: AgentEntry) {
        self.with(|v| v.push(entry));
    }

    /// Register the session's own agent, so it is an entry in the registry like
    /// every delegated one — same record, same counters, same chrome.
    ///
    /// This is what makes a frontend able to stop special-casing it: it builds
    /// this agent's view by replaying its record, exactly as it builds any
    /// other's. Idempotent.
    pub fn register_session(
        &self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        steering: SteeringQueue,
        model: String,
        provider: Option<String>,
        base_url: String,
        usage: crate::AgentUsage,
    ) {
        // The sandbox policy is fixed for the agent's lifetime, so capture the
        // mode once here; a busy lock (impossible at registration time) falls
        // back to the unconfined default rather than failing the call.
        let sandbox = agent
            .try_lock()
            .map(|a| a.sandbox_policy().mode)
            .unwrap_or(hrdr_tools::SandboxMode::None);
        self.with(|v| {
            if v.iter().any(|e| e.key == MAIN_KEY) {
                return;
            }
            v.push(AgentEntry {
                key: MAIN_KEY,
                bg_id: None,
                tool_id: None,
                label: "main".to_string(),
                model,
                provider,
                base_url,
                // Republished by the agent itself the moment it attaches
                // (`Agent::attach_live`) — it is the source of these, not us.
                effort: None,
                auto_compact: true,
                compaction_reserved: 0,
                sandbox,
                todos: Default::default(),
                usage,
                events: event_log(),
                reasoning_open: false,
                pending_notices: Vec::new(),
                turn: crate::TurnStats::default(),
                // Nobody delegated to it and nothing waits on its answer — the
                // reason `done`/`delivered` below are meaningless for it.
                agent,
                steering,
                running: false,
                compacting: false,
                // The session's agent is never finished, never owed, never pruned
                // (`AgentEntry::owns_session`).
                done: false,
                delivered: false,
                pinned: true,
                // Attached later via `attach_transcript`, once the session id is
                // assigned and its jsonl path is known — the same durable
                // event-fold transcript every delegated agent gets. `None` until
                // then (a brand-new session before its first save has no id yet).
                transcript: None,
            });
        });
    }

    /// Apply `f` to the entry with `key`, if it is still present.
    pub fn update(&self, key: u64, f: impl FnOnce(&mut AgentEntry)) {
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
        // Record usage/turn/events under the registry lock; the transcript
        // writer is cloned out so the file I/O below runs outside it.
        let out = self.with(|v| {
            v.iter_mut().find(|e| e.key == key).and_then(|e| {
                e.usage.record_event(ev);
                e.turn.record(ev);
                let mut to_write: Vec<crate::AgentEvent> = Vec::new();
                match ev {
                    // A reasoning delta opens — or continues — the model's
                    // thinking stream.
                    crate::AgentEvent::Reasoning(_) => {
                        e.reasoning_open = true;
                        to_write.push(ev.clone());
                    }
                    // A notice landing mid-thought must not split the thinking
                    // block: hold it and write it when the stream ends, just
                    // before whatever closes it, so the record reads
                    // `[reasoning][notice][closing]` and the fold shows one
                    // complete thought with the notice below it.
                    crate::AgentEvent::Notice(_) if e.reasoning_open => {
                        e.pending_notices.push(ev.clone());
                    }
                    _ => {
                        // The stream is over: flush any held notices ahead of
                        // the event that closed it.
                        if e.reasoning_open || !e.pending_notices.is_empty() {
                            e.reasoning_open = false;
                            to_write.append(&mut e.pending_notices);
                        }
                        to_write.push(ev.clone());
                    }
                }
                if let Ok(mut log) = e.events.lock() {
                    for w in &to_write {
                        log.push(w.clone());
                    }
                }
                e.transcript.clone().map(|t| (t, to_write))
            })
        });
        // Append to the agent's durable transcript, if it has one. This is
        // what makes the on-disk jsonl complete across BOTH a sub-agent's
        // delegated run and any later steered turn: every event travels
        // through here, so it is written exactly once, in order, regardless
        // of which task drove it. Best-effort — a poisoned lock or a failed
        // write must never break recording.
        //
        // An event with no record of its own still marks a moment worth
        // landing the writer's buffered deltas at: `Usage` ends a stream,
        // `History` commits a round, `TurnDone` ends the turn. So the
        // no-record case flushes rather than doing nothing — durability at
        // every round boundary, without a list of event kinds to keep in
        // sync here.
        if let Some((ts, to_write)) = out {
            let mut w = ts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for ev in &to_write {
                match crate::transcript_log::Record::from_event(ev) {
                    Some(rec) => w.write(&rec),
                    None => w.flush(),
                }
            }
        }
    }

    /// The failed `ToolEnd` every tool call still open on agent `key` never got,
    /// explained by `reason` — empty when the agent has nothing in flight.
    ///
    /// A turn that crashes unwinds between a call's `ToolStart` and the
    /// `ToolEnd` its completion would have emitted, so every view of that agent
    /// is left painting the call as still running: a spinner that never stops.
    /// That is the same shape [`crate::transcript::settle_restored_tools`]
    /// repairs for a session read back from disk, but it cannot be the same
    /// repair: that one edits already-folded [`crate::Entry`]s, and a *live*
    /// crash has to settle the thing the entries are folded FROM. So this hands
    /// back events. Emitted the way the turn's own events are emitted, they
    /// reach the TUI and the durable jsonl through the one
    /// fold ([`crate::transcript::apply_event`]) they already share, rather
    /// than being patched into one renderer and missing in the others.
    ///
    /// The caller emits them rather than this recording them itself, because the
    /// two crash paths hand their events on differently — a driven turn wraps
    /// [`Self::record`] in the watcher's callback, a delegated run calls it
    /// directly — and only the caller knows which.
    pub fn open_tool_ends(&self, key: u64, reason: &str) -> Vec<crate::AgentEvent> {
        self.with(|v| {
            let Some(e) = v.iter().find(|e| e.key == key) else {
                return Vec::new();
            };
            let Ok(log) = e.events.lock() else {
                return Vec::new();
            };
            log.open_tools()
                .iter()
                .map(|(id, name)| crate::AgentEvent::ToolEnd {
                    id: id.clone(),
                    name: name.clone(),
                    result: format!("(this call never returned — {reason})"),
                    ok: false,
                })
                .collect()
        })
    }

    /// Attach a durable transcript writer to agent `key` at `path`, if it does not
    /// already have one. Opens the file in **append** mode
    /// ([`crate::transcript_log::TranscriptLog::append`]): the main
    /// agent's id is stable across resumes, so a resumed session continues its
    /// existing jsonl rather than starting a second one.
    ///
    /// This is how the main agent joins the sub-agents' durable-transcript path:
    /// once attached, every event `record`ed against it appends to the jsonl, so
    /// its on-disk transcript is written incrementally instead of the whole
    /// `Vec<Entry>` being re-serialized into the session `.json` on every round.
    ///
    /// Best-effort and idempotent: a failed open leaves the agent without a writer
    /// (recording still works, just not to disk) rather than breaking the turn, and
    /// a second call while one is attached is a no-op — [`Self::detach_transcript`]
    /// is what a session switch uses to point it at the next file.
    pub fn attach_transcript(&self, key: u64, path: &std::path::Path) {
        // Opening the file is done outside the registry lock; the has/​set checks
        // are only ever driven from the single UI thread, so the window between
        // them cannot race a second attach.
        if self.with(|v| v.iter().any(|e| e.key == key && e.transcript.is_some())) {
            return;
        }
        let Ok(writer) = crate::transcript_log::TranscriptLog::append(path) else {
            return;
        };
        let writer = Arc::new(Mutex::new(writer));
        self.update(key, |e| {
            if e.transcript.is_none() {
                e.transcript = Some(writer);
            }
        });
    }

    /// Drop agent `key`'s transcript writer, so the next
    /// [`Self::attach_transcript`] opens a different session's file. A session
    /// switch (`/new`, `/resume`) uses this: without it, the new session's events
    /// would append onto the one we just left.
    pub fn detach_transcript(&self, key: u64) {
        self.update(key, |e| e.transcript = None);
    }

    /// A turn is starting on agent `key`: start its clock.
    /// Returns the generation this turn owns — see [`Self::turns`]. Callers that
    /// only want the state change (a frontend faking a turn, a test) can drop it.
    pub fn begin_turn(&self, key: u64) -> u64 {
        let generation = {
            let mut g = self.turns.lock().unwrap_or_else(|p| p.into_inner());
            let slot = g.entry(key).or_insert(0);
            *slot += 1;
            *slot
        };
        self.update(key, |e| {
            e.running = true;
            e.turn.begin();
            // Nothing is in flight at the start of a turn. Said here because the
            // one path that can leave a call open — a turn aborted mid-tool, where
            // the future is simply dropped and no `ToolEnd` is ever emitted — has
            // no handler to clear it, so without this the ids accumulate and the
            // NEXT crash would close a call from a turn that is long over,
            // stamping it with the wrong turn's reason.
            if let Ok(mut log) = e.events.lock() {
                log.clear_open_tools();
            }
        });
        generation
    }

    /// Agent `key`'s turn is over: stop its clock and mark it idle.
    pub fn end_turn(&self, key: u64) {
        self.update(key, |e| {
            e.running = false;
            e.turn.end();
            // The turn's last delta may still be buffered in the transcript
            // writer, and the next event on this agent could be minutes away (or
            // never). An idle agent's on-disk record must be its whole record:
            // this is what a resume folds back, and what a crash leaves behind.
            if let Some(ts) = &e.transcript {
                ts.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .flush();
            }
        });
    }

    /// Whether agent `key` has a turn in flight.
    pub fn is_running(&self, key: u64) -> bool {
        self.with(|v| v.iter().any(|e| e.key == key && e.running))
    }

    /// Whether agent `key` is summarizing its own context.
    pub fn is_compacting(&self, key: u64) -> bool {
        self.with(|v| v.iter().any(|e| e.key == key && e.compacting))
    }

    /// Queue a message for agent `key`, to reach it before its next request.
    ///
    /// The queue is the agent's, not a frontend's: what is waiting to be said to an
    /// agent is a fact about that agent. A frontend reads it back with
    /// [`Self::pending`] to show it, and does not keep a copy.
    pub fn enqueue(&self, key: u64, msg: crate::Steer) {
        self.with(|v| {
            if let Some(e) = v.iter().find(|e| e.key == key)
                && let Ok(mut q) = e.steering.lock()
            {
                // When the user types multiple lines while the agent is running,
                // each `spawn_turn` call pushes a separate message on the queue.
                // Merging adjacent messages avoids splitting them into separate
                // user turns — each of which carries the full system prompt and
                // tool definitions, and would be delivered as a separate
                // round-trip — for a burst of lines that are really one thought.
                if let Some(last) = q.back_mut() {
                    last.display.push('\n');
                    last.display.push_str(&msg.display);
                    last.sent.push('\n');
                    last.sent.push_str(&msg.sent);
                    // The merged text still refers to them, so the attachments
                    // merge too — dropping them here would leave the labels in
                    // `sent` pointing at images the model never received.
                    last.attachments.extend(msg.attachments);
                } else {
                    q.push_back(msg);
                }
            }
        });
    }

    /// What is still waiting to reach agent `key`, as the user typed it.
    pub fn pending(&self, key: u64) -> Vec<String> {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .and_then(|e| {
                    e.steering
                        .lock()
                        .ok()
                        .map(|q| q.iter().map(|s| s.display.clone()).collect())
                })
                .unwrap_or_default()
        })
    }

    /// Take the message at the head of agent `key`'s queue.
    pub fn take_pending(&self, key: u64) -> Option<crate::Steer> {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .and_then(|e| e.steering.lock().ok().and_then(|mut q| q.pop_front()))
        })
    }

    /// Take the message at the TAIL of agent `key`'s queue — the newest thing the
    /// user said to it, and the one they are most likely to want back.
    ///
    /// The counterpart to [`Self::take_pending`], which takes the head because the
    /// agent delivers in order. This end is for a frontend handing a queued message
    /// back to the user to edit: taking it off the queue is what stops the edited
    /// copy from being sent twice.
    pub fn take_newest_pending(&self, key: u64) -> Option<crate::Steer> {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .and_then(|e| e.steering.lock().ok().and_then(|mut q| q.pop_back()))
        })
    }

    /// Drop everything queued for agent `key`, returning how many were discarded
    /// (a cancelled turn must not leak them into the next one).
    pub fn clear_pending(&self, key: u64) -> usize {
        self.with(|v| {
            v.iter()
                .find(|e| e.key == key)
                .and_then(|e| {
                    e.steering.lock().ok().map(|mut q| {
                        let n = q.len();
                        q.clear();
                        n
                    })
                })
                .unwrap_or(0)
        })
    }

    /// After a turn ends: if another message is already queued for `key`, keep the
    /// agent running (the next `run` drains it as the opening) and return `true`;
    /// otherwise mark the turn finished (`running = false`, `turn.end()`) and
    /// return `false`.
    ///
    /// It does NOT pop (a queue-driven `run` drains the opening itself), it only
    /// decides continue-vs-finish under the entry lock — so a steer arriving
    /// concurrently (via `send_prompt`, which takes the same lock) can neither be
    /// lost nor spin the loop. Both this and `send_prompt` lock the entry before
    /// its queue, so a prompt cannot be accepted after the worker decides it is
    /// finished.
    pub fn continue_or_finish(&self, key: u64) -> bool {
        self.with(|v| {
            let Some(e) = v.iter_mut().find(|e| e.key == key) else {
                return false;
            };
            // Poison-tolerant, like every other lock here.
            let has_more = !e
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            if has_more {
                return true;
            }
            e.running = false;
            e.turn.end();
            false
        })
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
    pub fn send_prompt<F>(
        &self,
        key: u64,
        input: crate::Steer,
        on_event: F,
    ) -> Option<PromptDelivery>
    where
        F: FnMut(crate::AgentEvent) + Send + 'static,
    {
        // Decide and enqueue under the SAME entry lock the worker takes in
        // `continue_or_finish`, so a steer can't be pushed into the queue
        // *after* the worker has read it empty and marked the turn finished (a
        // lost message wrongly reported as `Steered`). The running-branch push
        // happens inside the closure; the idle branch hands `input` back out so a
        // fresh turn can be started without holding the lock across the spawn.
        let outcome = self.with(move |v| {
            let Some(e) = v.iter().find(|e| e.key == key) else {
                return SendOutcome::Unknown;
            };
            if e.running {
                if let Ok(mut q) = e.steering.lock() {
                    q.push_back(input);
                }
                SendOutcome::Steered
            } else {
                SendOutcome::Idle {
                    agent: Arc::clone(&e.agent),
                    steering: Arc::clone(&e.steering),
                    input,
                }
            }
        });
        let (agent, steering, input) = match outcome {
            SendOutcome::Unknown => return None,
            SendOutcome::Steered => return Some(PromptDelivery::Steered),
            SendOutcome::Idle {
                agent,
                steering,
                input,
            } => (agent, steering, input),
        };

        // Idle: a further turn on the agent we kept alive for exactly this.
        //
        // Enqueue the prompt as the turn's opening onto the very queue `run`
        // drains; `run` pops it, emits `Steered`, and pushes it into history — so
        // the question lands in the agent's record (and any pane rebuilt from it)
        // exactly like a mid-turn steer, through one path. A single turn, no
        // continuation loop.
        self.enqueue(key, input);
        let handle = self.start_turn_on(key, agent, steering, on_event, |_| async {});
        Some(PromptDelivery::StartedTurn(handle))
    }

    /// **Run a turn on agent `key`.** The one turn driver: whatever asked for the
    /// turn — a delegated run, a user's message in any frontend, a steer that
    /// arrived while the agent was idle — the turn itself happens here.
    ///
    /// What the caller supplies is how to *surface* the turn, never how to run it:
    ///
    /// * `on_event` sees each event after it has been recorded. A frontend uses it
    ///   to wake itself; nothing needs to be folded there, because the event is
    ///   already in the agent's own record ([`Self::record`]) and a pane is built
    ///   by replaying that.
    /// * `on_done` runs once, after the turn settles, with what went wrong (if
    ///   anything). Async, so a frontend can flush its own queue before it reports
    ///   the turn finished.
    ///
    /// Returns the task handle, so a caller that offers cancellation can abort the
    /// turn — for **any** agent, not only the session's.
    ///
    /// `None` if `key` names no agent (released, or never registered).
    pub fn start_turn<F, D, Fut>(&self, key: u64, on_event: F, on_done: D) -> Option<JoinHandle<()>>
    where
        F: FnMut(crate::AgentEvent) + Send + 'static,
        D: FnOnce(TurnOutcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (agent, steering) = self.handle(key)?;
        Some(self.start_turn_on(key, agent, steering, on_event, on_done))
    }

    /// [`Self::start_turn`] with the agent handle already in hand — the path
    /// `send_prompt` takes, where the handle was read under the same lock that
    /// decided the agent was idle.
    fn start_turn_on<F, D, Fut>(
        &self,
        key: u64,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        steering: crate::SteeringQueue,
        on_event: F,
        on_done: D,
    ) -> JoinHandle<()>
    where
        F: FnMut(crate::AgentEvent) + Send + 'static,
        D: FnOnce(TurnOutcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        // The turn clock belongs to the agent whose turn it is, so a frontend
        // showing that agent shows its loader.
        let generation = self.begin_turn(key);
        let live = self.clone();
        tokio::spawn(async move {
            // The guard marks the agent idle again on every exit — including
            // cancellation, where nothing after the await below would run. It
            // carries this turn's generation so an aborted predecessor, whose
            // guard may not run until after this turn started, cannot mark the
            // agent idle out from under it.
            let _guard = RunGuard::new(live.clone(), key, generation);
            // A handle kept out of the event closure below, which takes `live` by
            // value: the failure path still needs the registry once the turn is
            // over.
            let crashed = live.clone();
            let mut on_event = on_event;
            // Recorded on the agent's own entry rather than by whoever is watching:
            // what a turn did and what it spent are facts about the agent, not
            // about the fact that someone happened to be looking at it.
            let mut on_event = move |ev: crate::AgentEvent| {
                live.record(key, &ev);
                on_event(ev);
            };
            // A tool that panics unwinds this task. Guarded, so a crash becomes a
            // finished turn carrying an error instead of an agent stuck `running`
            // forever behind a spinner that never stops. (Cancellation aborts the
            // task, dropping the future — `catch_unwind` does not intercept that,
            // and the guard above is what ends the turn on that path.)
            let run = async {
                let mut a = agent.lock().await;
                a.run(steering, &mut on_event).await
            };
            let outcome = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
                Ok(Ok(())) => TurnOutcome::default(),
                Ok(Err(e)) => TurnOutcome {
                    error: Some(format!("{e:#}")),
                    panicked: false,
                },
                Err(payload) => TurnOutcome {
                    error: Some(format!("turn crashed: {}", panic_message(&*payload))),
                    panicked: true,
                },
            };
            if let Some(error) = &outcome.error {
                // A panicking tool unwinds the turn between its `ToolStart` and the
                // `ToolEnd` that finishing it would have emitted, so the call is
                // left open on every frontend — a spinner that runs forever, on a
                // tool that died with the turn. Close what is open before saying
                // the turn is over, so the crash lands on the call that caused it
                // rather than beside it. Run for a clean error too: nothing on that
                // path leaves a call open today, and if something ever does the
                // repair is the same one — it costs a lock and an empty vector
                // when there is nothing in flight.
                for ev in crashed.open_tool_ends(key, error) {
                    on_event(ev);
                }
                on_event(crate::AgentEvent::Notice(format!("[error] {error}")));
                // `run` only emits `TurnDone` on success; whoever is watching still
                // needs to know the turn is over.
                on_event(crate::AgentEvent::TurnDone);
            }
            on_done(outcome).await;
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

/// Age out finished/cancelled TODO items in place. Stamps each completed or
/// cancelled item with the `turn` it was first seen finished (in `stamps`,
/// keyed by content), then drops any terminal item that has been finished for
/// `ttl` turns. Stamps for items no longer present as terminal are forgotten,
/// so a re-finished item ages from scratch. Pending / in-progress items are kept.
///
/// This lives beside the agent, not in a frontend, because the TODO list is the
/// agent's own state: the model reads it every turn. Ageing it only in the TUI
/// meant a headless run — and every delegated sub-agent — accumulated terminal
/// items forever, growing the context they read from.
pub fn age_completed_todos(
    todos: &mut Vec<hrdr_tools::TodoItem>,
    stamps: &mut std::collections::HashMap<String, u64>,
    turn: u64,
    ttl: u64,
) {
    for t in todos.iter() {
        if t.status == "completed" || t.status == "cancelled" {
            stamps.entry(t.content.clone()).or_insert(turn);
        }
    }
    todos.retain(|t| {
        (t.status != "completed" && t.status != "cancelled")
            || stamps
                .get(&t.content)
                .is_none_or(|&done| turn.saturating_sub(done) < ttl)
    });
    stamps.retain(|content, _| {
        todos
            .iter()
            .any(|t| (t.status == "completed" || t.status == "cancelled") && &t.content == content)
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
    live: AgentRegistry,
    key: u64,
    /// The generation this guard's turn owns. See [`AgentRegistry::turns`].
    generation: u64,
}

impl RunGuard {
    pub fn new(live: AgentRegistry, key: u64, generation: u64) -> Self {
        Self {
            live,
            key,
            generation,
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // A cancelled turn's guard can run long after `abort()`, by which point
        // a replacement turn may already own the agent. Only the turn that still
        // owns it may declare it finished.
        if self.live.turn_gen(self.key) != self.generation {
            return;
        }
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

    fn entry(key: u64) -> AgentEntry {
        let agent = Agent::new(AgentConfig {
            // These tests drive turns against an endpoint nothing is listening
            // on — that unreachable endpoint IS the fixture. A refused
            // connection is a transient failure, so with the shipped policy the
            // turn would spend ten attempts and ~6¼ minutes of backoff getting
            // to the error the test asserts on. One attempt, no waiting.
            retry: hrdr_llm::RetryPolicy {
                max_attempts: 1,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        AgentEntry {
            key,
            bg_id: None,
            tool_id: None,
            label: "l".to_string(),
            model: "m".to_string(),
            provider: None,
            base_url: String::new(),
            effort: None,
            auto_compact: true,
            compaction_reserved: 0,
            sandbox: hrdr_tools::SandboxMode::None,
            todos: Default::default(),
            usage: crate::AgentUsage::default(),
            events: event_log(),
            reasoning_open: false,
            pending_notices: Vec::new(),
            turn: crate::TurnStats::default(),
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            steering: steering_queue(),
            running: true,
            compacting: false,
            done: false,
            delivered: false,
            pinned: false,
            transcript: None,
        }
    }

    #[test]
    fn prune_keeps_running_owed_and_pinned_entries() {
        let live = AgentRegistry::new();

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
        let a = AgentRegistry::next_key();
        let b = AgentRegistry::next_key();
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
            id: 0,
            status: status.to_string(),
            evidence: None,
        };
        let mut todos = vec![
            todo("done thing", "completed"),
            todo("cancelled thing", "cancelled"),
            todo("still going", "in_progress"),
        ];
        let mut stamps = std::collections::HashMap::new();

        // Both completed and cancelled are terminal — aged out together.
        age_completed_todos(&mut todos, &mut stamps, 1, 2);
        assert_eq!(todos.len(), 3, "freshly terminal items still show");

        age_completed_todos(&mut todos, &mut stamps, 2, 2);
        assert_eq!(todos.len(), 3, "and linger for their ttl");

        age_completed_todos(&mut todos, &mut stamps, 3, 2);
        assert_eq!(todos.len(), 1, "both aged out together");
        assert_eq!(todos[0].content, "still going");
        assert!(
            stamps.is_empty(),
            "stamps forgotten, so re-finished items age from scratch"
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
        let live = AgentRegistry::new();
        live.register(entry(1)); // `entry` is running
        let steering = live.with(|v| Arc::clone(&v[0].steering));

        let delivery = live.send_prompt(1, crate::Steer::plain("look at auth too"), |_| {});
        assert!(matches!(delivery, Some(PromptDelivery::Steered)));
        assert_eq!(
            steering
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.sent.clone())
                .collect::<Vec<_>>(),
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
        let live = AgentRegistry::new();
        live.register(entry(1));
        // Its delegated task has landed.
        live.update(1, |e| {
            e.running = false;
            e.done = true;
        });

        let delivery = live.send_prompt(1, crate::Steer::plain("now summarise"), |_| {});
        assert!(
            delivery.is_some_and(|d| d.started_turn()),
            "an idle agent is driven, not steered into a void"
        );
        assert!(
            live.with(|v| v[0].running),
            "and it is marked busy, so the next prompt steers instead"
        );
        // The turn itself runs against an unreachable endpoint and fails; the
        // RunGuard is what returns it to idle, which the cancellation test covers.
    }

    /// **A turn that fails reports it and ends.** The turn driver is shared, so
    /// this holds for every agent — the session's own and every delegated one —
    /// rather than only for whichever agent a frontend wrapped in its own guard.
    ///
    /// The agent here has no reachable endpoint, so its run returns an error: the
    /// outcome carries it, a `Notice` and a terminal `TurnDone` reach the watcher
    /// (a frontend cannot be left waiting on a turn that is over), and the agent is
    /// idle again.
    #[tokio::test]
    async fn a_failed_turn_reports_the_error_and_leaves_the_agent_idle() {
        let live = AgentRegistry::new();
        live.register(entry(1));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = Arc::new(Mutex::new(None));

        let handle = live
            .start_turn(
                1,
                {
                    let seen = Arc::clone(&seen);
                    move |ev| seen.lock().unwrap().push(ev)
                },
                {
                    let outcome = Arc::clone(&outcome);
                    move |o| async move {
                        *outcome.lock().unwrap() = Some(o);
                    }
                },
            )
            .expect("a registered agent can be driven");
        handle.await.unwrap();

        let outcome = outcome.lock().unwrap().take().expect("on_done ran");
        assert!(outcome.error.is_some(), "the failure is reported");
        assert!(!outcome.panicked, "it failed cleanly rather than crashing");

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|e| matches!(e, crate::AgentEvent::Notice(n) if n.starts_with("[error]"))),
            "the watcher is told what went wrong: {seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(crate::AgentEvent::TurnDone)),
            "and that the turn is over: {seen:?}"
        );
        assert!(
            !live.with(|v| v[0].running),
            "the guard returned it to idle"
        );
    }

    /// A prompt to an agent that has already been released reports that, so a
    /// caller can say so rather than silently swallowing what the user typed.
    #[test]
    fn a_prompt_to_a_released_agent_is_reported_not_swallowed() {
        let live = AgentRegistry::new();
        assert!(
            live.send_prompt(99, crate::Steer::plain("hello?"), |_| {})
                .is_none()
        );
    }

    /// A cancelled run must not strand its sub-agent. The update after `.await`
    /// never runs when the task is aborted, so the guard has to do it on drop.
    #[test]
    fn a_cancelled_run_still_releases_its_sub_agent() {
        let live = AgentRegistry::new();
        live.register(entry(1));
        {
            let _guard = RunGuard::new(live.clone(), 1, live.begin_turn(1));
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

    /// A cancelled turn's guard does not run at `abort()` — it runs whenever the
    /// runtime next polls the aborted task, which can be after its replacement
    /// has already begun. The late guard must not mark the agent idle while the
    /// live turn is still working.
    ///
    /// Regression: `cancel_turn` aborts, then immediately starts the next turn.
    /// Without the generation check the dying turn's guard flipped `running` to
    /// false under the new turn — stopping the loader, lying in the status bar,
    /// and letting `send_prompt` take its idle branch and start a *second*
    /// concurrent turn on the one agent.
    #[test]
    fn a_late_guard_from_a_cancelled_turn_does_not_end_its_successor() {
        let live = AgentRegistry::new();
        live.register(entry(1));

        // Turn A starts, then is cancelled: its guard is still alive, unpolled.
        let guard_a = RunGuard::new(live.clone(), 1, live.begin_turn(1));
        live.end_turn(1);

        // Turn B takes the agent over before A's task was ever polled again.
        let _guard_b = RunGuard::new(live.clone(), 1, live.begin_turn(1));
        assert!(live.with(|v| v[0].running), "turn B owns the agent");

        // Now A's aborted task finally unwinds.
        drop(guard_a);
        assert!(
            live.with(|v| v[0].running),
            "the superseded turn's guard must not end the turn that replaced it"
        );

        // B still ends its own turn normally.
        drop(_guard_b);
        assert!(!live.with(|v| v[0].running), "turn B ends its own turn");
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
        let live = AgentRegistry::new();
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
        let live = AgentRegistry::new();
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

    /// Which calls are in flight is tracked beside the record rather than read
    /// out of it, and this is why: a frontend that keeps up compacts the
    /// `ToolStart` away the moment it has folded it, so a crashed turn looking
    /// for what it left open would find an empty log and settle nothing. The
    /// ends it hands back are what close those entries (see
    /// [`AgentRegistry::open_tool_ends`]); a call that already ended is not
    /// among them.
    #[test]
    fn the_calls_in_flight_survive_the_record_being_compacted_away() {
        let live = AgentRegistry::new();
        live.register(entry(1));
        let start = |id: &str| crate::AgentEvent::ToolStart {
            id: id.to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        };
        live.record(1, &start("a"));
        live.record(1, &start("b"));
        live.record(
            1,
            &crate::AgentEvent::ToolEnd {
                id: "a".to_string(),
                name: "shell".to_string(),
                result: "done".to_string(),
                ok: true,
            },
        );

        // A frontend drains and releases the whole record, exactly as `pane` does.
        let (_, cursor) = live.events_since(1, 0).unwrap();
        live.compact(1, cursor);

        let ends = live.open_tool_ends(1, "turn crashed: boom");
        assert!(
            matches!(
                &ends[..],
                [crate::AgentEvent::ToolEnd { id, ok: false, result, .. }]
                    if id == "b" && result.contains("turn crashed: boom")
            ),
            "only the call still running is closed, and it is closed as failed: {ends:?}"
        );
        assert!(
            live.open_tool_ends(99, "gone").is_empty(),
            "an unknown key has nothing in flight"
        );
    }

    /// A sub-agent's tokens are a fact about *that agent*, so they are counted on
    /// its registry entry — not in whichever frontend happens to be showing it.
    /// A status bar reading the agent you are looking at reads these.
    #[test]
    fn a_sub_agents_calls_are_counted_on_its_own_entry() {
        let live = AgentRegistry::new();
        live.register(entry(1));
        live.register(entry(2));
        live.record(
            1,
            &crate::AgentEvent::Usage {
                prompt_tokens: 120,
                completion_tokens: 30,
                decode_ms: 0,
                cached_prompt_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
                cost_usd: None,
                session_cost_usd: Some(0.02),
                cost_partial: false,
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

    /// The durable-transcript writer rides on the registry entry, and `record`
    /// (the event path) appends to it. This is what lets a steered turn — which
    /// drives `record` from a different task than the delegated run — land in the
    /// same on-disk jsonl.
    #[test]
    fn record_writes_to_the_durable_transcript() {
        use crate::transcript_log;
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(Mutex::new(
            transcript_log::TranscriptLog::create(dir.path(), "000-x").unwrap(),
        ));
        let path = writer.lock().unwrap().path().to_path_buf();

        let live = AgentRegistry::new();
        live.register(entry(1));
        // Give the entry a real writer, as a spawned sub-agent's does.
        live.update(1, |e| e.transcript = Some(writer.clone()));

        live.record(1, &crate::AgentEvent::Text("steered ".into()));
        live.record(1, &crate::AgentEvent::Text("reply".into()));
        // An event with no record of its own is a boundary: it lands the deltas
        // the writer has been coalescing.
        live.record(1, &crate::AgentEvent::TurnDone);

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            body.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "the two deltas cost one line, not two: {body:?}"
        );
        let entries = transcript_log::read_transcript(&path);
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, crate::EntryKind::Assistant(s) if s == "steered reply")),
            "the recorded event landed in the jsonl: {entries:?}"
        );
    }

    /// A notice that lands while the model is mid-thought is deferred: it is
    /// written after the last reasoning delta, just before whatever closes the
    /// stream, so the record reads `[reasoning][notice][closing]` and the fold
    /// shows one complete thought block with the notice below it — never two
    /// running halves split around the notice. The durable jsonl is ordered
    /// the same way, so a read-back shows the notice after the thought too.
    #[test]
    fn a_notice_mid_thought_is_deferred_until_the_stream_ends() {
        use crate::transcript_log;
        let live = AgentRegistry::new();
        live.register(entry(1));
        // Give the entry a real writer, as a spawned sub-agent's does.
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(Mutex::new(
            transcript_log::TranscriptLog::create(dir.path(), "000-x").unwrap(),
        ));
        let path = writer.lock().unwrap().path().to_path_buf();
        live.update(1, |e| e.transcript = Some(writer.clone()));

        live.record(1, &crate::AgentEvent::Reasoning("first half ".into()));
        live.record(1, &crate::AgentEvent::Notice("mid-thought warning".into()));
        live.record(1, &crate::AgentEvent::Reasoning("second half".into()));
        live.record(1, &crate::AgentEvent::Text("the reply".into()));
        // A boundary event lands the reply the writer has been coalescing.
        live.record(1, &crate::AgentEvent::TurnDone);

        let (events, _) = live.events_since(1, 0).expect("recorded");
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                crate::AgentEvent::Reasoning(_) => "R",
                crate::AgentEvent::Notice(_) => "N",
                crate::AgentEvent::Text(_) => "T",
                _ => "?",
            })
            .collect();
        assert_eq!(
            &kinds[..4],
            ["R", "R", "N", "T"],
            "the notice follows the whole thought, ahead of the closing event"
        );

        // Folding the reordered record produces one complete thought, then the
        // notice, then the reply — the block is never split.
        let mut t: Vec<crate::Entry> = Vec::new();
        for ev in &events {
            crate::apply_event(&mut t, ev);
        }
        assert_eq!(t.len(), 3, "one thought, one notice, one reply: {t:?}");
        assert!(
            matches!(&t[0].kind,
                crate::EntryKind::Reasoning { text, took_ms: Some(_) }
                    if text == "first half second half"),
            "the thought is one closed block: {:?}",
            t[0].kind
        );
        assert!(
            matches!(&t[1].kind, crate::EntryKind::System(s) if s == "mid-thought warning"),
            "the notice sits below the thought: {:?}",
            t[1].kind
        );
        assert!(matches!(&t[2].kind, crate::EntryKind::Assistant(s) if s == "the reply"));

        // The durable transcript reads back the same way: one thought, then
        // the notice, then the reply — the notice's record is not between two
        // reasoning records.
        let from_jsonl = transcript_log::read_transcript(&path);
        assert_eq!(from_jsonl.len(), 3, "jsonl folds the same: {from_jsonl:?}");
        assert!(
            matches!(&from_jsonl[1].kind, crate::EntryKind::System(s) if s == "mid-thought warning"),
            "the notice's record lands after the thought in the jsonl: {from_jsonl:?}"
        );
    }

    /// One writer per file, and one file per agent: a second `attach_transcript`
    /// while one is open is a no-op (it must not add a second handle to the same
    /// jsonl), and a session switch — `detach` then `attach` at the new path —
    /// *replaces* the writer, so no record ever lands in the file we just left.
    #[test]
    fn attach_transcript_never_leaves_two_writers_on_one_file() {
        use crate::transcript_log;
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.jsonl");
        let second = dir.path().join("second.jsonl");

        let live = AgentRegistry::new();
        live.register(entry(1));
        live.attach_transcript(1, &first);
        // Idempotent: this must not open a second handle on `first`.
        live.attach_transcript(1, &first);
        live.record(1, &crate::AgentEvent::Text("one".into()));

        // A session switch points the agent at another file.
        live.detach_transcript(1);
        live.attach_transcript(1, &second);
        live.record(1, &crate::AgentEvent::Text("two".into()));
        // `first` was flushed by dropping its writer at `detach`; `second` still
        // holds its delta until a boundary — the end of the turn is one.
        live.end_turn(1);

        let body = std::fs::read_to_string(&first).unwrap();
        assert_eq!(
            body.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "the second attach did not duplicate the writer: {body:?}"
        );
        assert!(body.contains("one") && !body.contains("two"), "{body:?}");
        let body2 = std::fs::read_to_string(&second).unwrap();
        assert!(
            body2.contains("two") && !body2.contains("one"),
            "the replaced writer owns the new file alone: {body2:?}"
        );
        // And both files are intact, line by line.
        for p in [&first, &second] {
            let text = std::fs::read_to_string(p).unwrap();
            for l in text.lines().filter(|l| !l.trim().is_empty()) {
                serde_json::from_str::<transcript_log::Record>(l).expect("a standalone line");
            }
        }
    }

    /// `continue_or_finish` is the delegation loop's turn-boundary decision. With
    /// an EMPTY queue it FINISHES the turn: returns `false`, marks the agent idle,
    /// and stops its clock.
    #[test]
    fn continue_or_finish_finishes_an_empty_queue() {
        let live = AgentRegistry::new();
        live.register(entry(1)); // `entry` starts running
        // The loop leaves the clock ticking before each `run`; start it so the
        // finish path has something to stop.
        live.begin_turn(1);

        assert!(
            !live.continue_or_finish(1),
            "an empty queue finishes the turn"
        );
        assert!(!live.is_running(1), "the agent is marked idle");
        assert!(
            !live.turn(1).unwrap().inferring(),
            "and its turn clock is stopped"
        );
    }

    /// With a message already QUEUED it CONTINUES: returns `true`, leaves the agent
    /// running, and does NOT pop the message — the next `run` drains it as its own
    /// opener, so it must still be waiting.
    #[test]
    fn continue_or_finish_continues_when_a_message_is_queued() {
        let live = AgentRegistry::new();
        live.register(entry(1));
        live.begin_turn(1);
        live.enqueue(1, crate::Steer::plain("next"));

        assert!(
            live.continue_or_finish(1),
            "a queued message keeps the agent running for another turn"
        );
        assert!(live.is_running(1), "it stays running");
        assert!(
            live.turn(1).unwrap().inferring(),
            "its turn clock is not stopped"
        );
        assert_eq!(
            live.pending(1),
            vec!["next".to_string()],
            "the message is left for the next run to drain, not popped here"
        );
    }

    /// Merging two queued messages merges their attachments too.
    ///
    /// The merged `sent` text still names every attached file ("Image 1:
    /// shot.png"), so dropping the second message's bytes would leave the model
    /// reading a label for a picture it never got.
    #[test]
    fn merging_queued_messages_keeps_every_attachment() {
        let png = |name: &str| {
            hrdr_llm::media::Attachment::new(
                b"\x89PNG\r\n\x1a\n\0\0\0\0".to_vec(),
                hrdr_llm::media::MediaType::Png,
                name,
            )
            .unwrap()
        };
        let live = AgentRegistry::new();
        live.register(entry(1));
        live.begin_turn(1);
        live.enqueue(
            1,
            crate::Steer::plain("first").with_attachments(vec![png("a.png")]),
        );
        live.enqueue(
            1,
            crate::Steer::plain("second").with_attachments(vec![png("b.png")]),
        );

        let merged = live.take_pending(1).expect("one merged message");
        assert_eq!(merged.sent, "first\nsecond");
        assert_eq!(
            merged
                .attachments
                .iter()
                .map(|a| a.filename())
                .collect::<Vec<_>>(),
            vec!["a.png", "b.png"],
            "both files ride on the merged message, in order"
        );
    }

    /// An absent key is a no-op: no continuation, and no panic.
    #[test]
    fn continue_or_finish_on_an_unknown_key_is_false() {
        let live = AgentRegistry::new();
        assert!(
            !live.continue_or_finish(999),
            "no entry means no continuation"
        );
    }

    #[tokio::test]
    async fn handle_exposes_the_retained_agent_and_its_steering_queue() {
        let live = AgentRegistry::new();
        live.register(entry(7));
        let (agent, steering) = live.handle(7).expect("a live sub-agent is addressable");
        // The queue is the one `run` drains, so a push is a mid-turn injection.
        steering
            .lock()
            .unwrap()
            .push_back(crate::Steer::plain("steer me"));
        assert_eq!(steering.lock().unwrap().len(), 1);
        // And the agent itself is reachable for a further turn.
        assert!(agent.try_lock().is_ok());
        assert!(live.handle(999).is_none(), "an unknown key is not a handle");
    }
}
