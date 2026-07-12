//! Agent panes: the frontend-agnostic layer that makes the main agent and a
//! delegated sub-agent the *same kind of thing*.
//!
//! A pane is one addressable conversation — a transcript, a status, and (for a
//! sub-agent) the handles needed to steer it or drive a further turn on it. The
//! main agent is pane [`PaneId::Main`]; every retained sub-agent (see
//! [`hrdr_agent::LiveSubagents`]) is a pane of its own. A frontend switches which
//! pane is *active*, renders that pane's transcript, and sends input to it —
//! without caring which kind it is.
//!
//! The transcript itself is built by [`apply_event`], the shared event→entry
//! reducer. Both the main agent's stream and a sub-agent's stream go through it,
//! so a sub-agent's view is assembled by exactly the same rules as the main one:
//! assistant text coalesces, reasoning coalesces, tool calls open and close.

use hrdr_agent::{AgentEvent, LiveSubagents, SubagentKind};

use crate::{Entry, EntryKind};

/// Which conversation a pane is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneId {
    /// The session's own agent.
    Main,
    /// A delegated sub-agent, keyed by its [`hrdr_agent::LiveSubagent::key`].
    Sub(u64),
}

impl PaneId {
    pub fn is_main(self) -> bool {
        matches!(self, PaneId::Main)
    }

    /// The live-registry key, for a sub-agent pane.
    pub fn key(self) -> Option<u64> {
        match self {
            PaneId::Main => None,
            PaneId::Sub(k) => Some(k),
        }
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
    /// Row label: the session name for main, the task description for a sub.
    pub title: String,
    /// Model this pane's agent is on (shown on its row).
    pub model: String,
    pub status: PaneStatus,
    /// The pane's own transcript, built by [`apply_event`]. Every pane owns its
    /// transcript, main included — the session's saved transcript is refreshed
    /// *from* the main pane at save time (the same way `SessionState` refreshes
    /// its message mirror from the agent), so there is exactly one live copy.
    pub transcript: Vec<Entry>,
}

impl Pane {
    /// Fold one of this pane's agent events into its transcript.
    pub fn apply(&mut self, ev: &AgentEvent) {
        apply_event(&mut self.transcript, ev);
    }
}

/// Fold an agent event into a transcript. The shared reducer behind every pane —
/// the main agent's stream and a sub-agent's stream are assembled by the same
/// rules, so a sub-agent's view reads exactly like the main one.
///
/// Only transcript-visible events do anything; `Usage`, `History` and `TurnDone`
/// carry no transcript content and are ignored here (a frontend still handles
/// them for its own bookkeeping).
pub fn apply_event(transcript: &mut Vec<Entry>, ev: &AgentEvent) {
    // Close an open reasoning block as soon as anything else arrives, so its
    // duration label stops streaming.
    if !matches!(ev, AgentEvent::Reasoning(_)) {
        finish_reasoning(transcript);
    }
    match ev {
        AgentEvent::Text(t) => match transcript.last_mut().map(|e| &mut e.kind) {
            Some(EntryKind::Assistant(s)) => s.push_str(t),
            // An empty delta must not open an entry: a turn that only calls tools
            // would otherwise leave a blank assistant block behind.
            _ if t.is_empty() => {}
            _ => transcript.push(Entry::assistant(t.clone())),
        },
        AgentEvent::Reasoning(t) => match transcript.last_mut().map(|e| &mut e.kind) {
            Some(EntryKind::Reasoning {
                text,
                took_ms: None,
            }) => text.push_str(t),
            _ => transcript.push(Entry::reasoning(t.clone())),
        },
        AgentEvent::ToolStart { id, name, args } => {
            transcript.push(Entry::at(
                EntryKind::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    result: String::new(),
                    ok: true,
                    done: false,
                    expanded: false,
                },
                chrono::Local::now(),
            ));
        }
        AgentEvent::ToolOutput { id, chunk } => {
            if let Some(EntryKind::Tool { result, .. }) = open_tool(transcript, id) {
                result.push_str(chunk);
            }
        }
        AgentEvent::ToolEnd {
            id,
            result,
            ok,
            name: _,
        } => {
            if let Some(EntryKind::Tool {
                result: r,
                ok: o,
                done,
                ..
            }) = open_tool(transcript, id)
            {
                *r = result.clone();
                *o = *ok;
                *done = true;
            }
        }
        AgentEvent::Notice(text) => transcript.push(Entry::notice(text.clone())),
        // A steered message is a real user turn in this conversation.
        AgentEvent::Steered(sent) => transcript.push(Entry::user(sent.clone())),
        AgentEvent::Usage { .. } | AgentEvent::History(_) | AgentEvent::TurnDone => {}
    }
}

