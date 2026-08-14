//! The content-addressed store that keeps a session's attachments on disk.
//!
//! An attachment is bytes — an image or a PDF, routinely megabytes of them. A
//! session file is rewritten in full on **every committed tool round**
//! (`Session::save`), so inlining base64 into it would re-serialize and re-fsync
//! those megabytes for the rest of the conversation. The bytes therefore live
//! beside the session instead, in a `blobs/` directory named by the SHA-256 of
//! their own contents, and the session file carries only a reference
//! ([`AttachmentRef`]: name, media type, length, digest).
//!
//! Content addressing is what makes the store cheap to share: the same image
//! attached to two messages — or to two sessions in the same working directory —
//! is one file, written once. It is also what makes a resumed attachment
//! *checkable*: the blob's name is a claim about its bytes that
//! [`resolve_attachments`] verifies before the bytes are ever handed to a model.
//!
//! Durability comes from [`crate::write_atomic`] (temp sibling → fsync → rename
//! → directory fsync), so a crash mid-write leaves either no blob or the whole
//! blob, never a partial one. Belt and braces: a blob that is somehow damaged
//! anyway fails its digest check on load and is reported, not sent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hrdr_llm::media::{Attachment, MediaType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Message;

/// The directory holding one session directory's blobs, as a name.
///
/// It sits *inside* a session directory rather than beside the sessions, so it
/// is shared by every session filed under that working directory and is skipped
/// by everything that enumerates sessions — those all match `*.json` /
/// `*.json.zst` file names, and this is a directory (the same reason
/// `subagents/` is invisible to them).
const BLOB_SUBDIR: &str = "blobs";

/// How recently a blob must have been written or re-referenced to be spared by
/// [`sweep_blobs`], regardless of what the mark phase found.
///
/// Exclusion is [`lock_blob_store`]'s job: mark, sweep and every save that
/// touches blobs run under it, so a session referencing a blob cannot land in
/// the window between the sweep collecting references and the sweep deleting.
/// This window is the fallback for the one case that lock cannot cover — a save
/// whose acquire failed proceeds unguarded rather than losing the user's
/// attachment, and it is this grace period that then keeps its blobs alive. One
/// hour is comfortably longer than any such save (the retention worker itself
/// runs hourly), and a blob that is genuinely garbage is simply collected on the
/// next pass instead of this one.
const BLOB_GRACE_SECS: u64 = 60 * 60;

/// Where one message's attachments are stored — an entry per message that has
/// any, so a conversation whose messages are mostly plain text carries nothing
/// for them.
///
/// A list of records rather than a map keyed by index: a JSON object key is a
/// string, and `Session` reaches this type through `#[serde(flatten)]`, whose
/// buffering cannot turn that string back into an integer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttachments {
    /// Index into the session's `messages`.
    pub message: usize,
    pub files: Vec<AttachmentRef>,
}

/// One attachment as a session file stores it: everything needed to find its
/// blob and to prove the blob is the right bytes.
///
/// `len` is not redundant with `sha256`. It is the cheap check — one `stat`
/// rejects a truncated blob before anything reads or hashes it, and it bounds
/// the read of a file the process is about to pull into memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    /// The original file name, as the attachment carries it.
    pub filename: String,
    /// The validated media type, as [`MediaType::mime`] spells it.
    pub media_type: String,
    /// Length of the raw (un-encoded) bytes.
    pub len: u64,
    /// Lowercase-hex SHA-256 of the bytes — and the blob's file name.
    pub sha256: String,
}

impl AttachmentRef {
    /// Describe `a` for storage, hashing its bytes.
    pub fn of(a: &Attachment) -> Self {
        Self {
            filename: a.filename().to_string(),
            media_type: a.media_type().mime().to_string(),
            len: a.bytes().len() as u64,
            sha256: digest_hex(a.bytes()),
        }
    }
}

/// Why a stored attachment did not come back.
///
/// Every variant means the same thing to the conversation — the bytes are not
/// there — but they mean different things to the person trying to work out why,
/// which is the whole point of telling them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentLossReason {
    /// No blob with that digest exists. The usual cause is someone clearing the
    /// store, or copying a session file without its `blobs/` sibling.
    Missing,
    /// A blob is there, but it is not the bytes the reference names — wrong
    /// length, or a digest that does not match.
    Corrupt,
    /// The blob exists and could not be read (permissions, an I/O error).
    Unreadable,
    /// The bytes came back intact but no longer form something hrdr can attach —
    /// a media type it does not know, or bytes whose magic number disagrees with
    /// the recorded type.
    Invalid,
}

