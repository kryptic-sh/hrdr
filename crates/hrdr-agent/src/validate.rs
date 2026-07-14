//! Is this identity *real*? — the network-free checks hrdr runs on a
//! [`ModelRef`] before it commits to talking to it.
//!
//! The refactor that put a provider and a model into one value made a whole class
//! of mismatch *representable but wrong*: `chatgpt://gpt-4o` parses, resolves, and
//! then 404s mid-turn; `claude://sonet` is a typo you find out about after the
//! first request; `local://default` is a lie on any server that serves a model
//! list. This module answers those before the first token, from caches already on
//! disk.
//!
//! The rule that governs every check here: **REFUSE only what we KNOW is wrong.**
//!
//! * The ChatGPT account catalog is an ENTITLEMENT list — it is the account's own
//!   answer to "what may I run". A model missing from a populated one is a fact,
//!   so it is a refusal.
//! * The models.dev catalog is a THIRD-PARTY index, and it lags: a model released
//!   this morning is not in it. Absence there is evidence, never proof, so it is a
//!   warning and the request still goes out.
//! * A local server, a custom `[providers.*]`, a provider models.dev has never
//!   heard of — we know nothing at all. Silence, not a guess.
//!
//! Everything below is pure over its inputs (the caches are loaded at the edges),
//! so the rules are testable without a network, a cache file, or a `$HOME`.

use std::collections::HashMap;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::chatgpt_models::ChatGptModel;
use crate::model_ref::ModelRef;
use crate::resolve::ResolvedModel;
use crate::{
    AgentConfig, ProviderConfig, ResolvedProviderKind, is_local_endpoint, resolve_provider_in,
};

/// The model id that means "whatever this server is serving" — a *placeholder*,
/// not a name. Honest only against an endpoint with no model namespace to name a
/// model in. See [`validate_placeholder_model`].
pub const PLACEHOLDER_MODEL: &str = "default";

/// How many ids a message lists before it stops.
const SAMPLE: usize = 8;

/// `a, b, c` — the first few of `items`, with an ellipsis when there are more.
fn sample(items: impl IntoIterator<Item = String>, total: usize) -> String {
    let shown: Vec<String> = items.into_iter().take(SAMPLE).collect();
    let mut s = shown.join(", ");
    if total > shown.len() {
        s.push_str(", …");
    }
    s
}

/// Validate a resolved identity against the catalogs already cached on disk.
///
/// `Err` = **refuse**: we know this model is not on this provider, and sending the
/// request would only produce a worse error later. `Ok(warnings)` = proceed, having
/// said what looks wrong.
///
/// Network-free and cheap — safe to call on every `/model` switch, not just at
/// startup. See the module docs for why exactly one of the two catalogs is allowed
/// to refuse.
pub fn validate_identity(m: &ResolvedModel, cfg: &AgentConfig) -> Result<Vec<String>> {
    validate_identity_in(&cfg.providers, m)
}

/// [`validate_identity`] against just the `[providers.*]` table — the only part of
/// the config it reads, and all a live [`Agent`](crate::Agent) keeps. Mirrors
/// [`resolve_in`](crate::resolve_in), for the same reason: an agent re-validating a
/// candidate identity has no `AgentConfig` to hand, and a second copy of one would
/// be a second copy that can drift.
///
/// This is the edge that touches disk: it loads both caches (never the network) and
/// hands them to the pure core.
pub fn validate_identity_in(
    providers: &HashMap<String, ProviderConfig>,
    m: &ResolvedModel,
) -> Result<Vec<String>> {
    // The account catalog is keyed by ACCOUNT: only the credential currently in the
    // OAuth store can say which rows are this account's entitlements.
    let entitled = m
        .is_codex_oauth()
        .then(|| crate::load_oauth("chatgpt")?.account_id)
        .flatten()
        .and_then(|id| crate::chatgpt_models::cached_entitlements(&id));
    let catalog = hrdr_llm::catalog::load_cached();
    validate_identity_with(providers, m, entitled.as_deref(), catalog.as_ref())
}

