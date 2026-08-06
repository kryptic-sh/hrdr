//! What each provider **actually serves right now**, cached on disk so the
//! `/model` picker can read it synchronously.
//!
//! The picker's other source, models.dev, is a third party's index of the whole
//! ecosystem: excellent for friendly names, context windows and prices, and
//! reliably behind. At the time of writing it listed 24 free OpenCode Zen models
//! of which only 7 were still being served, and named several that answer "Model
//! … is not supported". A provider's own `GET /v1/models` is the authority on
//! *existence*; the catalog stays the authority on *metadata*. This module keeps
//! the first one, and [`crate::model_choices`] joins them.
//!
//! Two properties the design turns on:
//!
//! * **Reads never fetch.** The picker builds its list on a keypress. Every
//!   lookup here is a file read; the network lives in [`refresh_all`], which
//!   [`Agent::new`](crate::Agent) spawns once per session.
//! * **Per-provider files, freshness from the mtime.** One file per provider
//!   under `<XDG cache>/hrdr/providers/`, so a provider that is slow, down or
//!   newly-authenticated neither blocks nor invalidates any other, and no
//!   timestamp has to be written into (or trusted from) the content.
//!
//! `HRDR_DISABLE_MODELS_FETCH` suppresses the network here exactly as it does
//! for models.dev — the whole test suite runs under it.

use std::path::PathBuf;
use std::time::Duration;

use crate::model_ref::ModelRef;
use crate::{
    AgentConfig, BUILTIN_PROVIDERS, ProviderAuthState, builtin_provider, provider_auth_state,
};

/// How old a provider's model list may be before it is refetched. The same span
/// the models.dev catalog uses: a provider ships a model now and then, not by
/// the minute, and both lists are refreshed by the same startup pass.
const PROVIDER_MODELS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Give up on a slow provider rather than leave a background task pending for
/// the life of the session.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// The cache file for `provider`, `<XDG cache>/hrdr/providers/<name>.json`.
///
/// The name is slugged (`[a-z0-9._-]`, everything else `_`) because a provider
/// name is user-supplied — a `[providers."a/b"]` entry must not be able to name
/// a path outside the directory. Collisions between two slugged names are
/// possible and harmless: this is a cache, and the loser simply refetches.
fn cache_path(provider: &str) -> Option<PathBuf> {
    let slug: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() || slug.chars().all(|c| c == '.') {
        return None;
    }
    Some(
        hjkl_xdg::cache_dir("hrdr")
            .ok()?
            .join("providers")
            .join(format!("{slug}.json")),
    )
}

/// The model ids `provider` was last seen serving, from the cache — no network,
/// no `await`.
///
/// `None` means **unknown** (never fetched, unreadable, or the provider has no
/// listing endpoint), which a caller must not confuse with "serves nothing": the
/// picker falls back to the models.dev catalog for those, rather than showing an
/// empty provider. A cache older than [`PROVIDER_MODELS_TTL`] is still served —
/// a day-old list beats none, and [`refresh_all`] is already replacing it.
pub fn cached_models(provider: &str) -> Option<Vec<String>> {
    let path = cache_path(provider)?;
    let ids: Vec<String> = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (!ids.is_empty()).then_some(ids)
}

/// Every provider this machine is set up to talk to: each built-in whose auth
/// state is anything but `Missing`, plus every `[providers.*]` the user defined.
///
/// Deliberately NOT "the provider in use". The whole point of the refresh is
/// that opening `/model` offers real, current models for every provider you
/// could switch to — the list used to be right only for the one you were on.
fn refreshable_providers(config: &AgentConfig) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    for name in BUILTIN_PROVIDERS {
        if let Some(p) = builtin_provider(name)
            && provider_auth_state(name, &p, None, None) != ProviderAuthState::Missing
        {
            push((*name).to_string());
        }
    }
    for name in config.providers.keys() {
        push(crate::ProviderName::new(name).as_str().to_string());
    }
    names
}

