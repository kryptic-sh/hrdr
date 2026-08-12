//! On-disk session persistence.
//!
//! A session is the conversation (`ChatMessage` history) plus light metadata,
//! stored as JSON under `$XDG_DATA_HOME/hrdr/sessions` (default
//! `~/.local/share/hrdr/sessions`). Sessions are partitioned by working
//! directory: each lives at `sessions/<cwd-slug>/<name-slug>.json`, so the
//! files are easy to manage by hand and startup auto-resume scopes to a project.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::Entry;
use crate::{DEFAULT_MODEL_REF, Message, ModelRef, ModelSpec, cwd_slug};
use anyhow::{Context, Result};
use hrdr_tools::TodoItem;
use serde::{Deserialize, Serialize};

/// How long (in seconds) a reservation lock must exist before it may be
/// considered stale. A process that crashes after reserving an id but before
/// saving will leave a lock behind; this threshold prevents a permanent
/// deadlock while avoiding false reaping of a lock that a slow filesystem or
/// a concurrent process legitimately holds.
const STALE_LOCK_AGE_SECS: u64 = 60;

/// v2: the identity is ONE key — `model: "provider://model"` — where v1 wrote
/// `model` and `provider` side by side. A v1 file still loads (see
/// [`SessionState`]'s `Deserialize`): sessions are DATA, and refusing to open a
/// conversation because it predates a refactor would be hostile. Config is the
/// opposite case, and is refused.
const SESSION_VERSION: u32 = 2;

/// The token counters the status bar shows, persisted so a resumed conversation
/// picks up where it left off instead of restarting from zero.
///
/// These belong to the **agent**, not to the session — every agent makes its own
/// calls, fills its own window, and costs its own money — so the type lives in
/// `hrdr-agent` ([`crate::AgentUsage`]) where a sub-agent's counters are
/// kept with no frontend attached. This alias is the name the session file and
/// the status bar know it by.
pub use crate::AgentUsage as SessionUsage;

/// Everything about **one agent's** conversation that outlives the process: what
/// the user saw, what the model saw, which endpoint/model it ran on, and the
/// counters its status bar reads.
///
/// This is the single in-memory state a frontend keeps *per agent* and the exact
/// payload a state file stores — [`Session`] wraps it with file metadata and
/// nothing else. Loading is an assignment; saving is a serialize. There is no
/// lossy rebuild step, and no parallel vectors to keep in sync.
///
/// Every Pane owns one, main and sub-agent alike: a delegated
/// sub-agent has a name, a model, a provider, a history and a token bill exactly
/// as the main agent does, and the only thing that made the main one special was
/// that it was the one the frontend happened to store.
///
/// `messages` and `todos` mirror state whose runtime owners are the `Agent` and
/// the `todo` tool respectively; [`SessionState::sync_from`] refreshes them
/// before a save.
#[derive(Debug, Clone, Serialize)]
pub struct SessionState {
    /// Human-friendly session name (defaults to the first user message).
    #[serde(default)]
    pub name: String,
    /// Whether the user explicitly named this session (`/rename` or `/new <name>`)
    /// rather than letting it auto-derive from the first message. Retention's
    /// purge phase spares user-named sessions and only ever deletes auto-named
    /// ones. A pre-feature file has no key → `false` → treated as auto-named.
    #[serde(default)]
    pub named_by_user: bool,
    /// The run this state records was scoped to the read-only tool set
    /// ([`AgentConfig::read_only`](crate::AgentConfig::read_only)) — an
    /// `explore`/`review`/`plan`-style sub-agent. Persisted because capability is
    /// part of *what this run was*: `task_revive` restores it so a revived run is
    /// pruned to the same tool set instead of coming back write-capable.
    ///
    /// A snapshot written before the field existed has no key → `false` →
    /// write-capable. That is the truth for the main session and for every write
    /// sub-agent (the runs revive exists for); the price is that a read-only run
    /// persisted earlier still revives write-capable, exactly as it did before.
    #[serde(default)]
    pub read_only: bool,
    /// The session's file id (stem). Derived from the filename, not stored in it.
    #[serde(skip)]
    pub id: Option<String>,
    /// What this agent runs on: the provider AND the model, as ONE value, written
    /// as the single string `provider://model`.
    ///
    /// A v1 file's `model` + `provider` pair is folded into it on read (see the
    /// hand-written `Deserialize`), so an old conversation opens without a word.
    pub model: ModelRef,
    /// The v1 file named a model but no provider — so its provider half above is a
    /// placeholder, and the identity means "this model, on the provider currently in
    /// effect". Derived at load; never persisted (a written file always names both).
    #[serde(skip)]
    pub provider_unset: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub cwd: String,
    /// The chat history the model sees (the `Agent` is its runtime owner).
    ///
    /// Serialized through [`persisted_messages`], not `Message`'s own impl —
    /// that one is the OpenAI wire format and drops the model's thinking.
    #[serde(default, with = "persisted_messages")]
    pub messages: Vec<Message>,
    /// TODO items from the `todo` tool (the shared list is its runtime owner).
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// The display transcript: every entry the user saw, each with its own
    /// timestamp. This — not `messages` — is what a resume renders.
    ///
    /// **Not serialized into the `.json`.** It is written incrementally to a
    /// sibling append-only `<id>.jsonl` (the fold of the agent's `AgentEvent`
    /// stream — the same durable record a delegated sub-agent keeps) and rebuilt
    /// from it on load ([`Session::load_path`]). Embedding it here meant the whole
    /// multi-MB transcript was re-serialized on every mid-turn save — O(n²) in the
    /// length of the conversation. It still *deserializes* (via the hand-written
    /// `Deserialize`), so an older file that embeds one loads it as a fallback when
    /// no sibling jsonl exists.
    #[serde(default, skip_serializing)]
    pub transcript: Vec<Entry>,
    /// Token counters for the status bar (see [`SessionUsage`]).
    #[serde(default)]
    pub usage: SessionUsage,
    /// Attachment references read out of the file, one entry per message that
    /// had any — the bytes themselves live in the blob store beside it
    /// ([`crate::attachment_store`]).
    ///
    /// Transient, and never serialized from here: [`Session::load_path`] takes
    /// this the moment it has a path to resolve against, turns each reference
    /// back into a real `Attachment` on `messages`, and leaves it empty. The
    /// *write* side never reads it either — a save describes the attachments
    /// actually on the messages (see [`SessionBody`]), so a message whose
    /// attachment was dropped stops being described.
    #[serde(skip)]
    pub attachment_refs: Vec<crate::MessageAttachments>,
    /// Attachments this load could NOT restore. Empty in the normal case.
    ///
    /// Also transient — it is a fact about one read, not about the conversation.
    /// The frontend prints one line per entry when it adopts the state, which is
    /// the only reason a resume can be trusted: the message text says a file was
    /// attached, so a resume that quietly dropped it would leave the model
    /// looking at a label with nothing behind it.
    #[serde(skip)]
    pub attachment_losses: Vec<crate::AttachmentLoss>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            name: String::new(),
            named_by_user: false,
            read_only: false,
            id: None,
            model: DEFAULT_MODEL_REF.parse().expect("a valid default identity"),
            provider_unset: false,
            base_url: String::new(),
            cwd: String::new(),
            messages: Vec::new(),
            todos: Vec::new(),
            transcript: Vec::new(),
            usage: SessionUsage::default(),
            attachment_refs: Vec::new(),
            attachment_losses: Vec::new(),
        }
    }
}

/// Reading a session file, v1 and v2 alike.
///
/// A session is DATA — the record of a conversation someone had. It is not a
/// statement of intent that can be *stale*, the way a config file is, so an old one
/// is migrated in place and never refused:
///
/// * v2 (`model: "zen://kimi-k2"`) parses as the identity it is;
/// * v1 with both halves (`model: "kimi-k2"`, `provider: "zen"`) is folded into one
///   [`ModelRef`] here, at the read;
/// * v1 with a model but no provider means "this model, on the provider currently in
///   effect" — which the file cannot know, so the identity is flagged
///   [`provider_unset`](SessionState::provider_unset) and the *resume* supplies the
///   provider in force (see the TUI's `adopt_state`);
/// * a file naming nothing at all falls back to the default identity.
impl<'de> Deserialize<'de> for SessionState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// The union of the v1 and v2 shapes, exactly as they appear on disk.
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            name: String,
            #[serde(default)]
            named_by_user: bool,
            #[serde(default)]
            read_only: bool,
            /// v2: `provider://model`. v1: a bare model id.
            #[serde(default)]
            model: Option<String>,
            /// v1 only.
            #[serde(default)]
            provider: Option<String>,
            #[serde(default)]
            base_url: String,
            #[serde(default)]
            cwd: String,
            #[serde(default, with = "persisted_messages")]
            messages: Vec<Message>,
            #[serde(default)]
            todos: Vec<TodoItem>,
            #[serde(default)]
            transcript: Vec<Entry>,
            #[serde(default)]
            usage: SessionUsage,
            /// Attachment references, one entry per message that has any.
            /// Absent on any file whose conversation had no attachments — which
            /// is every file written before the blob store existed.
            #[serde(default)]
            attachments: Vec<crate::MessageAttachments>,
        }

        let raw = Raw::deserialize(d)?;
        let spec = raw
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| m.parse::<ModelSpec>())
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let provider = raw
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let default: ModelRef = DEFAULT_MODEL_REF.parse().expect("a valid default identity");
        let (model, provider_unset) = match (spec, provider) {
            // v2, or a v1 file whose provider half we already have.
            (Some(ModelSpec::Full(r)), _) => (r, false),
            // A session file never names a provider with no model: it records what an
            // agent RAN on, which is always complete. Treat it as naming nothing.
            (Some(ModelSpec::ProviderOnly(p)), _) => (
                ModelRef::new(p, crate::DEFAULT_MODEL).unwrap_or(default),
                false,
            ),
            (Some(ModelSpec::ModelOnly(m)), Some(p)) => (
                ModelRef::new(crate::ProviderName::new(p), &m).unwrap_or(default),
                false,
            ),
            // v1, model only: the provider is whatever this process is on.
            (Some(spec @ ModelSpec::ModelOnly(_)), None) => (
                spec.apply(&default)
                    .expect("a bare model id always resolves"),
                true,
            ),
            (None, Some(p)) => (
                ModelRef::new(crate::ProviderName::new(p), crate::DEFAULT_MODEL).unwrap_or(default),
                false,
            ),
            (None, None) => (default, true),
        };
        Ok(SessionState {
            name: raw.name,
            named_by_user: raw.named_by_user,
            read_only: raw.read_only,
            id: None,
            model,
            provider_unset,
            base_url: raw.base_url,
            cwd: raw.cwd,
            messages: raw.messages,
            todos: raw.todos,
            transcript: raw.transcript,
            usage: raw.usage,
            attachment_refs: raw.attachments,
            attachment_losses: Vec::new(),
        })
    }
}

/// Serialize chat messages *for the session file*, which is not the OpenAI wire.
///
/// `Message`'s own `Serialize` is the wire format: it drops `reasoning_content`,
/// `anthropic_thinking_blocks` and `responses_reasoning_items` via
/// `skip_serializing`, because replaying a prior turn's plaintext thinking
/// degrades reasoning models, and because the latter two are each one
/// provider's native objects that the others reject. `ChatRequest` serializes
/// `Vec<Message>` straight onto the wire, so that invariant has to live on the
/// type.
///
/// A session file has the opposite requirement: it must preserve them. Losing
/// `anthropic_thinking_blocks` breaks a resumed Anthropic conversation whose
/// last assistant turn has a pending `tool_use` — the API requires the signed
/// thinking block on the follow-up turn. Losing `responses_reasoning_items`
/// silently costs a resumed Codex/Responses conversation its whole reasoning
/// chain: the model re-derives its plan from scratch, and pays output tokens to
/// do it, on every tool round after the resume.
///
/// Encoding therefore round-trips each message through its wire form and adds
/// the dropped fields back. Decoding needs no help: `skip_serializing` only
/// affects the encode side, and every one of them is `#[serde(default)]`.
mod persisted_messages {
    use super::Message;
    use crate::MessageOrigin;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(messages: &[Message], s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            let mut v = serde_json::to_value(m).map_err(S::Error::custom)?;
            let Some(obj) = v.as_object_mut() else {
                return Err(S::Error::custom("message did not serialize to an object"));
            };
            if let Some(r) = &m.reasoning_content {
                obj.insert(
                    "reasoning_content".into(),
                    serde_json::Value::from(r.clone()),
                );
            }
            if !m.anthropic_thinking_blocks.is_empty() {
                obj.insert(
                    "anthropic_thinking_blocks".into(),
                    serde_json::Value::Array(m.anthropic_thinking_blocks.clone()),
                );
            }
            if !m.responses_reasoning_items.is_empty() {
                obj.insert(
                    "responses_reasoning_items".into(),
                    serde_json::Value::Array(m.responses_reasoning_items.clone()),
                );
            }
            // Preserve internal origin marker so real user turns stay
            // distinguishable from injected context after a session resume.
            if m.origin != MessageOrigin::User {
                obj.insert(
                    "origin".into(),
                    serde_json::to_value(m.origin).map_err(S::Error::custom)?,
                );
            }
            out.push(v);
        }
        out.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Message>, D::Error> {
        Vec::<Message>::deserialize(d)
    }
}

impl SessionState {
    /// Refresh the fields whose runtime owners live elsewhere, immediately
    /// before a save.
    pub fn sync_from(&mut self, messages: Vec<Message>, todos: Vec<TodoItem>, cwd: String) {
        self.messages = messages;
        self.todos = todos;
        self.cwd = cwd;
        if self.name.is_empty() {
            self.name = crate::session_name_from(&self.messages)
                .unwrap_or_else(|| crate::default_session_name(&self.cwd));
        }
    }

    /// This state as it should be written to disk: the session-chrome notices
    /// are dropped.
    ///
    /// Every launch prints its own welcome, and every resume prints its own
    /// "resumed session …" line. Persisting them means the next resume restores
    /// the old ones *and* appends a fresh one, so they accrete one copy per
    /// resume, forever.
    ///
    /// Consumes `self` so the save pipeline never clones the whole state just to
    /// filter the transcript: every caller already owns the snapshot it is about
    /// to write.
    pub fn persisted(mut self) -> SessionState {
        self.transcript
            .retain(|e| !matches!(e.kind, crate::EntryKind::Notice(_)));
        self
    }

    /// Whether this conversation is worth persisting: it has at least one user
    /// message.
    pub fn is_saveable(&self) -> bool {
        self.messages
            .iter()
            .any(|m| m.role == crate::MessageRole::User)
    }

    /// Adopt a loaded session, settling any tool call or thought that was
    /// mid-run when it was saved (nothing can finish it now, and a running
    /// block would spin its spinner forever).
    pub fn restored(mut self) -> Self {
        crate::settle_restored_entries(&mut self.transcript);
        // `Entry::content_hash` is not persisted (it is derived), so every restored
        // entry arrives with a zeroed one. Rebuild them.
        //
        // The renderer caches a laid-out entry under `(index, content_hash, …)`. Left
        // at zero, the content half of that key is a constant across the whole restored
        // transcript, and entries are told apart only by their index — so the cache is
        // correct exactly as long as nothing ever shifts an index without clearing it.
        // That invariant holds today (every prune/truncate/clear does), but it is not
        // one worth resting on: the failure it buys is one restored message rendering
        // as another.
        for e in &mut self.transcript {
            e.refresh_hash();
        }
        self
    }
}

/// Maximum size of a single session file we are willing to load.
///
/// This prevents OOM from a corrupt or pathologically large session file.
/// The limit is generous: even a conversation with tens of thousands of turns
/// should fit comfortably within it.
pub const MAX_SESSION_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// A saved conversation: file metadata plus the [`SessionState`] it carries.
///
/// `Serialize` is hand-written rather than derived so that there is exactly ONE
/// on-disk shape ([`SessionBody`]): serializing a `Session` directly and letting
/// [`Session::save`] write it must not be able to produce different files, and a
/// derived impl would have quietly omitted the attachment references (which are
/// derived from the messages, not stored on the struct).
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
    #[serde(flatten)]
    pub state: SessionState,
}

impl Session {
    /// Wrap a state snapshot for writing to disk.
    pub fn new(state: SessionState) -> Self {
        let t = hrdr_tools::unix_now();
        Self {
            version: SESSION_VERSION,
            created: t,
            updated: t,
            state,
        }
    }

    /// The serialized shape of a save: [`Session`] with `created` taken from the
    /// created-cache instead of `self.created` (which is a fresh-timestamp guess
    /// from [`Self::new`] until the first save learns the file's real creation
    /// time). Borrowed, so a save serializes without cloning the whole state —
    /// the message history and transcript used to be deep-cloned once per write
    /// just to patch one `u64`.
    /// `attachments` describes the attachments currently on `state.messages`;
    /// [`Session::save`] passes the set it has just written blobs for, so a save
    /// hashes each attachment exactly once.
    fn body(&self, created: u64, attachments: Vec<crate::MessageAttachments>) -> SessionBody<'_> {
        SessionBody {
            version: self.version,
            created,
            updated: self.updated,
            attachments,
            state: &self.state,
        }
    }
}

