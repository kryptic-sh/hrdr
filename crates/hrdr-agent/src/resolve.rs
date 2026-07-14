//! The ONE seam from a [`ModelRef`] to a usable endpoint.
//!
//! "Which provider, which model" (a [`ModelRef`]) is an *identity*. Talking to it
//! needs a base URL, a key, an API version, headers, a trust kind and a context
//! window — all of which are **derived** from that identity plus the config.
//! Today that derivation is spread across `repoint_to_provider`,
//! `apply_task_overrides`, the `/model` picker, `subagent_usage`, the session
//! restore path… each re-composing `resolve_provider` + `resolve_api_key` +
//! `context_window_for` by hand, and each free to compose them slightly wrong.
//!
//! [`resolve`] is that composition, once. It reimplements none of the rules — it
//! calls the same primitives those call sites call — so it is a *seam*, not a
//! second implementation. Its output, [`ResolvedModel`], is derived state: never
//! persisted, recomputed whenever the [`ModelRef`] or the config changes.

use std::collections::HashMap;

use anyhow::{Result, anyhow};

use crate::model_ref::{ModelRef, catalog_provider_key};
use crate::{
    AgentConfig, BUILTIN_PROVIDERS, CHATGPT_CODEX_BASE_URL, ProviderConfig, ResolvedProviderKind,
    builtin_provider, chatgpt_models, is_codex_oauth, resolve_api_key, resolve_provider_in,
};

/// The key-inheritance context a *child* agent resolves against: the caller's
/// (parent's) own key and the endpoint that key belongs to.
///
/// Both halves are needed, never just the key: [`resolve_api_key`] hands the
/// parent's credential down ONLY when the child resolves to the same `base_url`.
/// A sub-agent profile may name a different provider than its parent, and passing
/// the key without the endpoint it was minted for is exactly the cross-provider
/// key leak that guard exists to stop.
#[derive(Debug, Clone, Copy)]
pub struct AuthContext<'a> {
    /// The parent's resolved API key, if it has one.
    pub api_key: Option<&'a str>,
    /// The endpoint the parent is authenticated against.
    pub base_url: &'a str,
}

/// A [`ModelRef`] resolved against the config: everything needed to actually talk
/// to the model.
///
/// DERIVED state — never persisted. The `ModelRef` is the identity that goes on
/// disk; this is recomputed from it (and the current config) on every change. Two
/// consequences worth stating, because both are load-bearing:
///
/// * the trust [`kind`](Self::kind) comes from [`AgentConfig::resolve_provider`],
///   the sole trust gate — a `[providers.chatgpt]` entry shadows the built-in and
///   resolves `Custom`, so [`is_codex_oauth`](Self::is_codex_oauth) is `false` for
///   it however it is spelled;
/// * the [`context_window`](Self::context_window) is the window derived from
///   `(endpoint, model)` — NOT the final precedence answer. See that accessor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    reference: ModelRef,
    base_url: String,
    api_key: Option<String>,
    api_version: Option<String>,
    headers: Vec<(String, String)>,
    kind: ResolvedProviderKind,
    context_window: Option<u32>,
}

impl ResolvedModel {
    /// The identity this was resolved from.
    pub fn reference(&self) -> &ModelRef {
        &self.reference
    }

    /// The endpoint to send requests to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The API key, if one is available (a keyless local endpoint, or a
    /// ChatGPT-OAuth provider whose Bearer token comes from the OAuth store
    /// instead, resolves `None`).
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// The Azure OpenAI API version, if this is an Azure endpoint.
    pub fn api_version(&self) -> Option<&str> {
        self.api_version.as_deref()
    }

    /// Extra HTTP headers to send with every request, sorted by name (a config
    /// `HashMap` has no order of its own; header order is not significant).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The trust identity — see [`ResolvedProviderKind`].
    pub fn kind(&self) -> ResolvedProviderKind {
        self.kind
    }

