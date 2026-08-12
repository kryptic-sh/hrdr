//! HTTP client over `/v1/chat/completions` and `/v1/models`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::capped_read::{MAX_DIAGNOSTIC_BYTES, MAX_LOG_FILE_BYTES, MAX_STRUCTURED_JSON_BYTES};
use crate::sse::{SseDecoder, SseOverflow};
use crate::types::{CacheMode, ChatChunk, ChatMessage, ChatRequest, Role, ToolDef};

/// Wire-level debug log, enabled by `HRDR_LOG_REQUESTS=<path>`: every chat
/// request body, every raw SSE data line, and every non-2xx response body is
/// appended to the file as one JSON object per line. For debugging
/// harness ⇄ server disagreements (tool-call framing, stream shape) — off
/// unless the env var is set.
static REQUEST_LOG: OnceLock<Option<WireLog>> = OnceLock::new();
/// Latches once the wire log has permanently stopped (a rotation attempt
/// failed): subsequent writes short-circuit, matching the historical
/// stop-at-cap behavior and avoiding warning/rename spam.
static REQUEST_LOG_STOPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// One-shot slot for a client-level warning (wire log rotated, an auth header
/// stripped from `extra_headers`) awaiting delivery to the caller.
static CLIENT_WARNING: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// The open wire log together with its path, so [`log_wire`] can rotate the
/// file in place (rename active → `<name>.1`, reopen a fresh active file).
struct WireLog {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

/// Take the one-shot client warning for delivery through the caller's normal
/// event channel. This avoids writing stderr while a TUI owns the terminal.
pub fn take_client_warning() -> Option<String> {
    CLIENT_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut warning| warning.take())
}

fn request_log() -> Option<&'static WireLog> {
    REQUEST_LOG
        .get_or_init(|| {
            let path = std::env::var_os("HRDR_LOG_REQUESTS")?;
            let path = PathBuf::from(path);
            let file = open_wire_log(&path)?;
            Some(WireLog {
                path,
                file: Mutex::new(file),
            })
        })
        .as_ref()
}

/// Open (creating if needed) the wire-log file at `path` in append mode with
/// owner-only (0600) permission hardening on Unix, so local users cannot read
/// the API request/response data. Returns `None` if the target is not a
/// regular file or the permissions cannot be applied. Used both for the
/// initial open and for the fresh active file created on rotation, so both
/// share the same 0600 discipline.
///
/// Confidentiality is platform-dependent — see [`crate::fs::owner_only_options`]
/// for what "owner-only" is worth on each platform. One caveat specific to this
/// file: its path is caller-chosen (unlike the credential store, which lives
/// under the user profile), so on Windows it inherits the ACLs of whatever
/// directory `HRDR_LOG_REQUESTS` points at, and pointing it at a world-readable
/// directory leaks the logged request/response data on **any** platform. Callers
/// should keep it under a directory only they can read.
fn open_wire_log(path: &Path) -> Option<std::fs::File> {
    // Preflight: reject a pre-existing symlink or a non-regular file before
    // opening.  This gives a clean early rejection for the ordinary
    // mistaken-setup or symlink-in-path case, and is the only guard on
    // non-Unix (where no O_NOFOLLOW equivalent is applied below).  On Unix it
    // is backed up by the atomic O_NOFOLLOW open, so the preflight is not
    // relied on to close the check→open race.
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            // symlink_metadata on a symlink returns the symlink's own type,
            // so is_symlink() is true only for actual symbolic links.
            if meta.file_type().is_symlink() {
                return None;
            }
            if !meta.file_type().is_file() {
                return None;
            }
            // Existing regular file — proceed to open for append.
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Path does not exist — open will create it.
        }
        Err(_) => return None,
    }

    // Owner-only so local users cannot read the API request/response data, and
    // no-follow so the open itself refuses a symlinked final component — that is
    // what closes the check→open TOCTOU window the preflight above cannot: an
    // attacker cannot swap a symlink in between the two.  Both guarantees, and
    // what each is worth per platform, live on the helper.
    let mut opts = crate::fs::owner_only_options_no_follow();
    opts.create(true).append(true);
    let file = opts.open(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Post-open descriptor check: confirm the opened handle refers to a
        // regular file.  O_NOFOLLOW already rejects a final-component symlink,
        // so this now mainly guards non-regular targets reachable without a
        // final symlink (e.g. a pre-existing FIFO or device node).
        if !file.metadata().ok()?.file_type().is_file() {
            return None;
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .ok()?;
    }
    Some(file)
}

/// Rotate the active wire log: rename it to `<name>.1` (atomically replacing
/// any previous `.1`, which preserves the 0600 perms of the moved inode) and
/// swap `file` for a freshly created, 0600-hardened active file at `path`.
///
/// On any failure the original active file is left in place (a best-effort
/// rename-back undoes a partial move) and an error is returned so the caller
/// can fall back to stop-at-cap.
fn rotate_wire_log(path: &Path, file: &mut std::fs::File) -> std::io::Result<()> {
    let rotated = crate::fs::sibling_with_suffix(path, ".1");
    // `rename` replaces an existing `.1`, so no `.2`… ever accumulates, and it
    // preserves the moved file's permissions (same inode).
    std::fs::rename(path, &rotated)?;
    match open_wire_log(path) {
        Some(new_file) => {
            *file = new_file;
            Ok(())
        }
        None => {
            // Reopen failed after the move: restore the original name so we
            // don't leave the active path missing, then report failure.
            let _ = std::fs::rename(&rotated, path);
            Err(std::io::Error::other("failed to reopen rotated wire log"))
        }
    }
}

/// Whether appending a `line_len`-byte record to a `current`-byte file would
/// meet or exceed `cap` (and so should trigger rotation).
fn wire_log_over_cap(current: u64, line_len: u64, cap: u64) -> bool {
    current >= cap || line_len > cap.saturating_sub(current)
}

/// Publish the one-shot client warning for delivery through the caller's
/// event channel (see [`take_client_warning`]).
///
/// `pub(crate)` because degradations worth telling the user about are not all
/// discovered here: the native Anthropic path is the only code that sees a
/// `stop_reason` hrdr does not recognize, and that is a claim about the reply's
/// completeness the user has to hear (see [`crate::anthropic::map_stop_reason`]).
pub(crate) fn set_client_warning(msg: String) {
    if let Ok(mut pending) = CLIENT_WARNING.get_or_init(|| Mutex::new(None)).lock() {
        *pending = Some(msg);
    }
}

/// Append one `{"ts":…,"kind":…,…}` line to the wire log (no-op when off).
///
/// `pub(crate)` because the wire log is a promise about *every* backend, and the
/// native Anthropic/Codex paths build and send their own requests — they have to
/// be able to log them (see [`crate::anthropic::chat_stream`],
/// [`crate::codex::chat_stream`]). `fields` is a closure that is only called
/// once the log is confirmed live, so when it is off the call site's `json!`
/// never runs at all.
pub(crate) fn log_wire(kind: &str, fields: impl FnOnce() -> serde_json::Value) {
    use std::sync::atomic::Ordering::Relaxed;

    let Some(wire) = request_log() else {
        return;
    };
    // A prior rotation failed and we fell back to stop-at-cap: stay stopped.
    if REQUEST_LOG_STOPPED.load(Relaxed) {
        return;
    }
    let fields = fields();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut obj = serde_json::json!({"ts": ts, "kind": kind});
    if let (Some(o), Some(f)) = (obj.as_object_mut(), fields.as_object()) {
        for (k, v) in f {
            o.insert(k.clone(), v.clone());
        }
    }
    let mib = MAX_LOG_FILE_BYTES / (1024 * 1024);
    if let Ok(mut file) = wire.file.lock() {
        let current = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut line = obj.to_string();
        line.push('\n');
        if wire_log_over_cap(current, line.len() as u64, MAX_LOG_FILE_BYTES) {
            // At the cap: rotate to keep the newest window rather than dropping
            // the (usually most interesting) tail. Rotation fires once per fill,
            // so the warning is naturally throttled to once per rotation.
            match rotate_wire_log(&wire.path, &mut file) {
                Ok(()) => {
                    set_client_warning(format!(
                        "request log reached {mib} MiB; rotated to {} \
                         (keeping the newest {mib} MiB, at most {} MiB on disk)",
                        crate::fs::sibling_with_suffix(&wire.path, ".1").display(),
                        mib * 2
                    ));
                }
                Err(_) => {
                    // Rotation failed: fall back to the historical stop-at-cap
                    // behavior, warning once and then staying silent.
                    if !REQUEST_LOG_STOPPED.swap(true, Relaxed) {
                        set_client_warning(format!(
                            "request log rotation failed; logging stopped after \
                             reaching {mib} MiB"
                        ));
                    }
                    return;
                }
            }
        }
        // Always write after a successful rotation (fresh file), so a record is
        // never silently dropped — even one larger than the cap.
        let _ = file.write_all(line.as_bytes());
    }
}

/// Classification of a chat-endpoint error for the agent's retry and
/// compaction logic. Carried in the typed [`ChatError`] so hrdr-agent can
/// match on the kind directly rather than scanning Display strings.
///
/// Four classes, mapping onto codex's `should_retry_with_current_model`
/// taxonomy (see docs/backlog.md, "Compaction rewrite" item 5):
/// [`Transient`](ChatErrorKind::Transient) ≈ `UnexpectedStatus` /
/// `ServerOverloaded` / `InternalServerError` / `RetryLimit`, retry the same
/// request; [`Overflow`](ChatErrorKind::Overflow) ≈ `ContextWindowExceeded`,
/// compaction's job; [`Other`](ChatErrorKind::Other) ≈ `InvalidRequest`;
/// [`UsageLimit`](ChatErrorKind::UsageLimit) ≈ `UsageLimitReached`. The
/// model-specific half of codex's taxonomy (a different model might work) has
/// no counterpart here — hrdr has no model-switch machinery, so all four
/// classes surface rather than switch models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatErrorKind {
    /// The request exceeded the model's context window; compaction may help.
    Overflow,
    /// A transient network or server error worth retrying with backoff.
    Transient,
    /// Any other error (bad request, auth failure, unsupported parameter, …).
    Other,
    /// A usage/quota limit — billing exhausted, a spend cap, or insufficient
    /// quota. Retrying cannot help until the window resets (billing cycle,
    /// plan change), so it is terminal; codex's `UsageLimitReached`.
    UsageLimit,
}

/// A typed chat-endpoint error, emitted by [`Client::chat_stream`] for HTTP
/// non-2xx responses and truncated streams. Prefer matching on [`ChatErrorKind`]
/// for retry/compaction decisions; `message` preserves the full display string
/// for the fallback text-scanner in hrdr-agent (which handles errors that arrive
/// only as mid-stream bodies and never go through this path).
#[derive(Debug)]
pub struct ChatError {
    /// HTTP status code, if this was an HTTP-level error.
    pub status: Option<u16>,
    /// Server-requested retry delay parsed from the `Retry-After` header, if
    /// present (only meaningful for 429 responses). Clamped to 60 s upstream.
    pub retry_after: Option<std::time::Duration>,
    /// Coarse classification for retry/compaction decisions.
    pub kind: ChatErrorKind,
    /// Full display string — preserved so hrdr-agent's text-fallback scanner
    /// sees the same content it used to (and can scan the body text for e.g.
    /// 400-overflow messages whose kind can't be determined from status alone).
    pub message: String,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChatError {}

/// Map an HTTP status code to a [`ChatErrorKind`]. 413 is always overflow;
/// 429/5xx are transient. Everything else needs body-text analysis (handled
/// by hrdr-agent's fallback scanner on the `Other` path).
pub(crate) fn classify_status(status: u16) -> ChatErrorKind {
    match status {
        413 => ChatErrorKind::Overflow,
        // 408 request timeout and Cloudflare's origin-timeout family (522/524)
        // are transient — gateways in front of OpenAI-compatible providers emit
        // these under load, and the request is safe to retry.
        408 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529 => ChatErrorKind::Transient,
        _ => ChatErrorKind::Other,
    }
}

/// Turn a non-2xx HTTP response into the typed error every backend reports.
///
/// One definition, called from every backend (this one, `anthropic.rs`,
/// `codex.rs`), which each carried a byte-identical copy of it. They were
/// identical because they must be — the agent's retry and compaction decisions
/// read `kind`/`retry_after`, so a backend that classified differently would
/// silently retry differently — and a copy per backend is exactly the shape that
/// drifts. Consumes `resp`: the body is read (capped) for the diagnostic.
pub(crate) async fn error_from_response(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let retry_after = retry_after_from_headers(resp.headers());
    let text = crate::capped_read::read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
    let status_u16 = status.as_u16();
    log_wire(
        "error_response",
        || serde_json::json!({"status": status_u16, "body": text}),
    );
    let mut kind = classify_status(status_u16);
    // A 429 can be a rate limit (retryable) or a spent quota/billing cap
    // (permanent until the window resets); `classify_status` can only see the
    // status, so the body decides which. Only the transient set (408/429/5xx)
    // is even eligible.
    if kind == ChatErrorKind::Transient && crate::retry::is_usage_limit_text(&text) {
        kind = ChatErrorKind::UsageLimit;
    }
    anyhow::Error::new(ChatError {
        status: Some(status_u16),
        retry_after,
        kind,
        message: format!(
            "chat endpoint returned {status}: {text}{}",
            retry_after_suffix_from(retry_after)
        ),
    })
}

/// Parse a `Retry-After` header into a [`Duration`], clamped to
/// [`MAX_BACKOFF`](crate::MAX_BACKOFF) — the same ceiling the computed backoff
/// obeys, so no server can park a turn for longer than our own worst wait.
/// Accepts both delta-seconds (RFC 7231 §7.1.3) and IMF-fixdate formats.
pub(crate) fn retry_after_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim();
    // Delta-seconds: a bare integer.
    if let Ok(secs) = trimmed.parse::<u64>() {
        return (secs > 0)
            .then(|| std::time::Duration::from_secs(secs.min(crate::MAX_BACKOFF.as_secs())));
    }
    // IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`
    parse_imf_fixdate(trimmed)
}

/// Parse an IMF-fixdate per RFC 7231 §7.1.1.1 into a duration-from-now,
/// clamped to 60 s. Returns `None` on parse failure or past dates.
fn parse_imf_fixdate(raw: &str) -> Option<std::time::Duration> {
    // Format: `wkday "," SP date SP time SP "GMT"`
    // date = day SP month SP year, e.g. `06 Nov 1994`
    // time = hour ":" minute ":" second, e.g. `08:49:37`
    let raw = raw.strip_suffix(" GMT")?;
    // Strip the leading `wkday, ` prefix. The weekday is discarded without
    // validation — `Xyz, 06 Nov …` parses fine. Laxer than RFC 7231 and
    // harmless (the weekday is redundant with the date), but deliberate: know
    // it before "fixing" a test that relies on the laxness.
    let date_time = raw.split_once(", ").map(|(_, rest)| rest).unwrap_or(raw);
    let (date_str, time_str) = date_time.rsplit_once(' ')?;

    let mut date_parts = date_str.split(' ');
    let day: u64 = date_parts.next()?.parse().ok()?;
    let month = parse_month(date_parts.next()?)?;
    let year: i32 = date_parts.next()?.parse().ok()?;

    let mut parts = time_str.split(':');
    let hour: u64 = parts.next()?.parse().ok()?;
    let min: u64 = parts.next()?.parse().ok()?;
    let sec: u64 = parts.next()?.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let total_secs = days as u64 * 86400 + hour * 3600 + min * 60 + sec;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let delay = total_secs.saturating_sub(now);
    (delay > 0).then(|| std::time::Duration::from_secs(delay.min(crate::MAX_BACKOFF.as_secs())))
}

/// Days since Unix epoch (1970-01-01) from a Gregorian year/month/day.
fn days_from_civil(year: i32, month: u64, day: u64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's algorithm: shift so March is month 1, compute eras.
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (m as u32 - 3) + 2) / 5 + day as u32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let epoch_days = era as i64 * 146097 + doe as i64 - 719468;
    Some(epoch_days)
}

fn parse_month(s: &str) -> Option<u64> {
    match s.to_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

/// Format a retry-after duration as ` (retry-after: Ns)` for embedding in
/// error messages (preserves the text format hrdr-agent's fallback scanner
/// used to rely on).
pub(crate) fn retry_after_suffix_from(d: Option<std::time::Duration>) -> String {
    d.map(|d| format!(" (retry-after: {}s)", d.as_secs()))
        .unwrap_or_default()
}

/// Boxed stream of decoded streaming chunks.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// Which wire protocol the endpoint speaks. Auto-detected from `base_url`
/// (Anthropic's own host → native Messages API; the ChatGPT/Codex `/codex/`
/// endpoint → the OpenAI Responses API), else the OpenAI chat-completions shape
/// every other server uses.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    OpenAi,
    Anthropic,
    /// OpenAI **Responses API** — the ChatGPT/Codex OAuth endpoint
    /// (`https://chatgpt.com/backend-api/codex/responses`). See [`crate::codex`].
    Codex,
}

/// Last-resort `max_tokens` for the native Anthropic backend (the API requires
/// the field). Only used when the models.dev catalog can't name the model's real
/// cap — see [`Client::anthropic_max_tokens`]. It is *far* below every current
/// model's cap (128k on Opus 5 / Sonnet 5 / Opus 4.6-4.8, 64k on the 4.5 family,
/// 32k on Opus 4.1), so relying on it truncates real work.
const ANTHROPIC_MAX_TOKENS: u32 = 8192;

/// The model id that means **"whatever this endpoint serves"** rather than a
/// real model — hrdr's default identity, for a user who has pointed it at a
/// local server and named nothing.
///
/// It is not a name any provider knows, so putting it on the wire is a request
/// that cannot succeed anywhere it is actually read: vLLM validates `model`
/// against its served names and answers `404 The model 'default' does not
/// exist`, and llama.cpp's router (`--models-dir`) selects by the same field.
/// The one server that tolerates it is single-model llama.cpp, which ignores
/// `model` entirely. So the OpenAI-shaped request builder omits the field
/// instead — vLLM's own `model` is nullable and falls back to the served model,
/// which is the same thing the sentinel was trying to say. The two native
/// builders are the other side of the same decision: neither has a nullable
/// `model`, so `chat_stream` hands them the sentinel before `wire_model`
/// resolves it and they put the literal string on the wire — a provider entry
/// left at `default` pointed at Anthropic or Codex sends `"model": "default"`
/// and gets that provider's own "unknown model" error (pinned by
/// `the_unnamed_model_sentinel_reaches_the_wire_on_the_native_backends`).
///
/// Defined here rather than in hrdr-agent (whose `DEFAULT_MODEL` is the same
/// string) because this crate is what decides whether the field goes on the
/// wire; hrdr-agent's constant is checked against this one in its own tests.
pub const UNNAMED_MODEL: &str = "default";

