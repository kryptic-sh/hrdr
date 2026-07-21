//! The `/model` selector's data: every model across the user's configured
//! providers, paired with user-facing friendly names from the models.dev
//! catalog. Pure and catalog-driven so the list (and its fuzzy filter) is
//! testable without a network or a live endpoint.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::{
    AgentConfig, BUILTIN_PROVIDERS, ModelRef, ProviderName, builtin_provider, resolve_api_key,
    write_atomic,
};

/// One pickable model in the selector: the ids to switch to plus the friendly
/// labels to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// App provider name — the provider the picker switches to.
    pub provider: String,
    /// Model id to set on the agent.
    pub model: String,
    /// Friendly provider name (e.g. "OpenCode Zen").
    pub provider_label: String,
    /// Friendly model name (e.g. "Claude Fable 5.0").
    pub model_label: String,
    /// The model's advertised context window, when known (currently only
    /// authenticated ChatGPT rows carry one). Preferred over an endpoint probe
    /// on switch. `None` for rows whose window is unknown until probed.
    pub context_window: Option<u32>,
}

/// The models.dev catalog key for a built-in preset (or a catalog-matching
/// alias). `local` self-hosted endpoints have no catalog entry. One source of
/// truth: [`ProviderName::catalog_key`].
pub fn builtin_catalog_key(name: &str) -> Option<&'static str> {
    ProviderName::new(name).catalog_key()
}

/// A provider the user can pick a model from.
struct ConfiguredProvider {
    /// App provider name — the provider the picker switches to.
    name: String,
    /// models.dev catalog key (a built-in mapping, else the name itself).
    catalog_key: String,
    /// The provider's own configured default model — a fallback list entry when
    /// the catalog carries nothing for it.
    configured_model: Option<String>,
}

/// The providers the user can switch a model to: every custom `[providers.*]`,
/// each built-in preset whose API key resolves (so it's actually set up), the
/// keyless `local` preset, and the active provider — deduped by name.
fn configured_providers(config: &AgentConfig, active: Option<&str>) -> Vec<ConfiguredProvider> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ConfiguredProvider> = Vec::new();
    // Every name a row carries is CANONICAL — `[providers.anthropic]` offers rows
    // on `claude`, the same name `resolve_provider` looks the entry up by. A row
    // spelled with an alias would be validated against one endpoint (the auth gate
    // resolves it) and talked to on another (`ModelRef::new` folds it): the picker
    // is the one place both halves are composed, so it must not hold the split.
    let mut push = |name: String, model: Option<String>| {
        let name = ProviderName::new(&name).as_str().to_string();
        if seen.insert(name.clone()) {
            let catalog_key = builtin_catalog_key(&name)
                .map_or_else(|| name.to_ascii_lowercase(), str::to_string);
            out.push(ConfiguredProvider {
                name,
                catalog_key,
                configured_model: model,
            });
        }
    };

    // Custom providers are always in — the user defined them explicitly.
    for (name, c) in &config.providers {
        push(name.clone(), c.model.clone());
    }
    // Built-in presets the user has a resolvable key for, plus `local` —
    // keyless by design (a self-hosted endpoint), so it's always offered.
    for name in BUILTIN_PROVIDERS {
        if let Some(p) = builtin_provider(name)
            && (*name == "local" || resolve_api_key(name, &p, None, None).is_some())
        {
            push((*name).to_string(), p.model);
        }
    }
    // The active provider, even without a key (it's in use right now).
    if let Some(a) = active
        && let Some(p) = config.resolve_provider(a)
    {
        push(a.to_string(), p.model);
    }
    out
}

