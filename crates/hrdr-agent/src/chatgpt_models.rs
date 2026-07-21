//! Authenticated ChatGPT (Codex) model catalog: fetch the account's entitled
//! models, cache them per-account, and fall back to the built-in preset when the
//! endpoint is unreachable or the account is not entitled.
//!
//! The `/models` request declares a *client protocol* version
//! ([`CODEX_CATALOG_COMPAT_VERSION`]) — a compatibility pin, NOT a model
//! allowlist. Compatible new models arrive dynamically; the pin is bumped only
//! after validating the client protocol at the new version. Maintenance tracker:
//! `kryptic-sh/hrdr#2`.
//!
//! The exact wire schema mirrors opencode's `codex.ts`; the parser is tolerant
//! (unknown fields ignored, missing optional fields defaulted) so a schema drift
//! degrades to fewer fields rather than a hard failure. The field names are
//! validated end-to-end by the manual login smoke test, not by unit tests
//! (which run against local fixtures, no network).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{CHATGPT_CODEX_BASE_URL, OAuthAccess, builtin_provider, write_atomic};

/// The Codex client-protocol version declared on the catalog request. A
/// compatibility pin (protocol declaration), not a model allowlist — compatible
/// new models arrive dynamically. Bump only after validating the client
/// protocol at the new version. Tracker: `kryptic-sh/hrdr#2`.
pub const CODEX_CATALOG_COMPAT_VERSION: &str = "0.144.3";

/// On-disk cache schema. Bump on any incompatible layout change (invalidates
/// older entries, which are then treated as absent).
const CATALOG_CACHE_SCHEMA: u32 = 1;

/// Fresh-cache window: 5 minutes.
const CATALOG_TTL_MS: u64 = 5 * 60 * 1000;

/// Total-request timeout for the catalog HTTP call. reqwest's default `Client`
/// has NO total-request timeout, so without this a network black-hole would hang
/// the caller and the "timeout" stale-cache trigger could never fire.
const CATALOG_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap (10 MiB) on the catalog body, enforced while streaming so a hostile
/// endpoint cannot force an unbounded allocation before the check fires.
const MAX_CATALOG_BYTES: usize = 10 * 1024 * 1024;

/// Where an entitled catalog came from — surfaced to the selector so the UI can
/// distinguish live data from a cached or degraded list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    /// Live from the endpoint (or a fresh, matching cache).
    Fresh,
    /// A previous cache served because the endpoint was unreachable.
    Stale,
    /// The built-in ChatGPT preset (endpoint unreachable/unentitled, no cache).
    BuiltInFallback,
}

/// One entitled model. Every field is non-secret (safe to cache).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptModel {
    /// Model id to switch to.
    pub slug: String,
    /// Friendly display name (falls back to the slug).
    pub label: String,
    /// Advertised context window, when the endpoint reports one.
    pub context_window: Option<u32>,
}

/// The catalog plus provenance and an optional user-facing warning.
pub struct ChatGptCatalogResult {
    pub models: Vec<ChatGptModel>,
    pub source: CatalogSource,
    pub warning: Option<String>,
}

/// The persisted per-account cache. Holds only sanitized rows — never tokens or
/// the raw account id (only its digest).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    schema: u32,
    /// Lowercase-hex SHA-256 of the account id — isolates one account's entitled
    /// rows from another's without storing the raw id.
    account_digest: String,
    compat_version: String,
    fetched_ms: u64,
    #[serde(default)]
    etag: Option<String>,
    models: Vec<ChatGptModel>,
}