impl Serialize for Session {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.body(
            self.created,
            crate::attachment_store::attachment_refs(&self.state.messages),
        )
        .serialize(s)
    }
}

/// The on-disk session body, field-for-field identical to [`Session`]'s
/// serialized shape except that `created` is supplied by the caller.
#[derive(serde::Serialize)]
struct SessionBody<'a> {
    version: u32,
    created: u64,
    updated: u64,
    /// Where each message's attachments are stored — the message's index, plus
    /// the name, media type, length and SHA-256 that names the blob
    /// ([`crate::attachment_store`]). The bytes are NOT here: a session file is
    /// rewritten on every tool round, and inlining base64 would re-serialize
    /// them all every time.
    ///
    /// Omitted entirely when there are none, so a conversation without
    /// attachments writes the same bytes it wrote before the store existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<crate::MessageAttachments>,
    #[serde(flatten)]
    state: &'a SessionState,
}

/// Lightweight directory listing entry.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// File stem — the id you `/resume` by.
    pub id: String,
    /// The session's display name (empty when the file could not be parsed).
    pub name: String,
    /// The working directory this session belongs to (empty when unreadable).
    pub cwd: String,
    pub updated: u64,
    /// Absolute path to the session file.
    pub path: PathBuf,
    /// `None` = valid session; `Some(reason)` = file could not be loaded.
    pub error: Option<String>,
}

// ── Reservation guard ─────────────────────────────────────────────────────────

/// An owned reservation guard that prevents two processes from claiming the
/// same session id.
///
/// Holds an exclusive lock file (`.{id}.lock`) in the session directory. The
/// lock is released when the guard is dropped — either through normal cleanup
/// (the save completed) or on early return / error propagation (a crash or
/// failed save leaves no permanent lock).
///
/// Each lock file carries the owner's PID and a timestamp so that a stale lock
/// (an old or orphaned one) can be detected and reaped rather than blocking
/// the candidate forever.
#[derive(Debug)]
pub struct Reservation {
    lock_path: PathBuf,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Returns `true` when the lock file at `path` was written by a process that
/// is no longer alive (or that started it long enough ago to be considered
/// abandoned).
///
/// A lock whose content doesn't parse as `PID TIMESTAMP` (e.g. one written by
/// an earlier hrdr build, which left the file empty) falls back to the file's
/// mtime for the age check: without that, an unparseable lock could never be
/// judged stale and would burn its slug forever.
fn is_stale_lock(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut parts = content.split_whitespace();
    let parsed: Option<(u32, u64)> = parts
        .next()
        .and_then(|p| p.parse().ok())
        .zip(parts.next().and_then(|t| t.parse().ok()));
    let Some((pid, ts)) = parsed else {
        // Unparseable owner: age by mtime alone — no PID to probe, so old
        // enough means stale.
        let age = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok());
        return age.is_some_and(|a| a.as_secs() >= STALE_LOCK_AGE_SECS);
    };
    let now = hrdr_tools::unix_now();
    // Not old enough — definitely not stale.
    if now < ts || now.saturating_sub(ts) < STALE_LOCK_AGE_SECS {
        return false;
    }
    // Old enough: reap only if the owning process is really gone.
    !owner_process_alive(pid)
}

/// Best-effort, zero-dependency check for whether process `pid` is still alive.
/// Errs toward "alive" when the probe is unavailable so a live writer's lock is
/// never stolen on a platform without a cheap liveness check.
fn owner_process_alive(pid: u32) -> bool {
    // Linux: `/proc/<pid>` exists iff the process exists — no syscall crate.
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    // macOS / other Unix: `kill -0` probes existence without sending a signal.
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(true)
    }
    // No cheap dependency-free probe on Windows. The caller only reaches here
    // once the lock is already older than STALE_LOCK_AGE_SECS, so assume the
    // owner is gone — a crashed writer's lock can still be reaped.
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Atomically claim `cand` via `O_EXCL` lock file (after reaping any stale
/// lock for the same candidate).
fn try_reserve(dir: &Path, cand: &str) -> Result<Reservation, ()> {
    let lock_path = dir.join(format!(".{cand}.lock"));
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => {
                // Write owner PID + creation timestamp into the lock so
                // a concurrent (or future) process can detect staleness.
                let content = format!("{} {}", std::process::id(), hrdr_tools::unix_now());
                let mut f = f;
                let _ = f.write_all(content.as_bytes());
                let _ = f.flush();
                drop(f);
                return Ok(Reservation { lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(&lock_path) {
                    // Reap the stale lock and retry O_EXCL.
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                return Err(());
            }
            Err(_) => return Err(()),
        }
    }
}

// ── Open lock (per-session ownership) ──────────────────────────────────────────

/// An owned guard proving this process has a session **open** for editing.
///
/// Only one hrdr instance may hold a given session open at a time: without it,
/// two windows resuming the same session both autosave, and the last write wins
/// — silently discarding the other window's turns. The guard holds an exclusive
/// lock file `.{id}.open.lock` in the session directory, released on drop
/// (a clean swap, `/new`, or process exit). A crash can't run `Drop`, so the
/// dead-PID lock it leaves behind is reaped on the next open (see
/// [`acquire_open_lock`] / [`is_stale_lock`]).
///
/// This is a **distinct** file from the brief `.{id}.lock` id-reservation that
/// [`unique_session_id`] takes and [`Session::save`] deletes: save's cleanup
/// removes only `.{id}.lock`, so it never disturbs a live open-lock, and the two
/// mechanisms don't interfere.
#[derive(Debug)]
pub struct SessionLock {
    lock_path: PathBuf,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// The session is already held open by a live process. Carries the owner's PID
/// and the unix timestamp it acquired the lock, so a caller can name it in a
/// clear refusal message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBusy {
    pub pid: u32,
    pub started: u64,
}

/// The open-lock file for `id` inside `dir` — deliberately `.{id}.open.lock`,
/// separate from the `.{id}.lock` reservation.
fn open_lock_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!(".{}.open.lock", sanitize_name(id)))
}

/// Parse the `PID TIMESTAMP` owner out of a held open-lock. Unparseable content
/// (e.g. one written by an older build) still counts as busy, just with a zero
/// owner — better to respect a lock we can't read than to steal it.
fn read_open_lock_owner(path: &Path) -> SessionBusy {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut parts = content.split_whitespace();
    let pid = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let started = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    SessionBusy { pid, started }
}

/// Acquire the per-session open-lock for `id` in `dir`.
///
/// `O_EXCL`-creates `.{id}.open.lock` and writes `PID TIMESTAMP` (the exact
/// format [`try_reserve`] uses). When the lock already exists this distinguishes
/// two cases via [`is_stale_lock`]:
///
/// * **stale** — the owner PID is gone (or the lock is older than
///   [`STALE_LOCK_AGE_SECS`] with no live owner): reap it and retry the create.
///   This is the dead-PID auto-cleanup, so a crashed instance never blocks the
///   session forever.
/// * **live** — a running owner: return [`SessionBusy`] with its PID + timestamp,
///   without touching the lock.
///
/// A create failure that is *not* `AlreadyExists` (an unwritable dir, say) can't
/// prove another owner, so rather than fabricate a `SessionBusy` or block the
/// user out of their own session it degrades to a best-effort guard whose file
/// was never created (its `Drop`'s `remove_file` is benign) — the same graceful
/// fallback [`unique_session_id`] takes.
pub fn acquire_open_lock(dir: &Path, id: &str) -> Result<SessionLock, SessionBusy> {
    let lock_path = open_lock_path(dir, id);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                let content = format!("{} {}", std::process::id(), hrdr_tools::unix_now());
                let _ = f.write_all(content.as_bytes());
                let _ = f.flush();
                drop(f);
                return Ok(SessionLock { lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(&lock_path) {
                    // Dead-PID / abandoned lock: reap and retry the O_EXCL create.
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                return Err(read_open_lock_owner(&lock_path));
            }
            // Not a contention error — degrade to an un-backed guard rather than
            // refusing the owner access to their own session.
            Err(_) => return Ok(SessionLock { lock_path }),
        }
    }
}

/// Acquire the open-lock for session `id` living under `cwd`'s directory.
pub fn acquire_session_lock(cwd: &str, id: &str) -> Result<SessionLock, SessionBusy> {
    acquire_open_lock(&session_dir(cwd), id)
}

/// The on-disk path of session `id` under `cwd` — `<cwd-slug>/<id>.json`.
pub fn session_file_path(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join(format!("{}.json", sanitize_name(id)))
}

/// The session id (file stem) for a path, handling both `<id>.json` and the
/// compressed `<id>.json.zst`: `file_stem()` alone leaves `<id>.json` on the
/// compressed form, so strip a trailing `.json` too.
fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(stem.strip_suffix(".json").unwrap_or(stem).to_string())
}

