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
    let body = match resp.text().await {
        Ok(t) => t,
        Err(_) => return FetchOutcome::Recoverable("could not read the catalog response".into()),
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
}