/// Whether `base_url` points at a server on this machine.
///
/// Local servers differ from hosted ones in ways the request builder cares
/// about — they need no credential, they serve one model whose id is an
/// accident of how they were launched, and OpenAI-specific routing hints mean
/// nothing to them — so the predicate lives next to the code that decides the
/// request shape. `hrdr_agent::is_local_endpoint` delegates here so the two
/// cannot disagree about what "local" is.
pub fn is_local_host(base_url: &str) -> bool {
    let host = url_host(base_url);
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
        || host.ends_with(".local")
        || host.is_empty()
}

/// A configured chat-completions client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Model id sent with each request (a public field; set it directly).
    pub model: String,
    pub temperature: Option<f32>,
    /// Prompt-caching strategy (default [`CacheMode::Off`]).
    cache: CacheMode,
    /// Use the extended 1-hour cache TTL instead of the ~5-minute default.
    cache_1h: bool,
    /// Reasoning-effort label; sent as `reasoning_effort` when it names a known
    /// level (see [`crate::normalize_effort`]).
    effort: Option<String>,
    /// Opt-in request parameters (`max_tokens`, `top_p`, `seed`, `stop`,
    /// `include_usage`) applied to each request.
    params: crate::RequestParams,
    /// Extra HTTP headers (provider-configured) sent with every request.
    extra_headers: Vec<(String, String)>,
    /// OpenAI's `prompt_cache_key`: a caller-chosen routing hint combined with
    /// the prompt-prefix hash when OpenAI looks for a cache entry. Sent on the
    /// two OpenAI-shaped backends only ([`Backend::OpenAi`], [`Backend::Codex`]);
    /// the native Anthropic Messages API has no such field and would 400 on it.
    /// See [`Client::set_prompt_cache_key`] for why it must be set.
    prompt_cache_key: Option<String>,
    system_cache_split: Option<usize>,
    /// Azure OpenAI API version. When set, requests append `?api-version=<v>` and
    /// authenticate with an `api-key` header instead of `Bearer` (Azure is still
    /// the OpenAI chat-completions wire, just a different URL + auth).
    api_version: Option<String>,
    /// The user's per-attachment size ceiling (`max_attachment_bytes`), or
    /// `None` for the provider defaults. Applied by [`Client::check_attachments`]
    /// — the gate every request passes through — see
    /// [`crate::media::check_attachments`].
    max_attachment_bytes: Option<usize>,
    /// Wire protocol, derived from `base_url`.
    backend: Backend,
    /// What [`UNNAMED_MODEL`] resolved to at this endpoint, once asked.
    ///
    /// `None` = not asked yet; `Some(None)` = asked, and the endpoint named
    /// nothing usable (unreachable, or serving more than one model, where
    /// picking one would be a guess). Shared across clones and reset by
    /// [`Client::set_base_url`], since the answer is a property of the endpoint.
    resolved_model: std::sync::Arc<std::sync::Mutex<Option<Option<String>>>>,
}

/// Whether `model` is an OpenAI reasoning model that wants `max_completion_tokens`
/// instead of `max_tokens` (o-series, gpt-5). Handles a provider prefix like
/// `openai/o3-mini`. Non-OpenAI models are unaffected (they use `max_tokens`).
fn uses_max_completion_tokens(model: &str) -> bool {
    let m = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("o5")
        || m.starts_with("gpt-5")
}

/// The host portion of `base_url` (scheme, userinfo, port, and path stripped).
///
/// Handles bracketed IPv6 literals (`http://[::1]:8080/v1` → `::1`): a naive
/// `rsplit_once(':')` would chop an IPv6 address's internal colons instead of
/// just the trailing port, mangling the host. This helper is duplicated in
/// hrdr-agent's `resolve_cache_mode` helpers — keep both in sync (or, better,
/// have hrdr-agent call this one).
pub fn url_host(base_url: &str) -> &str {
    let authority = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: host is everything up to the closing `]`;
        // a trailing `:port` after the bracket is discarded.
        return rest.split(']').next().unwrap_or(authority);
    }
    authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
}

/// Whether a request to `base_url` for `model` must pass each assistant turn's
/// `reasoning_content` back to the API.
///
/// DeepSeek's thinking mode returns its chain-of-thought as
/// `reasoning_content`, and for requests carrying `tools` — always, in hrdr's
/// agent loop — it requires every assistant turn's `reasoning_content` back on
/// every subsequent request. Omitting it is a 400: *"The reasoning_content in
/// the thinking mode must be passed back to the API."*
///
/// That requirement follows the MODEL wherever it is served, so testing the
/// host alone is not enough — gateways are how most people reach DeepSeek, and
/// each has a host of its own while proxying the same upstream that 400s:
/// OpenCode Zen (`deepseek-v4-flash-free`), OpenRouter
/// (`deepseek/deepseek-chat`), Together (`deepseek-ai/DeepSeek-R1`), LiteLLM.
/// So the wire model id is matched by name, case-insensitively (`DeepSeek-R1`
/// counts), and DeepSeek's own host stays in the test for the ids a name test
/// would miss — a served-model alias, the [`UNNAMED_MODEL`] sentinel.
///
/// Kept this narrow deliberately: replaying whatever reasoning a provider
/// streamed us on every endpoint would send `reasoning_content` to whichever
/// model the session is switched to next, and to OpenAI-compatible servers
/// that reject unknown message fields.
fn replays_reasoning_content(base_url: &str, model: &str) -> bool {
    url_host(base_url) == "api.deepseek.com" || model.to_ascii_lowercase().contains("deepseek")
}

/// Whether `base_url` is OpenRouter — the one endpoint that takes a `plugins`
/// array on the chat-completions body (see [`openrouter_pdf_plugins`]).
///
/// Suffix-matched on the host, not the URL: `openrouter.ai.evil.com` is a
/// different site, and `gateway.openrouter.ai` is not.
fn is_openrouter(base_url: &str) -> bool {
    let host = url_host(base_url);
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// The `plugins` array an OpenRouter request carrying a PDF needs, or `None`
/// for the requests that are better off without one.
///
/// OpenRouter parses a PDF itself for models that cannot read one, and picks
/// the parser when the request names none: *"If you don't explicitly specify an
/// engine, OpenRouter will default first to the model's native file processing
/// capabilities, and if that's not available, we will use the `mistral-ocr`
/// engine"* — which is *"$2 per 1,000 pages"*, and billed to the OpenRouter
/// account even under BYOK. So the field is sent exactly when that fallback is
/// what would otherwise happen, naming the free engine instead
/// (*"cloudflare-ai: Converts PDFs to markdown using Cloudflare Workers AI
/// (Free)"*).
///
/// `accepts` is the model's models.dev input-modality list — the same list the
/// attachment gate reads. A model listed with `pdf` takes the file natively, so
/// no plugin is sent and OpenRouter's own native default applies: pinning an
/// engine there would replace the model's own reading of the pages with
/// markdown text. A model the catalog does not know (the gate lets those
/// through — see [`crate::media::check_attachments`]) is the case the free
/// engine is for.
///
/// <https://openrouter.ai/docs/features/multimodal/pdfs>
fn openrouter_pdf_plugins(accepts: Option<&[String]>) -> Option<serde_json::Value> {
    let native = accepts.is_some_and(|a| a.iter().any(|m| m == "pdf"));
    (!native)
        .then(|| serde_json::json!([{ "id": "file-parser", "pdf": { "engine": "cloudflare-ai" } }]))
}

/// Detect the wire protocol from `base_url`:
/// - `api.anthropic.com` → native Anthropic Messages API (unlocks caching).
/// - `chatgpt.com` with a `/codex/` path → the OpenAI Responses API (the
///   ChatGPT/Codex OAuth endpoint); the client POSTs to `{base_url}/responses`,
///   so point it at `https://chatgpt.com/backend-api/codex`.
/// - anything else → the OpenAI chat-completions shape.
fn detect_backend(base_url: &str) -> Backend {
    let host = url_host(base_url);
    if host == "api.anthropic.com" || host.ends_with(".anthropic.com") {
        Backend::Anthropic
    } else if (host == "chatgpt.com" || host.ends_with(".chatgpt.com"))
        && base_url.contains("/codex")
    {
        Backend::Codex
    } else {
        Backend::OpenAi
    }
}

/// Whether hrdr will speak the **native Anthropic Messages API** at `base_url`.
///
/// The predicate form of [`detect_backend`] for callers outside this crate, which
/// cannot see the private [`Backend`]. It exists so a caller that needs the
/// *decision* (does this endpoint consume `cache_control`?) asks for the decision
/// instead of string-comparing `wire_protocol`'s display name — that name is for
/// showing a human, and a rename there must not silently flip a behaviour.
pub fn is_anthropic_backend(base_url: &str) -> bool {
    detect_backend(base_url) == Backend::Anthropic
}

/// Header names that carry a credential. Only the client's own auth may set one
/// of these — see [`apply_extra_headers`].
const AUTH_HEADER_NAMES: [&str; 3] = ["authorization", "x-api-key", "api-key"];

/// Latches after the first stripped auth header so the warning is emitted once
/// per process instead of once per request.
static AUTH_HEADER_STRIPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether `name` is an auth-type header name. Compared case-insensitively
/// because HTTP header names are: a provider config keyed `authorization` or
/// `X-API-Key` has to match the same way `Authorization` does.
fn is_auth_header_name(name: &str) -> bool {
    AUTH_HEADER_NAMES
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
}

/// Apply operator-configured extra headers to `req`, skipping auth-type names.
///
/// `RequestBuilder::header` **appends**, so an `Authorization`/`x-api-key`
/// arriving through `extra_headers` would ride on the wire *alongside* the real
/// credential — and which of two same-named headers a server or proxy honors is
/// undefined. Dropping the configured one leaves exactly one credential on the
/// request. Shared by every backend's request builder so the guarantee
/// can't drift between them.
pub(crate) fn apply_extra_headers(
    mut req: reqwest::RequestBuilder,
    extra_headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (k, v) in extra_headers {
        if is_auth_header_name(k) {
            // Removing configured headers silently is a debugging trap, so say
            // so once — through the event channel rather than stderr (a TUI may
            // own the terminal). The value is never logged: it's a credential.
            if !AUTH_HEADER_STRIPPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                set_client_warning(format!(
                    "ignoring `{k}` from this provider's extra headers: auth headers come \
                     from the configured credential, and sending two is ambiguous"
                ));
            }
            continue;
        }
        req = req.header(k, v);
    }
    req
}

impl Client {
    /// `base_url` should include the `/v1` suffix where the provider uses one,
    /// e.g. `http://localhost:8080/v1`.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let backend = detect_backend(&base_url);
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("reqwest client"),
            base_url,
            api_key,
            model: model.into(),
            temperature: None,
            cache: CacheMode::Off,
            cache_1h: false,
            effort: None,
            params: crate::RequestParams::default(),
            extra_headers: Vec::new(),
            prompt_cache_key: None,
            system_cache_split: None,
            api_version: None,
            max_attachment_bytes: None,
            backend,
            resolved_model: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Set the prompt-caching strategy (builder form).
    pub fn with_cache(mut self, cache: CacheMode) -> Self {
        self.cache = cache;
        self
    }

    /// Set the prompt-caching strategy (e.g. after a mid-session provider switch).
    pub fn set_cache(&mut self, cache: CacheMode) {
        self.cache = cache;
    }

    /// Use the extended 1-hour cache TTL (`true`) or the default ~5-minute
    /// ephemeral (`false`).
    pub fn set_cache_ttl_1h(&mut self, one_hour: bool) {
        self.cache_1h = one_hour;
    }

    /// Whether the extended 1-hour cache TTL is in force.
    ///
    /// Exposed for **pricing**, not for request building: a cache *write* is
    /// billed at 1.25x the input rate on the 5-minute TTL and 2x on the 1-hour
    /// one, so a caller estimating the cost of a call has to know which one this
    /// client asked for. The client is the only thing that does — the TTL is set
    /// per identity, alongside the cache mode, and can change on a `/model`
    /// switch — so reading it back from here is what keeps the estimate and the
    /// request describing the same call.
    pub fn cache_ttl_1h(&self) -> bool {
        self.cache_1h
    }

    /// Set OpenAI's `prompt_cache_key` — the routing hint that decides whether
    /// hrdr's long, highly repetitive prompt prefix actually *hits* OpenAI's
    /// prompt cache. `None` omits the field.
    ///
    /// Why this is not optional in practice: OpenAI combines this value with the
    /// prompt-prefix hash when picking a cache entry, and **on GPT-5.6 models
    /// setting it is mandatory for reliable cache matching**. Without it hrdr
    /// sends a prompt that is *eligible* for caching (the system prompt alone
    /// clears the 1024-token floor caching requires) and still misses, paying
    /// full uncached input price on every round of every turn.
    ///
    /// Why the value must be per-conversation — neither per-process nor
    /// per-request. OpenAI's guidance is to use the key *consistently across
    /// requests that share a long common prefix*, and to keep each key's traffic
    /// to roughly **15 requests per minute**. One key per agent lands exactly on
    /// that: every request in a conversation shares the same system prompt and a
    /// growing history, so they share a prefix and belong on one key; a
    /// process-wide constant would pool unrelated conversations (different
    /// prefixes, and busy sessions blow past the rpm guidance), while a
    /// per-request value shares a prefix with nothing and defeats the parameter
    /// entirely.
    ///
    /// Applied to the OpenAI chat-completions body (in [`Client::body_json`]) and
    /// the Responses body (in [`crate::codex::build_body`]). The native Anthropic
    /// backend never sees it: the Messages API has no such field, and Anthropic
    /// rejects unknown top-level parameters.
    pub fn set_prompt_cache_key(&mut self, key: Option<String>) {
        self.prompt_cache_key = key;
    }

