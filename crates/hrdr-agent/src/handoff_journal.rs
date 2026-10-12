//! Best-effort, append-only handoff checkpoints for a saved session.
//!
//! The journal is deliberately bound to the bytes of the saved `<id>.json` snapshot,
//! rather than to an assertion supplied by a caller. Frames are flushed and synced,
//! but this is not a strict power-loss durability protocol: a sudden loss can lose or
//! ambiguously preserve the newest frame.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{Session, SessionLock, sanitize_name, session_file_path};

const MAGIC: [u8; 4] = *b"HRHJ";
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 4 + 1 + 4 + 8 + 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 1024;

/// A snapshot open or read failure encountered while attesting a handoff journal.
///
/// This marker distinguishes OS-level snapshot access failures from malformed
/// snapshots and every other journal validation failure.
#[derive(Debug)]
pub struct SnapshotIoError(anyhow::Error);

impl std::fmt::Display for SnapshotIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for SnapshotIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// A non-empty provider operation identifier for a successful update.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct UpdateOperationId(String);

impl UpdateOperationId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        anyhow::ensure!(!value.trim().is_empty(), "update operation id is empty");
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mint an opaque operation id from operating-system randomness.
    pub fn random() -> Self {
        use rand::RngExt;

        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        let value = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self(value)
    }
}

/// Durable evidence returned by the actual deferred update tool call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredUpdate {
    pub status: DeferredUpdateStatus,
    pub operation_id: UpdateOperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredUpdateStatus {
    Deferred,
}

/// The explicit recovery action requested by a handoff checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffIntent {
    Idle,
    UpdateContinuation,
}

/// Versioned, quiescent runtime state required before a handoff can be claimed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffPayload {
    version: u8,
    intent: HandoffIntent,
    operation_id: Option<UpdateOperationId>,
    runtime_queue: bool,
    runtime_tasks: bool,
    runtime_tools: bool,
    runtime_turn: bool,
    runtime_compaction: bool,
    runtime_watch: bool,
}

impl HandoffPayload {
    /// Parse only the current schema and reject any active runtime state.
    pub fn parse(bytes: &[u8]) -> Result<HandoffIntent> {
        Ok(Self::parse_typed(bytes)?.0)
    }

    /// Parse the handoff schema and retain the update operation id when required.
    pub fn parse_typed(bytes: &[u8]) -> Result<(HandoffIntent, Option<UpdateOperationId>)> {
        let payload: Self = serde_json::from_slice(bytes)?;
        anyhow::ensure!(payload.version == 1, "unsupported handoff payload version");
        anyhow::ensure!(
            !payload.runtime_queue
                && !payload.runtime_tasks
                && !payload.runtime_tools
                && !payload.runtime_turn
                && !payload.runtime_compaction
                && !payload.runtime_watch,
            "handoff payload contains active runtime work"
        );
        if let Some(operation_id) = &payload.operation_id {
            anyhow::ensure!(
                !operation_id.as_str().trim().is_empty(),
                "update operation id is empty"
            );
        }
        match payload.intent {
            HandoffIntent::Idle => anyhow::ensure!(
                payload.operation_id.is_none(),
                "idle handoff payload must not name an update operation"
            ),
            HandoffIntent::UpdateContinuation => anyhow::ensure!(
                payload.operation_id.is_some(),
                "update continuation handoff payload requires an operation id"
            ),
        }
        Ok((payload.intent, payload.operation_id))
    }

    /// Validate update-continuation evidence in the exact loaded snapshot.
    pub fn validate_snapshot(bytes: &[u8], messages: &[crate::Message]) -> Result<HandoffIntent> {
        let (intent, operation_id) = Self::parse_typed(bytes)?;
        if let Some(operation_id) = operation_id {
            validate_deferred_update(messages, &operation_id)?;
        }
        Ok(intent)
    }
}