/// The still-open tool entry with `id`, searched from the end (a tool id is
/// unique within a turn, and the newest match is the live one).
fn open_tool<'a>(transcript: &'a mut [Entry], id: &str) -> Option<&'a mut EntryKind> {
    transcript.iter_mut().rev().find_map(|e| match &e.kind {
        EntryKind::Tool {
            id: tid,
            done: false,
            ..
        } if tid == id => Some(&mut e.kind),
        _ => None,
    })
}

/// Stamp a duration on a reasoning block that is still streaming. The frontend
/// owns the wall-clock, so this only marks it closed (`took_ms: Some(0)` would
/// lie); a frontend that tracks timing overwrites it.
fn finish_reasoning(transcript: &mut [Entry]) {
    if let Some(EntryKind::Reasoning {
        took_ms: took @ None,
        ..
    }) = transcript.last_mut().map(|e| &mut e.kind)
    {
        *took = Some(0);
    }
}

/// The panes a frontend is showing: the main agent plus every retained
/// sub-agent, and which one is active.
///
/// [`Self::sync`] reconciles the sub-panes against the live registry — adopting
/// sub-agents as they are delegated, dropping those the agent has released, and
/// **pinning the active one** so the agent's prune leaves it alone while the user
/// is reading it.
pub struct PaneSet {
    /// The session's own agent. A pane like any other — it simply always exists.
    main: Pane,
    subs: Vec<Pane>,
    active: PaneId,
}