/// Refresh everything the `/model` picker reads: the models.dev catalog, then
/// every reachable provider's own model list, concurrently, skipping the ones
/// whose cache is still fresh.
///
/// models.dev comes first and unconditionally — it is a public index, needs no
/// credential, and is the one list that must land for a user who has logged in to
/// nothing (it is also where the provider listings get their friendly names).
/// Only the per-provider fetches depend on auth.
///
/// Best-effort and silent throughout: [`Agent::new`](crate::Agent) spawns this
/// and never awaits it, and a provider that is down, rate-limited or
/// misconfigured must cost the session nothing but a stale row in a picker.
pub async fn refresh_all(config: AgentConfig) {
    hrdr_llm::catalog::warm().await;
    if std::env::var_os("HRDR_DISABLE_MODELS_FETCH").is_some() {
        return;
    }
    let mut tasks = Vec::new();
    for name in refreshable_providers(&config) {
        let config = config.clone();
        tasks.push(tokio::spawn(async move {
            refresh_one(&config, &name).await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

/// Refresh one provider, unless its cache is still fresh.
async fn refresh_one(config: &AgentConfig, provider: &str) {
    let Some(path) = cache_path(provider) else {
        return;
    };
    if hrdr_llm::catalog::is_fresh(&path, PROVIDER_MODELS_TTL) {
        return;
    }
    let Some(ids) = list_models(config, provider).await else {
        return;
    };
    if ids.is_empty() {
        return; // an empty answer is not evidence; keep whatever is cached.
    }
    write_cache(&path, &ids);
}

/// The ids `provider` serves, from the network. `None` on any failure.
///
/// The model half of the identity is a placeholder: `/v1/models` is a property
/// of the endpoint, and [`ModelRef`] refuses a half-identity — so one is
/// supplied purely to resolve the provider's endpoint, key and trust kind
/// through the ONE seam ([`crate::resolve`]) rather than re-deriving them here.
async fn list_models(config: &AgentConfig, provider: &str) -> Option<Vec<String>> {
    let placeholder =
        ModelRef::new(crate::ProviderName::new(provider), crate::PLACEHOLDER_MODEL).ok()?;
    let reference = crate::oauth_derived(crate::resolve(&placeholder, config, None).ok()?);

    // The Codex backend 401s on `/v1/models`; the account's entitlements live
    // behind its OAuth token instead, and `chatgpt_model_catalog` keeps its own
    // per-account cache. Fan it out here anyway so a ChatGPT login's models are
    // current in the picker without having to be the provider in use.
    if reference.is_codex_oauth() {
        let access = crate::coordinated_oauth_access(reference.kind(), reference.base_url())
            .await
            .ok()?;
        let catalog = crate::chatgpt_model_catalog(&access, false).await;
        let mut ids: Vec<String> = catalog.models.into_iter().map(|m| m.slug).collect();
        ids.sort();
        return Some(ids);
    }

    let client = hrdr_llm::Client::new(
        reference.base_url().to_string(),
        reference.api_key().map(str::to_string),
        crate::PLACEHOLDER_MODEL.to_string(),
    );
    tokio::time::timeout(LIST_TIMEOUT, client.list_models())
        .await
        .ok()?
        .ok()
}

/// Write `ids` to `path` atomically, so a crash or a concurrent hrdr cannot
/// leave a half-written list for the picker to parse. Failure is ignored — the
/// caller has nothing better to do with it.
fn write_cache(path: &std::path::Path, ids: &[String]) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_vec(ids) else {
        return;
    };
    let _ = crate::write_atomic(path, &json);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider name is user-supplied, so the cache filename it produces must
    /// stay inside the cache directory whatever it says.
    #[test]
    fn a_provider_name_cannot_escape_the_cache_directory() {
        let dir = |name: &str| cache_path(name).map(|p| p.parent().unwrap().to_path_buf());
        let home = dir("zen").expect("a cache path resolves");
        for hostile in ["../../etc/passwd", "a/b", "..", "/abs", "."] {
            match cache_path(hostile) {
                None => {}
                Some(p) => {
                    assert_eq!(p.parent().unwrap(), home, "{hostile} escaped: {p:?}");
                    assert!(
                        p.file_name()
                            .is_some_and(|f| !f.to_string_lossy().contains('/')),
                        "{hostile}"
                    );
                }
            }
        }
        // A name of nothing but dots names no file at all.
        assert!(cache_path("..").is_none());
        assert!(cache_path("").is_none());
    }

    /// A written list reads back; a missing or empty one is `None` — "unknown",
    /// which the picker reads as "fall back to the catalog", not "serves
    /// nothing".
    #[test]
    fn a_cached_list_round_trips_and_absence_is_unknown() {
        assert_eq!(cached_models("a-provider-never-fetched"), None);

        let path = cache_path("roundtrip").expect("a cache path");
        write_cache(&path, &["b".to_string(), "a".to_string()]);
        hrdr_test_support::assert_sandboxed(&path);
        assert_eq!(
            cached_models("roundtrip"),
            Some(vec!["b".to_string(), "a".to_string()]),
            "the provider's own order is kept — it is the authority"
        );

        // An empty list is not evidence of an empty provider.
        write_cache(&path, &[]);
        assert_eq!(cached_models("roundtrip"), None);
    }

    /// A just-written cache is fresh; a missing one never is.
    #[test]
    fn freshness_follows_the_file_mtime() {
        let path = cache_path("freshness").expect("a cache path");
        assert!(
            !hrdr_llm::catalog::is_fresh(&path, PROVIDER_MODELS_TTL),
            "a missing cache is never fresh"
        );
        write_cache(&path, &["m".to_string()]);
        assert!(hrdr_llm::catalog::is_fresh(&path, PROVIDER_MODELS_TTL));
    }

    /// The refresh covers every provider the machine could switch to, not the
    /// one in use — `local` is keyless, so it always qualifies, and a custom
    /// entry is included by definition.
    #[test]
    fn every_reachable_provider_is_refreshed_not_just_the_active_one() {
        let mut cfg = AgentConfig::default();
        cfg.providers.insert(
            "mygateway".to_string(),
            crate::ProviderConfig {
                base_url: "http://localhost:9099/v1".to_string(),
                key_env: None,
                api_key: None,
                model: None,
                remote: None,
                context_window: None,
                headers: std::collections::HashMap::new(),
                api_version: None,
            },
        );
        let names = refreshable_providers(&cfg);
        assert!(names.contains(&"local".to_string()), "{names:?}");
        assert!(names.contains(&"mygateway".to_string()), "{names:?}");
        // Zen serves free models anonymously, so it is reachable with no login.
        assert!(names.contains(&"zen".to_string()), "{names:?}");
        // A remote built-in with no credential of any kind is not.
        assert!(!names.contains(&"openrouter".to_string()), "{names:?}");
    }

    /// A cached listing feeds the pre-flight, so hrdr stops warning about models
    /// that demonstrably exist.
    ///
    /// The regression: [`crate::models::preflight_model`] judged an id against models.dev
    /// alone. Pick a model a provider shipped this morning — or any of the ids Zen
    /// serves that the catalog has never indexed — and every turn opened with
    /// "⚠ model … isn't in provider 'zen's known catalog", about a model that
    /// works. The union of the two sources is what makes the warning mean
    /// something.
    #[test]
    fn a_cached_listing_stops_the_preflight_warning_about_a_model_that_exists() {
        let provider = crate::ProviderName::new("zen");
        let catalog = serde_json::json!({
            "opencode": { "models": { "an-indexed-model": {} } },
        });
        // Before: models.dev has never heard of it, so the warning fires.
        assert!(
            crate::models::preflight_model(Some(&catalog), &provider, "shipped-this-morning")
                .is_some()
        );

        write_cache(
            &cache_path("zen").expect("a cache path"),
            &["shipped-this-morning".to_string()],
        );

        // After: the provider itself says it serves it. No warning.
        assert_eq!(
            crate::models::preflight_model(Some(&catalog), &provider, "shipped-this-morning"),
            None
        );
        // …and the catalog's own ids are still known — it is a UNION, so a model
        // the listing missed is not suddenly suspect either.
        assert_eq!(
            crate::models::preflight_model(Some(&catalog), &provider, "an-indexed-model"),
            None
        );
        // Something neither source knows is still called out.
        assert!(crate::models::preflight_model(Some(&catalog), &provider, "typo-xyz").is_some());
    }

    /// The fetch honours `HRDR_DISABLE_MODELS_FETCH` — which the test harness
    /// sets for every binary — so no test can reach a provider's network.
    #[tokio::test]
    async fn the_refresh_is_disabled_by_the_same_switch_as_the_catalog() {
        assert!(
            std::env::var_os("HRDR_DISABLE_MODELS_FETCH").is_some(),
            "the sandbox ctor sets this for every test binary"
        );
        // Returns immediately, writing nothing.
        refresh_all(AgentConfig::default()).await;
        assert_eq!(cached_models("local"), None);
    }
}
