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
//! * The ChatGPT account catalog is an ENTITLEMENT list — the account's own answer
//!   to "what may I run", published by the endpoint that would serve it. It is the
//!   one source allowed to refuse. But a *cached* copy of it may only ever prove
//!   PRESENCE: an entitlement list grows (OpenAI ships a model, your account gets
//!   it, your hour-old cache does not know), so an absence from a stale snapshot is
//!   not evidence of anything. Absence is therefore a distinct outcome
//!   ([`Identity::Unconfirmed`]) that the *edge* must resolve by fetching a fresh
//!   list — and only a FRESH list may produce an `Err`.
//! * The models.dev catalog is a THIRD-PARTY index, and it lags too. Absence there
//!   is evidence, never proof, so it is a warning and the request still goes out.
//! * A local server, a custom `[providers.*]`, a provider models.dev has never
//!   heard of — we know nothing at all. Silence, not a guess.
//!
//! The [`validate_identity`] pass is pure over its inputs (the caches are loaded at
//! its edge) and never touches the network, so it is affordable on every `/model`
//! switch. The one round-trip lives in [`confirm_identity`], and is paid only in the
//! rare case where hrdr is about to refuse someone — exactly when it is worth paying.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::chatgpt_models::{CatalogSource, ChatGptModel, chatgpt_model_catalog};
use crate::model_ref::ModelRef;
use crate::oauth::coordinated_oauth_access;
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

/// What the network-free pass concluded — and, crucially, what it did NOT conclude.
///
/// The whole point of the type is that it has no "refused" arm. Nothing that can be
/// decided from a cache alone is allowed to refuse a ChatGPT identity, because the
/// only cached evidence for a refusal — absence from a stored entitlement list — is
/// exactly the evidence a stale cache cannot supply. The absence therefore comes
/// back as [`Unconfirmed`](Self::Unconfirmed), which has no way to become an error
/// except by going through [`confirm_identity`] and its fresh fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Everything a cache can establish, established. Proceed, saying these
    /// warnings (which are never fatal — see the module docs on models.dev).
    Known(Vec<String>),
    /// The model is not in the ChatGPT entitlement list we hold — which is a
    /// question, not an answer. Only the edge can settle it.
    Unconfirmed(Unconfirmed),
}

/// A ChatGPT identity whose entitlement could not be established from cache: the
/// slug is absent from a stored list, or there is no usable list at all.
///
/// It carries everything [`confirm_identity`] needs to go and ask, and *nothing*
/// that would let a caller turn it into a refusal on its own. That is deliberate: a
/// stale snapshot is authoritative about what WAS entitled, never about what is new,
/// and a `gpt-5.7` shipped this morning must not be refused by an hour-old cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    model: String,
    kind: ResolvedProviderKind,
    base_url: String,
}

impl Unconfirmed {
    /// The slug whose entitlement is in question.
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// A ChatGPT entitlement list, and how much it is worth.
///
/// The distinction is the entire fix: [`Fresh`](Self::Fresh) came off the endpoint
/// just now and is authoritative about ABSENCE as well as presence, so it may refuse.
/// [`Unavailable`](Self::Unavailable) means we could not get one (offline, 401,
/// timeout, rate-limited, a stale cache served instead) — so we could not confirm,
/// and something we could not confirm must never block a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entitlements {
    /// Fetched live (or revalidated with a 304): proof, in both directions.
    Fresh(Vec<String>),
    /// No authoritative answer, and why.
    Unavailable(String),
}

