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

/// The checkpoint phase recorded in a handoff journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HandoffState {
    Prepare = 1,
    Commit = 2,
    OwnedAbandon = 3,
    Claim = 4,
}

impl TryFrom<u8> for HandoffState {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Prepare),
            2 => Ok(Self::Commit),
            3 => Ok(Self::OwnedAbandon),
            4 => Ok(Self::Claim),
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
    _lock: &'a SessionLock,
    append_lock: Arc<Mutex<()>>,
}

impl<'a> HandoffJournal<'a> {
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
            _lock: lock,
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
            .with_context(|| format!("reading {}", session_path.display()))?;
        let path = session_path.with_file_name(format!("{id}.handoff"));
        Ok(Self {
            append_lock: journal_lock(&path),
            path,
            session_hash: Sha256::digest(bytes).into(),
            _lock: lock,
        })
    }

    /// Append a checkpoint after checking it still belongs to the saved snapshot.
    ///
    /// The source identity is captured from this process, rather than accepted from
    /// the caller. This prevents one process from writing a claim on another's behalf.
    pub fn append(&self, epoch: u64, state: HandoffState, payload: &[u8]) -> Result<()> {
        let _append_guard = self
            .append_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("handoff journal append lock is poisoned"))?;
        let source = hrdr_tools::current_process_lease()
            .context("process-instance handoff journals are unsupported on this platform")?;
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

    /// Claim the newest committed checkpoint after its source lease is certainly gone.
    ///
    /// The scan, liveness check, and durable `Claim` append share one process-local
    /// append lock. A failed append leaves the committed frame in place for retry.
    pub fn claim<F>(&self, validate: F) -> Result<Option<HandoffRecord>>
    where
        F: FnOnce(&[u8]) -> Result<()>,
    {
        let _append_guard = self
            .append_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("handoff journal append lock is poisoned"))?;
        let Some(record) = self.claimable()? else {
            return Ok(None);
        };
        validate(&record.payload)?;
        let source = hrdr_tools::current_process_lease()
            .context("process-instance handoff journals are unsupported on this platform")?;
        let frame = self.frame(record.epoch, HandoffState::Claim, source, &record.payload)?;
        self.append_frame_locked(&frame)?;
        Ok(Some(record))
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
        .with_context(|| format!("loading saved session {}", path.display()))?;
    if session.state.id.as_deref() != Some(id) {
        bail!("saved session marker does not match {id}");
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Sha256::digest(bytes).into())
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
            _lock: Box::leak(Box::new(SessionLock::test_unbacked())),
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
    fn prepare_does_not_require_recovery_but_committed_handoffs_do() {
        assert!(!requires_recovery_state(None));
        assert!(!requires_recovery_state(Some(HandoffState::Prepare)));
        assert!(requires_recovery_state(Some(HandoffState::Commit)));
        assert!(requires_recovery_state(Some(HandoffState::OwnedAbandon)));
        assert!(!requires_recovery_state(Some(HandoffState::Claim)));
    }

    #[test]
    fn only_commits_are_claimable() {
        for (state, claimable) in [
            (HandoffState::Prepare, false),
            (HandoffState::Commit, true),
            (HandoffState::OwnedAbandon, false),
            (HandoffState::Claim, false),
        ] {
            assert_eq!(claimable_state(state), claimable, "{state:?}");
        }
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
