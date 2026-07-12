//! The `/model` selector's data: every model across the user's configured
//! providers, paired with user-facing friendly names from the models.dev
//! catalog. Pure and catalog-driven so the list (and its fuzzy filter) is
//! testable without a network or a live endpoint.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::{AgentConfig, BUILTIN_PROVIDERS, builtin_provider, resolve_api_key, write_atomic};

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
/// alias). `local` self-hosted endpoints have no catalog entry.
pub fn builtin_catalog_key(name: &str) -> Option<&'static str> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "zen" | "opencode" | "opencode-zen" => "opencode",
        "go" | "opencode-go" => "opencode-go",
        "openai" => "openai",
        "openrouter" => "openrouter",
        "claude" | "anthropic" => "anthropic",
        _ => return None,
    })
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
    let mut push = |name: String, model: Option<String>| {
        if seen.insert(name.to_ascii_lowercase()) {
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
/// the global ordering. Any existing `chatgpt` rows in `base` are replaced (the
/// authenticated catalog supersedes the static one); every other provider is
/// left untouched. ChatGPT rows retain their upstream order within equal
/// usage/label ties via the stable sort.
pub fn merge_chatgpt_choices(
    base: Vec<ModelChoice>,
    chatgpt: &[crate::ChatGptModel],
) -> Vec<ModelChoice> {
    let usage = load_model_usage();
    let mut out: Vec<ModelChoice> = base
        .into_iter()
        .filter(|c| c.provider != "chatgpt")
        .collect();
    out.extend(chatgpt_model_choices(chatgpt));
    sort_choices(&mut out, &usage);
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

/// The last-used `(provider, model)` store's path,
/// `<XDG data>/hrdr/last_model.json`.
fn last_model_path() -> Option<PathBuf> {
    Some(hjkl_xdg::data_dir("hrdr").ok()?.join("last_model.json"))
}

/// The most recently switched-to `(provider, model)`, if any has been recorded.
/// The startup resolver falls back to this when neither a flag, env var, session,
/// nor config pins a provider/model — so a fresh launch resumes where you left
/// off. An empty provider (a keyless local endpoint) yields `None`, since it
/// can't be resolved back to a provider preset.
pub fn load_last_model() -> Option<(String, String)> {
    let v: Value = last_model_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())?;
    parse_last_model(&v)
}

/// Pull a valid `(provider, model)` out of the stored JSON. Both must be present
/// and non-empty (an empty provider can't be resolved to a preset). Pure, so the
/// parse rules are testable without a file.
fn parse_last_model(v: &Value) -> Option<(String, String)> {
    let provider = v.get("provider")?.as_str()?.to_string();
    let model = v.get("model")?.as_str()?.to_string();
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

/// Record `(provider, model)` as the most recently used combo. Best-effort: any
/// I/O error is swallowed.
pub fn record_last_model(provider: &str, model: &str) {
    let Some(path) = last_model_path() else {
        return;
    };
    let json = serde_json::json!({ "provider": provider, "model": model }).to_string();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = write_atomic(&path, json.as_bytes());
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
        let out = merge_chatgpt_choices(base, &cg);
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

    #[test]
    fn last_model_parses_a_valid_pair_only() {
        assert_eq!(
            parse_last_model(&json!({"provider": "zen", "model": "grok-code"})),
            Some(("zen".to_string(), "grok-code".to_string()))
        );
        // An empty provider (keyless local) can't resolve to a preset → None.
        assert!(parse_last_model(&json!({"provider": "", "model": "m"})).is_none());
        assert!(parse_last_model(&json!({"provider": "zen"})).is_none());
        assert!(parse_last_model(&json!({})).is_none());
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