impl std::fmt::Display for AttachmentLossReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "its stored copy is missing",
            Self::Corrupt => "its stored copy does not match the recorded checksum",
            Self::Unreadable => "its stored copy could not be read",
            Self::Invalid => "its stored copy is not a usable image or PDF",
        })
    }
}

/// One attachment that a resume could not restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentLoss {
    pub filename: String,
    pub reason: AttachmentLossReason,
}

impl std::fmt::Display for AttachmentLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.filename, self.reason)
    }
}

/// Lowercase-hex SHA-256, the form a blob is named by.
pub(crate) fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `name` is a blob file name — exactly 64 lowercase hex digits.
///
/// The deletion side of [`sweep_blobs`] is gated on this and nothing else, so a
/// `write_atomic` temp file in flight (`…hrdr-tmp…`) or anything a user dropped
/// in the directory can never be chosen as a victim.
fn is_blob_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The blob directory belonging to the session file at `session_path`.
pub fn blob_dir(session_path: &Path) -> PathBuf {
    blob_dir_in(session_path.parent().unwrap_or_else(|| Path::new(".")))
}

/// The blob directory inside a directory of session files.
pub fn blob_dir_in(session_dir: &Path) -> PathBuf {
    session_dir.join(BLOB_SUBDIR)
}

/// Take the exclusive cross-process lock on the blob store under `session_dir`.
///
/// Blobs are shared by every session filed under one working directory, and two
/// hrdr processes can be live in that directory at once. Everything that writes
/// blobs holds this, and so does the mark-and-sweep collector across *both* of
/// its phases — which is what stops the collector from observing the gap between
/// a peer writing a blob and writing the session file that names it.
///
/// The lock file is the sibling `<session_dir>/blobs.lock`, not something inside
/// `blobs/`: a lock file in a directory the sweep enumerates would be needless
/// confusion, and `blobs/` may not exist yet whereas both callers have already
/// created the session directory.
///
/// **The two sides deliberately differ on failure, and neither is a bug:**
///
/// * The collector (`session::collect_blob_garbage`) **skips entirely**.
///   Its mark set is only trustworthy while nothing else writes here, and
///   nothing is lost by waiting — the retention worker runs hourly, so the
///   garbage goes on the next pass.
/// * A save **proceeds without the guard**. Failing a save because a lock was
///   contended would cost the user an attachment, or the whole session file —
///   far worse than the race the lock closes, which [`BLOB_GRACE_SECS`] still
///   covers as a fallback.
///
/// [`StoreKind::BlobStore`](crate::store_lock::StoreKind::BlobStore) is what
/// makes the lock survive the save that holds it: a save writes blobs and a
/// session file, not one small file, so it is held far longer than a credential
/// store's read-modify-write and must not be reapable on that store's schedule.
pub(crate) fn lock_blob_store(session_dir: &Path) -> Result<crate::store_lock::StoreLock> {
    crate::store_lock::StoreLock::acquire(
        &blob_dir_in(session_dir),
        crate::store_lock::StoreKind::BlobStore,
    )
}

/// Describe every attachment in `messages`, one entry per message that has any.
///
/// Sparse on purpose: a conversation with no attachments produces an empty list,
/// which the session body omits entirely — so its file is byte-for-byte what it
/// was before attachments were persisted at all.
pub fn attachment_refs(messages: &[Message]) -> Vec<MessageAttachments> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.attachments.is_empty())
        .map(|(message, m)| MessageAttachments {
            message,
            files: m.attachments.iter().map(AttachmentRef::of).collect(),
        })
        .collect()
}