/// Why [`Session::open_path`] couldn't take ownership of a session.
#[derive(Debug)]
pub enum OpenError {
    /// Another live process holds the session's open-lock. Do not swap it in.
    Busy { pid: u32, started: u64 },
    /// The open-lock was acquired but the file failed to load; the lock has
    /// already been released before this is returned.
    Load(anyhow::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Busy { pid, .. } => {
                write!(f, "session is open in another hrdr instance (pid {pid})")
            }
            OpenError::Load(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// `$XDG_DATA_HOME/hrdr/sessions`, or `~/.local/share/hrdr/sessions`.
pub fn sessions_dir() -> PathBuf {
    // The fallback must be absolute: a relative path would scatter session
    // JSON into whatever directory the agent happens to run in.
    hjkl_xdg::data_dir("hrdr")
        .unwrap_or_else(|_| std::env::temp_dir().join("hrdr"))
        .join("sessions")
}

/// The per-cwd directory a session lives in.
fn session_dir(cwd: &str) -> PathBuf {
    sessions_dir().join(cwd_slug(cwd))
}

/// `sessions/<cwd-slug>/subagents/<session-id>/` — where a session's sub-agent
/// transcripts live (one `.jsonl` per delegated `task`).
pub fn child_transcript_dir(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join("subagents").join(sanitize_name(id))
}

/// `sessions/<cwd-slug>/<session-id>.jsonl` — the MAIN agent's durable transcript,
/// a sibling of its `<session-id>.json` snapshot. The append-only fold of its
/// `AgentEvent` stream, rebuilt on resume; the same shape as a sub-agent's, so
/// both agents share one on-disk transcript format.
pub fn session_transcript_path(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join(format!("{}.jsonl", sanitize_name(id)))
}

/// Reduce an arbitrary name to a safe, length-capped, lowercase file stem.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let s = s.trim_matches('-').to_lowercase();
    if s.is_empty() {
        "session".to_string()
    } else {
        s
    }
}

/// In-process cache of each session file's `created` timestamp, keyed by its
/// absolute path.
///
/// `created` is fixed at a file's first write and never changes again, so it
/// only ever needs to be *learned* once per path — either from a load, or (on
/// a cache miss) the one fallback read in [`Session::save`] below. Every
/// subsequent save for the same path is then a hash-map lookup instead of a
/// full read + JSON parse of the previous file — which otherwise carries the
/// entire message history, and autosave runs synchronously on the UI thread
/// after every turn, `!command`, cancel and rename.
fn created_cache() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Session {
    /// Save as `<cwd-slug>/<id>.json` (the cwd comes from `self.cwd`); returns
    /// the written path.
    pub fn save(&self, id: &str) -> Result<PathBuf> {
        let dir = session_dir(&self.state.cwd);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.json", sanitize_name(id)));
        // Autosave rebuilds a fresh `Session` per write; keep the original
        // creation time from the file being overwritten — from the in-process
        // cache when known (see `created_cache`), so this doesn't cost a
        // read + parse of the previous file on every save. A cache miss (the
        // first save this process makes to this path) falls back to reading
        // it once, exactly as before, and remembers the result.
        let cached = created_cache()
            .lock()
            .ok()
            .and_then(|c| c.get(&path).copied());
        let created = match cached {
            Some(c) => c,
            None => {
                let c = Self::load_path(&path).map_or(self.created, |prev| prev.created);
                if let Ok(mut cache) = created_cache().lock() {
                    cache.entry(path.clone()).or_insert(c);
                }
                c
            }
        };
        // Compact, not pretty: a real session reaches multiple MB (full message
        // history + transcript with tool results), and this is rewritten on every
        // tool round — pretty-printing spends ~15-30% more bytes and serialize CPU
        // per save for indentation no one reads (idle files are zstd-compressed by
        // retention anyway).
        // Attachment bytes go into the content-addressed store beside the file
        // BEFORE the file naming them lands, so a crash between the two leaves
        // blobs nothing references (collected by retention) rather than a session
        // referencing blobs that were never written.
        let attachments = crate::attachment_store::attachment_refs(&self.state.messages);
        // Both halves under the blob store's lock, so a peer's garbage collector
        // sees either no blob yet or the session that references it, never the
        // gap between (the guard lives to the end of this function, past the
        // `write_atomic` below). No attachments → no lock: that save adds
        // nothing to the collector's mark set, and it is the path taken on every
        // tool round. Contended → save anyway and fall back to the sweep's grace
        // window; see `lock_blob_store`.
        let _blob_lock = (!attachments.is_empty())
            .then(|| crate::attachment_store::lock_blob_store(&dir).ok())
            .flatten();
        crate::attachment_store::write_blobs(
            &crate::attachment_store::blob_dir(&path),
            &self.state.messages,
            &attachments,
        )?;
        let json = serde_json::to_string(&self.body(created, attachments))
            .context("serializing session")?;
        crate::write_atomic(&path, json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        // If retention had compressed this session, the plaintext we just wrote
        // is now the live copy — drop the stale `<id>.json.zst` so the two don't
        // coexist (and so a listing/load never picks the outdated compressed one).
        let compressed = path.with_extension("json.zst");
        if compressed.exists() {
            let _ = std::fs::remove_file(&compressed);
        }
        // Clean up the reservation lock left by `unique_session_id`, if any.
        // A save that was NOT preceded by a reservation (e.g. an autosave of an
        // already-assigned id) has no lock to clean up; `remove_file` is benign.
        let _ = std::fs::remove_file(dir.join(format!(".{}.lock", sanitize_name(id))));
        // Two writes can land within the filesystem's mtime granularity
        // (Windows timestamps tick coarsely), and `meta_cache` trusts an
        // unchanged mtime — so a listing right after e.g. a rename could
        // serve the pre-rename entry. This process just changed the file:
        // drop its cached meta so the next listing re-reads it.
        if let Ok(mut cache) = meta_cache().lock() {
            cache.remove(&path);
        }
        Ok(path)
    }

    /// Write this session's state atomically to an explicit path (used for a
    /// sub-agent's own state file, a child artifact of its parent session — so
    /// no cwd-slug id scheme and no open-lock, unlike [`Self::save`]). Creates
    /// parent dirs. Preserves `created` across rewrites via the same
    /// created-cache [`Self::save`] uses.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // Keep the original creation time from any file being overwritten — from
        // the in-process cache when known (see `created_cache`), else one fallback
        // read. Mirrors `save`'s created-time preservation exactly.
        let cached = created_cache()
            .lock()
            .ok()
            .and_then(|c| c.get(path).copied());
        let created = match cached {
            Some(c) => c,
            None => {
                let c = Self::load_path(path).map_or(self.created, |prev| prev.created);
                if let Ok(mut cache) = created_cache().lock() {
                    cache.entry(path.to_path_buf()).or_insert(c);
                }
                c
            }
        };
        // Attachment blobs first, exactly as `save` does — a sub-agent's snapshot
        // gets its own `blobs/` beside it, and it really is reached from either
        // direction: a user typing into a focused sub-agent pane sends attachments
        // through `send_to_subagent`, and the main agent's own `task` /
        // `task_steer` take an `attachments` argument, so a sub-agent's history
        // can hold them whoever sent them.
        let attachments = crate::attachment_store::attachment_refs(&self.state.messages);
        // Under the blob store's lock across both writes, on the same terms as
        // `save` — see the note there and on `lock_blob_store`.
        let _blob_lock = (!attachments.is_empty())
            .then(|| {
                crate::attachment_store::lock_blob_store(
                    path.parent().unwrap_or_else(|| Path::new(".")),
                )
                .ok()
            })
            .flatten();
        crate::attachment_store::write_blobs(
            &crate::attachment_store::blob_dir(path),
            &self.state.messages,
            &attachments,
        )?;
        // Compact, not pretty — see the note in `save`.
        let json = serde_json::to_string(&self.body(created, attachments))
            .context("serializing session")?;
        crate::write_atomic(path, json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Load `<cwd-slug>/<id>.json`; if that doesn't exist, try the compressed
    /// `<cwd-slug>/<id>.json.zst` (retention may have compressed an idle session).
    pub fn load(cwd: &str, id: &str) -> Result<Session> {
        let stem = sanitize_name(id);
        let json = session_dir(cwd).join(format!("{}.json", stem));
        let zst = session_dir(cwd).join(format!("{}.json.zst", stem));
        if json.exists() {
            Self::load_path(&json)
        } else {
            Self::load_path(&zst)
        }
    }

    /// Load a session directly from a file path. The file id isn't stored in the
    /// file — it *is* the file name — so it's filled in here.
    ///
    /// Returns an error when the file exceeds [`MAX_SESSION_FILE_BYTES`].
    ///
    /// Safety: opens the file once and reads through the opened handle to
    /// eliminate the TOCTOU race between stat and open. The limit is enforced
    /// on the metadata length *and* on the bytes actually read (in case the
    /// file grew between the checks).
    pub fn load_path(path: &Path) -> Result<Session> {
        let mut session = Self::parse_path(path)?;
        // The transcript is no longer embedded in the `.json` (see
        // `SessionState::transcript`). Rebuild it from the sibling `<id>.jsonl` —
        // the append-only fold of the agent's event stream — through the SAME
        // reducer the live path uses, so a resumed transcript renders identically.
        // `with_file_name` gives the sibling regardless of `.json` vs `.json.zst`.
        // An older file that still carries an embedded transcript and has no jsonl
        // keeps the one it deserialized (the fallback branch does nothing).
        if let Some(stem) = session.state.id.as_deref() {
            let jsonl = path.with_file_name(format!("{stem}.jsonl"));
            if jsonl.exists() {
                session.state.transcript = crate::transcript_log::read_transcript(&jsonl);
            }
        }
        // Put the attachment bytes back on the messages that reference them,
        // verifying each blob against the digest the file recorded. Anything that
        // does not come back is dropped and REPORTED — never silently — because
        // the message text still names it (see `resolve_attachments`).
        let refs = std::mem::take(&mut session.state.attachment_refs);
        if !refs.is_empty() {
            session.state.attachment_losses = crate::attachment_store::resolve_attachments(
                &crate::attachment_store::blob_dir(path),
                &refs,
                &mut session.state.messages,
            );
        }
        // A load always knows the true `created` for this path — remember it,
        // so a later `save` (an autosave, a rename, …) doesn't have to re-read
        // the file just to preserve it.
        if let Ok(mut cache) = created_cache().lock() {
            cache.insert(path.to_path_buf(), session.created);
        }
        Ok(session)
    }

    /// Read and parse a session file, and nothing more: no transcript fold, and
    /// attachment references left AS references, in
    /// [`SessionState::attachment_refs`].
    ///
    /// [`Self::load_path`] is this plus the resolution steps. The blob GC
    /// (`sweep_dir`) wants the unresolved form on its own — its mark phase only
    /// needs to know which digests are still spoken for, and resolving would pull
    /// every attachment in every surviving session into memory to learn it.
    fn parse_path(path: &Path) -> Result<Session> {
        let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = f
            .metadata()
            .with_context(|| format!("reading metadata for {}", path.display()))?
            .len();
        // A `.json.zst` file is zstd-compressed (retention compressed an idle
        // session); decode it through the same output cap. The limit for a
        // plaintext file is on its own length; for a compressed one it is on the
        // *decompressed* bytes (a small archive can expand past the cap), so both
        // paths read through `take(limit + 1)` and check the resulting length.
        let is_compressed = path.extension().and_then(|e| e.to_str()) == Some("zst");
        let mut data = String::new();
        if is_compressed {
            let dec = zstd::stream::read::Decoder::new(f)
                .with_context(|| format!("opening zstd stream for {}", path.display()))?;
            dec.take(MAX_SESSION_FILE_BYTES + 1)
                .read_to_string(&mut data)
                .with_context(|| format!("decompressing {}", path.display()))?;
        } else {
            if len > MAX_SESSION_FILE_BYTES {
                anyhow::bail!(
                    "session file {} is {:.1} MiB, exceeds the {:.1} MiB limit",
                    path.display(),
                    len as f64 / (1024.0 * 1024.0),
                    MAX_SESSION_FILE_BYTES as f64 / (1024.0 * 1024.0),
                );
            }
            data.reserve(1024.min(len as usize));
            // Read through a limit so a file that grew between metadata and read
            // cannot OOM the process.
            f.take(MAX_SESSION_FILE_BYTES + 1)
                .read_to_string(&mut data)
                .with_context(|| format!("reading {}", path.display()))?;
        }
        // Reject the data if more bytes were present than allowed (take()
        // silently truncates past the limit, so we must check).
        if data.len() as u64 > MAX_SESSION_FILE_BYTES {
            anyhow::bail!(
                "session file {} exceeds the {:.1} MiB limit",
                path.display(),
                MAX_SESSION_FILE_BYTES as f64 / (1024.0 * 1024.0),
            );
        }
        let mut session: Session =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        session.state.id = session_id_from_path(path);
        Ok(session)
    }

    /// Open a session for **exclusive** editing: acquire its open-lock *first*
    /// (so we never read a session another live window owns), then load it.
    ///
    /// On success returns the session paired with the [`SessionLock`] the caller
    /// must hold for as long as the session stays active — dropping the guard
    /// releases the lock. On [`OpenError::Busy`] the file is **not** loaded. A
    /// load error after the lock was taken releases it before returning.
    ///
    /// This is the ownership-taking counterpart to [`Self::load_path`], which
    /// stays lock-free for listing, preview and tests.
    pub fn open_path(path: &Path) -> Result<(Session, SessionLock), OpenError> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let id = session_id_from_path(path).unwrap_or_else(|| "session".to_string());
        let lock = acquire_open_lock(dir, &id)
            .map_err(|SessionBusy { pid, started }| OpenError::Busy { pid, started })?;
        match Self::load_path(path) {
            Ok(session) => Ok((session, lock)),
            Err(e) => {
                drop(lock); // release the lock we just took before surfacing the error
                Err(OpenError::Load(e))
            }
        }
    }

    /// [`Self::open_path`] keyed by `cwd` + `id` (the locked counterpart to
    /// [`Self::load`]).
    pub fn open(cwd: &str, id: &str) -> Result<(Session, SessionLock), OpenError> {
        Self::open_path(&session_file_path(cwd, id))
    }

    /// Open a **copy** of the session at `source_path` as a fresh, independently
    /// owned session under `cwd` — the escape hatch when the source is held open
    /// by another live instance (an [`OpenError::Busy`]) and can't be resumed
    /// directly.
    ///
    /// The source is read with the **unlocked** [`Self::load_path`]: reading is
    /// safe because writes are atomic (a whole old-or-new file is always seen),
    /// and deliberately *not* taking the source's open-lock is the whole point —
    /// the other instance holds it, and the fork must not disturb it. The source
    /// file and its `.{id}.open.lock` are left entirely untouched.
    ///
    /// The copy is renamed `"{orig} (fork)"` (falling back to a plain base when
    /// the source has no name), given a fresh collision-free id, and persisted via
    /// the ordinary new-session save path ([`crate::save_session`]) — so it lands
    /// with BOTH its file written AND its own `.{id}.open.lock` held. The returned
    /// [`SessionLock`] is exactly the guard a first save mints; the caller holds it
    /// for as long as the fork stays active.
    ///
    /// The source's display transcript (its sibling `<id>.jsonl`) is copied to the
    /// fork's jsonl too, so the copy opens with the same conversation on screen.
    ///
    /// Returns `(new_id, forked_session, fork_lock)`.
    pub fn fork(cwd: &str, source_path: &Path) -> Result<(String, Session, SessionLock)> {
        // Read the source's current on-disk snapshot WITHOUT taking its lock.
        let mut state = Self::load_path(source_path)
            .with_context(|| format!("reading {}", source_path.display()))?
            .state;
        // Derive a fork name from the source's, falling back to a sensible base.
        let base = state.name.trim();
        let base = if base.is_empty() {
            crate::session_name_from(&state.messages)
                .unwrap_or_else(|| crate::default_session_name(cwd))
        } else {
            base.to_string()
        };
        // Force the new-session save path to mint a fresh id + open-lock, and file
        // the copy under this window's cwd.
        state.name = format!("{base} (fork)");
        state.id = None;
        state.cwd = cwd.to_string();
        let outcome = crate::save_session(&state)?
            .context("forked session had no user message to persist")?;
        let lock = outcome
            .open_lock
            .context("could not acquire the forked session's open-lock")?;
        // Carry the source's display transcript over: the fork keeps the
        // conversation's display fold, without which the copy opens with an
        // empty pane. Best-effort — a failed copy must not fail the fork, and a
        // source with no jsonl keeps an empty fork transcript as before.
        if let Some(source_id) = session_id_from_path(source_path) {
            let source_jsonl = source_path.with_file_name(format!("{source_id}.jsonl"));
            let fork_jsonl =
                session_file_path(cwd, &outcome.id).with_file_name(format!("{}.jsonl", outcome.id));
            if source_jsonl.exists() {
                let _ = std::fs::copy(&source_jsonl, &fork_jsonl);
            }
        }
        // Reload so the returned session reflects exactly what was written
        // (persisted transcript, id stamped from the filename).
        let session = Self::load_path(&session_file_path(cwd, &outcome.id))
            .with_context(|| format!("reloading forked session {}", outcome.id))?;
        Ok((outcome.id, session, lock))
    }
}

/// Resolve a `/resume` argument to `(id, Session)`. Looks in `cwd`'s directory
/// first (by file id), then scans every directory — preferring the current
/// `cwd` — matching the file id or the display `name` (case-insensitive, e.g.
/// after `/rename`).
pub fn resolve_session(cwd: &str, arg: &str) -> Option<(String, Session)> {
    let id = sanitize_name(arg);
    if let Ok(s) = Session::load(cwd, &id) {
        return Some((id, s));
    }
    let cur = cwd_slug(cwd);
    let mut metas = list_sessions();
    // Stable sort keeps newest-first ordering within each group; current cwd
    // (key `false`) sorts ahead of the rest.
    metas.sort_by_key(|m| cwd_slug(&m.cwd) != cur);
    let arg = arg.trim();
    metas
        .into_iter()
        .find(|m| m.name.eq_ignore_ascii_case(arg) || m.id.eq_ignore_ascii_case(arg))
        .and_then(|m| Session::load_path(&m.path).ok().map(|s| (m.id, s)))
}

/// A collision-free file id (within `cwd`'s directory) derived from `name`:
/// the slug, then `slug-2`, `slug-3`, … if files already exist.
///
/// The id is **reserved atomically** via a hidden lock file created with
/// `O_EXCL` that carries the caller's PID and a timestamp, so two concurrent
/// processes in the same cwd never race on the existence check and mint the
/// same id. The returned [`Reservation`] guard releases the lock on drop (a
/// crash or error leaves no permanent lock behind).
///
/// A lock whose owner PID no longer exists (or that is older than
/// [`STALE_LOCK_AGE_SECS`]) is reaped automatically, so a crashed writer
/// does not block future sessions forever.
pub fn unique_session_id(cwd: &str, name: &str) -> (String, Reservation) {
    let slug = sanitize_name(name);
    let dir = session_dir(cwd);
    let _ = std::fs::create_dir_all(&dir);

    // Fail fast: if the directory is unwritable, skip the 10k probe loop.
    let dir_ok = dir.is_dir();

    for i in 1..10_000 {
        if !dir_ok {
            break;
        }
        let cand = if i == 1 {
            slug.clone()
        } else {
            format!("{slug}-{i}")
        };
        let json_path = dir.join(format!("{cand}.json"));
        let zst_path = dir.join(format!("{cand}.json.zst"));

        match try_reserve(&dir, &cand) {
            Ok(res) => {
                // The session may have appeared before this reservation.
                // Check only after owning the lock — no TOCTOU race.  Both
                // plaintext and retention-compressed forms occupy the id.
                if json_path.exists() || zst_path.exists() {
                    drop(res); // releases the lock
                    continue;
                }
                return (cand, res);
            }
            Err(()) => continue,
        }
    }
    // Fallback: nanosecond timestamp (very unlikely to collide).
    let fallback = format!(
        "{slug}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    // Last resort. Bounded: an unwritable session dir makes `try_reserve`
    // fail on EVERY attempt, and an unbounded retry loop would hard-hang the
    // app inside what looks like a pure id-picking call. After the bounded
    // tries, return the id unreserved — a `Reservation` whose lock file was
    // never created (its `Drop`'s `remove_file` is benign). That degrades to
    // the pre-reservation race behavior instead of never returning; the save
    // itself will surface the real filesystem error to the user.
    for _ in 0..8 {
        if let Ok(res) = try_reserve(&dir, &fallback) {
            return (fallback, res);
        }
    }
    let lock_path = dir.join(format!(".{fallback}.lock"));
    (fallback, Reservation { lock_path })
}

/// In-process cache of each session file's [`SessionMeta`], keyed by its
/// absolute path and guarded by the file's mtime.
///
/// [`list_sessions`] is called from `arg_completions` for `/resume`, which
/// runs on every keystroke while that argument is being typed — and a full
/// [`Session::load_path`] deserializes the WHOLE session (message history and
/// transcript included) just to read three metadata fields. Checking a file's
/// mtime is a cheap stat; a file whose mtime hasn't moved since it was last
/// read need not be re-read (or re-parsed) at all.
fn meta_cache() -> &'static Mutex<HashMap<PathBuf, (SystemTime, SessionMeta)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, SessionMeta)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Collect session files from one directory into `out`.
fn collect_sessions(dir: &Path, out: &mut Vec<SessionMeta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // A session lives as `<id>.json` or, once retention compressed it, as
        // `<id>.json.zst`. Skip anything else (lock files, the `subagents/` dir).
        let is_session = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with(".json") || n.ends_with(".json.zst"));
        if !is_session {
            continue;
        }
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
        if let Some(mtime) = mtime {
            let cached = meta_cache()
                .lock()
                .ok()
                .and_then(|c| c.get(&path).cloned())
                .filter(|(cached_mtime, _)| *cached_mtime == mtime);
            if let Some((_, meta)) = cached {
                out.push(meta);
                continue;
            }
        }
        let id = session_id_from_path(&path).unwrap_or_default();
        // The unresolved parse, not `load_path`: a listing wants three metadata
        // fields, and resolving would read every attachment of every session in
        // the store into memory — on every keystroke of a `/resume` argument —
        // to produce a name, a cwd and a timestamp.
        match Session::parse_path(&path) {
            Ok(s) => {
                let meta = SessionMeta {
                    id,
                    name: s.state.name,
                    cwd: s.state.cwd,
                    updated: s.updated,
                    path: path.clone(),
                    error: None,
                };
                if let (Some(mtime), Ok(mut cache)) = (mtime, meta_cache().lock()) {
                    cache.insert(path, (mtime, meta.clone()));
                }
                out.push(meta);
            }
            Err(err) => {
                // A session file that could not be parsed is still listed
                // (as an error row) so the `/resume` picker and `/doctor`
                // can report it, rather than silently disappearing.
                let ts = mtime
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.push(SessionMeta {
                    id,
                    name: String::new(),
                    cwd: String::new(),
                    updated: ts,
                    path: path.clone(),
                    error: Some(format!("{err:#}")),
                });
            }
        }
    }
}

/// List saved sessions across every working directory, newest first. Also
/// picks up legacy flat-layout files written directly under `sessions/`.
pub fn list_sessions() -> Vec<SessionMeta> {
    let base = sessions_dir();
    let mut out = Vec::new();
    // Legacy flat layout.
    collect_sessions(&base, &mut out);
    // Per-cwd subdirectories.
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_sessions(&path, &mut out);
            }
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.updated));
    out
}

// ── retention: compress idle sessions, purge old auto-named ones ──────────────
//
// A background worker calls [`sweep_sessions`]
// every hour. It is peer-safe: multiple hrdr instances can run at once, and the
// sweep acts on a session only if it can take that session's open-lock — which
// fails (skipped) when a live instance is using it or another sweeper is on it.

/// One retention pass over every stored session. `compress_after` /
/// `purge_after` are ages in seconds; `None` or `0` disables that phase.
///
/// - A session whose file mtime is older than `compress_after` and is still
///   plaintext (`<id>.json`) is zstd-compressed to `<id>.json.zst`, preserving
///   its mtime so the purge clock is not reset.
/// - A session whose file mtime is older than `purge_after` is deleted — but
///   ONLY if it was not named by the user (`named_by_user == false`).
///
/// Best-effort throughout: any per-file error (a busy lock, an unreadable file)
/// skips that file and the sweep continues. Cheap to call when nothing is due —
/// it decides by `stat` mtime and only opens a file it is about to act on.
pub fn sweep_sessions(compress_after: Option<u64>, purge_after: Option<u64>) {
    let compress_after = compress_after.filter(|&t| t > 0);
    let purge_after = purge_after.filter(|&t| t > 0);
    if compress_after.is_none() && purge_after.is_none() {
        return;
    }
    let base = sessions_dir();
    // Legacy flat layout, then each per-cwd subdirectory. `subagents/` sits under
    // a cwd dir (not a direct child of base) and holds `.jsonl` files, so it is
    // never treated as a session dir nor swept.
    sweep_dir(&base, compress_after, purge_after);
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sweep_dir(&path, compress_after, purge_after);
            }
        }
    }
}

