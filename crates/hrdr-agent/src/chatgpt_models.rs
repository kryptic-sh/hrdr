use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{OAuthAccess, valid_access_token_result, write_atomic};

pub const CODEX_CATALOG_COMPAT_VERSION: &str = "0.124.0";
const CACHE_SCHEMA: u32 = 1;
const FRESH_TTL_MS: u64 = 5 * 60 * 1000;
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    Fresh,
    Stale,
    BuiltInFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptModel {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub minimal_client_version: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub required_features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptCatalogResult {
    pub models: Vec<ChatGptModel>,
    pub source: CatalogSource,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    schema: u32,
    compatibility_version: String,
    account_digest: String,
    fetched_ms: u64,
    #[serde(default)]
    etag: Option<String>,
    models: Vec<ChatGptModel>,
}

#[derive(Deserialize)]
struct CatalogResponse {
    models: Vec<ChatGptModel>,
}

pub async fn load_chatgpt_catalog(force_refresh: bool) -> ChatGptCatalogResult {
    let token = match valid_access_token_result("chatgpt").await {
        Ok(token) => token,
        Err(e) => return fallback(format!("could not authorize model discovery: {e}")),
    };
    let digest = token.account_id.as_deref().map(account_digest);
    let cached = digest.as_deref().and_then(load_cache);
    if !force_refresh
        && let Some(cache) = &cached
        && now_ms().saturating_sub(cache.fetched_ms) < FRESH_TTL_MS
        && cache.compatibility_version == CODEX_CATALOG_COMPAT_VERSION
    {
        return result(
            cache.models.clone(),
            CatalogSource::Fresh,
            token.persistence_warning,
        );
    }
    if std::env::var_os("HRDR_DISABLE_MODELS_FETCH").is_some() {
        return cached.map_or_else(
            || fallback("ChatGPT model discovery is disabled".to_string()),
            |cache| {
                result(
                    cache.models,
                    CatalogSource::Stale,
                    append_warning(
                        token.persistence_warning.clone(),
                        "ChatGPT model discovery is disabled; using cached models".to_string(),
                    ),
                )
            },
        );
    }
    let etag = cached.as_ref().and_then(|cache| {
        (cache.compatibility_version == CODEX_CATALOG_COMPAT_VERSION)
            .then_some(cache.etag.as_deref())
            .flatten()
    });
    match fetch_catalog(&token, etag).await {
        Ok(FetchResult::Models(models, etag)) => {
            let mut warning = token.persistence_warning;
            if let Some(digest) = digest {
                let cache = CacheEnvelope {
                    schema: CACHE_SCHEMA,
                    compatibility_version: CODEX_CATALOG_COMPAT_VERSION.to_string(),
                    account_digest: digest,
                    fetched_ms: now_ms(),
                    etag,
                    models: models.clone(),
                };
                if let Err(e) = save_cache(&cache) {
                    warning =
                        append_warning(warning, format!("could not cache ChatGPT models: {e}"));
                }
            }
            result(models, CatalogSource::Fresh, warning)
        }
        Ok(FetchResult::NotModified) if cached.is_some() => {
            let mut cache = cached.expect("checked above");
            cache.fetched_ms = now_ms();
            cache.compatibility_version = CODEX_CATALOG_COMPAT_VERSION.to_string();
            let warning = save_cache(&cache)
                .err()
                .map(|e| format!("could not refresh ChatGPT model cache age: {e}"));
            let warning = warning.map_or(token.persistence_warning.clone(), |warning| {
                append_warning(token.persistence_warning.clone(), warning)
            });
            result(cache.models, CatalogSource::Fresh, warning)
        }
        Ok(FetchResult::NotModified) => {
            fallback("model service returned an unusable empty response".to_string())
        }
        Err(e) => cached.map_or_else(
            || fallback(format!("could not refresh ChatGPT models: {e}")),
            |cache| {
                result(
                    cache.models,
                    CatalogSource::Stale,
                    append_warning(
                        token.persistence_warning.clone(),
                        format!("could not refresh ChatGPT models; using cache: {e}"),
                    ),
                )
            },
        ),
    }
}

enum FetchResult {
    Models(Vec<ChatGptModel>, Option<String>),
    NotModified,
}

async fn fetch_catalog(token: &OAuthAccess, etag: Option<&str>) -> anyhow::Result<FetchResult> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut request = client
        .get(format!("{CODEX_BASE_URL}/models"))
        .query(&[("client_version", CODEX_CATALOG_COMPAT_VERSION)])
        .bearer_auth(&token.access)
        .header("originator", "hrdr");
    if let Some(account_id) = &token.account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchResult::NotModified);
    }
    let status = response.status();
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !status.is_success() {
        anyhow::bail!("model service returned {status}");
    }
    let models = sanitize_catalog(response.json::<CatalogResponse>().await?);
    if models.is_empty() {
        anyhow::bail!("model service returned no list-visible models");
    }
    Ok(FetchResult::Models(models, etag))
}