    /// The context window DERIVED from `(endpoint, model)` — exactly what
    /// [`context_window_for`](crate::context_window_for) returns.
    ///
    /// This is **not** the final precedence answer. A user-configured window
    /// (`context_window` in `config.toml`, or `[providers.<name>].context_window`)
    /// still overrides it, and an endpoint-probed window (a local server's
    /// `max_model_len`/`n_ctx`) may too. That precedence lives in the callers
    /// today — `Agent::ensure_context_window` only derives when
    /// `context_window_probed` is false, and that flag is initialized from
    /// `config.context_window.is_some()` — and this slice deliberately does NOT
    /// move it here. A later slice wiring `resolve()` into those call sites must
    /// keep the configured value winning over this one.
    pub fn context_window(&self) -> Option<u32> {
        self.context_window
    }

    /// Whether this is the REAL Codex OAuth endpoint — the double gate, in one
    /// place: the trusted [`ResolvedProviderKind::ChatGptOAuth`] kind AND the
    /// canonical [`CHATGPT_CODEX_BASE_URL`].
    ///
    /// Both halves are required. The kind alone is not enough (a built-in
    /// `chatgpt` provider repointed at another URL must not have the OAuth Bearer
    /// or the `ChatGPT-Account-Id` header injected into it), and the URL alone is
    /// not enough (a `[providers.*]` entry pointed at the Codex URL is `Custom` —
    /// it never earns the account's credentials).
    ///
    /// The conjunction itself lives in [`is_codex_oauth`](crate::is_codex_oauth) —
    /// the one definition every call site (`Agent::refresh_oauth_if_needed`,
    /// `oauth::coordinated_oauth_access`, `list_provider_models`, the
    /// `has_oauth_credentials` gating) now goes through.
    pub fn is_codex_oauth(&self) -> bool {
        is_codex_oauth(self.kind, &self.base_url)
    }

    /// Adopt the config's **cached derived** endpoint (`AgentConfig`'s `base_url` /
    /// `api_key` / `api_version` / `headers`) for its identity, rather than
    /// re-deriving it.
    ///
    /// Those fields are what an earlier [`resolve`] produced — but they may have
    /// been *relocated* since (a `--base-url` / `$HRDR_BASE_URL` override, or a
    /// free-floating `base_url =` in config.toml that names no provider). An agent
    /// constructed from such a config must talk to the endpoint it was given, not
    /// to the one its provider preset would resolve to, so construction adopts and
    /// never re-resolves. The trust [`kind`](Self::kind) is still resolved from the
    /// config (the sole trust gate), and the window is still derived from the
    /// `(endpoint, model)` in force.
    pub fn from_config(cfg: &AgentConfig) -> Self {
        let name = cfg.model.provider().as_str();
        let kind = cfg
            .resolve_provider(name)
            .map_or(ResolvedProviderKind::BuiltIn, |p| p.kind);
        Self {
            reference: cfg.model.clone(),
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            api_version: cfg.api_version.clone(),
            headers: cfg.headers.clone(),
            kind,
            context_window: derived_context_window(Some(name), &cfg.base_url, cfg.model.model()),
        }
    }

    /// Move this resolved model onto another endpoint — the `--base-url` override,
    /// which *relocates* a provider rather than replacing it.
    ///
    /// The identity and the trust kind ride along unchanged (this is still that
    /// provider, at a different address), so the double gate re-evaluates honestly:
    /// a built-in ChatGPT relocated off the Codex URL stops being
    /// [`is_codex_oauth`](Self::is_codex_oauth) and is never handed the account's
    /// bearer. The window is re-derived, since it is a fact about the endpoint.
    pub fn relocate(&mut self, base_url: impl Into<String>, api_key: Option<String>) {
        self.base_url = base_url.into();
        self.api_key = api_key;
        self.context_window = derived_context_window(
            Some(self.reference.provider().as_str()),
            &self.base_url,
            self.reference.model(),
        );
    }
}

