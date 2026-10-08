//! Authenticated local session messaging, independent of the agent and frontend.
mod protocol;
#[cfg(test)]
mod tests;

use crate::{PeerIdentity, PeerMessage, Steer};
use hrdr_tools::local_ipc::{Connection, Listener, Registration, SessionDescriptor, UserDirectory};
use protocol::{DEADLINE, Hello, Payload, Reply, Request, SessionRef, VERSION};
pub use protocol::{MAX_MESSAGE_BYTES, SendStatus};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep, timeout},
};

pub const MAX_INBOX_MESSAGES: usize = 128;
pub const MAX_INBOX_BYTES: usize = 1024 * 1024;
const MAX_WORKERS: usize = 16;
const MAX_PROBES: usize = 64;

struct State {
    descriptor: SessionDescriptor,
    open: bool,
    inbox: VecDeque<PeerMessage>,
    bytes: usize,
}
struct Shared {
    state: Mutex<State>,
    notify: Notify,
    #[cfg(test)]
    worker_exit: Mutex<Option<Arc<tests::WorkerExit>>>,
}
struct Refresh {
    name: String,
    cwd: String,
    reply: oneshot::Sender<io::Result<()>>,
}

/// Owns listener lifetime. Explicitly close and drain pending messages before drop
/// when the frontend must report cancellation. Drop aborts all transport workers.
pub struct Messaging {
    directory: Arc<UserDirectory>,
    shared: Arc<Shared>,
    refresh: mpsc::Sender<Refresh>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Messaging {
    pub async fn start(
        directory: UserDirectory,
        session_id: String,
        name: String,
        cwd: String,
    ) -> io::Result<Self> {
        let listener = retry(|| Listener::bind(&directory)).await?;
        let descriptor = SessionDescriptor {
            endpoint: listener.endpoint().clone(),
            session_id,
            session_name: name,
            working_directory: cwd,
        };
        let registration = retry(|| Registration::publish(&listener, descriptor.clone())).await?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                descriptor,
                open: true,
                inbox: VecDeque::new(),
                bytes: 0,
            }),
            notify: Notify::new(),
            #[cfg(test)]
            worker_exit: Mutex::new(None),
        });
        let directory = Arc::new(directory);
        let (refresh, commands) = mpsc::channel(1);
        let (shutdown, stopping) = oneshot::channel();
        let task = tokio::spawn(serve(
            listener,
            registration,
            commands,
            stopping,
            directory.clone(),
            shared.clone(),
        ));
        Ok(Self {
            directory,
            shared,
            refresh,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    /// Internal endpoint-bearing selection; do not serialize this into model tools.
    pub fn descriptor(&self) -> SessionDescriptor {
        self.shared.state.lock().unwrap().descriptor.clone()
    }

    pub async fn refresh(&self, name: String, cwd: String) -> io::Result<()> {
        let (reply, result) = oneshot::channel();
        self.refresh
            .send(Refresh { name, cwd, reply })
            .await
            .map_err(|_| closed())?;
        result.await.map_err(|_| closed())?
    }

    /// Only kernel-authenticated live probes are returned. A bounded sequential
    /// scan avoids creating a connection/task for every registry candidate.
    pub async fn discover(&self) -> io::Result<Vec<SessionDescriptor>> {
        let candidates = retry(|| self.directory.list_candidates()).await?;
        if candidates.len() > MAX_PROBES {
            return Err(io::Error::other(
                "messaging discovery probe budget exceeded",
            ));
        }
        let mut live = Vec::new();
        for candidate in candidates {
            match exchange(&self.directory, &candidate, Payload::Probe).await {
                Ok((SendStatus::Accepted, descriptor)) => live.push(descriptor),
                Ok(_) => {}
                Err(error) if unavailable(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(live)
    }

    pub async fn send(&self, target: &SessionDescriptor, text: String) -> io::Result<SendStatus> {
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "peer message exceeds byte limit",
            ));
        }
        let sender = {
            let state = self.shared.state.lock().unwrap();
            if !state.open {
                return Ok(SendStatus::Unavailable);
            }
            SessionRef::from(&state.descriptor)
        };
        // Once payload IO starts, even a disconnect can mean the receiver accepted
        // it. Preserve that error rather than claiming delivery failed safely.
        exchange(&self.directory, target, Payload::Message { sender, text })
            .await
            .map(|(status, _)| status)
    }

    /// Removing a message releases its byte and count budget, not wakeup polling.
    pub fn dequeue(&self) -> Option<Steer> {
        let mut state = self.shared.state.lock().unwrap();
        let message = state.inbox.pop_front()?;
        state.bytes -= message_bytes(&message);
        Some(Steer::peer(message))
    }

    /// Single frontend consumer. Register before inspecting the queue so arrival
    /// between the idle check and awaiting cannot be lost.
    pub async fn wait(&self) {
        let notified = self.shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = self.shared.state.lock().unwrap();
            if !state.inbox.is_empty() || !state.open {
                return;
            }
        }
        notified.await;
    }

    /// Refuse admission atomically and return the pending cancellation count.
    /// Pending typed messages remain available through dequeue after closing.
    pub async fn close(&mut self) -> usize {
        let pending = {
            let mut state = self.shared.state.lock().unwrap();
            state.open = false;
            state.inbox.len()
        };
        self.shared.notify.notify_one();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = &mut self.task {
            let _ = task.await;
            self.task.take();
        }
        pending
    }
}

