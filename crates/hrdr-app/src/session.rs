//! On-disk session persistence.
//!
//! A session is the conversation (`ChatMessage` history) plus light metadata,
//! stored as JSON under `$XDG_DATA_HOME/hrdr/sessions` (default
//! `~/.local/share/hrdr/sessions`). Sessions are partitioned by working
//! directory: each lives at `sessions/<cwd-slug>/<name-slug>.json`, so the
//! files are easy to manage by hand and startup auto-resume scopes to a project.

use std::path::{Path, PathBuf};

use crate::Entry;
use anyhow::{Context, Result};
use hrdr_agent::{DEFAULT_MODEL_REF, Message, ModelRef, ModelSpec, cwd_slug};
use hrdr_tools::TodoItem;
use serde::{Deserialize, Serialize};

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
/// `hrdr-agent` ([`hrdr_agent::AgentUsage`]) where a sub-agent's counters are
/// kept with no frontend attached. This alias is the name the session file and
/// the status bar know it by.
pub use hrdr_agent::AgentUsage as SessionUsage;

/// Everything about **one agent's** conversation that outlives the process: what
/// the user saw, what the model saw, which endpoint/model it ran on, and the
/// counters its status bar reads.
///
/// This is the single in-memory state a frontend keeps *per agent* and the exact
/// payload a state file stores — [`Session`] wraps it with file metadata and
/// nothing else. Loading is an assignment; saving is a serialize. There is no
/// lossy rebuild step, and no parallel vectors to keep in sync.
///
/// Every [`crate::Pane`] owns one, main and sub-agent alike: a delegated
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
    #[serde(default)]
    pub transcript: Vec<Entry>,
    /// Token counters for the status bar (see [`SessionUsage`]).
    #[serde(default)]
    pub usage: SessionUsage,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            name: String::new(),
            id: None,
            model: DEFAULT_MODEL_REF.parse().expect("a valid default identity"),
            provider_unset: false,
            base_url: String::new(),
            cwd: String::new(),
            messages: Vec::new(),
            todos: Vec::new(),
            transcript: Vec::new(),
            usage: SessionUsage::default(),
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
        }

        let raw = Raw::deserialize(d)?;
        let spec = raw
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::parse::<ModelSpec>)
            .transpose()
            .ok()
            .flatten();
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
                ModelRef::new(p, hrdr_agent::DEFAULT_MODEL).unwrap_or(default),
                false,
            ),
            (Some(ModelSpec::ModelOnly(m)), Some(p)) => (
                ModelRef::new(hrdr_agent::ProviderName::new(p), &m).unwrap_or(default),
                false,
            ),
            // v1, model only: the provider is whatever this process is on.
            (Some(spec @ ModelSpec::ModelOnly(_)), None) => (
                spec.apply(&default)
                    .expect("a bare model id always resolves"),
                true,
            ),
            (None, Some(p)) => (
                ModelRef::new(hrdr_agent::ProviderName::new(p), hrdr_agent::DEFAULT_MODEL)
                    .unwrap_or(default),
                false,
            ),
            (None, None) => (default, true),
        };
        Ok(SessionState {
            name: raw.name,
            id: None,
            model,
            provider_unset,
            base_url: raw.base_url,
            cwd: raw.cwd,
            messages: raw.messages,
            todos: raw.todos,
            transcript: raw.transcript,
            usage: raw.usage,
        })
    }
}