/// Resolve a [`ModelRef`] against the config into everything needed to talk to it.
///
/// Composed from the existing primitives, in the order the scattered call sites
/// compose them today:
///
/// 1. [`AgentConfig::resolve_provider`] — the sole trust gate; a
///    `[providers.<name>]` entry shadows a built-in of the same name and resolves
///    `Custom`.
/// 2. [`resolve_api_key`] — inline key → `key_env` → the `/login` store → the
///    parent's key, and that last one ONLY when the base URLs match.
/// 3. base URL / API version / headers / kind, straight off the resolved preset.
/// 4. the derived context window — [`context_window_for`](crate::context_window_for),
///    which gates the ChatGPT account catalog on the ENDPOINT, not the name.
///
/// `parent` is the caller's key-inheritance context (see [`AuthContext`]);
/// `None` for a top-level agent that inherits from nobody.
///
/// Errors when the provider names neither a built-in nor a `[providers.<name>]`.
pub fn resolve(
    reference: &ModelRef,
    cfg: &AgentConfig,
    parent: Option<&AuthContext<'_>>,
) -> Result<ResolvedModel> {
    resolve_in(&cfg.providers, reference, parent)
}

/// [`resolve`] against just the `[providers.*]` table — the only part of the
/// config it reads.
///
/// A live [`Agent`](crate::Agent) keeps that table (and not a whole `AgentConfig`,
/// which would be a second, drifting copy of settings it has already unpacked) so
/// [`Agent::set_model_ref`](crate::Agent::set_model_ref) can re-resolve a new
/// identity against the user's providers.
pub fn resolve_in(
    providers: &HashMap<String, ProviderConfig>,
    reference: &ModelRef,
    parent: Option<&AuthContext<'_>>,
) -> Result<ResolvedModel> {
    let name = reference.provider().as_str();
    let p = resolve_provider_in(providers, name).ok_or_else(|| {
        anyhow!(
            "unknown provider '{name}' (built-ins: {}, or define [providers.{name}])",
            BUILTIN_PROVIDERS.join(", ")
        )
    })?;
    let api_key = resolve_api_key(
        name,
        &p,
        parent.and_then(|c| c.api_key),
        parent.map(|c| c.base_url),
    );
    let context_window = derived_context_window(Some(name), &p.base_url, reference.model());
    Ok(ResolvedModel {
        reference: reference.clone(),
        base_url: p.base_url,
        api_key,
        api_version: p.api_version,
        headers: sorted_headers(&p.headers),
        kind: p.kind,
        context_window,
    })
}

/// A config `HashMap` of headers as a stable, ordered list. Header order carries
/// no meaning on the wire; sorting only makes the resolved value deterministic.
fn sorted_headers(headers: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.sort();
    out
}

