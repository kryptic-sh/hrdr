//! A model's identity: the provider it is served by AND the model id, as ONE
//! value.
//!
//! hrdr used to carry `provider: Option<String>` and `model: String` side by
//! side — in the config, on the agent, in the session state — which makes a
//! mismatched pair (an OpenRouter model id against the Anthropic endpoint)
//! representable and unchecked. [`ModelRef`] couples them: it is always complete,
//! it round-trips through a single `provider://model` string, and a provider's
//! three parallel namespaces (app name, models.dev catalog key, auth.toml key)
//! are *derived* from [`ProviderName`] rather than re-encoded at every call site.
//!
//! [`ModelSpec`] is the *user's* input to a `/model` switch: either a complete
//! `provider://model` or a bare model id meaning "same provider, new model".

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The separator between the provider and the model id on the wire.
///
/// Chosen because no model id contains it, which makes the parse CONTEXT-FREE:
/// split once on `://` and everything after it is the model id verbatim, slashes
/// (`deepseek/deepseek-chat`) and colons (`llama3:8b`) included.
const SEP: &str = "://";

/// Why a `provider://model` string (or a [`ModelRef`]'s parts) is not a valid
/// model reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRefError {
    /// No `://` in the input — it names a model but not a provider.
    MissingSeparator,
    /// The provider side is empty or whitespace.
    EmptyProvider,
    /// The model side is empty or whitespace.
    EmptyModel,
}

impl fmt::Display for ModelRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::MissingSeparator => "expected `provider://model`",
            Self::EmptyProvider => "the provider is empty in `provider://model`",
            Self::EmptyModel => "the model is empty in `provider://model`",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ModelRefError {}

/// A provider's canonical app-facing name — the single owner of the alias
/// folding (`opencode` → `zen`, `anthropic` → `claude`, `codex` → `chatgpt`, …)
/// and of the two *other* namespaces the same provider lives in:
/// [`catalog_key`](Self::catalog_key) (models.dev) and
/// [`auth_key`](Self::auth_key) (`auth.toml`).
///
/// NOT a closed enum: a name matching no built-in is still valid — it may be a
/// user's `[providers.<name>]`. Names are canonicalized (trimmed, lowercased) on
/// construction, so `" ZEN "`, `"opencode"` and `"zen"` are one value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub struct ProviderName(String);

impl ProviderName {
    /// Canonicalize `name`: trim, lowercase, fold the built-in aliases. Any
    /// other name is kept as-is (a custom `[providers.<name>]`).
    pub fn new(name: &str) -> Self {
        let folded = name.trim().to_ascii_lowercase();
        let canonical = match folded.as_str() {
            "zen" | "opencode" | "opencode-zen" => "zen",
            "go" | "opencode-go" => "go",
            "claude" | "anthropic" => "claude",
            "chatgpt" | "codex" | "openai-oauth" => "chatgpt",
            "local" | "infr" => "local",
            "openai" => "openai",
            "openrouter" => "openrouter",
            _ => return Self(folded),
        };
        Self(canonical.to_string())
    }

    /// The canonical app-facing name (`zen`, `claude`, `mycustom`, …).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is one of hrdr's built-in presets (see `BUILTIN_PROVIDERS`)
    /// rather than a user-defined `[providers.<name>]`.
    pub fn is_builtin(&self) -> bool {
        matches!(
            self.0.as_str(),
            "zen" | "go" | "openai" | "openrouter" | "claude" | "chatgpt" | "local"
        )
    }

    /// The models.dev catalog key for a built-in preset. hrdr's names and
    /// models.dev's are NOT the same namespace: `zen` is models.dev's `opencode`,
    /// `go` is `opencode-go`, `claude` is `anthropic`. `None` for a provider with
    /// no catalog presence — `local` (a server you run), `chatgpt` (the Codex
    /// subscription models are resolved from the account catalog, and models.dev
    /// lists the differently-windowed API models of the same ids) — and for a
    /// custom provider (see [`catalog_provider_key`] for the custom-name fallback).
    pub fn catalog_key(&self) -> Option<&'static str> {
        Some(match self.0.as_str() {
            "zen" => "opencode",
            "go" => "opencode-go",
            "openai" => "openai",
            "openrouter" => "openrouter",
            "claude" => "anthropic",
            _ => return None,
        })
    }

    /// The `auth.toml` key. OpenCode's endpoints — `zen` and `go` — authenticate
    /// against the same OpenCode account (the same `OPENCODE_API_KEY`), so they
    /// share one stored credential (`opencode`). Every other provider keys on its
    /// own canonical name.
    pub fn auth_key(&self) -> &str {
        match self.0.as_str() {
            "zen" | "go" => "opencode",
            other => other,
        }
    }
}

