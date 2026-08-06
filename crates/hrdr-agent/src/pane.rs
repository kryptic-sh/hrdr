//! Agent panes: the frontend-agnostic layer where every agent is the *same kind
//! of thing*.
//!
//! A pane is one addressable conversation — a transcript, a status, and the
//! handles needed to steer the agent or drive a further turn on it. There is one
//! per agent in [`crate::AgentRegistry`], keyed the same way ([`PaneId`] *is* the
//! registry key), and the session's own agent is simply the one at
//! [`PaneId::MAIN`]. A frontend switches which pane is *active*, renders that
//! pane's transcript, and sends input to it — with nothing to know about which
//! agent it happens to be.
//!
//! The transcript itself is built by [`apply_event`], the shared event→entry
//! reducer, from the agent's own event record. Every agent's stream goes through
//! it, so every view is assembled by exactly the same rules: assistant text
//! coalesces, reasoning coalesces, tool calls open and close.

use crate::{AgentEvent, AgentRegistry, Entry, SessionState, apply_event};

/// Which conversation a pane is: the [`crate::AgentEntry::key`] of the agent it
/// shows. The session's own agent is [`PaneId::MAIN`] and is a key like any
/// other — a pane identifies an *agent*, and there is only one kind of those.
///
/// This used to be `Main` plus `Sub(key)`, which gave the session's agent a
/// second identity with no key in it, so every frontend carried a conversion
/// (`PaneId::MAIN => MAIN_KEY`) and every lookup carried a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

impl PaneId {
    /// The session's own agent's pane. Always present in a [`PaneSet`].
    pub const MAIN: PaneId = PaneId(crate::MAIN_KEY);

    /// Whether this is the session's own agent — the conversation itself, as
    /// opposed to a task within it.
    pub fn is_main(self) -> bool {
        self == Self::MAIN
    }

    /// The registry key of the agent this pane shows.
    pub fn key(self) -> u64 {
        self.0
    }
}

/// What a pane is doing — drives the panel's marker and whether input steers an
/// in-flight turn or starts a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// A turn is in flight. Input is delivered as mid-turn steering.
    Running,
    /// Idle and ready. Input starts a new turn.
    Idle,
    /// A sub-agent whose delegated task has finished. Still addressable — input
    /// starts a further turn on it — until it is released.
    Done,
}

/// One addressable conversation.
pub struct Pane {
    pub id: PaneId,
    pub status: PaneStatus,
    /// **This agent's own state**: its name, the model/provider/endpoint it runs
    /// on, its chat history, its transcript, its token and cost counters.
    ///
    /// The same type the session file stores, held by every pane — because a
    /// sub-agent has all of those things too. It is what makes the status bar able
    /// to describe *the agent you are looking at* rather than always describing the
    /// main one, and what a sub-agent's own state file will serialize.
    pub state: SessionState,
    /// How many of this agent's recorded events have been folded into the
    /// transcript above. [`PaneSet::sync`] replays the rest.
    ///
    /// A sub-agent records everything it emits ([`crate::AgentEntry::events`]),
    /// so a pane opened ten minutes into a run still shows the whole run — the
    /// transcript is rebuilt from the agent's own record rather than assembled from
    /// whatever the frontend happened to be listening for at the time.
    consumed: usize,
    /// The clock on this agent's current turn — what its loader shows: whether it
    /// is inferring, how long it has worked, its throughput, its time-to-first-token.
    ///
    /// Per agent, because a turn is per agent. The loader used to be the main
    /// agent's no matter who was on screen: watching a sub-agent work showed the
    /// *main* agent's spinner and throughput, and a sub-agent grinding away under an
    /// idle main agent showed no loader at all.
    pub turn: crate::TurnStats,
    /// This agent is summarizing its own context. Per agent, because compaction is:
    /// a sub-agent on a small local model compacts itself, and its pane should say
    /// so rather than looking hung.
    pub compacting: bool,
    /// What the user has said to this agent that has not reached it yet — the
    /// agent's own queue, shown as pending blocks under its transcript.
    pub pending: Vec<String>,
    /// Reasoning effort this agent is running at.
    pub effort: Option<String>,
    /// Whether this agent auto-compacts, and the buffer it keeps below its window —
    /// which is where its context gauge turns red. Per agent: a sub-agent on a
    /// 64k local model has a different threshold from a main agent on 200k.
    pub auto_compact: bool,
    pub compaction_reserved: u32,
    /// The sandbox confinement this agent's tools enforce — the badge the status
    /// bar shows next to the context gauge. Per agent: a jailed delegation has
    /// its own.
    pub sandbox: hrdr_tools::SandboxMode,
    /// This agent's live TODO list — the one its own `todo` tool writes.
    pub todos: std::sync::Arc<std::sync::Mutex<Vec<hrdr_tools::TodoItem>>>,
    /// Where the reader is in this conversation, and what they had half-typed to
    /// it.
    ///
    /// These belong to the *conversation*, not to the terminal: glancing at the
    /// main agent and coming back should leave you where you were, with your
    /// half-written message still in the box. Kept per pane so switching agents is
    /// a change of view, not a loss of place.
    pub view: PaneView,
}