impl Drop for Messaging {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().open = false;
        self.shared.notify.notify_one();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve(
    mut listener: Listener,
    mut registration: Registration,
    mut commands: mpsc::Receiver<Refresh>,
    mut stopping: oneshot::Receiver<()>,
    directory: Arc<UserDirectory>,
    shared: Arc<Shared>,
) {
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopping => break,
            Some(command) = commands.recv() => {
                let mut descriptor = shared.state.lock().unwrap().descriptor.clone();
                descriptor.session_name = command.name;
                descriptor.working_directory = command.cwd;
                let result = retry(|| registration.refresh(&listener, descriptor.clone())).await;
                if result.is_ok() { shared.state.lock().unwrap().descriptor = descriptor; }
                let _ = command.reply.send(result);
            }
            Some(_) = workers.join_next(), if !workers.is_empty() => {}
            accepted = listener.accept(DEADLINE), if workers.len() < MAX_WORKERS => {
                match accepted {
                    Ok(connection) => {
                        let directory = directory.clone();
                        let shared = shared.clone();
                        workers.spawn(async move {
                            #[cfg(test)]
                            let _exit = tests::WorkerExitGuard(shared.worker_exit.lock().unwrap().clone());
                            timeout(DEADLINE, receive(connection, &directory, &shared)).await
                        });
                    }
                    Err(error) if unavailable(&error) => {},
                    Err(_) => break,
                }
            }
        }
    }
    shared.state.lock().unwrap().open = false;
    shared.notify.notify_one();
    workers.abort_all();
    while workers.join_next().await.is_some() {}
}

async fn receive(
    mut connection: Connection,
    directory: &UserDirectory,
    shared: &Shared,
) -> io::Result<()> {
    let descriptor = shared.state.lock().unwrap().descriptor.clone();
    protocol::write(
        &mut connection,
        &Hello {
            version: VERSION,
            descriptor: descriptor.clone(),
        },
    )
    .await?;
    let request: Request = protocol::read(&mut connection).await?;
    protocol::version(request.version)?;
    let status = if !request.receiver.matches(&descriptor) {
        SendStatus::SessionChanged
    } else {
        match request.payload {
            Payload::Probe => {
                if shared.state.lock().unwrap().open {
                    SendStatus::Accepted
                } else {
                    SendStatus::Unavailable
                }
            }
            Payload::Message { sender, text } => {
                if text.len() > MAX_MESSAGE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "peer message exceeds byte limit",
                    ));
                }
                let candidates = retry(|| directory.list_candidates()).await?;
                let sender = candidates
                    .into_iter()
                    .find(|candidate| {
                        sender.matches(candidate)
                            && candidate.endpoint.pid() == connection.peer_pid()
                    })
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::PermissionDenied, "unregistered peer sender")
                    })?;
                let message = PeerMessage {
                    sender: PeerIdentity {
                        session_id: sender.session_id,
                        session_name: sender.session_name,
                        cwd: sender.working_directory,
                        agent: "main".into(),
                    },
                    body: text,
                };
                let mut state = shared.state.lock().unwrap();
                if !state.open {
                    SendStatus::Unavailable
                } else if state.inbox.len() >= MAX_INBOX_MESSAGES
                    || state.bytes + message_bytes(&message) > MAX_INBOX_BYTES
                {
                    SendStatus::QueueFull
                } else {
                    state.bytes += message_bytes(&message);
                    state.inbox.push_back(message);
                    shared.notify.notify_one();
                    SendStatus::Accepted
                }
            }
        }
    };
    protocol::write(
        &mut connection,
        &Reply {
            version: VERSION,
            status,
        },
    )
    .await
}

async fn exchange(
    directory: &UserDirectory,
    target: &SessionDescriptor,
    payload: Payload,
) -> io::Result<(SendStatus, SessionDescriptor)> {
    timeout(DEADLINE, async {
        let mut connection = match Connection::connect(directory, &target.endpoint, DEADLINE).await
        {
            Ok(connection) => connection,
            Err(error) if unavailable(&error) => {
                return Ok((SendStatus::Unavailable, target.clone()));
            }
            Err(error) => return Err(error),
        };
        let hello: Hello = protocol::read(&mut connection).await?;
        protocol::version(hello.version)?;
        if !SessionRef::from(target).matches(&hello.descriptor) {
            return Ok((SendStatus::SessionChanged, hello.descriptor));
        }
        protocol::write(
            &mut connection,
            &Request {
                version: VERSION,
                receiver: SessionRef::from(target),
                payload,
            },
        )
        .await?;
        let reply: Reply = protocol::read(&mut connection).await?;
        protocol::version(reply.version)?;
        Ok((reply.status, hello.descriptor))
    })
    .await?
}

async fn retry<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let end = Instant::now() + DEADLINE;
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < end => {
                sleep(Duration::from_millis(5)).await
            }
            result => return result,
        }
    }
}
fn unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
    )
}
fn message_bytes(message: &PeerMessage) -> usize {
    message.body.len()
        + message.sender.session_id.len()
        + message.sender.session_name.len()
        + message.sender.cwd.len()
        + message.sender.agent.len()
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "messaging runtime closed")
}
