//! The models.dev catalog: per-`provider/model` metadata, used here for one
//! thing — a model's context window.
//!
//! Most OpenAI-compatible endpoints never put the window on the wire. `GET
//! /v1/models` returns `{id, object, created, owned_by}` and nothing else, so
//! [`crate::Client::context_window`] comes back empty and the status bar has no
//! "of Y" to show. The number does exist, just in a catalog rather than an API:
//! <https://models.dev/api.json>, keyed `provider → models → model → limit.context`.
//!
//! Reads are served from a cache file under the XDG cache dir. The catalog is
//! fetched only when that file is missing or older than [`CACHE_TTL`], and a
//! failed fetch falls back to whatever stale copy exists — a context window that
//! is a few days out of date beats none at all. Callers run this off the UI
//! thread; it never blocks a frame.
//!
//! Escape hatches, mirroring opencode's:
//!
//! * `HRDR_MODELS_URL` — fetch the catalog from somewhere else.
//! * `HRDR_MODELS_PATH` — read this file and never fetch (an air-gapped mirror).
//! * `HRDR_DISABLE_MODELS_FETCH` — never fetch; use the cache if one exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::capped_read::MAX_CATALOG_JSON_BYTES;

/// Where the catalog is fetched from, unless `HRDR_MODELS_URL` says otherwise.
const DEFAULT_URL: &str = "https://models.dev";

/// How old the cache may be before it is refetched. The catalog changes when a
/// provider ships a model, not by the minute.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Give up on a slow catalog rather than delay the probe indefinitely.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The catalog read synchronously from the cache (or `HRDR_MODELS_PATH`), for
/// the `/model` selector — it builds a UI list on a keypress and can't await a
/// fetch. `None` when no cache exists yet (the async [`context_window`] path
/// populates it on startup). Never fetches.
///
/// The parsed catalog is memoized per path, keyed by the file's mtime (see
/// [`cached_read`]): the Anthropic branch consults this on every round —
/// `max_tokens` is `None` by default, so every request pays a full 3.5 MB
/// read + JSON parse unless the parse is held resident.
pub fn load_cached() -> Option<Arc<Value>> {
    if let Some(pinned) = std::env::var_os("HRDR_MODELS_PATH") {
        return cached_read(std::path::Path::new(&pinned));
    }
    cached_read(&cache_path()?)
}

/// One memoized parse: the file's mtime at load time and the parsed catalog
/// (`None` when the file was missing or failed to parse — a miss is cached
/// too, so a deleted catalog isn't re-read on every call).
type CatalogEntry = (Option<SystemTime>, Option<Arc<Value>>);

/// The parsed catalog memoized per path, keyed by the file's mtime — see
/// [`cached_read`]. The parsed 3.5 MB `Value` is held resident so a live
/// turn's per-round window/max-output lookups don't re-read and re-parse it.
static CATALOG_LOAD_CACHE: LazyLock<Mutex<HashMap<PathBuf, CatalogEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read and parse the catalog at `path`, memoized by path + mtime: an
/// unchanged file is served from [`CATALOG_LOAD_CACHE`] without touching disk.
///
/// Coarse-clock caveat, stated honestly (the same one as hrdr-agent's
/// `auth_store` cache): on a filesystem whose mtime granularity is coarser
/// than the write spacing, a write landing in the same tick as the last read
/// is missed and the cache serves the pre-write catalog. For this path that
/// degrades gracefully — a context window that is a moment out of date beats
/// none — which is why this cache does not need the fine-mtime probe
/// `hrdr-tools/src/memory.rs` uses for its durability-critical cache.
///
/// A miss is cached too, keyed by the same mtime: a file that doesn't exist
/// stays `None` until it appears (appearing moves the mtime from `None` to
/// `Some`, a miss), and a file that fails to parse stays `None` until its
/// mtime moves.
fn cached_read(path: &std::path::Path) -> Option<Arc<Value>> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut cache = CATALOG_LOAD_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((cached_mtime, value)) = cache.get(path)
        && mtime == *cached_mtime
    {
        return value.clone();
    }
    let value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .map(Arc::new);
    cache.insert(path.to_path_buf(), (mtime, value.clone()));
    value
}

/// For catalog provider `key`, its friendly display name and every model it
/// lists as `(model_id, friendly_name)`. Friendly names fall back to the id/key
/// when the catalog omits a `name`. `None` when the key isn't in the catalog.
/// Pure, so the selector's list-building is testable without a cache.
pub fn provider_models(catalog: &Value, key: &str) -> Option<(String, Vec<(String, String)>)> {
    let p = catalog.get(key)?;
    let provider_name = p
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_string();
    let mut models: Vec<(String, String)> = p
        .get("models")?
        .as_object()?
        .iter()
        .map(|(id, m)| {
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            (id.clone(), name)
        })
        .collect();
    models.sort_by_key(|(_, name)| name.to_lowercase());
    Some((provider_name, models))
}