    /// The `prompt_cache_key` currently in force — so a caller that reconfigures
    /// the client for a new identity can assert the key survived the switch
    /// (a dropped key is a silent, invisible cache miss, not an error).
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }

    /// Set the reasoning-effort label; only recognized levels
    /// ([`crate::normalize_effort`]) are actually sent.
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
    }

    /// Current reasoning-effort label, including display-only values.
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Set the opt-in request parameters (`max_tokens`, `top_p`, `seed`, `stop`,
    /// `include_usage`).
    pub fn set_params(&mut self, params: crate::RequestParams) {
        self.params = params;
    }

    /// Stop sending a parameter this endpoint rejected as unsupported (see
    /// [`crate::unsupported_param`]), so the retry — and every later request —
    /// omits it.
    ///
    /// The wire-name → field mapping lives here, next to the bodies that write
    /// those names ([`Client::body_json`], [`crate::codex::build_body`],
    /// [`crate::anthropic`]), rather than in the agent: a caller recovering from
    /// a rejection knows *that* a parameter was refused, and should not also
    /// have to know which of this struct's fields backs it.
    pub fn clear_unsupported_param(&mut self, param: crate::UnsupportedParam) {
        match param {
            crate::UnsupportedParam::MaxTokens => self.params.max_tokens = None,
            crate::UnsupportedParam::Temperature => self.temperature = None,
            crate::UnsupportedParam::TopP => self.params.top_p = None,
            crate::UnsupportedParam::PromptCacheKey => self.prompt_cache_key = None,
            crate::UnsupportedParam::ReasoningEffort => self.effort = None,
        }
    }

    /// The opt-in request parameters currently in force.
    ///
    /// Exists so a caller can make a **scoped** change — one out-of-band request
    /// with a different output cap — and put back exactly what it found, rather
    /// than reconstructing the session's parameters from a config it may no
    /// longer have (see `hrdr_agent`'s summarization call).
    pub fn params(&self) -> &crate::RequestParams {
        &self.params
    }

    /// Rebuild the HTTP client with a connect + per-chunk read timeout (so a
    /// hung or stalled provider fails the request instead of blocking forever).
    /// `None` sets a 300-second connect and per-chunk read timeout. (This differs
    /// from [`Client::new`], which uses an overall request deadline; the
    /// per-phase timeouts are better suited to streaming responses.) A build
    /// error keeps the current client.
    pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) {
        let mut builder = reqwest::Client::builder();
        let dur = timeout.unwrap_or(std::time::Duration::from_secs(300));
        builder = builder.connect_timeout(dur).read_timeout(dur);
        if let Ok(http) = builder.build() {
            self.http = http;
        }
    }

    /// Set the provider-configured extra headers sent with every request.
    pub fn set_headers(&mut self, headers: Vec<(String, String)>) {
        self.extra_headers = headers;
    }

    /// Whether an extra header with `name` (case-insensitive) is currently set.
    pub fn extra_headers_contains(&self, name: &str) -> bool {
        self.extra_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// Whether a credential (API key, or an injected OAuth bearer) is currently
    /// set. Returns only the presence bit — never the secret — so a caller can
    /// assert a credential was cleared without being able to read it.
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    /// Set the Azure OpenAI API version (enables the Azure URL + `api-key` auth
    /// quirks); `None` for a standard OpenAI-compatible endpoint.
    /// Byte offset in the system prompt where the cache-stable prefix ends.
    ///
    /// The caller assembles the prompt least-volatile-first, so everything below
    /// this offset repeats across runs while the tail (working directory, date)
    /// does not. Used only by the native Anthropic path, which turns it into a
    /// second `cache_control` breakpoint; other backends ignore it.
    pub fn set_system_cache_split(&mut self, at: Option<usize>) {
        self.system_cache_split = at;
    }

    /// The cache-split offset currently in force — so a caller that rebuilds the
    /// system prompt can assert the boundary it installed describes the text it
    /// installed (an offset from a *different* build closes the breakpoint in the
    /// wrong place, silently losing the prefix cache hit).
    pub fn system_cache_split(&self) -> Option<usize> {
        self.system_cache_split
    }

    pub fn set_api_version(&mut self, api_version: Option<String>) {
        self.api_version = api_version;
    }

    /// Set the per-attachment size ceiling in encoded bytes (the user's
    /// `max_attachment_bytes`), or `None` to use each type's provider default.
    /// Enforced in [`Client::check_attachments`], ahead of every request.
    pub fn set_max_attachment_bytes(&mut self, max: Option<usize>) {
        self.max_attachment_bytes = max;
    }

    /// The per-attachment ceiling currently in force — so a caller that
    /// configured one can assert the value it installed is the value the gate
    /// will use (a dropped cap is invisible until a request the user meant to
    /// refuse goes out).
    pub fn max_attachment_bytes(&self) -> Option<usize> {
        self.max_attachment_bytes
    }

    /// Build a request URL for `path` (e.g. `chat/completions`), appending the
    /// Azure `?api-version=` query when configured.
    fn url(&self, path: &str) -> String {
        match &self.api_version {
            Some(v) => format!("{}/{path}?api-version={v}", self.base_url),
            None => format!("{}/{path}", self.base_url),
        }
    }

    /// The current endpoint base URL (including the `/v1` suffix where the
    /// provider uses one).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// How this endpoint prices an image
    /// ([`crate::media::Attachment::estimated_tokens`]).
    ///
    /// Reads the backend already detected for the wire format rather than
    /// re-deciding from the URL, so a request's shape and its cost estimate can
    /// never disagree about which provider they are talking to — including
    /// through [`Client::set_backend_for_test`], where the URL says nothing.
    pub fn token_target(&self) -> crate::media::TokenTarget {
        match self.backend {
            Backend::Anthropic => crate::media::TokenTarget::Anthropic,
            Backend::OpenAi | Backend::Codex => crate::media::TokenTarget::OpenAi,
        }
    }

    /// Repoint the client at a different endpoint (for mid-session provider switch).
    pub fn set_base_url(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self.backend = detect_backend(&self.base_url);
        // "Whatever this endpoint serves" is a different answer at a different
        // endpoint. A stale one would send the previous server's model id to
        // the new one — the exact 404 this resolution exists to avoid.
        *self
            .resolved_model
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Point this client at `backend` regardless of what [`detect_backend`] says
    /// about its `base_url`.
    ///
    /// Test-only, and the only way the native backends can be exercised at all:
    /// detection keys on the **host**, so a mock server bound to `127.0.0.1` is
    /// always [`Backend::OpenAi`] and [`Client::chat_stream`] never dispatches to
    /// [`crate::anthropic`] / [`crate::codex`]. Everything those two do *after*
    /// their event loop — the thinking-block and reasoning-item flushes, the
    /// missing-terminator truncation errors — is unreachable without this, which
    /// is why it is not dead code even though nothing in the crate proper calls
    /// it. It mutates one client instance and touches no shared state, so
    /// parallel tests cannot race through it.
    ///
    /// Used by the library's unit tests and by the
    /// `tests/wire_log_native_backends.rs` integration binary — which needs it
    /// precisely because the wire log's env var forces the tests there, while
    /// host-keyed detection would otherwise route a `127.0.0.1` mock to
    /// [`Backend::OpenAi`].
    #[doc(hidden)]
    pub fn set_backend_for_test(&mut self, backend: Backend) {
        self.backend = backend;
    }

    /// Whether this endpoint is one that reads OpenAI's `prompt_cache_key`:
    /// OpenAI itself, an Azure OpenAI deployment, or the ChatGPT/Codex backend.
    ///
    /// An allowlist, deliberately. Anything else is either an
    /// OpenAI-*compatible* server that self-hosts its own prefix cache — which
    /// may perfectly well be a vLLM box behind private DNS rather than on
    /// `localhost` — or a gateway that has no reason to understand the field.
    fn consumes_prompt_cache_key(&self) -> bool {
        if self.backend == Backend::Codex || self.api_version.is_some() {
            return true;
        }
        let host = url_host(&self.base_url);
        host == "api.openai.com" || host.ends_with(".openai.com")
    }

    /// The model id to put on the wire, resolving [`UNNAMED_MODEL`] against the
    /// endpoint the first time it is needed.
    ///
    /// The sentinel means "whatever this server serves", and `/v1/models` is
    /// where a server says what that is — llama.cpp returns exactly one entry
    /// (its `id` the gguf path, which its own router then accepts), vLLM returns
    /// its `--served-model-name`. Asking is what makes hrdr work against a stock
    /// vLLM, which validates `model` and 404s anything it doesn't serve.
    ///
    /// Only a **single-model** listing is adopted. A server offering several has
    /// no "the" model, and picking the first would be the same guess that made
    /// the context-window probe adopt a stranger's window (see
    /// [`Client::context_from_models`]). Then, and whenever the endpoint can't be
    /// reached, the field is omitted entirely instead: vLLM's `model` is nullable
    /// and falls back to its served model, and llama.cpp ignores it — both
    /// strictly better than a placeholder no one knows.
    ///
    /// One request per endpoint, cached; a named model never asks at all.
    async fn wire_model(&self) -> Option<String> {
        if self.model != UNNAMED_MODEL {
            return Some(self.model.clone());
        }
        if let Some(cached) = self
            .resolved_model
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return cached;
        }
        let ids = self.list_models().await.unwrap_or_default();
        let resolved = match ids.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        };
        *self
            .resolved_model
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(resolved.clone());
        resolved
    }

    /// Replace the API key (or clear it with `None`).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.api_key = api_key;
    }

    fn request(
        &self,
        model: Option<String>,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        stream: bool,
    ) -> ChatRequest {
        // OpenAI reasoning models want `max_completion_tokens`, not `max_tokens`.
        // Route on the resolved wire model, not `self.model`: the configured id
        // (possibly the `UNNAMED_MODEL` sentinel) is not what `wire_model`
        // resolved and put on the wire.
        let (max_tokens, max_completion_tokens) = match self.params.max_tokens {
            Some(n) if model.as_deref().is_some_and(uses_max_completion_tokens) => (None, Some(n)),
            other => (other, None),
        };
        ChatRequest {
            // Already resolved by `wire_model`: the configured id, the one the
            // endpoint named for the sentinel, or nothing at all.
            model,
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: self.temperature,
            reasoning_effort: self.effort.as_deref().and_then(crate::normalize_effort),
            max_tokens,
            max_completion_tokens,
            top_p: self.params.top_p,
            seed: self.params.seed,
            stop: self.params.stop.clone(),
            stream,
            // Ask for token usage on streamed turns (for the live loader stats),
            // unless a strict server rejects `stream_options`.
            stream_options: (stream && self.params.include_usage).then_some(
                crate::types::StreamOptions {
                    include_usage: true,
                },
            ),
        }
    }

    fn post_bytes(&self, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.auth(
            self.http
                .post(self.url("chat/completions"))
                // `.json()` set this implicitly; `.body(bytes)` sets no
                // Content-Type at all, and OpenAI-compatible endpoints 415 a
                // JSON body without it.
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
        )
    }

    /// Apply the backend's auth + any provider-configured extra headers to a
    /// request builder: `x-api-key` + `anthropic-version` for the native
    /// Anthropic backend, else `Bearer`.
    fn auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // Apply operator-configured extra headers first so the auth header
        // (applied next) always wins — avoids ambiguity from header ordering.
        req = apply_extra_headers(req, &self.extra_headers);
        if let Some(key) = &self.api_key {
            req = match self.backend {
                Backend::Anthropic => req
                    .header("x-api-key", key)
                    .header("anthropic-version", crate::anthropic::API_VERSION),
                // Azure OpenAI authenticates with an `api-key` header, not Bearer.
                Backend::OpenAi if self.api_version.is_some() => req.header("api-key", key),
                // Codex (Responses API) uses the same Bearer auth; the streaming
                // path builds its own request (see `crate::codex::chat_stream`),
                // this only covers the best-effort `/models` + `/props` GETs.
                Backend::OpenAi | Backend::Codex => req.bearer_auth(key),
            };
        }
        req
    }

    /// The `max_tokens` to send on the native Anthropic backend when the user
    /// configured none. The Messages API makes the field mandatory, so this is
    /// the model's real output cap or nothing — a fixed default either truncates
    /// long answers (and, on the manual-thinking path, starves the answer of the
    /// room the budget scales out of) or 400s for exceeding the model's cap.
    ///
    /// Asked with `provider: None` on purpose: `Client` only knows a base URL
    /// and a model id, never hrdr's provider name — and even that name would be
    /// the wrong key, since models.dev's provider namespace is a different one
    /// (hrdr's `zen` is models.dev's `opencode`). `None` selects the catalog's
    /// cross-provider scan, which takes the *smallest* cap on offer; overstating
    /// `max_tokens` past a model's real cap is a 400, so low is the safe miss.
    /// Cache-only: this runs while building a request inside a live turn, where
    /// an out-of-band fetch would interleave with the stream about to open.
    fn anthropic_max_tokens(&self) -> u32 {
        crate::catalog::max_output_cached(None, &self.model).unwrap_or(ANTHROPIC_MAX_TOKENS)
    }

    /// Whether the OpenAI request body needs a post-serialization graft:
    /// cache breakpoints (Ephemeral mode), the `prompt_cache_key` routing hint,
    /// the DeepSeek `reasoning_content` replay (see
    /// [`replays_reasoning_content`]), or attachments to render into
    /// content parts (which is also what carries OpenRouter's PDF-parser
    /// selection). With none of these the body is the request serialized
    /// as-is — which is what `chat_stream`'s fast path sends, skipping the
    /// `serde_json::Value` tree entirely. The single source of truth for both
    /// [`Self::body_json`] and that fast path.
    fn grafts_needed(&self, messages: &[ChatMessage]) -> bool {
        self.cache == CacheMode::Ephemeral
            || (self.prompt_cache_key.is_some() && self.consumes_prompt_cache_key())
            || replays_reasoning_content(&self.base_url, &self.model)
            || messages.iter().any(|m| !m.attachments.is_empty())
    }

    /// Serialize a request and apply cache breakpoints per the active [`CacheMode`].
    fn body_json(&self, body: &ChatRequest) -> serde_json::Value {
        let mut json = serde_json::to_value(body).unwrap_or_default();
        if !self.grafts_needed(&body.messages) {
            return json;
        }
        if self.cache == CacheMode::Ephemeral {
            crate::types::apply_cache_breakpoints(
                &mut json,
                self.cache_1h,
                self.system_cache_split,
            );
        }
        // `prompt_cache_key` is grafted onto the serialized body rather than
        // carried as a `ChatRequest` field, for the same reason the cache
        // breakpoints above are: it is an OpenAI-shape-only parameter, and
        // `ChatRequest` is the shared struct every backend serializes from.
        // Skipped entirely when unset, so an OpenAI-compatible server that has
        // never heard of the field sees no change. See
        // [`Client::set_prompt_cache_key`] for why it is set at all.
        //
        // Sent only to the endpoints that actually READ it — an allowlist, not
        // "everything that isn't localhost". A self-hosted llama.cpp, vLLM or
        // Ollama is just as likely to sit behind real DNS on another machine as
        // on `localhost`, and none of them consume this field: they cache by
        // prompt prefix inside the one server already (llama.cpp ignores unknown
        // keys, vLLM logs them as ignored). Keying on the host would have sent
        // it to exactly those servers whenever they weren't local.
        if let Some(key) = &self.prompt_cache_key
            && self.consumes_prompt_cache_key()
            && let Some(obj) = json.as_object_mut()
        {
            obj.insert("prompt_cache_key".to_string(), serde_json::json!(key));
        }
        // DeepSeek's `reasoning_content` replay, for the endpoints and models
        // that 400 without it — see [`replays_reasoning_content`].
        // `ChatMessage.reasoning_content` is `skip_serializing` for every other
        // backend, so the graft is the only route, and it happens here keyed by
        // index off the ORIGINAL messages (which still hold the field). Only an
        // assistant turn that really produced reasoning grows the field, and
        // only for a DeepSeek model or host, so an OpenAI-compatible server
        // that rejects unknown message fields sees the unchanged body.
        if replays_reasoning_content(&self.base_url, &self.model)
            && let Some(messages) = json
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
        {
            for (msg, original) in messages.iter_mut().zip(&body.messages) {
                if original.role == Role::Assistant
                    && let Some(reasoning) = &original.reasoning_content
                    && let Some(obj) = msg.as_object_mut()
                {
                    obj.insert(
                        "reasoning_content".to_string(),
                        serde_json::json!(reasoning),
                    );
                }
            }
        }
        graft_attachments(&mut json, &body.messages);
        // OpenRouter's PDF parser selection, once a PDF is actually on the wire
        // — a top-level field, so unlike the attachment parts above it neither
        // reads nor disturbs what the cache breakpoints did to `messages`. See
        // [`openrouter_pdf_plugins`] for why it is conditional on the model.
        if is_openrouter(&self.base_url)
            && body
                .messages
                .iter()
                .flat_map(|m| &m.attachments)
                .any(|a| a.media_type() == crate::media::MediaType::Pdf)
        {
            // Cache-only, for the reason [`Self::check_attachments`] gives: this
            // runs inside a live turn, where an out-of-band catalog fetch would
            // interleave with the stream about to open.
            let accepts = crate::catalog::input_modalities_cached(None, &self.model);
            if let Some(plugins) = openrouter_pdf_plugins(accepts.as_deref())
                && let Some(obj) = json.as_object_mut()
            {
                obj.insert("plugins".to_string(), plugins);
            }
        }
        json
    }

    /// Refuse a request whose attachments the model cannot take, or that are
    /// over the configured per-attachment ceiling, before it goes out. See
    /// [`crate::media::check_attachments`] — including why the size cap is
    /// enforced here rather than at construction, and why a model the catalog
    /// has never heard of is allowed through.
    ///
    /// Reported as a [`ChatError`] with [`ChatErrorKind::Other`] rather than a
    /// bare error: that is the shape hrdr-agent already classifies, and `Other`
    /// is terminal — no amount of retrying makes a text-only model see a
    /// picture.
    fn check_attachments(&self, messages: &[ChatMessage]) -> Result<()> {
        // Cache-only, deliberately: this runs while building a request inside a
        // live turn, where an out-of-band catalog fetch would interleave with
        // the stream about to open. `None` (no catalog, or no entry) is the
        // allow case.
        let accepts = crate::catalog::input_modalities_cached(None, &self.model);
        crate::media::check_attachments(
            &self.model,
            accepts.as_deref(),
            messages,
            self.max_attachment_bytes,
        )
        .map_err(|e| {
            anyhow::Error::new(ChatError {
                status: None,
                retry_after: None,
                kind: ChatErrorKind::Other,
                message: e.to_string(),
            })
        })
    }

    /// Streaming completion. Yields decoded chunks as they arrive. Dispatches to
    /// the native Anthropic Messages API or the OpenAI chat-completions shape
    /// based on the detected [`Backend`].
    ///
    /// Takes slices to avoid cloning the full history on every retry. The
    /// request body is serialized before any network I/O, so the borrow does
    /// not extend into the returned [`ChatStream`] future.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<ChatStream> {
        // Before anything is built or sent, and ahead of the backend split so
        // all three dialects answer identically.
        self.check_attachments(messages)?;
        // The native backends build, log, and send their own requests — see the
        // `log_wire` calls in `crate::anthropic` / `crate::codex`. Logging here
        // instead would only ever record a request that already succeeded.
        if self.backend == Backend::Anthropic {
            return crate::anthropic::chat_stream(
                &self.http,
                &self.base_url,
                self.api_key.as_deref(),
                &self.model,
                self.params
                    .max_tokens
                    .unwrap_or_else(|| self.anthropic_max_tokens()),
                self.effort.as_deref(),
                self.temperature,
                self.params.top_p,
                &self.params.stop,
                // `self.params.seed` is intentionally not passed: the native
                // Messages API has no determinism-seed equivalent.
                self.cache,
                self.cache_1h,
                &self.extra_headers,
                // `self.prompt_cache_key` is intentionally not passed: it is an
                // OpenAI parameter. The Messages API has no equivalent (its
                // caching is explicit, via the `cache_control` breakpoints the
                // `cache` argument above drives) and rejects unknown top-level
                // fields, so sending it here would be a 400.
                self.system_cache_split,
                messages,
                tools,
            )
            .await;
        }
        if self.backend == Backend::Codex {
            return crate::codex::chat_stream(
                &self.http,
                &self.base_url,
                self.api_key.as_deref(),
                &self.model,
                self.effort.as_deref(),
                self.temperature,
                self.params.top_p,
                self.params.max_tokens,
                // The Responses API takes the same `prompt_cache_key` as
                // chat-completions, at the top level — see
                // `Client::set_prompt_cache_key`.
                self.prompt_cache_key.as_deref(),
                // `ChatGPT-Account-Id` rides here (set via `set_headers`);
                // `originator: hrdr` + `Authorization: Bearer` are added inside.
                &self.extra_headers,
                messages,
                tools,
            )
            .await;
        }
        // Resolve the model before serializing: on the "whatever you serve"
        // sentinel this asks the endpoint once, so vLLM gets the name it
        // validates against instead of a placeholder it 404s.
        let model = self.wire_model().await;
        let request = self.request(model, messages, tools, true);
        // Fast path: `body_json` builds a `serde_json::Value` tree that reqwest
        // would serialize again. With no graft to apply (default cache mode, no
        // prompt-cache key, not DeepSeek) the tree is pure intermediate — the
        // request serializes straight to bytes, byte-identical to the Value
        // path's output (both are the same Serialize impl, compact).
        let body = if self.grafts_needed(&request.messages) {
            let json = self.body_json(&request);
            log_wire("request", || {
                serde_json::json!({
                    "url": self.url("chat/completions"),
                    "body": json,
                })
            });
            serde_json::to_vec(&json).context("serializing request")?
        } else {
            let bytes = serde_json::to_vec(&request).context("serializing request")?;
            log_wire("request", || {
                // The wire log wants a Value; re-parse the bytes only when the
                // log is actually live (it is off by default).
                serde_json::json!({
                    "url": self.url("chat/completions"),
                    "body": serde_json::from_slice::<serde_json::Value>(&bytes)
                        .unwrap_or_default(),
                })
            });
            bytes
        };
        let resp = self
            .post_bytes(body)
            .send()
            .await
            .context("chat stream request failed")?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            // Feed raw byte chunks into the SSE decoder, which buffers per-line
            // and yields complete events on blank-line terminators.  Splitting
            // on 0x0A is safe for UTF-8: the byte never appears inside a
            // multi-byte sequence, so a codepoint split across chunks is
            // buffered whole and decoded only when its line is complete.
            let mut decoder = SseDecoder::new();
            loop {
                // On EOF, `finish()` flushes a final `data:` line that had no
                // blank-line terminator (lenient servers end with `data: [DONE]\n`
                // rather than a spec `\n\n`), so the sentinel isn't lost.
                let (events, at_eof) = match bytes.next().await {
                    Some(chunk) => {
                        // A transport error mid-body (connection reset, WiFi blip)
                        // is safe to retry — the reply was partial. Type it as
                        // Transient so the agent retry loop catches it; an untyped
                        // anyhow error would print only "reading stream chunk" and
                        // slip past the classifier.
                        let bytes = chunk.map_err(|e| ChatError {
                            status: None,
                            retry_after: None,
                            kind: ChatErrorKind::Transient,
                            message: format!(
                                "incomplete stream: transport error mid-response \
                                 ({e}) (partial response, safe to retry)"
                            ),
                        })?;
                        if decoder.push(&bytes).is_err() {
                            let _ = decoder.drain(); // discard truncated events
                            Err(ChatError {
                                status: None,
                                retry_after: None,
                                kind: ChatErrorKind::Other,
                                message: SseOverflow.to_string(),
                            })?;
                        }
                        (decoder.drain(), false)
                    }
                    None => {
                        // If overflow was flagged during the stream, the final
                        // events may be truncated — never parse them.
                        let events = match decoder.finish() {
                            Ok(events) => events,
                            Err(_) => Err(ChatError {
                                status: None,
                                retry_after: None,
                                kind: ChatErrorKind::Other,
                                message: SseOverflow.to_string(),
                            })?,
                        };
                        (events, true)
                    }
                };
                for ev in events {
                    let data = ev.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    log_wire("sse", || serde_json::json!({"data": data}));
                    if data == "[DONE]" {
                        return;
                    }
                    // A mid-stream error object (`{"error":{"message":"..."}}`) would
                    // otherwise deserialize as an empty `ChatChunk` (every field is
                    // `#[serde(default)]`), silently swallowing the server's real
                    // error and letting the stream fall through to the generic
                    // "incomplete stream" retryable classification below. Surface it
                    // here instead, as a terminal (non-retryable) error carrying the
                    // server's message — mirrors the native Anthropic `"error"` event
                    // handling in `anthropic::map_event`. The `contains("\"error\"")`
                    // pre-check cannot miss a real error object (any `{"error": …}`
                    // payload contains that literal); a false positive in a content
                    // delta just takes this slower path with identical results, so
                    // the common case parses `data` straight into `ChatChunk`.
                    if data.contains("\"error\"") {
                        let value: serde_json::Value = serde_json::from_str(data)
                            .with_context(|| format!("decoding stream event: {data}"))?;
                        if let Some(err_obj) = value.get("error").filter(|e| !e.is_null()) {
                            let msg = err_obj
                                .get("message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown error");
                            // Gateways (OpenRouter, LiteLLM, …) deliver rate-limit and
                            // overload conditions as mid-stream error objects. Classify
                            // them Transient by code/type so the retry loop catches
                            // them, matching the native Anthropic path.
                            let code = err_obj
                                .get("code")
                                .and_then(|c| c.as_u64())
                                .map(|c| c as u16)
                                .or_else(|| {
                                    err_obj.get("status").and_then(|c| c.as_u64()).map(|c| c as u16)
                                });
                            let type_str = err_obj
                                .get("type")
                                .or_else(|| err_obj.get("code"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            let transient = code.map(classify_status)
                                == Some(ChatErrorKind::Transient)
                                || type_str.contains("rate_limit")
                                || type_str.contains("overloaded")
                                || type_str.contains("server_error");
                            // A spent quota/billing cap is terminal, whatever the
                            // embedded code says — `insufficient_quota` or a quota
                            // message must not ride the `code == 429` transient path.
                            let kind = if crate::retry::is_usage_limit_text(&format!("{type_str} {msg}")) {
                                ChatErrorKind::UsageLimit
                            } else if transient {
                                ChatErrorKind::Transient
                            } else {
                                ChatErrorKind::Other
                            };
                            Err(ChatError {
                                status: None,
                                retry_after: None,
                                kind,
                                message: format!("mid-stream error: {msg}"),
                            })?;
                        }
                        let parsed: ChatChunk = serde_json::from_value(value)
                            .with_context(|| format!("decoding stream event: {data}"))?;
                        yield parsed;
                    } else {
                        let parsed: ChatChunk = serde_json::from_str(data)
                            .with_context(|| format!("decoding stream event: {data}"))?;
                        yield parsed;
                    }
                }
                if at_eof {
                    break;
                }
            }
            // Reaching here means the byte stream closed without the [DONE]
            // sentinel — truncated response or network drop. Classify as
            // transient so the agent retry loop can re-request.
            Err(ChatError {
                status: None,
                retry_after: None,
                kind: ChatErrorKind::Transient,
                message: "incomplete stream: OpenAI stream ended without [DONE] \
                          (partial response, safe to retry)"
                    .to_string(),
            })?;
        };
        Ok(Box::pin(stream))
    }

    /// List available models from `GET {base_url}/models`.
    /// Returns model ids sorted alphabetically.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let req = self.auth(self.http.get(self.url("models")));
        let resp = req.send().await.context("models request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = crate::capped_read::read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
            bail!("models endpoint returned {status}: {text}");
        }
        let parsed: ModelsResponse =
            crate::capped_read::read_capped_json(resp, MAX_STRUCTURED_JSON_BYTES)
                .await
                .context("decoding models response")?;
        let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Best-effort probe of the server's context window in tokens (for the status
    /// bar's "X of Y" + the auto-compaction threshold). This is **not** part of
    /// the OpenAI spec, but many OpenAI-compatible servers advertise it:
    ///
    /// - on the `/v1/models` entry as a non-standard field — vLLM's
    ///   `max_model_len`, LM Studio's `max_context_length`, and similar;
    /// - or, for llama.cpp, via `GET /props`
    ///   (`default_generation_settings.n_ctx`).
    ///
    /// Returns `None` when nothing exposes it (e.g. OpenAI itself, or infr
    /// today), so the caller can fall back to a configured/default value.
    ///
    /// # Known gap: Ollama
    ///
    /// Ollama's OpenAI-compatible surface publishes no window anywhere this
    /// probe can see it. Its `/v1/models` entries are `{id, object, created,
    /// owned_by}` and nothing more (`openai.go`'s `Model` struct), and it serves
    /// no `/props`, so both branches come back empty and an Ollama user has to
    /// set `context_window` in config.
    ///
    /// Deliberately not worked around here. Reaching the number means leaving
    /// the OpenAI shape entirely — `POST /api/show`, reading
    /// `model_info["<arch>.context_length"]` — and even that answers the wrong
    /// question: it reports what the MODEL supports, while what a request
    /// actually gets is Ollama's own `num_ctx`, which defaults far below the
    /// model's maximum unless the user raises it. A probe that confidently
    /// returned the model's ceiling would overstate the real window, which is
    /// the direction that overflows rather than compacts (the same trap as
    /// llama.cpp's `n_ctx_train` — see [`context_field`]). An explicitly
    /// configured value is both more honest and more correct.
    pub async fn context_window(&self) -> Option<u32> {
        if let Some(n) = self.context_from_models().await {
            return Some(n);
        }
        self.context_from_props().await
    }

    /// Look for a context-length field on this client's model in `/v1/models`.
    ///
    /// The no-match fallback applies **only when the server lists exactly one
    /// model** — the local-server case it was written for (llama.cpp / vLLM
    /// serving a single model under a name the user's config may not spell the
    /// same way). On a multi-model list it is actively harmful: OpenRouter's
    /// `/v1/models` returns hundreds of entries whose first is an unrelated
    /// 1M-context model, so any id typo, alias, or variant suffix would silently
    /// adopt a 1M window — and because this probe outranks the models.dev
    /// catalog, the agent would then never compact and overflow instead. Return
    /// `None` there and let the catalog answer.
    async fn context_from_models(&self) -> Option<u32> {
        let v = self.get_json(&self.url("models")).await?;
        let data = v.get("data")?.as_array()?;
        let entry = match data
            .iter()
            .find(|e| e.get("id").and_then(|i| i.as_str()) == Some(self.model.as_str()))
        {
            Some(e) => e,
            None if data.len() == 1 => data.first()?,
            None => return None,
        };
        context_field(entry)
    }

    /// llama.cpp exposes the loaded context via `GET /props` (served at the root,
    /// not under `/v1`), either top-level or under `default_generation_settings`.
    ///
    /// `GET /props` is available by default — the `--props` flag only unlocks the
    /// POST form — so this works against a stock `llama-server`.
    ///
    /// The number it reports is the **per-slot** context: a server started
    /// `--parallel 4 -c 32768` gives each concurrent request 8192, and 8192 is
    /// what lands here. That is the right figure for hrdr (one request occupies
    /// one slot), and it is why a user who raises `--parallel` sees the context
    /// gauge shrink by the same factor.
    async fn context_from_props(&self) -> Option<u32> {
        let root = self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url);
        let v = self.get_json(&format!("{root}/props")).await?;
        context_field(&v).or_else(|| v.get("default_generation_settings").and_then(context_field))
    }

    /// GET `url` with the backend's auth and decode JSON; `None` on any error
    /// (unreachable endpoint, non-2xx, or unparseable body) — detection is
    /// best-effort and never fails the caller.
    async fn get_json(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self.auth(self.http.get(url)).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<serde_json::Value>().await.ok()
    }
}