/// Write every attachment in `messages` into `dir`, using the digests already
/// computed in `refs` so nothing is hashed twice per save.
///
/// A blob whose file is already present is not rewritten — its name *is* its
/// contents, so there is nothing a rewrite could change. Its mtime is refreshed
/// instead, which costs one `utimes` and is what keeps a blob that is still in
/// use out of reach of [`sweep_blobs`]'s grace window.
pub fn write_blobs(dir: &Path, messages: &[Message], refs: &[MessageAttachments]) -> Result<()> {
    if refs.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for entry in refs {
        let Some(message) = messages.get(entry.message) else {
            continue;
        };
        for (r, a) in entry.files.iter().zip(&message.attachments) {
            let path = dir.join(&r.sha256);
            if path.exists() {
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
                continue;
            }
            crate::write_atomic(&path, a.bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Read one blob back as an attachment, checking it against what `r` claims.
fn read_blob(dir: &Path, r: &AttachmentRef) -> Result<Attachment, AttachmentLossReason> {
    let Some(media_type) = MediaType::from_mime(&r.media_type) else {
        return Err(AttachmentLossReason::Invalid);
    };
    let path = dir.join(&r.sha256);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AttachmentLossReason::Missing);
        }
        Err(_) => return Err(AttachmentLossReason::Unreadable),
    };
    // The cheap check first: a blob whose size disagrees is already wrong, and
    // this is also what bounds the read below to the length the session recorded.
    if meta.len() != r.len {
        return Err(AttachmentLossReason::Corrupt);
    }
    let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => AttachmentLossReason::Missing,
        _ => AttachmentLossReason::Unreadable,
    })?;
    if bytes.len() as u64 != r.len || digest_hex(&bytes) != r.sha256 {
        return Err(AttachmentLossReason::Corrupt);
    }
    // `Attachment::new` re-sniffs the magic number, so a blob that hashes
    // correctly but whose recorded media type is wrong is still refused.
    Attachment::new(bytes, media_type, r.filename.clone())
        .map_err(|_| AttachmentLossReason::Invalid)
}

/// Put the stored bytes back onto `messages`, and report everything that could
/// not be restored.
///
/// A loss is never silent, and never partial. The attachment is dropped, and the
/// message's own text gains a line saying so — because the text already carries
/// the `--- Attached files ---` block naming the file, and a model shown that
/// label with no image behind it is being lied to. Appending the note is what
/// keeps the message honest to the model; the returned [`AttachmentLoss`] list is
/// what makes it visible to the user (the frontend prints one line per loss on
/// resume).
///
/// The note is written once: the attachment is gone from the message afterwards,
/// so the next save records no reference for it and a later resume has nothing
/// left to lose.
pub fn resolve_attachments(
    dir: &Path,
    refs: &[MessageAttachments],
    messages: &mut [Message],
) -> Vec<AttachmentLoss> {
    let mut losses = Vec::new();
    for entry in refs {
        let Some(message) = messages.get_mut(entry.message) else {
            continue;
        };
        let mut lost_here = Vec::new();
        for r in &entry.files {
            match read_blob(dir, r) {
                Ok(a) => message.attachments.push(a),
                Err(reason) => {
                    let loss = AttachmentLoss {
                        filename: r.filename.clone(),
                        reason,
                    };
                    lost_here.push(loss.to_string());
                    losses.push(loss);
                }
            }
        }
        if !lost_here.is_empty() {
            let mut note = String::from("\n\n--- Attachments unavailable ---\n");
            for line in &lost_here {
                note.push_str(line);
                note.push('\n');
            }
            message
                .content
                .get_or_insert_with(String::new)
                .push_str(&note);
        }
    }
    losses
}