/// Lowercase-hex SHA-256 of an account id.
fn account_digest(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Current time in epoch milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The per-account catalog cache path, `<XDG cache>/hrdr/chatgpt_models.json`.
fn cache_path() -> Option<PathBuf> {
    Some(
        hjkl_xdg::cache_dir("hrdr")
            .ok()?
            .join("chatgpt_models.json"),
    )
}

// ── Parser ──────────────────────────────────────────────────────────────────

/// Model features hrdr's OpenAI-compatible chat path cannot serve. A catalog row
/// that *requires* one of these is unusable, so it is hidden from the picker.
///
/// Deliberately a deny-list, not an allow-list: the upstream feature vocabulary
/// is not a contract we control, and an allow-list would hide any model that
/// named a feature we had not enumerated yet — including entitled models the
/// user is paying for. Matched case-insensitively.
const UNSUPPORTED_FEATURES: &[&str] = &[
    // The synthetic marker the catalog fixtures use for "hrdr cannot run this".
    "unsupported",
];

/// Whether a `required_features` entry names something hrdr cannot serve.
/// Unrecognised features are treated as supported — see [`UNSUPPORTED_FEATURES`].
fn is_unsupported_feature(feature: &str) -> bool {
    UNSUPPORTED_FEATURES
        .iter()
        .any(|f| f.eq_ignore_ascii_case(feature))
}

/// Parse a Codex `/models` success payload into list-visible models, in upstream
/// order. Tolerant: unknown fields are ignored and missing optional fields
/// defaulted. Rows that are not list-visible, have an empty slug, or require a
/// feature hrdr cannot serve ([`UNSUPPORTED_FEATURES`]) are dropped.
///
/// Errors only on a structurally malformed payload (no models array at all), so
/// the caller can distinguish "parsed, possibly empty" from "not a catalog".
pub fn parse_catalog(v: &Value) -> Result<Vec<ChatGptModel>> {
    let arr = v
        .get("models")
        .or_else(|| v.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("catalog payload has no models array"))?;

    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        // Default list-visible to true when the field is absent (tolerant), but
        // honour an explicit false.
        if !m
            .get("list_visible")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            continue;
        }
        let slug = m
            .get("slug")
            .or_else(|| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        // A row declaring a feature we know we cannot serve is dropped. The
        // upstream feature vocabulary is not contractual, so this is a deny-list
        // of features hrdr is known to lack, never an allow-list: an unrecognised
        // feature keeps the row (hiding an entitled model the user paid for is
        // worse than a request that fails at send time and says why).
        if let Some(reqs) = m.get("required_features").and_then(Value::as_array)
            && reqs
                .iter()
                .filter_map(Value::as_str)
                .any(is_unsupported_feature)
        {
            continue;
        }
        let label = m
            .get("label")
            .or_else(|| m.get("display_name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(slug)
            .to_string();
        // Reject a window we cannot act on: `as u32` would wrap a >=2^32 value to
        // a small number (or to 0), and `Some(0)` propagates into `apply_choice`
        // as "window known", suppressing the endpoint probe and silently
        // disabling the context gauge and auto-compaction for the whole session.
        let context_window = m
            .get("context_window")
            .or_else(|| m.get("max_context_window"))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|w| *w > 0);
        out.push(ChatGptModel {
            slug: slug.to_string(),
            label,
            context_window,
        });
    }
    Ok(out)
}

// ── Fallback ────────────────────────────────────────────────────────────────

/// The built-in ChatGPT preset as a single-row catalog — the fallback when the
/// endpoint is unreachable or the account is not entitled. Derived from
/// [`builtin_provider`], never duplicated literals.
fn builtin_fallback() -> Vec<ChatGptModel> {
    let p = builtin_provider("chatgpt");
    match p {
        Some(p) => {
            let slug = p.model.unwrap_or_default();
            if slug.is_empty() {
                Vec::new()
            } else {
                vec![ChatGptModel {
                    label: slug.clone(),
                    slug,
                    context_window: p.context_window,
                }]
            }
        }
        None => Vec::new(),
    }
}

// ── Cache I/O (path-injectable cores) ───────────────────────────────────────

fn load_cache_at(path: &Path) -> Option<CacheFile> {
    if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_CATALOG_BYTES as u64 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache_at(path: &Path, entry: &CacheFile) -> std::io::Result<()> {
    // `write_atomic` (0600 on unix) requires the parent to exist.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_atomic(path, &json)
}

/// Pure slug→window lookup over a model list, so the resolution rule is testable
/// without a cache file.
fn context_window_in(models: &[ChatGptModel], slug: &str) -> Option<u32> {
    models
        .iter()
        .find(|m| m.slug == slug)
        .and_then(|m| m.context_window)
}

/// The advertised context window for `slug` in the catalog cache file at `path`,
/// network-free. Path-injectable so it is testable without the real XDG cache.
///
/// Only the cache SCHEMA is gated (a layout bump invalidates older files). The
/// account digest and TTL are deliberately not: a model's context window is a
/// property of the model, identical across every entitled account, so a slug
/// match is authoritative regardless of which account wrote the cache or how old
/// it is — unlike *entitlement* (which rows exist), which those checks guard on
/// the serve path.
fn cached_context_window_at(path: &Path, slug: &str) -> Option<u32> {
    let cache = load_cache_at(path)?;
    if cache.schema != CATALOG_CACHE_SCHEMA {
        return None;
    }
    context_window_in(&cache.models, slug)
}

/// The context window for ChatGPT model `slug` from the on-disk account catalog
/// cache (`chatgpt_models.json`), network-free. This is the only source that
/// knows per-model windows for ChatGPT *subscription* models: the endpoint's
/// `/v1/models` returns 401 and models.dev lists the differently-windowed API
/// models. `None` when there is no cache, the slug is absent, or the row
/// advertised no window.
pub fn cached_context_window(slug: &str) -> Option<u32> {
    cached_context_window_at(&cache_path()?, slug)
}

/// The account's ENTITLED models, from the on-disk cache, network-free — the
/// authoritative "what may this account run" list, as opposed to
/// [`cached_context_window`]'s "how big is this model", which is a fact about the
/// model and therefore account-independent.
///
/// Every gate matters here, because a caller REFUSES on the strength of this list:
/// schema, compatibility version and account digest must all match, and the row set
/// must be non-empty. `None` means *we do not know* (cold cache, another account's
/// cache, a layout bump) — never "nothing is entitled". A caller that cannot tell
/// the difference would refuse an entitled model on a cold cache.
fn cached_entitlements_at(path: &Path, account_id: &str) -> Option<Vec<ChatGptModel>> {
    let cache = load_cache_at(path)?;
    if !cache_matches(&cache, &account_digest(account_id)) || cache.models.is_empty() {
        return None;
    }
    Some(cache.models)
}

/// [`cached_entitlements_at`] against the real per-account cache file. `None` when
/// there is no usable cache for `account_id` — see that function: not knowing and
/// knowing-nothing-is-entitled are different answers.
pub fn cached_entitlements(account_id: &str) -> Option<Vec<ChatGptModel>> {
    cached_entitlements_at(&cache_path()?, account_id)
}

/// A cache entry is usable — for fresh OR stale serve — only when its schema,
/// account digest, and compatibility version all match the current credential.
fn cache_matches(entry: &CacheFile, digest: &str) -> bool {
    entry.schema == CATALOG_CACHE_SCHEMA
        && entry.account_digest == digest
        && entry.compat_version == CODEX_CATALOG_COMPAT_VERSION
}

/// Fresh = matching AND within the TTL window.
fn cache_is_fresh(entry: &CacheFile, digest: &str, now: u64) -> bool {
    cache_matches(entry, digest) && now.saturating_sub(entry.fetched_ms) < CATALOG_TTL_MS
}

// ── Decision logic (pure) ───────────────────────────────────────────────────

/// The outcome of the network attempt, abstracted so the stale/fallback rules
/// are testable without HTTP.
enum FetchOutcome {
    /// A 2xx with a parsed (possibly empty) catalog.
    Fresh {
        models: Vec<ChatGptModel>,
        etag: Option<String>,
    },
    /// A 304 Not Modified.
    NotModified,
    /// A 401/403 — credentials rejected. NEVER serve stale after this.
    AuthFailed,
    /// Transport error, timeout, 5xx, 429, explicit disable, or a malformed/empty
    /// 2xx — stale cache may be served.
    Recoverable(String),
}

/// Apply the fresh/stale/fallback rules. Returns the result plus the cache entry
/// to persist (if any). `digest` is `None` when the credential has no account id
/// — persistent cache is then disabled so account-less credentials cannot share
/// another account's entitled rows.
fn resolve_catalog(
    outcome: FetchOutcome,
    cache: Option<CacheFile>,
    digest: Option<&str>,
    now: u64,
) -> (ChatGptCatalogResult, Option<CacheFile>) {
    let matching_cache = || -> Option<&CacheFile> {
        match (&cache, digest) {
            (Some(c), Some(d)) if cache_matches(c, d) => Some(c),
            _ => None,
        }
    };

    match outcome {
        FetchOutcome::Fresh { models, etag } => {
            let persist = digest.map(|d| CacheFile {
                schema: CATALOG_CACHE_SCHEMA,
                account_digest: d.to_string(),
                compat_version: CODEX_CATALOG_COMPAT_VERSION.to_string(),
                fetched_ms: now,
                etag,
                models: models.clone(),
            });
            (
                ChatGptCatalogResult {
                    models,
                    source: CatalogSource::Fresh,
                    warning: None,
                },
                persist,
            )
        }
        FetchOutcome::NotModified => match matching_cache() {
            Some(c) => (
                ChatGptCatalogResult {
                    models: c.models.clone(),
                    source: CatalogSource::Fresh,
                    warning: None,
                },
                // Refresh the fetched timestamp so the TTL restarts.
                digest.map(|d| CacheFile {
                    fetched_ms: now,
                    account_digest: d.to_string(),
                    ..c.clone()
                }),
            ),
            None => (fallback_result(None), None),
        },
        FetchOutcome::AuthFailed => (
            // Never serve stale after 401/403 — return fallback + auth warning.
            fallback_result(Some(
                "ChatGPT authorization was rejected; showing built-in models. Run /login."
                    .to_string(),
            )),
            None,
        ),
        FetchOutcome::Recoverable(why) => match matching_cache() {
            Some(c) => (
                ChatGptCatalogResult {
                    models: c.models.clone(),
                    source: CatalogSource::Stale,
                    warning: Some(format!("Using cached ChatGPT models ({why}).")),
                },
                None,
            ),
            None => (
                fallback_result(Some(format!("Showing built-in ChatGPT models ({why})."))),
                None,
            ),
        },
    }
}

fn fallback_result(warning: Option<String>) -> ChatGptCatalogResult {
    ChatGptCatalogResult {
        models: builtin_fallback(),
        source: CatalogSource::BuiltInFallback,
        warning: warning.or_else(|| Some("Showing built-in ChatGPT models.".to_string())),
    }
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// Fetch the account's entitled ChatGPT models, using a 5-minute per-account
/// cache. `force` bypasses the fresh-cache short-circuit (e.g. just after login).
///
/// Never serves another account's rows: when the credential has no account id,
/// the persistent cache is disabled entirely.
pub async fn chatgpt_model_catalog(access: &OAuthAccess, force: bool) -> ChatGptCatalogResult {
    let digest = access.account_id.as_deref().map(account_digest);
    let path = cache_path();
    let cache = match (&path, &digest) {
        // Only load the cache when we can match it to this account.
        (Some(p), Some(_)) => load_cache_at(p),
        _ => None,
    };

    // Fresh cache short-circuit (skips the network entirely).
    if !force
        && let (Some(c), Some(d)) = (&cache, &digest)
        && cache_is_fresh(c, d, now_ms())
    {
        return ChatGptCatalogResult {
            models: c.models.clone(),
            source: CatalogSource::Fresh,
            warning: None,
        };
    }

    // Only send `If-None-Match` when the cached entry actually matches the
    // current account/schema/compat. Sending another account's etag can draw a
    // 304 that then fails the match gate (NotModified → fallback, no persist) and
    // wedges the catalog on the built-in fallback until upstream content changes.
    let etag = match (&cache, &digest) {
        (Some(c), Some(d)) if cache_matches(c, d) => c.etag.clone(),
        _ => None,
    };
    let outcome = fetch_catalog(access, etag).await;
    let (result, persist) = resolve_catalog(outcome, cache, digest.as_deref(), now_ms());

    // Persist only when we have an account digest (account-less → no cache).
    if let (Some(entry), Some(p)) = (persist, &path) {
        let _ = write_cache_at(p, &entry);
    }
    result
}

/// Read a byte stream into a `Vec`, aborting the moment the accumulated length
/// would exceed `cap`. Peak memory stays at or below `cap` (an over-cap chunk is
/// never appended), so a hostile endpoint streaming a huge body cannot exhaust
/// memory before the check fires. Generic over the chunk/error types so it is
/// unit-testable with `futures_util::stream::iter`.
///
/// Returns `Err(e)` on a transport error mid-stream, `Ok(Err(TooLarge))` when the
/// cap is crossed, and `Ok(Ok(body))` with the full body otherwise.
async fn read_body_capped<S, B, E>(mut stream: S, cap: usize) -> Result<Vec<u8>, ReadError<E>>
where
    S: Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ReadError::Transport)?;
        let chunk = chunk.as_ref();
        if buf.len() + chunk.len() > cap {
            return Err(ReadError::TooLarge);
        }
        buf.extend_from_slice(chunk);
    }
    Ok(buf)
}