/// Sweep one directory of session files (non-recursive), acting on each that is
/// past a threshold. See [`sweep_sessions`].
fn sweep_dir(dir: &Path, compress_after: Option<u64>, purge_after: Option<u64>) {
    let now = hrdr_tools::unix_now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Set once a purge actually deletes something, so the blob GC below — which
    // has to re-read every surviving session in the directory — only runs when
    // there is a reason to.
    let mut purged = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let is_json = name.ends_with(".json");
        if !is_json && !name.ends_with(".json.zst") {
            continue;
        }
        let Some(mtime) = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
        else {
            continue;
        };
        let age = now.saturating_sub(mtime);
        let want_purge = purge_after.is_some_and(|t| age > t);
        let want_compress = is_json && compress_after.is_some_and(|t| age > t);
        if !want_purge && !want_compress {
            continue;
        }
        let Some(id) = session_id_from_path(&path) else {
            continue;
        };
        // Take the session's open-lock. Busy → a live instance is using it, or
        // another sweeper is on it; either way, skip. Held for the whole action.
        let Ok(_lock) = acquire_open_lock(dir, &id) else {
            continue;
        };
        if want_purge {
            match Session::load_path(&path) {
                // Auto-named and old enough → purge outright, together with the
                // session's derived data: its sibling transcript jsonl and its
                // sub-agents' transcript dir. Otherwise deleting the `.json` orphans
                // them under `sessions/<cwd>/`.
                Ok(s) if !s.state.named_by_user => {
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(dir.join(format!("{id}.jsonl")));
                    let _ = std::fs::remove_dir_all(dir.join("subagents").join(&id));
                    // Its attachment blobs are shared with every other session in
                    // this directory, so they cannot be deleted from here — the
                    // GC below decides, once every survivor has been consulted.
                    purged = true;
                    continue;
                }
                // User-named → keep. It may still be a compression candidate below.
                Ok(_) => {}
                // Unparseable → leave it for `/doctor` rather than delete blindly.
                Err(_) => continue,
            }
        }
        if want_compress {
            let _ = compress_session_file(&path);
        }
    }
    if purged {
        collect_blob_garbage(dir);
    }
}

/// Delete the attachment blobs in `dir` that no session left in `dir` still
/// references — the cleanup half of a purge.
///
/// Blobs are content-addressed and shared: two sessions that attached the same
/// image name the same file, so a purged session's references are not its
/// property to delete. The set of live digests is therefore rebuilt from the
/// survivors (mark), and only what is in no survivor is unlinked (sweep).
///
/// **A directory with any unreadable session is left completely alone.** An
/// incomplete mark set does not produce a smaller cleanup, it produces deletion
/// of blobs something still needs — and `sweep_dir` deliberately keeps
/// unparseable session files rather than deleting them, so this case is real.
///
/// **The store's lock is held across mark AND sweep**, so no peer can write a
/// blob and the session naming it in between the two. A lock we cannot take
/// means a peer is writing right now, which is exactly when a mark set goes
/// stale: skip the whole collection, and let the next hourly pass do it.
fn collect_blob_garbage(dir: &Path) {
    let blobs = crate::attachment_store::blob_dir_in(dir);
    if !blobs.is_dir() {
        return;
    }
    let Ok(_lock) = crate::attachment_store::lock_blob_store(dir) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut keep: HashSet<String> = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_session = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with(".json") || n.ends_with(".json.zst"));
        if !is_session {
            continue;
        }
        // The unresolved parse: the mark phase needs the digests, not the bytes.
        let Ok(session) = Session::parse_path(&path) else {
            return; // a session we cannot read may reference anything — do nothing
        };
        for entry in &session.state.attachment_refs {
            keep.extend(entry.files.iter().map(|r| r.sha256.clone()));
        }
    }
    crate::attachment_store::sweep_blobs(&blobs, &keep);
}

/// Compress a plaintext `<id>.json` to `<id>.json.zst` (zstd level 3), atomically
/// and preserving the original mtime, then remove the plaintext. The caller must
/// hold the session's open-lock. Returns an error without touching the plaintext
/// if any step before the swap fails.
fn compress_session_file(json_path: &Path) -> Result<()> {
    let orig_mtime = std::fs::metadata(json_path)
        .and_then(|m| m.modified())
        .with_context(|| format!("reading mtime for {}", json_path.display()))?;
    let data =
        std::fs::read(json_path).with_context(|| format!("reading {}", json_path.display()))?;
    // Level 3 is zstd's default — strong ratio for very little CPU.
    let compressed = zstd::encode_all(&data[..], 3)
        .with_context(|| format!("compressing {}", json_path.display()))?;
    let zst_path = json_path.with_extension("json.zst");
    crate::write_atomic(&zst_path, &compressed)
        .with_context(|| format!("writing {}", zst_path.display()))?;
    // Keep the compressed file's mtime equal to the plaintext's, so compression
    // does not reset the "last used" clock the purge phase reads.
    let _ = filetime::set_file_mtime(&zst_path, filetime::FileTime::from_system_time(orig_mtime));
    // Only now drop the plaintext: the compressed copy is fully written.
    std::fs::remove_file(json_path).with_context(|| format!("removing {}", json_path.display()))?;
    Ok(())
}

// ── save_session: persistence shared by the frontends ─────────────────────────

/// The result of a session-id mint: the session's file id, whether this call was
/// the one that first assigned it (so the frontend can notify once), the
/// open-lock the frontend must hold for the session's lifetime, and — on a
/// first mint — the reservation that must stay alive until the first write
/// lands (its drop removes the `.id.lock` that keeps a second instance from
/// minting the same id).
pub struct SaveOutcome {
    pub id: String,
    pub first_save: bool,
    /// The freshly-minted session's open-lock, present only on `first_save`. The
    /// frontend must hold it for as long as the session is active (see
    /// [`crate::SessionLock`]) so another instance's auto-resume can't adopt this
    /// brand-new session and collide. `None` on every subsequent save, and on the
    /// (near-impossible) case that the minted id's open-lock was already held.
    pub open_lock: Option<crate::SessionLock>,
    /// The id-reservation guard, present only on a first mint. The caller must
    /// hold it until the first write attempt ends: its drop removes the
    /// `.id.lock` a failed first write would otherwise leave behind until it
    /// aged out as stale.
    pub reservation: Option<crate::Reservation>,
}

/// Claim a session's file id without writing anything: derive a collision-free
/// id from the state's cwd+name (see [`crate::unique_session_id`]), take its
/// open-lock, and reserve it against a second instance minting the same id
/// before the first write lands. `Ok(None)` when there's nothing worth saving
/// yet (no user message).
///
/// This is the synchronous, cheap half of [`save_session`] — the part a
/// frontend needs *before* the write can go off-thread: the id, the open-lock
/// to hold, and the reservation to keep alive until the write completes. The
/// write itself (`Session::save`) can then run on a background task without the
/// UI thread waiting on the disk.
pub fn mint_session(state: &SessionState) -> anyhow::Result<Option<SaveOutcome>> {
    if !state.is_saveable() {
        return Ok(None);
    }
    if let Some(id) = &state.id {
        return Ok(Some(SaveOutcome {
            id: id.clone(),
            first_save: false,
            open_lock: None,
            reservation: None,
        }));
    }
    let (id, reservation) = crate::unique_session_id(&state.cwd, &state.name);
    // Take the per-session open-lock for the freshly-minted id, keyed by the
    // same directory the file lands in. `.ok()` degrades gracefully: the
    // brand-new id is unique, so a live open-lock for it is near-impossible;
    // if the acquire somehow can't take it we still save (just without the
    // extra guard) rather than erroring the user's first turn.
    let lock = crate::acquire_session_lock(&state.cwd, &id).ok();
    Ok(Some(SaveOutcome {
        id,
        first_save: true,
        open_lock: lock,
        reservation: Some(reservation),
    }))
}

/// Persist a conversation as a session. Returns `Ok(None)` when there's nothing
/// worth saving yet (no user message); filesystem failures are returned so a
/// frontend never claims an unsaved conversation is durable.
///
/// `state` is the frontend's whole session state, written as-is apart from the
/// ephemeral session-chrome notices ([`SessionState::persisted`]). The file id
/// comes from `state.id` when the session already has one, otherwise a fresh
/// collision-free id is derived from its name (see [`crate::unique_session_id`])
/// and reported back as `first_save`.
pub fn save_session(state: &SessionState) -> anyhow::Result<Option<SaveOutcome>> {
    let Some(outcome) = mint_session(state)? else {
        return Ok(None);
    };
    // `persisted` consumes the state; `save_session` only holds a reference
    // (this is the once-per-session first-save path, not the per-round one), so
    // clone here.
    Session::new(state.clone().persisted()).save(&outcome.id)?;
    // The reservation is dropped here. If `save` failed above, the drop
    // removes the lock file that `unique_session_id` created — no stale
    // lock is left behind. If `save` succeeded, `save()` already removed
    // the reservation lock (`.{id}.lock`, distinct from the open-lock); the
    // second `remove_file` in `Reservation::drop` is benign. `open_lock` is
    // NOT dropped — it is handed to the caller to hold.
    Ok(Some(SaveOutcome {
        reservation: None,
        ..outcome
    }))
}

/// Longest auto-derived session name, in characters. Long enough to stay
/// recognisable in the `/resume` picker, short enough that the name column does
/// not push the other columns off a narrow terminal.
pub const SESSION_NAME_MAX_CHARS: usize = 60;