/// Whether `provider_key` serves `model` **free of charge** — the catalog prices
/// it, and prices it at zero on both sides.
///
/// This is what decides which models an unauthenticated provider may still be
/// offered: OpenCode Zen serves its zero-cost models to anonymous callers (see
/// `hrdr_agent::public_api_key`) and 401s on every priced one, so listing a
/// priced model to a logged-out user is offering something that cannot work.
///
/// An **unpriced** model answers `false`, deliberately. The catalog omitting a
/// `cost` object means *we do not know what this costs*, and "unknown" must not
/// be read as "free" — that is the direction that puts an unusable row in the
/// picker. (opencode's own check is `!model.cost.some(c => c.input > 0)`, which
/// does read a missing price as free; hrdr is stricter on purpose.)
pub fn is_free_model(catalog: &Value, provider_key: &str, model: &str) -> bool {
    let Some(cost) = catalog
        .get(provider_key)
        .and_then(|p| p.get("models"))
        .and_then(|m| m.get(model))
        .and_then(|m| m.get("cost"))
    else {
        return false;
    };
    // Priced at zero, not merely *unpriced*: `Some(0.0)` on both sides.
    let is_zero = |k: &str| cost.get(k).and_then(Value::as_f64) == Some(0.0);
    is_zero("input") && is_zero("output")
}

/// The context window for `model`, from the models.dev catalog.
///
/// `provider` is the configured provider name (`opencode-go`, `openrouter`, …)
/// when there is one; it disambiguates a model that several providers serve with
/// different limits. Without it — or when the name isn't in the catalog — every
/// provider is searched and the *smallest* window any of them reports is used:
/// understating the window only compacts earlier than needed, while overstating
/// it overflows the model's real context.
///
/// `None` when the catalog can't be read or doesn't know the model.
pub async fn context_window(provider: Option<&str>, model: &str) -> Option<u32> {
    lookup(&load().await?, provider, model)
}

/// Make sure the catalog is on disk, fetching it if it is missing or stale.
/// Best-effort and silent: the catalog is a nicety, never a reason to fail a
/// session, and this is spawned rather than awaited by its caller.
///
/// **Why this exists.** Every other consumer either reads the cache without
/// fetching ([`load_cached`], which the `/model` selector must use because it
/// builds its list on a keypress) or fetches only as a *side effect* of needing
/// one model's window. So the catalog was populated by luck: an endpoint that
/// answered `/v1/models` with a window, or a window already known from config or
/// cache, meant nothing ever fetched, and the selector had no models to offer —
/// on a fresh install, forever. Warming it explicitly at startup is what makes
/// the selector's list a property of hrdr rather than of which code path
/// happened to run first.
pub async fn warm() {
    let _ = load().await;
}

/// [`context_window`] against the **already-cached** catalog only — no network, no
/// await. For callers inside a live turn (the agent deciding whether its context is
/// full), where firing an out-of-band HTTP request would interleave with the stream
/// it is about to open. `None` when the catalog isn't cached or doesn't know the
/// model; the caller simply doesn't get to compact proactively.
pub fn context_window_cached(provider: Option<&str>, model: &str) -> Option<u32> {
    lookup(load_cached()?.as_ref(), provider, model)
}

/// Find `model`'s window in an already-loaded catalog. Pure, so the resolution
/// rules are testable without a cache or a network.
///
/// `provider` is a models.dev **catalog key**, not an app provider name — the two
/// are different namespaces (hrdr's `zen` is models.dev's `opencode`). Callers
/// map before they ask; a name that isn't a catalog key silently falls through to
/// the cross-provider scan below.
pub fn lookup(catalog: &Value, provider: Option<&str>, model: &str) -> Option<u32> {
    let window = |p: &Value| -> Option<u32> {
        p.get("models")?
            .get(model)?
            .get("limit")?
            .get("context")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
    };
    // The configured provider's own answer, when the catalog has one.
    if let Some(name) = provider
        && let Some(p) = catalog.get(name)
        && let Some(n) = window(p)
    {
        return Some(n);
    }
    // Otherwise the most conservative window offered for this model id.
    catalog.as_object()?.values().filter_map(window).min()
}

/// The largest `max_tokens` `model` accepts, from the already-cached catalog —
/// no network, no await, for callers building a request inside a live turn.
///
/// Same shape as [`context_window_cached`], reading `limit.output` instead.
/// `None` when the catalog isn't cached or doesn't know the model; the caller
/// then falls back to its own conservative default.
pub fn max_output_cached(provider: Option<&str>, model: &str) -> Option<u32> {
    lookup_max_output(load_cached()?.as_ref(), provider, model)
}