/// Validate a resolved identity against the catalogs already cached on disk.
///
/// Network-free and cheap — safe to call on every `/model` switch, not just at
/// startup — and, by construction, unable to refuse: see [`Identity`]. Hand the
/// result to [`confirm_identity`] to settle it.
pub fn validate_identity(m: &ResolvedModel, cfg: &AgentConfig) -> Identity {
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
) -> Identity {
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
) -> Identity {
    let reference = m.reference();
    let model = reference.model();

    // 1. The ChatGPT/Codex endpoint is AUTHORITATIVE about itself — but a CACHE of
    //    its answer is not, and only in one direction. An entitlement list grows:
    //    presence in a stale copy still means entitled (near enough), while absence
    //    from one means only that the model did not exist for this account when the
    //    copy was taken. So presence settles it here, for free; absence — and a cold
    //    or foreign-account cache, which is the same ignorance by another route — is
    //    handed on unresolved, for the edge to confirm against a fresh list.
    if m.is_codex_oauth() {
        let known = entitled.is_some_and(|rows| rows.iter().any(|r| r.slug == model));
        if known {
            return Identity::Known(Vec::new());
        }
        return Identity::Unconfirmed(Unconfirmed {
            model: model.to_string(),
            kind: m.kind(),
            base_url: m.base_url().to_string(),
        });
    }

    // 2. A `[providers.<name>]` entry SHADOWS the built-in: the endpoint is the
    //    user's, so models.dev's list for that name describes somebody else's
    //    server. `resolve_provider` is the sole trust gate — ask it, not the raw
    //    map, so a shadow spelled with any casing is caught.
    let name = reference.provider();
    let shadowed = resolve_provider_in(providers, name.as_str())
        .is_some_and(|p| p.kind == ResolvedProviderKind::Custom);
    if shadowed || m.kind() == ResolvedProviderKind::Custom {
        return Identity::Known(Vec::new());
    }

    // 3. models.dev knows this provider → it may only WARN. The catalog lags every
    //    new release by days; a model shipped this morning must still run. Refusing
    //    on its silence would make hrdr unusable on exactly the models people are
    //    most excited to try.
    let Some(catalog) = catalog else {
        return Identity::Known(Vec::new());
    };
    let Some(key) = name.catalog_key() else {
        // `local`, `chatgpt`, a custom name — models.dev covers none of them.
        return Identity::Known(Vec::new());
    };
    // The provider is absent from the cached catalog (a partial or stale index):
    // that is a fact about the catalog, not about the model. Silence.
    let Some((_, models)) = hrdr_llm::catalog::provider_models(catalog, key) else {
        return Identity::Known(Vec::new());
    };
    if models.is_empty() || models.iter().any(|(id, _)| id == model) {
        return Identity::Known(Vec::new());
    }
    Identity::Known(vec![format!(
        "⚠ models.dev doesn't list '{model}' on {name} — it may be new, or a typo"
    )])
}

/// Settle an [`Identity`] — the ONE place a ChatGPT model may be refused.
///
/// [`Known`](Identity::Known) passes straight through: **no network**, which is the
/// overwhelmingly common path (the model is in the cache, or the provider is not
/// ChatGPT at all). Only an [`Unconfirmed`] one pays for a round-trip, and it is
/// paid precisely when hrdr is otherwise about to tell someone their model does not
/// exist — the one moment the cost is obviously worth it.
pub async fn confirm_identity(v: Identity) -> Result<Vec<String>> {
    confirm_identity_with(v, fetch_entitlements).await
}

/// [`confirm_identity`] with the fetch injected, so the confirm/refuse/warn rules —
/// and the fact that a cached hit fetches NOTHING — are testable without a network
/// or an OAuth store.
pub async fn confirm_identity_with<F, Fut>(v: Identity, fetch: F) -> Result<Vec<String>>
where
    F: FnOnce(Unconfirmed) -> Fut,
    Fut: Future<Output = Entitlements>,
{
    match v {
        Identity::Known(warnings) => Ok(warnings),
        Identity::Unconfirmed(u) => {
            let model = u.model.clone();
            confirm(&model, fetch(u).await)
        }
    }
}

