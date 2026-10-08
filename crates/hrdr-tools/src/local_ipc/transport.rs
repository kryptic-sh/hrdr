use std::{io, time::Duration};

use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

use super::UserDirectory;

#[cfg(unix)]
#[path = "transport_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "transport_windows.rs"]
mod platform;

/// Maximum byte payload in one frame (the length prefix is not included).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A process and random bind generation, never an arbitrary filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EndpointWire", into = "EndpointWire")]
pub struct EndpointId {
    pid: u32,
    generation: [u8; 16],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointWire {
    pid: u32,
    generation: [u8; 16],
}

impl TryFrom<EndpointWire> for EndpointId {
    type Error = &'static str;

    fn try_from(value: EndpointWire) -> Result<Self, Self::Error> {
        if value.pid == 0 || value.generation == [0; 16] {
            return Err("invalid IPC endpoint identity");
        }
        Ok(Self {
            pid: value.pid,
            generation: value.generation,
        })
    }
}

impl From<EndpointId> for EndpointWire {
    fn from(value: EndpointId) -> Self {
        Self {
            pid: value.pid,
            generation: value.generation,
        }
    }
}

impl EndpointId {
    fn fresh() -> Self {
        let mut generation = [0; 16];
        while generation == [0; 16] {
            rand::rng().fill(&mut generation);
        }
        Self {
            pid: std::process::id(),
            generation,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn name(&self) -> String {
        let generation = u128::from_be_bytes(self.generation);
        format!("{}-{generation:032x}", self.pid)
    }
}

/// A listener retaining its private directory and endpoint generation.
pub struct Listener {
    inner: platform::Listener,
    endpoint: EndpointId,
}

impl Listener {
    /// Bind a fresh endpoint. Never removes an existing endpoint to make room.
    pub fn bind(directory: &UserDirectory) -> io::Result<Self> {
        let endpoint = EndpointId::fresh();
        Ok(Self {
            inner: platform::Listener::bind(directory, &endpoint)?,
            endpoint,
        })
    }

    pub fn endpoint(&self) -> &EndpointId {
        &self.endpoint
    }

    /// Accept only authenticated same-user peers, within the supplied deadline.
    pub async fn accept(&mut self, deadline: Duration) -> io::Result<Connection> {
        timeout(deadline, self.inner.accept()).await?
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Authenticated peer with length-delimited bytes, not an application protocol.
///
/// Failed or cancelled frame IO closes the connection: partial IO must never be
/// retried as a new frame on a desynchronized stream. Oversized writes are rejected
/// before touching the stream and leave the connection usable.
pub struct Connection {
    stream: Option<Box<dyn Stream>>,
    peer_pid: u32,
}

impl Connection {
    fn new(stream: impl Stream + 'static, peer_pid: u32) -> Self {
        Self {
            stream: Some(Box::new(stream)),
            peer_pid,
        }
    }

    pub async fn connect(
        directory: &UserDirectory,
        endpoint: &EndpointId,
        deadline: Duration,
    ) -> io::Result<Self> {
        timeout(deadline, platform::connect(directory, endpoint)).await?
    }

    pub fn peer_pid(&self) -> u32 {
        self.peer_pid
    }

    pub async fn read_frame(&mut self, deadline: Duration) -> io::Result<Vec<u8>> {
        let mut stream = self.stream.take().ok_or_else(closed)?;
        let payload = timeout(deadline, async {
            let length = stream.read_u32().await? as usize;
            if length > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "IPC frame too large",
                ));
            }
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload).await?;
            Ok(payload)
        })
        .await??;
        self.stream = Some(stream);
        Ok(payload)
    }

    pub async fn write_frame(&mut self, payload: &[u8], deadline: Duration) -> io::Result<()> {
        if payload.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC frame too large",
            ));
        }
        let mut stream = self.stream.take().ok_or_else(closed)?;
        timeout(deadline, async {
            stream.write_u32(payload.len() as u32).await?;
            stream.write_all(payload).await?;
            stream.flush().await
        })
        .await??;
        self.stream = Some(stream);
        Ok(())
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "IPC connection closed")
}

fn verify_pid(pid: u32, expected: Option<u32>) -> io::Result<u32> {
    if pid == 0 || expected.is_some_and(|expected| expected != pid) {
        return Err(super::insecure());
    }
    Ok(pid)
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