/// Rewrite `content` into a content-parts array for every message carrying
/// attachments, and leave every other message untouched.
///
/// A post-serialization graft, for the same reason the DeepSeek
/// `reasoning_content` replay above is one: `ChatMessage.attachments` is
/// `skip_serializing`, so the derived body never mentions it, and each dialect
/// renders it itself. Keyed by index off the ORIGINAL messages — which still
/// hold the attachments — exactly as that graft is.
///
/// Runs **after** [`crate::types::apply_cache_breakpoints`], and handles the
/// array `content` that leaves behind by prepending to it rather than replacing
/// it: the breakpoint has already been placed on the text part, and rebuilding
/// the array would silently drop it. Attachments go first, matching the other
/// two dialects.
fn graft_attachments(json: &mut serde_json::Value, originals: &[ChatMessage]) {
    let Some(messages) = json
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for (msg, original) in messages.iter_mut().zip(originals) {
        if original.attachments.is_empty() {
            continue;
        }
        let mut parts: Vec<serde_json::Value> = original
            .attachments
            .iter()
            .map(crate::media::Attachment::openai_part)
            .collect();
        match msg.get_mut("content") {
            // Already a parts array (a cache breakpoint landed here): keep it.
            Some(serde_json::Value::Array(existing)) => parts.append(existing),
            // The ordinary case: a plain string becomes a `text` part.
            Some(serde_json::Value::String(text)) => parts.push(serde_json::json!({
                "type": "text",
                "text": std::mem::take(text),
            })),
            // Absent or null — an attachment-only message. The parts array is
            // the whole content.
            _ => {}
        }
        msg["content"] = serde_json::Value::Array(parts);
    }
}

// --- /v1/models response types (local to this module) ---

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Pull a context-window value from a JSON object, trying the field names the
/// various OpenAI-compatible servers use. Accepts a number or a numeric string;
/// ignores non-positive values.
///
/// Note what is *not* here, in both directions:
///
/// * Anthropic's `/v1/models` publishes both `max_input_tokens` (the window,
///   listed below) and `max_tokens` (the largest value the `max_tokens` request
///   param may take). Reading the latter as a window would understate it by an
///   order of magnitude, so only the former is a key.
/// * llama.cpp's `/v1/models` publishes `meta.n_ctx_train` — the context the
///   model was **trained** at, not the one the server was started with. It is
///   deliberately not read (nor is `meta` descended into): a server run with
///   `-c 8192` on a 131072-train model would otherwise advertise 131072, and the
///   agent would fill a window four times larger than the one that exists. The
///   loaded figure comes from `/props` instead — see
///   [`Client::context_from_props`]. Do not "fix" this omission.
fn context_field(v: &serde_json::Value) -> Option<u32> {
    const KEYS: &[&str] = &[
        "max_model_len",      // vLLM
        "max_context_length", // LM Studio et al.
        "context_length",     // Ollama-style model_info; OpenRouter
        "max_input_tokens",   // Anthropic's own /v1/models
        "context_window",     // generic
        "n_ctx",              // llama.cpp
        "context_size",
        "max_context",
    ];
    let find = |obj: &serde_json::Value| {
        KEYS.iter()
            .find_map(|k| obj.get(k).and_then(json_u32).filter(|n| *n > 0))
    };
    // OpenRouter nests a second copy of the window under `top_provider`
    // (`top_provider.context_length`), which is the one that survives when the
    // top-level field is absent for a given entry.
    find(v).or_else(|| v.get("top_provider").and_then(find))
}

/// Read a `u32` from a JSON number or numeric string.
fn json_u32(v: &serde_json::Value) -> Option<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Minimal in-process HTTP server used to exercise the SSE decoding and error
/// paths in the `chat_stream` implementations (mirrors the mock server in
/// hrdr-agent's test module, trimmed to a single canned response).
///
/// Lives outside `mod tests` because [`crate::anthropic`] and [`crate::codex`]
/// drive their own streams through it too — the path is ignored, so the same
/// server answers `/v1/chat/completions`, `/v1/messages` and `/v1/responses`.
/// The response carries no `Content-Length` and closes the connection, so the
/// client reads to EOF: a body that stops short of its terminator is exactly a
/// truncated stream.
///
/// `status_line` is the raw HTTP status line (`"HTTP/1.1 200 OK"`, or a non-2xx
/// like `"HTTP/1.1 401 Unauthorized"`), so the same server can serve a healthy
/// SSE stream or the error body a provider sends for a rejected request. The
/// `Content-Type` stays `text/event-stream` either way — the native code paths
/// read the body as text regardless of status.
///
/// Public (`#[doc(hidden)]`) for the `tests/wire_log_native_backends.rs`
/// integration binary, which drives the native backends through a `127.0.0.1`
/// mock to assert the wire log's `sse` and `error_response` records.
#[doc(hidden)]
pub async fn serve_response(status_line: &'static str, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        // Read (and discard) the request headers + body; we don't care
        // about the request shape for this test.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let headers_end = loop {
            match stream.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                }
            }
        };
        let hdrs = String::from_utf8_lossy(&buf[..headers_end]);
        let content_len: usize = hdrs
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let body_so_far = buf.len().saturating_sub(headers_end);
        let remaining = content_len.saturating_sub(body_so_far);
        if remaining > 0 {
            let mut body_buf = vec![0u8; remaining];
            let _ = stream.read_exact(&mut body_buf).await;
        }
        let resp = format!(
            "{status_line}\r\n\
             Content-Type: text/event-stream\r\n\
             Connection: close\r\n\
             \r\n\
             {body}"
        );
        let _ = stream.write_all(resp.as_bytes()).await;
    });
    format!("http://127.0.0.1:{port}/v1")
}