/// Find the exact assistant `update` call and its adjacent deferred tool result.
pub fn validate_deferred_update(
    messages: &[crate::Message],
    operation_id: &UpdateOperationId,
) -> Result<()> {
    for (index, message) in messages.iter().enumerate() {
        if message.role != hrdr_llm::Role::Assistant {
            continue;
        }
        for call in message.tool_calls.iter().flatten() {
            if call.kind != "function"
                || call.id.trim().is_empty()
                || call.function.name != "update"
            {
                continue;
            }
            if !is_empty_update_arguments(&call.function.arguments) {
                continue;
            }
            let Some(result) = messages.get(index + 1) else {
                continue;
            };
            if result.role != hrdr_llm::Role::Tool
                || result.tool_call_id.as_deref() != Some(call.id.as_str())
            {
                continue;
            }
            let Some(content) = result.content.as_deref() else {
                continue;
            };
            let Ok(result_evidence) =
                serde_json::from_str::<DeferredUpdate>(tool_result_body(content))
            else {
                continue;
            };
            if result_evidence.operation_id.as_str().trim().is_empty() {
                continue;
            }
            if result_evidence.operation_id == *operation_id {
                return Ok(());
            }
        }
    }
    bail!(
        "saved session lacks a paired deferred update for operation {}",
        operation_id.as_str()
    )
}

fn is_empty_update_arguments(arguments: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(arguments),
        Ok(serde_json::Value::Object(object)) if object.is_empty()
    )
}

fn tool_result_body(content: &str) -> &str {
    let Some((body, timing)) = content.rsplit_once("\n\n(took ") else {
        return content;
    };
    if timing.ends_with(')') { body } else { content }
}

/// The checkpoint phase recorded in a handoff journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HandoffState {
    Prepare = 1,
    Commit = 2,
    OwnedAbandon = 3,
    Claim = 4,
    Abort = 5,
}

impl TryFrom<u8> for HandoffState {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Prepare),
            2 => Ok(Self::Commit),
            3 => Ok(Self::OwnedAbandon),
            4 => Ok(Self::Claim),
            5 => Ok(Self::Abort),
            _ => bail!("unknown handoff state {value}"),
        }
    }
}

/// A validated journal checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRecord {
    pub epoch: u64,
    pub state: HandoffState,
    pub payload: Vec<u8>,
    pub source: hrdr_tools::ProcessLease,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPayload {
    session_hash: [u8; 32],
    source: hrdr_tools::ProcessLease,
    payload: Vec<u8>,
}

struct JournalScan {
    last: Option<HandoffRecord>,
    valid_end: u64,
    truncate_to: Option<u64>,
}

/// A journal tied to one saved session snapshot and its exact backed open-lock.
pub struct HandoffJournal<'a> {
    path: PathBuf,
    session_hash: [u8; 32],
    _lock: Option<&'a SessionLock>,
    append_lock: Arc<Mutex<()>>,
}