/// A best-effort friendly name for a provider the catalog doesn't carry:
/// title-case the app name's words (`my-local` → `My Local`).
fn pretty_provider(name: &str) -> String {
    name.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the list of every model across the configured providers, using
/// `catalog` for model lists and friendly names. Ordered by **usage** (the
/// most-often-selected first, from `usage`), then by model name
/// (case-insensitive) to break ties. A provider with no catalog entry
/// contributes its single configured model (or "default" when it names none).
/// Pure — the runtime entry point [`model_choices`] supplies the cached
/// catalog and usage counts.
fn choices_from(
    providers: &[ConfiguredProvider],
    catalog: Option<&Value>,
    usage: &HashMap<String, u64>,
) -> Vec<ModelChoice> {
    let mut out: Vec<ModelChoice> = Vec::new();
    for p in providers {
        let from_catalog =
            catalog.and_then(|c| hrdr_llm::catalog::provider_models(c, &p.catalog_key));
        match from_catalog {
            Some((provider_label, models)) => {
                for (id, name) in models {
                    out.push(ModelChoice {
                        provider: p.name.clone(),
                        model: id,
                        provider_label: provider_label.clone(),
                        model_label: name,
                        context_window: None,
                    });
                }
            }
            None => {
                // No catalog entry: offer the configured model, or "default"
                // (the server's own pick) when none is named — keyless `local`
                // endpoints land here.
                let m = p
                    .configured_model
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                out.push(ModelChoice {
                    provider: p.name.clone(),
                    model: m.clone(),
                    provider_label: pretty_provider(&p.name),
                    model_label: m,
                    context_window: None,
                });
            }
        }
    }
    sort_choices(&mut out, usage);
    out
}

/// The global selector ordering: most-used first, then case-insensitive
/// `model_label` for ties. A single total order over ALL rows — the stable
/// `sort_by` then preserves insertion (server) order for exact ties, so ChatGPT
/// rows keep their upstream order relative to each other without a special-case
/// comparator (which would not be a strict weak ordering).
fn sort_choices(out: &mut [ModelChoice], usage: &HashMap<String, u64>) {
    let uses = |c: &ModelChoice| {
        usage
            .get(&usage_key(&c.provider, &c.model))
            .copied()
            .unwrap_or(0)
    };
    out.sort_by(|a, b| {
        uses(b).cmp(&uses(a)).then_with(|| {
            a.model_label
                .to_lowercase()
                .cmp(&b.model_label.to_lowercase())
        })
    });
}

/// Convert entitled ChatGPT catalog rows into selector choices for the built-in
/// `chatgpt` provider, carrying each model's context window.
pub fn chatgpt_model_choices(models: &[crate::ChatGptModel]) -> Vec<ModelChoice> {
    models
        .iter()
        .map(|m| ModelChoice {
            provider: "chatgpt".to_string(),
            model: m.slug.clone(),
            provider_label: "ChatGPT".to_string(),
            model_label: m.label.clone(),
            context_window: m.context_window,
        })
        .collect()
}

/// Merge authenticated ChatGPT rows into a base selector list, then re-sort by
/// the global ordering. Any existing ChatGPT row in `base` is replaced (the
/// authenticated catalog supersedes the static one); every other provider is
/// left untouched. ChatGPT rows retain their upstream order within equal
/// usage/label ties via the stable sort.
///
/// The superseded rows are matched with [`crate::is_chatgpt_provider_name`], not
/// an exact `"chatgpt"` compare: a base row carries the provider name as the
/// user spelled it, so a config that says `provider = "codex"` would otherwise
/// survive the filter and leave the picker showing the model twice — once from
/// the stale preset (with no context window) and once from the live catalog.
///
/// `usage` is passed in rather than loaded here: this runs on the UI thread when
/// the async catalog lands, and a pure merge keeps its test hermetic.
pub fn merge_chatgpt_choices(
    base: Vec<ModelChoice>,
    chatgpt: &[crate::ChatGptModel],
    usage: &HashMap<String, u64>,
) -> Vec<ModelChoice> {
    let mut out: Vec<ModelChoice> = base
        .into_iter()
        .filter(|c| !crate::is_chatgpt_provider_name(&c.provider))
        .collect();
    out.extend(chatgpt_model_choices(chatgpt));
    sort_choices(&mut out, usage);
    out
}

/// The usage-count store's path, `<XDG data>/hrdr/model_usage.json`.
fn usage_path() -> Option<PathBuf> {
    Some(hjkl_xdg::data_dir("hrdr").ok()?.join("model_usage.json"))
}

/// The store key for a `(provider, model)` pick.
fn usage_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Load the per-model selection counts; empty when nothing has been picked yet
/// (or the file is missing/corrupt — usage stats are a nicety, never fatal).
pub fn load_model_usage() -> HashMap<String, u64> {
    usage_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Record that the user selected `model` on `provider` in the `/model` selector,
/// bumping its count so it sorts higher next time. Best-effort: any I/O error is
/// swallowed.
pub fn record_model_use(provider: &str, model: &str) {
    let Some(path) = usage_path() else { return };
    let mut usage = load_model_usage();
    *usage.entry(usage_key(provider, model)).or_insert(0) += 1;
    let Ok(json) = serde_json::to_string(&usage) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = write_atomic(&path, json.as_bytes());
}

/// The last-used-model store's path, `<XDG data>/hrdr/last_model.json`.
fn last_model_path() -> Option<PathBuf> {
    Some(hjkl_xdg::data_dir("hrdr").ok()?.join("last_model.json"))
}

/// What `last_model.json` remembers: the last identity used *overall*, and the
/// last model used **on each provider**.
///
/// The per-provider map is what makes a provider-only switch (`/login zen`, the
/// `/model` picker's provider rows) expressible at all: a provider names no model,
/// and the model
/// you were using on some *other* provider is exactly the one that must not follow
/// you there. "The model you last used on THIS provider" is the answer that both
/// exists and is right — see [`model_for_provider`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastModels {
    /// The identity most recently switched to, whatever the provider.
    pub last: Option<ModelRef>,
    /// provider (canonical) → the model id last used on it.
    pub by_provider: HashMap<String, String>,
}

impl LastModels {
    /// The model last used on `provider`, as a complete identity.
    pub fn on(&self, provider: &ProviderName) -> Option<ModelRef> {
        let model = self.by_provider.get(provider.as_str())?;
        ModelRef::new(provider.clone(), model).ok()
    }

    /// Fold `r` in: it becomes the last identity, and the last model on ITS
    /// provider. Other providers' entries are untouched — that is the point.
    fn record(&mut self, r: &ModelRef) {
        self.by_provider
            .insert(r.provider().as_str().to_string(), r.model().to_string());
        self.last = Some(r.clone());
    }

    fn to_json(&self) -> Value {
        serde_json::json!({
            "last": self.last.as_ref().map(ToString::to_string),
            "by_provider": self.by_provider,
        })
    }
}

/// Everything the store remembers (empty when there is no file, or it is corrupt
/// — a last-used memory is a nicety, never fatal).
pub fn load_last_models() -> LastModels {
    last_model_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| parse_last_model(&v))
        .unwrap_or_default()
}