/// Serve one canned `200 OK` SSE body like [`serve_response`], for the
/// library's own unit tests.
#[cfg(test)]
pub(crate) async fn serve_once(body: &'static str) -> String {
    serve_response("HTTP/1.1 200 OK", body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The chat-completions body for `messages`, through the same path a real
    /// request takes.
    fn openai_body(messages: &[ChatMessage]) -> serde_json::Value {
        let client = Client::new("https://api.openai.com/v1", None, "gpt-5.6");
        client.body_json(&client.request(Some(client.model.clone()), messages, &[], true))
    }

    /// A message with attachments has its string `content` rewritten into a
    /// content-parts array, attachments first.
    #[test]
    fn attachments_become_content_parts_ahead_of_the_text() {
        use crate::media::tests::{pdf_attachment, png_attachment};
        let mut m = ChatMessage::user("what is in these");
        m.attachments = vec![png_attachment("a.png"), pdf_attachment("b.pdf")];
        let body = openai_body(&[m]);
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["detail"], "auto");
        assert!(
            parts[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"),
            "{body}"
        );
        assert_eq!(parts[1]["type"], "file");
        assert_eq!(parts[1]["file"]["filename"], "b.pdf");
        assert_eq!(
            parts[2],
            json!({ "type": "text", "text": "what is in these" })
        );
        // The role is untouched, and the graft never invents an `attachments`
        // key on the wire.
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(!body.to_string().contains("attachments"), "{body}");
    }

    /// An attachment-only message becomes a parts array with no `text` part —
    /// `content` was absent from the serialized message entirely.
    #[test]
    fn an_attachment_only_message_becomes_a_bare_parts_array() {
        use crate::media::tests::png_attachment;
        let mut m = ChatMessage::user("");
        m.content = None;
        m.attachments = vec![png_attachment("a.png"), png_attachment("b.png")];
        let body = openai_body(&[m]);
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "{body}");
        assert!(parts.iter().all(|p| p["type"] == "image_url"));
    }

    /// The attachment graft runs after the cache breakpoints and must not
    /// destroy one: a marked message's parts array is prepended to, not
    /// replaced.
    #[test]
    fn a_cache_breakpoint_survives_the_attachment_graft() {
        use crate::media::tests::png_attachment;
        let mut m = ChatMessage::user("look at this");
        m.attachments = vec![png_attachment("a.png")];
        let mut client = Client::new("https://api.anthropic.com/v1/openai", None, "claude");
        client.set_cache(CacheMode::Ephemeral);
        let body = client.body_json(&client.request(Some(client.model.clone()), &[m], &[], true));
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "{body}");
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(
            parts[1]["cache_control"],
            json!({ "type": "ephemeral" }),
            "the rolling breakpoint is still on the text part: {body}"
        );
    }

    /// The regression that matters most: with no attachments anywhere, the
    /// serialized body is exactly what it was before attachments existed —
    /// plain string `content`, no `attachments` key, and no graft taken.
    #[test]
    fn a_history_without_attachments_serializes_unchanged() {
        let messages = vec![
            ChatMessage::system("you are hrdr"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::tool_result("t1", "output"),
        ];
        let client = Client::new("https://api.openai.com/v1", None, "gpt-5.6");
        assert!(
            !client.grafts_needed(&messages),
            "the fast path is still taken"
        );
        let body = openai_body(&messages);
        assert_eq!(
            body["messages"],
            json!([
                { "role": "system", "content": "you are hrdr" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "tool", "content": "output", "tool_call_id": "t1" },
            ])
        );
        // The field must never reach the wire, empty or not — asserted on the
        // serialized JSON, not on the struct.
        assert!(!body.to_string().contains("attachments"), "{body}");

        // And an attachment anywhere in the history *does* take the graft path,
        // so the assertion above is about the absence of attachments and not
        // about `grafts_needed` never firing.
        let mut with = messages.clone();
        with[1].attachments = vec![crate::media::tests::png_attachment("a.png")];
        assert!(client.grafts_needed(&with));
    }

    /// OpenRouter's `plugins` array rides on the body of a request that carries
    /// a PDF — and on no other request, and to no other host.
    ///
    /// The model id is one no catalog can carry, which is the unknown-model case
    /// [`openrouter_pdf_plugins`] exists for: without the field OpenRouter falls
    /// back to `mistral-ocr` at $2 per 1,000 pages.
    #[test]
    fn the_openrouter_pdf_plugin_rides_only_on_an_openrouter_request_with_a_pdf() {
        use crate::media::tests::{pdf_attachment, png_attachment};
        let with_pdf = |name: &str| {
            let mut m = ChatMessage::user("read this");
            m.attachments = vec![pdf_attachment(name)];
            m
        };
        let body = |url: &str, messages: &[ChatMessage]| {
            let client = Client::new(url, None, "hrdr-test/no-such-model");
            client.body_json(&client.request(Some(client.model.clone()), messages, &[], true))
        };

        let sent = body("https://openrouter.ai/api/v1", &[with_pdf("spec.pdf")]);
        assert_eq!(
            sent["plugins"],
            json!([{ "id": "file-parser", "pdf": { "engine": "cloudflare-ai" } }]),
            "{sent}"
        );
        // Additive: the PDF itself still goes out as a content part.
        assert_eq!(sent["messages"][0]["content"][0]["type"], "file");

        // An image on the same host needs no parser.
        let mut image_only = ChatMessage::user("look");
        image_only.attachments = vec![png_attachment("a.png")];
        let images = body("https://openrouter.ai/api/v1", &[image_only]);
        assert!(images.get("plugins").is_none(), "{images}");

        // No attachments at all, and every other host: no `plugins` field. A
        // server that rejects unknown top-level keys must see the body it always
        // saw.
        let bare = body("https://openrouter.ai/api/v1", &[ChatMessage::user("hi")]);
        assert!(bare.get("plugins").is_none(), "{bare}");
        for url in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com",
            "http://localhost:8080/v1",
            // A lookalike host is not OpenRouter.
            "https://openrouter.ai.evil.com/v1",
        ] {
            let other = body(url, &[with_pdf("spec.pdf")]);
            assert!(other.get("plugins").is_none(), "{url}: {other}");
        }
    }

    /// Which engine the plugin names, per what the catalog says the model can
    /// read: a model listed with `pdf` keeps OpenRouter's native path (no field
    /// at all), anything else gets the free parser.
    #[test]
    fn the_openrouter_pdf_engine_follows_the_models_dev_modalities() {
        let modalities =
            |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_string()).collect() };
        let native = modalities(&["text", "image", "pdf"]);
        assert_eq!(openrouter_pdf_plugins(Some(&native)), None);

        for accepts in [
            Some(modalities(&["text"])),
            Some(modalities(&["text", "image"])),
        ] {
            assert_eq!(
                openrouter_pdf_plugins(accepts.as_deref()),
                Some(json!([{ "id": "file-parser", "pdf": { "engine": "cloudflare-ai" } }])),
                "a model that cannot read a PDF itself needs the parser named"
            );
        }
        // A model the catalog has never heard of is the same case: OpenRouter
        // would otherwise pick the billed engine.
        assert_eq!(
            openrouter_pdf_plugins(None),
            Some(json!([{ "id": "file-parser", "pdf": { "engine": "cloudflare-ai" } }]))
        );
    }

    /// The gate is wired into the send path for every backend, and reports a
    /// **terminal** [`ChatError`] rather than a bare one.
    ///
    /// Driven by a refusal that needs no catalog (an attachment on an assistant
    /// message), because the modality rules themselves are covered in
    /// [`crate::media`] — what is under test here is that `chat_stream` asks at
    /// all, ahead of the backend split, and how it reports the answer.
    #[tokio::test]
    async fn the_gate_refuses_before_any_backend_builds_a_request() {
        use crate::media::tests::png_attachment;
        let mut m = ChatMessage::assistant("here you go");
        m.attachments = vec![png_attachment("a.png")];

        // One per backend, all pointed at a port nobody is listening on: a
        // request that got past the gate would fail with a connection error
        // instead, which is what the `downcast` below distinguishes.
        for url in [
            "http://127.0.0.1:1/v1",
            "https://api.anthropic.com/v1",
            "https://chatgpt.com/backend-api/codex",
        ] {
            let client = Client::new(url, None, "some-model");
            let Err(err) = client.chat_stream(std::slice::from_ref(&m), &[]).await else {
                panic!("{url}: the gate must refuse an attachment on an assistant turn");
            };
            let chat: &ChatError = err
                .downcast_ref()
                .unwrap_or_else(|| panic!("{url}: expected a typed ChatError, got {err:#}"));
            assert_eq!(
                chat.kind,
                ChatErrorKind::Other,
                "{url}: terminal — retrying cannot help"
            );
            assert!(
                chat.message.contains("user message"),
                "{url}: {}",
                chat.message
            );
            assert_eq!(chat.status, None, "{url}: nothing was sent");
        }
    }

    /// The configured per-attachment ceiling is the number the send path
    /// actually enforces: the same client refuses an attachment it accepted a
    /// moment earlier, and the message names the configured value.
    #[tokio::test]
    async fn the_configured_attachment_cap_is_what_the_send_path_enforces() {
        use crate::media::tests::png_attachment;
        let mut m = ChatMessage::user("what is this");
        m.attachments = vec![png_attachment("a.png")];
        let encoded = m.attachments[0].encoded_len();

        // A closed port: past the gate this fails with a connection error, which
        // is not a typed `ChatError` — that is how "allowed through" is told
        // apart from "refused".
        let mut client = Client::new("http://127.0.0.1:1/v1", None, "qwen3-vl-local");
        let Err(err) = client.chat_stream(std::slice::from_ref(&m), &[]).await else {
            panic!("a closed port cannot answer");
        };
        assert!(
            err.downcast_ref::<ChatError>().is_none(),
            "with no cap configured this attachment is fine: {err:#}"
        );

        client.set_max_attachment_bytes(Some(encoded - 1));
        assert_eq!(client.max_attachment_bytes(), Some(encoded - 1));
        let Err(err) = client.chat_stream(std::slice::from_ref(&m), &[]).await else {
            panic!("the configured cap must refuse it");
        };
        let chat: &ChatError = err
            .downcast_ref()
            .unwrap_or_else(|| panic!("expected a typed ChatError, got {err:#}"));
        assert_eq!(chat.status, None, "nothing was sent");
        assert!(
            chat.message.contains(&(encoded - 1).to_string()),
            "the refusal names the configured cap: {}",
            chat.message
        );
    }

    /// A model the catalog has never heard of — a local server, an unlisted id
    /// — is **not** refused. The sandboxed XDG roots mean there is no cached
    /// catalog here at all, which is exactly the unknown case.
    #[tokio::test]
    async fn an_unknown_model_may_still_carry_attachments() {
        use crate::media::tests::png_attachment;
        let mut m = ChatMessage::user("what is this");
        m.attachments = vec![png_attachment("a.png")];

        // Gets as far as a connection failure to a closed port — the point
        // being that the gate did not stop it first.
        let client = Client::new("http://127.0.0.1:1/v1", None, "qwen3-vl-local");
        let Err(err) = client.chat_stream(&[m], &[]).await else {
            panic!("a closed port cannot answer");
        };
        assert!(
            err.downcast_ref::<ChatError>().is_none(),
            "an unknown model must not be refused by the gate: {err:#}"
        );
    }

    #[test]
    fn effort_getter_preserves_display_only_values_and_clear() {
        let mut client = Client::new("http://localhost/v1", None, "model");
        assert_eq!(client.effort(), None);
        client.set_effort(Some("high".to_string()));
        assert_eq!(client.effort(), Some("high"));
        client.set_effort(Some("custom-display-label".to_string()));
        assert_eq!(client.effort(), Some("custom-display-label"));
        client.set_effort(None);
        assert_eq!(client.effort(), None);
    }

    #[test]
    fn effort_getter_reflects_latest_value_and_request_mapping() {
        let mut client = Client::new("http://localhost/v1", None, "model");

        client.set_effort(Some("off".to_string()));
        assert_eq!(client.effort(), Some("off"));
        let off = client.request(Some(client.model.clone()), &[], &[], false);
        assert!(off.reasoning_effort.is_none());

        client.set_effort(Some("high".to_string()));
        assert_eq!(client.effort(), Some("high"));
        let high = client.request(Some(client.model.clone()), &[], &[], false);
        assert_eq!(high.reasoning_effort.as_deref(), Some("high"));

        client.set_effort(None);
        assert_eq!(client.effort(), None);
        let none = client.request(Some(client.model.clone()), &[], &[], false);
        assert!(none.reasoning_effort.is_none());
    }

    /// The chat-completions body grows a top-level `prompt_cache_key` when one
    /// is set, and carries no such field when it is not — an OpenAI-compatible
    /// server that has never heard of the parameter must see an unchanged body.
    #[test]
    fn prompt_cache_key_appears_on_the_chat_completions_body_only_when_set() {
        let mut client = Client::new("https://api.openai.com/v1", None, "gpt-5.6");
        assert_eq!(client.prompt_cache_key(), None);

        let unset = client.body_json(&client.request(Some(client.model.clone()), &[], &[], true));
        assert!(
            unset.get("prompt_cache_key").is_none(),
            "an unset key must not put an empty/null field on the wire: {unset}"
        );

        client.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));
        assert_eq!(client.prompt_cache_key(), Some("hrdr-agent-0f1e2d3c"));
        let set = client.body_json(&client.request(Some(client.model.clone()), &[], &[], true));
        assert_eq!(set["prompt_cache_key"], "hrdr-agent-0f1e2d3c");
        // Additive only — the request the struct serialized is otherwise intact.
        assert_eq!(set["model"], "gpt-5.6");
        assert_eq!(set["stream"], true);

        // Clearing it removes the field again (a provider switch that drops the
        // key must be visible as an absent field, not a stale one).
        client.set_prompt_cache_key(None);
        let cleared = client.body_json(&client.request(Some(client.model.clone()), &[], &[], true));
        assert!(cleared.get("prompt_cache_key").is_none());
    }

    /// DeepSeek's thinking mode requires an assistant turn's
    /// `reasoning_content` back on every subsequent request when `tools` is
    /// present (hrdr's agent loop always sends tools) — omitting it is a 400
    /// reading *"The reasoning_content in the thinking mode must be passed back
    /// to the API."*
    ///
    /// The graft is keyed on the model OR the host, not the host alone: the
    /// same model reached through a gateway — OpenCode Zen, OpenRouter,
    /// Together — 400s identically from a host that is not `api.deepseek.com`.
    /// `ChatMessage.reasoning_content` is `skip_serializing`, so the graft is
    /// the only thing that puts the field on the wire, and it must stay off
    /// every other body: an OpenAI-compatible server rejecting unknown message
    /// fields is the failure the pass-back must not reintroduce.
    #[test]
    fn reasoning_content_is_grafted_for_the_deepseek_model_or_host() {
        // A reasoning assistant turn, an assistant turn with no reasoning, and
        // a user turn holding the field on the struct — `reasoning_content`
        // deserializes on any role, so a resumed session can carry one, and
        // only the assistant turn may grow it on the wire.
        fn assistant_with_reasoning(content: &str, reasoning: &str) -> ChatMessage {
            ChatMessage {
                role: Role::Assistant,
                content: Some(content.to_string()),
                reasoning_content: Some(reasoning.to_string()),
                anthropic_thinking_blocks: vec![],
                responses_reasoning_items: vec![],
                attachments: vec![],
                origin: Default::default(),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }
        }
        let mut user = ChatMessage::user("ok");
        user.reasoning_content = Some("a user turn's field is never replayed".to_string());
        let messages = vec![
            assistant_with_reasoning("Let me check that.", "I should verify the docs first."),
            ChatMessage::assistant("No reasoning here."),
            user,
        ];

        // DeepSeek's own host, plus the gateways that serve DeepSeek models
        // under a hostname of their own — the second row is the session that
        // reported the 400, the next two are vendor-prefixed ids, and the last
        // one matches only once the id is folded to lower case.
        for (url, model) in [
            ("https://api.deepseek.com", "deepseek-v4-pro"),
            ("https://opencode.ai/zen/v1", "deepseek-v4-flash-free"),
            ("https://openrouter.ai/api/v1", "deepseek/deepseek-chat"),
            ("https://api.together.xyz/v1", "deepseek-ai/DeepSeek-R1"),
            ("https://models.github.ai/inference", "DeepSeek-R1"),
        ] {
            let client = Client::new(url, None, model);
            assert!(
                replays_reasoning_content(&client.base_url, &client.model),
                "{url} / {model} must replay reasoning_content"
            );
            let body =
                client.body_json(&client.request(Some(client.model.clone()), &messages, &[], true));
            let msgs = body["messages"].as_array().expect("messages array");
            assert_eq!(msgs.len(), 3);
            assert_eq!(msgs[0]["role"], "assistant");
            assert_eq!(
                msgs[0]["reasoning_content"], "I should verify the docs first.",
                "{url} / {model} must carry the assistant's reasoning: {body}"
            );
            // The graft is additive — the serialized message is otherwise intact.
            assert_eq!(msgs[0]["content"], "Let me check that.");
            // No reasoning on the field-less assistant turn or the user turn.
            assert!(
                msgs[1].get("reasoning_content").is_none(),
                "an assistant message with no reasoning must stay bare: {body}"
            );
            assert!(
                msgs[2].get("reasoning_content").is_none(),
                "a user message must never carry reasoning_content: {body}"
            );
        }

        // Neither the host nor the model is DeepSeek's: the field never reaches
        // the wire, on any of them.
        for (url, model) in [
            ("https://api.openai.com", "gpt-5.6"),
            ("https://openrouter.ai/api/v1", "qwen/qwen3-coder"),
            ("http://localhost:8080/v1", "glm-5"),
        ] {
            let other = Client::new(url, None, model);
            assert!(!replays_reasoning_content(&other.base_url, &other.model));
            let body =
                other.body_json(&other.request(Some(other.model.clone()), &messages, &[], true));
            assert!(
                !body.to_string().contains("reasoning_content"),
                "{url} / {model} must not receive reasoning_content: {body}"
            );
        }
    }

    /// Every [`UnsupportedParam`](crate::UnsupportedParam) variant actually
    /// leaves the chat-completions wire when cleared.
    ///
    /// Driven off `UnsupportedParam::ALL` rather than a hand-written list, so a
    /// sixth variant fails here until its field mapping is written — a `match`
    /// arm that forgot to clear anything would otherwise look exactly like one
    /// that worked.
    #[test]
    fn clearing_a_rejected_parameter_removes_it_from_the_chat_completions_body() {
        // `gpt-4o` keeps the original `max_tokens` spelling; a reasoning model
        // renames the same field to `max_completion_tokens`
        // (`uses_max_completion_tokens`), so both are exercised — a matcher that
        // knew only one spelling would leave half of OpenAI unrecoverable.
        for (model, cap_field) in [
            ("gpt-4o", "max_tokens"),
            ("o3-mini", "max_completion_tokens"),
        ] {
            for (param, field) in [
                (crate::UnsupportedParam::MaxTokens, cap_field),
                (crate::UnsupportedParam::Temperature, "temperature"),
                (crate::UnsupportedParam::TopP, "top_p"),
                (crate::UnsupportedParam::PromptCacheKey, "prompt_cache_key"),
                (crate::UnsupportedParam::ReasoningEffort, "reasoning_effort"),
            ] {
                let mut client = Client::new("https://api.openai.com/v1", None, model);
                client.temperature = Some(0.3);
                client.set_effort(Some("high".to_string()));
                client.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));
                client.set_params(crate::RequestParams {
                    max_tokens: Some(4096),
                    top_p: Some(0.9),
                    ..Default::default()
                });

                let before =
                    client.body_json(&client.request(Some(client.model.clone()), &[], &[], true));
                assert!(
                    before.get(field).is_some(),
                    "precondition: `{field}` is on the wire before clearing: {before}"
                );

                client.clear_unsupported_param(param);
                let after =
                    client.body_json(&client.request(Some(client.model.clone()), &[], &[], true));
                assert!(
                    after.get(field).is_none(),
                    "`{field}` must be gone after clearing {param:?}: {after}"
                );
                // Only the named field goes: dropping one rejected parameter must
                // not quietly take the others with it.
                for other in [
                    cap_field,
                    "temperature",
                    "top_p",
                    "prompt_cache_key",
                    "reasoning_effort",
                ] {
                    if other != field {
                        assert!(
                            after.get(other).is_some(),
                            "clearing {param:?} must not also drop `{other}`: {after}"
                        );
                    }
                }
            }
        }
    }

    /// The Responses shape spells two of them differently — `max_output_tokens`
    /// and a nested `reasoning.effort` — off the *same* `Client` fields. Clearing
    /// has to reach both spellings, or a Codex session would keep sending the
    /// parameter it was just told to stop sending.
    #[test]
    fn clearing_a_rejected_parameter_removes_it_from_the_responses_body() {
        let codex_body = |client: &Client| {
            crate::codex::build_body(
                &client.model,
                client.effort(),
                client.temperature,
                client.params().top_p,
                client.params().max_tokens,
                client.prompt_cache_key(),
                &[],
                &[],
            )
        };
        for (param, probe) in [
            (crate::UnsupportedParam::MaxTokens, "max_output_tokens"),
            (crate::UnsupportedParam::ReasoningEffort, "reasoning"),
            (crate::UnsupportedParam::Temperature, "temperature"),
            (crate::UnsupportedParam::TopP, "top_p"),
            (crate::UnsupportedParam::PromptCacheKey, "prompt_cache_key"),
        ] {
            let mut client = Client::new("https://chatgpt.com/backend-api/codex", None, "gpt-5.6");
            client.temperature = Some(0.3);
            client.set_effort(Some("high".to_string()));
            client.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));
            client.set_params(crate::RequestParams {
                max_tokens: Some(4096),
                top_p: Some(0.9),
                ..Default::default()
            });

            assert!(
                codex_body(&client).get(probe).is_some(),
                "precondition: `{probe}` is on the Responses wire before clearing"
            );
            client.clear_unsupported_param(param);
            assert!(
                codex_body(&client).get(probe).is_none(),
                "`{probe}` must be gone after clearing {param:?}"
            );
        }
    }

    /// Two consecutive requests from one client send the *same* key. That is the
    /// whole point: OpenAI combines the key with the prompt-prefix hash, so a
    /// value that changed per request would share a prefix with nothing. (It is
    /// also why the key is per-conversation rather than per-process — OpenAI's
    /// guidance caps a single key at roughly 15 requests per minute.)
    #[test]
    fn prompt_cache_key_is_stable_across_consecutive_requests() {
        let mut client = Client::new("https://api.openai.com/v1", None, "gpt-5.6");
        client.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));

        let first = client.body_json(&client.request(
            Some(client.model.clone()),
            &[ChatMessage::user("one")],
            &[],
            true,
        ));
        let second = client.body_json(&client.request(
            Some(client.model.clone()),
            &[ChatMessage::user("one"), ChatMessage::user("two")],
            &[],
            true,
        ));
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        assert_eq!(first["prompt_cache_key"], "hrdr-agent-0f1e2d3c");
    }

    /// The native Anthropic backend must never carry `prompt_cache_key`: the
    /// Messages API has no such parameter (its caching is the explicit
    /// `cache_control` breakpoints) and rejects unknown top-level fields, so
    /// sending it would turn every request into a 400.
    ///
    /// The guarantee is structural — `crate::anthropic::chat_stream` has no
    /// `prompt_cache_key` parameter for `Client::chat_stream` to pass, so the
    /// key cannot reach the Anthropic body. This drives the body builder that
    /// dispatch calls, as the tripwire for anyone who later adds one.
    #[test]
    fn anthropic_body_never_carries_prompt_cache_key() {
        let client = Client::new("https://api.anthropic.com/v1", None, "claude-opus-4-8");
        assert_eq!(
            detect_backend(client.base_url()),
            Backend::Anthropic,
            "this test is meaningless unless the endpoint really is the native backend"
        );
        let body = crate::anthropic::build_body(
            "claude-opus-4-8",
            8192,
            Some("high"),
            None,
            None,
            &[],
            CacheMode::Ephemeral,
            false,
            None,
            &[ChatMessage::system("you are hrdr"), ChatMessage::user("hi")],
            &[],
        );
        assert!(
            body.get("prompt_cache_key").is_none(),
            "the Messages API would 400 on an unknown top-level field: {body}"
        );

        // And the OpenAI shape, from a client configured the same way, does
        // carry it — so the absence above is backend-specific, not a no-op.
        let mut openai = Client::new("https://api.openai.com/v1", None, "gpt-5.6");
        openai.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));
        let openai_body =
            openai.body_json(&openai.request(Some(openai.model.clone()), &[], &[], true));
        assert_eq!(openai_body["prompt_cache_key"], "hrdr-agent-0f1e2d3c");
    }

    #[test]
    fn max_tokens_routes_to_completion_field_for_reasoning_models() {
        assert!(uses_max_completion_tokens("o3-mini"));
        assert!(uses_max_completion_tokens("openai/gpt-5"));
        assert!(uses_max_completion_tokens("o1"));
        assert!(!uses_max_completion_tokens("gpt-4o"));
        assert!(!uses_max_completion_tokens("claude-opus-4-8"));

        // A reasoning model routes the cap to `max_completion_tokens`.
        let mut c = Client::new("https://api.openai.com/v1", None, "o3-mini");
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(Some(c.model.clone()), &[], &[], false);
        assert_eq!(r.max_tokens, None);
        assert_eq!(r.max_completion_tokens, Some(1000));

        // A normal model uses `max_tokens`.
        let mut c = Client::new("https://api.openai.com/v1", None, "gpt-4o");
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(Some(c.model.clone()), &[], &[], false);
        assert_eq!(r.max_tokens, Some(1000));
        assert_eq!(r.max_completion_tokens, None);

        // The sentinel path: the configured id is `UNNAMED_MODEL`, but the
        // resolved wire model names the reasoning model, so the cap still
        // routes to `max_completion_tokens`.
        let mut c = Client::new("https://example.com/v1", None, UNNAMED_MODEL);
        c.set_params(crate::RequestParams {
            max_tokens: Some(1000),
            ..Default::default()
        });
        let r = c.request(Some("o3-mini".to_string()), &[], &[], false);
        assert_eq!(r.max_tokens, None);
        assert_eq!(r.max_completion_tokens, Some(1000));

        // With no wire model the field is omitted and the fallback keeps
        // `max_tokens`.
        let r = c.request(None, &[], &[], false);
        assert_eq!(r.max_tokens, Some(1000));
        assert_eq!(r.max_completion_tokens, None);
    }

    /// The "whatever you serve" sentinel never reaches the wire as a model id.
    /// It is not a name any server knows: vLLM validates `model` and 404s it,
    /// and llama.cpp's router selects by the same field. Omitting it is what
    /// vLLM's own nullable `model` is for.
    #[test]
    fn the_unnamed_model_sentinel_is_omitted_rather_than_sent() {
        let client = Client::new("http://gpu-box.lan:8000/v1", None, UNNAMED_MODEL);
        // `wire_model` resolves it against the endpoint; with none reachable it
        // yields `None`, and the field must then be absent (not `"default"`,
        // not `null`).
        let body = client.body_json(&client.request(None, &[], &[], true));
        assert!(
            body.get("model").is_none(),
            "an unresolvable sentinel must send no `model` at all: {body}"
        );
        // A real id is always sent, sentinel logic or not.
        let named = Client::new("http://gpu-box.lan:8000/v1", None, "qwen3-coder");
        let body = named.body_json(&named.request(Some("qwen3-coder".to_string()), &[], &[], true));
        assert_eq!(body["model"], "qwen3-coder");
    }

    /// **Known limitation, pinned deliberately** — this test documents current
    /// behaviour, it does not endorse it.
    ///
    /// The sentinel is handled on the OpenAI path only. [`Client::chat_stream`]
    /// returns into [`crate::anthropic`] / [`crate::codex`] with `&self.model`
    /// *before* it reaches `wire_model()`, and both native builders write
    /// `"model": model` unconditionally — so a provider entry left at `default`
    /// and pointed at either endpoint puts the literal string on the wire, and
    /// the only diagnosis is that provider's own "unknown model" error.
    ///
    /// It is not resolved there because there is nothing to resolve it *to*:
    /// [`Client::wire_model`] adopts a `/v1/models` listing only when it holds
    /// exactly one entry, which a hosted multi-model provider's never does. So
    /// the honest choices are today's pass-through or an up-front error — not
    /// asking the endpoint.
    #[test]
    fn the_unnamed_model_sentinel_reaches_the_wire_on_the_native_backends() {
        let anthropic = crate::anthropic::build_body(
            UNNAMED_MODEL,
            8192,
            None,
            None,
            None,
            &[],
            CacheMode::Off,
            false,
            None,
            &[ChatMessage::user("hi")],
            &[],
        );
        assert_eq!(
            anthropic["model"], UNNAMED_MODEL,
            "pinned: the Messages API body carries the sentinel verbatim"
        );

        let codex = crate::codex::build_body(
            UNNAMED_MODEL,
            None,
            None,
            None,
            None,
            None,
            &[ChatMessage::user("hi")],
            &[],
        );
        assert_eq!(
            codex["model"], UNNAMED_MODEL,
            "pinned: the Responses API body carries the sentinel verbatim"
        );

        // The divergence is the finding, so assert both halves in one place:
        // handed the same sentinel (already resolved to nothing), the OpenAI
        // builder omits the field entirely instead of sending `"default"`.
        let openai = Client::new("http://gpu-box.lan:8000/v1", None, UNNAMED_MODEL);
        let body = openai.body_json(&openai.request(None, &[], &[], true));
        assert!(
            body.get("model").is_none(),
            "the OpenAI path still omits it: {body}"
        );
    }

    /// `prompt_cache_key` goes only to the endpoints that read it. The gate is
    /// an allowlist rather than "not localhost" on purpose: a self-hosted vLLM,
    /// llama.cpp or Ollama is as likely to sit behind private DNS on another
    /// machine as on `localhost`, and none of them consume the field.
    #[test]
    fn prompt_cache_key_goes_only_to_endpoints_that_read_it() {
        let with_key = |url: &str, api_version: Option<&str>| {
            let mut c = Client::new(url, None, "m");
            c.set_api_version(api_version.map(str::to_string));
            c.set_prompt_cache_key(Some("hrdr-agent-0f1e2d3c".to_string()));
            let body = c.body_json(&c.request(Some("m".to_string()), &[], &[], true));
            body.get("prompt_cache_key").is_some()
        };
        // Reads it: OpenAI, an Azure OpenAI deployment, the Codex backend.
        assert!(with_key("https://api.openai.com/v1", None));
        assert!(with_key(
            "https://my-org.openai.azure.com/openai/deployments/gpt-5",
            Some("2024-10-21")
        ));
        assert!(with_key("https://chatgpt.com/backend-api/codex", None));
        // Does not: self-hosted servers, wherever they live, and gateways.
        for url in [
            "http://localhost:8080/v1",
            "http://gpu-box.lan:8000/v1",
            "https://vllm.internal.example.com/v1",
            "http://10.0.0.5:11434/v1",
            "https://openrouter.ai/api/v1",
        ] {
            assert!(!with_key(url, None), "{url} must not receive the key");
        }
    }

    #[test]
    fn backend_detected_from_host() {
        assert_eq!(
            detect_backend("https://api.anthropic.com/v1"),
            Backend::Anthropic
        );
        assert_eq!(detect_backend("https://api.openai.com/v1"), Backend::OpenAi);
        assert_eq!(detect_backend("http://localhost:8080/v1"), Backend::OpenAi);
        // ChatGPT/Codex OAuth endpoint → the Responses API backend.
        assert_eq!(
            detect_backend("https://chatgpt.com/backend-api/codex"),
            Backend::Codex
        );
        // chatgpt.com without a `/codex/` path is not the Responses backend.
        assert_eq!(
            detect_backend("https://chatgpt.com/backend-api"),
            Backend::OpenAi
        );
    }

    /// How an image is priced follows the detected backend and nothing else, so
    /// a request's shape and its token estimate cannot disagree about which
    /// provider they are talking to. Both OpenAI dialects price alike; only the
    /// native Messages API is charged Anthropic's 28px patches.
    #[test]
    fn the_token_target_follows_the_detected_backend() {
        let target = |url: &str| Client::new(url, None, "m").token_target();
        assert_eq!(
            target("https://api.anthropic.com/v1"),
            crate::media::TokenTarget::Anthropic
        );
        for url in [
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "http://localhost:8080/v1",
            "https://chatgpt.com/backend-api/codex",
        ] {
            assert_eq!(target(url), crate::media::TokenTarget::OpenAi, "{url}");
        }
        // Forced past detection, the estimate follows the force.
        let mut client = Client::new("http://127.0.0.1:9/v1", None, "m");
        client.set_backend_for_test(Backend::Anthropic);
        assert_eq!(client.token_target(), crate::media::TokenTarget::Anthropic);
    }

    #[test]
    fn url_host_handles_bracketed_ipv6_literal() {
        // A naive `rsplit_once(':')` would chop this into `[:` / `:1]:8080`,
        // mangling the address. The bracket-aware parse must return the bare
        // address with the port stripped.
        assert_eq!(url_host("http://[::1]:8080/v1"), "::1");
        assert_eq!(url_host("https://[2001:db8::1]/v1"), "2001:db8::1");
        // Plain hostname + port still works.
        assert_eq!(url_host("http://localhost:8080/v1"), "localhost");
        // Anthropic detection must still work through the shared helper.
        assert_eq!(
            detect_backend("http://[::1]:8080/v1"),
            Backend::OpenAi,
            "an IPv6-literal endpoint must not mis-detect as Anthropic"
        );
    }

    #[test]
    fn auth_header_names_match_case_insensitively() {
        for name in [
            "Authorization",
            "authorization",
            "AUTHORIZATION",
            "x-api-key",
            "X-API-Key",
            "api-key",
            "Api-Key",
        ] {
            assert!(is_auth_header_name(name), "{name} must count as auth");
        }
        // Headers that merely look adjacent are not auth and must pass through.
        for name in [
            "anthropic-version",
            "ChatGPT-Account-Id",
            "x-api-key-id",
            "proxy-authorization",
        ] {
            assert!(!is_auth_header_name(name), "{name} must not count as auth");
        }
    }

    #[test]
    fn extra_headers_cannot_duplicate_or_forge_the_credential() {
        let mut client = Client::new(
            "https://api.openai.com/v1",
            Some("real-key".to_string()),
            "gpt-4o",
        );
        client.set_headers(vec![
            ("Authorization".to_string(), "Bearer forged".to_string()),
            (
                "authorization".to_string(),
                "Bearer forged-lower".to_string(),
            ),
            ("X-API-Key".to_string(), "forged".to_string()),
            ("api-key".to_string(), "forged".to_string()),
            ("ChatGPT-Account-Id".to_string(), "acct-1".to_string()),
        ]);
        let req = client
            .auth(client.http.post(client.url("chat/completions")))
            .build()
            .expect("request builds");

        // `header()` appends, so a leaked auth entry would show up as a second
        // value here — the real credential must be the only one.
        let auth: Vec<_> = req.headers().get_all("authorization").iter().collect();
        assert_eq!(auth.len(), 1, "exactly one Authorization header");
        assert_eq!(auth[0].to_str().unwrap(), "Bearer real-key");
        assert!(req.headers().get("x-api-key").is_none());
        assert!(req.headers().get("api-key").is_none());
        // A non-auth extra header still rides along untouched.
        assert_eq!(
            req.headers()
                .get("chatgpt-account-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "acct-1"
        );
    }

    #[test]
    fn anthropic_auth_keeps_a_single_x_api_key() {
        let mut client = Client::new(
            "https://api.anthropic.com/v1",
            Some("real-key".to_string()),
            "claude-sonnet-4-5",
        );
        client.set_headers(vec![("x-api-key".to_string(), "forged".to_string())]);
        let req = client
            .auth(client.http.post(client.url("messages")))
            .build()
            .expect("request builds");

        let keys: Vec<_> = req.headers().get_all("x-api-key").iter().collect();
        assert_eq!(keys.len(), 1, "exactly one x-api-key header");
        assert_eq!(keys[0].to_str().unwrap(), "real-key");
    }

    /// Azure OpenAI is the one auth arm whose *guard* can fail rather than just
    /// its header: it is selected by `api_version.is_some()`, not by the
    /// backend, so anything that disturbs that condition falls through to the
    /// `Bearer` arm below it and every Azure request 401s while OpenAI,
    /// Anthropic and Codex stay green. The two neighbours above cover their own
    /// arms; this covers that one, with a key actually set.
    #[test]
    fn azure_auth_sends_api_key_and_never_a_bearer() {
        let mut client = Client::new(
            "https://my-org.openai.azure.com/openai/deployments/gpt-5",
            Some("real-key".to_string()),
            "gpt-5",
        );
        client.set_api_version(Some("2024-10-21".to_string()));
        client.set_headers(vec![
            ("api-key".to_string(), "forged".to_string()),
            ("Authorization".to_string(), "Bearer forged".to_string()),
        ]);
        let req = client
            .auth(client.http.post(client.url("chat/completions")))
            .build()
            .expect("request builds");

        let keys: Vec<_> = req.headers().get_all("api-key").iter().collect();
        assert_eq!(keys.len(), 1, "exactly one api-key header");
        assert_eq!(keys[0].to_str().unwrap(), "real-key");
        // The negative half is the point. "`api-key` carries the key" is equally
        // true of a client that sends `Authorization` alongside it — the likelier
        // bug, since Bearer is what every other OpenAI-shaped endpoint wants.
        assert!(
            req.headers().get("authorization").is_none(),
            "Azure authenticates with api-key alone, never Bearer too"
        );
        // Same request, so the URL half of the Azure shape is covered here too:
        // the `api-version` query the auth arm keys on is actually on the wire.
        assert_eq!(
            req.url().as_str(),
            "https://my-org.openai.azure.com/openai/deployments/gpt-5/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn apply_extra_headers_filters_auth_names_for_the_streaming_backends() {
        // The Anthropic/Codex streaming paths build their own requests and reach
        // the filter through this helper, so assert on it directly too.
        let http = reqwest::Client::new();
        let req = apply_extra_headers(
            http.post("http://localhost/v1/messages")
                .header("x-api-key", "real-key"),
            &[
                ("Authorization".to_string(), "Bearer forged".to_string()),
                ("X-API-KEY".to_string(), "forged".to_string()),
                ("originator".to_string(), "hrdr".to_string()),
            ],
        )
        .build()
        .expect("request builds");

        assert!(req.headers().get("authorization").is_none());
        let keys: Vec<_> = req.headers().get_all("x-api-key").iter().collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].to_str().unwrap(), "real-key");
        assert_eq!(
            req.headers().get("originator").unwrap().to_str().unwrap(),
            "hrdr"
        );
    }

    #[tokio::test]
    async fn mid_stream_error_object_without_a_type_is_terminal() {
        // A server that sends a well-formed error object mid-stream, with no
        // [DONE] sentinel. Before the fix this deserialized as an empty
        // `ChatChunk` (every field `#[serde(default)]`) and the stream fell
        // through to the generic "incomplete stream" transient error,
        // swallowing the real message. An untyped error stays terminal.
        let body = "data: {\"error\":{\"message\":\"something broke\"}}\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let first = stream
            .next()
            .await
            .expect("stream must yield the error, not end silently");
        let err = first.expect_err("mid-stream error object must surface as Err");
        let chat_err = err
            .downcast_ref::<ChatError>()
            .expect("error must be a typed ChatError");
        assert_eq!(
            chat_err.kind,
            ChatErrorKind::Other,
            "an untyped mid-stream error must not be classified transient"
        );
        assert!(
            chat_err.message.contains("something broke"),
            "message must carry the server's text: {}",
            chat_err.message
        );
    }

    #[tokio::test]
    async fn mid_stream_rate_limit_error_is_transient() {
        // Gateways (OpenRouter, LiteLLM) deliver overload as a typed mid-stream
        // error object. It must retry, matching the native Anthropic path.
        let body =
            "data: {\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let err = stream
            .next()
            .await
            .expect("stream must yield the error")
            .expect_err("typed error must surface as Err");
        let chat_err = err.downcast_ref::<ChatError>().expect("typed ChatError");
        assert_eq!(chat_err.kind, ChatErrorKind::Transient);
    }

    #[tokio::test]
    async fn explicit_null_error_field_does_not_abort_the_stream() {
        // Some proxies emit `"error": null` on healthy chunks. That must not be
        // read as an error.
        let body = "data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: [DONE]\n\n";
        let base_url = serve_once(body).await;

        let client = Client::new(base_url, None, "test-model");
        let mut stream = client.chat_stream(&[], &[]).await.unwrap();
        let chunk = stream
            .next()
            .await
            .expect("stream must yield the content chunk")
            .expect("null error field must not be treated as an error");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }

    /// Drive a canned SSE body through the real [`Client::chat_stream`] on a
    /// forced backend and return the typed error the stream ended with.
    ///
    /// The forced backend is what makes one helper serve all three: a mock bound
    /// to `127.0.0.1` is `Backend::OpenAi` to [`detect_backend`], so the native
    /// paths are unreachable from a test without it.
    async fn stream_error(backend: Backend, body: &'static str) -> ChatError {
        let base_url = serve_once(body).await;
        let mut client = Client::new(base_url, Some("test-key".to_string()), "test-model");
        client.set_backend_for_test(backend);
        let mut stream = client
            .chat_stream(&[ChatMessage::user("hi")], &[])
            .await
            .expect("the mock server answers 200");
        let mut last = None;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                last = Some(e);
            }
        }
        let err = last.expect("the stream must have terminated with an error");
        let typed = err
            .downcast_ref::<ChatError>()
            .unwrap_or_else(|| panic!("error must be a typed ChatError, got: {err:#}"));
        ChatError {
            status: typed.status,
            retry_after: typed.retry_after,
            kind: typed.kind,
            message: typed.message.clone(),
        }
    }

    /// The same failure, delivered mid-stream by each backend,
    /// must reach hrdr-agent's retry loop as the same [`ChatErrorKind`] — that
    /// kind is the entire difference between backing off and abandoning the
    /// turn, and a user switching providers should not get a different answer to
    /// "is this worth retrying".
    ///
    /// Every backend is tested against this alone elsewhere; nothing compares
    /// them, and the classifiers have nothing in common but their output.
    /// The OpenAI one (in `Client::chat_stream` above) substring-matches
    /// `type`/`code` *and* runs a numeric `code`/`status` through
    /// [`classify_status`]; `anthropic::map_event` matches three exact type
    /// names; `codex::classify_codex_error` matches a four-code allowlist. Three
    /// shapes agreeing today is a coincidence a table has to hold in place.
    ///
    /// The last situation is a **divergence**, kept in the table rather than
    /// dropped: a rate limit that names itself only by numeric HTTP status
    /// inside the error object is retryable on OpenAI and terminal on the other
    /// two, because only the OpenAI classifier reads a number there. The
    /// gateways that emit that shape (OpenRouter, LiteLLM) are OpenAI-shaped, so
    /// nothing is known to hit it on the native paths — but the asymmetry is
    /// real and this is where it is recorded.
    ///
    /// Also asserted, and the reason `retry_after` is in the loop below: all
    /// three mid-stream paths hardcode `retry_after: None` (the `ChatError`
    /// literals in `Client::chat_stream`, `anthropic::map_event`'s `"error"`
    /// arm, and `codex::map_event`'s), so a rate limit delivered mid-stream
    /// never carries the delay the server asked for. `retry::retry_after_hint`
    /// returns a typed error's field directly without falling back to its text
    /// scan, and these messages carry no `retry-after:` suffix either, so the
    /// agent backs off on its own schedule. Only the HTTP-status path
    /// (`error_from_response`) honours `Retry-After`. Recorded, not fixed.
    #[tokio::test]
    async fn one_failure_classifies_the_same_on_all_three_backends() {
        for (situation, backend, body, expected) in [
            (
                "rate limit",
                Backend::OpenAi,
                "data: {\"error\":{\"type\":\"rate_limit_exceeded\",\"message\":\"rate limit reached\"}}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "rate limit",
                Backend::Anthropic,
                "event: error\n\
                 data: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"rate limit reached\"}}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "rate limit",
                Backend::Codex,
                "event: error\n\
                 data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"rate limit reached\"}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "server overload",
                Backend::OpenAi,
                "data: {\"error\":{\"type\":\"server_error\",\"message\":\"upstream overloaded\"}}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "server overload",
                Backend::Anthropic,
                "event: error\n\
                 data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream overloaded\"}}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "server overload",
                Backend::Codex,
                "event: error\n\
                 data: {\"type\":\"error\",\"code\":\"server_is_overloaded\",\"message\":\"upstream overloaded\"}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "bad credential",
                Backend::OpenAi,
                "data: {\"error\":{\"type\":\"invalid_request_error\",\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key\"}}\n\n",
                ChatErrorKind::Other,
            ),
            (
                "bad credential",
                Backend::Anthropic,
                "event: error\n\
                 data: {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"Incorrect API key\"}}\n\n",
                ChatErrorKind::Other,
            ),
            (
                "bad credential",
                Backend::Codex,
                "event: error\n\
                 data: {\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key\"}\n\n",
                ChatErrorKind::Other,
            ),
            // The divergence. Identical situation to the first three rows, said
            // with a number instead of a name.
            (
                "rate limit named only by HTTP status",
                Backend::OpenAi,
                "data: {\"error\":{\"code\":429,\"message\":\"rate limit reached\"}}\n\n",
                ChatErrorKind::Transient,
            ),
            (
                "rate limit named only by HTTP status",
                Backend::Anthropic,
                // Terminal, not Transient: `map_event` reads `error.type` and
                // nothing else, so a bare 429 is an unrecognized error.
                "event: error\n\
                 data: {\"type\":\"error\",\"error\":{\"status\":429,\"message\":\"rate limit reached\"}}\n\n",
                ChatErrorKind::Other,
            ),
            (
                "rate limit named only by HTTP status",
                Backend::Codex,
                // Terminal for a second, different reason: the code is read with
                // `as_str`, so a JSON *number* is not a code at all.
                "event: error\n\
                 data: {\"type\":\"error\",\"code\":429,\"message\":\"rate limit reached\"}\n\n",
                ChatErrorKind::Other,
            ),
            // Spent quota/billing: terminal, whatever the code says — even
            // `rate_limit_exceeded`/`rate_limit_error`, whose type alone would
            // have made them retryable. Only explicit usage wording counts
            // (`insufficient_quota`, billing, credit/spend caps); the Codex row
            // below shows a bare "usage quota" message no longer does.
            (
                "usage limit",
                Backend::OpenAi,
                "data: {\"error\":{\"type\":\"insufficient_quota\",\"message\":\"You exceeded your current quota\"}}\n\n",
                ChatErrorKind::UsageLimit,
            ),
            (
                "usage limit",
                Backend::Anthropic,
                "event: error\n\
                 data: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Your credit balance is too low to access the API\"}}\n\n",
                ChatErrorKind::UsageLimit,
            ),
            // The mirror image: a `rate_limit_exceeded` whose message only
            // names "usage quota" (no billing / credit / spend /
            // insufficient_quota marker) is a rate limit, not a spent cap.
            (
                "rate limit with quota wording",
                Backend::Codex,
                "event: error\n\
                 data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"you have reached your usage quota\"}\n\n",
                ChatErrorKind::Transient,
            ),
        ] {
            // The server's own message text for this situation; every payload in
            // the table above carries one of them verbatim. A situation whose
            // rows carry different messages (the usage-limit rows) lists all of
            // them and matches any.
            let needle: &[&str] = match situation {
                "rate limit" | "rate limit named only by HTTP status" => &["rate limit reached"],
                "rate limit with quota wording" => &["you have reached your usage quota"],
                "server overload" => &["upstream overloaded"],
                "bad credential" => &["Incorrect API key"],
                "usage limit" => &[
                    "You exceeded your current quota",
                    "Your credit balance is too low",
                ],
                other => panic!("no expected message text for {other:?}"),
            };
            let err = stream_error(backend, body).await;
            // Checked before the kind, because it is what stops half this table
            // passing for the wrong reason: a body whose error object went
            // unrecognized runs off the end of the stream, and every backend's
            // missing-terminator error is *also* Transient with no
            // `retry_after`. Every row's payload carries the server's message,
            // so the message is what proves the error came from the classifier.
            assert!(
                !err.message.contains("incomplete stream"),
                "{situation} on {backend:?} fell through to the truncation error \
                 instead of being classified: {}",
                err.message
            );
            assert!(
                needle.iter().any(|n| err.message.contains(n)),
                "{situation} on {backend:?} lost the server's message ({needle:?}): {}",
                err.message
            );
            assert_eq!(
                err.kind, expected,
                "{situation} on {backend:?} classified {:?}, expected {expected:?}: {}",
                err.kind, err.message
            );
            assert_eq!(
                err.retry_after, None,
                "{situation} on {backend:?}: no mid-stream path sets retry_after \
                 (see this test's doc comment) — if one now does, the comment is stale"
            );
        }
    }

    /// Serve one canned SSE body like [`serve_once`], but hand the **request**
    /// back: the returned handle resolves to the raw request headers plus the
    /// JSON hrdr actually put on the wire. `serve_once` reads the request only
    /// to drain the socket and then discards it, and what the tests below are
    /// about is a request field (or header).
    async fn serve_once_capturing(
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<(serde_json::Value, String)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("the client connects");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let headers_end = loop {
                let n = stream.read(&mut tmp).await.expect("reading the request");
                assert_ne!(n, 0, "the client closed before sending its headers");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let content_len: usize = String::from_utf8_lossy(&buf[..headers_end])
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .expect("a POST body carries Content-Length");
            let mut request = buf[headers_end..].to_vec();
            while request.len() < content_len {
                let n = stream
                    .read(&mut tmp)
                    .await
                    .expect("reading the request body");
                assert_ne!(n, 0, "the client closed mid-body");
                request.extend_from_slice(&tmp[..n]);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}"
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let headers = String::from_utf8_lossy(&buf[..headers_end]).into_owned();
            let body = serde_json::from_slice(&request).expect("the request body is JSON");
            (body, headers)
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    /// The shortest Anthropic stream that ends cleanly — the two tests below
    /// assert on the REQUEST, so the response only has to let `chat_stream` run
    /// to completion without an error of its own.
    const MINIMAL_ANTHROPIC_STREAM: &str = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n";

    /// [`Client::anthropic_max_tokens`] and its [`ANTHROPIC_MAX_TOKENS`]
    /// fallback, asserted where it lands: the `max_tokens` the Messages API
    /// requires on every request.
    ///
    /// `max_output_cached` answers `None` in two situations that share this one
    /// branch — no catalog on disk (a first run, or offline) and a catalog that
    /// does not list the model. Naming a model no catalog can list is what
    /// reaches that branch on *any* machine, a developer box with a warm cache
    /// included, without setting `HRDR_MODELS_PATH` or redirecting the XDG cache
    /// dir: both are process-global and would leak into every other test in this
    /// binary. The warm path is therefore not asserted here at all; the
    /// resolution rules it uses are covered purely by `catalog`'s
    /// `lookup_max_output_prefers_provider_then_smallest`.
    #[tokio::test]
    async fn an_uncatalogued_model_sends_the_fallback_max_tokens() {
        let (url, request) = serve_once_capturing(MINIMAL_ANTHROPIC_STREAM).await;
        let mut client = Client::new(url, Some("test-key".to_string()), "no-such-model-8c1f");
        client.set_backend_for_test(Backend::Anthropic);
        assert_eq!(client.anthropic_max_tokens(), ANTHROPIC_MAX_TOKENS);

        let mut stream = client
            .chat_stream(&[ChatMessage::user("hi")], &[])
            .await
            .expect("the mock server answers 200");
        while stream.next().await.is_some() {}

        let (body, _headers) = request.await.expect("the server captured the request");
        // Spelled as a literal on purpose. Written as `ANTHROPIC_MAX_TOKENS`
        // this assertion would follow the constant anywhere it went, and the
        // constant's VALUE is the thing that hurts: every Anthropic reply capped
        // at 8192 against models that allow 64k-128k, with the manual thinking
        // budget scaled out of the same number, and no error to show for it.
        assert_eq!(body["max_tokens"], 8192, "{body}");
    }

    /// The other half of `params.max_tokens.unwrap_or_else(…)`: a configured cap
    /// must reach the wire untouched. Without this, dropping the `unwrap_or_else`
    /// and always calling [`Client::anthropic_max_tokens`] would keep the test
    /// above green while silently overriding what the user asked for.
    #[tokio::test]
    async fn a_configured_max_tokens_wins_over_the_fallback() {
        let (url, request) = serve_once_capturing(MINIMAL_ANTHROPIC_STREAM).await;
        let mut client = Client::new(url, Some("test-key".to_string()), "no-such-model-8c1f");
        client.set_backend_for_test(Backend::Anthropic);
        client.set_params(crate::RequestParams {
            max_tokens: Some(4321),
            ..Default::default()
        });

        let mut stream = client
            .chat_stream(&[ChatMessage::user("hi")], &[])
            .await
            .expect("the mock server answers 200");
        while stream.next().await.is_some() {}

        let (body, _headers) = request.await.expect("the server captured the request");
        assert_eq!(body["max_tokens"], 4321, "{body}");
    }

    /// With no graft to apply — default cache mode, no `prompt_cache_key`, not
    /// DeepSeek — the OpenAI request serializes straight from `ChatRequest` to
    /// bytes (the fast path in `chat_stream`), so the wire body is the request
    /// verbatim, with no intermediate `Value` tree. Asserted against a
    /// re-built request, so a fast path that dropped a field or changed the
    /// `include_usage` flag goes red. Also pins the `Content-Type` the request
    /// carries: `.body(bytes)` sends no header of its own, and an
    /// OpenAI-compatible endpoint answers a JSON body without
    /// `Content-Type: application/json` with a 415 (the regression this test
    /// guards).
    #[tokio::test]
    async fn the_no_graft_openai_request_is_the_request_serialized_verbatim() {
        let stream_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                           data: [DONE]\n\n";
        let (url, request) = serve_once_capturing(stream_body).await;
        let msgs = vec![ChatMessage::user("hi")];
        let client = Client::new(url, None, "fast-path-test");
        // Default client: CacheMode::Off, no prompt-cache key, not DeepSeek.

        let mut stream = client.chat_stream(&msgs, &[]).await.unwrap();
        while stream.next().await.is_some() {}

        let (body, headers) = request.await.expect("the server captured the request");
        let headers = headers.to_ascii_lowercase();
        assert!(
            headers.contains("content-type: application/json"),
            "the JSON chat request must carry Content-Type: application/json, or \
             OpenAI-compatible endpoints answer 415: {headers}"
        );
        let expected =
            serde_json::to_value(client.request(Some("fast-path-test".into()), &msgs, &[], true))
                .unwrap();
        assert_eq!(
            body, expected,
            "the wire body is the request serialized as-is"
        );
    }

    /// The graft path still routes through `body_json`: with a graft in force,
    /// the fast path must not run, or the graft would be missing from the wire.
    /// Ephemeral cache is used here because the other grafts are gated on the
    /// endpoint (`prompt_cache_key` only goes to OpenAI's hosts; the
    /// `reasoning_content` replay needs a DeepSeek model or host) — cache
    /// breakpoints apply to every host.
    #[tokio::test]
    async fn a_graft_reaches_the_wire_through_body_json() {
        let stream_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                           data: [DONE]\n\n";
        let (url, request) = serve_once_capturing(stream_body).await;
        let mut client = Client::new(url, None, "fast-path-test");
        client.set_cache(CacheMode::Ephemeral);

        let mut stream = client
            .chat_stream(
                &[ChatMessage::system("be brief"), ChatMessage::user("hi")],
                &[],
            )
            .await
            .unwrap();
        while stream.next().await.is_some() {}

        let (body, _headers) = request.await.expect("the server captured the request");
        let messages = body["messages"].as_array().expect("messages on the wire");
        let blocks_carry_cache = messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .any(|b| b.get("cache_control").is_some());
        assert!(
            blocks_carry_cache,
            "the cache breakpoint graft reached the wire: {body}"
        );
    }

    /// Serve one canned JSON body (with `Connection: close`, so the client reads
    /// to EOF) — enough to drive the `/v1/models` context probe.
    async fn serve_json_once(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut tmp = [0u8; 4096];
            let _ = stream.read(&mut tmp).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// A multi-entry `/v1/models` with no id match must yield `None`, not the
    /// first entry's window. Verified live: OpenRouter returns 364 entries whose
    /// first is an unrelated 1M-context model, so the old `data.first()` fallback
    /// gave any typo or alias a 1M window — and since this probe outranks the
    /// models.dev catalog, the agent would never compact.
    #[tokio::test]
    async fn multi_entry_models_list_without_a_match_yields_none() {
        let body = r#"{"data":[
            {"id":"qwen/qwen3.7-flash","context_length":1000000},
            {"id":"anthropic/claude-opus-5","context_length":1000000}
        ]}"#;
        let base_url = serve_json_once(body).await;
        let client = Client::new(base_url, None, "anthropic/claude-opus-5-typo");
        assert_eq!(client.context_from_models().await, None);
    }

    /// The same list *with* a match still answers from the matching entry.
    #[tokio::test]
    async fn multi_entry_models_list_reads_the_matching_entry() {
        let body = r#"{"data":[
            {"id":"qwen/qwen3.7-flash","context_length":1000000},
            {"id":"anthropic/claude-haiku-4-5","context_length":200000}
        ]}"#;
        let base_url = serve_json_once(body).await;
        let client = Client::new(base_url, None, "anthropic/claude-haiku-4-5");
        assert_eq!(client.context_from_models().await, Some(200_000));
    }

    /// The single-entry fallback the rule was written for still works: a local
    /// server (llama.cpp / vLLM) advertising one model under a name the config
    /// may not spell the same way.
    #[tokio::test]
    async fn single_entry_models_list_still_falls_back() {
        let body = r#"{"data":[{"id":"/models/qwen3-30b.gguf","max_model_len":32768}]}"#;
        let base_url = serve_json_once(body).await;
        let client = Client::new(base_url, None, "qwen3-30b");
        assert_eq!(client.context_from_models().await, Some(32_768));
    }

    #[test]
    fn url_appends_azure_api_version_when_set() {
        let mut c = Client::new(
            "https://r.openai.azure.com/openai/deployments/gpt4o",
            None,
            "gpt4o",
        );
        // Standard endpoint: plain path.
        assert_eq!(
            Client::new("https://api.openai.com/v1", None, "m").url("chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        // Azure: api-version query appended.
        c.set_api_version(Some("2024-10-21".to_string()));
        assert_eq!(
            c.url("chat/completions"),
            "https://r.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn reads_common_context_fields() {
        // vLLM
        assert_eq!(context_field(&json!({"max_model_len": 32768})), Some(32768));
        // LM Studio
        assert_eq!(
            context_field(&json!({"max_context_length": 8192})),
            Some(8192)
        );
        // llama.cpp /props
        assert_eq!(context_field(&json!({"n_ctx": 4096})), Some(4096));
        // numeric string
        assert_eq!(
            context_field(&json!({"context_window": "16384"})),
            Some(16384)
        );
        // nothing recognizable (e.g. OpenAI/infr)
        assert_eq!(context_field(&json!({"id": "m", "object": "model"})), None);
        // non-positive is ignored
        assert_eq!(context_field(&json!({"n_ctx": 0})), None);
    }

    /// Anthropic's own `/v1/models` publishes the window as `max_input_tokens`,
    /// alongside a `max_tokens` that is the *output* cap — reading that as the
    /// window would understate it ~8×, so only the former counts.
    #[test]
    fn reads_anthropic_max_input_tokens_not_max_tokens() {
        let entry = json!({
            "id": "claude-opus-5",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": { "image_input": { "supported": true } },
        });
        assert_eq!(context_field(&entry), Some(1_000_000));
        assert_eq!(context_field(&json!({"max_tokens": 128_000})), None);
    }

    /// OpenRouter nests a second copy of the window under `top_provider`; it is
    /// the fallback when the top-level keys miss.
    #[test]
    fn reads_openrouter_top_provider_context_length() {
        assert_eq!(
            context_field(&json!({
                "id": "anthropic/claude-opus-5",
                "top_provider": { "context_length": 1_000_000, "max_completion_tokens": 128_000 },
            })),
            Some(1_000_000)
        );
        // A top-level value still wins over the nested copy.
        assert_eq!(
            context_field(&json!({
                "context_length": 200_000,
                "top_provider": { "context_length": 1_000_000 },
            })),
            Some(200_000)
        );
    }

    #[test]
    fn json_u32_parses_numeric_string() {
        assert_eq!(json_u32(&json!("1234")), Some(1234u32));
        assert_eq!(json_u32(&json!("0")), Some(0u32));
    }

    #[test]
    fn json_u32_negative_string_is_none() {
        // A negative numeric string must not parse to a valid u32.
        assert_eq!(json_u32(&json!("-1")), None);
    }

    #[test]
    fn json_u32_u64_overflow_is_none() {
        // A JSON number > u32::MAX cannot be represented; must return None.
        let big = serde_json::Value::Number(serde_json::Number::from(u64::from(u32::MAX) + 1));
        assert_eq!(json_u32(&big), None);
    }

    #[test]
    fn context_field_string_zero_is_filtered() {
        // "0" parses as u32 0 but is filtered out by the `> 0` guard.
        assert_eq!(context_field(&json!({"n_ctx": "0"})), None);
    }

    #[test]
    fn context_field_empty_object_is_none() {
        assert_eq!(context_field(&json!({})), None);
    }

    // ── Log hardening ───────────────────────────────────────────────────
    //
    // These tests verify the REQUEST_LOG file creation and growth-cap logic
    // without exercising the global singleton (which is hard to reset).

    #[test]
    #[cfg(unix)]
    fn log_file_created_with_0600_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // The same open options open_wire_log() uses.
        let mut opts = crate::fs::owner_only_options_no_follow();
        opts.create(true).append(true);
        let file = opts.open(&path).unwrap();
        drop(file);

        // On Unix, the mode argument to OpenOptions is only a *request* —
        // the kernel applies the umask on top.  The resulting file must not
        // have group/other bits set, even though the exact mode may differ
        // from 0600.
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).unwrap();
        let perm = meta.permissions().mode();
        // Check that group and other bits are clear.
        assert_eq!(
            perm & 0o077,
            0,
            "log file must not have group/other permissions (mode={perm:#o})"
        );
    }

    #[test]
    fn log_wire_skips_write_when_file_exceeds_cap() {
        // The growth-cap check inside log_wire compares the target file's
        // length against MAX_LOG_FILE_BYTES.  We verify the same logic by
        // writing up to and past the limit to a temp file, then re-checking
        // against the cap constant.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Write MAX_LOG_FILE_BYTES bytes.
        let data = vec![b'x'; MAX_LOG_FILE_BYTES as usize];
        std::fs::write(&path, &data).unwrap();

        // The metadata check should see length >= MAX_LOG_FILE_BYTES.
        let meta = std::fs::metadata(&path).unwrap();
        assert!(
            meta.len() >= MAX_LOG_FILE_BYTES,
            "file should be at or past the cap"
        );

        // Opening for append and writing a line would be skipped by the
        // guard in log_wire.  We verify the guard condition directly.
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let would_skip = file.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_LOG_FILE_BYTES;
        assert!(
            would_skip,
            "log_wire must skip writes when file >= MAX_LOG_FILE_BYTES"
        );
        drop(file);
    }

    #[test]
    fn log_wire_allows_write_when_file_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Write a small amount well under the cap.
        std::fs::write(&path, b"small").unwrap();

        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let would_skip = file.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_LOG_FILE_BYTES;
        assert!(
            !would_skip,
            "log_wire must allow writes when file < MAX_LOG_FILE_BYTES"
        );
    }

    // ── Preflight hardening ────────────────────────────────────────────
    //
    // open_wire_log now rejects pre-existing symlinks and non-regular files
    // before opening.  These tests verify the preflight independently of the
    // global singleton.

    #[test]
    #[cfg(unix)]
    fn open_wire_log_rejects_pre_existing_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.log");
        let link = dir.path().join("requests.log");

        // Create a regular target with known content and permissions.
        std::fs::write(&target, b"secret data").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Place a symlink at the wire-log path.
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // open_wire_log must refuse to follow the symlink.
        assert!(
            open_wire_log(&link).is_none(),
            "must reject a pre-existing symlink"
        );

        // Neither the target content nor its permissions may be changed.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "secret data",
            "target content must be unchanged"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0o044,
            "target permissions must be unchanged (mode={mode:#o})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn open_wire_log_rejects_symlink_placed_after_rename() {
        // Simulate the race window that rotate_wire_log faces: rename the
        // original active file to .1, then an external actor places a symlink
        // at the now-empty path before open_wire_log is called.  This is a
        // deterministic test of that code path (no timing needed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");
        let rotated = crate::fs::sibling_with_suffix(&path, ".1");

        // 1. Create a regular active file.
        std::fs::write(&path, b"original data").unwrap();

        // 2. Rename it to .1 (as rotate_wire_log does).
        std::fs::rename(&path, &rotated).unwrap();

        // 3. A symlink appears at the now-vacant active path.
        let target = dir.path().join("target.log");
        std::fs::write(&target, b"evil payload").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        // 4. open_wire_log (called by rotate_wire_log for the fresh file)
        //    must reject the symlink.
        assert!(
            open_wire_log(&path).is_none(),
            "must reject a symlink placed at path after rename"
        );

        // 5. The symlink target must be untouched.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "evil payload",
            "target content must be unchanged"
        );
    }

    #[test]
    #[cfg(unix)]
    fn open_wire_log_refuses_final_component_symlink_with_o_nofollow() {
        // Exercises the O_NOFOLLOW open specifically.  The symlink at the exact
        // wire-log path points at a *non-existent* target: with create(true)
        // and no O_NOFOLLOW, a followed open would traverse the link and CREATE
        // the target file (silently redirecting the append).  O_NOFOLLOW makes
        // the open on a final-component symlink fail with ELOOP instead, so
        // open_wire_log must return None and the link's target must never come
        // into existence — proving the descriptor-level guarantee, not just the
        // preflight, refuses the swap.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("requests.log");
        let target = dir.path().join("does-not-exist.log");

        // Dangling symlink at the wire-log path.
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!target.exists(), "precondition: target must not exist yet");

        // open_wire_log must refuse the symlink...
        assert!(
            open_wire_log(&link).is_none(),
            "must refuse a final-component symlink at open time"
        );

        // ...and must not have created or written the link's target.
        assert!(
            !target.exists(),
            "symlink target must be neither created nor written"
        );
    }

    #[test]
    fn open_wire_log_creates_new_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        let file = open_wire_log(&path);
        assert!(file.is_some(), "must create a new file at a fresh path");
        drop(file);

        let meta = std::fs::metadata(&path).unwrap();
        assert!(
            meta.file_type().is_file(),
            "created path must be a regular file"
        );
    }

    #[test]
    fn open_wire_log_opens_existing_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");

        // Pre-create a regular file.
        std::fs::write(&path, b"existing content").unwrap();

        let file = open_wire_log(&path);
        assert!(
            file.is_some(),
            "must open an existing regular file for append"
        );
    }

    #[test]
    fn open_wire_log_rejects_directory() {
        // A directory is non-regular on every platform.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("mydir");
        std::fs::create_dir(&d).unwrap();

        assert!(
            open_wire_log(&d).is_none(),
            "must reject a pre-existing directory"
        );
    }

    // ── Rotation ────────────────────────────────────────────────────────
    //
    // These exercise the rotation mechanics directly (bypassing the 10 MiB
    // global singleton) with a small simulated cap, mirroring log_wire's
    // over-cap branch: fill the active file, rotate, keep writing.

    /// Append `line` to the wire log at `path`, rotating first when the append
    /// would meet/exceed `cap`. A trimmed clone of `log_wire`'s file-locked
    /// body, parameterised on `cap` so a test needn't write 10 MiB.
    fn append_with_cap(path: &Path, file: &mut std::fs::File, line: &str, cap: u64) {
        let current = file.metadata().map(|m| m.len()).unwrap_or(0);
        if wire_log_over_cap(current, line.len() as u64, cap) {
            rotate_wire_log(path, file).expect("rotation should succeed");
        }
        file.write_all(line.as_bytes()).unwrap();
    }

    #[test]
    fn writing_past_cap_rotates_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");
        let mut file = open_wire_log(&path).unwrap();

        // Cap small enough that the second line trips rotation.
        let cap = 4u64;
        append_with_cap(&path, &mut file, "OLD\n", cap); // 4 bytes, under cap
        append_with_cap(&path, &mut file, "NEW\n", cap); // would hit cap → rotate

        let rotated = crate::fs::sibling_with_suffix(&path, ".1");
        assert!(rotated.exists(), ".1 file must exist after rotation");
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "OLD\n");
        // The active file continues with the new content.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "NEW\n");
    }

    #[test]
    fn second_rotation_replaces_dot_one_without_accumulating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");
        let mut file = open_wire_log(&path).unwrap();

        let cap = 4u64;
        append_with_cap(&path, &mut file, "AAA\n", cap);
        append_with_cap(&path, &mut file, "BBB\n", cap); // rotate: .1 = AAA
        append_with_cap(&path, &mut file, "CCC\n", cap); // rotate: .1 = BBB

        let rotated = crate::fs::sibling_with_suffix(&path, ".1");
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "BBB\n",
            ".1 must hold the most-recently-rotated content"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "CCC\n");
        // No `.2` (or deeper) may accumulate.
        let mut name = path.as_os_str().to_owned();
        name.push(".2");
        assert!(
            !PathBuf::from(name).exists(),
            "rotation must not create a .2 file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn both_files_keep_0600_after_rotation() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.log");
        let mut file = open_wire_log(&path).unwrap();

        let cap = 4u64;
        append_with_cap(&path, &mut file, "OLD\n", cap);
        append_with_cap(&path, &mut file, "NEW\n", cap); // rotate

        let rotated = crate::fs::sibling_with_suffix(&path, ".1");
        for p in [&path, &rotated] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "{p:?} must not have group/other bits (mode={mode:#o})"
            );
        }
    }

    #[test]
    fn rotation_failure_returns_err_without_panicking() {
        // Rename fails when the active path does not exist on disk (source
        // missing). We hand rotate_wire_log a live file handle but a path that
        // was never created, so the internal `rename` errors and the helper
        // returns Err rather than panicking.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created.log");
        // A real, unrelated file to satisfy the &mut File argument.
        let real = dir.path().join("scratch.log");
        let mut file = open_wire_log(&real).unwrap();

        let result = rotate_wire_log(&missing, &mut file);
        assert!(
            result.is_err(),
            "rotating a nonexistent active path must return Err, not panic"
        );
    }

    #[test]
    fn wire_log_over_cap_boundaries() {
        // At or past cap → rotate.
        assert!(wire_log_over_cap(10, 1, 10));
        assert!(wire_log_over_cap(11, 1, 10));
        // Fits exactly → no rotation.
        assert!(!wire_log_over_cap(6, 4, 10));
        // One byte too many → rotate.
        assert!(wire_log_over_cap(7, 4, 10));
    }

    // ── days_from_civil ──────────────────────────────────────────────────

    #[test]
    fn days_from_civil_known_dates() {
        // 1970-01-01 = Unix epoch day 0.
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        // 1994-11-06 = day 9075 (verified by manual epoch-days arithmetic).
        assert_eq!(days_from_civil(1994, 11, 6), Some(9075));
        // 2023-01-01.
        assert_eq!(days_from_civil(2023, 1, 1), Some(19358));
        // 1969-12-31 = day -1 (before epoch).
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
    }

    #[test]
    fn days_from_civil_out_of_range() {
        assert_eq!(days_from_civil(1970, 0, 1), None);
        assert_eq!(days_from_civil(1970, 13, 1), None);
        assert_eq!(days_from_civil(1970, 1, 0), None);
        assert_eq!(days_from_civil(1970, 1, 32), None);
    }

    // ── Retry-After parsing ──────────────────────────────────────────────

    /// A `HeaderMap` carrying one `Retry-After` value — the only input
    /// [`retry_after_from_headers`] reads.
    fn retry_after_header(value: &str) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).expect("valid header value"),
        );
        headers
    }

    /// Format `epoch_secs` as an RFC 7231 IMF-fixdate, so the date cases below
    /// can be stated relative to *now* rather than as a literal that goes stale.
    ///
    /// This is Hinnant's `civil_from_days`, the inverse of [`days_from_civil`]
    /// above — which is why `imf_fixdate_helper_is_correct` pins it against the
    /// RFC's own worked example first. A wrong helper would hand every case
    /// below a date that only *looks* like the one it asked for.
    fn imf_fixdate(epoch_secs: u64) -> String {
        const WKDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTH: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];

        let days = (epoch_secs / 86400) as i64;
        let tod = epoch_secs % 86400;

        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };

        format!(
            "{wkday}, {d:02} {mon} {year:04} {h:02}:{mi:02}:{s:02} GMT",
            // Day 0 (1970-01-01) was a Thursday, index 4.
            wkday = WKDAY[(days + 4).rem_euclid(7) as usize],
            mon = MONTH[(m - 1) as usize],
            h = tod / 3600,
            mi = (tod % 3600) / 60,
            s = tod % 60,
        )
    }

    #[test]
    fn imf_fixdate_helper_is_correct() {
        assert_eq!(imf_fixdate(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // RFC 7231 §7.1.1.1's own example date, built from the epoch-day count
        // `days_from_civil_known_dates` already pins.
        assert_eq!(days_from_civil(1994, 11, 6), Some(9075));
        let rfc_example = 9075 * 86400 + 8 * 3600 + 49 * 60 + 37;
        assert_eq!(imf_fixdate(rfc_example), "Sun, 06 Nov 1994 08:49:37 GMT");
        // A leap day, where an off-by-one in the era arithmetic would show.
        let leap = days_from_civil(2024, 2, 29).unwrap() as u64 * 86400 + 23 * 3600 + 59 * 60 + 59;
        assert_eq!(imf_fixdate(leap), "Thu, 29 Feb 2024 23:59:59 GMT");
    }

    /// The `Retry-After` parse, over a real `HeaderMap`.
    ///
    /// Nothing else reaches this function: hrdr-agent's mock responses set no
    /// response headers, and retry.rs's `retry_after_hint_parses_and_clamps`
    /// parses the ` (retry-after: Ns)` suffix back *out* of a message string —
    /// this function's output, not this function. Untested, a 429 that named its
    /// own delay would be answered with hrdr's jittered backoff instead, i.e.
    /// hammering a provider that just said when to come back.
    #[test]
    fn retry_after_from_headers_parses_seconds_dates_and_clamps() {
        let max = crate::MAX_BACKOFF;

        // Delta-seconds (RFC 7231 §7.1.3) — the form providers actually send.
        assert_eq!(
            retry_after_from_headers(&retry_after_header("30")),
            Some(Duration::from_secs(30))
        );
        // Surrounding whitespace is trimmed before the parse.
        assert_eq!(
            retry_after_from_headers(&retry_after_header(" 30 ")),
            Some(Duration::from_secs(30))
        );
        // Zero is "come back now": no hint, so the jittered backoff applies.
        assert_eq!(retry_after_from_headers(&retry_after_header("0")), None);
        // No header at all — the overwhelmingly common case.
        assert_eq!(
            retry_after_from_headers(&reqwest::header::HeaderMap::new()),
            None
        );

        // The clamp, which is what stops a hostile or absurd value parking the
        // turn. Asserted against the constant by name: hard-coding 60 here would
        // let a raised `MAX_BACKOFF` turn this into a check of nothing.
        assert!(
            max < Duration::from_secs(86_400),
            "the clamp cases below only mean something while 86400 is over the ceiling"
        );
        assert_eq!(
            retry_after_from_headers(&retry_after_header("86400")),
            Some(max)
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_secs();

        // An IMF-fixdate inside the ceiling comes back as the remaining wait.
        // A range rather than an equality: the clock moves between formatting
        // the date and parsing it.
        let soon = retry_after_from_headers(&retry_after_header(&imf_fixdate(now + 45)))
            .expect("a future IMF-fixdate is a wait");
        assert!(
            soon <= Duration::from_secs(45) && soon >= Duration::from_secs(40),
            "expected ~45s from a date 45s out, got {soon:?}"
        );
        // The date form obeys the same ceiling as the integer form — a separate
        // `.min()` call site, so it needs its own case.
        assert_eq!(
            retry_after_from_headers(&retry_after_header(&imf_fixdate(now + 3600))),
            Some(max)
        );
        // A date already past is not a wait at all.
        assert_eq!(
            retry_after_from_headers(&retry_after_header(&imf_fixdate(now - 60))),
            None
        );

        // Garbage in each shape the parser can meet: not a number, empty, a
        // negative delta, a date missing its zone, a date with a bad month.
        for junk in [
            "soon",
            "",
            "-5",
            "Sun, 06 Nov 2999 08:49:37",
            "Xyz, 09 Zzz 2999 08:49:37 GMT",
        ] {
            assert_eq!(
                retry_after_from_headers(&retry_after_header(junk)),
                None,
                "{junk:?} must not parse as a delay"
            );
        }
    }

    // ── Status classification ────────────────────────────────────────────

    /// Every arm of [`classify_status`], plus representatives of the default.
    ///
    /// The kind is what hrdr-agent's retry and compaction decisions read, so a
    /// dropped arm is not a cosmetic problem: it is a turn that dies instead of
    /// retrying, or one that never compacts. The call sites of this
    /// function were all indirect before this test — the closest thing to
    /// coverage was one end-to-end 413 on the OpenAI path.
    #[test]
    fn classify_status_maps_every_arm_and_its_default() {
        assert_eq!(classify_status(413), ChatErrorKind::Overflow);

        for status in [408u16, 429, 500, 502, 503, 504, 522, 524, 529] {
            assert_eq!(
                classify_status(status),
                ChatErrorKind::Transient,
                "{status} must be retryable"
            );
        }

        // The default arm. 501/505 pin that 5xx is *not* blanket-transient, and
        // 523 that the Cloudflare pair is two codes rather than a range — both
        // are the shape a careless widening would take.
        for status in [200u16, 400, 401, 403, 404, 422, 501, 505, 523] {
            assert_eq!(
                classify_status(status),
                ChatErrorKind::Other,
                "{status} must classify as Other"
            );
        }

        // Spelled out separately because it is the expensive direction to get
        // wrong: a 400 is a malformed request and a 401 is a bad credential, and
        // retrying either buys the same failure a few backoffs later while
        // looking, to the user, like the provider is slow.
        for status in [400u16, 401] {
            assert_ne!(classify_status(status), ChatErrorKind::Transient);
        }
    }

    /// Serve one canned non-2xx response — status line, extra header lines (each
    /// already `\r\n`-terminated), body.
    ///
    /// Separate from [`serve_once`] above, which always answers `200 OK`: this
    /// is the only way to hand [`error_from_response`] a real
    /// `reqwest::Response`, since nothing in this crate can construct one.
    async fn serve_error_once(
        status_line: &'static str,
        extra_headers: &'static str,
        body: &'static str,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut tmp = [0u8; 4096];
            let _ = stream.read(&mut tmp).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {len}\r\n\
                 {extra_headers}\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                len = body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
        format!("http://127.0.0.1:{port}/")
    }

    /// One case per [`ChatErrorKind`]: the typed error must carry the status
    /// through, classify it with [`classify_status`], and pick `Retry-After` off
    /// the response headers — the fields hrdr-agent reads — while leaving
    /// the server's own body in `message` for the untyped fallback scanner.
    #[tokio::test]
    async fn error_from_response_round_trips_status_kind_and_retry_after() {
        for (status_line, extra_headers, body, status, kind, retry_after) in [
            (
                "413 Payload Too Large",
                "",
                r#"{"error":"prompt is too long"}"#,
                413u16,
                ChatErrorKind::Overflow,
                None,
            ),
            (
                "429 Too Many Requests",
                "Retry-After: 12\r\n",
                r#"{"error":"slow down"}"#,
                429,
                ChatErrorKind::Transient,
                Some(Duration::from_secs(12)),
            ),
            // Same status, quota-only body: bare "quota" wording is a rate
            // limit, so the 429 stays retryable — only explicit usage wording
            // (billing / credit / spend / insufficient_quota) flips it.
            (
                "429 Too Many Requests",
                "",
                r#"{"error":{"message":"You exceeded your current quota"}}"#,
                429,
                ChatErrorKind::Transient,
                None,
            ),
            (
                "400 Bad Request",
                "",
                r#"{"error":"invalid tool schema"}"#,
                400,
                ChatErrorKind::Other,
                None,
            ),
        ] {
            let url = serve_error_once(status_line, extra_headers, body).await;
            let resp = reqwest::Client::new().get(&url).send().await.unwrap();
            let err = error_from_response(resp).await;
            let chat_err = err.downcast_ref::<ChatError>().expect("typed ChatError");

            assert_eq!(chat_err.status, Some(status), "{status_line}");
            assert_eq!(chat_err.kind, kind, "{status_line}");
            assert_eq!(chat_err.retry_after, retry_after, "{status_line}");
            assert!(
                chat_err.message.contains(body),
                "the server's body must survive into the message: {}",
                chat_err.message
            );
            // A hint also has to reach the text suffix, which is the only form
            // of it an error that never went through the typed path can carry.
            if let Some(d) = retry_after {
                let suffix = format!("(retry-after: {}s)", d.as_secs());
                assert!(
                    chat_err.message.contains(&suffix),
                    "expected {suffix} in: {}",
                    chat_err.message
                );
            }
        }
    }

    #[test]
    fn rotated_path_appends_dot_one() {
        assert_eq!(
            crate::fs::sibling_with_suffix(Path::new("/var/log/requests.log"), ".1"),
            PathBuf::from("/var/log/requests.log.1")
        );
    }
}