/// Delete blobs in `dir` that `keep` does not name.
///
/// Called after retention purges a session, with `keep` being every digest still
/// referenced by every session that survived in that directory — so a blob two
/// sessions share outlives the one that was purged. A blob written or
/// re-referenced within [`BLOB_GRACE_SECS`] is spared whatever the mark phase
/// said, which is what keeps the bytes of a save that could not take the store
/// lock (see [`lock_blob_store`]) from being collected under it.
///
/// Best-effort: an unreadable directory or a failed unlink skips that entry.
/// **The caller must hold [`lock_blob_store`], and must be sure `keep` is
/// complete** — a caller that could not read some session in the directory must
/// not call this at all, since an incomplete mark set turns a live blob into a
/// victim.
pub fn sweep_blobs(dir: &Path, keep: &HashSet<String>) {
    let now = hrdr_tools::unix_now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_blob_name(name) || keep.contains(name) {
            continue;
        }
        let recent = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| now.saturating_sub(d.as_secs()) < BLOB_GRACE_SECS);
        if recent {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(pad: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend(std::iter::repeat_n(0u8, pad));
        v
    }

    fn attachment(pad: usize, name: &str) -> Attachment {
        Attachment::new(png(pad), MediaType::Png, name).expect("a valid png")
    }

    fn user_with(attachments: Vec<Attachment>) -> Message {
        let mut m = Message::user("look");
        m.attachments = attachments;
        m
    }

    /// A blob's name is the SHA-256 of its own bytes, so identical bytes land on
    /// one file however many messages reference them.
    #[test]
    fn identical_bytes_share_one_blob() {
        let dir = tempfile::tempdir().unwrap();
        let a = attachment(16, "one.png");
        let b = attachment(16, "two.png");
        let messages = vec![user_with(vec![a]), user_with(vec![b])];
        let refs = attachment_refs(&messages);
        write_blobs(dir.path(), &messages, &refs).unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(files.len(), 1, "one blob for identical bytes: {files:?}");
    }

    /// A blob whose bytes were replaced under the session fails the digest check
    /// rather than coming back as an attachment.
    #[test]
    fn a_tampered_blob_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![user_with(vec![attachment(16, "shot.png")])];
        let refs = attachment_refs(&messages);
        write_blobs(dir.path(), &messages, &refs).unwrap();
        let digest = &refs[0].files[0].sha256;
        // Same length, different bytes — so only the checksum can catch it.
        let mut tampered = png(16);
        *tampered.last_mut().unwrap() = 0xFF;
        std::fs::write(dir.path().join(digest), &tampered).unwrap();
        let mut restored = vec![Message::user("look")];
        let losses = resolve_attachments(dir.path(), &refs, &mut restored);
        assert_eq!(
            losses,
            vec![AttachmentLoss {
                filename: "shot.png".into(),
                reason: AttachmentLossReason::Corrupt,
            }]
        );
        assert!(restored[0].attachments.is_empty());
    }

    /// A blob truncated in place is caught by the length check, before anything
    /// reads it.
    #[test]
    fn a_truncated_blob_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![user_with(vec![attachment(64, "shot.png")])];
        let refs = attachment_refs(&messages);
        write_blobs(dir.path(), &messages, &refs).unwrap();
        std::fs::write(dir.path().join(&refs[0].files[0].sha256), png(4)).unwrap();
        let mut restored = vec![Message::user("look")];
        let losses = resolve_attachments(dir.path(), &refs, &mut restored);
        assert_eq!(losses[0].reason, AttachmentLossReason::Corrupt);
    }

    /// The sweep deletes only what the mark set omits, only for blob-shaped
    /// names, and only past the grace window.
    #[test]
    fn the_sweep_spares_kept_recent_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![user_with(vec![attachment(16, "kept.png")])];
        let refs = attachment_refs(&messages);
        write_blobs(dir.path(), &messages, &refs).unwrap();
        let kept = refs[0].files[0].sha256.clone();

        let dead = "b".repeat(64);
        let recent = "c".repeat(64);
        let foreign = dir.path().join("notes.txt");
        std::fs::write(dir.path().join(&dead), b"x").unwrap();
        std::fs::write(dir.path().join(&recent), b"x").unwrap();
        std::fs::write(&foreign, b"x").unwrap();
        // Age the referenced blob and one unreferenced one past the grace window;
        // `recent` keeps its fresh mtime.
        let old = filetime::FileTime::from_unix_time(
            (hrdr_tools::unix_now() - BLOB_GRACE_SECS - 60) as i64,
            0,
        );
        filetime::set_file_mtime(dir.path().join(&kept), old).unwrap();
        filetime::set_file_mtime(dir.path().join(&dead), old).unwrap();

        sweep_blobs(dir.path(), &HashSet::from([kept.clone()]));

        assert!(
            dir.path().join(&kept).exists(),
            "a referenced blob survives"
        );
        assert!(
            !dir.path().join(&dead).exists(),
            "an old orphan is collected"
        );
        assert!(
            dir.path().join(&recent).exists(),
            "a freshly written orphan is inside the grace window"
        );
        assert!(foreign.exists(), "a non-blob name is never a victim");
    }
}