/// Per-conversation view state: the reader's position and their unsent message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneView {
    /// Scroll offset from the newest entry (0 = following the live output).
    pub scroll: usize,
    /// An unsent message typed into this conversation.
    pub draft: String,
}

impl Pane {
    /// Fold one of this pane's agent events into its transcript *and* its
    /// counters — the two things an event says about the agent it came from.
    pub fn apply(&mut self, ev: &AgentEvent) {
        apply_event(&mut self.state.transcript, ev);
        self.state.usage.record_event(ev);
    }

    /// This pane's live transcript.
    pub fn transcript(&self) -> &Vec<Entry> {
        &self.state.transcript
    }

    pub fn transcript_mut(&mut self) -> &mut Vec<Entry> {
        &mut self.state.transcript
    }

    /// Row label: always "main" for the main agent, the task description for a
    /// sub-agent.
    ///
    /// The main row is not named after the session: the session's name is already
    /// on the status bar, and repeating it here says nothing about *which agent*
    /// the row is — which is the only thing the list is for.
    pub fn title(&self) -> &str {
        match self.id.is_main() {
            true => "main",
            false => &self.state.name,
        }
    }

    /// The model this pane's agent is on.
    pub fn model(&self) -> &str {
        self.state.model.model()
    }

    /// The provider this pane's agent is on.
    pub fn provider(&self) -> &str {
        self.state.model.provider().as_str()
    }

    /// The whole identity this pane's agent runs on.
    pub fn model_ref(&self) -> &crate::ModelRef {
        &self.state.model
    }
}

/// The panes a frontend is showing — one per agent — and which one is active.
///
/// [`Self::sync`] reconciles them against the agent registry: adopting agents as
/// they are delegated, dropping those that have been released, and **pinning the
/// active one** so the prune leaves it alone while the user is reading it.
pub struct PaneSet {
    /// Every agent's pane, in the order they appeared.
    ///
    /// **Invariant:** `panes[0]` is [`PaneId::MAIN`]. It is created with the set
    /// and never removed — nothing releases the session's own agent — which is
    /// what lets [`Self::main`] hand out a `&Pane` rather than an `Option`. The
    /// session's agent is otherwise an ordinary member of this list: `sync`
    /// updates it, replays into it, and reads it back through the same code as
    /// every other pane.
    panes: Vec<Pane>,
    active: PaneId,
}

impl Default for PaneSet {
    fn default() -> Self {
        Self {
            panes: vec![Pane {
                id: PaneId::MAIN,
                status: PaneStatus::Idle,
                state: SessionState::default(),
                turn: crate::TurnStats::default(),
                compacting: false,
                pending: Vec::new(),
                effort: None,
                auto_compact: true,
                compaction_reserved: 0,
                sandbox: hrdr_tools::SandboxMode::None,
                todos: Default::default(),
                consumed: 0,
                view: PaneView::default(),
            }],
            active: PaneId::MAIN,
        }
    }
}

