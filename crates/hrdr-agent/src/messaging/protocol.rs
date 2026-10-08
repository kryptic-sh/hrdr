use std::{io, time::Duration};

use hrdr_tools::local_ipc::{Connection, EndpointId, SessionDescriptor};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(super) const VERSION: u32 = 1;
pub(super) const DEADLINE: Duration = Duration::from_secs(2);
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hello {
    pub version: u32,
    pub descriptor: SessionDescriptor,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRef {
    pub endpoint: EndpointId,
    pub session_id: String,
}

impl SessionRef {
    pub fn matches(&self, descriptor: &SessionDescriptor) -> bool {
        self.endpoint == descriptor.endpoint && self.session_id == descriptor.session_id
    }
}

impl From<&SessionDescriptor> for SessionRef {
    fn from(value: &SessionDescriptor) -> Self {
        Self {
            endpoint: value.endpoint.clone(),
            session_id: value.session_id.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Payload {
    Probe,
    Message { sender: SessionRef, text: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub version: u32,
    pub receiver: SessionRef,
    pub payload: Payload,
}

/// Acceptance means enqueued, not consumed. IO failure after sending is ambiguous;
/// callers must not automatically resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    Accepted,
    QueueFull,
    SessionChanged,
    Unavailable,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub version: u32,
    pub status: SendStatus,
}

pub(super) async fn read<T: DeserializeOwned>(connection: &mut Connection) -> io::Result<T> {
    Ok(serde_json::from_slice(
        &connection.read_frame(DEADLINE).await?,
    )?)
}

pub(super) async fn write<T: Serialize>(connection: &mut Connection, value: &T) -> io::Result<()> {
    connection
        .write_frame(&serde_json::to_vec(value)?, DEADLINE)
        .await
}

pub(super) fn version(version: u32) -> io::Result<()> {
    if version == VERSION {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported messaging version",
        ))
    }
}