/// The most recently switched-to identity, if one has been recorded. The startup
/// resolver falls back to this when neither a flag, env var, session, nor config
/// names an identity — so a fresh launch resumes where you left off.
pub fn load_last_model() -> Option<ModelRef> {
    load_last_models().last
}

/// The model last used on `provider`, if any — step (1) of [`model_for_provider`].
pub fn last_model_on(provider: &ProviderName) -> Option<ModelRef> {
    load_last_models().on(provider)
}

/// Parse the stored JSON. Pure, so the rules are testable without a file.
///
/// A malformed half (either side empty, or a `last` that doesn't parse) is
/// dropped rather than fabricated into a half-identity.
fn parse_last_model(v: &Value) -> LastModels {
    let mut out = LastModels {
        last: v
            .get("last")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<ModelRef>().ok()),
        by_provider: HashMap::new(),
    };
    if let Some(map) = v.get("by_provider").and_then(Value::as_object) {
        for (provider, model) in map {
            let Some(model) = model.as_str() else {
                continue;
            };
            // Route both halves through `ModelRef` so the stored key is canonical
            // (an `anthropic` entry answers a lookup for `claude`) and an empty
            // half is rejected rather than stored.
            if let Ok(r) = ModelRef::new(ProviderName::new(provider), model) {
                out.by_provider
                    .insert(r.provider().as_str().to_string(), r.model().to_string());
            }
        }
    }
    out
}

/// Record `r` as the most recently used identity — and as the last model used on
/// its provider. Best-effort: any I/O error is swallowed.
///
/// Read-modify-write: the other providers' entries are the whole value of the
/// store, so a write must never drop them.
pub fn record_last_model(r: &ModelRef) {
    let Some(path) = last_model_path() else {
        return;
    };
    let mut store = load_last_models();
    store.record(r);
    let Ok(json) = serde_json::to_string(&store.to_json()) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = write_atomic(&path, json.as_bytes());
}

/// The identity to use when a provider is named but **no model is** — `/login
/// <provider>`, the `/model` picker switching provider.
///
/// A provider-only switch is not expressible as a [`ModelRef`], and that is the
/// point: the bug this refactor exists to kill was `repoint_to_provider` leaving
/// `cfg.model` alone when the new provider declared no default (6 of the 7
/// built-ins), so the model you were running on **silently followed you onto a
/// provider that has never heard of it**. The old model is never an answer here.
/// The answers, in order:
///
/// 1. the model you last used ON THAT PROVIDER ([`last_model_on`]);
/// 2. a model the provider itself declares — `[providers.<name>].model`, or a
///    built-in preset's default (only `chatgpt` has one);
/// 3. nothing: an error naming the flag that would settle it.
///
/// A caller with a UI may catch (3) and open a model picker filtered to that
/// provider instead of surfacing it.
pub fn model_for_provider(provider: &ProviderName, config: &AgentConfig) -> Result<ModelRef> {
    model_for_provider_in(&load_last_models(), provider, config)
}