/// Find `model`'s output cap in an already-loaded catalog. Pure, so the
/// resolution rules are testable without a cache or a network.
///
/// Resolution mirrors [`lookup`] — the configured provider's own answer first,
/// then the **smallest** cap any provider reports for this model id. Small is
/// the safe direction here for the same reason it is for the context window,
/// but with a harder edge: asking for more output tokens than the model's real
/// cap is a 400, so an overstatement fails every request rather than merely
/// wasting a window.
pub fn lookup_max_output(catalog: &Value, provider: Option<&str>, model: &str) -> Option<u32> {
    let output = |p: &Value| -> Option<u32> {
        p.get("models")?
            .get(model)?
            .get("limit")?
            .get("output")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
    };
    if let Some(name) = provider
        && let Some(p) = catalog.get(name)
        && let Some(n) = output(p)
    {
        return Some(n);
    }
    catalog.as_object()?.values().filter_map(output).min()
}

/// A model's price card: **USD per million tokens**, from the catalog's
/// `cost` object. `cache_read` is the discounted rate for prompt tokens served
/// from the provider's prompt cache; `cache_write` the *premium* rate for
/// prompt tokens written into it. Either is `None` when the provider doesn't
/// publish one, and [`ModelCost::call_cost`] then falls back — to the full
/// input rate for reads, to a multiple of it for writes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    /// Rate for prompt tokens **written** into the cache. See
    /// [`CACHE_WRITE_5M_MULTIPLIER`] for what happens when it's absent.
    pub cache_write: Option<f64>,
}

/// What a **5-minute** cache write costs relative to plain input: **1.25x**.
/// Used only when the catalog publishes no `cache_write` rate for the model.
///
/// Anthropic's own worked example, Claude Opus 5 at $5/MTok input: a 5m cache
/// write is $6.25/MTok, a 1h write $10/MTok, a cache read $0.50/MTok.
const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;

/// What a **1-hour** cache write costs relative to plain input: **2x** (see
/// [`CACHE_WRITE_5M_MULTIPLIER`]).
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

impl ModelCost {
    /// Estimated USD for one model call.
    ///
    /// `prompt_tokens` is the **inclusive** prompt total the provider reported,
    /// and is partitioned three ways — this is the part that has to be exactly
    /// right, since both `/cost` and the budget cap run off this number:
    ///
    /// | bucket        | tokens                    | rate                       |
    /// |---------------|---------------------------|----------------------------|
    /// | cache read    | `cached`                  | `cache_read` (≈0.1x input) |
    /// | cache write   | `cache_creation`          | `cache_write` (see below)  |
    /// | plain input   | whatever is left over     | `input`                    |
    ///
    /// The three are disjoint and sum to `prompt_tokens` exactly: `cached` is
    /// clamped to the prompt, `cache_creation` to what remains after it, and
    /// plain input is the remainder — so no bucket can go negative and none can
    /// be counted twice, whatever a provider reports.
    ///
    /// Cache **writes** are billed at a premium, not at the plain input rate:
    /// 1.25x for the 5-minute TTL, 2x for the 1-hour one. hrdr writes the cache
    /// on essentially every turn (the rolling `cache_control` breakpoint in
    /// [`crate::apply_cache_breakpoints`] / `anthropic::build_body`), so pricing
    /// those tokens as plain input — as this did before — understates every
    /// priced session and, worse, loosens the budget cap that stops the agent.
    ///
    /// `ttl_1h` selects which fallback multiplier applies; it is ignored when
    /// the catalog publishes a real `cache_write` rate. Completion tokens
    /// (reasoning included) go at the output rate.
    pub fn call_cost(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        cached: Option<u32>,
        cache_creation: Option<u32>,
        ttl_1h: bool,
    ) -> f64 {
        const M: f64 = 1_000_000.0;
        // Clamp in sequence so read + write can never exceed the prompt, and
        // the plain-input remainder can never go negative.
        let read = cached.unwrap_or(0).min(prompt_tokens);
        let written = cache_creation.unwrap_or(0).min(prompt_tokens - read);
        let plain = prompt_tokens - read - written;
        let write_rate = self
            .cache_write
            .unwrap_or(self.input * write_multiplier(ttl_1h));
        f64::from(plain) / M * self.input
            + f64::from(read) / M * self.cache_read.unwrap_or(self.input)
            + f64::from(written) / M * write_rate
            + f64::from(completion_tokens) / M * self.output
    }
}

/// The fallback cache-write multiplier for the TTL in force.
fn write_multiplier(ttl_1h: bool) -> f64 {
    if ttl_1h {
        CACHE_WRITE_1H_MULTIPLIER
    } else {
        CACHE_WRITE_5M_MULTIPLIER
    }
}