/// The refusal rule, pure: a FRESH entitlement list is proof in both directions; an
/// unavailable one is proof of nothing.
///
/// An empty "fresh" list is treated as unavailable rather than as "nothing is
/// entitled": an account with zero entitled models is not a thing, so an empty
/// answer means the catalog failed to say anything, and hrdr must not refuse on it.
fn confirm(model: &str, entitlements: Entitlements) -> Result<Vec<String>> {
    let why = match entitlements {
        Entitlements::Fresh(slugs) if !slugs.is_empty() => {
            if slugs.iter().any(|s| s == model) {
                return Ok(Vec::new());
            }
            let listed = sample(slugs.iter().cloned(), slugs.len());
            bail!(
                "model '{model}' is not entitled on this ChatGPT account — \
                 entitled: {listed} (run `/model` to pick one)"
            );
        }
        Entitlements::Fresh(_) => "the account catalog came back empty".to_string(),
        Entitlements::Unavailable(why) => why,
    };
    // We could not confirm — so we do not block. Say so and carry on: a user who
    // knows their model exists must never be stopped by hrdr's inability to check.
    Ok(vec![format!(
        "⚠ couldn't confirm '{model}' against your ChatGPT entitlements ({why}) — continuing"
    )])
}

/// Fetch the account's entitlements, live. Only a catalog the endpoint actually
/// answered for ([`CatalogSource::Fresh`], which includes a `304`-revalidated cache)
/// is authoritative; a stale cache or the built-in fallback is exactly the
/// not-good-enough evidence this whole path exists to avoid refusing on.
async fn fetch_entitlements(u: Unconfirmed) -> Entitlements {
    let access = match coordinated_oauth_access(u.kind, &u.base_url).await {
        Ok(a) => a,
        Err(e) => return Entitlements::Unavailable(format!("{e}")),
    };
    // `force`: the whole point is to get past the cache that could not answer.
    let result = chatgpt_model_catalog(&access, true).await;
    match result.source {
        CatalogSource::Fresh => {
            Entitlements::Fresh(result.models.into_iter().map(|m| m.slug).collect())
        }
        CatalogSource::Stale | CatalogSource::BuiltInFallback => Entitlements::Unavailable(
            result
                .warning
                .unwrap_or_else(|| "the account catalog was unavailable".to_string()),
        ),
    }
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

    /// A counting fetcher: the confirmation round-trip must be paid ONLY when hrdr is
    /// about to refuse someone, never on the ordinary path.
    fn counting(
        answer: Entitlements,
    ) -> (
        impl FnOnce(Unconfirmed) -> std::future::Ready<Entitlements>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let f = move |_u: Unconfirmed| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(answer)
        };
        (f, calls)
    }

    async fn settle(v: Identity, answer: Entitlements) -> (Result<Vec<String>>, usize) {
        let (fetch, calls) = counting(answer);
        let out = confirm_identity_with(v, fetch).await;
        (out, calls.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// PRESENCE in a cached entitlement list settles it — and settles it for FREE.
    /// The fetch is not attempted, because there is nothing left to ask.
    #[tokio::test]
    async fn a_model_present_in_the_cached_entitlements_is_accepted_without_a_fetch() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-5.5-codex", &cfg);
        assert!(
            m.is_codex_oauth(),
            "the real Codex endpoint, or no authority"
        );
        // A STALE list — the cache carries no freshness signal here, and does not need
        // to: an entitlement list only grows, so a model that was entitled an hour ago
        // is entitled now.
        let rows = entitlements(&["gpt-5.5", "gpt-5.5-codex", "gpt-5.3-codex-spark"]);
        let v = validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog()));
        assert_eq!(v, Identity::Known(Vec::new()));

        let (out, calls) = settle(v, Entitlements::Fresh(vec!["unused".to_string()])).await;
        assert_eq!(out.unwrap(), Vec::<String>::new());
        assert_eq!(calls, 0, "a cached hit costs no round-trip");
    }

    /// THE BUG THIS EXISTS TO KILL: a STALE cache that omits the model must not refuse
    /// when the refresh fails. An entitlement list grows — OpenAI ships `gpt-5.7`, the
    /// account gets it, the hour-old cache has never heard of it — so absence from a
    /// snapshot is not evidence, and hrdr must not tell a user their real model is not
    /// real just because it could not check.
    #[tokio::test]
    async fn a_stale_absence_warns_when_it_cannot_be_confirmed_and_never_refuses() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-5.7", &cfg); // shipped this morning
        let rows = entitlements(&["gpt-5.5", "gpt-5.5-codex"]); // yesterday's list
        let v = validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog()));
        assert!(
            matches!(&v, Identity::Unconfirmed(u) if u.model() == "gpt-5.7"),
            "an absence from a cache is a QUESTION, and the type says so: {v:?}",
        );

        // The refresh fails (offline, 401, timeout, rate-limited) → warn, carry on.
        let (out, calls) = settle(
            v.clone(),
            Entitlements::Unavailable("endpoint unreachable".to_string()),
        )
        .await;
        assert_eq!(
            out.unwrap(),
            [
                "⚠ couldn't confirm 'gpt-5.7' against your ChatGPT entitlements (endpoint unreachable) — continuing"
            ],
        );
        assert_eq!(calls, 1, "and it did try");

        // An "empty" fresh catalog is not "nothing is entitled" — an account with zero
        // entitled models is not a thing. Treated as unconfirmable, never as proof.
        let (out, _) = settle(v.clone(), Entitlements::Fresh(Vec::new())).await;
        assert!(out.unwrap()[0].contains("couldn't confirm 'gpt-5.7'"));

        // A FRESH list that DOES have it: the cache was simply behind. No warning.
        let (out, _) = settle(
            v,
            Entitlements::Fresh(vec!["gpt-5.5".to_string(), "gpt-5.7".to_string()]),
        )
        .await;
        assert_eq!(out.unwrap(), Vec::<String>::new());
    }

    /// THE REFUSAL, and the only one: a model absent from a list fetched JUST NOW. The
    /// account catalog is the account's own answer to "what may I run", and a fresh
    /// one is authoritative about absence as well as presence — so now, and only now,
    /// the statement "this model is not entitled" is true.
    #[tokio::test]
    async fn a_confirmed_absence_from_a_fresh_entitlement_list_is_refused() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-4o", &cfg);
        let rows = entitlements(&["gpt-5.5", "gpt-5.5-codex"]);
        let v = validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog()));

        let (out, calls) = settle(
            v,
            Entitlements::Fresh(vec![
                "gpt-5.5".to_string(),
                "gpt-5.5-codex".to_string(),
                "gpt-5.3-codex-spark".to_string(),
            ]),
        )
        .await;
        assert_eq!(calls, 1, "we paid for proof before refusing");
        let err = out
            .expect_err("a CONFIRMED absence is a refusal")
            .to_string();
        assert!(err.contains("'gpt-4o' is not entitled"), "{err}");
        assert!(err.contains("gpt-5.5"), "it names what IS entitled: {err}");
        assert!(err.contains("/model"), "and how to fix it: {err}");
    }

    /// A COLD cache (or one written for another account — `cached_entitlements`
    /// returns `None` for both) is ignorance, and it routes through the very same
    /// "cannot confirm" path: a failed refresh warns, and never refuses.
    #[tokio::test]
    async fn a_cold_account_catalog_routes_through_confirmation_and_never_refuses() {
        let cfg = cfg();
        let m = resolved("chatgpt://gpt-4o", &cfg);
        for entitled in [None, Some(&[][..])] {
            let v = validate_identity_with(&cfg.providers, &m, entitled, Some(&catalog()));
            assert!(matches!(v, Identity::Unconfirmed(_)), "{v:?}");
            let (out, calls) =
                settle(v, Entitlements::Unavailable("no credentials".to_string())).await;
            assert!(
                out.unwrap()[0].contains("couldn't confirm 'gpt-4o'"),
                "an unconfirmable absence warns; it never blocks a launch",
            );
            assert_eq!(calls, 1);
        }
        // And models.dev never gets a say about ChatGPT: it lists the differently
        // windowed *API* models under `openai`, which are not this account's
        // entitlements. `chatgpt` has no catalog key at all — rule 1 is the whole rule.
        assert!(m.reference().provider().catalog_key().is_none());
    }

    /// A `[providers.chatgpt]` shadow is `Custom`, never OAuth — so it is never
    /// measured against somebody's account entitlements, and never even asks. It is
    /// the user's own server.
    #[tokio::test]
    async fn a_shadowed_chatgpt_provider_is_never_refused_by_the_account_catalog() {
        let cfg = cfg_with("chatgpt", "http://localhost:9099/v1");
        let m = resolved("chatgpt://gpt-4o", &cfg);
        assert!(!m.is_codex_oauth());
        let rows = entitlements(&["gpt-5.5"]);
        let v = validate_identity_with(&cfg.providers, &m, Some(&rows), Some(&catalog()));
        assert_eq!(v, Identity::Known(Vec::new()));
        let (out, calls) = settle(v, Entitlements::Fresh(vec!["gpt-5.5".to_string()])).await;
        assert_eq!(out.unwrap(), Vec::<String>::new());
        assert_eq!(calls, 0);
    }

    /// models.dev only ever WARNS. It is a third-party index and it lags: a model
    /// released this morning is not in it, and must still run. Refusing on its
    /// silence would break hrdr on exactly the models people most want to try.
    #[test]
    fn an_unknown_model_on_a_models_dev_provider_warns_but_never_errs() {
        let cfg = cfg();
        let m = resolved("claude://claude-sonet-4-5", &cfg); // typo'd
        assert_eq!(
            validate_identity_with(&cfg.providers, &m, None, Some(&catalog())),
            Identity::Known(vec![
                "⚠ models.dev doesn't list 'claude-sonet-4-5' on claude — it may be new, or a typo"
                    .to_string()
            ]),
            "models.dev NEVER refuses — it cannot even express a refusal from here",
        );
        // A model it does list is silent.
        let known = resolved("claude://claude-opus-4-8", &cfg);
        assert_eq!(
            validate_identity_with(&cfg.providers, &known, None, Some(&catalog())),
            Identity::Known(Vec::new())
        );
        // The warning is keyed through the CATALOG name (`claude` → `anthropic`), so
        // an alias spelling folds onto the same answer.
        let alias = resolved("anthropic://claude-opus-4-8", &cfg);
        assert_eq!(
            validate_identity_with(&cfg.providers, &alias, None, Some(&catalog())),
            Identity::Known(Vec::new())
        );
    }

    /// We know nothing → we say nothing. A local server, a custom provider, and a
    /// provider the cached catalog has never heard of are all SILENT: a brand-new
    /// model on a provider models.dev does not cover must not be nagged about.
    #[test]
    fn what_we_cannot_know_we_do_not_mention() {
        let catalog = catalog();
        let silent = Identity::Known(Vec::new());
        // `local`: no catalog key. A local server serves whatever it was started with.
        let cfg = cfg();
        for spec in ["local://qwen3-coder-next", "local://default"] {
            let m = resolved(spec, &cfg);
            assert_eq!(
                validate_identity_with(&cfg.providers, &m, None, Some(&catalog)),
                silent,
                "{spec}"
            );
        }
        // A custom `[providers.*]`: models.dev describes somebody else's server.
        let custom = cfg_with("mygateway", "https://gw.internal/v1");
        let m = resolved("mygateway://whatever-v9", &custom);
        assert_eq!(
            validate_identity_with(&custom.providers, &m, None, Some(&catalog)),
            silent
        );
        // A built-in models.dev DOES key (`openrouter`) but which is absent from this
        // cached index: that is a fact about the catalog, not the model. Silence.
        let m = resolved("openrouter://deepseek/deepseek-v4", &cfg);
        assert!(m.reference().provider().catalog_key().is_some());
        assert_eq!(
            validate_identity_with(&cfg.providers, &m, None, Some(&catalog)),
            silent,
            "a provider the catalog does not carry is not evidence of anything",
        );
        // No cached catalog at all → nothing to say about anyone.
        let m = resolved("claude://claude-sonet-4-5", &cfg);
        assert_eq!(
            validate_identity_with(&cfg.providers, &m, None, None),
            silent
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
        assert_eq!(validate_identity(&m, &cfg), Identity::Known(Vec::new()));
        assert_eq!(m.base_url(), "http://localhost:8080/v1");
        // …and the Codex endpoint is the authoritative one, so the double gate that
        // decides it is worth pinning here too.
        let codex = resolved("chatgpt://gpt-5.5", &cfg);
        assert_eq!(codex.base_url(), CHATGPT_CODEX_BASE_URL);
        assert!(codex.is_codex_oauth());
    }
}