/// [`model_for_provider`] against an explicit store — the pure core, so the
/// interactive chain is testable without the real `last_model.json` (and so a test
/// cannot silently stop testing anything the moment the developer uses that
/// provider for real).
pub fn model_for_provider_in(
    store: &LastModels,
    provider: &ProviderName,
    config: &AgentConfig,
) -> Result<ModelRef> {
    let resolved = config.resolve_provider(provider.as_str()).ok_or_else(|| {
        anyhow!(
            "unknown provider '{provider}' (built-ins: {}, or define [providers.{provider}])",
            BUILTIN_PROVIDERS.join(", ")
        )
    })?;
    model_for_resolved_provider_in(store, provider, &resolved)
}

/// [`model_for_provider`] for a provider that is already resolved (a caller
/// holding a [`ResolvedProvider`] rather than the config it came from). One
/// implementation of the chain; this is it.
pub fn model_for_resolved_provider(
    provider: &ProviderName,
    resolved: &crate::ResolvedProvider,
) -> Result<ModelRef> {
    model_for_resolved_provider_in(&load_last_models(), provider, resolved)
}

/// [`model_for_resolved_provider`] against an explicit store — the pure core, so
/// the chain's rules are testable without the real `<XDG data>/hrdr/last_model.json`.
///
/// The store is deliberately a parameter rather than a global read: a test that
/// consults the developer's actual store is one that silently stops testing the
/// moment they use that provider (it would have to guard on "only assert when the
/// store happens to be empty"), and that guard passes green while asserting
/// nothing.
pub fn model_for_resolved_provider_in(
    store: &LastModels,
    provider: &ProviderName,
    resolved: &crate::ResolvedProvider,
) -> Result<ModelRef> {
    if let Some(r) = store.on(provider) {
        return Ok(r);
    }
    if let Some(model) = resolved.model.as_deref() {
        return ModelRef::new(provider.clone(), model).map_err(Into::into);
    }
    Err(anyhow!(
        "provider '{provider}' needs a model — pass --model '{provider}://<model>' \
         (or pick one with /model)"
    ))
}

/// Every model the user can pick, across their configured providers, with
/// friendly labels — the `/model` selector's list. Reads the models.dev catalog
/// synchronously from cache (no network); a provider the catalog doesn't cover
/// still contributes its configured model.
pub fn model_choices(config: &AgentConfig, active: Option<&str>) -> Vec<ModelChoice> {
    let providers = configured_providers(config, active);
    let catalog = hrdr_llm::catalog::load_cached();
    let usage = load_model_usage();
    choices_from(&providers, catalog.as_ref(), &usage)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    AccountCatalog,
    ModelsDev,
    Configured,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AvailableModel {
    pub provider: String,
    pub model: String,
    pub label: String,
    pub source: ModelSource,
}

/// Discoverable configured/catalog models with provenance for agent introspection.
pub fn available_models(config: &AgentConfig, active: Option<&str>) -> Vec<AvailableModel> {
    let providers = configured_providers(config, active);
    let catalog = hrdr_llm::catalog::load_cached();
    let mut rows = Vec::new();
    for provider in providers {
        if let Some((_, models)) = catalog
            .as_ref()
            .and_then(|c| hrdr_llm::catalog::provider_models(c, &provider.catalog_key))
        {
            rows.extend(models.into_iter().filter(|(id, _)| id != "default").map(
                |(model, label)| AvailableModel {
                    provider: provider.name.clone(),
                    model,
                    label,
                    source: ModelSource::ModelsDev,
                },
            ));
        } else if let Some(model) = provider.configured_model.filter(|m| m != "default") {
            rows.push(AvailableModel {
                provider: provider.name,
                label: model.clone(),
                model,
                source: ModelSource::Configured,
            });
        }
    }
    rows.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
    rows.dedup_by(|a, b| a.provider == b.provider && a.model == b.model);
    // The active provider is always in `providers`, but its model may have no
    // row when the catalog is missing and the built-in provider carries no
    // configured model — a bare `[providers.openai]` entry, a keyless CI
    // runner.  Without a row, the `models` tool's `current: true` flag has
    // nothing to attach to.  Insert the session's actual model so the agent
    // can always identify the row it is running on.
    if let Some(active_provider) = active {
        let active_model = config.model.model();
        if !rows
            .iter()
            .any(|r| r.provider == active_provider && r.model == active_model)
        {
            rows.push(AvailableModel {
                provider: active_provider.to_string(),
                model: active_model.to_string(),
                label: active_model.to_string(),
                source: ModelSource::Configured,
            });
            rows.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
        }
    }
    rows
}

/// Case-insensitive fuzzy filter over the choices: the query's characters must
/// appear in order somewhere within `"model_label provider_label"`. Returns the
/// matching indices in their original (sorted) order; an empty query matches
/// everything.
pub fn filter_model_choices(choices: &[ModelChoice], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..choices.len()).collect();
    }
    choices
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = format!("{} {}", c.model_label, c.provider_label).to_lowercase();
            is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

