use super::{EndpointId, Listener, UserDirectory, storage};
use serde::{Deserialize, Serialize};
use std::io::{self, Read};

pub const MAX_RECORD_BYTES: usize = 32 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 4096;
pub const MAX_SESSION_ID_BYTES: usize = 256;
pub const MAX_SESSION_NAME_BYTES: usize = 1024;
pub const MAX_WORKING_DIRECTORY_BYTES: usize = 4096;
const VERSION: u32 = 1;

/// Public discovery metadata only; never put credentials or message contents here.
/// These records are untrusted candidates, not evidence of a live session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DescriptorWire", into = "DescriptorWire")]
pub struct SessionDescriptor {
    pub endpoint: EndpointId,
    pub session_id: String,
    pub session_name: String,
    pub working_directory: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorWire {
    version: u32,
    endpoint: EndpointId,
    session_id: String,
    session_name: String,
    working_directory: String,
}

impl SessionDescriptor {
    fn validate(&self) -> io::Result<()> {
        for (value, max) in [
            (&self.session_id, MAX_SESSION_ID_BYTES),
            (&self.session_name, MAX_SESSION_NAME_BYTES),
            (&self.working_directory, MAX_WORKING_DIRECTORY_BYTES),
        ] {
            if value.is_empty() || value.len() > max || value.contains('\0') {
                return Err(invalid("invalid IPC descriptor field"));
            }
        }
        Ok(())
    }
}

impl TryFrom<DescriptorWire> for SessionDescriptor {
    type Error = io::Error;
    fn try_from(wire: DescriptorWire) -> io::Result<Self> {
        if wire.version != VERSION {
            return Err(invalid("unsupported IPC descriptor version"));
        }
        let descriptor = Self {
            endpoint: wire.endpoint,
            session_id: wire.session_id,
            session_name: wire.session_name,
            working_directory: wire.working_directory,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }
}

impl From<SessionDescriptor> for DescriptorWire {
    fn from(value: SessionDescriptor) -> Self {
        Self {
            version: VERSION,
            endpoint: value.endpoint,
            session_id: value.session_id,
            session_name: value.session_name,
            working_directory: value.working_directory,
        }
    }
}

/// Published metadata. Dropping this value deliberately leaves a stale candidate:
/// check-then-unlink could delete a replacement. Runtime discovery must live-probe
/// and authenticate candidates. Hostile same-user code is not a security principal.
pub struct Registration {
    descriptor: SessionDescriptor,
}

impl Registration {
    /// Requires a bound listener and publishes only its endpoint generation.
    pub fn publish(listener: &Listener, descriptor: SessionDescriptor) -> io::Result<Self> {
        descriptor.validate()?;
        if &descriptor.endpoint != listener.endpoint() {
            return Err(invalid("descriptor does not match bound listener"));
        }
        let bytes = serde_json::to_vec(&descriptor)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(invalid("IPC descriptor too large"));
        }
        let directory = &listener.directory;
        let mutation = storage::MutationGuard::acquire(directory)?;
        mutation.publish(directory, &descriptor.endpoint, &bytes)?;
        Ok(Self { descriptor })
    }

    /// Refresh descriptive metadata without changing the bound endpoint generation.
    pub fn refresh(
        &mut self,
        listener: &Listener,
        descriptor: SessionDescriptor,
    ) -> io::Result<()> {
        if descriptor.endpoint != self.descriptor.endpoint {
            return Err(invalid("cannot change registration endpoint"));
        }
        *self = Self::publish(listener, descriptor)?;
        Ok(())
    }

    pub fn descriptor(&self) -> &SessionDescriptor {
        &self.descriptor
    }
}

impl UserDirectory {
    /// Return raw candidates, never live sessions. A malformed/insecure record or
    /// exhausted scan budget fails the listing, rather than treating it as valid.
    /// All entries count toward the budget, including sockets and temporary files.
    pub fn list_candidates(&self) -> io::Result<Vec<SessionDescriptor>> {
        let mutation = storage::MutationGuard::acquire(self)?;
        let mut candidates = Vec::new();
        for name in mutation.reap(self)? {
            let text = name
                .to_str()
                .ok_or_else(|| invalid("non-Unicode IPC directory entry"))?;
            let Some(stem) = text.strip_suffix(".json") else {
                continue;
            };
            let endpoint = EndpointId::from_name(stem)
                .ok_or_else(|| invalid("invalid IPC descriptor filename"))?;
            let file = storage::read(self, &record_name(&endpoint))?;
            if file.metadata()?.len() > MAX_RECORD_BYTES as u64 {
                return Err(invalid("IPC descriptor too large"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_RECORD_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(invalid("IPC descriptor grew beyond limit"));
            }
            let descriptor: SessionDescriptor = serde_json::from_slice(&bytes)?;
            if descriptor.endpoint != endpoint {
                return Err(invalid("IPC descriptor filename mismatch"));
            }
            candidates.push(descriptor);
        }
        Ok(candidates)
    }
}

fn record_name(endpoint: &EndpointId) -> String {
    format!("{}.json", endpoint.name())
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "registration_tests.rs"]
mod tests;