impl Default for PaneSet {
    fn default() -> Self {
        Self {
            main: Pane {
                id: PaneId::Main,
                title: "main".to_string(),
                model: String::new(),
                status: PaneStatus::Idle,
                transcript: Vec::new(),
            },
            subs: Vec::new(),
            active: PaneId::Main,
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

    /// The main agent's pane. Its transcript is the session's — the one that is
    /// saved and restored.
    pub fn main(&self) -> &Pane {
        &self.main
    }

    pub fn main_mut(&mut self) -> &mut Pane {
        &mut self.main
    }

    /// The transcript currently on screen: whichever pane is active.
    pub fn active_transcript(&self) -> &Vec<Entry> {
        match self.active {
            PaneId::Main => &self.main.transcript,
            PaneId::Sub(k) => self
                .subs
                .iter()
                .find(|p| p.id == PaneId::Sub(k))
                .map_or(&self.main.transcript, |p| &p.transcript),
        }
    }

    /// Mutable access to the transcript on screen (per-entry `/expand`, etc.).
    pub fn active_transcript_mut(&mut self) -> &mut Vec<Entry> {
        match self.active {
            PaneId::Main => &mut self.main.transcript,
            PaneId::Sub(k) => {
                if let Some(i) = self.subs.iter().position(|p| p.id == PaneId::Sub(k)) {
                    &mut self.subs[i].transcript
                } else {
                    &mut self.main.transcript
                }
            }
        }
    }

    /// Whether the agent switcher should be shown at all. With nothing delegated
    /// there is only the main agent, and a one-row list of the thing you are
    /// already looking at is just noise — so a fresh session shows no list.
    pub fn show_switcher(&self) -> bool {
        !self.subs.is_empty()
    }

    /// Switch the active pane. Selecting a sub-agent that is no longer live falls
    /// back to main rather than stranding the view on a dead pane.
    pub fn focus(&mut self, id: PaneId) {
        self.active = match id {
            PaneId::Main => PaneId::Main,
            PaneId::Sub(k) if self.subs.iter().any(|p| p.id == PaneId::Sub(k)) => PaneId::Sub(k),
            PaneId::Sub(_) => PaneId::Main,
        };
    }

    pub fn subs(&self) -> &[Pane] {
        &self.subs
    }

    pub fn sub_mut(&mut self, key: u64) -> Option<&mut Pane> {
        self.subs.iter_mut().find(|p| p.id == PaneId::Sub(key))
    }

    /// The active sub-agent pane, if a sub-agent is what's active.
    pub fn active_sub(&self) -> Option<&Pane> {
        let key = self.active.key()?;
        self.subs.iter().find(|p| p.id == PaneId::Sub(key))
    }

    /// Reconcile against the live registry and pin the active pane.
    ///
    /// Pinning here — every sync, from the frontend that is actually displaying
    /// it — is what keeps the user's pane alive: the agent prunes a sub-agent as
    /// soon as it is finished, delivered, and unpinned, and this is the only
    /// thing that says "someone is still reading this one".
    pub fn sync(&mut self, live: &LiveSubagents) {
        let active_key = self.active.key();
        let seen: Vec<(u64, String, String, bool, bool)> = live.with(|v| {
            for e in v.iter_mut() {
                e.pinned = Some(e.key) == active_key;
            }
            v.iter()
                .map(|e| (e.key, e.label.clone(), e.model.clone(), e.running, e.done))
                .collect()
        });

        // Adopt newly delegated sub-agents, refresh the status of known ones.
        for (key, label, model, running, done) in &seen {
            let status = match (running, done) {
                (true, _) => PaneStatus::Running,
                (false, true) => PaneStatus::Done,
                (false, false) => PaneStatus::Idle,
            };
            match self.sub_mut(*key) {
                Some(p) => {
                    p.status = status;
                    p.title = label.clone();
                }
                None => self.subs.push(Pane {
                    id: PaneId::Sub(*key),
                    title: label.clone(),
                    model: model.clone(),
                    status,
                    transcript: Vec::new(),
                }),
            }
        }

        // Drop panes whose sub-agent the agent has released. The active one is
        // pinned above, so it cannot vanish from under the user mid-read.
        self.subs.retain(|p| {
            p.id.key()
                .is_some_and(|k| seen.iter().any(|(s, ..)| *s == k))
        });

        // If the active pane was released anyway (it was never live), fall back.
        if let Some(k) = self.active.key()
            && !self.subs.iter().any(|p| p.id == PaneId::Sub(k))
        {
            self.active = PaneId::Main;
        }
    }
}

/// One row of the pane switcher: the main agent first, then each sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub id: PaneId,
    pub title: String,
    pub status: PaneStatus,
    /// The row is the pane currently being displayed.
    pub active: bool,
}

/// The switcher's rows. Main is always first — it is how you get back.
pub fn pane_rows(panes: &PaneSet) -> Vec<PaneRow> {
    let mut rows = vec![PaneRow {
        id: PaneId::Main,
        title: panes.main().title.clone(),
        status: panes.main().status,
        active: panes.active().is_main(),
    }];
    rows.extend(panes.subs().iter().map(|p| PaneRow {
        id: p.id,
        title: p.title.clone(),
        status: p.status,
        active: panes.active() == p.id,
    }));
    rows
}

/// The marker a row shows: running spinner, finished tick, or idle dot.
pub fn pane_row_marker(status: PaneStatus, running_marker: &str) -> String {
    match status {
        PaneStatus::Running => running_marker.to_string(),
        PaneStatus::Done => "✓".to_string(),
        PaneStatus::Idle => "·".to_string(),
    }
}

/// Whether a sub-agent kind may be steered mid-turn. Both can — the queue is the
/// one its `run` drains — but a blocking sub-agent is holding the main agent's
/// turn open while it works, which is worth surfacing.
pub fn holds_main_turn(kind: SubagentKind) -> bool {
    matches!(kind, SubagentKind::Blocking)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            args: "{}".to_string(),
        }
    }

    #[test]
    fn text_coalesces_and_an_empty_delta_opens_nothing() {
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Text(String::new()));
        assert!(t.is_empty(), "an empty delta must not open an entry");
        apply_event(&mut t, &AgentEvent::Text("he".into()));
        apply_event(&mut t, &AgentEvent::Text("llo".into()));
        assert_eq!(t.len(), 1);
        assert!(matches!(&t[0].kind, EntryKind::Assistant(s) if s == "hello"));
    }

    #[test]
    fn a_tool_call_opens_streams_and_closes() {
        let mut t = Vec::new();
        apply_event(&mut t, &tool_start("c1", "bash"));
        apply_event(
            &mut t,
            &AgentEvent::ToolOutput {
                id: "c1".into(),
                chunk: "partial".into(),
            },
        );
        assert!(
            matches!(&t[0].kind, EntryKind::Tool { result, done: false, .. } if result == "partial")
        );
        apply_event(
            &mut t,
            &AgentEvent::ToolEnd {
                id: "c1".into(),
                name: "bash".into(),
                result: "final".into(),
                ok: false,
            },
        );
        assert!(
            matches!(&t[0].kind, EntryKind::Tool { result, done: true, ok: false, .. } if result == "final")
        );
    }

    #[test]
    fn a_steered_message_becomes_a_user_turn_in_the_pane() {
        // This is what makes a sub-agent view a conversation rather than a log:
        // what you send it shows up in its transcript, where you said it.
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Text("working".into()));
        apply_event(&mut t, &AgentEvent::Steered("actually, stop".into()));
        apply_event(&mut t, &AgentEvent::Text("ok".into()));
        let kinds: Vec<&EntryKind> = t.iter().map(|e| &e.kind).collect();
        assert!(matches!(kinds[0], EntryKind::Assistant(s) if s == "working"));
        assert!(matches!(kinds[1], EntryKind::User(s) if s == "actually, stop"));
        assert!(
            matches!(kinds[2], EntryKind::Assistant(s) if s == "ok"),
            "the reply after steering is a new block, not appended to the old one"
        );
    }

    #[test]
    fn reasoning_closes_when_anything_else_arrives() {
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Reasoning("hmm".into()));
        assert!(matches!(
            &t[0].kind,
            EntryKind::Reasoning { took_ms: None, .. }
        ));
        apply_event(&mut t, &AgentEvent::Text("answer".into()));
        assert!(
            matches!(
                &t[0].kind,
                EntryKind::Reasoning {
                    took_ms: Some(_),
                    ..
                }
            ),
            "the block is closed once the model moves on"
        );
    }

    #[test]
    fn main_is_always_the_first_row_so_you_can_get_back() {
        let mut panes = PaneSet::new();
        panes.main_mut().title = "my session".to_string();
        let rows = pane_rows(&panes);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, PaneId::Main);
        assert!(rows[0].active, "main is active by default");
        assert_eq!(rows[0].title, "my session");
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
            &mut panes.main_mut().transcript,
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
        panes.focus(PaneId::Sub(3));
        assert!(
            panes.active_transcript().is_empty(),
            "a fresh sub-agent pane starts empty"
        );
        apply_event(
            panes.active_transcript_mut(),
            &AgentEvent::Text("sub".into()),
        );
        assert_eq!(panes.active_transcript().len(), 1);

        panes.focus(PaneId::Main);
        assert!(
            matches!(&panes.active_transcript()[0].kind, EntryKind::Assistant(s) if s == "hi"),
            "the main transcript survived the excursion"
        );
    }

    #[test]
    fn focusing_a_sub_agent_that_is_gone_falls_back_to_main() {
        let mut panes = PaneSet::new();
        panes.focus(PaneId::Sub(42));
        assert_eq!(
            panes.active(),
            PaneId::Main,
            "a dead sub-agent must not strand the view"
        );
    }

    /// Build a live registry entry for `key`, finished and delivered — i.e. one
    /// the agent will prune the moment nobody is looking at it.
    fn live_with(keys: &[u64]) -> LiveSubagents {
        let live = LiveSubagents::new();
        for &key in keys {
            let agent = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
                checkpoints: Some("off".to_string()),
                ..Default::default()
            })
            .unwrap();
            live.register(hrdr_agent::LiveSubagent {
                key,
                bg_id: None,
                tool_id: None,
                label: format!("task {key}"),
                model: "m".to_string(),
                provider: None,
                kind: SubagentKind::Blocking,
                agent: std::sync::Arc::new(tokio::sync::Mutex::new(agent)),
                steering: hrdr_agent::steering_queue(),
                running: false,
                done: true,
                delivered: true,
                pinned: false,
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
        panes.focus(PaneId::Sub(7));
        panes.sync(&live);
        live.prune();
        panes.sync(&live);

        let keys: Vec<PaneId> = panes.subs().iter().map(|p| p.id).collect();
        assert_eq!(keys, vec![PaneId::Sub(7)], "the pane being read is kept");
        assert_eq!(panes.active(), PaneId::Sub(7));

        // Switch back to main: nothing pins it now, so it is released and the
        // switcher is back to just the main row.
        panes.focus(PaneId::Main);
        panes.sync(&live);
        live.prune();
        panes.sync(&live);
        assert!(panes.subs().is_empty());
        assert_eq!(pane_rows(&panes).len(), 1);
    }
}