impl PaneSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active(&self) -> PaneId {
        self.active
    }

    /// The session agent's pane. Its transcript is the session's — the one that is
    /// saved and restored. Always present (see [`PaneSet::panes`]).
    pub fn main(&self) -> &Pane {
        &self.panes[0]
    }

    pub fn main_mut(&mut self) -> &mut Pane {
        &mut self.panes[0]
    }

    /// The transcript currently on screen: whichever pane is active.
    pub fn active_transcript(&self) -> &Vec<Entry> {
        self.active_pane().transcript()
    }

    /// Mutable access to the transcript on screen (per-entry expansion, etc.).
    pub fn active_transcript_mut(&mut self) -> &mut Vec<Entry> {
        self.active_pane_mut().transcript_mut()
    }

    /// Whether the agent switcher should be shown at all. With nothing delegated
    /// there is only the main agent, and a one-row list of the thing you are
    /// already looking at is just noise — so a fresh session shows no list.
    pub fn show_switcher(&self) -> bool {
        self.panes.len() > 1
    }

    /// Switch the active pane. Selecting an agent that is no longer live falls
    /// back to the session's own pane rather than stranding the view on a dead one.
    pub fn focus(&mut self, id: PaneId) {
        self.active = match self.panes.iter().any(|p| p.id == id) {
            true => id,
            false => PaneId::MAIN,
        };
    }

    /// The delegated agents' panes — everything but the session's own.
    pub fn subs(&self) -> &[Pane] {
        &self.panes[1..]
    }

    /// The pane for a registry key, session's own or delegated. One lookup, which
    /// is what lets `sync` treat every agent the same.
    pub fn pane_for(&mut self, key: u64) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id.key() == key)
    }

    /// A pane by id.
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.pane_for(id.key())
    }

    /// The pane on screen. Falls back to the session's own if the active agent has
    /// gone away between a `focus` and this read.
    pub fn active_pane(&self) -> &Pane {
        self.panes
            .iter()
            .find(|p| p.id == self.active)
            .unwrap_or_else(|| self.main())
    }

    /// The pane on screen, mutably — where a frontend stows the reader's place and
    /// their unsent draft before switching away.
    pub fn active_pane_mut(&mut self) -> &mut Pane {
        match self.panes.iter().position(|p| p.id == self.active) {
            Some(i) => &mut self.panes[i],
            None => self.main_mut(),
        }
    }

    /// Reconcile against the live registry and pin the active pane.
    ///
    /// Pinning here — every sync, from the frontend that is actually displaying
    /// it — is what keeps the user's pane alive: the agent prunes a sub-agent as
    /// soon as it is finished, delivered, and unpinned, and this is the only
    /// thing that says "someone is still reading this one".
    pub fn sync(&mut self, live: &AgentRegistry) {
        let active = self.active;
        let seen: Vec<LiveSnapshot> = live.with(|v| {
            for e in v.iter_mut() {
                // The session's agent is always pinned — it is the conversation,
                // and `owns_session` is the one place that says so.
                e.pinned = e.owns_session() || PaneId(e.key) == active;
            }
            v.iter()
                .map(|e| LiveSnapshot {
                    key: e.key,
                    label: e.label.clone(),
                    model: e.model.clone(),
                    provider: e.provider.clone(),
                    base_url: e.base_url.clone(),
                    effort: e.effort.clone(),
                    auto_compact: e.auto_compact,
                    compaction_reserved: e.compaction_reserved,
                    sandbox: e.sandbox,
                    todos: std::sync::Arc::clone(&e.todos),
                    usage: e.usage,
                    turn: e.turn,
                    running: e.running,
                    done: e.done,
                    compacting: e.compacting,
                    pending: e
                        .steering
                        .lock()
                        .map(|q| q.iter().map(|s| s.display.clone()).collect())
                        .unwrap_or_default(),
                    // A finished `task` block shows *what was delegated to*, not the
                    // work — the work is in that agent's own transcript.
                    tool_id: e.tool_id.clone(),
                    delegation: (!e.owns_session()).then(|| match &e.provider {
                        Some(p) => format!("{} · {p}/{}", e.label, e.model),
                        None => format!("{} · {}", e.label, e.model),
                    }),
                })
                .collect()
        });

        // Every agent is brought up to date the same way — the main one included.
        // Adopt any newly delegated agent, refresh what the registry owns (its
        // model/provider/endpoint, which a `/model` switch repoints, and its usage,
        // which it counts for itself), then replay whatever it has emitted since we
        // last looked.
        for s in &seen {
            let status = match (s.running, s.done) {
                (true, _) => PaneStatus::Running,
                (false, true) => PaneStatus::Done,
                (false, false) => PaneStatus::Idle,
            };
            let is_main = PaneId(s.key).is_main();
            let pane = match self.pane_for(s.key) {
                Some(p) => p,
                None => {
                    self.panes.push(Pane {
                        id: PaneId(s.key),
                        status,
                        state: SessionState::default(),
                        turn: crate::TurnStats::default(),
                        compacting: false,
                        pending: Vec::new(),
                        effort: None,
                        auto_compact: true,
                        compaction_reserved: 0,
                        sandbox: s.sandbox,
                        todos: Default::default(),
                        consumed: 0,
                        view: PaneView::default(),
                    });
                    self.panes.last_mut().expect("just pushed")
                }
            };
            pane.status = status;
            pane.turn = s.turn;
            pane.compacting = s.compacting;
            pane.pending = s.pending.clone();
            pane.effort = s.effort.clone();
            pane.auto_compact = s.auto_compact;
            pane.compaction_reserved = s.compaction_reserved;
            pane.sandbox = s.sandbox;
            pane.todos = std::sync::Arc::clone(&s.todos);
            // The registry still carries the agent's identity as two values; it is
            // paired back up here, at the edge, exactly as the session file's is.
            pane.state.model = crate::ModelRef::new(
                crate::ProviderName::new(s.provider.as_deref().unwrap_or("local")),
                &s.model,
            )
            .unwrap_or_else(|_| pane.state.model.clone());
            pane.state.base_url = s.base_url.clone();
            pane.state.usage = s.usage;
            // The main pane's name is the *session's*, which the session file owns —
            // it is not the agent's label.
            if !is_main {
                pane.state.name = s.label.clone();
            }

            // Replay the agent's own record into its transcript. This is the only
            // thing that builds any transcript, main or delegated: one reducer, one
            // record, one rule — a frontend renders an agent without knowing which
            // kind it is.
            let from = pane.consumed;
            if let Some((events, next)) = live.events_since(s.key, from) {
                let pane = self.pane_for(s.key).expect("just adopted");
                for ev in &events {
                    apply_replayed(&mut pane.state.transcript, ev, &seen);
                }
                pane.consumed = next;
                // Folded in — the agent may release them.
                live.compact(s.key, next);
            }
        }

        // Drop panes whose agent has been released. The active one is pinned above,
        // so it cannot vanish from under the user mid-read.
        // The session's own pane is never released, so it is kept unconditionally
        // (`panes[0]`, the invariant `main()` rests on) rather than depending on the
        // registry having reported it this tick.
        self.panes
            .retain(|p| p.id.is_main() || seen.iter().any(|s| s.key == p.id.key()));

        // If the active pane was released anyway (it was never live), fall back.
        if !self.panes.iter().any(|p| p.id == self.active) {
            self.active = PaneId::MAIN;
        }
    }
}