/// The single implementation of the per-model context window, shared by
/// [`resolve`] and [`context_window_for`](crate::context_window_for) (which
/// delegates here — one implementation, two entry points).
///
/// The ChatGPT branch is gated on the **endpoint**, not the provider name: only
/// the real Codex endpoint reads the account catalog cache (the only place
/// subscription windows live — `/v1/models` 401s there and models.dev lists the
/// differently-windowed API models of the same ids), with the built-in preset as a
/// cold-cache floor. Every other endpoint resolves from models.dev through
/// [`catalog_provider_key`], since the catalog is keyed by ITS names (`opencode`,
/// `anthropic`), not hrdr's (`zen`, `claude`).
pub(crate) fn derived_context_window(
    provider: Option<&str>,
    base_url: &str,
    model: &str,
) -> Option<u32> {
    if base_url == CHATGPT_CODEX_BASE_URL {
        return chatgpt_models::cached_context_window(model)
            .or_else(|| builtin_provider("chatgpt").and_then(|p| p.context_window));
    }
    hrdr_llm::catalog::context_window_cached(catalog_provider_key(provider).as_deref(), model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderConfig, context_window_for};

    /// A config with no `[providers.*]` entries: every name resolves to a built-in.
    fn cfg() -> AgentConfig {
        AgentConfig::default()
    }

    /// A config carrying one `[providers.<name>]` entry.
    fn cfg_with(name: &str, p: ProviderConfig) -> AgentConfig {
        let mut c = AgentConfig::default();
        c.providers.insert(name.to_string(), p);
        c
    }

    fn provider_config(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            base_url: base_url.to_string(),
            key_env: None,
            api_key: None,
            model: None,
            remote: None,
            context_window: None,
            headers: HashMap::new(),
            api_version: None,
        }
    }

    fn r(s: &str) -> ModelRef {
        s.parse().unwrap()
    }

    /// PARITY: every built-in resolves to exactly what `builtin_provider` says —
    /// endpoint, trust kind, API version, headers. `resolve()` adds no opinion of
    /// its own.
    #[test]
    fn builtins_resolve_exactly_as_builtin_provider_does() {
        for name in BUILTIN_PROVIDERS {
            let want = builtin_provider(name).expect("a built-in resolves");
            let got = resolve(&r(&format!("{name}://some-model")), &cfg(), None).unwrap();
            assert_eq!(got.base_url(), want.base_url, "{name}: base_url");
            assert_eq!(got.kind(), want.kind, "{name}: kind");
            assert_eq!(got.api_version(), want.api_version.as_deref(), "{name}");
            assert_eq!(got.headers(), sorted_headers(&want.headers), "{name}");
            assert_eq!(got.reference().model(), "some-model", "{name}");
        }
        // The endpoints themselves, spelled out — a preset URL changing under us is
        // a behavior change, and this slice permits none.
        let url = |n: &str| {
            resolve(&r(&format!("{n}://m")), &cfg(), None)
                .unwrap()
                .base_url
        };
        assert_eq!(url("zen"), "https://opencode.ai/zen/v1");
        assert_eq!(url("go"), "https://opencode.ai/zen/go/v1");
        assert_eq!(url("openai"), "https://api.openai.com/v1");
        assert_eq!(url("openrouter"), "https://openrouter.ai/api/v1");
        assert_eq!(url("claude"), "https://api.anthropic.com/v1");
        assert_eq!(url("chatgpt"), CHATGPT_CODEX_BASE_URL);
        assert_eq!(url("local"), "http://localhost:8080/v1");
        // Only the built-in ChatGPT preset is OAuth-trusted, and it sits on the
        // canonical endpoint — so it, and only it, passes the double gate.
        for name in BUILTIN_PROVIDERS {
            let m = resolve(&r(&format!("{name}://m")), &cfg(), None).unwrap();
            assert_eq!(m.is_codex_oauth(), *name == "chatgpt", "{name}");
        }
        // Aliases fold before resolution: `anthropic://` IS `claude://`.
        assert_eq!(
            resolve(&r("anthropic://m"), &cfg(), None).unwrap(),
            resolve(&r("claude://m"), &cfg(), None).unwrap()
        );
        assert!(
            resolve(&r("codex://m"), &cfg(), None)
                .unwrap()
                .is_codex_oauth()
        );
    }

    /// THE TRUST GATE: a `[providers.chatgpt]` entry shadows the built-in — the
    /// name buys nothing. It resolves `Custom`, so it can never be handed the
    /// account's OAuth Bearer, however it is spelled.
    #[test]
    fn a_config_entry_named_chatgpt_shadows_the_builtin_and_is_never_oauth() {
        const URL: &str = "http://localhost:9099/v1";
        let cfg = cfg_with("chatgpt", provider_config(URL));
        let m = resolve(&r("chatgpt://gpt-5.5"), &cfg, None).unwrap();
        assert_eq!(m.kind(), ResolvedProviderKind::Custom, "user entry wins");
        assert_eq!(m.base_url(), URL);
        assert!(!m.is_codex_oauth(), "the double gate holds on the kind");
        // And it windows like any other endpoint: models.dev, never the account
        // catalog / preset floor. Deterministic on a slug no catalog knows — the
        // shadowed endpoint yields nothing, where the real Codex one would have
        // yielded its 272k floor.
        let fake = resolve(&r("chatgpt://totally-fake-model-xyz"), &cfg, None).unwrap();
        assert_eq!(fake.context_window(), None);
        assert_eq!(
            fake.context_window(),
            context_window_for(Some("chatgpt"), URL, "totally-fake-model-xyz"),
        );
        // The alias spellings fold onto `chatgpt`, so they hit the same shadow.
        assert!(
            !resolve(&r("codex://gpt-5.5"), &cfg, None)
                .unwrap()
                .is_codex_oauth()
        );
    }

    /// The surprising-but-correct interaction, pinned: the TRUST gate keys on the
    /// name-vs-config shadow, the WINDOW gate keys on the endpoint. A custom
    /// provider aimed at the real Codex URL is therefore `Custom` (no OAuth) yet
    /// reads its window from the ChatGPT account cache.
    #[test]
    fn a_custom_provider_at_the_codex_url_is_custom_but_windows_from_the_account_cache() {
        let cfg = cfg_with("myproxy", provider_config(CHATGPT_CODEX_BASE_URL));
        let m = resolve(&r("myproxy://totally-fake-model-xyz"), &cfg, None).unwrap();
        assert_eq!(m.kind(), ResolvedProviderKind::Custom);
        assert!(
            !m.is_codex_oauth(),
            "the endpoint is right but the kind is not — no OAuth credentials for it"
        );
        // …and yet: the window comes from the account catalog (preset floor for an
        // uncached slug), because `context_window_for` gates on the endpoint.
        assert_eq!(m.context_window(), Some(272_000));
        assert_eq!(
            m.context_window(),
            context_window_for(
                Some("myproxy"),
                CHATGPT_CODEX_BASE_URL,
                "totally-fake-model-xyz"
            ),
            "parity with today's function, surprise included",
        );
    }

    /// KEY PRECEDENCE, in full: inline → `key_env` → (the `/login` store) → the
    /// parent's key, and the parent's ONLY across the same endpoint.
    #[test]
    fn key_precedence_matches_resolve_api_key() {
        const URL: &str = "http://localhost:9099/v1";
        // `PATH` is always set in a test process and is never our inline/parent key,
        // so `key_env` can be exercised without mutating the process environment.
        let path = std::env::var("PATH").expect("PATH is set for the test process");
        let parent = AuthContext {
            api_key: Some("parent-key"),
            base_url: URL,
        };

        // 1. inline beats key_env.
        let mut p = provider_config(URL);
        p.api_key = Some("inline-key".into());
        p.key_env = Some("PATH".into());
        let cfg = cfg_with("keytest", p);
        let m = resolve(&r("keytest://m"), &cfg, Some(&parent)).unwrap();
        assert_eq!(m.api_key(), Some("inline-key"));

        // 2. key_env beats the parent's key.
        let mut p = provider_config(URL);
        p.key_env = Some("PATH".into());
        let cfg = cfg_with("keytest", p);
        let m = resolve(&r("keytest://m"), &cfg, Some(&parent)).unwrap();
        assert_eq!(m.api_key(), Some(path.as_str()));
        assert_ne!(m.api_key(), Some("parent-key"));

        // 3. no inline, no key_env, no stored credential → the parent's key is
        //    inherited, because the base URLs match.
        let cfg = cfg_with("keytest", provider_config(URL));
        let m = resolve(&r("keytest://m"), &cfg, Some(&parent)).unwrap();
        assert_eq!(m.api_key(), Some("parent-key"));
        // Trailing-slash-insensitive, exactly as `resolve_api_key` compares.
        let slashed = AuthContext {
            api_key: Some("parent-key"),
            base_url: "http://localhost:9099/v1/",
        };
        assert_eq!(
            resolve(&r("keytest://m"), &cfg, Some(&slashed))
                .unwrap()
                .api_key(),
            Some("parent-key")
        );

        // 4. THE LEAK GUARD: a different endpoint never inherits the parent's key.
        let elsewhere = AuthContext {
            api_key: Some("parent-key"),
            base_url: "https://api.openai.com/v1",
        };
        let m = resolve(&r("keytest://m"), &cfg, Some(&elsewhere)).unwrap();
        assert_eq!(m.api_key(), None, "a key never crosses endpoints");
        // No parent at all → nothing to inherit.
        assert_eq!(
            resolve(&r("keytest://m"), &cfg, None).unwrap().api_key(),
            None
        );

        // PARITY: whatever the store/env say, `resolve()` says what `resolve_api_key`
        // says — it is the same call, not a second implementation.
        let p = cfg.resolve_provider("keytest").unwrap();
        assert_eq!(
            resolve(&r("keytest://m"), &cfg, Some(&parent))
                .unwrap()
                .api_key()
                .map(str::to_string),
            resolve_api_key("keytest", &p, Some("parent-key"), Some(URL)),
        );
    }

    /// The window is derived from the ENDPOINT, and non-Codex endpoints go through
    /// the models.dev CATALOG key (`zen` → `opencode`), never the raw app name.
    #[test]
    fn context_window_derives_from_the_endpoint_and_the_catalog_key() {
        // The Codex endpoint: an uncached slug lands on the 272k preset floor —
        // models.dev is never consulted for it.
        let m = resolve(&r("chatgpt://totally-fake-model-xyz"), &cfg(), None).unwrap();
        assert!(m.is_codex_oauth());
        assert_eq!(m.context_window(), Some(272_000));

        // Everyone else: models.dev, keyed by the CATALOG name. Asserted against the
        // catalog directly, so it holds with or without a cached models.json.
        for (provider, catalog_key) in [
            ("zen", Some("opencode")),
            ("go", Some("opencode-go")),
            ("claude", Some("anthropic")),
            ("openai", Some("openai")),
            ("openrouter", Some("openrouter")),
            ("local", None),
        ] {
            assert_eq!(
                catalog_provider_key(Some(provider)).as_deref(),
                catalog_key,
                "{provider}: catalog key"
            );
            let m = resolve(&r(&format!("{provider}://kimi-k2")), &cfg(), None).unwrap();
            assert_eq!(
                m.context_window(),
                hrdr_llm::catalog::context_window_cached(catalog_key, "kimi-k2"),
                "{provider}: resolved through the catalog key, not the app name"
            );
            // …which is exactly what today's `context_window_for` returns.
            assert_eq!(
                m.context_window(),
                context_window_for(Some(provider), m.base_url(), "kimi-k2"),
                "{provider}: parity"
            );
        }
    }

    /// A name that is neither a built-in nor a `[providers.<name>]` is an error —
    /// the same message shape the call sites raise today.
    #[test]
    fn an_unknown_provider_is_an_error() {
        let err = resolve(&r("nosuchprovider://m"), &cfg(), None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("unknown provider 'nosuchprovider'"),
            "{msg}"
        );
        assert!(msg.contains("built-ins: zen, go, openai"), "{msg}");
        assert!(msg.contains("[providers.nosuchprovider]"), "{msg}");
        // …but a `[providers.*]` entry makes any name resolvable.
        let cfg = cfg_with("nosuchprovider", provider_config("http://x/v1"));
        assert!(resolve(&r("nosuchprovider://m"), &cfg, None).is_ok());
    }

    /// A custom entry's api_version and headers ride through untouched (Azure).
    #[test]
    fn custom_entries_carry_their_api_version_and_headers() {
        let mut p = provider_config("https://acme.openai.azure.com/openai/deployments/gpt5");
        p.api_version = Some("2024-08-01-preview".into());
        p.headers = HashMap::from([
            ("X-Title".to_string(), "hrdr".to_string()),
            ("HTTP-Referer".to_string(), "https://hrdr.dev".to_string()),
        ]);
        let cfg = cfg_with("azure", p);
        let m = resolve(&r("azure://gpt-5"), &cfg, None).unwrap();
        assert_eq!(m.api_version(), Some("2024-08-01-preview"));
        assert_eq!(
            m.headers(),
            [
                ("HTTP-Referer".to_string(), "https://hrdr.dev".to_string()),
                ("X-Title".to_string(), "hrdr".to_string()),
            ],
            "sorted by name, so the resolved value is deterministic"
        );
    }
}