/// Serialize chat messages *for the session file*, which is not the OpenAI wire.
///
/// `Message`'s own `Serialize` is the wire format: it drops `reasoning_content`
/// and `anthropic_thinking_blocks` via `skip_serializing`, because replaying a
/// prior turn's thinking degrades reasoning models, and because those fields are
/// Anthropic-only. `ChatRequest` serializes `Vec<Message>` straight onto the
/// wire, so that invariant has to live on the type.
///
/// A session file has the opposite requirement: it must preserve them. Losing
/// `anthropic_thinking_blocks` breaks a resumed Anthropic conversation whose
/// last assistant turn has a pending `tool_use` — the API requires the signed
/// thinking block on the follow-up turn.
///
/// Encoding therefore round-trips each message through its wire form and adds
/// the two dropped fields back. Decoding needs no help: `skip_serializing` only
/// affects the encode side, and both fields are `#[serde(default)]`.
mod persisted_messages {
    use super::Message;
    use hrdr_agent::MessageOrigin;
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
            self.name = crate::session_name_from(&self.messages);
        }
    }

    /// A copy of this state as it should be written to disk: the session-chrome
    /// notices are dropped.
    ///
    /// Every launch prints its own welcome, and every resume prints its own
    /// "resumed session …" line. Persisting them means the next resume restores
    /// the old ones *and* appends a fresh one, so they accrete one copy per
    /// resume, forever.
    pub fn persisted(&self) -> SessionState {
        let mut out = self.clone();
        out.transcript
            .retain(|e| !matches!(e.kind, crate::EntryKind::Notice(_)));
        out
    }

    /// Whether this conversation is worth persisting: it has at least one user
    /// message.
    pub fn is_saveable(&self) -> bool {
        self.messages
            .iter()
            .any(|m| m.role == hrdr_agent::MessageRole::User)
    }

    /// Adopt a loaded session, settling any tool call that was mid-run when it
    /// was saved (nothing can finish it now, and a running block would spin its
    /// spinner forever).
    pub fn restored(mut self) -> Self {
        crate::settle_restored_tools(&mut self.transcript);
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

/// A saved conversation: file metadata plus the [`SessionState`] it carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Lightweight directory listing entry.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// File stem — the id you `/resume` by.
    pub id: String,
    /// The session's display name.
    pub name: String,
    /// The working directory this session belongs to.
    pub cwd: String,
    pub updated: u64,
    /// Absolute path to the session file.
    pub path: PathBuf,
}

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
pub fn subagent_transcript_dir(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join("subagents").join(sanitize_name(id))
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

impl Session {
    /// Save as `<cwd-slug>/<id>.json` (the cwd comes from `self.cwd`); returns
    /// the written path.
    pub fn save(&self, id: &str) -> Result<PathBuf> {
        let dir = session_dir(&self.state.cwd);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.json", sanitize_name(id)));
        // Autosave rebuilds a fresh `Session` per write; keep the original
        // creation time from the file being overwritten.
        let mut snap = self.clone();
        if let Ok(prev) = Self::load_path(&path) {
            snap.created = prev.created;
        }
        let json = serde_json::to_string_pretty(&snap).context("serializing session")?;
        hrdr_agent::write_atomic(&path, json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Load `<cwd-slug>/<id>.json`.
    pub fn load(cwd: &str, id: &str) -> Result<Session> {
        Self::load_path(&session_dir(cwd).join(format!("{}.json", sanitize_name(id))))
    }

    /// Load a session directly from a file path. The file id isn't stored in the
    /// file — it *is* the file name — so it's filled in here.
    pub fn load_path(path: &Path) -> Result<Session> {
        let data =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut session: Session =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;
        session.state.id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string);
        Ok(session)
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
pub fn unique_session_id(cwd: &str, name: &str) -> String {
    let slug = sanitize_name(name);
    let dir = session_dir(cwd);
    if !dir.join(format!("{slug}.json")).exists() {
        return slug;
    }
    for i in 2..10_000 {
        let cand = format!("{slug}-{i}");
        if !dir.join(format!("{cand}.json")).exists() {
            return cand;
        }
    }
    slug
}

/// Collect session files from one directory into `out`.
fn collect_sessions(dir: &Path, out: &mut Vec<SessionMeta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if let Ok(s) = Session::load_path(&path) {
            out.push(SessionMeta {
                id,
                name: s.state.name,
                cwd: s.state.cwd,
                updated: s.updated,
                path,
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryKind;
    use std::sync::Mutex;

    /// Global lock so env-var-dependent session tests don't race on HOME / XDG
    /// vars (std::env::set_var is not thread-safe in Rust tests).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn subagent_transcript_dir_nests_under_session() {
        // Both paths below are derived from $XDG_DATA_HOME, and `with_test_env`
        // swaps that process-global for other tests. Take the same lock: without
        // it a concurrent swap lands between the two calls and they disagree —
        // a latent race that only showed up once the suite grew enough to
        // reschedule around it.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = subagent_transcript_dir("/home/me/proj", "My Session");
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
                last_prompt_tokens: Some(10),
                last_completion_tokens: Some(5),
                context_window: Some(1000),
            };
            Session::new(st.clone()).save("round-trip").unwrap();

            let back = Session::load(&cwd, "round-trip").unwrap().state;
            assert_eq!(back.transcript, st.transcript, "transcript verbatim");
            assert_eq!(back.usage, st.usage, "token counters");
            assert_eq!(back.messages.len(), 1);
            assert_eq!(
                back.id.as_deref(),
                Some("round-trip"),
                "id from the filename"
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
    }

    // ── unique_session_id ─────────────────────────────────────────────────────

    #[test]
    fn unique_session_id_returns_plain_slug_when_dir_absent() {
        let id = unique_session_id("/nonexistent/hrdr/test/path/12345", "my session");
        assert_eq!(id, "my-session");
    }

    #[test]
    fn unique_session_id_appends_suffix_on_collision() {
        with_test_env(|_tmp| {
            let cwd = std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert_eq!(unique_session_id(&cwd, "chat"), "chat");
            Session::new(state("chat", &cwd)).save("chat").unwrap();
            assert_eq!(unique_session_id(&cwd, "chat"), "chat-2");
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
}

#[cfg(test)]
mod roundtrip_audit {
    use super::tests::{state, with_test_env};
    use super::*;
    use crate::EntryKind;

    /// The audit: a `SessionState` with every field populated, encoded to the
    /// session file's JSON and decoded back, must come out identical — except
    /// for the two fields deliberately not persisted.
    ///
    /// This is the load-bearing property of the design ("save is a serialize,
    /// load is an assignment"), so it gets an explicit test rather than trust.
    #[test]
    fn state_to_json_to_state_is_lossless_except_for_view_state() {
        let t = crate::time_from_unix(1_700_000_000, chrono::Local::now());
        let mut assistant = Message::assistant("hello");
        assistant.reasoning_content = Some("secret thoughts".into());

        let state = SessionState {
            name: "Chat".into(),
            id: Some("chat".into()), // NOT persisted: it is the file name
            model: "go://m".parse().unwrap(),
            provider_unset: false,
            base_url: "http://x/v1".into(),
            cwd: "/tmp/p".into(),
            messages: vec![Message::user("hi"), assistant],
            todos: vec![hrdr_tools::TodoItem {
                content: "task".into(),
                status: "completed".into(),
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
                        done: true,
                        expanded: true, // NOT persisted: view state
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
                last_prompt_tokens: Some(10),
                last_completion_tokens: Some(5),
                context_window: Some(1000),
            },
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
        assert_eq!(back.transcript.len(), state.transcript.len());
        // Every entry kind and timestamp survives verbatim. The one exception is
        // the tool block's `expanded` flag (view state), so normalise it before
        // comparing — `the_only_lossy_fields_are_the_intended_ones` pins that.
        let mut want = state.transcript.clone();
        for e in &mut want {
            if let EntryKind::Tool { expanded, .. } = &mut e.kind {
                *expanded = false;
            }
        }
        assert_eq!(back.transcript, want, "transcript round-trips");
    }

    /// The two fields that deliberately do not survive, and why.
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
                    expanded: true,
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

        // A tool block's expanded flag is view state: restored blocks start collapsed.
        let EntryKind::Tool { expanded, .. } = &back.transcript[0].kind else {
            panic!("not a tool");
        };
        assert!(!*expanded, "expanded is view state, not persisted");
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
    /// `ChatRequest` puts on the OpenAI wire — still drops both fields. Only the
    /// session file's encoding adds them back.
    #[test]
    fn the_openai_wire_form_still_drops_thinking() {
        let mut assistant = Message::assistant("hello");
        assistant.reasoning_content = Some("secret thoughts".into());
        assistant.anthropic_thinking_blocks = vec![serde_json::json!({"type": "thinking"})];

        let wire = serde_json::to_string(&assistant).unwrap();
        assert!(!wire.contains("secret thoughts"), "{wire}");
        assert!(!wire.contains("anthropic_thinking_blocks"), "{wire}");
        assert!(!wire.contains("reasoning_content"), "{wire}");
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

        let steer = Message {
            origin: hrdr_agent::MessageOrigin::Steering,
            ..Message::user("steer")
        };
        let bg = Message {
            origin: hrdr_agent::MessageOrigin::BackgroundResult,
            ..Message::user("bg result")
        };

        let st = SessionState {
            cwd: cwd.clone(),
            messages: vec![Message::user("real"), steer.clone(), bg.clone()],
            ..Default::default()
        };

        // — Wire form (Message's own Serialize) drops origin —
        let wire = serde_json::to_string(&steer).unwrap();
        assert!(
            !wire.contains("origin"),
            "origin must not appear on the wire: {wire}"
        );
        let wire = serde_json::to_string(&bg).unwrap();
        assert!(
            !wire.contains("origin"),
            "origin must not appear on the wire: {wire}"
        );

        // — Session file round-trip preserves origin —
        let json = serde_json::to_string(&Session::new(st)).unwrap();
        let back = serde_json::from_str::<Session>(&json).unwrap().state;

        assert_eq!(back.messages.len(), 3);
        assert_eq!(
            back.messages[0].origin,
            hrdr_agent::MessageOrigin::User,
            "default-origin messages retain User"
        );
        assert_eq!(
            back.messages[1].origin,
            hrdr_agent::MessageOrigin::Steering,
            "Steering origin survives session file"
        );
        assert_eq!(
            back.messages[2].origin,
            hrdr_agent::MessageOrigin::BackgroundResult,
            "BackgroundResult origin survives session file"
        );

        // — Old session files (no origin field) default to User on read —
        let old = r#"{"version":2,"created":0,"updated":0,"cwd":"/tmp","messages":[{"role":"user","content":"legacy"}]}"#;
        let parsed: Session = serde_json::from_str(old).unwrap();
        assert_eq!(
            parsed.state.messages[0].origin,
            hrdr_agent::MessageOrigin::User,
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
}