/// The models.dev key to query for `provider`, or `None` to let the catalog scan
/// every provider for the model id.
///
/// A built-in maps through [`ProviderName::catalog_key`]; a *custom* provider
/// falls back to its own name, which may well be a real models.dev key (a user
/// pointing `[providers.cortecs]` at cortecs). Mirrors the `/model` picker's rule
/// (`models::configured_providers`), and is what every raw-name catalog call site
/// must route through — passing hrdr's `zen`/`go`/`claude` straight to the
/// catalog never matches, and silently falls through to its cross-provider scan.
pub fn catalog_provider_key(provider: Option<&str>) -> Option<String> {
    let name = ProviderName::new(provider?);
    if name.is_builtin() {
        name.catalog_key().map(str::to_string)
    } else if name.as_str().is_empty() {
        None
    } else {
        Some(name.0)
    }
}

impl From<&str> for ProviderName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ProviderName {
    fn from(s: String) -> Self {
        Self::new(&s)
    }
}

impl From<ProviderName> for String {
    fn from(p: ProviderName) -> Self {
        p.0
    }
}

impl FromStr for ProviderName {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl fmt::Display for ProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A complete model identity: WHICH model, at WHICH provider. Never half a pair.
///
/// Displays and parses as `provider://model` — `chatgpt://gpt-5.5`,
/// `openrouter://deepseek/deepseek-chat`, `local://llama3:8b` — and serializes as
/// that single string, so it is one field on disk and one token on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelRef {
    provider: ProviderName,
    model: String,
}

impl ModelRef {
    /// Pair `provider` with `model`. The model id is kept verbatim (only trimmed
    /// of surrounding whitespace); an empty/whitespace model is rejected, since a
    /// `ModelRef` is by definition complete.
    pub fn new(provider: impl Into<ProviderName>, model: &str) -> Result<Self, ModelRefError> {
        let provider = provider.into();
        if provider.as_str().is_empty() {
            return Err(ModelRefError::EmptyProvider);
        }
        let model = model.trim();
        if model.is_empty() {
            return Err(ModelRefError::EmptyModel);
        }
        Ok(Self {
            provider,
            model: model.to_string(),
        })
    }

    /// The provider half.
    pub fn provider(&self) -> &ProviderName {
        &self.provider
    }

    /// The model id, exactly as the endpoint wants it.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Take the pair apart — for the call sites that still speak in two values.
    pub fn into_parts(self) -> (ProviderName, String) {
        (self.provider, self.model)
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;

    /// Split ONCE on `://`: everything after it is the model id, verbatim. No
    /// model id contains `://`, so `openrouter://deepseek/deepseek-chat` and
    /// `local://llama3:8b` keep their slashes and colons.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (provider, model) = s
            .trim()
            .split_once(SEP)
            .ok_or(ModelRefError::MissingSeparator)?;
        if provider.trim().is_empty() {
            return Err(ModelRefError::EmptyProvider);
        }
        Self::new(ProviderName::new(provider), model)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{SEP}{}", self.provider, self.model)
    }
}

impl TryFrom<String> for ModelRef {
    type Error = ModelRefError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModelRef> for String {
    fn from(r: ModelRef) -> Self {
        r.to_string()
    }
}

/// What the user asked to switch to: a whole new identity, or just a new model on
/// the provider they are already on.
///
/// The distinction is purely syntactic — the presence of `://` — which is why the
/// parse never has to guess whether `moonshotai/kimi-k2` names a provider.
///
/// This is the **input** type of every model-naming surface: `--model`,
/// `$HRDR_MODEL`, `--subagent-model`, `model = …` in config.toml, an agent
/// profile's `model:`, and the `task` tool's `model` argument. It serializes as
/// the one string it was written as, so a config file carries exactly what the
/// user typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ModelSpec {
    /// The input carried a `://`: switch provider AND model.
    Full(ModelRef),
    /// No `://`: same provider, new model id (kept verbatim).
    ModelOnly(String),
}

impl ModelSpec {
    /// Resolve against the identity currently in use: a [`Full`](Self::Full) spec
    /// replaces it outright; a [`ModelOnly`](Self::ModelOnly) one keeps `base`'s
    /// provider. Total — never needs a fallback.
    pub fn apply(&self, base: &ModelRef) -> ModelRef {
        match self {
            Self::Full(r) => r.clone(),
            Self::ModelOnly(m) => ModelRef {
                provider: base.provider.clone(),
                model: m.clone(),
            },
        }
    }
}