/// The price card for `model`, from the models.dev catalog. Same resolution
/// as [`context_window`]: the configured provider's own entry wins; without
/// one, every provider serving this model id is searched and the **most
/// expensive** offering is used — overstating an estimate is the safe
/// direction for a cost display and a budget cap alike. `None` when the
/// catalog can't be read or doesn't price the model (e.g. a local server).
pub async fn model_cost(provider: Option<&str>, model: &str) -> Option<ModelCost> {
    lookup_cost(&load().await?, provider, model)
}

/// Find `model`'s price card in an already-loaded catalog. Pure, so the
/// resolution rules are testable without a cache or a network.
fn lookup_cost(catalog: &Value, provider: Option<&str>, model: &str) -> Option<ModelCost> {
    let cost = |p: &Value| -> Option<ModelCost> {
        let c = p.get("models")?.get(model)?.get("cost")?;
        let f = |k: &str| c.get(k).and_then(Value::as_f64);
        Some(ModelCost {
            input: f("input")?,
            output: f("output")?,
            cache_read: f("cache_read"),
            // models.dev carries this for providers that publish it; absent
            // leaves `None`, and `call_cost` falls back to a multiple of the
            // input rate rather than pretending a write is plain input.
            cache_write: f("cache_write"),
        })
    };
    if let Some(name) = provider
        && let Some(p) = catalog.get(name)
        && let Some(c) = cost(p)
    {
        return Some(c);
    }
    catalog
        .as_object()?
        .values()
        .filter_map(cost)
        .max_by(|a, b| (a.input + a.output).total_cmp(&(b.input + b.output)))
}

/// The reasoning-effort levels `model` accepts, from the catalog's
/// `reasoning_options` (`{type: "effort", values: […]}` entries). Values come
/// back in the catalog's order (lowest effort first). Resolution mirrors
/// [`context_window`]: the configured provider's entry wins, else the first
/// provider serving this model id that declares effort levels. `None` when the
/// catalog doesn't know the model (or it has no effort control).
pub fn lookup_effort_levels(
    catalog: &Value,
    provider: Option<&str>,
    model: &str,
) -> Option<Vec<String>> {
    let efforts = |p: &Value| -> Option<Vec<String>> {
        let opts = p.get("models")?.get(model)?.get("reasoning_options")?;
        let values: Vec<String> = opts
            .as_array()?
            .iter()
            .find(|o| o.get("type").and_then(Value::as_str) == Some("effort"))?
            .get("values")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        (!values.is_empty()).then_some(values)
    };
    if let Some(name) = provider
        && let Some(p) = catalog.get(name)
        && let Some(v) = efforts(p)
    {
        return Some(v);
    }
    catalog.as_object()?.values().find_map(efforts)
}

/// The cache file, `$XDG_CACHE_HOME/hrdr/models.json`.
fn cache_path() -> Option<PathBuf> {
    Some(hjkl_xdg::cache_dir("hrdr").ok()?.join("models.json"))
}

/// Whether `path` exists and was written within `ttl`.
pub fn is_fresh(path: &std::path::Path, ttl: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age < ttl)
}

/// The catalog: a pinned file, else a fresh cache, else a fetch (cached on
/// success), else whatever stale cache is on disk.
async fn load() -> Option<Value> {
    let read = |p: &std::path::Path| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };

    // A pinned file is authoritative: read it, never fetch, never cache.
    if let Some(pinned) = std::env::var_os("HRDR_MODELS_PATH") {
        return read(std::path::Path::new(&pinned));
    }

    let cache = cache_path();
    if let Some(p) = &cache
        && is_fresh(p, CACHE_TTL)
        && let Some(v) = read(p)
    {
        return Some(v);
    }
    let stale = cache.as_deref().and_then(read);
    if std::env::var_os("HRDR_DISABLE_MODELS_FETCH").is_some() {
        return stale;
    }

    match fetch().await {
        Some(v) => {
            if let Some(p) = &cache {
                write_cache(p, &v);
            }
            Some(v)
        }
        // The network is down or models.dev moved: a stale window beats none.
        None => stale,
    }
}

