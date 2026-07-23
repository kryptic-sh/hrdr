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

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::capped_read::MAX_STRUCTURED_JSON_BYTES;

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
pub fn load_cached() -> Option<Value> {
    let read = |p: &std::path::Path| serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok();
    if let Some(pinned) = std::env::var_os("HRDR_MODELS_PATH") {
        return read(std::path::Path::new(&pinned));
    }
    read(&cache_path()?)
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

/// [`context_window`] against the **already-cached** catalog only — no network, no
/// await. For callers inside a live turn (the agent deciding whether its context is
/// full), where firing an out-of-band HTTP request would interleave with the stream
/// it is about to open. `None` when the catalog isn't cached or doesn't know the
/// model; the caller simply doesn't get to compact proactively.
pub fn context_window_cached(provider: Option<&str>, model: &str) -> Option<u32> {
    lookup(&load_cached()?, provider, model)
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

/// A model's price card: **USD per million tokens**, from the catalog's
/// `cost` object. `cache_read` is the discounted rate for prompt tokens served
/// from the provider's prompt cache; `None` when the provider doesn't publish
/// one (cached tokens are then priced at the full input rate).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
}

impl ModelCost {
    /// Estimated USD for one model call: uncached prompt tokens at the input
    /// rate, cached ones at the cache-read rate, completion (which includes
    /// any reasoning tokens) at the output rate.
    pub fn call_cost(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        cached: Option<u32>,
    ) -> f64 {
        const M: f64 = 1_000_000.0;
        let cached = f64::from(cached.unwrap_or(0).min(prompt_tokens));
        let uncached = f64::from(prompt_tokens) - cached;
        uncached / M * self.input
            + cached / M * self.cache_read.unwrap_or(self.input)
            + f64::from(completion_tokens) / M * self.output
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

/// Whether `path` exists and was written within [`CACHE_TTL`].
fn is_fresh(path: &std::path::Path, ttl: Duration) -> bool {
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
    crate::capped_read::read_capped_json(resp, MAX_STRUCTURED_JSON_BYTES)
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
    let Some(s) = serde_json::to_string(v).ok() else { return; };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
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

    /// The price-card resolution: the configured provider wins; otherwise the
    /// most expensive offering across providers; `None` when unpriced.
    #[test]
    fn lookup_cost_prefers_provider_then_most_expensive() {
        let c = json!({
            "cheap": { "models": {
                "m": { "cost": { "input": 1.0, "output": 5.0, "cache_read": 0.1 } },
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
                cache_read: Some(0.1)
            })
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
        };
        // 1M uncached prompt + 1M output = $10 + $50.
        assert!((card.call_cost(1_000_000, 1_000_000, None) - 60.0).abs() < 1e-9);
        // Fully cached prompt: $1 instead of $10.
        assert!((card.call_cost(1_000_000, 0, Some(1_000_000)) - 1.0).abs() < 1e-9);
        // Cached count is clamped to the prompt size.
        assert!(
            (card.call_cost(100, 0, Some(1_000)) - card.call_cost(100, 0, Some(100))).abs() < 1e-12
        );
        // No published cache rate → cached tokens cost the full input rate.
        let flat = ModelCost {
            input: 10.0,
            output: 50.0,
            cache_read: None,
        };
        assert!((flat.call_cost(1_000_000, 0, Some(1_000_000)) - 10.0).abs() < 1e-9);
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
}