/// Why a capped body read failed: a transport error, or the size cap being hit.
enum ReadError<E> {
    Transport(E),
    TooLarge,
}

/// Perform the catalog HTTP request and classify the outcome. Isolated so the
/// decision logic in [`resolve_catalog`] stays pure and testable.
async fn fetch_catalog(access: &OAuthAccess, etag: Option<String>) -> FetchOutcome {
    let client = match reqwest::Client::builder()
        .timeout(CATALOG_HTTP_TIMEOUT)
        // The catalog endpoint does not redirect. Refuse to follow one: reqwest
        // strips `Authorization` across origins, but NOT our custom
        // `ChatGPT-Account-Id`, so an open redirect on the host would hand the
        // account id to a third party.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => return FetchOutcome::Recoverable(format!("client build failed: {e}")),
    };

    let url = format!("{CHATGPT_CODEX_BASE_URL}/models");
    let mut req = client
        .get(&url)
        .query(&[("client_version", CODEX_CATALOG_COMPAT_VERSION)])
        .bearer_auth(&access.access);
    if let Some(id) = &access.account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    if let Some(tag) = &etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return FetchOutcome::Recoverable("request timed out".into()),
        Err(_) => return FetchOutcome::Recoverable("endpoint unreachable".into()),
    };

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return FetchOutcome::NotModified;
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return FetchOutcome::AuthFailed;
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return FetchOutcome::Recoverable(format!("endpoint returned {status}"));
    }
    if !status.is_success() {
        return FetchOutcome::Recoverable(format!("endpoint returned {status}"));
    }

    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match read_body_capped(Box::pin(resp.bytes_stream()), MAX_CATALOG_BYTES).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(ReadError::TooLarge) => {
            return FetchOutcome::Recoverable("catalog response too large".into());
        }
        Err(ReadError::Transport(_)) => {
            return FetchOutcome::Recoverable("could not read the catalog response".into());
        }
    };
    let value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return FetchOutcome::Recoverable("malformed catalog payload".into()),
    };
    match parse_catalog(&value) {
        Ok(models) if !models.is_empty() => FetchOutcome::Fresh {
            models,
            etag: new_etag,
        },
        // A structurally valid but empty catalog is treated as recoverable so a
        // prior good cache (or the fallback) is preferred over an empty list.
        Ok(_) => FetchOutcome::Recoverable("catalog was empty".into()),
        Err(_) => FetchOutcome::Recoverable("malformed catalog payload".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(models: Value) -> Value {
        serde_json::json!({ "models": models })
    }

    // ── Parser ───────────────────────────────────────────────────────────────

    #[test]
    fn parse_keeps_list_visible_in_upstream_order_ignoring_unknown_fields() {
        let v = payload(serde_json::json!([
            { "slug": "gpt-5.5", "display_name": "GPT-5.5", "context_window": 400000, "surprise": 1 },
            { "slug": "gpt-5.5-codex", "context_window": 272000 },
        ]));
        let out = parse_catalog(&v).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].slug, "gpt-5.5");
        assert_eq!(out[0].label, "GPT-5.5");
        assert_eq!(out[0].context_window, Some(400_000));
        // Missing display_name falls back to the slug; order preserved.
        assert_eq!(out[1].slug, "gpt-5.5-codex");
        assert_eq!(out[1].label, "gpt-5.5-codex");
        assert_eq!(out[1].context_window, Some(272_000));
    }

    #[test]
    fn parse_drops_non_visible_empty_slug_and_unsupported_rows() {
        let v = payload(serde_json::json!([
            { "slug": "shown" },
            { "slug": "hidden", "list_visible": false },
            { "slug": "" },
            { "display_name": "no slug at all" },
            { "slug": "needs-feature", "required_features": ["unsupported"] },
        ]));
        let out = parse_catalog(&v).unwrap();
        assert_eq!(
            out.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            ["shown"]
        );
    }

    #[test]
    fn parse_tolerates_missing_optional_fields() {
        let out = parse_catalog(&payload(serde_json::json!([{ "slug": "m" }]))).unwrap();
        assert_eq!(out[0].context_window, None);
        assert_eq!(out[0].label, "m");
    }

    #[test]
    fn parse_keeps_rows_whose_required_features_are_unrecognised() {
        // The feature gate is a deny-list, not an allow-list: a feature we have
        // never heard of must not hide an entitled model from the picker.
        let v = payload(serde_json::json!([
            { "slug": "novel", "required_features": ["some_future_feature"] },
            { "slug": "mixed", "required_features": ["some_future_feature", "UNSUPPORTED"] },
        ]));
        let out = parse_catalog(&v).unwrap();
        assert_eq!(
            out.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            ["novel"],
            "unknown feature keeps the row; a known-unsupported one (any case) drops it"
        );
    }

    #[test]
    fn parse_rejects_a_context_window_we_cannot_act_on() {
        // `Some(0)` would tell `apply_choice` the window is known, suppressing the
        // endpoint probe and silently disabling the context gauge and
        // auto-compaction for the session. A >=2^32 value must not wrap into one.
        let v = payload(serde_json::json!([
            { "slug": "zero", "context_window": 0 },
            { "slug": "huge", "context_window": 4_294_967_296u64 },
            { "slug": "wraps", "context_window": 4_294_967_297u64 },
            { "slug": "sane", "context_window": 400_000 },
        ]));
        let out = parse_catalog(&v).unwrap();
        let windows: Vec<(&str, Option<u32>)> = out
            .iter()
            .map(|m| (m.slug.as_str(), m.context_window))
            .collect();
        assert_eq!(
            windows,
            [
                ("zero", None),
                ("huge", None),
                ("wraps", None),
                ("sane", Some(400_000)),
            ],
            "an unusable window is None (probe it), never Some(0)"
        );
    }

    #[test]
    fn parse_errors_on_malformed_payload() {
        assert!(parse_catalog(&serde_json::json!({ "nope": 1 })).is_err());
        assert!(parse_catalog(&serde_json::json!("not even an object")).is_err());
    }

    #[test]
    fn parse_accepts_data_array_and_id_field_aliases() {
        let v = serde_json::json!({ "data": [{ "id": "aliased", "max_context_window": 8000 }] });
        let out = parse_catalog(&v).unwrap();
        assert_eq!(out[0].slug, "aliased");
        assert_eq!(out[0].context_window, Some(8000));
    }

    // ── Fallback ─────────────────────────────────────────────────────────────

    #[test]
    fn fallback_derives_from_builtin_provider_not_literals() {
        let fb = builtin_fallback();
        let p = builtin_provider("chatgpt").unwrap();
        assert_eq!(fb.len(), 1);
        assert_eq!(Some(&fb[0].slug), p.model.as_ref());
        assert_eq!(fb[0].context_window, p.context_window);
    }

    // ── Cache freshness / matching ──────────────────────────────────────────

    fn entry(digest: &str, fetched_ms: u64) -> CacheFile {
        CacheFile {
            schema: CATALOG_CACHE_SCHEMA,
            account_digest: digest.to_string(),
            compat_version: CODEX_CATALOG_COMPAT_VERSION.to_string(),
            fetched_ms,
            etag: Some("etag-1".to_string()),
            models: vec![ChatGptModel {
                slug: "cached".to_string(),
                label: "Cached".to_string(),
                context_window: Some(100),
            }],
        }
    }

    #[test]
    fn cache_fresh_requires_match_and_ttl() {
        let d = account_digest("acct");
        let e = entry(&d, 1_000_000);
        assert!(cache_is_fresh(&e, &d, 1_000_000 + CATALOG_TTL_MS - 1));
        assert!(!cache_is_fresh(&e, &d, 1_000_000 + CATALOG_TTL_MS + 1));
        // Different account digest → not usable at all.
        assert!(!cache_matches(&e, &account_digest("other")));
        // Different compat version → not usable.
        let mut stale_compat = e.clone();
        stale_compat.compat_version = "0.0.0".to_string();
        assert!(!cache_matches(&stale_compat, &d));
    }

    #[test]
    fn cache_round_trips_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("chatgpt_models.json");
        let d = account_digest("acct");
        let e = entry(&d, 42);
        write_cache_at(&path, &e).unwrap(); // creates the parent dir
        let loaded = load_cache_at(&path).unwrap();
        assert_eq!(loaded.account_digest, d);
        assert_eq!(loaded.models, e.models);
        // Cache holds no bearer token or raw account id.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("bearer"));
        assert!(!raw.contains("acct"));
    }

    #[test]
    fn cached_window_reads_the_persisted_per_model_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatgpt_models.json");
        let mut e = entry(&account_digest("acct"), 0);
        e.models = vec![
            ChatGptModel {
                slug: "gpt-5.5".to_string(),
                label: "GPT-5.5".to_string(),
                context_window: Some(272_000),
            },
            ChatGptModel {
                slug: "gpt-5.3-codex-spark".to_string(),
                label: "Spark".to_string(),
                context_window: Some(128_000),
            },
            ChatGptModel {
                slug: "no-window".to_string(),
                label: "x".to_string(),
                context_window: None,
            },
        ];
        write_cache_at(&path, &e).unwrap();

        // Each entitled model resolves to its OWN advertised window — not a
        // single provider-wide constant.
        assert_eq!(cached_context_window_at(&path, "gpt-5.5"), Some(272_000));
        assert_eq!(
            cached_context_window_at(&path, "gpt-5.3-codex-spark"),
            Some(128_000)
        );
        // A row that advertised no window, and a slug not in the cache, are both
        // `None` — the caller falls back rather than inventing a number.
        assert_eq!(cached_context_window_at(&path, "no-window"), None);
        assert_eq!(cached_context_window_at(&path, "absent"), None);
        // A missing file is `None`, not an error.
        assert_eq!(
            cached_context_window_at(&dir.path().join("nope.json"), "gpt-5.5"),
            None
        );

        // A cache written under an incompatible schema is not trusted, even though
        // it deserializes — the row layout may mean something else.
        let mut bad = e.clone();
        bad.schema = CATALOG_CACHE_SCHEMA + 1;
        let bad_path = dir.path().join("bad_schema.json");
        write_cache_at(&bad_path, &bad).unwrap();
        assert_eq!(cached_context_window_at(&bad_path, "gpt-5.5"), None);
    }

    /// ENTITLEMENT is account-scoped, and a caller REFUSES on the strength of it —
    /// so every gate is load-bearing. `None` must mean "we do not know", never
    /// "nothing is entitled": a cold cache, another account's cache, or a layout bump
    /// would otherwise refuse a model the user is paying for.
    ///
    /// Contrast [`cached_context_window_at`], which is deliberately laxer: a model's
    /// window is the same for every account, so a slug match is authoritative there
    /// no matter who wrote the file.
    #[test]
    fn entitlements_are_only_read_from_this_accounts_own_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatgpt_models.json");
        let e = entry(&account_digest("acct"), 0);
        write_cache_at(&path, &e).unwrap();

        // The account that wrote it sees its rows — however old they are (entitlement
        // does not go stale in five minutes; the serve path refetches, this one only
        // needs to know what is on the list).
        let rows = cached_entitlements_at(&path, "acct").expect("this account's own cache");
        assert_eq!(
            rows.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            ["cached"]
        );

        // ANOTHER account's cache says nothing about this one's entitlements.
        assert_eq!(cached_entitlements_at(&path, "other"), None);
        // A missing file is ignorance, not an empty entitlement list.
        assert_eq!(
            cached_entitlements_at(&dir.path().join("nope.json"), "acct"),
            None
        );
        // An incompatible schema, or a stale compat pin: the rows may not mean what
        // this build thinks they mean, so they are not trusted to refuse with.
        for mutate in [
            (|c: &mut CacheFile| c.schema = CATALOG_CACHE_SCHEMA + 1) as fn(&mut CacheFile),
            |c: &mut CacheFile| c.compat_version = "0.0.0".to_string(),
            // A structurally valid but EMPTY row set is not "nothing is entitled"
            // either — the serve path treats an empty catalog as recoverable.
            |c: &mut CacheFile| c.models.clear(),
        ] {
            let mut bad = e.clone();
            mutate(&mut bad);
            let bad_path = dir.path().join("bad.json");
            write_cache_at(&bad_path, &bad).unwrap();
            assert_eq!(cached_entitlements_at(&bad_path, "acct"), None);
        }
    }

    // ── Decision logic ──────────────────────────────────────────────────────

    fn some_models() -> Vec<ChatGptModel> {
        vec![ChatGptModel {
            slug: "fresh".to_string(),
            label: "Fresh".to_string(),
            context_window: Some(1),
        }]
    }

    #[test]
    fn fresh_persists_and_labels_fresh() {
        let d = account_digest("acct");
        let (res, persist) = resolve_catalog(
            FetchOutcome::Fresh {
                models: some_models(),
                etag: Some("e".to_string()),
            },
            None,
            Some(&d),
            5,
        );
        assert_eq!(res.source, CatalogSource::Fresh);
        assert_eq!(res.models[0].slug, "fresh");
        let persist = persist.expect("persists with a digest");
        assert_eq!(persist.account_digest, d);
        assert_eq!(persist.fetched_ms, 5);
    }

    #[test]
    fn fresh_without_account_does_not_persist() {
        let (_res, persist) = resolve_catalog(
            FetchOutcome::Fresh {
                models: some_models(),
                etag: None,
            },
            None,
            None, // account-less
            5,
        );
        assert!(
            persist.is_none(),
            "account-less credentials never write cache"
        );
    }

    #[test]
    fn auth_failed_never_serves_stale_returns_fallback_with_warning() {
        let d = account_digest("acct");
        let (res, persist) = resolve_catalog(
            FetchOutcome::AuthFailed,
            Some(entry(&d, 1)), // a matching cache exists...
            Some(&d),
            9,
        );
        // ...but 401/403 must NOT serve it.
        assert_eq!(res.source, CatalogSource::BuiltInFallback);
        assert!(res.warning.unwrap().to_lowercase().contains("login"));
        assert!(persist.is_none());
    }

    #[test]
    fn recoverable_serves_matching_stale_cache() {
        let d = account_digest("acct");
        let (res, _) = resolve_catalog(
            FetchOutcome::Recoverable("endpoint unreachable".into()),
            Some(entry(&d, 1)),
            Some(&d),
            9,
        );
        assert_eq!(res.source, CatalogSource::Stale);
        assert_eq!(res.models[0].slug, "cached");
        assert!(res.warning.is_some());
    }

    #[test]
    fn recoverable_without_matching_cache_falls_back() {
        let d = account_digest("acct");
        // Cache belongs to a DIFFERENT account → must not be served.
        let other = entry(&account_digest("other"), 1);
        let (res, _) = resolve_catalog(
            FetchOutcome::Recoverable("timeout".into()),
            Some(other),
            Some(&d),
            9,
        );
        assert_eq!(res.source, CatalogSource::BuiltInFallback);
    }

    #[test]
    fn not_modified_requires_matching_cache_else_fallback() {
        let d = account_digest("acct");
        // Matching cache → served as Fresh, timestamp refreshed.
        let (res, persist) =
            resolve_catalog(FetchOutcome::NotModified, Some(entry(&d, 1)), Some(&d), 50);
        assert_eq!(res.source, CatalogSource::Fresh);
        assert_eq!(persist.unwrap().fetched_ms, 50);
        // No matching cache → fallback.
        let (res2, _) = resolve_catalog(FetchOutcome::NotModified, None, Some(&d), 50);
        assert_eq!(res2.source, CatalogSource::BuiltInFallback);
    }

    // ── Capped streaming body read ───────────────────────────────────────────

    /// A stream whose total exceeds `cap` aborts early with `TooLarge`, and the
    /// aborting chunk is never appended — so peak allocation stays at or below
    /// `cap` (well under the full body it refused to buffer).
    #[tokio::test]
    async fn read_body_capped_aborts_over_cap_without_buffering_whole_body() {
        let cap = 10;
        // Total 12 bytes across 4-byte chunks: 4, 8, then the third would hit 12.
        let chunks: Vec<std::result::Result<Vec<u8>, ()>> =
            vec![Ok(vec![b'a'; 4]), Ok(vec![b'b'; 4]), Ok(vec![b'c'; 4])];
        let stream = futures_util::stream::iter(chunks);
        match read_body_capped(stream, cap).await {
            Err(ReadError::TooLarge) => {}
            _ => panic!("expected TooLarge"),
        }
    }

    /// A single chunk larger than `cap` is rejected without being appended.
    #[tokio::test]
    async fn read_body_capped_rejects_first_oversized_chunk() {
        let cap = 8;
        let chunks: Vec<std::result::Result<Vec<u8>, ()>> = vec![Ok(vec![b'x'; 100])];
        let stream = futures_util::stream::iter(chunks);
        assert!(matches!(
            read_body_capped(stream, cap).await,
            Err(ReadError::TooLarge)
        ));
    }

    /// A stream at or under `cap` yields the full body intact, in order.
    #[tokio::test]
    async fn read_body_capped_under_cap_returns_full_body() {
        let cap = 10;
        let chunks: Vec<std::result::Result<Vec<u8>, ()>> =
            vec![Ok(b"hel".to_vec()), Ok(b"lo".to_vec())];
        let stream = futures_util::stream::iter(chunks);
        let body = read_body_capped(stream, cap).await.ok().unwrap();
        assert_eq!(body, b"hello");
    }

    /// A body exactly `cap` bytes is accepted (matches the original `<=` bound).
    #[tokio::test]
    async fn read_body_capped_accepts_exactly_cap() {
        let cap = 6;
        let chunks: Vec<std::result::Result<Vec<u8>, ()>> =
            vec![Ok(b"abc".to_vec()), Ok(b"def".to_vec())];
        let stream = futures_util::stream::iter(chunks);
        let body = read_body_capped(stream, cap).await.ok().unwrap();
        assert_eq!(body, b"abcdef");
    }

    /// A transport error mid-stream surfaces as `Transport`, not `TooLarge`.
    #[tokio::test]
    async fn read_body_capped_propagates_transport_error() {
        let cap = 100;
        let chunks: Vec<std::result::Result<Vec<u8>, &str>> = vec![Ok(b"ok".to_vec()), Err("boom")];
        let stream = futures_util::stream::iter(chunks);
        assert!(matches!(
            read_body_capped(stream, cap).await,
            Err(ReadError::Transport("boom"))
        ));
    }
}