/// The pure core of [`validate_identity`]: both catalogs are passed in, so every
/// rule (and every "we cannot know" hole in them) is testable without disk.
///
/// `entitled` is the ChatGPT ACCOUNT catalog for the credential in force — `None`
/// when there is no usable cache for it, which is emphatically not the same as an
/// empty one. `catalog` is the models.dev index, `None` when it has never been
/// fetched.
fn validate_identity_with(
    providers: &HashMap<String, ProviderConfig>,
    m: &ResolvedModel,
    entitled: Option<&[ChatGptModel]>,
    catalog: Option<&Value>,
) -> Result<Vec<String>> {
    let reference = m.reference();
    let model = reference.model();

    // 1. The ChatGPT/Codex endpoint is AUTHORITATIVE about itself. The account
    //    catalog is not an index of models that exist — it is the list of models
    //    THIS ACCOUNT may run, published by the endpoint that will serve them. A
    //    populated one that omits the slug is proof, and proof refuses.
    if m.is_codex_oauth() {
        // No usable cache (cold, or written for another account): we cannot know,
        // so we do not refuse — and we do not warn either, because there is nothing
        // to report but our own ignorance.
        let Some(rows) = entitled.filter(|r| !r.is_empty()) else {
            return Ok(Vec::new());
        };
        if !rows.iter().any(|r| r.slug == model) {
            let slugs = sample(rows.iter().map(|r| r.slug.clone()), rows.len());
            bail!(
                "model '{model}' is not entitled on this ChatGPT account — \
                 entitled: {slugs} (run `/model` to pick one)"
            );
        }
        return Ok(Vec::new());
    }

    // 2. A `[providers.<name>]` entry SHADOWS the built-in: the endpoint is the
    //    user's, so models.dev's list for that name describes somebody else's
    //    server. `resolve_provider` is the sole trust gate — ask it, not the raw
    //    map, so a shadow spelled with any casing is caught.
    let name = reference.provider();
    let shadowed = resolve_provider_in(providers, name.as_str())
        .is_some_and(|p| p.kind == ResolvedProviderKind::Custom);
    if shadowed || m.kind() == ResolvedProviderKind::Custom {
        return Ok(Vec::new());
    }

    // 3. models.dev knows this provider → it may only WARN. The catalog lags every
    //    new release by days; a model shipped this morning must still run. Refusing
    //    on its silence would make hrdr unusable on exactly the models people are
    //    most excited to try.
    let Some(catalog) = catalog else {
        return Ok(Vec::new());
    };
    let Some(key) = name.catalog_key() else {
        // `local`, `chatgpt`, a custom name — models.dev covers none of them.
        return Ok(Vec::new());
    };
    // The provider is absent from the cached catalog (a partial or stale index):
    // that is a fact about the catalog, not about the model. Silence.
    let Some((_, models)) = hrdr_llm::catalog::provider_models(catalog, key) else {
        return Ok(Vec::new());
    };
    if models.is_empty() || models.iter().any(|(id, _)| id == model) {
        return Ok(Vec::new());
    }
    Ok(vec![format!(
        "⚠ models.dev doesn't list '{model}' on {name} — it may be new, or a typo"
    )])
}

/// `"default"` is only honest against a server with **no model namespace**.
///
/// It is a placeholder meaning "whatever you are serving", and it says something
/// true exactly when the endpoint has nothing to name: a `llama-server` started on
/// one GGUF, an `infr serve`, a 404 on `/v1/models`. Against a server that
/// advertises a list, `default` names nothing — the request goes out with a model
/// id the server has never heard of, and the failure that comes back describes the
/// model, not the mistake.
///
/// `advertised` is what `/v1/models` returned: `None` when the probe failed or the
/// endpoint has no list (**fail open** — refusing a session over a network blip
/// would be hostile, and the existing unreachable-endpoint warning already covers
/// it), `Some(&[])` when it serves nothing to name.
///
/// Applies to ANY endpoint, not just `local`: a custom `[providers.*]` can be left
/// on `default` too.
pub fn validate_placeholder_model(
    reference: &ModelRef,
    advertised: Option<&[String]>,
) -> Result<()> {
    if reference.model() != PLACEHOLDER_MODEL {
        return Ok(());
    }
    let Some(models) = advertised.filter(|m| !m.is_empty()) else {
        return Ok(());
    };
    let provider = reference.provider();
    let n = models.len();
    let example = &models[0];
    let list = sample(models.iter().cloned(), n);
    bail!(
        "this endpoint serves {n} models — name one (e.g. --model '{provider}://{example}'); \
         '{PLACEHOLDER_MODEL}' only means something on a server with no model list. \
         It serves: {list}"
    )
}

