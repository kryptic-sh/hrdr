//! Session messaging for a headless run (`hrdr run …`).
//!
//! The run registers for the length of its prompt's turn, so the user's other
//! sessions can find it and steer it while it works. Peer messages join the
//! turn one at a time on its steering queue. When the prompt's turn ends,
//! admission closes and whatever was already accepted runs as further turns, so
//! the command still ends: nothing new can arrive, and the inbox is bounded.

use std::sync::Arc;

use hrdr_agent::SteeringQueue;
use hrdr_agent::messaging::Messaging;

pub(crate) struct HeadlessMessaging {
    runtime: Arc<Messaging>,
}

impl HeadlessMessaging {
    /// Register this run for discovery, named after its prompt. `None` when
    /// messaging is off; a failure to register is said on stderr and the run
    /// goes on without it.
    pub(crate) async fn start(
        enabled: bool,
        cwd: &std::path::Path,
        prompt: &str,
        steering: &SteeringQueue,
        quiet: bool,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }
        let cwd = cwd.display().to_string();
        let name = hrdr_agent::session_name_from(&[hrdr_agent::Message::user(prompt)])
            .unwrap_or_else(|| hrdr_agent::default_session_name(&cwd));
        let started = async {
            let directory = hrdr_tools::local_ipc::UserDirectory::open()?;
            Messaging::start(directory, format!("run-{}", std::process::id()), name, cwd).await
        }
        .await;
        let runtime = match started {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                if !quiet {
                    crate::chrome_line(
                        crossterm::style::Color::DarkGrey,
                        &format!("[session messaging unavailable: {error}]"),
                        "",
                    );
                }
                return None;
            }
        };
        let forwarder = runtime.clone();
        let steering = steering.clone();
        tokio::spawn(async move {
            while forwarder.is_open() {
                forwarder.arrival().await;
                feed(&forwarder, &steering);
            }
        });
        Some(Self { runtime })
    }

    /// Close admission once the prompt's turn is over, then hand the agent the
    /// next accepted message. Returns whether there is another turn to run.
    pub(crate) async fn next_turn(&self, steering: &SteeringQueue) -> bool {
        self.runtime.close().await;
        feed(&self.runtime, steering);
        !steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Accepted messages the run ends without delivering — left only when a
    /// turn failed and the run stopped early.
    pub(crate) async fn finish(self) -> usize {
        self.runtime.close().await
    }
}

/// Move one accepted message onto the steering queue, unless one is already
/// waiting there: the rest stay in the bounded inbox until the agent reads it.
fn feed(runtime: &Messaging, steering: &SteeringQueue) {
    let mut queue = steering
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.iter().any(|steer| steer.peer.is_some()) {
        return;
    }
    if let Some(steer) = runtime.dequeue() {
        queue.push_back(steer);
    }
}