/// A short session name derived from the first real user turn (first line,
/// trimmed, capped at [`SESSION_NAME_MAX_CHARS`]), or `None` when there is no
/// usable text. A user-role message that is compaction's own summary, a nudge
/// or a detached sub-agent report is not the user speaking and does not name
/// the session — after a compaction the first user-role message is the summary,
/// and naming a session after it would be "This conversation was compacted…".
///
/// The caller supplies the default when this returns `None`
/// ([`default_session_name`] — the working directory's basename).
pub fn session_name_from(msgs: &[Message]) -> Option<String> {
    msgs.iter()
        .find(|m| crate::compaction::is_user_turn(m))
        .and_then(|m| m.content.as_deref())
        // The user turn's content carries an immutable model-facing timestamp
        // prefix; a session name is for humans, so strip it first.
        .map(crate::strip_user_timestamp)
        .map(|c| {
            c.lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(SESSION_NAME_MAX_CHARS)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
}

/// The default session name when no conversation-derived name exists: the
/// working directory's basename (`~/Projects/my-app` → `my-app`), so an
/// unnamed session is identifiable in the `/resume` picker instead of reading
/// "untitled". Falls back to `"untitled"` only when the cwd has no basename
/// (the filesystem root, or an empty path).
pub fn default_session_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "untitled".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryKind;
    use crate::MessageRole;
    use std::sync::Mutex;

    /// Global lock so env-var-dependent session tests don't race on HOME / XDG
    /// vars (std::env::set_var is not thread-safe in Rust tests).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn child_transcript_dir_nests_under_session() {
        // Both paths below are derived from $XDG_DATA_HOME, and `with_test_env`
        // swaps that process-global for other tests. Take the same lock: without
        // it a concurrent swap lands between the two calls and they disagree —
        // a latent race that only showed up once the suite grew enough to
        // reschedule around it.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = child_transcript_dir("/home/me/proj", "My Session");
        // sessions/<cwd-slug>/subagents/<sanitized-id>
        assert!(dir.ends_with("subagents/my-session"), "got {dir:?}");
        assert!(
            dir.to_string_lossy().contains("home-me-proj"),
            "keyed by cwd slug: {dir:?}"
        );
        // Shares the session's per-cwd directory.
        assert!(dir.starts_with(session_dir("/home/me/proj")));
    }

    /// Set XDG_DATA_HOME to an isolated temp dir for the duration of `f`.
    pub(super) fn with_test_env(f: impl FnOnce(&tempfile::TempDir)) {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", tmp.path());
        }
        f(&tmp);
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    /// A saveable state: one user message, named, rooted at `cwd`.
    pub(super) fn state(name: &str, cwd: &str) -> SessionState {
        SessionState {
            name: name.to_string(),
            model: "local://model".parse().unwrap(),
            base_url: "http://x/v1".to_string(),
            cwd: cwd.to_string(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        }
    }

    /// A session first named after a compaction must not take its name from the
    /// summary — the summary is user-role but is not the user speaking, and
    /// neither is a nudge or a detached sub-agent report. Named from the first
    /// real user turn only; falls back to `"untitled"` when there is none.
    #[test]
    fn session_name_skips_compaction_summary_and_other_synthetic_user_roles() {
        let summary = Message {
            role: MessageRole::User,
            content: Some("This conversation was compacted…".to_string()),
            reasoning_content: None,
            anthropic_thinking_blocks: vec![],
            responses_reasoning_items: vec![],
            attachments: vec![],
            origin: crate::MessageOrigin::Summary(crate::CompactionReason::ContextOverflow),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let nudge = Message {
            origin: crate::MessageOrigin::Nudge,
            ..summary.clone()
        };
        assert_eq!(
            session_name_from(&[summary.clone(), Message::user("Deploy the fix")]),
            Some("Deploy the fix".to_string()),
            "the summary does not name the session; the first real user turn does"
        );
        assert_eq!(
            session_name_from(&[nudge, summary.clone(), Message::user("Ship it")]),
            Some("Ship it".to_string())
        );
        assert_eq!(
            session_name_from(&[summary]),
            None,
            "no real user turn → no derived name; the caller supplies the default"
        );
    }

    /// The default session name is the working directory's basename, so an
    /// unnamed session reads "my-app" in the `/resume` picker instead of
    /// "untitled" — "untitled" survives only for a cwd with no basename.
    #[test]
    fn default_session_name_is_the_cwd_basename() {
        assert_eq!(default_session_name("/home/me/Projects/my-app"), "my-app");
        assert_eq!(default_session_name("/home/me/My Project"), "My Project");
        assert_eq!(default_session_name("/"), "untitled");
        assert_eq!(default_session_name(""), "untitled");
    }

    /// Async endpoint/catalog warnings are `Notice` entries, which `persisted()`
    /// strips — so they never persist and cannot accrete a fresh copy across
    /// save/resume cycles (the Task 6 diagnostics invariant).
    #[test]
    fn async_warning_notices_do_not_persist_or_accrete() {
        let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
        let mut s = state("Chat", "/tmp/p");
        s.transcript = vec![
            Entry::at(EntryKind::User("hi".into()), t),
            Entry::at(
                EntryKind::Notice("Showing built-in ChatGPT models.".into()),
                t,
            ),
        ];
        // Save strips the async-warning Notice; the User entry is kept.
        let saved = s.persisted();
        let notices = |st: &SessionState| {
            st.transcript
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Notice(_)))
                .count()
        };
        assert_eq!(notices(&saved), 0, "async warnings are not persisted");
        assert_eq!(
            saved
                .transcript
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::User(_)))
                .count(),
            1
        );
        // Resume restores `saved`; a fresh warning is added at runtime and it is
        // re-saved — still no accreted warnings.
        let mut resumed = saved.clone();
        resumed.transcript.push(Entry::at(
            EntryKind::Notice("Showing built-in ChatGPT models.".into()),
            t,
        ));
        assert_eq!(
            notices(&resumed.persisted()),
            0,
            "warnings do not accrete across resume cycles"
        );
    }

    /// `save_to_path` writes to an explicit file (creating parent dirs) and the
    /// state round-trips back through `load_path` — the sub-agent snapshot
    /// primitive. It carries **messages + metadata** (the model context a
    /// `task_revive` relaunch needs); the sub-agent's *transcript* is its own
    /// sibling jsonl, not this snapshot, so it is deliberately absent here.
    #[test]
    fn save_to_path_round_trips_through_load_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subagents").join("000-probe.json");
        let mut s = state("Sub Agent", "/tmp/proj");
        // A snapshot taken with a live in-memory transcript still writes none of it
        // to the `.json`: the transcript is not part of the snapshot's serde shape.
        s.transcript = vec![Entry::now(EntryKind::Tool {
            id: "call_1".into(),
            name: "read".into(),
            args: "{\"path\":\"x\"}".into(),
            result: "ok".into(),
            ok: true,
            done: true,
        })];
        Session::new(s.clone())
            .save_to_path(&path)
            .expect("save_to_path writes the file and creates parent dirs");
        assert!(path.exists(), "the explicit-path file was written");

        let loaded = Session::load_path(&path).expect("round-trips back");
        assert_eq!(loaded.state.name, "Sub Agent");
        assert_eq!(loaded.state.cwd, "/tmp/proj");
        // Model context — what a relaunch resumes from — survives verbatim.
        assert_eq!(loaded.state.messages.len(), 1);
        assert_eq!(loaded.state.messages[0].role, MessageRole::User);
        // The transcript is NOT in the snapshot; no sibling jsonl exists here, so
        // it loads empty. (Its own round-trip is pinned by the transcript_log
        // tests and `roundtrip_audit::transcript_rebuilds_from_the_sibling_jsonl`.)
        assert!(
            loaded.state.transcript.is_empty(),
            "the transcript is the sibling jsonl's, not the snapshot's"
        );
    }

    // ── sanitize_name ─────────────────────────────────────────────────────────

    #[test]
    fn sanitize_name_lowercases_and_slugifies() {
        assert_eq!(sanitize_name("Hello World"), "hello-world");
        assert_eq!(sanitize_name("  UPPER  "), "upper");
        assert_eq!(sanitize_name("foo/bar.baz"), "foo-bar-baz");
    }

    #[test]
    fn sanitize_name_fallback_on_empty() {
        assert_eq!(sanitize_name(""), "session");
        assert_eq!(sanitize_name("!!!"), "session");
    }

    #[test]
    fn sanitize_name_caps_at_48_chars() {
        let long = "a".repeat(100);
        assert!(
            sanitize_name(&long).len() <= 48,
            "sanitized name must be ≤48 chars"
        );
    }

    // ── save / load round-trip ────────────────────────────────────────────────

    /// A state written to disk loads back identically — transcript entries with
    /// their timestamps, usage counters, todos and all. This is the whole point
    /// of the design: save is a serialize, load is an assignment.
    #[test]
    fn a_state_round_trips_through_a_file() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state("Round Trip", &cwd);
            // Entry times persist as whole unix seconds; use one so the
            // comparison below isn't defeated by dropped sub-second precision.
            let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
            st.transcript = vec![
                Entry::at(EntryKind::User("hi".into()), t),
                Entry::at(
                    EntryKind::Reasoning {
                        text: "hmm".into(),
                        took_ms: Some(900),
                    },
                    t,
                ),
                Entry::at(EntryKind::Assistant("hello".into()), t),
                Entry::at(EntryKind::Stats("✓ 1 tok".into()), t),
            ];
            st.usage = SessionUsage {
                tokens_in: 10,
                tokens_out: 5,
                cost_usd: 0.25,
                cost_partial: false,
                last_prompt_tokens: Some(10),
                last_completion_tokens: Some(5),
                context_window: Some(1000),
                ..Default::default()
            };
            Session::new(st.clone()).save("round-trip").unwrap();

            let back = Session::load(&cwd, "round-trip").unwrap().state;
            // The transcript lives in the sibling jsonl, not the `.json`; `save`
            // wrote no jsonl here, so it comes back empty. Its own round-trip is
            // pinned by `roundtrip_audit::transcript_rebuilds_from_the_sibling_jsonl`.
            assert!(
                back.transcript.is_empty(),
                "transcript is not embedded in the .json"
            );
            assert_eq!(back.usage, st.usage, "token counters");
            assert_eq!(back.messages.len(), 1);
            assert_eq!(
                back.id.as_deref(),
                Some("round-trip"),
                "id from the filename"
            );
        });
    }

    /// `Session::save` preserves the original `created` across repeated
    /// autosaves via the in-process cache (`created_cache`) rather than by
    /// re-reading the previous file on every write. Proven by deleting the
    /// file between saves: a save that fell back to reading it would find
    /// nothing there and mint a fresh timestamp instead of the original one —
    /// the cache must supply it regardless.
    #[test]
    fn save_preserves_created_via_cache_without_rereading_the_file() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let st = state("Cache Created", &cwd);
            let original = Session {
                version: SESSION_VERSION,
                created: 1_700_000_000,
                updated: 1_700_000_000,
                state: st.clone(),
            };
            let path = original.save("cache-created").unwrap();
            assert_eq!(
                Session::load_path(&path).unwrap().created,
                1_700_000_000,
                "first save wrote the given created time"
            );

            // Remove the file: a save that fell back to re-reading the
            // previous file to preserve `created` would find nothing and
            // mint a fresh timestamp instead.
            std::fs::remove_file(&path).unwrap();

            // A brand-new `Session` (as every autosave builds via
            // `Session::new`) for the same state/id — its own `created` is
            // whatever `Session::new` mints; `save` must still recover the
            // ORIGINAL time from the cache, not the fresh one.
            let fresh = Session::new(st);
            assert_ne!(
                fresh.created, 1_700_000_000,
                "sanity: Session::new did not coincidentally mint the same time"
            );
            fresh.save("cache-created").unwrap();

            let back = Session::load_path(&path).unwrap();
            assert_eq!(
                back.created, 1_700_000_000,
                "the cached creation time survived even with the file gone"
            );
        });
    }

    /// A restored entry gets its render hash rebuilt. It is derived state, so it is
    /// not persisted — and a zeroed one makes the renderer's cache key
    /// `(index, content_hash, …)` degenerate: the content half becomes a constant
    /// across the whole restored transcript, leaving only the index to tell two
    /// entries apart. Any future path that shifts an index without clearing the cache
    /// would then render one restored message as another.
    #[test]
    fn restoring_rebuilds_the_render_hashes() {
        let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
        let raw = SessionState {
            transcript: vec![
                // As they arrive from serde: content present, hash zeroed.
                Entry {
                    kind: EntryKind::User("first".into()),
                    time: t,
                    content_hash: 0,
                },
                Entry {
                    kind: EntryKind::User("second".into()),
                    time: t,
                    content_hash: 0,
                },
            ],
            ..Default::default()
        };
        let st = raw.restored();
        assert!(
            st.transcript.iter().all(|e| e.content_hash != 0),
            "every restored entry carries its own hash again"
        );
        assert_ne!(
            st.transcript[0].content_hash, st.transcript[1].content_hash,
            "and different content still hashes differently — which is the whole \
             point of the cache key"
        );
        assert_eq!(
            st.transcript[0].content_hash,
            Entry::at(EntryKind::User("first".into()), t).content_hash,
            "a restored entry hashes exactly like a freshly built one"
        );
    }

    /// A tool call left running at save time is settled on restore — nothing can
    /// finish it now, and a running block would spin its spinner forever.
    #[test]
    fn restoring_settles_an_unfinished_tool_call() {
        let running = Entry::tool_running("c1", "bash", "{}");
        let st = SessionState {
            transcript: vec![running],
            ..Default::default()
        }
        .restored();
        let EntryKind::Tool { ok, done, .. } = &st.transcript[0].kind else {
            panic!("not a tool");
        };
        assert!(*done && !*ok, "settled as finished-and-failed");
    }

    /// Only conversations with a user message are worth saving.
    #[test]
    fn is_saveable_requires_a_user_message() {
        assert!(!SessionState::default().is_saveable());
        let assistant_only = SessionState {
            messages: vec![Message::assistant("hi")],
            ..Default::default()
        };
        assert!(!assistant_only.is_saveable());
        assert!(state("x", "/tmp/x").is_saveable());
    }

    /// `sync_from` pulls in the agent-owned mirrors and derives a name when the
    /// session hasn't got one yet.
    #[test]
    fn sync_from_refreshes_the_agent_owned_mirrors() {
        let mut st = SessionState::default();
        st.sync_from(vec![Message::user("first line")], vec![], "/tmp/p".into());
        assert_eq!(st.cwd, "/tmp/p");
        assert_eq!(st.messages.len(), 1);
        assert_eq!(
            st.name, "first line",
            "name derived from the first user message"
        );

        // An existing name is never overwritten (a `/rename` must stick).
        st.name = "Kept".into();
        st.sync_from(vec![Message::user("other")], vec![], "/tmp/p".into());
        assert_eq!(st.name, "Kept");

        // No conversation-derived name → the default is the cwd's basename.
        st.name = String::new();
        st.sync_from(vec![], vec![], "/tmp/my-proj".into());
        assert_eq!(st.name, "my-proj", "default name is the cwd basename");
    }

    // ── unique_session_id ─────────────────────────────────────────────────────

    #[test]
    fn unique_session_id_returns_plain_slug_when_dir_absent() {
        let (id, _res) = unique_session_id("/nonexistent/hrdr/test/path/12345", "my session");
        assert_eq!(id, "my-session");
    }

    #[test]
    fn unique_session_id_appends_suffix_on_collision() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let (id, _res) = unique_session_id(&cwd, "chat");
            assert_eq!(id, "chat");
            Session::new(state("chat", &cwd)).save("chat").unwrap();
            let (id, _res) = unique_session_id(&cwd, "chat");
            assert_eq!(id, "chat-2");
        });
    }

    /// The lock file created by `unique_session_id` is cleaned up after a
    /// successful `Session::save`.
    #[test]
    fn unique_session_id_lock_is_cleaned_up_after_save() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let dir = session_dir(&cwd);
            let _ = std::fs::create_dir_all(&dir);

            let (id, _res) = unique_session_id(&cwd, "cleanup test");
            assert_eq!(id, "cleanup-test");
            let lock = dir.join(".cleanup-test.lock");
            assert!(lock.exists(), "lock file exists after reservation");

            // Save removes the lock.
            Session::new(state("cleanup", &cwd)).save(&id).unwrap();
            assert!(!lock.exists(), "lock file removed after save");
        });
    }

    /// `mint_session` claims the id, the open-lock and the reservation WITHOUT
    /// writing a file — the synchronous half of `save_session` a frontend needs
    /// on the UI thread before the two-fsync write can go off-thread.
    #[test]
    fn mint_session_claims_without_writing() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let st = state("chat", &cwd);
            let Some(outcome) = mint_session(&st).unwrap() else {
                panic!("a saveable state mints");
            };
            assert_eq!(outcome.id, "chat");
            assert!(outcome.first_save, "the first mint reports first_save");
            assert!(
                outcome.open_lock.is_some(),
                "the open-lock is taken at mint time"
            );
            assert!(
                outcome.reservation.is_some(),
                "the id is reserved at mint time"
            );
            let dir = session_dir(&cwd);
            let lock = dir.join(".chat.lock");
            assert!(lock.exists(), "reservation lock written at mint");
            assert!(
                !dir.join("chat.json").exists(),
                "mint writes no session file"
            );

            // Holding the reservation blocks a second instance minting the same
            // id…
            let (second, _res2) = unique_session_id(&cwd, "chat");
            assert_eq!(second, "chat-2");
            // …and the write, once it lands, removes the reservation lock.
            Session::new(st.persisted()).save(&outcome.id).unwrap();
            assert!(!lock.exists(), "save removed the reservation lock");
            // Dropping the outcome (with its reservation) is then benign.
            drop(outcome);
            assert!(!lock.exists(), "no lock to clean after a successful save");
        });
    }

    /// `mint_session` on a state that already has an id is a no-op write-wise:
    /// the id comes back as-is, nothing is re-reserved.
    #[test]
    fn mint_session_on_an_ided_state_is_not_a_first_save() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state("chat", &cwd);
            st.id = Some("chat".to_string());
            let Some(outcome) = mint_session(&st).unwrap() else {
                panic!("a saveable state mints");
            };
            assert_eq!(outcome.id, "chat");
            assert!(!outcome.first_save);
            assert!(outcome.open_lock.is_none(), "nothing new to lock");
            assert!(outcome.reservation.is_none(), "nothing new to reserve");
            let dir = session_dir(&cwd);
            assert!(
                !dir.join(".chat.lock").exists(),
                "no reservation lock created for an already-ided state"
            );
        });
    }

    /// Two reservations without an intervening save produce different ids
    /// (the second process would get the next suffix).
    #[test]
    fn unique_session_id_two_reservations_get_different_ids() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let (first, _res1) = unique_session_id(&cwd, "multi");
            let (second, _res2) = unique_session_id(&cwd, "multi");
            assert_eq!(first, "multi");
            assert_eq!(second, "multi-2", "second reservation gets next suffix");
        });
    }

    // ── file size limit ────────────────────────────────────────────────────────

    /// A session file exceeding `MAX_SESSION_FILE_BYTES` is rejected with a
    /// clear error message mentioning the limit.
    #[test]
    fn load_path_rejects_oversized_file() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            Session::new(state("big", &cwd)).save("big").unwrap();
            let path = session_dir(&cwd).join("big.json");

            // Stretch the file past the limit by appending junk.
            let padding = " ".repeat(MAX_SESSION_FILE_BYTES as usize + 1);
            std::fs::write(&path, padding).unwrap();

            let err = Session::load_path(&path).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("100.0 MiB"),
                "error mentions size limit: {msg}"
            );
            assert!(
                msg.contains(&path.display().to_string()),
                "error names the file: {msg}"
            );
        });
    }

    /// A normal-sized session file loads without error (the limit is generous
    /// and only blocks pathologically large files).
    #[test]
    fn load_path_accepts_small_file() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            Session::new(state("small", &cwd)).save("small").unwrap();
            let loaded = Session::load(&cwd, "small").unwrap();
            assert_eq!(loaded.state.name, "small");
        });
    }

    // ── retention: compress / purge sweep ─────────────────────────────────────

    /// Backdate a file's mtime by `secs_old` seconds so the sweep sees it as idle.
    fn age_file(path: &Path, secs_old: u64) {
        let ts = hrdr_tools::unix_now().saturating_sub(secs_old) as i64;
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(ts, 0)).unwrap();
    }

    #[test]
    fn compress_then_load_round_trips_and_preserves_mtime() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state("Chat", &cwd);
            st.named_by_user = true;
            Session::new(st).save("rt").unwrap();
            let json = session_file_path(&cwd, "rt");
            age_file(&json, 100_000);
            let before = std::fs::metadata(&json).unwrap().modified().unwrap();

            compress_session_file(&json).unwrap();

            let zst = json.with_extension("json.zst");
            assert!(!json.exists(), "plaintext removed");
            assert!(zst.exists(), "compressed written");
            let after = std::fs::metadata(&zst).unwrap().modified().unwrap();
            assert_eq!(
                before
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                after
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                "compression preserves mtime (the purge clock)"
            );
            // Decodes transparently; id is derived without the `.json.zst` tail.
            let loaded = Session::load_path(&zst).unwrap();
            assert_eq!(loaded.state.name, "Chat");
            assert!(loaded.state.named_by_user);
            assert_eq!(loaded.state.id.as_deref(), Some("rt"));
        });
    }

    #[test]
    fn sweep_compresses_idle_and_purges_old_auto_named() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let week = 7 * 24 * 60 * 60;
            let month = 30 * 24 * 60 * 60;

            // (a) auto-named, older than a month → purged.
            Session::new(state("old-auto", &cwd))
                .save("old-auto")
                .unwrap();
            age_file(&session_file_path(&cwd, "old-auto"), month + 1000);
            // (b) user-named, older than a month → kept, but compressed.
            let mut named = state("kept", &cwd);
            named.named_by_user = true;
            Session::new(named).save("old-named").unwrap();
            age_file(&session_file_path(&cwd, "old-named"), month + 1000);
            // (c) auto-named, older than a week but not a month → compressed only.
            Session::new(state("weekish", &cwd))
                .save("weekish")
                .unwrap();
            age_file(&session_file_path(&cwd, "weekish"), week + 1000);
            // (d) fresh → untouched.
            Session::new(state("fresh", &cwd)).save("fresh").unwrap();

            sweep_sessions(Some(week), Some(month));

            let p = |id: &str| session_file_path(&cwd, id);
            let z = |id: &str| p(id).with_extension("json.zst");
            assert!(
                !p("old-auto").exists() && !z("old-auto").exists(),
                "old auto-named session purged"
            );
            assert!(
                !p("old-named").exists() && z("old-named").exists(),
                "old user-named session kept but compressed"
            );
            assert!(
                !p("weekish").exists() && z("weekish").exists(),
                "week-old session compressed, not purged"
            );
            assert!(
                p("fresh").exists() && !z("fresh").exists(),
                "fresh session untouched"
            );
            assert_eq!(
                Session::load_path(&z("old-named")).unwrap().state.name,
                "kept"
            );
        });
    }

    #[test]
    fn save_drops_a_stale_compressed_copy() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state("Chat", &cwd);
            st.named_by_user = true;
            Session::new(st.clone()).save("resume").unwrap();
            let json = session_file_path(&cwd, "resume");
            compress_session_file(&json).unwrap();
            assert!(json.with_extension("json.zst").exists());
            // Resuming then autosaving rewrites plaintext and drops the stale `.zst`.
            Session::new(st).save("resume").unwrap();
            assert!(json.exists(), "plaintext rewritten");
            assert!(
                !json.with_extension("json.zst").exists(),
                "stale compressed copy dropped"
            );
        });
    }

    #[test]
    fn open_path_locks_a_compressed_session_under_its_real_id() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state("Chat", &cwd);
            st.named_by_user = true;
            Session::new(st).save("resume").unwrap();
            let json = session_file_path(&cwd, "resume");
            compress_session_file(&json).unwrap();
            let zst = json.with_extension("json.zst");
            let dir = session_dir(&cwd);
            let (_, lock) = Session::open_path(&zst).unwrap();
            // The lock is keyed on the real id — the name every other actor uses —
            // not the file_stem of the compressed path.
            assert!(dir.join(".resume.open.lock").exists());
            assert!(!dir.join(".resume-json.open.lock").exists());
            // A second open of the same session now contends on the same name.
            assert!(matches!(
                Session::open_path(&zst),
                Err(OpenError::Busy { .. })
            ));
            drop(lock);
        });
    }

    #[test]
    fn sweep_skips_a_session_held_by_a_live_lock() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let month = 30 * 24 * 60 * 60;
            Session::new(state("busy", &cwd)).save("busy").unwrap();
            let json = session_file_path(&cwd, "busy");
            age_file(&json, month + 1000);
            // A live instance holds the open-lock; the sweep must not touch it.
            let _lock = acquire_open_lock(&session_dir(&cwd), "busy").unwrap();
            sweep_sessions(Some(month / 4), Some(month));
            assert!(json.exists(), "a busy session is skipped");
            assert!(!json.with_extension("json.zst").exists());
        });
    }

    // ── list_sessions ─────────────────────────────────────────────────────────

    /// `list_sessions` (via `collect_sessions`) caches parsed metadata per file
    /// path, keyed by mtime — it backs `arg_completions` for `/resume`, which
    /// runs on every keystroke, so re-parsing every session file's full
    /// message history each time would be a per-key freeze. A listing right
    /// after a rename (a fresh write, so the mtime moves) must still show the
    /// new name, not a stale cached one from before the write.
    #[test]
    fn list_sessions_reflects_a_rename_despite_the_mtime_cache() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            Session::new(state("Before Rename", &cwd))
                .save("renamed")
                .unwrap();

            let first = list_sessions();
            assert!(
                first.iter().any(|m| m.name == "Before Rename"),
                "first listing sees the session: {first:?}"
            );

            // Overwrite the same file with a different name: a fresh write,
            // so its mtime moves and the cached entry must not be reused.
            Session::new(state("After Rename", &cwd))
                .save("renamed")
                .unwrap();

            let second = list_sessions();
            assert!(
                second.iter().any(|m| m.name == "After Rename"),
                "second listing sees the rename, not a stale cached entry: {second:?}"
            );
            assert!(
                !second.iter().any(|m| m.name == "Before Rename"),
                "the stale name is gone: {second:?}"
            );
        });
    }

    // ── resolve_session ───────────────────────────────────────────────────────

    #[test]
    fn resolve_session_returns_none_for_unknown_id() {
        assert!(resolve_session("/nonexistent/path/xyz", "no-such-session").is_none());
    }

    #[test]
    fn resolve_session_exact_id_in_current_cwd() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("project");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            Session::new(state("My Chat", &cwd))
                .save("my-chat")
                .unwrap();
            let (id, s) = resolve_session(&cwd, "my-chat").unwrap();
            assert_eq!(id, "my-chat");
            assert_eq!(s.state.name, "My Chat");
            assert_eq!(s.state.cwd, cwd);
        });
    }

    #[test]
    fn resolve_session_case_insensitive_display_name_match() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("proj");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            Session::new(state("Work Session", &cwd))
                .save("work")
                .unwrap();
            let (id, s) = resolve_session(&cwd, "WORK SESSION").expect("case-insensitive match");
            assert_eq!(id, "work");
            assert_eq!(s.state.name, "Work Session");
        });
    }

    #[test]
    fn resolve_session_current_cwd_preferred_over_other_cwd() {
        with_test_env(|tmp| {
            let cwd_a = tmp.path().join("a");
            let cwd_b = tmp.path().join("b");
            std::fs::create_dir(&cwd_a).unwrap();
            std::fs::create_dir(&cwd_b).unwrap();
            let a = cwd_a.to_str().unwrap().to_string();
            let b = cwd_b.to_str().unwrap().to_string();

            Session::new(state("Alpha A", &a)).save("alpha").unwrap();
            Session::new(state("Alpha B", &b)).save("alpha").unwrap();

            let (_, s) = resolve_session(&a, "alpha").unwrap();
            assert_eq!(
                s.state.name, "Alpha A",
                "current-cwd exact match takes precedence"
            );
            let (_, s) = resolve_session(&b, "alpha").unwrap();
            assert_eq!(s.state.name, "Alpha B");
        });
    }

    // ── stale lock / reservation cleanup / concurrency ──────────────────────────

    /// A stale lock file (old timestamp, dead PID) is reaped by
    /// `unique_session_id` so the candidate can be claimed.
    #[test]
    fn stale_lock_is_reaped() {
        // Under the env lock: `session_dir` is evaluated twice (here and inside
        // `unique_session_id`) and resolves through `XDG_DATA_HOME`, which a
        // parallel `with_test_env` test mutates.
        with_test_env(|tmp| {
            let dir = tmp.path().join("p");
            std::fs::create_dir(&dir).unwrap();
            let cwd = dir.to_str().unwrap().to_string();

            let sdir = session_dir(&cwd);
            let _ = std::fs::create_dir_all(&sdir);
            let lock_path = sdir.join(".stale-cand.lock");

            // Create a lock with an old timestamp and a PID that (almost
            // certainly) does not exist.
            let old_ts = hrdr_tools::unix_now().saturating_sub(STALE_LOCK_AGE_SECS + 60);
            std::fs::write(&lock_path, format!("4294967294 {old_ts}")).unwrap();

            let (id, _res) = unique_session_id(&cwd, "stale-cand");
            assert_eq!(id, "stale-cand", "stale lock was reaped");
        });
    }

    /// The [`Reservation`] guard releases the lock on drop, so a failed
    /// save never leaves a permanent lock behind.
    #[test]
    fn reservation_cleans_up_lock_on_drop() {
        // `session_dir` resolves through `XDG_DATA_HOME`; run under the env
        // lock so a parallel `with_test_env` test can't flip the base dir
        // between the two `session_dir` evaluations here.
        with_test_env(|tmp| {
            let dir = tmp.path().join("p");
            std::fs::create_dir(&dir).unwrap();
            let cwd = dir.to_str().unwrap().to_string();
            let sdir = session_dir(&cwd);
            let _ = std::fs::create_dir_all(&sdir);
            let lock_path = sdir.join(".reservation-drop.lock");

            {
                let (id, _res) = unique_session_id(&cwd, "reservation-drop");
                assert_eq!(id, "reservation-drop");
                assert!(lock_path.exists(), "lock exists during reservation");
                // _res dropped here
            }
            assert!(!lock_path.exists(), "lock removed after Reservation::drop");
        });
    }

    /// Two concurrent reservations for the same name get different ids.
    #[test]
    fn concurrent_unique_session_ids_are_different() {
        // Under the env lock: both threads must resolve the SAME sessions base
        // dir, and a parallel `with_test_env` test mutating `XDG_DATA_HOME`
        // could otherwise flip it under one of them (giving both the plain id).
        with_test_env(|tmp| {
            let dir = tmp.path().join("con");
            std::fs::create_dir(&dir).unwrap();
            let cwd = std::sync::Arc::new(dir.to_str().unwrap().to_string());

            let cwd1 = cwd.clone();
            let cwd2 = cwd.clone();
            let t1 = std::thread::spawn(move || {
                let sdir = session_dir(&cwd1);
                let _ = std::fs::create_dir_all(&sdir);
                unique_session_id(&cwd1, "concurrent")
            });
            let t2 = std::thread::spawn(move || {
                let sdir = session_dir(&cwd2);
                let _ = std::fs::create_dir_all(&sdir);
                unique_session_id(&cwd2, "concurrent")
            });

            let (id1, _r1) = t1.join().unwrap();
            let (id2, _r2) = t2.join().unwrap();
            assert_ne!(id1, id2, "concurrent reservations get different ids");
        });
    }

    // ── open lock (per-session ownership) ──────────────────────────────────────

    /// A fresh acquire writes `PID TIMESTAMP` into `.{id}.open.lock`, and the
    /// guard's `Drop` removes the file.
    #[test]
    fn open_lock_acquire_writes_owner_and_drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".chat.open.lock");
        {
            let _lock = acquire_open_lock(dir.path(), "chat").expect("fresh id acquires");
            assert!(path.exists(), "lock file exists while the guard is held");
            let content = std::fs::read_to_string(&path).unwrap();
            let mut parts = content.split_whitespace();
            let pid: u32 = parts.next().unwrap().parse().unwrap();
            let ts: u64 = parts.next().unwrap().parse().unwrap();
            assert_eq!(pid, std::process::id(), "records this process's PID");
            assert!(ts > 0, "records a timestamp");
            // _lock dropped here
        }
        assert!(!path.exists(), "Drop removes the lock file");
    }

    /// A second acquire while the first guard is alive is refused with the live
    /// owner's PID.
    #[test]
    fn open_lock_second_acquire_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire_open_lock(dir.path(), "chat").expect("first acquire");
        let err = acquire_open_lock(dir.path(), "chat").expect_err("second is refused");
        assert_eq!(
            err.pid,
            std::process::id(),
            "SessionBusy names the live owner"
        );
    }

    /// Dead-PID reclaim: a lock left by a process that no longer exists (old
    /// timestamp, never-alive PID) is reaped so the acquire succeeds. This is the
    /// crash auto-cleanup — remove the `is_stale_lock` reap branch in
    /// `acquire_open_lock` and this test fails with `SessionBusy` instead.
    #[test]
    fn open_lock_dead_pid_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".chat.open.lock");
        let old_ts = hrdr_tools::unix_now().saturating_sub(STALE_LOCK_AGE_SECS + 60);
        // 4294967294 (u32::MAX - 1) is almost certainly not a live PID.
        std::fs::write(&path, format!("4294967294 {old_ts}")).unwrap();
        let lock = acquire_open_lock(dir.path(), "chat").expect("stale lock is reclaimed");
        assert!(path.exists(), "the reclaimed lock is now ours");
        drop(lock);
    }

    /// A live, recent lock (this process's PID, current timestamp) is NOT
    /// reclaimed — it is respected as busy.
    #[test]
    fn open_lock_live_recent_is_not_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".chat.open.lock");
        std::fs::write(
            &path,
            format!("{} {}", std::process::id(), hrdr_tools::unix_now()),
        )
        .unwrap();
        let err = acquire_open_lock(dir.path(), "chat").expect_err("live lock is not stolen");
        assert_eq!(err.pid, std::process::id());
    }

    /// `Session::open_path` refuses (without loading) when the open-lock is held,
    /// and returns the session + guard when it is free.
    #[test]
    fn open_path_refuses_when_busy_and_opens_when_free() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            Session::new(state("My Chat", &cwd))
                .save("my-chat")
                .unwrap();
            let path = session_file_path(&cwd, "my-chat");

            // Hold the open-lock, so open_path must refuse.
            let held = acquire_open_lock(&session_dir(&cwd), "my-chat").unwrap();
            match Session::open_path(&path) {
                Err(OpenError::Busy { pid, .. }) => assert_eq!(pid, std::process::id()),
                other => panic!("expected Busy, got {other:?}"),
            }
            drop(held);

            // Free now: open_path loads and hands back the guard.
            let (session, _lock) = Session::open_path(&path).expect("opens when free");
            assert_eq!(session.state.name, "My Chat");
        });
    }

    /// The open-lock file name is distinct from the `.{id}.lock` reservation, and
    /// `Session::save` (which removes the reservation) leaves the open-lock alone.
    #[test]
    fn open_lock_is_distinct_from_reservation_and_survives_save() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            // A first save creates the session directory (and the file).
            Session::new(state("My Chat", &cwd))
                .save("my-chat")
                .unwrap();
            let sdir = session_dir(&cwd);

            let _held = acquire_open_lock(&sdir, "my-chat").unwrap();
            let open_lock = sdir.join(".my-chat.open.lock");
            let reservation = sdir.join(".my-chat.lock");
            assert_ne!(open_lock, reservation, "distinct file names");
            assert!(open_lock.exists(), "open-lock file was created");

            // Simulate a stray reservation lock next to the open-lock.
            std::fs::write(&reservation, format!("{} 0", std::process::id())).unwrap();

            // A save deletes only the reservation; the open-lock must survive.
            Session::new(state("My Chat", &cwd))
                .save("my-chat")
                .unwrap();
            assert!(
                open_lock.exists(),
                "save must not remove the live open-lock"
            );
            assert!(!reservation.exists(), "save removes the reservation lock");
        });
    }

    // ── fork ──────────────────────────────────────────────────────────────────

    /// Forking copies the source's on-disk snapshot into a NEW, independently
    /// locked session: a distinct id, a `" (fork)"` name, the content copied —
    /// while the SOURCE file and its held open-lock are left untouched.
    #[test]
    fn fork_copies_content_into_a_new_locked_session_leaving_the_source_alone() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            let sdir = session_dir(&cwd);

            // A source session with real content.
            let mut src = state("Orig", &cwd);
            src.messages = vec![Message::user("keep me"), Message::assistant("sure")];
            Session::new(src).save("orig").unwrap();
            let source_path = session_file_path(&cwd, "orig");
            // Give the source a real display transcript too, as a busy live
            // session would have: a sibling `orig.jsonl` the fork must carry.
            let mut log = crate::transcript_log::TranscriptLog::create(&sdir, "orig").unwrap();
            log.write(
                &crate::transcript_log::Record::from_event(&crate::AgentEvent::Text(
                    "the conversation so far".into(),
                ))
                .unwrap(),
            );
            log.flush();
            assert!(sdir.join("orig.jsonl").exists());

            // The other instance holds the source's open-lock.
            let _source_held = acquire_open_lock(&sdir, "orig").unwrap();
            let source_open_lock = sdir.join(".orig.open.lock");
            assert!(source_open_lock.exists());

            let (new_id, forked, fork_lock) = Session::fork(&cwd, &source_path).unwrap();

            // Distinct id, forked name, content copied.
            assert_ne!(new_id, "orig", "fork gets a fresh id");
            assert!(
                forked.state.name.ends_with(" (fork)"),
                "forked name ends with ' (fork)': {}",
                forked.state.name
            );
            assert_eq!(forked.state.name, "Orig (fork)");
            assert_eq!(
                forked.state.messages.len(),
                2,
                "conversation copied from the source"
            );
            // The fork's display transcript came along with the jsonl: the same
            // fold the source's pane renders, not an empty conversation.
            assert!(
                !forked.state.transcript.is_empty(),
                "fork carries the source's display transcript"
            );
            assert!(
                forked.state.transcript.iter().any(|e| matches!(
                    &e.kind,
                    EntryKind::Assistant(s) if s == "the conversation so far"
                )),
                "forked transcript folds the source's records: {:?}",
                forked.state.transcript
            );
            assert_eq!(forked.state.id.as_deref(), Some(new_id.as_str()));

            // The fork holds its OWN open-lock, and a fresh open of it is Busy.
            assert!(
                sdir.join(format!(".{new_id}.open.lock")).exists(),
                "fork's open-lock file was created"
            );
            let fork_path = session_file_path(&cwd, &new_id);
            match Session::open_path(&fork_path) {
                Err(OpenError::Busy { pid, .. }) => assert_eq!(pid, std::process::id()),
                other => panic!("fork lock is not real — expected Busy, got {other:?}"),
            }

            // The SOURCE file and its open-lock are untouched.
            assert!(source_open_lock.exists(), "source open-lock left intact");
            let reloaded = Session::load_path(&source_path).unwrap();
            assert_eq!(reloaded.state.name, "Orig", "source file not renamed");
            assert_eq!(reloaded.state.messages.len(), 2, "source content unchanged");

            drop(fork_lock);
            // Once the fork lock is released, the fork opens cleanly.
            Session::open_path(&fork_path).expect("fork opens once its lock is freed");
        });
    }

    /// Two forks of the same source get different ids (no collision) and each
    /// holds its own lock.
    #[test]
    fn forking_twice_yields_distinct_ids() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();
            Session::new(state("Orig", &cwd)).save("orig").unwrap();
            let source_path = session_file_path(&cwd, "orig");

            let (id1, _s1, _l1) = Session::fork(&cwd, &source_path).unwrap();
            let (id2, _s2, _l2) = Session::fork(&cwd, &source_path).unwrap();
            assert_ne!(id1, id2, "a second fork does not collide with the first");
        });
    }

    // ── corrupt session listing ───────────────────────────────────────────────

    /// A session file that cannot be parsed is still listed (with an error
    /// message) rather than silently skipped.
    #[test]
    fn corrupt_session_file_is_listed_with_error() {
        with_test_env(|tmp| {
            let cwd = tmp.path().join("p");
            std::fs::create_dir(&cwd).unwrap();
            let cwd = cwd.to_str().unwrap().to_string();

            // Write a valid session.
            Session::new(state("good", &cwd)).save("good").unwrap();

            // Write an unparseable file next to it.
            let dir = session_dir(&cwd);
            std::fs::write(dir.join("corrupt.json"), "not valid json").unwrap();

            let all = list_sessions();
            assert_eq!(all.len(), 2, "both valid and corrupt are listed");
            let good = all.iter().find(|m| m.id == "good").unwrap();
            assert!(good.error.is_none(), "valid session has no error");
            let bad = all.iter().find(|m| m.id == "corrupt").unwrap();
            assert!(
                bad.error.is_some(),
                "corrupt session carries an error message"
            );
            assert!(bad.name.is_empty(), "corrupt entry has no name");
            assert!(bad.cwd.is_empty(), "corrupt entry has no cwd");
        });
    }

    // ── attachments: blobs beside the session ─────────────────────────────────

    use hrdr_llm::media::{Attachment, MediaType};

    /// A PNG whose bytes are decided by `tag`, so two calls with different tags
    /// are genuinely different content (and different digests).
    fn png_bytes(tag: u8, pad: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend(std::iter::repeat_n(tag, pad));
        v
    }

    fn png(tag: u8, name: &str) -> Attachment {
        Attachment::new(png_bytes(tag, 64), MediaType::Png, name).expect("a valid png")
    }

    /// The state `state()` builds, with `attachments` on its one user message.
    fn state_with(name: &str, cwd: &str, attachments: Vec<Attachment>) -> SessionState {
        let mut st = state(name, cwd);
        st.messages[0].attachments = attachments;
        st
    }

    fn blobs_in(cwd: &str) -> Vec<String> {
        let dir = crate::attachment_store::blob_dir_in(&session_dir(cwd));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    /// How every dialect renders one message's attachments — the thing that
    /// actually goes on the wire, and so the thing a resume has to reproduce.
    /// Asserting on this rather than on the struct is the point: a resumed
    /// attachment that is "equal" but renders differently is still a regression.
    fn rendered(m: &Message) -> Vec<serde_json::Value> {
        m.attachments
            .iter()
            .map(|a| {
                serde_json::json!({
                    "anthropic": a.anthropic_block(),
                    "responses": a.responses_item(),
                    "openai": a.openai_part(),
                })
            })
            .collect()
    }

    /// The whole point of the slice: bytes attached before a save are the same
    /// bytes, rendering the same blocks, after a resume.
    #[test]
    fn attachments_survive_a_save_and_resume() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let pdf =
                Attachment::new(b"%PDF-1.7 body".to_vec(), MediaType::Pdf, "report.pdf").unwrap();
            let st = state_with("Chat", &cwd, vec![png(1, "shot.png"), pdf]);
            let before = rendered(&st.messages[0]);
            let raw: Vec<Vec<u8>> = st.messages[0]
                .attachments
                .iter()
                .map(|a| a.bytes().to_vec())
                .collect();

            Session::new(st).save("att").unwrap();
            let back = Session::load(&cwd, "att").unwrap();

            let m = &back.state.messages[0];
            assert!(back.state.attachment_losses.is_empty(), "nothing was lost");
            assert_eq!(m.attachments.len(), 2);
            assert_eq!(
                m.attachments
                    .iter()
                    .map(|a| a.bytes().to_vec())
                    .collect::<Vec<_>>(),
                raw,
                "the bytes come back identical"
            );
            assert_eq!(
                m.attachments
                    .iter()
                    .map(|a| a.media_type())
                    .collect::<Vec<_>>(),
                vec![MediaType::Png, MediaType::Pdf],
            );
            assert_eq!(
                m.attachments
                    .iter()
                    .map(|a| a.filename())
                    .collect::<Vec<_>>(),
                vec!["shot.png", "report.pdf"],
            );
            assert_eq!(rendered(m), before, "every dialect renders it as before");
            // And the text is untouched — nothing was lost, so nothing is annotated.
            assert_eq!(m.content.as_deref(), Some("hi"));
            assert_eq!(blobs_in(&cwd).len(), 2, "one blob per distinct attachment");
        });
    }

    /// Content addressing: the same image on two messages is one file, and both
    /// messages get it back.
    #[test]
    fn one_image_on_two_messages_is_stored_once() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state_with("Chat", &cwd, vec![png(7, "first.png")]);
            // A second user turn attaching byte-identical content under another name.
            let mut second = Message::user("again");
            second.attachments = vec![png(7, "second.png")];
            st.messages.push(second);

            Session::new(st).save("dup").unwrap();
            assert_eq!(blobs_in(&cwd).len(), 1, "identical bytes → one blob");

            let back = Session::load(&cwd, "dup").unwrap();
            assert!(back.state.attachment_losses.is_empty());
            let names: Vec<&str> = back
                .state
                .messages
                .iter()
                .flat_map(|m| m.attachments.iter().map(|a| a.filename()))
                .collect();
            assert_eq!(
                names,
                vec!["first.png", "second.png"],
                "both messages resolve the shared blob, each keeping its own name"
            );
            for m in &back.state.messages {
                assert_eq!(m.attachments[0].bytes(), &png_bytes(7, 64)[..]);
            }
        });
    }

    /// The regression that matters: a conversation with no attachments writes
    /// exactly the file it wrote before the blob store existed — no extra key,
    /// and no `blobs/` directory.
    #[test]
    fn a_session_without_attachments_writes_the_same_file() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let path = Session::new(state("Chat", &cwd)).save("plain").unwrap();

            let json = std::fs::read_to_string(&path).unwrap();
            assert!(
                !json.contains("attachments"),
                "no attachment key is written: {json}"
            );
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec![
                    "base_url",
                    "created",
                    "cwd",
                    "messages",
                    "model",
                    "name",
                    "named_by_user",
                    "read_only",
                    "todos",
                    "updated",
                    "usage",
                    "version",
                ],
                "the on-disk shape is unchanged"
            );
            assert!(
                !crate::attachment_store::blob_dir(&path).exists(),
                "no blob directory is created for a session with nothing to store"
            );
        });
    }

    /// A blob deleted behind the session's back. The attachment is dropped — and
    /// BOTH readers are told: the user, through the returned loss list (the TUI
    /// prints one line per entry), and the model, through the message text, which
    /// still carries the "--- Attached files ---" block naming the file.
    #[test]
    fn a_missing_blob_is_reported_not_swallowed() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let mut st = state_with("Chat", &cwd, vec![png(3, "gone.png")]);
            st.messages[0].content =
                Some("what is this\n\n--- Attached files ---\nImage 1: gone.png\n".into());
            let path = Session::new(st).save("lost").unwrap();

            // Delete the one blob, by name, exactly as a `rm` on the store would.
            let blobs = blobs_in(&cwd);
            assert_eq!(blobs.len(), 1);
            std::fs::remove_file(crate::attachment_store::blob_dir(&path).join(&blobs[0])).unwrap();

            let back = Session::load(&cwd, "lost").unwrap();
            let m = &back.state.messages[0];
            assert!(
                m.attachments.is_empty(),
                "the attachment is dropped, not resurrected empty"
            );
            assert_eq!(
                back.state.attachment_losses,
                vec![crate::AttachmentLoss {
                    filename: "gone.png".into(),
                    reason: crate::AttachmentLossReason::Missing,
                }],
                "the loss is surfaced for the user"
            );
            let text = m.content.as_deref().unwrap();
            assert!(
                text.contains("--- Attachments unavailable ---")
                    && text.contains("gone.png: its stored copy is missing"),
                "the message itself stops claiming an image it no longer has: {text}"
            );

            // Saving again records no reference for it, so a second resume has
            // nothing left to lose and does not annotate the text twice.
            Session::new(back.state.clone()).save("lost").unwrap();
            let again = Session::load(&cwd, "lost").unwrap();
            assert!(again.state.attachment_losses.is_empty());
            assert_eq!(again.state.messages[0].content.as_deref(), Some(text));
        });
    }

    /// A blob whose bytes were corrupted is never handed to a model as an image:
    /// the digest check catches it and it is reported like any other loss.
    #[test]
    fn a_corrupt_blob_is_never_rendered() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let path = Session::new(state_with("Chat", &cwd, vec![png(5, "bad.png")]))
                .save("corrupt")
                .unwrap();
            let name = blobs_in(&cwd).remove(0);
            // Still a valid PNG header, still the right length — only the checksum
            // can tell. Overwritten in place, as filesystem damage would.
            std::fs::write(
                crate::attachment_store::blob_dir(&path).join(&name),
                png_bytes(6, 64),
            )
            .unwrap();

            let back = Session::load(&cwd, "corrupt").unwrap();
            assert_eq!(
                back.state.attachment_losses,
                vec![crate::AttachmentLoss {
                    filename: "bad.png".into(),
                    reason: crate::AttachmentLossReason::Corrupt,
                }]
            );
            assert!(
                back.state.messages[0].attachments.is_empty(),
                "nothing to render, so nothing reaches a request body"
            );
            assert!(rendered(&back.state.messages[0]).is_empty());
        });
    }

    /// Retention's purge takes the purged session's blobs with it — but a blob
    /// the surviving session also references is not the purged session's to
    /// delete, and stays.
    #[test]
    fn purging_a_session_collects_only_its_own_blobs() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let month = 30 * 24 * 60 * 60;

            // Auto-named and old → purged. Holds a shared image and one of its own.
            let mut doomed = state_with("doomed", &cwd, vec![png(1, "shared.png")]);
            let mut only_mine = Message::user("mine");
            only_mine.attachments = vec![png(2, "only-mine.png")];
            doomed.messages.push(only_mine);
            Session::new(doomed).save("doomed").unwrap();

            // User-named → survives the purge (it is compressed instead), and
            // references the same image bytes as the doomed one.
            let mut keeper = state_with("keeper", &cwd, vec![png(1, "shared.png")]);
            keeper.named_by_user = true;
            Session::new(keeper).save("keeper").unwrap();

            let shared = crate::AttachmentRef::of(&png(1, "x.png")).sha256;
            let orphan = crate::AttachmentRef::of(&png(2, "x.png")).sha256;
            assert_eq!(blobs_in(&cwd), {
                let mut v = vec![shared.clone(), orphan.clone()];
                v.sort();
                v
            });

            // Age everything past the purge threshold — the session files so the
            // sweep acts, the blobs so they are outside the GC's grace window.
            age_file(&session_file_path(&cwd, "doomed"), month + 1000);
            age_file(&session_file_path(&cwd, "keeper"), month + 1000);
            let blob_dir = crate::attachment_store::blob_dir_in(&session_dir(&cwd));
            age_file(&blob_dir.join(&shared), month + 1000);
            age_file(&blob_dir.join(&orphan), month + 1000);

            sweep_sessions(Some(7 * 24 * 60 * 60), Some(month));

            assert!(
                !session_file_path(&cwd, "doomed").exists(),
                "the auto-named session was purged"
            );
            assert_eq!(
                blobs_in(&cwd),
                vec![shared.clone()],
                "the purged session's own blob is collected; the shared one is not"
            );
            // And the survivor still resolves it.
            let kept = Session::load(&cwd, "keeper").unwrap();
            assert!(kept.state.attachment_losses.is_empty());
            assert_eq!(
                kept.state.messages[0].attachments[0].bytes(),
                &png_bytes(1, 64)[..],
                "the shared blob still serves the session that survived"
            );
        });
    }

    // ── attachments: the blob store's cross-process lock ──────────────────────

    /// The GC is exclusive. While a peer holds the store lock it may be mid-save
    /// — writing a blob, not yet the session file that names it — so a mark set
    /// built now can miss a live digest. The collection is skipped whole.
    #[test]
    fn the_blob_gc_skips_a_locked_store() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let blobs = crate::attachment_store::blob_dir_in(dir);
        std::fs::create_dir_all(&blobs).unwrap();
        let orphan = "a".repeat(64);
        std::fs::write(blobs.join(&orphan), b"x").unwrap();
        // Far past the sweep's grace window, so the lock is the only thing that
        // can spare it — and a lock is what a live peer holds.
        age_file(&blobs.join(&orphan), 30 * 24 * 60 * 60);

        let held = crate::attachment_store::lock_blob_store(dir).unwrap();
        collect_blob_garbage(dir);
        assert!(
            blobs.join(&orphan).exists(),
            "no sweeping while the store is locked"
        );

        // …and the same call collects it once the store is free, so the test
        // above is passing on the lock and not on a sweep that never works.
        drop(held);
        collect_blob_garbage(dir);
        assert!(
            !blobs.join(&orphan).exists(),
            "an unlocked store is collected as before"
        );
    }

    /// A contended lock must degrade, never abort: losing the user's attachment
    /// (or their whole session file) would be far worse than the race the lock
    /// closes. The save waits out its acquire budget, gives up, and writes.
    #[test]
    fn a_save_with_attachments_waits_for_the_lock_then_writes_anyway() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let dir = session_dir(&cwd);
            std::fs::create_dir_all(&dir).unwrap();
            let _held = crate::attachment_store::lock_blob_store(&dir).unwrap();

            let started = std::time::Instant::now();
            Session::new(state_with("Chat", &cwd, vec![png(1, "shot.png")]))
                .save("att")
                .unwrap();
            let elapsed = started.elapsed();

            // A timing assertion, and deliberately a loose one: an acquire that
            // never succeeds sleeps `LOCK_ACQUIRE_ATTEMPTS` × `LOCK_RETRY_DELAY`
            // (≈5s in `store_lock`) before giving up, while a save that never
            // tried for the lock returns in milliseconds. Two seconds sits far
            // from both ends, so neither a loaded runner (which only ever makes
            // the contended case slower) nor a fast one can cross it.
            assert!(
                elapsed >= std::time::Duration::from_secs(2),
                "the save contended for the blob lock, taking {elapsed:?}"
            );
            assert!(
                session_file_path(&cwd, "att").exists(),
                "the session file still landed"
            );
            assert_eq!(
                blobs_in(&cwd),
                vec![crate::AttachmentRef::of(&png(1, "shot.png")).sha256],
                "and so did its blob"
            );
        });
    }

    /// The overwhelmingly common save — no attachments at all, once per tool
    /// round — contributes nothing to the GC's mark set and must not pay for the
    /// lock. Nothing in the filesystem can show a lock that was never taken, so
    /// the observable is time: this save cannot have waited on the held lock.
    #[test]
    fn a_save_without_attachments_does_not_take_the_lock() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let dir = session_dir(&cwd);
            std::fs::create_dir_all(&dir).unwrap();
            let _held = crate::attachment_store::lock_blob_store(&dir).unwrap();

            let started = std::time::Instant::now();
            Session::new(state("Chat", &cwd)).save("plain").unwrap();
            let elapsed = started.elapsed();

            // The same timing assertion from the other side (see the note there):
            // one second is a fifth of the ≈5s a contended acquire would burn, and
            // twenty times what writing a one-message session costs.
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "no lock was waited on, taking {elapsed:?}"
            );
            assert!(session_file_path(&cwd, "plain").exists());
        });
    }

    /// A sub-agent's snapshot goes through `save_to_path`, which owns a second
    /// copy of the guard — and locks the store beside the file it is writing, not
    /// the file itself. Same contract as `save`: it waits, then writes regardless.
    #[test]
    fn save_to_path_locks_the_blob_store_beside_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subagents").join("child");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let _held = crate::attachment_store::lock_blob_store(&dir).unwrap();

        let started = std::time::Instant::now();
        Session::new(state_with("Child", "/tmp", vec![png(3, "shot.png")]))
            .save_to_path(&path)
            .unwrap();
        let elapsed = started.elapsed();

        // Timing again, and for the same reason (see the note in
        // `a_save_with_attachments_waits_for_the_lock_then_writes_anyway`): it is
        // the only way to tell a lock that was taken from one that was not.
        assert!(
            elapsed >= std::time::Duration::from_secs(2),
            "it contended for the store beside the file, taking {elapsed:?}"
        );
        assert!(path.exists(), "the snapshot still landed");
        assert!(
            crate::attachment_store::blob_dir(&path)
                .join(crate::AttachmentRef::of(&png(3, "shot.png")).sha256)
                .exists(),
            "and so did its blob"
        );
    }

    /// A session file the sweep cannot parse may reference anything, so the GC
    /// refuses to run at all rather than guess.
    #[test]
    fn an_unreadable_session_stops_the_blob_gc() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let month = 30 * 24 * 60 * 60;
            Session::new(state_with("doomed", &cwd, vec![png(1, "a.png")]))
                .save("doomed")
                .unwrap();
            let digest = crate::AttachmentRef::of(&png(1, "a.png")).sha256;
            std::fs::write(session_dir(&cwd).join("broken.json"), "not valid json").unwrap();

            age_file(&session_file_path(&cwd, "doomed"), month + 1000);
            age_file(
                &crate::attachment_store::blob_dir_in(&session_dir(&cwd)).join(&digest),
                month + 1000,
            );

            sweep_sessions(None, Some(month));

            assert!(!session_file_path(&cwd, "doomed").exists(), "still purged");
            assert_eq!(
                blobs_in(&cwd),
                vec![digest],
                "the blob is kept: a session we cannot read might need it"
            );
        });
    }
}

