//! Session messaging: publish this session to the user's other hrdr sessions,
//! keep its registration in step with the session, and deliver what peers send.
//!
//! Peer messages wait in the runtime's bounded inbox until the agent can take
//! them. An idle agent is woken by one; a busy agent gets one at a time on its
//! steering queue, the next only once the agent has read the last, so the
//! inbox's byte and count limits still bound everything waiting for it.

use super::messaging_tools::{MessageSessionTool, SessionsTool};
use super::*;

/// What a registration publishes: the session id peers address, and the name
/// and working directory they are shown.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Published {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: String,
}

/// A started runtime and the registration it currently publishes.
pub(crate) struct ActiveMessaging {
    runtime: Arc<hrdr_agent::messaging::Messaging>,
    /// What was last published, or last attempted if that failed.
    published: Published,
    /// Published under [`provisional_session_id`], before the session had an id.
    provisional: bool,
    refreshing: bool,
}

/// The id a session is published under before it has one of its own. A session
/// is named, and its id minted, from its first message; until then it is still
/// a running session other sessions can find and message.
pub(crate) fn provisional_session_id() -> String {
    format!("new-{}", std::process::id())
}

/// The messaging lifecycle for the session's main agent.
pub(crate) struct MessagingState {
    enabled: bool,
    active: Option<ActiveMessaging>,
    /// Publishes the active runtime to the model's messaging tools.
    slot: tokio::sync::watch::Sender<Option<Arc<hrdr_agent::messaging::Messaging>>>,
    /// The session id a start is in flight for.
    starting: Option<String>,
    /// The session id whose start failed; not retried until the session changes.
    failed: Option<String>,
    /// Peer messages dropped by the closing runtime at exit, reported once the
    /// terminal is restored.
    pub(crate) undelivered_on_exit: usize,
}

impl MessagingState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            active: None,
            slot: tokio::sync::watch::Sender::new(None),
            starting: None,
            failed: None,
            undelivered_on_exit: 0,
        }
    }

    /// Give the agent the tools that list and message other sessions. Only the
    /// interactive main agent gets them (see `Agent::register_tool`).
    pub(crate) fn register_tools(&self, agent: &mut Agent) {
        if !self.enabled {
            return;
        }
        agent.register_tool(Arc::new(SessionsTool {
            view: self.slot.subscribe(),
        }));
        agent.register_tool(Arc::new(MessageSessionTool {
            view: self.slot.subscribe(),
        }));
    }

    fn set_active(&mut self, active: Option<ActiveMessaging>) {
        self.slot
            .send_replace(active.as_ref().map(|active| active.runtime.clone()));
        self.active = active;
    }

    fn take_active(&mut self) -> Option<ActiveMessaging> {
        let active = self.active.take();
        self.set_active(None);
        active
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_starting(&self) -> bool {
        self.starting.is_some()
    }

    #[cfg(test)]
    pub(crate) fn view(&self) -> super::messaging_tools::MessagingView {
        self.slot.subscribe()
    }

    /// Accepted peer messages the agent has not been handed yet.
    pub(crate) fn pending(&self) -> usize {
        self.active
            .as_ref()
            .map_or(0, |active| active.runtime.pending())
    }
}

impl App {
    /// What this session should be published as now, and whether that id is
    /// the provisional one.
    fn wanted_registration(&self) -> (Published, bool) {
        let (id, provisional) = match self.state().id.clone() {
            Some(id) => (id, false),
            None => (provisional_session_id(), true),
        };
        let published = Published {
            name: published_name(&self.state().name, &id),
            cwd: self.dir_cwd.display().to_string(),
            id,
        };
        (published, provisional)
    }