/// Whether `needle`'s chars appear in order within `haystack`.
fn is_subsequence(needle: &[char], haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.iter().all(|&c| it.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The whole promise, tested the way it will be broken: a test that calls the real
    /// state-writing code with **no fixture, no harness, no helper** — exactly what a
    /// new contributor writes — and cannot reach the developer's files anyway.
    ///
    /// `record_last_model` rewrites `last_model.json`, the file this suite used to
    /// silently overwrite on the owner's machine. It still writes it. It just cannot
    /// write it anywhere but the sandbox `hrdr-test-support`'s ctor installed before
    /// `main` — no line in this test asks for that.
    #[test]
    fn a_test_that_asks_for_nothing_still_cannot_write_the_real_last_model_store() {
        record_last_model(&r("zen://a-model-nobody-uses"));

        let path = last_model_path().expect("the store resolves to a path");
        assert!(path.exists(), "the write really happened");
        // It landed in the sandbox, and nowhere near the real home.
        hrdr_test_support::assert_sandboxed(&path);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("a-model-nobody-uses"));
    }

    /// A `provider://model` identity, for the tests that speak in them.
    fn r(s: &str) -> ModelRef {
        s.parse().unwrap()
    }

    fn catalog() -> Value {
        json!({
            "opencode": { "name": "OpenCode Zen", "models": {
                "claude-fable-5": { "name": "Claude Fable 5.0" },
                "gpt-5-6": { "name": "GPT-5.6" },
            }},
            "opencode-go": { "name": "OpenCode Go", "models": {
                "deepseek-v4-pro": { "name": "DeepSeek V4 Pro" },
            }},
        })
    }

    fn providers() -> Vec<ConfiguredProvider> {
        vec![
            ConfiguredProvider {
                name: "zen".into(),
                catalog_key: "opencode".into(),
                configured_model: None,
            },
            ConfiguredProvider {
                name: "go".into(),
                catalog_key: "opencode-go".into(),
                configured_model: None,
            },
        ]
    }

    #[test]
    fn choices_are_friendly_and_sorted_across_providers() {
        // With no usage recorded yet, the order is the model-name tie-break:
        // alphabetical by friendly model name across both providers.
        let out = choices_from(&providers(), Some(&catalog()), &HashMap::new());
        let rows: Vec<(&str, &str, &str)> = out
            .iter()
            .map(|c| {
                (
                    c.model_label.as_str(),
                    c.provider_label.as_str(),
                    c.model.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Claude Fable 5.0", "OpenCode Zen", "claude-fable-5"),
                ("DeepSeek V4 Pro", "OpenCode Go", "deepseek-v4-pro"),
                ("GPT-5.6", "OpenCode Zen", "gpt-5-6"),
            ]
        );
        // The switch targets are the app provider names, not the catalog keys.
        assert_eq!(out[0].provider, "zen");
        assert_eq!(out[1].provider, "go");
    }

    #[test]
    fn usage_orders_the_list_most_used_first_then_by_name() {
        // GPT is used twice, DeepSeek once, Claude never.
        let usage = HashMap::from([
            (usage_key("zen", "gpt-5-6"), 2),
            (usage_key("go", "deepseek-v4-pro"), 1),
        ]);
        let out = choices_from(&providers(), Some(&catalog()), &usage);
        let order: Vec<&str> = out.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(order, vec!["gpt-5-6", "deepseek-v4-pro", "claude-fable-5"]);

        // A tie in usage falls back to the model name: give both the same count.
        let tied = HashMap::from([
            (usage_key("zen", "gpt-5-6"), 1),
            (usage_key("go", "deepseek-v4-pro"), 1),
            (usage_key("zen", "claude-fable-5"), 1),
        ]);
        let out = choices_from(&providers(), Some(&catalog()), &tied);
        let order: Vec<&str> = out.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(order, vec!["claude-fable-5", "deepseek-v4-pro", "gpt-5-6"]);
    }

    #[test]
    fn chatgpt_rows_merge_in_one_total_order_carrying_context_windows() {
        use crate::ChatGptModel;
        let cg = vec![
            ChatGptModel {
                slug: "z-model".into(),
                label: "z".into(),
                context_window: Some(400_000),
            },
            ChatGptModel {
                slug: "a-model".into(),
                label: "a".into(),
                context_window: Some(272_000),
            },
        ];
        let mut choices = chatgpt_model_choices(&cg);
        // A non-ChatGPT row whose label sorts BETWEEN the two ChatGPT rows.
        choices.push(ModelChoice {
            provider: "zen".into(),
            model: "m".into(),
            provider_label: "OpenCode Zen".into(),
            model_label: "m".into(),
            context_window: None,
        });
        // All usage zero → a single label order (no ChatGPT special-casing that
        // would let two ChatGPT rows straddle the middle row).
        sort_choices(&mut choices, &HashMap::new());
        let order: Vec<&str> = choices.iter().map(|c| c.model_label.as_str()).collect();
        assert_eq!(order, vec!["a", "m", "z"]);

        let a = choices.iter().find(|c| c.model == "a-model").unwrap();
        assert_eq!(a.context_window, Some(272_000));
        assert_eq!(a.provider, "chatgpt");
        assert_eq!(a.provider_label, "ChatGPT");
    }

    #[test]
    fn chatgpt_equal_label_ties_keep_upstream_server_order() {
        use crate::ChatGptModel;
        // Two rows sharing a label: the stable sort must preserve their inserted
        // (server) order — first-in stays first.
        let cg = vec![
            ChatGptModel {
                slug: "server-first".into(),
                label: "dup".into(),
                context_window: None,
            },
            ChatGptModel {
                slug: "server-second".into(),
                label: "dup".into(),
                context_window: None,
            },
        ];
        let mut choices = chatgpt_model_choices(&cg);
        sort_choices(&mut choices, &HashMap::new());
        let slugs: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(slugs, vec!["server-first", "server-second"]);
    }

    #[test]
    fn merge_replaces_existing_chatgpt_rows_and_leaves_others() {
        use crate::ChatGptModel;
        let base = vec![
            ModelChoice {
                provider: "chatgpt".into(),
                model: "stale".into(),
                provider_label: "ChatGPT".into(),
                model_label: "stale".into(),
                context_window: None,
            },
            ModelChoice {
                provider: "zen".into(),
                model: "keep".into(),
                provider_label: "OpenCode Zen".into(),
                model_label: "keep".into(),
                context_window: None,
            },
        ];
        let cg = vec![ChatGptModel {
            slug: "fresh".into(),
            label: "fresh".into(),
            context_window: Some(1),
        }];
        let out = merge_chatgpt_choices(base, &cg, &HashMap::new());
        let chatgpt: Vec<&str> = out
            .iter()
            .filter(|c| c.provider == "chatgpt")
            .map(|c| c.model.as_str())
            .collect();
        assert_eq!(chatgpt, vec!["fresh"], "stale chatgpt rows replaced");
        assert!(
            out.iter().any(|c| c.provider == "zen" && c.model == "keep"),
            "other providers untouched"
        );
    }

    #[test]
    fn merge_replaces_chatgpt_rows_spelled_with_any_alias() {
        use crate::ChatGptModel;
        // A base row carries the provider name as the user spelled it in config.
        // Every ChatGPT alias must be superseded by the authenticated catalog —
        // an exact `== "chatgpt"` compare would leave a duplicate row behind.
        for alias in ["chatgpt", "codex", "openai-oauth", "ChatGPT", "CODEX"] {
            let base = vec![ModelChoice {
                provider: alias.into(),
                model: "stale".into(),
                provider_label: "ChatGPT".into(),
                model_label: "stale".into(),
                context_window: None,
            }];
            let cg = vec![ChatGptModel {
                slug: "fresh".into(),
                label: "fresh".into(),
                context_window: Some(400_000),
            }];
            let out = merge_chatgpt_choices(base, &cg, &HashMap::new());
            assert_eq!(
                out.len(),
                1,
                "{alias}: stale row must be replaced, not duplicated"
            );
            assert_eq!(out[0].model, "fresh");
            assert_eq!(out[0].context_window, Some(400_000));
        }
    }

    #[test]
    fn a_provider_without_a_catalog_entry_offers_its_configured_model() {
        let ps = vec![ConfiguredProvider {
            name: "mylocal".into(),
            catalog_key: "mylocal".into(),
            configured_model: Some("Qwen3-30B".into()),
        }];
        let out = choices_from(&ps, Some(&catalog()), &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider, "mylocal");
        assert_eq!(out[0].model, "Qwen3-30B");
        assert_eq!(out[0].provider_label, "Mylocal"); // prettified fallback
        assert_eq!(out[0].model_label, "Qwen3-30B");
    }

    /// The picker's rows name providers CANONICALLY — `[providers.anthropic]` offers
    /// rows on `claude`.
    ///
    /// The row's name is used twice on a pick: once to resolve the provider (the auth
    /// gate, `host.resolve_provider`) and once to build the identity (`ModelRef::new`,
    /// which FOLDS it). An alias-spelled row made those two disagree — one endpoint
    /// validated, another talked to. Canonical rows leave nothing to disagree about.
    #[test]
    fn picker_rows_name_providers_canonically() {
        let mut cfg = crate::AgentConfig::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            crate::ProviderConfig {
                base_url: "http://localhost:9999/v1".to_string(),
                key_env: None,
                api_key: Some("k".to_string()),
                model: Some("claude-x".to_string()),
                remote: None,
                context_window: None,
                headers: HashMap::new(),
                api_version: None,
            },
        );
        let ps = configured_providers(&cfg, None);
        let claude = ps
            .iter()
            .find(|p| p.name == "claude")
            .expect("the entry is offered under the name it folds onto");
        assert_eq!(
            claude.catalog_key, "anthropic",
            "…and windows/labels via models.dev's key"
        );
        assert_eq!(claude.configured_model.as_deref(), Some("claude-x"));
        assert!(
            !ps.iter().any(|p| p.name == "anthropic"),
            "no row carries a name `ModelRef` would fold away underneath it"
        );
        // Both spellings resolve to that one entry, so the pick and the auth gate agree.
        for name in ["claude", "anthropic"] {
            assert_eq!(
                cfg.resolve_provider(name).map(|p| p.base_url),
                Some("http://localhost:9999/v1".to_string()),
                "{name}"
            );
        }
    }

    #[test]
    fn a_keyless_provider_without_a_model_offers_the_server_default() {
        // `local` has no catalog entry and no configured model; it still shows
        // up as a pickable "default" (the server's own pick) entry.
        let ps = vec![ConfiguredProvider {
            name: "local".into(),
            catalog_key: "local".into(),
            configured_model: None,
        }];
        let out = choices_from(&ps, Some(&catalog()), &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider, "local");
        assert_eq!(out[0].model, "default");
        assert_eq!(out[0].provider_label, "Local");
        assert_eq!(out[0].model_label, "default");
    }

    #[test]
    fn filter_matches_model_and_provider_case_insensitively() {
        let out = choices_from(&providers(), Some(&catalog()), &HashMap::new());
        // Matches on the model name.
        let deepseek = filter_model_choices(&out, "deepseek");
        assert_eq!(deepseek.len(), 1);
        assert_eq!(out[deepseek[0]].model, "deepseek-v4-pro");
        // Matches on the provider name (case-insensitive) — both Zen models.
        let zen = filter_model_choices(&out, "zen");
        assert_eq!(zen.len(), 2);
        // Fuzzy subsequence across the combined text.
        let fuzzy = filter_model_choices(&out, "fable zen");
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(out[fuzzy[0]].model, "claude-fable-5");
        // Empty query matches everything.
        assert_eq!(filter_model_choices(&out, "  ").len(), out.len());
        // No match.
        assert!(filter_model_choices(&out, "zzzzz").is_empty());
    }

    /// The store keeps a model PER PROVIDER, because that is the only honest answer
    /// to "you named a provider but no model". A half-written pair is not an
    /// identity and is dropped rather than fabricated.
    #[test]
    fn last_model_parses_the_per_provider_map() {
        let store = parse_last_model(&json!({
            "last": "openai://gpt-5.5",
            "by_provider": {
                "openai": "gpt-5.5",
                // Folded onto the canonical name on the way in, so a lookup for
                // `claude` finds a file written as `anthropic`.
                "anthropic": "sonnet",
                // Not identities: dropped, never half-stored.
                "zen": "",
                "": "grok-code",
            },
        }));
        assert_eq!(store.last, Some(r("openai://gpt-5.5")));
        assert_eq!(
            store.on(&ProviderName::new("openai")),
            Some(r("openai://gpt-5.5"))
        );
        assert_eq!(
            store.on(&ProviderName::new("claude")),
            Some(r("claude://sonnet"))
        );
        assert_eq!(store.on(&ProviderName::new("zen")), None);
        assert_eq!(store.by_provider.len(), 2);

        // A `last` that isn't a complete identity is not one.
        assert_eq!(parse_last_model(&json!({"last": "gpt-5.5"})).last, None);
        assert_eq!(parse_last_model(&json!({})), LastModels::default());
    }

    /// Recording an identity touches ITS provider's entry and no other — a store
    /// that dropped the rest would make the per-provider fallback useless after one
    /// switch.
    #[test]
    fn recording_a_model_leaves_the_other_providers_alone() {
        let mut store = parse_last_model(&json!({
            "last": "zen://grok-code",
            "by_provider": {"zen": "grok-code", "openai": "gpt-5.5"},
        }));
        store.record(&r("claude://opus-4-8"));
        assert_eq!(store.last, Some(r("claude://opus-4-8")));
        assert_eq!(
            store.on(&ProviderName::new("claude")),
            Some(r("claude://opus-4-8"))
        );
        assert_eq!(
            store.on(&ProviderName::new("zen")),
            Some(r("zen://grok-code"))
        );
        assert_eq!(
            store.on(&ProviderName::new("openai")),
            Some(r("openai://gpt-5.5"))
        );
        // Round-trips through the file shape.
        assert_eq!(parse_last_model(&store.to_json()), store);
    }

    /// The fallback chain for a provider-only switch: the model last used THERE,
    /// else one the provider declares, else an actionable error — and never the
    /// model you were running on somewhere else.
    #[test]
    fn model_for_a_provider_falls_back_and_never_carries_the_old_model_over() {
        // Driven against an EXPLICIT store, never the developer's real
        // `<XDG data>/hrdr/last_model.json`. A test that reads the real store has to
        // guard every assertion with "…only if the store happens to be empty", which
        // passes green while asserting nothing the moment you actually use that
        // provider. Injecting the store lets every branch be asserted outright.
        let empty = LastModels::default();
        let zen = builtin_provider("zen").unwrap();
        // `chatgpt`/`codex` fold onto the merged built-in `openai`.
        let openai = builtin_provider("openai").unwrap();

        // (1) The model last used ON THAT PROVIDER wins. The OAuth/Codex spellings
        //     fold onto `openai`, so a `chatgpt://` pick is remembered for `openai`.
        let mut remembered = LastModels::default();
        remembered.record(&r("zen://kimi-k2"));
        remembered.record(&r("chatgpt://gpt-5.6-sol"));
        assert_eq!(
            model_for_resolved_provider_in(&remembered, &ProviderName::new("zen"), &zen).unwrap(),
            r("zen://kimi-k2"),
        );
        assert_eq!(
            model_for_resolved_provider_in(&remembered, &ProviderName::new("codex"), &openai)
                .unwrap(),
            r("openai://gpt-5.6-sol"),
            "what you last used there wins, and the alias folds to `openai`",
        );

        // (2) No built-in declares a model of its own any more — the merged `openai`
        //     included (its Codex default lives in `CHATGPT_DEFAULT_MODEL`, not the
        //     preset). With nothing remembered, resolution falls through to (3).
        assert!(openai.model.is_none());

        // (3) Nothing remembered and none declared → an ERROR naming the flag that
        //     settles it. Emphatically NOT whatever model the caller was using
        //     elsewhere: the old model silently following you onto a new provider is
        //     the exact bug this whole refactor exists to kill.
        assert!(zen.model.is_none());
        let err = model_for_resolved_provider_in(&empty, &ProviderName::new("zen"), &zen)
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider 'zen' needs a model"), "{err}");
        assert!(err.contains("--model 'zen://<model>'"), "{err}");
    }

    #[test]
    fn builtin_catalog_keys_map_the_presets() {
        assert_eq!(builtin_catalog_key("zen"), Some("opencode"));
        assert_eq!(builtin_catalog_key("go"), Some("opencode-go"));
        assert_eq!(builtin_catalog_key("claude"), Some("anthropic"));
        assert_eq!(builtin_catalog_key("OpenAI"), Some("openai")); // case-insensitive
        assert_eq!(builtin_catalog_key("local"), None);
        assert_eq!(builtin_catalog_key("mycustom"), None);
    }
}