/// What [`PaneSet::sync`] reads off one registry entry, taken under the lock and
/// applied to the pane outside it.
struct LiveSnapshot {
    key: u64,
    label: String,
    model: String,
    provider: Option<String>,
    base_url: String,
    effort: Option<String>,
    auto_compact: bool,
    compaction_reserved: u32,
    sandbox: hrdr_tools::SandboxMode,
    todos: std::sync::Arc<std::sync::Mutex<Vec<hrdr_tools::TodoItem>>>,
    usage: crate::SessionUsage,
    turn: crate::TurnStats,
    running: bool,
    done: bool,
    compacting: bool,
    pending: Vec<String>,
    /// The `task` call that spawned this agent, if it was delegated.
    tool_id: Option<String>,
    /// How that `task` block should read once it finishes: what was delegated, and
    /// which provider/model answered. `None` for the main agent.
    delegation: Option<String>,
}

/// Fold a replayed event into a transcript, applying the one thing a transcript
/// cannot know from the event alone: a `task` call is a *delegation*.
///
/// A `task` block shows what was handed off and who to — not the sub-agent's
/// output. The output streams back through the same call (`ToolOutput`) and its
/// answer lands as the call's result, but replaying either into the parent would
/// make the parent's transcript a second, flattened copy of a conversation that
/// has a transcript of its own. The model still receives the real result; this is
/// only what is *shown*.
fn apply_replayed(transcript: &mut Vec<Entry>, ev: &AgentEvent, agents: &[LiveSnapshot]) {
    let delegated = |id: &str| {
        agents
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(id))
            .and_then(|s| s.delegation.clone())
    };
    match ev {
        AgentEvent::ToolOutput { id, .. } if delegated(id).is_some() => {}
        AgentEvent::ToolEnd { id, name, ok, .. } if delegated(id).is_some() => {
            apply_event(
                transcript,
                &AgentEvent::ToolEnd {
                    id: id.clone(),
                    name: name.clone(),
                    result: format!("↳ delegated to {}", delegated(id).unwrap_or_default()),
                    ok: *ok,
                },
            );
        }
        _ => apply_event(transcript, ev),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryKind;

    fn tool_start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            args: "{}".to_string(),
        }
    }

    /// Each pane carries *its own* agent's model, provider, endpoint and token
    /// counters — which is what lets a status bar describe the agent being viewed
    /// instead of always describing the main one.
    #[test]
    fn a_pane_carries_its_own_agents_model_and_counters() {
        let live = live_with(&[1]);
        live.update(1, |e| {
            e.model = "haiku".into();
            e.provider = Some("claude".into());
            e.base_url = "https://api.anthropic.com/v1".into();
            e.usage = crate::AgentUsage {
                tokens_in: 900,
                tokens_out: 40,
                last_prompt_tokens: Some(900),
                last_completion_tokens: Some(40),
                context_window: Some(64_000),
                cost_usd: 0.01,
                cost_partial: false,
                ..Default::default()
            };
        });

        let mut panes = PaneSet::new();
        panes.main_mut().state.model = "claude://opus".parse().unwrap();
        panes.sync(&live);

        // Looking at main: main's model.
        assert_eq!(panes.active_pane().model(), "opus");
        assert_eq!(panes.active_pane().state.usage.ctx_used(), 0);

        // Looking at the sub-agent: *its* model, endpoint, window and tokens.
        panes.focus(PaneId(1));
        let p = panes.active_pane();
        assert_eq!(p.model(), "haiku");
        assert_eq!(p.provider(), "claude");
        assert_eq!(p.state.base_url, "https://api.anthropic.com/v1");
        assert_eq!(p.state.usage.ctx_used(), 900);
        assert_eq!(p.state.usage.context_window, Some(64_000));
        assert_eq!(p.state.usage.tokens_out, 40);
        assert_eq!(p.state.usage.cost_usd, 0.01);

        // And the main agent's own counters are untouched by any of it.
        assert_eq!(panes.main().state.usage.tokens_in, 0);
        assert_eq!(panes.main().model(), "opus");
    }

    /// A fresh session has only the main agent, and a one-row list of the thing
    /// you are already looking at is noise — so the switcher stays hidden until
    /// something is actually delegated.
    #[test]
    fn the_switcher_is_hidden_until_there_is_more_than_one_agent() {
        let mut panes = PaneSet::new();
        assert!(
            !panes.show_switcher(),
            "a fresh session shows no agent list"
        );

        let live = live_with(&[1]);
        panes.sync(&live);
        assert!(
            panes.show_switcher(),
            "delegating a sub-agent brings the list up"
        );

        // The sub-agent is released; we are back to one agent, so it goes away.
        live.prune();
        panes.sync(&live);
        assert!(!panes.show_switcher());
    }

    #[test]
    fn the_main_pane_owns_the_session_transcript() {
        let mut panes = PaneSet::new();
        apply_event(
            panes.main_mut().transcript_mut(),
            &AgentEvent::Text("hi".into()),
        );
        assert_eq!(
            panes.active_transcript().len(),
            1,
            "main is active, so its transcript is on screen"
        );

        // Switching to a sub-agent shows *that* pane's transcript, and the main
        // one is untouched behind it.
        let live = live_with(&[3]);
        panes.sync(&live);
        panes.focus(PaneId(3));
        assert!(
            panes.active_transcript().is_empty(),
            "a fresh sub-agent pane starts empty"
        );
        apply_event(
            panes.active_transcript_mut(),
            &AgentEvent::Text("sub".into()),
        );
        assert_eq!(panes.active_transcript().len(), 1);

        panes.focus(PaneId::MAIN);
        assert!(
            matches!(&panes.active_transcript()[0].kind, EntryKind::Assistant(s) if s == "hi"),
            "the main transcript survived the excursion"
        );
    }

    /// A sub-agent's transcript is replayed from the agent's own record, so it is
    /// built whether or not anyone was watching while it ran — and it is built from
    /// the *events*, so its tool calls are real tool blocks.
    ///
    /// Regression: a background sub-agent emitted nothing to the frontend (its
    /// `task` call returns the instant it is spawned, leaving no live tool call to
    /// stream through), so its pane was empty no matter how long it worked — and
    /// background is the default. A blocking one streamed flattened text through
    /// its parent's call, so its tool calls showed up as prose.
    #[test]
    fn a_sub_agents_transcript_is_replayed_from_its_own_record() {
        let live = live_with(&[1]);
        // It works away with nobody watching — no pane exists yet.
        live.record(1, &AgentEvent::Steered("audit the auth module".into()));
        live.record(1, &tool_start("t1", "grep"));
        live.record(
            1,
            &AgentEvent::ToolEnd {
                id: "t1".into(),
                name: "grep".into(),
                result: "3 hits".into(),
                ok: true,
            },
        );
        live.record(1, &AgentEvent::Text("found it".into()));

        // The pane is adopted now, and the whole run is there.
        let mut panes = PaneSet::new();
        panes.sync(&live);
        let t = panes.subs()[0].transcript();
        assert!(
            matches!(&t[0].kind, EntryKind::User(s) if s == "audit the auth module"),
            "it opens with the task it was given, not just the answer"
        );
        assert!(
            matches!(&t[1].kind, EntryKind::Tool { name, result, done: true, .. }
                     if name == "grep" && result == "3 hits"),
            "its tool calls are tool blocks, not prose: {:?}",
            t[1].kind
        );
        assert!(matches!(&t[2].kind, EntryKind::Assistant(s) if s == "found it"));

        // Syncing again replays nothing: the cursor means no event lands twice.
        let before = t.len();
        panes.sync(&live);
        panes.sync(&live);
        assert_eq!(panes.subs()[0].transcript().len(), before);

        // New work appends.
        live.record(1, &AgentEvent::Text(" — auth.rs:42".into()));
        panes.sync(&live);
        let t = panes.subs()[0].transcript();
        assert_eq!(
            t.len(),
            before,
            "streamed text coalesces into the same block"
        );
        assert!(matches!(&t[2].kind, EntryKind::Assistant(s) if s == "found it — auth.rs:42"));
    }

    /// The main agent is an agent. Its pane is built from its own record, by the
    /// same reducer, from the same registry, as a delegated one — a frontend
    /// renders an agent without knowing which kind it is.
    ///
    /// Before this there were two implementations of "what does an event do to a
    /// conversation": the shared reducer for sub-agents, and a hand-written copy in
    /// the TUI for the main agent. They had already drifted.
    #[test]
    fn the_main_agent_is_built_from_its_own_record_like_any_other() {
        let live = AgentRegistry::new();
        let agent = || {
            std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::Agent::new(crate::AgentConfig {
                    ..Default::default()
                })
                .unwrap(),
            ))
        };
        live.register_session(
            agent(),
            crate::steering_queue(),
            "opus".to_string(),
            Some("claude".to_string()),
            "https://api".to_string(),
            crate::AgentUsage::default(),
        );

        let mut panes = PaneSet::new();
        live.record(crate::MAIN_KEY, &AgentEvent::Text("hello".into()));
        panes.sync(&live);

        assert!(
            matches!(&panes.main().transcript()[0].kind, EntryKind::Assistant(s) if s == "hello"),
            "the main agent's transcript is a replay of its record"
        );
        assert_eq!(panes.main().model(), "opus", "and so is its chrome");
        assert!(
            !panes.show_switcher(),
            "being in the registry does not make the main agent a row in the list"
        );

        // It is never pruned, however idle it is: it is the conversation.
        live.prune();
        panes.sync(&live);
        assert_eq!(panes.main().transcript().len(), 1);
        assert_eq!(
            live.len(),
            1,
            "the session's agent survives a prune that would collect any sub-agent"
        );
    }

    /// Everything that describes an agent comes off *that agent's* entry: its
    /// effort, its compaction thresholds (which set where its context gauge turns
    /// red), and its own TODO list. None of it is the frontend's to keep.
    #[test]
    fn a_pane_carries_its_own_agents_effort_thresholds_and_todos() {
        let live = live_with(&[1]);
        let todos = std::sync::Arc::new(std::sync::Mutex::new(vec![hrdr_tools::TodoItem {
            content: "the sub-agent's own task".to_string(),
            id: 0,
            status: "in_progress".to_string(),
            evidence: None,
        }]));
        live.update(1, |e| {
            e.effort = Some("high".into());
            e.auto_compact = false;
            e.compaction_reserved = 4_000;
            e.todos = std::sync::Arc::clone(&todos);
        });

        let mut panes = PaneSet::new();
        panes.sync(&live);
        panes.focus(PaneId(1));

        let p = panes.active_pane();
        assert_eq!(p.effort.as_deref(), Some("high"));
        assert!(!p.auto_compact, "its own auto-compact setting");
        assert_eq!(p.compaction_reserved, 4_000, "its own red-line");
        assert_eq!(
            p.todos.lock().unwrap()[0].content,
            "the sub-agent's own task",
            "the TODO panel shows the list of the agent on screen"
        );

        // And the main agent's are untouched by any of it.
        let m = panes.main();
        assert_eq!(m.effort, None);
        assert!(m.todos.lock().unwrap().is_empty());
    }

    /// Running, compacting, and what is queued are facts about an agent, so they come
    /// off the agent's entry — for every agent.
    ///
    /// This is what a frontend copy cost: only the main agent had them. A sub-agent
    /// compacting itself — which it decides on its own, and which no frontend is told
    /// about — just looked hung; and a message queued for one agent showed under
    /// whichever agent happened to be on screen.
    #[test]
    fn running_compacting_and_the_queue_are_the_agents() {
        let live = live_with(&[1]);
        let mut panes = PaneSet::new();
        panes.sync(&live);
        assert!(!panes.subs()[0].compacting);
        assert!(panes.subs()[0].pending.is_empty());

        // It decides to summarize itself — nothing asked it to.
        live.update(1, |e| e.compacting = true);
        // And the user says something to it while it is busy.
        live.enqueue(1, crate::Steer::new("<expanded @file blob>", "check auth"));

        panes.sync(&live);
        let p = &panes.subs()[0];
        assert!(p.compacting, "its own pane says it is compacting");
        assert_eq!(
            p.pending,
            vec!["check auth".to_string()],
            "the pending block shows what was typed, not the @file expansion the \
             model will read"
        );

        // The main agent's view is untouched by either.
        assert!(!panes.main().compacting);
        assert!(panes.main().pending.is_empty());

        // The agent takes it off its own queue; nothing else has to be told.
        let taken = live.take_pending(1).expect("it was queued on the agent");
        assert_eq!(taken.sent, "<expanded @file blob>");
        panes.sync(&live);
        assert!(panes.subs()[0].pending.is_empty());
    }

    #[test]
    fn focusing_a_sub_agent_that_is_gone_falls_back_to_main() {
        let mut panes = PaneSet::new();
        panes.focus(PaneId(42));
        assert_eq!(
            panes.active(),
            PaneId::MAIN,
            "a dead sub-agent must not strand the view"
        );
    }

    /// The status bar's sandbox badge is the session agent's REAL confinement,
    /// tracked from the registry like every other per-agent field — the main
    /// pane's `Default` value must not survive the first sync.
    ///
    /// Regression: the main pane was seeded with `SandboxMode::None` and `sync`
    /// refreshed every field of an existing pane except `sandbox`, so the badge
    /// read "Yolo" forever, whatever the session actually enforced (a plain
    /// `hrdr` run is `Write`; only `--yolo`/`--sandbox none` is unconfined).
    #[test]
    fn the_main_panes_sandbox_badge_tracks_the_registry() {
        let live = AgentRegistry::new();
        let agent = crate::Agent::new(crate::AgentConfig {
            ..Default::default()
        })
        .unwrap();
        live.register(crate::AgentEntry {
            key: crate::MAIN_KEY,
            bg_id: None,
            tool_id: None,
            label: "main".to_string(),
            model: "m".to_string(),
            provider: None,
            base_url: String::new(),
            effort: None,
            auto_compact: true,
            compaction_reserved: 0,
            sandbox: hrdr_tools::SandboxMode::Write,
            todos: Default::default(),
            usage: crate::AgentUsage::default(),
            events: crate::event_log(),
            reasoning_open: false,
            pending_notices: Vec::new(),
            turn: crate::TurnStats::default(),
            agent: std::sync::Arc::new(tokio::sync::Mutex::new(agent)),
            steering: crate::steering_queue(),
            running: false,
            compacting: false,
            done: false,
            delivered: false,
            pinned: true,
            transcript: None,
        });

        let mut panes = PaneSet::new();
        // The seed is the bug: it claims unconfined before any registry exists.
        assert_eq!(panes.main().sandbox, hrdr_tools::SandboxMode::None);
        panes.sync(&live);
        assert_eq!(
            panes.main().sandbox,
            hrdr_tools::SandboxMode::Write,
            "the badge must report the mode the session actually enforces"
        );
        assert_eq!(
            panes.active_pane().sandbox,
            hrdr_tools::SandboxMode::Write,
            "what the status bar reads"
        );
    }

    /// Build a live registry entry for `key`, finished and delivered — i.e. one
    /// the agent will prune the moment nobody is looking at it.
    fn live_with(keys: &[u64]) -> AgentRegistry {
        let live = AgentRegistry::new();
        for &key in keys {
            let agent = crate::Agent::new(crate::AgentConfig {
                ..Default::default()
            })
            .unwrap();
            live.register(crate::AgentEntry {
                key,
                bg_id: None,
                tool_id: None,
                label: format!("task {key}"),
                model: "m".to_string(),
                provider: None,
                base_url: String::new(),
                effort: None,
                auto_compact: true,
                compaction_reserved: 0,
                sandbox: hrdr_tools::SandboxMode::None,
                todos: Default::default(),
                usage: crate::AgentUsage::default(),
                events: crate::event_log(),
                reasoning_open: false,
                pending_notices: Vec::new(),
                turn: crate::TurnStats::default(),
                agent: std::sync::Arc::new(tokio::sync::Mutex::new(agent)),
                steering: crate::steering_queue(),
                running: false,
                compacting: false,
                done: true,
                delivered: true,
                pinned: false,
                transcript: None,
            });
        }
        live
    }

    /// The pane the user is reading must survive the agent's prune. `sync` pins
    /// the active pane every pass — that pin is the *only* thing keeping a
    /// finished, delivered sub-agent alive, so a bug here silently deletes the
    /// conversation out from under the reader.
    #[test]
    fn syncing_pins_the_active_pane_so_the_prune_spares_it() {
        let live = live_with(&[1, 2]);
        let mut panes = PaneSet::new();

        panes.sync(&live);
        assert_eq!(panes.subs().len(), 2, "delegated sub-agents become panes");
        assert!(
            panes.subs().iter().all(|p| p.status == PaneStatus::Done),
            "finished sub-agents show as done"
        );

        // Nobody is viewing a sub-agent: the agent releases both.
        live.prune();
        panes.sync(&live);
        assert!(panes.subs().is_empty(), "unwatched sub-agents are released");

        // Now view one. The next sync pins it, and it survives the prune.
        let live = live_with(&[7, 8]);
        let mut panes = PaneSet::new();
        panes.sync(&live);
        panes.focus(PaneId(7));
        panes.sync(&live);
        live.prune();
        panes.sync(&live);

        let keys: Vec<PaneId> = panes.subs().iter().map(|p| p.id).collect();
        assert_eq!(keys, vec![PaneId(7)], "the pane being read is kept");
        assert_eq!(panes.active(), PaneId(7));

        // Switch back to main: nothing pins it now, so it is released and only the
        // main pane remains.
        panes.focus(PaneId::MAIN);
        panes.sync(&live);
        live.prune();
        panes.sync(&live);
        assert!(panes.subs().is_empty());
    }
}