impl<'a> HandoffJournal<'a> {
    /// Return whether `error` came from a transient Windows failure opening or
    /// reading the snapshot while this journal was being attested.
    pub fn is_transient_snapshot_io_error(error: &anyhow::Error) -> bool {
        #[cfg(windows)]
        {
            const ERROR_SHARING_VIOLATION: i32 = 32;
            const ERROR_LOCK_VIOLATION: i32 = 33;
            const ERROR_DELETE_PENDING: i32 = 303;

            error
                .chain()
                .any(|cause| cause.downcast_ref::<SnapshotIoError>().is_some())
                && error
                    .chain()
                    .filter_map(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .and_then(std::io::Error::raw_os_error)
                    })
                    .any(|code| {
                        matches!(
                            code,
                            ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_DELETE_PENDING
                        )
                    })
        }
        #[cfg(not(windows))]
        {
            let _ = error;
            false
        }
    }

    /// Return whether the latest valid frame says this snapshot must be recovered
    /// by its handoff owner before an ordinary resume may use it.
    ///
    /// `Prepare` intentionally does not block a resume: it records an attempted
    /// checkpoint, not a handoff that has taken ownership away from the session.
    pub fn requires_recovery(cwd: &str, id: &str, lock: &'a SessionLock) -> Result<bool> {
        let journal = Self::open(cwd, id, lock)?;
        Ok(requires_recovery_state(
            journal.scan()?.last.map(|record| record.state),
        ))
    }

    /// Inspect the current saved snapshot's newest committed checkpoint before its
    /// open-lock is acquired. Only a checksum- and snapshot-bound record from a
    /// certainly dead or replaced source yields a lease.
    pub fn claimable_source(cwd: &str, id: &str) -> Result<Option<hrdr_tools::ProcessLease>> {
        let id = sanitize_name(id);
        let session_path = session_file_path(cwd, &id);
        let session_hash = saved_session_hash(&session_path, &id)?;
        let path = session_path.with_file_name(format!("{id}.handoff"));
        let journal = Self {
            append_lock: journal_lock(&path),
            path,
            session_hash,
            _lock: None,
        };
        let Some(record) = journal.scan()?.last else {
            return Ok(None);
        };
        if !claimable_state(record.state) {
            return Ok(None);
        }
        match hrdr_tools::process_lease_liveness(record.source) {
            hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced => {
                Ok(Some(record.source))
            }
            hrdr_tools::ProcessLiveness::Live => bail!("handoff source owner is still live"),
            hrdr_tools::ProcessLiveness::Unknown => {
                bail!("handoff source owner liveness could not be determined")
            }
        }
    }

    /// Inspect the journal without opening the snapshot.
    ///
    /// This is limited to a transient snapshot I/O retry: it verifies every
    /// complete frame's checksum and the latest frame's state and source, but does
    /// not use a frame's snapshot hash or payload as snapshot validation. The next
    /// recovery attempt performs the normal snapshot-bound validation before claim.
    pub fn has_claimable_source_without_snapshot(
        cwd: &str,
        id: &str,
        source: hrdr_tools::ProcessLease,
    ) -> Result<bool> {
        let id = sanitize_name(id);
        let path = session_file_path(cwd, &id).with_file_name(format!("{id}.handoff"));
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some(record) = latest_checksummed_source(&bytes)? else {
            return Ok(false);
        };
        Ok(claimable_state(record.0) && record.1 == source)
    }

    /// Open the journal for an existing saved session.
    ///
    /// This reads and parses `<id>.json` and hashes its actual bytes. A guard for
    /// another session, an unbacked fallback guard, or an unsaved/invalid snapshot
    /// cannot create a journal.
    pub fn open(cwd: &str, id: &str, lock: &'a SessionLock) -> Result<Self> {
        let id = sanitize_name(id);
        if !lock.is_backed() {
            bail!("handoff journal requires a backed session lock");
        }
        let session_path = session_file_path(cwd, &id);
        if !lock.guards_path(&session_path) {
            bail!("session lock does not guard {}", session_path.display());
        }
        let hash = saved_session_hash(&session_path, &id)?;
        let path = session_path.with_file_name(format!("{id}.handoff"));
        Ok(Self {
            append_lock: journal_lock(&path),
            path,
            session_hash: hash,
            _lock: Some(lock),
        })
    }

    /// Open the journal bound to an already-loaded, exclusively locked snapshot.
    ///
    /// Recovery uses this instead of [`Self::open`] so it parses the snapshot only
    /// once while still hashing the exact bytes protected by its open-lock.
    pub fn open_for_loaded(
        cwd: &str,
        id: &str,
        lock: &'a SessionLock,
        session: &Session,
    ) -> Result<Self> {
        let id = sanitize_name(id);
        if !lock.is_backed() {
            bail!("handoff journal requires a backed session lock");
        }
        let session_path = session_file_path(cwd, &id);
        if !lock.guards_path(&session_path) {
            bail!("session lock does not guard {}", session_path.display());
        }
        if session.state.id.as_deref() != Some(id.as_str()) || session.state.cwd != cwd {
            bail!("loaded session does not match handoff binding");
        }
        let bytes = std::fs::read(&session_path)
            .map_err(|error| SnapshotIoError(error.into()))
            .with_context(|| format!("reading {}", session_path.display()))?;
        let path = session_path.with_file_name(format!("{id}.handoff"));
        Ok(Self {
            append_lock: journal_lock(&path),
            path,
            session_hash: Sha256::digest(bytes).into(),
            _lock: Some(lock),
        })
    }

    /// Append a checkpoint after checking it still belongs to the saved snapshot.
    ///
    /// The source identity is captured from this process, rather than accepted from
    /// the caller. This prevents one process from writing a claim on another's behalf.
    pub fn append(&self, epoch: u64, state: HandoffState, payload: &[u8]) -> Result<()> {
        let source = hrdr_tools::current_process_lease()
            .context("process-instance handoff journals are unsupported on this platform")?;
        self.append_from_source(epoch, state, source, payload)
    }

    fn append_from_source(
        &self,
        epoch: u64,
        state: HandoffState,
        source: hrdr_tools::ProcessLease,
        payload: &[u8],
    ) -> Result<()> {
        let _append_guard = self
            .append_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("handoff journal append lock is poisoned"))?;
        let frame = self.frame(epoch, state, source, payload)?;
        let scan = self.scan()?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        if let Some(prefix) = scan.truncate_to {
            file.set_len(prefix)
                .with_context(|| format!("truncating torn {}", self.path.display()))?;
        }
        file.seek(SeekFrom::End(0))?;
        debug_assert_eq!(file.stream_position()?, scan.valid_end);
        file.write_all(&frame)
            .with_context(|| format!("appending {}", self.path.display()))?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    #[cfg(all(test, windows))]
    pub(crate) fn append_for_source(
        &self,
        epoch: u64,
        state: HandoffState,
        source: hrdr_tools::ProcessLease,
        payload: &[u8],
    ) -> Result<()> {
        self.append_from_source(epoch, state, source, payload)
    }

    /// Claim the newest committed checkpoint after its source lease is certainly gone.
    ///
    /// The scan, liveness check, and durable `Claim` append share one process-local
    /// append lock. A failed append leaves the committed frame in place for retry.
    pub fn claim<T, F>(&self, validate: F) -> Result<Option<(HandoffRecord, T)>>
    where
        F: FnOnce(&[u8]) -> Result<T>,
    {
        let _append_guard = self
            .append_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("handoff journal append lock is poisoned"))?;
        let Some(record) = self.claimable()? else {
            return Ok(None);
        };
        let intent = validate(&record.payload)?;
        let source = hrdr_tools::current_process_lease()
            .context("process-instance handoff journals are unsupported on this platform")?;
        let frame = self.frame(record.epoch, HandoffState::Claim, source, &record.payload)?;
        self.append_frame_locked(&frame)?;
        Ok(Some((record, intent)))
    }

    fn append_frame_locked(&self, frame: &[u8]) -> Result<()> {
        let scan = self.scan()?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        if let Some(prefix) = scan.truncate_to {
            file.set_len(prefix)
                .with_context(|| format!("truncating torn {}", self.path.display()))?;
        }
        file.seek(SeekFrom::End(0))?;
        debug_assert_eq!(file.stream_position()?, scan.valid_end);
        file.write_all(frame)
            .with_context(|| format!("appending {}", self.path.display()))?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    /// Return the newest committed checkpoint if its source is certainly not live.
    pub fn claimable(&self) -> Result<Option<HandoffRecord>> {
        let Some(record) = self.scan()?.last else {
            return Ok(None);
        };
        if !claimable_state(record.state) {
            return Ok(None);
        }
        match hrdr_tools::process_lease_liveness(record.source) {
            hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced => {
                Ok(Some(record))
            }
            hrdr_tools::ProcessLiveness::Live => bail!("handoff source owner is still live"),
            hrdr_tools::ProcessLiveness::Unknown => {
                bail!("handoff source owner liveness could not be determined")
            }
        }
    }

    fn frame(
        &self,
        epoch: u64,
        state: HandoffState,
        source: hrdr_tools::ProcessLease,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        if source.pid == 0 {
            bail!("handoff source lease has PID zero");
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            bail!("handoff payload exceeds {MAX_PAYLOAD_BYTES} bytes");
        }
        let stored = serde_json::to_vec(&StoredPayload {
            session_hash: self.session_hash,
            source,
            payload: payload.to_vec(),
        })?;
        if stored.len() > MAX_PAYLOAD_BYTES {
            bail!("encoded handoff payload exceeds {MAX_PAYLOAD_BYTES} bytes");
        }
        let mut frame = Vec::with_capacity(HEADER_BYTES + stored.len() + CHECKSUM_BYTES);
        frame.extend_from_slice(&MAGIC);
        frame.push(VERSION);
        frame.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        frame.extend_from_slice(&epoch.to_le_bytes());
        frame.push(state as u8);
        frame.extend_from_slice(&stored);
        frame.extend_from_slice(&Sha256::digest(&frame));
        Ok(frame)
    }

    fn scan(&self) -> Result<JournalScan> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JournalScan {
                    last: None,
                    valid_end: 0,
                    truncate_to: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let total = file.metadata()?.len();
        if total > MAX_JOURNAL_BYTES {
            bail!("handoff journal exceeds {MAX_JOURNAL_BYTES} bytes");
        }
        let mut valid_end = 0;
        let mut last = None;
        let mut records = 0;
        loop {
            let start = file.stream_position()?;
            let mut header = [0; HEADER_BYTES];
            let read = read_partial(&mut file, &mut header)?;
            if read == 0 {
                return Ok(JournalScan {
                    last,
                    valid_end,
                    truncate_to: None,
                });
            }
            if read != HEADER_BYTES {
                return self.terminal_corruption(start, total, &mut file, last, valid_end);
            }
            if header[..4] != MAGIC || header[4] != VERSION {
                return self.terminal_corruption(start, total, &mut file, last, valid_end);
            }
            let length = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
            if length > MAX_PAYLOAD_BYTES {
                return self.terminal_corruption(start, total, &mut file, last, valid_end);
            }
            let mut body = vec![0; length + CHECKSUM_BYTES];
            if read_partial(&mut file, &mut body)? != body.len() {
                return self.terminal_corruption(start, total, &mut file, last, valid_end);
            }
            let end = file.stream_position()?;
            let state = match HandoffState::try_from(header[17]) {
                Ok(state) => state,
                Err(_) => {
                    return self.terminal_corruption(start, total, &mut file, last, valid_end);
                }
            };
            let checksum_at = length;
            let mut protected = Vec::with_capacity(HEADER_BYTES + length);
            protected.extend_from_slice(&header);
            protected.extend_from_slice(&body[..checksum_at]);
            if Sha256::digest(&protected).as_slice() != &body[checksum_at..] {
                return self.terminal_corruption(start, total, &mut file, last, valid_end);
            }
            let stored: StoredPayload = serde_json::from_slice(&body[..checksum_at])
                .with_context(|| format!("parsing handoff frame at byte {start}"))?;
            if stored.session_hash != self.session_hash || stored.source.pid == 0 {
                bail!("handoff frame at byte {start} does not match its saved session");
            }
            records += 1;
            if records > MAX_RECORDS {
                bail!("handoff journal exceeds {MAX_RECORDS} records");
            }
            let epoch = u64::from_le_bytes(header[9..17].try_into().unwrap());
            last = Some(HandoffRecord {
                epoch,
                state,
                payload: stored.payload,
                source: stored.source,
            });
            valid_end = end;
        }
    }

    fn terminal_corruption(
        &self,
        start: u64,
        total: u64,
        file: &mut File,
        last: Option<HandoffRecord>,
        valid_end: u64,
    ) -> Result<JournalScan> {
        if has_valid_later_frame(file, start, total, self.session_hash)? {
            bail!("corrupt handoff frame at byte {start}");
        }
        Ok(JournalScan {
            last,
            valid_end,
            truncate_to: Some(valid_end),
        })
    }
}