/// GET `{base}/api.json`, decoded. `None` on any error — the catalog is a
/// nicety, never a reason to fail a session.
async fn fetch() -> Option<Value> {
    let base = std::env::var("HRDR_MODELS_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("hrdr/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/api.json", base.trim_end_matches('/')))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    // The catalog gets its own, far larger cap — see `MAX_CATALOG_JSON_BYTES`. The
    // shared 1 MiB one had been rejecting the whole 3 MiB catalog, silently, for
    // as long as it had been that big.
    crate::capped_read::read_capped_json(resp, MAX_CATALOG_JSON_BYTES)
        .await
        .ok()
}

/// Write the catalog to `path` via a temporary file in the same directory, so a
/// crash or a concurrent hrdr can't leave a half-written cache behind. Failure
/// is ignored: the caller already has the value in hand.
///
/// The temp name comes from [`crate::unique_sibling_path`] (paired with the
/// PID and a process-wide counter): two concurrent hrdr processes both
/// refreshing a stale catalog would otherwise share one fixed temp name — one
/// truncating the other's in-flight write, and their renames racing to
/// publish a partially-written file.
fn write_cache(path: &std::path::Path, v: &Value) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = crate::unique_sibling_path(path, "hrdr-tmp");
    let Some(s) = serde_json::to_string(v).ok() else {
        return;
    };
    // Owner-only: the cached catalog is fetched with the user's credentials and
    // can name provider/model entitlements that are theirs to know, so it gets
    // the same treatment as the rest of hrdr's on-disk state
    // ([`crate::fs::owner_only_options`] states what that is worth per platform).
    let mut opts = crate::fs::owner_only_options();
    opts.write(true).create(true).truncate(true);
    if opts
        .open(&tmp)
        .ok()
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(s.as_bytes()).ok()
        })
        .is_some()
    {
        let _ = std::fs::rename(&tmp, path);
    }
    let _ = std::fs::remove_file(&tmp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Value {
        json!({
            "opencode-go": { "models": {
                "deepseek-v4-flash": { "limit": { "context": 1_000_000, "output": 384_000 } },
            }},
            "cortecs": { "models": {
                "deepseek-v4-flash": { "limit": { "context": 1_048_576 } },
            }},
            "openai": { "models": {
                "gpt-5": { "limit": { "context": 400_000 } },
                // A model whose entry carries no limit at all.
                "weird": { "id": "weird" },
            }},
        })
    }

    /// Output-cap resolution: the configured provider's own `limit.output`
    /// wins; without one the smallest cap any provider reports is used, since
    /// asking for more `max_tokens` than a model allows is a 400.
    #[test]
    fn lookup_max_output_prefers_provider_then_smallest() {
        let c = json!({
            "anthropic": { "models": {
                "claude-opus-5": { "limit": { "context": 1_000_000, "output": 128_000 } },
                "claude-opus-4-1": { "limit": { "context": 200_000, "output": 32_000 } },
                // A model whose entry carries no limit at all.
                "weird": { "id": "weird" },
            }},
            "some-reseller": { "models": {
                "claude-opus-5": { "limit": { "context": 200_000, "output": 64_000 } },
                // A zero cap is a missing one, not a real answer.
                "claude-opus-4-1": { "limit": { "output": 0 } },
            }},
        });
        assert_eq!(
            lookup_max_output(&c, Some("anthropic"), "claude-opus-5"),
            Some(128_000)
        );
        // No provider (hrdr's provider names are a different namespace from the
        // catalog's keys) → the most conservative cap on offer.
        assert_eq!(lookup_max_output(&c, None, "claude-opus-5"), Some(64_000));
        // An unknown provider key falls through to the same scan.
        assert_eq!(
            lookup_max_output(&c, Some("nope"), "claude-opus-5"),
            Some(64_000)
        );
        // Zero-capped entries are skipped rather than winning the `min`.
        assert_eq!(lookup_max_output(&c, None, "claude-opus-4-1"), Some(32_000));
        // Unknown model, and a known model with no `limit` at all.
        assert_eq!(lookup_max_output(&c, None, "claude-opus-9"), None);
        assert_eq!(lookup_max_output(&c, Some("anthropic"), "weird"), None);
    }

    /// The price-card resolution: the configured provider wins; otherwise the
    /// most expensive offering across providers; `None` when unpriced.
    #[test]
    fn lookup_cost_prefers_provider_then_most_expensive() {
        let c = json!({
            "cheap": { "models": {
                "m": { "cost": {
                    "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25,
                }},
            }},
            "pricey": { "models": {
                "m": { "cost": { "input": 10.0, "output": 50.0 } },
            }},
            "local": { "models": { "m": {} } },
        });
        // The configured provider's own card.
        assert_eq!(
            lookup_cost(&c, Some("cheap"), "m"),
            Some(ModelCost {
                input: 1.0,
                output: 5.0,
                cache_read: Some(0.1),
                cache_write: Some(1.25),
            })
        );
        // A card without a `cache_write` key leaves it `None` (→ the fallback
        // multiplier in `call_cost`), never `Some(0.0)`.
        assert_eq!(
            lookup_cost(&c, Some("pricey"), "m").unwrap().cache_write,
            None
        );
        // No provider → the most expensive offering (safe overestimate).
        assert_eq!(lookup_cost(&c, None, "m").unwrap().input, 10.0);
        // Unknown provider falls back the same way.
        assert_eq!(lookup_cost(&c, Some("nope"), "m").unwrap().output, 50.0);
        // Unpriced model → None.
        assert_eq!(lookup_cost(&c, Some("local"), "unpriced"), None);
    }

    /// Call pricing: cached prompt tokens get the cache-read rate (input rate
    /// when the provider doesn't publish one).
    #[test]
    fn call_cost_prices_cached_tokens_at_the_discount() {
        let card = ModelCost {
            input: 10.0,
            output: 50.0,
            cache_read: Some(1.0),
            cache_write: None,
        };
        // 1M uncached prompt + 1M output = $10 + $50.
        assert!((card.call_cost(1_000_000, 1_000_000, None, None, false) - 60.0).abs() < 1e-9);
        // Fully cached prompt: $1 instead of $10.
        assert!((card.call_cost(1_000_000, 0, Some(1_000_000), None, false) - 1.0).abs() < 1e-9);
        // Cached count is clamped to the prompt size.
        assert!(
            (card.call_cost(100, 0, Some(1_000), None, false)
                - card.call_cost(100, 0, Some(100), None, false))
            .abs()
                < 1e-12
        );
        // No published cache rate → cached tokens cost the full input rate.
        let flat = ModelCost {
            input: 10.0,
            output: 50.0,
            cache_read: None,
            cache_write: None,
        };
        assert!((flat.call_cost(1_000_000, 0, Some(1_000_000), None, false) - 10.0).abs() < 1e-9);
    }

    /// The three prompt buckets — plain input, cache **read**, cache **write** —
    /// partition `prompt_tokens` exactly: disjoint, non-negative, summing to the
    /// whole, whatever a provider reports. This is the part `/cost` and the
    /// `max_cost` cap both ride on, so it is checked by reconstruction rather
    /// than by a single expected total.
    #[test]
    fn call_cost_partitions_the_prompt_three_ways() {
        // Distinct rates, so a token landing in the wrong bucket shows up.
        let card = ModelCost {
            input: 10.0,
            output: 0.0,
            cache_read: Some(1.0),
            cache_write: Some(100.0),
        };
        // 1M prompt = 200k read + 300k write + 500k plain.
        let got = card.call_cost(1_000_000, 0, Some(200_000), Some(300_000), false);
        let want = 0.2 * 1.0 + 0.3 * 100.0 + 0.5 * 10.0;
        assert!((got - want).abs() < 1e-9, "{got} != {want}");

        // Every split of a fixed prompt costs the sum of its parts priced
        // individually — the buckets never overlap and never leave a gap.
        for (read, write) in [(0, 0), (0, 1_000), (1_000, 0), (400, 600), (999, 1)] {
            let plain = 1_000 - read - write;
            let want = f64::from(read) / 1e6 * 1.0
                + f64::from(write) / 1e6 * 100.0
                + f64::from(plain) / 1e6 * 10.0;
            let got = card.call_cost(1_000, 0, Some(read), Some(write), false);
            assert!((got - want).abs() < 1e-12, "read={read} write={write}");
        }

        // Clamping: counts larger than the prompt (or than what's left of it)
        // can't push another bucket negative or double-count. A read that
        // swallows the whole prompt leaves nothing to write.
        assert!(
            (card.call_cost(100, 0, Some(9_999), Some(9_999), false)
                - card.call_cost(100, 0, Some(100), Some(0), false))
            .abs()
                < 1e-12
        );
        // A write larger than the prompt is clamped to the prompt.
        assert!(
            (card.call_cost(100, 0, None, Some(9_999), false)
                - card.call_cost(100, 0, None, Some(100), false))
            .abs()
                < 1e-12
        );
        // Read + write over the prompt: the write absorbs only the remainder.
        assert!(
            (card.call_cost(100, 0, Some(60), Some(80), false)
                - card.call_cost(100, 0, Some(60), Some(40), false))
            .abs()
                < 1e-12
        );

        // A call with no cache activity at all prices exactly as it did before
        // cache-write pricing existed: every prompt token at the input rate.
        let plain_only = ModelCost {
            input: 10.0,
            output: 50.0,
            cache_read: Some(1.0),
            cache_write: Some(100.0),
        };
        assert!(
            (plain_only.call_cost(1_000_000, 1_000_000, None, None, false) - 60.0).abs() < 1e-9
        );
        assert!(
            (plain_only.call_cost(1_000_000, 1_000_000, Some(0), Some(0), false) - 60.0).abs()
                < 1e-9
        );
    }

    /// Without a catalog `cache_write` rate, a write is priced as a multiple of
    /// the input rate: **1.25x** at the 5-minute TTL, **2x** at the 1-hour one.
    /// (Anthropic's worked example: $5/MTok input → $6.25 / $10 / $0.50 for a
    /// 5m write, a 1h write, and a read.)
    #[test]
    fn call_cost_falls_back_to_the_write_multiplier() {
        let card = ModelCost {
            input: 5.0,
            output: 25.0,
            cache_read: Some(0.5),
            cache_write: None,
        };
        // 1M cache-write tokens, 5-minute TTL: $6.25.
        assert!((card.call_cost(1_000_000, 0, None, Some(1_000_000), false) - 6.25).abs() < 1e-9);
        // Same call at the 1-hour TTL: $10.
        assert!((card.call_cost(1_000_000, 0, None, Some(1_000_000), true) - 10.0).abs() < 1e-9);
        // And a read is $0.50 either way — the TTL only selects the *write*
        // fallback.
        assert!((card.call_cost(1_000_000, 0, Some(1_000_000), None, true) - 0.5).abs() < 1e-9);

        // A catalog-supplied rate wins outright, and the TTL then changes
        // nothing: the published number already accounts for it.
        let published = ModelCost {
            cache_write: Some(3.0),
            ..card
        };
        assert!(
            (published.call_cost(1_000_000, 0, None, Some(1_000_000), false) - 3.0).abs() < 1e-9
        );
        assert!(
            (published.call_cost(1_000_000, 0, None, Some(1_000_000), true) - 3.0).abs() < 1e-9
        );
    }

    /// Effort-level resolution: the configured provider wins, else the first
    /// provider declaring effort values; models without them yield `None`.
    #[test]
    fn lookup_effort_levels_reads_reasoning_options() {
        let c = json!({
            "a": { "models": {
                "m": { "reasoning_options": [
                    { "type": "effort", "values": ["low", "medium", "high"] },
                ]},
                "budget-only": { "reasoning_options": [
                    { "type": "budget_tokens", "min": 1024 },
                ]},
            }},
            "b": { "models": {
                "m": { "reasoning_options": [
                    { "type": "effort", "values": ["minimal", "low"] },
                ]},
            }},
        });
        assert_eq!(
            lookup_effort_levels(&c, Some("b"), "m"),
            Some(vec!["minimal".to_string(), "low".to_string()])
        );
        // No provider → first one that declares effort levels.
        assert!(lookup_effort_levels(&c, None, "m").is_some());
        // budget_tokens-only models have no effort *values*.
        assert_eq!(lookup_effort_levels(&c, Some("a"), "budget-only"), None);
        assert_eq!(lookup_effort_levels(&c, None, "unknown"), None);
    }

    /// `provider_models` returns the provider's friendly name and its models,
    /// each with a friendly name (falling back to the id), sorted by that name.
    #[test]
    fn provider_models_returns_friendly_names_sorted() {
        let c = json!({
            "opencode": { "name": "OpenCode Zen", "models": {
                "z-gpt": { "name": "GPT-5.6" },
                "a-claude": { "name": "Claude Fable 5.0" },
                "no-name-model": {},
            }},
        });
        let (provider, models) = provider_models(&c, "opencode").unwrap();
        assert_eq!(provider, "OpenCode Zen");
        // Sorted by friendly name: "Claude Fable 5.0" < "GPT-5.6" < "no-name-model".
        assert_eq!(
            models,
            vec![
                ("a-claude".to_string(), "Claude Fable 5.0".to_string()),
                ("z-gpt".to_string(), "GPT-5.6".to_string()),
                ("no-name-model".to_string(), "no-name-model".to_string()),
            ]
        );
        // An unknown provider key yields nothing.
        assert!(provider_models(&c, "nope").is_none());
        // A provider with no `name` falls back to its key.
        let c2 = json!({ "x": { "models": { "m": {} } } });
        assert_eq!(provider_models(&c2, "x").unwrap().0, "x");
    }

    /// Free means **priced, at zero, on both sides**. An unpriced model is
    /// unknown, not free — the direction that keeps an unusable row out of an
    /// anonymous picker.
    #[test]
    fn free_models_are_the_ones_priced_at_zero() {
        let c = json!({
            "opencode": { "models": {
                "grok-code": { "cost": { "input": 0, "output": 0, "cache_read": 0 } },
                "claude-opus-5": { "cost": { "input": 5, "output": 25 } },
                // Zero in, priced out: not free.
                "half": { "cost": { "input": 0, "output": 2 } },
                // No `cost` object at all: unknown, so not offered.
                "unpriced": { "limit": { "context": 1000 } },
            }},
        });
        assert!(is_free_model(&c, "opencode", "grok-code"));
        assert!(!is_free_model(&c, "opencode", "claude-opus-5"));
        assert!(!is_free_model(&c, "opencode", "half"));
        assert!(!is_free_model(&c, "opencode", "unpriced"));
        // Unknown provider / model / empty catalog are all "not free".
        assert!(!is_free_model(&c, "nope", "grok-code"));
        assert!(!is_free_model(&c, "opencode", "nope"));
        assert!(!is_free_model(&json!({}), "opencode", "grok-code"));
    }

    /// The configured provider's own number wins — the same model is served with
    /// different windows by different providers.
    #[test]
    fn the_named_provider_decides() {
        let c = catalog();
        assert_eq!(
            lookup(&c, Some("opencode-go"), "deepseek-v4-flash"),
            Some(1_000_000)
        );
        assert_eq!(
            lookup(&c, Some("cortecs"), "deepseek-v4-flash"),
            Some(1_048_576)
        );
    }

    /// With no provider — or one the catalog doesn't carry — take the smallest
    /// window on offer: compacting early is recoverable, overflowing isn't.
    #[test]
    fn without_a_provider_the_smallest_window_wins() {
        let c = catalog();
        assert_eq!(lookup(&c, None, "deepseek-v4-flash"), Some(1_000_000));
        assert_eq!(
            lookup(&c, Some("not-in-catalog"), "deepseek-v4-flash"),
            Some(1_000_000),
            "an unknown provider falls back to the scan"
        );
        // The named provider knows the model but not its window: scan instead.
        assert_eq!(lookup(&c, Some("openai"), "weird"), None);
    }

    /// An unknown model, an empty catalog, and a zero window are all `None`.
    #[test]
    fn unknown_models_and_junk_windows_are_none() {
        let c = catalog();
        assert_eq!(lookup(&c, Some("openai"), "no-such-model"), None);
        assert_eq!(lookup(&json!({}), None, "gpt-5"), None);
        assert_eq!(lookup(&json!([1, 2]), None, "gpt-5"), None, "not an object");
        let zero = json!({"p": {"models": {"m": {"limit": {"context": 0}}}}});
        assert_eq!(lookup(&zero, Some("p"), "m"), None, "0 is not a window");
    }

    /// A file younger than the TTL is fresh; an older one isn't, and a missing
    /// one never is.
    #[test]
    fn cache_freshness_follows_the_file_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("models.json");
        assert!(!is_fresh(&p, CACHE_TTL), "a missing cache is never fresh");

        std::fs::write(&p, "{}").unwrap();
        assert!(is_fresh(&p, CACHE_TTL), "just written");
        // Zero TTL: anything already on disk is stale.
        assert!(!is_fresh(&p, Duration::ZERO));
    }

    /// The cache lands atomically and leaves no temp file behind.
    #[test]
    fn writing_the_cache_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("models.json");
        write_cache(&p, &catalog());

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            lookup(&back, Some("opencode-go"), "deepseek-v4-flash"),
            Some(1_000_000)
        );
        // No stray temp file left in the directory (either naming scheme).
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != p.file_name().unwrap())
            .collect();
        assert!(leftovers.is_empty(), "temp file cleaned up: {leftovers:?}");
    }

    /// The memoization contract: an unchanged file is served without re-reading
    /// — a same-mtime rewrite is invisible — and moving the mtime re-reads.
    ///
    /// The same-mtime assertion is the regression test for the memo: before it
    /// existed, `cached_read` re-read the file on every call and returned the
    /// new content, so this assertion fails on the old code.
    #[test]
    fn cached_read_serves_the_memoized_parse_until_the_mtime_moves() {
        let _guard = catalog_cache_test_guard();
        reset_catalog_cache_for_test();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("models.json");
        let write = |json: &str| std::fs::write(&p, json).unwrap();
        let pin_mtime = |t: SystemTime| {
            std::fs::File::open(&p)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        };

        write(r#"{"a": {"models": {"m": {"limit": {"context": 1000}}}}}"#);
        let t0 = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(lookup(&cached_read(&p).unwrap(), None, "m"), Some(1000));

        // Rewrite with different content, then pin the mtime back to the
        // cached one: the memo serves the first parse (the file is not
        // re-read).
        write(r#"{"b": {"models": {"m": {"limit": {"context": 2000}}}}}"#);
        pin_mtime(t0);
        assert_eq!(lookup(&cached_read(&p).unwrap(), None, "m"), Some(1000));

        // Move the mtime forward: the file is re-read and the new content
        // wins.
        pin_mtime(t0 + Duration::from_secs(1));
        assert_eq!(lookup(&cached_read(&p).unwrap(), None, "m"), Some(2000));
    }

    /// Clear the process-local [`CATALOG_LOAD_CACHE`] — test isolation: a
    /// recycled tempdir path landing in the same coarse mtime tick as a
    /// previous test's write could serve that test's parse.
    pub(crate) fn reset_catalog_cache_for_test() {
        CATALOG_LOAD_CACHE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Serialize the tests that touch [`CATALOG_LOAD_CACHE`] against each
    /// other: the harness runs `#[test]`s on parallel threads, and a reset
    /// mid-assertion is nondeterministic. Hold the guard for the whole test
    /// body.
    pub(crate) fn catalog_cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