/// The two things a `--base-url` relocation quietly changes, said out loud.
///
/// `--base-url` **relocates** a provider — same identity, same key, different
/// address — which is exactly what a proxy (LiteLLM, a corporate gateway) needs,
/// and exactly why two invisible things happen:
///
/// * **the wire protocol follows the HOST**, not the provider. `detect_backend`
///   keys on the hostname, so moving `claude://sonnet` to `localhost` swaps the
///   Anthropic Messages API for OpenAI chat-completions — a different request
///   shape, silently.
/// * **the API key follows the URL.** A relocated *keyed* provider sends that
///   provider's credential to the new host. For a proxy that is the point (it needs
///   the upstream key to forward). For `--base-url http://evil.example/v1` it is
///   your Anthropic key, gone. So: a warning, never an error — hrdr cannot tell the
///   two apart, and only the user can.
///
/// Silent when the endpoint was not relocated, and — deliberately — when the
/// relocation is LOCAL (pointing at your own server is the overwhelmingly common
/// case, and your machine is not a third party) or the provider is keyless.
pub fn relocation_warnings(m: &ResolvedModel, canonical_base_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    let relocated = m.base_url();
    let canonical_host = hrdr_llm::url_host(canonical_base_url);
    let host = hrdr_llm::url_host(relocated);
    if relocated.trim_end_matches('/') == canonical_base_url.trim_end_matches('/') {
        return out;
    }
    let name = m.reference().provider();

    // (a) The wire protocol is a function of the host, so a relocation can change
    //     which API hrdr speaks without changing a single line of config.
    let was = hrdr_llm::wire_protocol(canonical_base_url);
    let now = hrdr_llm::wire_protocol(relocated);
    if was != now {
        out.push(format!(
            "⚠ --base-url moves {name} off {canonical_host} — hrdr will speak the {now} API here, not {was}'s"
        ));
    }

    // (b) The credential rides along. Only worth saying when there IS one, and only
    //     when it is leaving this machine for a host that is not the provider's own.
    if m.api_key().is_some() && !is_local_endpoint(relocated) && host != canonical_host {
        out.push(format!(
            "⚠ your {name} API key will be sent to {host} (--base-url moved the endpoint off {canonical_host})"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::{CHATGPT_CODEX_BASE_URL, ProviderConfig, resolve};
    use serde_json::json;

    fn r(s: &str) -> ModelRef {
        s.parse().unwrap()
    }

    fn cfg() -> AgentConfig {
        AgentConfig::default()
    }

    fn cfg_with(name: &str, base_url: &str) -> AgentConfig {
        let mut c = AgentConfig::default();
        c.providers.insert(
            name.to_string(),
            ProviderConfig {
                base_url: base_url.to_string(),
                key_env: None,
                api_key: None,
                model: None,
                remote: None,
                context_window: None,
                headers: HashMap::new(),
                api_version: None,
            },
        );
        c
    }

    fn resolved(spec: &str, cfg: &AgentConfig) -> ResolvedModel {
        resolve(&r(spec), cfg, None).expect("resolves")
    }

    fn entitlements(slugs: &[&str]) -> Vec<ChatGptModel> {
        slugs
            .iter()
            .map(|s| ChatGptModel {
                slug: (*s).to_string(),
                label: (*s).to_string(),
                context_window: Some(272_000),
            })
            .collect()
    }

    /// The models.dev catalog, as cached: `anthropic` and `openai` are known;
    /// `openrouter` is absent from THIS index entirely.
    fn catalog() -> Value {
        json!({
            "anthropic": { "models": {
                "claude-sonnet-4-5": { "limit": { "context": 200_000 } },
                "claude-opus-4-8": { "limit": { "context": 200_000 } },
            }},
            "openai": { "models": {
                "gpt-5": { "limit": { "context": 400_000 } },
            }},
        })
    }

    /// THE REFUSAL: the ChatGPT account catalog is the account's own entitlement
    /// list, published by the endpoint that would serve the request. A populated one
    /// that omits the slug is not a hint — it is the answer.
    #[test]
    fn a_chatgpt_model_absent_from_a_populated_account_catalog_is_refused() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-4o", &cfg);
        assert!(
            m.is_codex_oauth(),
            "the real Codex endpoint, or no authority"
        );
        let rows = entitlements(&["gpt-5.5", "gpt-5.5-codex", "gpt-5.3-codex-spark"]);

        let err = validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog()))
            .expect_err("a model this account cannot run is refused")
            .to_string();
        assert!(err.contains("'gpt-4o' is not entitled"), "{err}");
        assert!(err.contains("gpt-5.5"), "it names what IS entitled: {err}");
        assert!(err.contains("/model"), "and how to fix it: {err}");

        // An entitled slug passes, silently.
        let ok = resolved("chatgpt://gpt-5.5-codex", &cfg);
        assert_eq!(
            validate_identity_with(&cfg.providers, &ok, Some(&rows), Some(&catalog())).unwrap(),
            Vec::<String>::new()
        );
    }

    /// …and the SAME model is NOT refused when we have no cache to refuse it from.
    /// A cold cache, or one written for a different account, is ignorance — and
    /// ignorance never refuses. (`cached_entitlements` returns `None` for both, so a
    /// `None` here IS the cold/other-account case.)
    #[test]
    fn a_cold_or_empty_account_catalog_refuses_nothing_and_warns_about_nothing() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-4o", &cfg);
        for entitled in [None, Some(&[][..])] {
            assert_eq!(
                validate_identity_with(&cfg.providers, &m, entitled, Some(&catalog())).unwrap(),
                Vec::<String>::new(),
                "no usable catalog → no refusal, and nothing to say either",
            );
        }
        // And models.dev never gets a say about ChatGPT: it lists the differently
        // windowed *API* models under `openai`, which are not this account's
        // entitlements. `chatgpt` has no catalog key at all — rule 1 is the whole rule.
        assert!(m.reference().provider().catalog_key().is_none());
    }

    /// A `[providers.chatgpt]` shadow is `Custom`, never OAuth — so it is never
    /// measured against somebody's account entitlements. It is the user's own server.
    #[test]
    fn a_shadowed_chatgpt_provider_is_never_refused_by_the_account_catalog() {
        let cfg = cfg_with("chatgpt", "http://localhost:9099/v1");
        let m = resolved("chatgpt://gpt-4o", &cfg);
        assert!(!m.is_codex_oauth());
        let rows = entitlements(&["gpt-5.5"]);
        assert_eq!(
            validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog())).unwrap(),
            Vec::<String>::new()
        );
    }

    /// models.dev only ever WARNS. It is a third-party index and it lags: a model
    /// released this morning is not in it, and must still run. Refusing on its
    /// silence would break hrdr on exactly the models people most want to try.
    #[test]
    fn an_unknown_model_on_a_models_dev_provider_warns_but_never_errs() {
        let cfg = cfg();
        let m = resolved("claude://claude-sonet-4-5", &cfg); // typo'd
        let warnings = validate_identity_with(&cfg.providers, &m, None, Some(&catalog()))
            .expect("models.dev NEVER refuses");
        assert_eq!(
            warnings,
            ["⚠ models.dev doesn't list 'claude-sonet-4-5' on claude — it may be new, or a typo"],
        );
        // A model it does list is silent.
        let known = resolved("claude://claude-opus-4-8", &cfg);
        assert!(
            validate_identity_with(&cfg.providers, &known, None, Some(&catalog()))
                .unwrap()
                .is_empty()
        );
        // The warning is keyed through the CATALOG name (`claude` → `anthropic`), so
        // an alias spelling folds onto the same answer.
        let alias = resolved("anthropic://claude-opus-4-8", &cfg);
        assert!(
            validate_identity_with(&cfg.providers, &alias, None, Some(&catalog()))
                .unwrap()
                .is_empty()
        );
    }

    /// We know nothing → we say nothing. A local server, a custom provider, and a
    /// provider the cached catalog has never heard of are all SILENT: a brand-new
    /// model on a provider models.dev does not cover must not be nagged about.
    #[test]
    fn what_we_cannot_know_we_do_not_mention() {
        let catalog = catalog();
        // `local`: no catalog key. A local server serves whatever it was started with.
        let cfg = cfg();
        for spec in ["local://qwen3-coder-next", "local://default"] {
            let m = resolved(spec, &cfg);
            assert!(
                validate_identity_with(&cfg.providers, &m, None, Some(&catalog))
                    .unwrap()
                    .is_empty(),
                "{spec}"
            );
        }
        // A custom `[providers.*]`: models.dev describes somebody else's server.
        let custom = cfg_with("mygateway", "https://gw.internal/v1");
        let m = resolved("mygateway://whatever-v9", &custom);
        assert!(
            validate_identity_with(&custom.providers, &m, None, Some(&catalog))
                .unwrap()
                .is_empty()
        );
        // A built-in models.dev DOES key (`openrouter`) but which is absent from this
        // cached index: that is a fact about the catalog, not the model. Silence.
        let m = resolved("openrouter://deepseek/deepseek-v4", &cfg);
        assert!(m.reference().provider().catalog_key().is_some());
        assert!(
            validate_identity_with(&cfg.providers, &m, None, Some(&catalog))
                .unwrap()
                .is_empty(),
            "a provider the catalog does not carry is not evidence of anything",
        );
        // No cached catalog at all → nothing to say about anyone.
        let m = resolved("claude://claude-sonet-4-5", &cfg);
        assert!(
            validate_identity_with(&cfg.providers, &m, None, None)
                .unwrap()
                .is_empty()
        );
    }

    /// `default` is a PLACEHOLDER, and it is a lie against a server with a model
    /// list. It is legal only where there is nothing to name.
    #[test]
    fn default_is_legal_only_where_the_endpoint_names_nothing() {
        let local = r("local://default");
        let advertised = [
            "qwen3-coder".to_string(),
            "llama-3.3-70b".to_string(),
            "gpt-oss-120b".to_string(),
        ];

        // The endpoint advertises a namespace → `default` names nothing in it.
        let err = validate_placeholder_model(&local, Some(&advertised))
            .expect_err("a served model list makes `default` meaningless")
            .to_string();
        assert!(err.contains("this endpoint serves 3 models"), "{err}");
        assert!(
            err.contains("--model 'local://qwen3-coder'"),
            "it names a real one: {err}"
        );
        assert!(
            err.contains("qwen3-coder, llama-3.3-70b, gpt-oss-120b"),
            "{err}"
        );
        assert!(
            err.contains("only means something on a server with no model list"),
            "{err}"
        );

        // A 404 / an empty list → `default` is exactly right. Proceed, silently.
        assert!(validate_placeholder_model(&local, Some(&[])).is_ok());
        // The probe failed (timeout, connection refused) → FAIL OPEN. Refusing a
        // session over a network blip would be hostile, and the unreachable-endpoint
        // warning already covers it.
        assert!(validate_placeholder_model(&local, None).is_ok());
        // A NAMED model is never the placeholder, whatever the endpoint serves.
        assert!(validate_placeholder_model(&r("local://qwen3-coder"), Some(&advertised)).is_ok());

        // ANY endpoint, not just `local` — a custom provider can be left on `default`
        // too, and the suggestion is spelled with ITS name.
        let custom = r("mygateway://default");
        let err = validate_placeholder_model(&custom, Some(&advertised))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--model 'mygateway://qwen3-coder'"), "{err}");
    }

    /// (a) THE WIRE-PROTOCOL FLIP. `detect_backend` keys on the HOST, so relocating
    /// `claude` off api.anthropic.com stops speaking the Anthropic Messages API and
    /// starts speaking OpenAI chat-completions — a different request shape, with no
    /// config change to point at.
    #[test]
    fn relocating_a_provider_off_its_host_warns_that_the_api_changed() {
        const ANTHROPIC: &str = "https://api.anthropic.com/v1";
        let cfg = cfg();
        let mut m = resolved("claude://claude-sonnet-4-5", &cfg);
        assert_eq!(m.base_url(), ANTHROPIC);

        m.relocate("http://localhost:1234/v1", None);
        let warnings = relocation_warnings(&m, ANTHROPIC);
        assert_eq!(
            warnings,
            [
                "⚠ --base-url moves claude off api.anthropic.com — hrdr will speak the OpenAI API here, not Anthropic's"
            ],
            "the protocol flip is named; a keyless local move leaks nothing, so that is all",
        );

        // A relocation that does NOT cross a protocol boundary says nothing about the
        // protocol: openai → an OpenAI-shaped gateway is still chat-completions.
        let mut m = resolved("openai://gpt-5", &cfg);
        let canonical = m.base_url().to_string();
        m.relocate("http://localhost:4000/v1", None);
        assert!(relocation_warnings(&m, &canonical).is_empty());

        // Not relocated at all → nothing to warn about.
        let m = resolved("claude://claude-sonnet-4-5", &cfg);
        assert!(relocation_warnings(&m, ANTHROPIC).is_empty());
        // The same host at a trailing slash is the same endpoint, not a relocation.
        let mut same = resolved("claude://claude-sonnet-4-5", &cfg);
        same.relocate(format!("{ANTHROPIC}/"), Some("sk-ant".to_string()));
        assert!(relocation_warnings(&same, ANTHROPIC).is_empty());
    }

    /// (b) THE KEY FOLLOWS THE URL. Relocating a KEYED provider ships that
    /// provider's credential to the new host. Intentional for a proxy (LiteLLM needs
    /// the upstream key), catastrophic for a typo — so hrdr says where the key is
    /// going and lets the user decide.
    #[test]
    fn relocating_a_keyed_provider_to_a_foreign_host_warns_where_the_key_is_going() {
        const ANTHROPIC: &str = "https://api.anthropic.com/v1";
        let cfg = cfg();
        let mut m = resolved("claude://claude-sonnet-4-5", &cfg);
        m.relocate("http://evil.example/v1", Some("sk-ant-secret".to_string()));

        let warnings = relocation_warnings(&m, ANTHROPIC);
        assert!(
            warnings.contains(
                &"⚠ your claude API key will be sent to evil.example (--base-url moved the endpoint off api.anthropic.com)"
                    .to_string()
            ),
            "{warnings:?}"
        );
        // The message never contains the key itself.
        assert!(!warnings.join("\n").contains("sk-ant-secret"));

        // A LOCAL relocation is the common case — your own machine is not a third
        // party. Keyed or not, no key warning.
        let mut local = resolved("claude://claude-sonnet-4-5", &cfg);
        local.relocate(
            "http://localhost:1234/v1",
            Some("sk-ant-secret".to_string()),
        );
        assert!(
            !relocation_warnings(&local, ANTHROPIC)
                .iter()
                .any(|w| w.contains("API key will be sent")),
            "pointing at your own server leaks nothing",
        );
        // A KEYLESS relocation has no credential to leak.
        let mut keyless = resolved("claude://claude-sonnet-4-5", &cfg);
        keyless.relocate("http://gateway.example/v1", None);
        assert!(
            !relocation_warnings(&keyless, ANTHROPIC)
                .iter()
                .any(|w| w.contains("API key will be sent")),
        );
        // A relocation to the provider's OWN host (a different path on it) is not a
        // move off the provider at all.
        let mut same_host = resolved("claude://claude-sonnet-4-5", &cfg);
        same_host.relocate(
            "https://api.anthropic.com/v2",
            Some("sk-ant-secret".to_string()),
        );
        assert!(relocation_warnings(&same_host, ANTHROPIC).is_empty());
    }

    /// The real entry point agrees with the pure core on the one case that needs no
    /// disk: a Codex identity with no cached account catalog (the test process has
    /// no OAuth store) refuses nothing.
    #[test]
    fn the_public_entry_point_is_network_free_and_refuses_nothing_it_cannot_prove() {
        let cfg = cfg();
        let m = resolved("local://qwen3", &cfg);
        assert!(validate_identity(&m, &cfg).is_ok());
        assert_eq!(m.base_url(), "http://localhost:8080/v1");
        // …and the Codex endpoint is the authoritative one, so the double gate that
        // decides it is worth pinning here too.
        let codex = resolved("chatgpt://gpt-5.5", &cfg);
        assert_eq!(codex.base_url(), CHATGPT_CODEX_BASE_URL);
        assert!(codex.is_codex_oauth());
    }
}