impl FromStr for ModelSpec {
    type Err = ModelRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.contains(SEP) {
            return Ok(Self::Full(s.parse()?));
        }
        if s.is_empty() {
            return Err(ModelRefError::EmptyModel);
        }
        Ok(Self::ModelOnly(s.to_string()))
    }
}

impl fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(r) => write!(f, "{r}"),
            Self::ModelOnly(m) => f.write_str(m),
        }
    }
}

impl TryFrom<String> for ModelSpec {
    type Error = ModelRefError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModelSpec> for String {
    fn from(s: ModelSpec) -> Self {
        s.to_string()
    }
}

impl From<ModelRef> for ModelSpec {
    fn from(r: ModelRef) -> Self {
        Self::Full(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every alias folds onto one canonical name — the whole point of the type.
    #[test]
    fn aliases_fold_onto_the_canonical_name() {
        let n = |s: &str| ProviderName::new(s).as_str().to_string();
        assert_eq!(n("zen"), "zen");
        assert_eq!(n("opencode"), "zen");
        assert_eq!(n("opencode-zen"), "zen");
        assert_eq!(n("go"), "go");
        assert_eq!(n("opencode-go"), "go");
        assert_eq!(n("claude"), "claude");
        assert_eq!(n("anthropic"), "claude");
        assert_eq!(n("chatgpt"), "chatgpt");
        assert_eq!(n("codex"), "chatgpt");
        assert_eq!(n("openai-oauth"), "chatgpt");
        assert_eq!(n("local"), "local");
        assert_eq!(n("infr"), "local");
        assert_eq!(n("openai"), "openai");
        assert_eq!(n("openrouter"), "openrouter");
        // Case- and whitespace-insensitive, like `builtin_provider`.
        assert_eq!(n("  OpenCode-GO \n"), "go");
        assert_eq!(ProviderName::new("ZEN"), ProviderName::new("opencode"));
        // An unknown name is VALID — it may be a user's `[providers.<name>]`.
        assert_eq!(n(" MyCustom "), "mycustom");
        assert!(!ProviderName::new("mycustom").is_builtin());
        assert!(ProviderName::new("anthropic").is_builtin());
    }

    /// The three namespaces are derived, not parallel: app name, models.dev key,
    /// auth.toml key.
    #[test]
    fn catalog_and_auth_keys_are_derived_from_the_name() {
        let p = ProviderName::new;
        assert_eq!(p("zen").catalog_key(), Some("opencode"));
        assert_eq!(p("opencode-zen").catalog_key(), Some("opencode"));
        assert_eq!(p("go").catalog_key(), Some("opencode-go"));
        assert_eq!(p("claude").catalog_key(), Some("anthropic"));
        assert_eq!(p("Anthropic").catalog_key(), Some("anthropic"));
        assert_eq!(p("openai").catalog_key(), Some("openai"));
        assert_eq!(p("openrouter").catalog_key(), Some("openrouter"));
        // No catalog presence.
        assert_eq!(p("local").catalog_key(), None);
        assert_eq!(p("chatgpt").catalog_key(), None);
        assert_eq!(p("codex").catalog_key(), None);
        assert_eq!(p("mycustom").catalog_key(), None);

        // The OpenCode endpoints share one credential; everyone else keys on self.
        assert_eq!(p("zen").auth_key(), "opencode");
        assert_eq!(p("go").auth_key(), "opencode");
        assert_eq!(p("opencode-go").auth_key(), "opencode");
        assert_eq!(p("claude").auth_key(), "claude");
        assert_eq!(p("anthropic").auth_key(), "claude");
        assert_eq!(p("openai").auth_key(), "openai");
        assert_eq!(p("mycustom").auth_key(), "mycustom");
    }

    /// What a catalog call site must pass: the built-in mapping, else the custom
    /// name itself (which may be a real models.dev key), else nothing.
    #[test]
    fn catalog_provider_key_maps_builtins_and_keeps_custom_names() {
        assert_eq!(
            catalog_provider_key(Some("zen")).as_deref(),
            Some("opencode")
        );
        assert_eq!(
            catalog_provider_key(Some("go")).as_deref(),
            Some("opencode-go")
        );
        assert_eq!(
            catalog_provider_key(Some("claude")).as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            catalog_provider_key(Some("openai")).as_deref(),
            Some("openai")
        );
        // A custom provider may be spelled exactly as its models.dev key.
        assert_eq!(
            catalog_provider_key(Some("cortecs")).as_deref(),
            Some("cortecs")
        );
        // No catalog entry exists for these: scan instead of matching nothing.
        assert_eq!(catalog_provider_key(Some("local")), None);
        assert_eq!(catalog_provider_key(Some("chatgpt")), None);
        assert_eq!(catalog_provider_key(None), None);
        assert_eq!(catalog_provider_key(Some("   ")), None);
    }

    /// `provider://model` round-trips, and the model id survives verbatim —
    /// slashes and colons included.
    #[test]
    fn model_refs_round_trip_through_the_wire_format() {
        let cases = [
            ("chatgpt://gpt-5.5", "chatgpt", "gpt-5.5"),
            (
                "openrouter://deepseek/deepseek-chat",
                "openrouter",
                "deepseek/deepseek-chat",
            ),
            ("local://llama3:8b", "local", "llama3:8b"),
            ("zen://claude-sonnet-4-5", "zen", "claude-sonnet-4-5"),
            (
                "mycustom://some/weird:id-v2",
                "mycustom",
                "some/weird:id-v2",
            ),
        ];
        for (s, provider, model) in cases {
            let r: ModelRef = s.parse().unwrap();
            assert_eq!(r.provider().as_str(), provider, "{s}");
            assert_eq!(r.model(), model, "{s}");
            assert_eq!(r.to_string(), s, "display round-trips: {s}");
            assert_eq!(r.to_string().parse::<ModelRef>().unwrap(), r);
        }
        // The provider half folds on parse, so the render is canonical.
        assert_eq!(
            "  OPENCODE://kimi-k2  "
                .parse::<ModelRef>()
                .unwrap()
                .to_string(),
            "zen://kimi-k2"
        );
        // A typo'd provider still PARSES — resolving it is a later concern.
        let typo: ModelRef = "opencodee://kimi-k2".parse().unwrap();
        assert_eq!(typo.provider().as_str(), "opencodee");
        assert!(!typo.provider().is_builtin());
    }

    /// Both halves are required; whitespace is not a value.
    #[test]
    fn empty_halves_are_rejected() {
        assert_eq!(
            "://gpt-5.5".parse::<ModelRef>(),
            Err(ModelRefError::EmptyProvider)
        );
        assert_eq!(
            "   ://gpt-5.5".parse::<ModelRef>(),
            Err(ModelRefError::EmptyProvider)
        );
        assert_eq!("zen://".parse::<ModelRef>(), Err(ModelRefError::EmptyModel));
        assert_eq!(
            "zen://   ".parse::<ModelRef>(),
            Err(ModelRefError::EmptyModel)
        );
        assert_eq!(
            "gpt-5.5".parse::<ModelRef>(),
            Err(ModelRefError::MissingSeparator)
        );
        assert_eq!(
            ModelRef::new(ProviderName::new(" "), "m"),
            Err(ModelRefError::EmptyProvider)
        );
    }

    /// A `ModelRef` is one string on disk, not a nested struct.
    #[test]
    fn serde_uses_the_single_string_form() {
        let r: ModelRef = "openrouter://deepseek/deepseek-chat".parse().unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"openrouter://deepseek/deepseek-chat\"");
        assert_eq!(serde_json::from_str::<ModelRef>(&json).unwrap(), r);
        // Aliases fold through deserialization too.
        assert_eq!(
            serde_json::from_str::<ModelRef>("\"anthropic://claude-opus-4-8\"")
                .unwrap()
                .provider()
                .as_str(),
            "claude"
        );
        // An invalid ref fails to deserialize rather than landing half-formed.
        assert!(serde_json::from_str::<ModelRef>("\"zen://\"").is_err());
        assert!(serde_json::from_str::<ModelRef>("\"gpt-5.5\"").is_err());
        // ProviderName is a plain string too, canonicalized on the way in.
        assert_eq!(
            serde_json::to_string(&ProviderName::new("opencode")).unwrap(),
            "\"zen\""
        );
        assert_eq!(
            serde_json::from_str::<ProviderName>("\"Codex\"").unwrap(),
            ProviderName::new("chatgpt")
        );
    }

    /// `://` decides: with it, a whole identity; without it, a bare model id that
    /// stays intact — slashes and colons and all.
    #[test]
    fn specs_split_on_the_separator_only() {
        assert_eq!(
            "openrouter://deepseek/deepseek-chat"
                .parse::<ModelSpec>()
                .unwrap(),
            ModelSpec::Full("openrouter://deepseek/deepseek-chat".parse().unwrap())
        );
        // A bare id with a slash is a MODEL, not a provider.
        assert_eq!(
            "moonshotai/kimi-k2".parse::<ModelSpec>().unwrap(),
            ModelSpec::ModelOnly("moonshotai/kimi-k2".to_string())
        );
        assert_eq!(
            "llama3:8b".parse::<ModelSpec>().unwrap(),
            ModelSpec::ModelOnly("llama3:8b".to_string())
        );
        assert_eq!("".parse::<ModelSpec>(), Err(ModelRefError::EmptyModel));
        assert_eq!(
            "zen://".parse::<ModelSpec>(),
            Err(ModelRefError::EmptyModel)
        );
        // Display round-trips both shapes.
        for s in ["openrouter://deepseek/deepseek-chat", "moonshotai/kimi-k2"] {
            assert_eq!(s.parse::<ModelSpec>().unwrap().to_string(), s);
        }
    }

    /// A spec is one string on disk too — exactly what the user typed. It is the
    /// type of every model-naming key now (`model = …` in config.toml, an agent
    /// profile's `model:`, `subagent_model`), so what a config carries is what a
    /// `/model` switch would have accepted.
    #[test]
    fn a_spec_serializes_as_the_one_string_it_was_written_as() {
        for s in [
            "openrouter://deepseek/deepseek-chat",
            "moonshotai/kimi-k2",
            "llama3:8b",
        ] {
            let spec: ModelSpec = s.parse().unwrap();
            let json = serde_json::to_string(&spec).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            assert_eq!(serde_json::from_str::<ModelSpec>(&json).unwrap(), spec);
        }
        // The provider half still folds through deserialization.
        assert_eq!(
            serde_json::from_str::<ModelSpec>("\"anthropic://claude-opus-4-8\"").unwrap(),
            ModelSpec::Full("claude://claude-opus-4-8".parse().unwrap())
        );
        // A half-written identity is refused rather than landing as a model id.
        assert!(serde_json::from_str::<ModelSpec>("\"zen://\"").is_err());
        assert!(serde_json::from_str::<ModelSpec>("\"\"").is_err());
    }

    /// `apply` is total: a full spec replaces the identity, a bare model keeps the
    /// provider in use.
    #[test]
    fn apply_keeps_the_base_provider_for_a_bare_model() {
        let base: ModelRef = "zen://kimi-k2".parse().unwrap();
        assert_eq!(
            "grok-code".parse::<ModelSpec>().unwrap().apply(&base),
            "zen://grok-code".parse::<ModelRef>().unwrap()
        );
        assert_eq!(
            "local://llama3:8b"
                .parse::<ModelSpec>()
                .unwrap()
                .apply(&base),
            "local://llama3:8b".parse::<ModelRef>().unwrap()
        );
        // The base is untouched by a bare-model apply.
        assert_eq!(base.to_string(), "zen://kimi-k2");
    }

    /// The catalog-key bug this type exists to kill: hrdr's `zen` is models.dev's
    /// `opencode`. Handing the RAW name to the catalog never matches, so it falls
    /// through to its "smallest window any provider reports" scan and returns a
    /// plausible-but-wrong number.
    #[test]
    fn zen_resolves_through_the_opencode_catalog_key_not_the_min_scan() {
        let catalog = serde_json::json!({
            "opencode": { "models": {
                "kimi-k2": { "limit": { "context": 256_000 } },
            }},
            // A decoy serving the same model id with a much smaller window: what
            // the min-scan fallback would have returned.
            "decoy": { "models": {
                "kimi-k2": { "limit": { "context": 8_000 } },
            }},
        });
        let window = |p: Option<&str>| hrdr_llm::catalog::lookup(&catalog, p, "kimi-k2");

        // The bug: the raw app name is not a catalog key, so the scan wins.
        assert_eq!(window(Some("zen")), Some(8_000), "the old, wrong answer");
        // The fix: route through the catalog key.
        assert_eq!(
            window(catalog_provider_key(Some("zen")).as_deref()),
            Some(256_000)
        );
        // Same for the other renamed built-ins.
        let go = serde_json::json!({
            "opencode-go": { "models": { "m": { "limit": { "context": 1_000_000 } } } },
            "decoy": { "models": { "m": { "limit": { "context": 4_000 } } } },
        });
        assert_eq!(
            hrdr_llm::catalog::lookup(&go, catalog_provider_key(Some("go")).as_deref(), "m"),
            Some(1_000_000)
        );
        let claude = serde_json::json!({
            "anthropic": { "models": { "m": { "limit": { "context": 200_000 } } } },
            "decoy": { "models": { "m": { "limit": { "context": 4_000 } } } },
        });
        assert_eq!(
            hrdr_llm::catalog::lookup(
                &claude,
                catalog_provider_key(Some("claude")).as_deref(),
                "m"
            ),
            Some(200_000)
        );
    }
}