    /// Bring the published registration in line with the session: start it as
    /// the app opens, rotate it when the session is replaced, and refresh the id,
    /// name and working directory peers see when any of them changes.
    ///
    /// A session with no id yet is published under [`provisional_session_id`].
    /// When its first message mints the real id, the same registration is
    /// renamed rather than rotated: it is the same conversation, so the endpoint
    /// and any message already waiting in its inbox carry over.
    pub(crate) fn sync_messaging(&mut self) {
        if !self.messaging.enabled {
            return;
        }
        let (wanted, provisional) = self.wanted_registration();
        if self
            .messaging
            .active
            .as_ref()
            .is_some_and(|active| !active.provisional && active.published.id != wanted.id)
        {
            let previous = self.messaging.take_active().expect("checked above");
            self.close_messaging(previous);
        }
        if let Some(active) = self.messaging.active.as_mut() {
            if active.refreshing || active.published == wanted {
                return;
            }
            active.refreshing = true;
            let runtime = active.runtime.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let Published { id, name, cwd } = wanted.clone();
                let result = runtime.refresh(id, name, cwd).await;
                let _ = tx
                    .send(TurnMsg::MessagingRefreshed(
                        runtime,
                        wanted,
                        provisional,
                        result.map_err(|error| error.to_string()),
                    ))
                    .await;
            });
            return;
        }
        if self.messaging.starting.is_some() || self.messaging.failed.as_ref() == Some(&wanted.id) {
            return;
        }
        self.messaging.starting = Some(wanted.id.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let Published { id, name, cwd } = wanted.clone();
            let result = async {
                let directory = hrdr_tools::local_ipc::UserDirectory::open()?;
                hrdr_agent::messaging::Messaging::start(directory, id, name, cwd).await
            }
            .await;
            let _ = tx
                .send(TurnMsg::MessagingStarted(
                    wanted,
                    provisional,
                    result.map(Arc::new).map_err(|error| error.to_string()),
                ))
                .await;
        });
    }

    pub(crate) fn on_messaging_started(
        &mut self,
        published: Published,
        provisional: bool,
        result: Result<Arc<hrdr_agent::messaging::Messaging>, String>,
    ) {
        if self.messaging.starting.as_ref() == Some(&published.id) {
            self.messaging.starting = None;
        }
        match result {
            Ok(runtime) => {
                let active = ActiveMessaging {
                    runtime,
                    published,
                    provisional,
                    refreshing: false,
                };
                // The session moved on while the start was in flight. A
                // provisional start that the session's new id overtook is kept:
                // the next sync renames it.
                let moved_on =
                    !provisional && self.state().id.as_ref() != Some(&active.published.id);
                if moved_on || self.messaging.active.is_some() {
                    self.close_messaging(active);
                } else {
                    self.forward_arrivals(active.runtime.clone());
                    self.messaging.set_active(Some(active));
                }
            }
            Err(error) => {
                self.messaging.failed = Some(published.id);
                self.system(format!("session messaging unavailable: {error}"));
            }
        }
    }

    pub(crate) fn on_messaging_refreshed(
        &mut self,
        runtime: Arc<hrdr_agent::messaging::Messaging>,
        published: Published,
        provisional: bool,
        result: Result<(), String>,
    ) {
        let Some(active) = self
            .messaging
            .active
            .as_mut()
            .filter(|active| Arc::ptr_eq(&active.runtime, &runtime))
        else {
            return;
        };
        active.refreshing = false;
        // Recorded even on failure, so a value the registry refuses is reported
        // once rather than retried on every frame. A rename the registry refused
        // leaves the registration under its old id, so it stays provisional.
        if result.is_ok() {
            active.provisional = provisional;
        }
        active.published = published;
        if let Err(error) = result {
            self.system(format!(
                "session messaging: updating this session failed: {error}"
            ));
        }
    }

    /// Wake the loop for every arrival: an idle loop does not tick, so without
    /// this a message would wait for the next keypress. Ends once the runtime
    /// closes.
    fn forward_arrivals(&self, runtime: Arc<hrdr_agent::messaging::Messaging>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while runtime.is_open() {
                runtime.arrival().await;
                if tx.send(TurnMsg::PeerReady).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Close a runtime the session no longer publishes, reporting what it drops.
    fn close_messaging(&self, previous: ActiveMessaging) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let undelivered = previous.runtime.close().await;
            if undelivered > 0 {
                let _ = tx
                    .send(TurnMsg::System(format!(
                        "{undelivered} peer message(s) for the previous session were not delivered"
                    )))
                    .await;
            }
        });
    }

    /// Close the runtime as the app exits, counting the messages it drops: those
    /// still in its inbox, and the one already queued behind a turn the quit
    /// cancels.
    pub(crate) async fn shutdown_messaging(&mut self) {
        if let Some(active) = self.messaging.take_active() {
            self.messaging.undelivered_on_exit +=
                active.runtime.close().await + self.registry.pending_peers(hrdr_agent::MAIN_KEY);
        }
    }

    /// Hand the agent the next peer message it can take: wake it when idle, or
    /// queue one behind its current turn when it holds none already.
    pub(crate) fn maybe_deliver_peer(&mut self) {
        // Once quitting, what is left stays in the inbox for the exit report.
        if self.should_quit
            || self.handoff_gate
            || self.compacting()
            || self
                .user_shell
                .as_ref()
                .is_some_and(|shell| !shell.handle.is_finished())
        {
            return;
        }
        let Some(active) = self.messaging.active.as_ref() else {
            return;
        };
        // A turn whose `Done` is still in flight counts as running: the agent is
        // already marked idle then, but launching here would race that `Done`,
        // which relaunches for anything left on the queue anyway.
        let running = self.running() || self.turn_handle.is_some();
        if running && self.registry.has_pending_peer(hrdr_agent::MAIN_KEY) {
            return;
        }
        let Some(steer) = active.runtime.dequeue() else {
            return;
        };
        if !running {
            // A message can now wake a session nobody has typed into yet. Claim its
            // id before the turn, as a typed first message does, so a sub-agent the
            // turn spawns has its transcript directory.
            self.reserve_session_id(&steer.sent);
        }
        self.registry.enqueue(hrdr_agent::MAIN_KEY, steer);
        if !running {
            self.launch_turn();
        }
    }
}

/// The name peers see: the session's own, or its id while it has none yet (a
/// session is named on its first save). The registry refuses an empty field and
/// caps its length, so a long name is cut to fit rather than unpublished.
pub(super) fn published_name(name: &str, id: &str) -> String {
    let name = name.trim();
    let name = if name.is_empty() { id } else { name };
    let max = hrdr_tools::local_ipc::MAX_SESSION_NAME_BYTES;
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].replace('\0', "")
}

#[cfg(test)]
mod tests {
    use super::published_name;

    #[test]
    fn an_unnamed_session_publishes_its_id_and_long_names_fit() {
        assert_eq!(published_name("  ", "session-1"), "session-1");
        assert_eq!(published_name(" release ", "session-1"), "release");
        let max = hrdr_tools::local_ipc::MAX_SESSION_NAME_BYTES;
        let long = "é".repeat(max);
        let published = published_name(&long, "id");
        assert!(published.len() <= max && published.len() > max - 2);
        assert!(published.chars().all(|c| c == 'é'));
    }
}