#[cfg(test)]
mod roundtrip_audit {
    use super::tests::{state, with_test_env};
    use super::*;
    use crate::EntryKind;

    /// The audit: a `SessionState` with every field populated, encoded to the
    /// session file's JSON and decoded back, must come out identical — except for
    /// the transcript, which is no longer embedded in the `.json` (it lives in the
    /// sibling `<id>.jsonl`; see `transcript_rebuilds_from_the_sibling_jsonl`).
    ///
    /// This is the load-bearing property of the design ("save is a serialize,
    /// load is an assignment"), so it gets an explicit test rather than trust.
    #[test]
    fn state_to_json_to_state_is_lossless_except_for_the_transcript() {
        let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
        let mut assistant = Message::assistant("hello");
        assistant.reasoning_content = Some("secret thoughts".into());

        let state = SessionState {
            name: "Chat".into(),
            named_by_user: true, // round-trips through the file like any other field
            read_only: true,     // ditto — a revive reads the run's scope back off it
            id: Some("chat".into()), // NOT persisted: it is the file name
            model: "go://m".parse().unwrap(),
            provider_unset: false,
            base_url: "http://x/v1".into(),
            cwd: "/tmp/p".into(),
            messages: vec![Message::user("hi"), assistant],
            todos: vec![hrdr_tools::TodoItem {
                content: "task".into(),
                id: 0,
                status: "completed".into(),
                evidence: None,
            }],
            transcript: vec![
                Entry::at(EntryKind::Header, t),
                Entry::at(EntryKind::User("hi".into()), t),
                Entry::at(
                    EntryKind::Reasoning {
                        text: "thinking".into(),
                        took_ms: Some(900),
                    },
                    t,
                ),
                Entry::at(EntryKind::Assistant("hello".into()), t),
                Entry::at(
                    EntryKind::Tool {
                        id: "c1".into(),
                        name: "bash".into(),
                        args: "{}".into(),
                        result: "out".into(),
                        ok: true,
                        done: true, // NOT persisted: view state
                    },
                    t,
                ),
                Entry::at(EntryKind::System("note".into()), t),
                Entry::at(EntryKind::Stats("✓ 1 tok".into()), t),
                Entry::at(EntryKind::Diff("+added".into()), t),
            ],
            usage: SessionUsage {
                tokens_in: 10,
                tokens_out: 5,
                cost_usd: 0.25,
                cost_partial: false,
                last_prompt_tokens: Some(10),
                last_completion_tokens: Some(5),
                context_window: Some(1000),
                ..Default::default()
            },
            // Both are per-load scratch, not conversation state: a save describes
            // the attachments on the messages, and a load fills these in.
            attachment_refs: Vec::new(),
            attachment_losses: Vec::new(),
        };

        let json = serde_json::to_string(&Session::new(state.clone())).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        // Everything the file carries comes back byte-identical.
        assert_eq!(back.name, state.name);
        assert_eq!(back.model, state.model);
        assert_eq!(back.base_url, state.base_url);
        assert_eq!(back.cwd, state.cwd);
        assert_eq!(back.usage, state.usage);
        assert_eq!(back.todos.len(), 1);
        assert_eq!(back.todos[0].content, "task");
        assert_eq!(back.messages.len(), 2);
        // The transcript is deliberately NOT in the `.json` any more — it is
        // written to the sibling jsonl and rebuilt on load. A bare `from_str`
        // (no sibling file) therefore yields an empty one.
        assert!(
            back.transcript.is_empty(),
            "transcript is not embedded in the .json"
        );
    }