fn sanitize_catalog(response: CatalogResponse) -> Vec<ChatGptModel> {
    let mut models = response.models;
    models.retain(|model| {
        !model.slug.trim().is_empty()
            && model.visibility.as_deref() == Some("list")
            && model.required_features.iter().all(|feature| {
                matches!(
                    feature.as_str(),
                    "responses" | "streaming" | "tools" | "reasoning"
                )
            })
    });
    models.sort_by(|a, b| {
        a.priority
            .unwrap_or(i64::MAX)
            .cmp(&b.priority.unwrap_or(i64::MAX))
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    models
}

fn cache_path() -> Option<PathBuf> {
    Some(
        hjkl_xdg::cache_dir("hrdr")
            .ok()?
            .join("chatgpt_models.json"),
    )
}

fn load_cache(expected_digest: &str) -> Option<CacheEnvelope> {
    let cache: CacheEnvelope = serde_json::from_slice(&std::fs::read(cache_path()?).ok()?).ok()?;
    (cache.schema == CACHE_SCHEMA
        && cache.account_digest == expected_digest
        && !cache.models.is_empty())
    .then_some(cache)
}

fn save_cache(cache: &CacheEnvelope) -> anyhow::Result<()> {
    let path = cache_path().ok_or_else(|| anyhow::anyhow!("no cache directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(write_atomic(&path, &serde_json::to_vec_pretty(cache)?)?)
}

fn account_digest(account_id: &str) -> String {
    format!("{:x}", Sha256::digest(account_id.as_bytes()))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn result(
    models: Vec<ChatGptModel>,
    source: CatalogSource,
    warning: Option<String>,
) -> ChatGptCatalogResult {
    ChatGptCatalogResult {
        models,
        source,
        warning,
    }
}
fn fallback(warning: String) -> ChatGptCatalogResult {
    result(
        vec![ChatGptModel {
            slug: "gpt-5.5".to_string(),
            display_name: "GPT-5.5".to_string(),
            visibility: Some("list".to_string()),
            minimal_client_version: None,
            priority: None,
            context_window: Some(400_000),
            required_features: Vec::new(),
        }],
        CatalogSource::BuiltInFallback,
        Some(warning),
    )
}
fn append_warning(current: Option<String>, next: String) -> Option<String> {
    Some(current.map_or(next.clone(), |value| format!("{value}; {next}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_keeps_only_list_visible_nonempty_slugs() {
        let response: CatalogResponse = serde_json::from_value(serde_json::json!({
            "models": [
                {"slug":"gpt-b", "display_name":"B", "visibility":"list", "priority":2, "unknown":true},
                {"slug":"gpt-a", "display_name":"A", "visibility":"list", "priority":1},
                {"slug":"hidden", "display_name":"Hidden", "visibility":"hide"},
                {"slug":"missing", "display_name":"Missing visibility"},
                {"slug":"", "display_name":"Empty", "visibility":"list"}
            ]
        })).unwrap();
        let models = sanitize_catalog(response);
        assert_eq!(
            models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            ["gpt-a", "gpt-b"]
        );
    }

    #[test]
    fn cache_contains_digest_but_no_raw_account_or_tokens() {
        let cache = CacheEnvelope {
            schema: CACHE_SCHEMA,
            compatibility_version: CODEX_CATALOG_COMPAT_VERSION.to_string(),
            account_digest: account_digest("acct-secret"),
            fetched_ms: 1,
            etag: Some("etag".to_string()),
            models: fallback("warning".to_string()).models,
        };
        let json = serde_json::to_string(&cache).unwrap();
        assert!(!json.contains("acct-secret"));
        assert!(!json.contains("access_token"));
        assert!(json.contains(&account_digest("acct-secret")));
    }
}