fn claimable_state(state: HandoffState) -> bool {
    state == HandoffState::Commit
}

fn requires_recovery_state(state: Option<HandoffState>) -> bool {
    matches!(
        state,
        Some(HandoffState::Commit | HandoffState::OwnedAbandon)
    )
}

fn journal_lock(path: &Path) -> Arc<Mutex<()>> {
    static JOURNAL_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = JOURNAL_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

/// Return the state and source from the final complete checksummed frame.
///
/// This deliberately does not bind the journal to a snapshot. It is solely the
/// narrow, lockless predicate needed to retry a snapshot I/O failure; recovery
/// performs that binding before it can claim a handoff.
fn latest_checksummed_source(
    bytes: &[u8],
) -> Result<Option<(HandoffState, hrdr_tools::ProcessLease)>> {
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!("handoff journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let mut offset = 0;
    let mut records = 0;
    let mut last = None;
    while offset < bytes.len() {
        if bytes.len() - offset < HEADER_BYTES {
            bail!("incomplete handoff frame at byte {offset}");
        }
        let header = &bytes[offset..offset + HEADER_BYTES];
        if header[..4] != MAGIC || header[4] != VERSION {
            bail!("invalid handoff frame at byte {offset}");
        }
        let length = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD_BYTES {
            bail!("handoff frame at byte {offset} exceeds {MAX_PAYLOAD_BYTES} bytes");
        }
        let frame_len = HEADER_BYTES + length + CHECKSUM_BYTES;
        if bytes.len() - offset < frame_len {
            bail!("incomplete handoff frame at byte {offset}");
        }
        let frame = &bytes[offset..offset + frame_len];
        let state = HandoffState::try_from(header[17])
            .map_err(|_| anyhow::anyhow!("invalid handoff state at byte {offset}"))?;
        if Sha256::digest(&frame[..HEADER_BYTES + length]).as_slice()
            != &frame[HEADER_BYTES + length..]
        {
            bail!("invalid handoff checksum at byte {offset}");
        }
        let stored: StoredPayload =
            serde_json::from_slice(&frame[HEADER_BYTES..HEADER_BYTES + length])
                .with_context(|| format!("parsing handoff frame at byte {offset}"))?;
        if stored.source.pid == 0 {
            bail!("handoff frame at byte {offset} has a PID zero source");
        }
        HandoffPayload::parse(&stored.payload)
            .with_context(|| format!("validating handoff frame payload at byte {offset}"))?;
        records += 1;
        if records > MAX_RECORDS {
            bail!("handoff journal exceeds {MAX_RECORDS} records");
        }
        last = Some((state, stored.source));
        offset += frame_len;
    }
    Ok(last)
}

/// Determine whether damaged bytes precede a complete valid frame.
///
/// A later frame is recognized only by the full bounded framing contract: magic,
/// version, bounded length, valid state, checksum, and saved-session binding. That
/// makes truncation safe only when no complete later frame can be established.
fn has_valid_later_frame(
    file: &mut File,
    start: u64,
    total: u64,
    session_hash: [u8; 32],
) -> Result<bool> {
    file.seek(SeekFrom::Start(start.saturating_add(1)))?;
    let mut suffix = Vec::new();
    file.read_to_end(&mut suffix)?;
    for offset in 0..suffix.len().saturating_sub(HEADER_BYTES - 1) {
        if suffix[offset..].starts_with(&MAGIC) && valid_frame_at(&suffix[offset..], session_hash) {
            return Ok(true);
        }
    }
    debug_assert_eq!(start + 1 + suffix.len() as u64, total);
    Ok(false)
}

fn valid_frame_at(bytes: &[u8], session_hash: [u8; 32]) -> bool {
    if bytes.len() < HEADER_BYTES || bytes[..4] != MAGIC || bytes[4] != VERSION {
        return false;
    }
    let length = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
    if length > MAX_PAYLOAD_BYTES || bytes.len() < HEADER_BYTES + length + CHECKSUM_BYTES {
        return false;
    }
    let frame_len = HEADER_BYTES + length + CHECKSUM_BYTES;
    if HandoffState::try_from(bytes[17]).is_err()
        || Sha256::digest(&bytes[..HEADER_BYTES + length]).as_slice()
            != &bytes[HEADER_BYTES + length..frame_len]
    {
        return false;
    }
    match serde_json::from_slice::<StoredPayload>(&bytes[HEADER_BYTES..HEADER_BYTES + length]) {
        Ok(stored) => stored.session_hash == session_hash && stored.source.pid != 0,
        Err(_) => false,
    }
}

fn read_partial(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut read = 0;
    while read < buffer.len() {
        let count = file.read(&mut buffer[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    Ok(read)
}

fn saved_session_hash(path: &Path, id: &str) -> Result<[u8; 32]> {
    let session = Session::load_path(path, false)
        .map_err(snapshot_io_error)
        .with_context(|| format!("loading saved session {}", path.display()))?;
    if session.state.id.as_deref() != Some(id) {
        bail!("saved session marker does not match {id}");
    }
    let bytes = std::fs::read(path)
        .map_err(|error| SnapshotIoError(error.into()))
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Sha256::digest(bytes).into())
}

pub(crate) fn snapshot_io_error(error: anyhow::Error) -> anyhow::Error {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
    {
        SnapshotIoError(error).into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(dir: &tempfile::TempDir) -> HandoffJournal<'static> {
        let path = dir.path().join("session.handoff");
        HandoffJournal {
            append_lock: journal_lock(&path),
            path,
            session_hash: [7; 32],
            _lock: Some(Box::leak(Box::new(SessionLock::test_unbacked()))),
        }
    }

    fn lease() -> hrdr_tools::ProcessLease {
        hrdr_tools::ProcessLease {
            pid: 1234,
            created: 5,
        }
    }

    fn append_frame(journal: &HandoffJournal, epoch: u64, payload: &[u8]) {
        let frame = journal
            .frame(epoch, HandoffState::Prepare, lease(), payload)
            .unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal.path)
            .unwrap();
        file.write_all(&frame).unwrap();
    }

    #[test]
    fn update_evidence_requires_the_adjacent_deferred_result() {
        let operation = UpdateOperationId::new("op-1").unwrap();
        let assistant: crate::Message = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "tool_calls": [{"id":"call-1","type":"function","function":{"name":"update","arguments":"{ }"}}]
        }))
        .unwrap();
        let result = crate::Message::tool_result(
            "call-1",
            "{\"status\":\"deferred\",\"operation_id\":\"op-1\"}\n\n(took 0ms)",
        );
        assert!(validate_deferred_update(&[assistant.clone(), result.clone()], &operation).is_ok());
        assert!(!is_empty_update_arguments("{\"unexpected\":true}"));
        assert!(!is_empty_update_arguments("not json"));
        assert_eq!(
            tool_result_body("{\"status\":\"deferred\"}\n\n(took 0ms)"),
            "{\"status\":\"deferred\"}"
        );
        assert_eq!(
            tool_result_body("{\"status\":\"deferred\"}\n\n(took"),
            "{\"status\":\"deferred\"}\n\n(took"
        );
        for messages in [
            vec![assistant.clone()],
            vec![result.clone(), assistant.clone()],
            vec![
                assistant.clone(),
                crate::Message::tool_result("call-1", r#"{"operation_id":"other"}"#),
            ],
        ] {
            assert!(validate_deferred_update(&messages, &operation).is_err());
        }
    }

    #[test]
    fn update_operation_ids_and_payloads_reject_empty_or_mismatched_shapes() {
        assert!(UpdateOperationId::new(" \t").is_err());
        assert!(HandoffPayload::parse_typed(br#"{"version":1,"intent":"idle","operation_id":"op","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#).is_err());
        assert!(HandoffPayload::parse_typed(br#"{"version":1,"intent":"update_continuation","operation_id":" ","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#).is_err());
        assert!(HandoffPayload::parse_typed(br#"{"version":1,"intent":"update_continuation","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#).is_err());
    }
    #[test]
    fn snapshot_validation_refuses_unpaired_update_evidence() {
        let payload = br#"{"version":1,"intent":"update_continuation","operation_id":"op-1","runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#;
        let assistant: crate::Message = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "tool_calls": [{"id":"","type":"function","function":{"name":"update","arguments":"{\"operation_id\":\"op-1\"}"}}]
        }))
        .unwrap();
        assert!(HandoffPayload::validate_snapshot(payload, &[assistant]).is_err());
    }

    #[test]
    fn prepare_does_not_require_recovery_but_committed_handoffs_do() {
        assert!(!requires_recovery_state(None));
        assert!(!requires_recovery_state(Some(HandoffState::Prepare)));
        assert!(requires_recovery_state(Some(HandoffState::Commit)));
        assert!(requires_recovery_state(Some(HandoffState::OwnedAbandon)));
        assert!(!requires_recovery_state(Some(HandoffState::Claim)));
        assert!(!requires_recovery_state(Some(HandoffState::Abort)));
    }

    #[test]
    fn committed_then_aborted_handoff_allows_resume_and_cannot_be_claimed() {
        let states = [HandoffState::Commit, HandoffState::Abort];
        assert!(requires_recovery_state(Some(states[0])));
        assert!(!requires_recovery_state(states.last().copied()));
        assert!(!claimable_state(*states.last().unwrap()));
    }

    #[test]
    fn only_commits_are_claimable() {
        for (state, claimable) in [
            (HandoffState::Prepare, false),
            (HandoffState::Commit, true),
            (HandoffState::OwnedAbandon, false),
            (HandoffState::Claim, false),
            (HandoffState::Abort, false),
        ] {
            assert_eq!(claimable_state(state), claimable, "{state:?}");
        }
    }

    #[test]
    fn lockless_retry_inspection_requires_an_exact_committed_checksummed_source() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let source = lease();
        let payload = br#"{"version":1,"intent":"idle","operation_id":null,"runtime_queue":false,"runtime_tasks":false,"runtime_tools":false,"runtime_turn":false,"runtime_compaction":false,"runtime_watch":false}"#;
        let commit = journal
            .frame(1, HandoffState::Commit, source, payload)
            .unwrap();
        assert_eq!(
            latest_checksummed_source(&commit).unwrap(),
            Some((HandoffState::Commit, source))
        );

        let replaced = journal
            .frame(
                2,
                HandoffState::Claim,
                hrdr_tools::ProcessLease {
                    pid: source.pid + 1,
                    created: source.created,
                },
                payload,
            )
            .unwrap();
        let mut nonclaimable = commit.clone();
        nonclaimable.extend_from_slice(&replaced);
        assert_eq!(
            latest_checksummed_source(&nonclaimable).unwrap(),
            Some((
                HandoffState::Claim,
                hrdr_tools::ProcessLease {
                    pid: source.pid + 1,
                    created: source.created,
                }
            ))
        );

        let malformed_payload = journal
            .frame(3, HandoffState::Commit, source, b"malformed payload")
            .unwrap();
        assert!(latest_checksummed_source(&malformed_payload).is_err());

        let mut corrupt = commit;
        corrupt[HEADER_BYTES] ^= 1;
        assert!(latest_checksummed_source(&corrupt).is_err());
    }
    #[test]
    fn invalid_payload_is_not_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let frame = journal
            .frame(
                1,
                HandoffState::Commit,
                hrdr_tools::ProcessLease {
                    pid: u32::MAX - 1,
                    created: 0,
                },
                b"invalid",
            )
            .unwrap();
        std::fs::write(&journal.path, frame).unwrap();

        assert!(
            journal
                .claim::<(), _>(|_| anyhow::bail!("invalid handoff payload"))
                .is_err()
        );
        assert_eq!(
            journal.scan().unwrap().last.unwrap().state,
            HandoffState::Commit,
            "validation runs before Claim is appended"
        );
    }

    #[test]
    fn torn_final_frame_keeps_the_valid_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        append_frame(&journal, 1, b"first");
        let valid = std::fs::read(&journal.path).unwrap();
        let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
        file.write_all(&valid[..HEADER_BYTES - 1]).unwrap();
        drop(file);
        let scan = journal.scan().unwrap();
        assert_eq!(scan.last.unwrap().payload, b"first");
        assert_eq!(scan.truncate_to, Some(valid.len() as u64));
    }

    #[test]
    fn checksum_corruption_before_a_later_frame_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        append_frame(&journal, 1, b"first");
        let first_len = std::fs::metadata(&journal.path).unwrap().len() as usize;
        append_frame(&journal, 2, b"second");
        let mut bytes = std::fs::read(&journal.path).unwrap();
        bytes[first_len - 1] ^= 1;
        std::fs::write(&journal.path, bytes).unwrap();
        assert!(
            journal.scan().is_err(),
            "a damaged prior frame cannot be skipped"
        );
    }

    #[test]
    fn oversized_journal_is_rejected_before_reading_frames() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        let file = File::create(&journal.path).unwrap();
        file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();
        assert!(journal.scan().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn malformed_terminal_bytes_are_replaced_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        journal.append(1, HandoffState::Prepare, b"first").unwrap();
        let valid_len = std::fs::metadata(&journal.path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
        file.write_all(b"not a frame, but a torn terminal suffix")
            .unwrap();
        drop(file);

        journal.append(2, HandoffState::Prepare, b"second").unwrap();

        let scan = journal.scan().unwrap();
        assert_eq!(scan.last.unwrap().payload, b"second");
        assert!(scan.valid_end > valid_len);
        assert_eq!(
            scan.valid_end,
            std::fs::metadata(&journal.path).unwrap().len()
        );
    }

    #[cfg(windows)]
    #[test]
    fn concurrent_appends_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        const WRITERS: u64 = 16;
        std::thread::scope(|scope| {
            for epoch in 0..WRITERS {
                let journal = &journal;
                scope.spawn(move || {
                    journal
                        .append(
                            epoch,
                            HandoffState::Prepare,
                            format!("frame-{epoch}").as_bytes(),
                        )
                        .unwrap();
                });
            }
        });

        assert!(journal.scan().is_ok());
        let bytes = std::fs::read(&journal.path).unwrap();
        let mut offset = 0;
        let mut frames = 0;
        while offset < bytes.len() {
            let length =
                u32::from_le_bytes(bytes[offset + 5..offset + 9].try_into().unwrap()) as usize;
            let frame_len = HEADER_BYTES + length + CHECKSUM_BYTES;
            assert!(valid_frame_at(
                &bytes[offset..offset + frame_len],
                journal.session_hash
            ));
            offset += frame_len;
            frames += 1;
        }
        assert_eq!(frames, WRITERS as usize);
    }

    #[cfg(windows)]
    #[test]
    fn torn_frame_is_truncated_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        journal.append(1, HandoffState::Prepare, b"first").unwrap();
        let valid = std::fs::read(&journal.path).unwrap();
        let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
        file.write_all(&valid[..HEADER_BYTES - 1]).unwrap();
        drop(file);
        journal.append(2, HandoffState::Prepare, b"second").unwrap();
        let scan = journal.scan().unwrap();
        assert_eq!(scan.last.unwrap().payload, b"second");
        assert_eq!(
            scan.valid_end,
            std::fs::metadata(&journal.path).unwrap().len()
        );
    }

    #[cfg(windows)]
    #[test]
    fn live_source_owner_cannot_be_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(&dir);
        journal.append(1, HandoffState::Commit, b"").unwrap();
        assert!(journal.claimable().is_err());
    }
}