    /// The fields that deliberately do not survive a bare JSON round-trip, and
    /// why: the `id` (it is the file name) and the whole transcript (it lives in
    /// the sibling jsonl now).
    #[test]
    fn the_only_lossy_fields_are_the_intended_ones() {
        let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
        let state = SessionState {
            id: Some("chat".into()),
            transcript: vec![Entry::at(
                EntryKind::Tool {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: "{}".into(),
                    result: String::new(),
                    ok: true,
                    done: true,
                },
                t,
            )],
            ..Default::default()
        };
        let json = serde_json::to_string(&Session::new(state)).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        // `id` is the file's *name*, so it isn't stored inside it. `Session::load_path`
        // fills it from the path; a bare `from_str` leaves it None.
        assert_eq!(back.id, None, "id comes from the file name");
        let top: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            top.get("id").is_none(),
            "no session id key at the top level: {json}"
        );

        // The transcript key is absent from the serialized file entirely — it is
        // written incrementally to `<id>.jsonl`, not re-serialized here on every
        // save (the O(n²) this design removes).
        assert!(
            top.get("transcript").is_none(),
            "no transcript key in the .json: {json}"
        );
        assert!(back.transcript.is_empty(), "and none comes back from it");
    }

    /// The other half of the audit: the transcript round-trips through the sibling
    /// `<id>.jsonl`, folded back through the SAME reducer the live path uses, with
    /// tool args and results intact. This is what a resume actually rebuilds from.
    #[test]
    fn transcript_rebuilds_from_the_sibling_jsonl() {
        use crate::transcript_log::{Record, TranscriptLog};
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            // Save the snapshot (messages + metadata, no transcript).
            let st = state("Jsonl Rebuild", &cwd);
            Session::new(st).save("jsonl-rebuild").unwrap();

            // Write the sibling transcript as the live path would: a user turn, a
            // reply, and a tool call with args + result.
            let path = session_transcript_path(&cwd, "jsonl-rebuild");
            let mut w = TranscriptLog::append(&path).unwrap();
            w.write(&Record::Steered {
                text: "audit the config".into(),
            });
            w.write(&Record::Text {
                chunk: "on it".into(),
            });
            w.write(&Record::ToolStart {
                id: "c1".into(),
                name: "read".into(),
                args: r#"{"path":"config.toml"}"#.into(),
            });
            w.write(&Record::ToolEnd {
                id: "c1".into(),
                name: "read".into(),
                result: "port = 8080".into(),
                ok: true,
            });
            drop(w);

            // Load rebuilds the transcript from that jsonl.
            let back = Session::load(&cwd, "jsonl-rebuild").unwrap().state;
            let kinds: Vec<&EntryKind> = back.transcript.iter().map(|e| &e.kind).collect();
            assert!(
                matches!(
                    kinds.as_slice(),
                    [EntryKind::User(u), EntryKind::Assistant(_), EntryKind::Tool { .. }]
                        if u == "audit the config"
                ),
                "folded from the jsonl in order: {kinds:?}"
            );
            let tool = back
                .transcript
                .iter()
                .find_map(|e| match &e.kind {
                    EntryKind::Tool { args, result, .. } => Some((args.clone(), result.clone())),
                    _ => None,
                })
                .expect("a folded tool entry");
            assert!(tool.0.contains("config.toml"), "tool args survive");
            assert!(tool.1.contains("8080"), "tool result survives");
        });
    }

    /// A session file must preserve the model's thinking, even though
    /// `Message`'s own `Serialize` (the OpenAI wire format) drops it.
    ///
    /// Regression: the session file reused that wire impl, so `reasoning_content`
    /// and `anthropic_thinking_blocks` were silently discarded. Losing the latter
    /// breaks a resumed Anthropic conversation whose last assistant turn has a
    /// pending `tool_use`: the API requires the signed thinking block on the
    /// follow-up turn, and it was gone.
    #[test]
    fn message_thinking_survives_the_file() {
        let block = serde_json::json!({
            "type": "thinking",
            "thinking": "step by step",
            "signature": "sig-abc",
        });
        let mut assistant = Message::assistant("hello");
        assistant.reasoning_content = Some("secret thoughts".into());
        assistant.anthropic_thinking_blocks = vec![block.clone()];

        let state = SessionState {
            messages: vec![Message::user("hi"), assistant],
            ..Default::default()
        };
        let json = serde_json::to_string(&Session::new(state)).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        assert_eq!(
            back.messages[1].reasoning_content.as_deref(),
            Some("secret thoughts"),
            "reasoning survives: {json}"
        );
        assert_eq!(
            back.messages[1].anthropic_thinking_blocks,
            vec![block],
            "the signed thinking block survives verbatim: {json}"
        );
        // Messages with no thinking don't grow empty keys.
        assert_eq!(back.messages[0].reasoning_content, None);
        assert!(back.messages[0].anthropic_thinking_blocks.is_empty());
        assert!(
            !json.contains("\"anthropic_thinking_blocks\":[]"),
            "no empty thinking arrays written: {json}"
        );
    }

    /// A resumed Codex/Responses conversation must keep its reasoning chain.
    ///
    /// The items are `skip_serializing` on the wire form (they are Responses-API
    /// objects, meaningless to every other endpoint), so the session file has to
    /// re-insert them exactly as it does the Anthropic thinking blocks. Without
    /// this the resume silently loses the chain: the model re-derives its whole
    /// plan, and pays output tokens to do it, on every tool round afterwards.
    #[test]
    fn responses_reasoning_items_survive_the_file() {
        let item = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "plan"}],
            "encrypted_content": "ENC_ABC",
        });
        let mut assistant = Message::assistant("hello");
        assistant.responses_reasoning_items = vec![item.clone()];

        let state = SessionState {
            messages: vec![Message::user("hi"), assistant],
            ..Default::default()
        };
        let json = serde_json::to_string(&Session::new(state)).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        assert_eq!(
            back.messages[1].responses_reasoning_items,
            vec![item],
            "the encrypted reasoning item survives verbatim: {json}"
        );
        // Messages with no items don't grow empty keys.
        assert!(back.messages[0].responses_reasoning_items.is_empty());
        assert!(
            !json.contains("\"responses_reasoning_items\":[]"),
            "no empty reasoning-item arrays written: {json}"
        );
    }

    /// The same, through an actual file: save, load, and the encrypted reasoning
    /// item is still there to replay in the next Responses request.
    #[test]
    fn responses_reasoning_items_survive_a_real_save_and_load() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let item = serde_json::json!({
                "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC_ABC",
            });
            let mut assistant = Message::assistant("hi");
            assistant.responses_reasoning_items = vec![item.clone()];

            let mut st = state("Reasoning", &cwd);
            st.messages.push(assistant);
            Session::new(st).save("reasoning").unwrap();

            let back = Session::load(&cwd, "reasoning").unwrap().state;
            assert_eq!(
                back.messages.last().unwrap().responses_reasoning_items,
                vec![item]
            );
        });
    }

    /// The same, through an actual file: save, load, and the signed thinking
    /// block is still there for the follow-up Anthropic turn.
    #[test]
    fn thinking_survives_a_real_save_and_load() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let block = serde_json::json!({"type": "thinking", "signature": "sig-1"});
            let mut assistant = Message::assistant("hi");
            assistant.anthropic_thinking_blocks = vec![block.clone()];

            let mut st = state("Thinking", &cwd);
            st.messages.push(assistant);
            Session::new(st).save("thinking").unwrap();

            let back = Session::load(&cwd, "thinking").unwrap().state;
            assert_eq!(
                back.messages.last().unwrap().anthropic_thinking_blocks,
                vec![block]
            );
        });
    }

    /// The wire invariant is untouched: `Message`'s own `Serialize` — what
    /// `ChatRequest` puts on the OpenAI wire — still drops every one of the
    /// reasoning fields. Only the session file's encoding adds them back.
    #[test]
    fn the_openai_wire_form_still_drops_thinking() {
        let mut assistant = Message::assistant("hello");
        assistant.reasoning_content = Some("secret thoughts".into());
        assistant.anthropic_thinking_blocks = vec![serde_json::json!({"type": "thinking"})];
        assistant.responses_reasoning_items =
            vec![serde_json::json!({"type": "reasoning", "encrypted_content": "ENC_ABC"})];

        let wire = serde_json::to_string(&assistant).unwrap();
        assert!(!wire.contains("secret thoughts"), "{wire}");
        assert!(!wire.contains("anthropic_thinking_blocks"), "{wire}");
        assert!(!wire.contains("reasoning_content"), "{wire}");
        assert!(!wire.contains("responses_reasoning_items"), "{wire}");
        assert!(!wire.contains("ENC_ABC"), "{wire}");
    }

    /// Origin markers survive a session-file round-trip, so real user turns stay
    /// distinguishable from injected context after a resume. The wire form
    /// (OpenAI request) must never carry the origin field.
    #[test]
    fn synthetic_origin_survives_session_file_and_is_absent_from_wire() {
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let nudge = Message {
            origin: crate::MessageOrigin::Nudge,
            ..Message::user("nudge")
        };
        let tool = Message {
            origin: crate::MessageOrigin::Tool,
            ..Message::user("bg result")
        };
        // A resumed session must still know its summary is a summary, or the
        // next compaction summarizes it again and the tail loses a real turn.
        let summary = Message {
            origin: crate::MessageOrigin::Summary(crate::CompactionReason::ContextOverflow),
            ..Message::user("summary of earlier conversation")
        };

        let st = SessionState {
            cwd: cwd.clone(),
            messages: vec![
                Message::user("real"),
                nudge.clone(),
                tool.clone(),
                summary.clone(),
            ],
            ..Default::default()
        };

        // — Wire form (Message's own Serialize) drops origin —
        for m in [&nudge, &tool, &summary] {
            let wire = serde_json::to_string(m).unwrap();
            assert!(
                !wire.contains("origin"),
                "origin must not appear on the wire: {wire}"
            );
        }

        // — Session file round-trip preserves origin —
        let json = serde_json::to_string(&Session::new(st)).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        assert_eq!(back.messages.len(), 4);
        assert_eq!(
            back.messages[0].origin,
            crate::MessageOrigin::User,
            "default-origin messages retain User"
        );
        assert_eq!(
            back.messages[1].origin,
            crate::MessageOrigin::Nudge,
            "Nudge origin survives session file"
        );
        assert_eq!(
            back.messages[2].origin,
            crate::MessageOrigin::Tool,
            "Tool origin survives session file"
        );
        assert_eq!(
            back.messages[3].origin,
            crate::MessageOrigin::Summary(crate::CompactionReason::ContextOverflow),
            "the Summary tag AND the reason it carries survive a session file"
        );

        // — Old session files (no origin field) default to User on read —
        let old = r#"{"version":2,"created":0,"updated":0,"cwd":"/tmp","messages":[{"role":"user","content":"legacy"}]}"#;
        let parsed: Session = serde_json::from_str(old).unwrap();
        assert_eq!(
            parsed.state.messages[0].origin,
            crate::MessageOrigin::User,
            "a message without an origin field defaults to User"
        );
    }
}

/// The on-disk migration: an old session file is DATA, and opens without a word.
#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A v1 file carrying BOTH halves is folded into one identity on read.
    #[test]
    fn a_legacy_session_with_both_halves_migrates_silently() {
        let v1 = serde_json::json!({
            "version": 1,
            "created": 1_700_000_000u64,
            "updated": 1_700_000_000u64,
            "name": "old chat",
            "model": "deepseek/deepseek-chat",
            "provider": "openrouter",
            "base_url": "https://openrouter.ai/api/v1",
            "cwd": "/tmp/p",
        });
        let back: Session = serde_json::from_value(v1).expect("an old session still loads");
        assert_eq!(
            back.state.model,
            "openrouter://deepseek/deepseek-chat".parse().unwrap(),
            "the two halves are paired up at the read"
        );
        assert!(
            !back.state.provider_unset,
            "the file named a provider, so nothing needs supplying"
        );
        assert_eq!(back.state.name, "old chat", "and the rest is untouched");

        // What is written back is the new, coupled form — one key, one string.
        let json = serde_json::to_value(Session::new(back.state)).unwrap();
        assert_eq!(json["model"], "openrouter://deepseek/deepseek-chat");
        assert!(
            json.get("provider").is_none(),
            "the split key is gone: {json}"
        );
        assert_eq!(json["version"], 2);
    }

    /// A v1 file with a model but NO provider means "this model, on the provider
    /// currently in effect" — which the file cannot know, so it is flagged and the
    /// resume supplies it. It is never silently rehomed onto `local`.
    #[test]
    fn a_legacy_model_only_session_defers_to_the_provider_in_effect() {
        let v1 = serde_json::json!({
            "version": 1,
            "created": 0u64,
            "updated": 0u64,
            "model": "kimi-k2",
        });
        let back: Session = serde_json::from_value(v1).expect("loads");
        assert!(
            back.state.provider_unset,
            "the provider half is a placeholder, to be supplied by the resume"
        );
        assert_eq!(
            back.state.model.model(),
            "kimi-k2",
            "the model is the file's"
        );

        // What the resume then does with it (the TUI's `adopt_state`): the model,
        // on the identity in force.
        let in_force: ModelRef = "zen://grok-code".parse().unwrap();
        assert_eq!(
            ModelSpec::ModelOnly(back.state.model.model().to_string())
                .apply(&in_force)
                .unwrap(),
            "zen://kimi-k2".parse().unwrap()
        );
    }

    /// A v2 file is just read: one key, one string, no migration at all.
    #[test]
    fn a_v2_session_round_trips_as_one_key() {
        let st = SessionState {
            model: "zen://kimi-k2".parse().unwrap(),
            messages: vec![Message::user("hi")],
            ..Default::default()
        };
        let json = serde_json::to_string(&Session::new(st)).unwrap();
        assert!(json.contains("\"model\":\"zen://kimi-k2\""), "{json}");
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state.model, "zen://kimi-k2".parse().unwrap());
        assert!(!back.state.provider_unset);
        assert_eq!(back.version, SESSION_VERSION);
    }

    /// A present but malformed `model` field (e.g. `"://"` with an empty
    /// provider) is a load error — the file must be fixable by hand.
    /// Absent or empty model fields still get the legacy fallback.
    #[test]
    fn malformed_present_model_identity_is_a_load_error() {
        let json = r#"{"version":2,"created":0,"updated":0,"model":"://","name":"bad"}"#;
        let err = serde_json::from_str::<Session>(json).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expected") || msg.contains("Empty") || msg.contains("provider"),
            "error message about malformed model: {msg}"
        );
    }

    /// An absent model field in a v2 file still gets the default identity,
    /// not an error — preserving the legacy fallback.
    #[test]
    fn absent_model_field_gets_default_identity() {
        let json = r#"{"version":2,"created":0,"updated":0,"name":"no-model"}"#;
        let back: Session = serde_json::from_str(json).unwrap();
        assert_eq!(
            back.state.model.to_string(),
            crate::DEFAULT_MODEL_REF,
            "missing model gets the default"
        );
        assert!(
            back.state.provider_unset,
            "a file with no model is flagged as provider_unset"
        );
    }
}
