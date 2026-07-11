//! OAuth mechanics for provider logins that don't use a plain pasted API key.
//!
//! Two flows live here, plus the shared plumbing (PKCE, a localhost callback
//! server, and a token store):
//!
//! - **OpenRouter** — a browser PKCE flow that mints a normal API key. The key
//!   is handed back to the caller, which saves it via the ordinary
//!   [`auth`](crate::auth) store; nothing OAuth-specific is persisted.
//! - **OpenAI (Codex)** — the ChatGPT Pro/Plus browser flow, mirroring
//!   opencode's `codex.ts`. It yields short-lived `access`/`refresh` tokens that
//!   are stored here (in `oauth.json`) and transparently refreshed.
//!
//! The wire details (endpoints, params, constants) match opencode's
//! `packages/opencode/src/plugin/openai/codex.ts` and
//! `packages/llm/src/providers/openrouter.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

// ── OpenAI (Codex) constants — mirror codex.ts ──────────────────────────────

/// OpenAI OAuth client id (the Codex CLI app), from `codex.ts`.
pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OpenAI OAuth issuer base URL.
pub const OPENAI_ISSUER: &str = "https://auth.openai.com";
/// Fixed localhost port the OpenAI flow registers as its redirect target.
pub const OPENAI_OAUTH_PORT: u16 = 1455;
/// The redirect URI registered for [`OPENAI_CLIENT_ID`] — must match exactly.
pub const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// The unreserved character set for a PKCE verifier (RFC 7636 §4.1).
const PKCE_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
/// Verifier length. 64 sits comfortably inside the spec's 43–128 range.
const PKCE_VERIFIER_LEN: usize = 64;
/// The callback server gives up after this long (matches codex.ts).
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Stable credential-store key shared by the built-in ChatGPT aliases.
pub fn canonical_provider_key(provider: &str) -> Option<&'static str> {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "chatgpt" | "codex" | "openai-oauth"
    )
    .then_some("chatgpt")
}

// ── PKCE ────────────────────────────────────────────────────────────────────

/// Generate a PKCE `(verifier, challenge)` pair.
///
/// The verifier is [`PKCE_VERIFIER_LEN`] characters drawn from the unreserved
/// set `[A-Za-z0-9-._~]`. The challenge is `base64url-nopad(SHA-256(verifier))`
/// (the S256 method).
pub fn generate_pkce() -> (String, String) {
    let mut rng = rand::rng();
    let verifier: String = (0..PKCE_VERIFIER_LEN)
        .map(|_| {
            let idx = rng.random_range(0..PKCE_CHARSET.len());
            PKCE_CHARSET[idx] as char
        })
        .collect();
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

/// The S256 challenge for `verifier`: `base64url-nopad(SHA-256(verifier))`.
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// A URL-safe random state token (32 bytes, base64url-nopad) for CSRF defence.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

// ── Localhost callback server ───────────────────────────────────────────────

/// Bind `127.0.0.1:<port>` and wait (up to 5 minutes) for the browser's OAuth
/// redirect, returning the `code` once `state` matches `expected_state`.
///
/// A minimal HTTP handler: it reads each request's request line, ignores
/// anything without a `code`/`error` query (favicon probes and the like), and
/// on the real callback returns a small success (or error) page to the browser.
pub async fn await_oauth_code(port: u16, expected_state: &str) -> Result<String> {
    match tokio::time::timeout(
        CALLBACK_TIMEOUT,
        await_oauth_code_unbounded(port, expected_state),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => bail!("timed out waiting for the OAuth callback (5 minutes)"),
    }
}

pub async fn await_oauth_code_unbounded(port: u16, expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port} for the OAuth callback"))?;
    accept_callback(&listener, expected_state).await
}

/// Accept connections until one is the OAuth callback, then resolve it.
async fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("accepting an OAuth callback connection")?;
        let line = read_request_line(&mut stream).await.unwrap_or_default();
        let query = request_target(&line)
            .and_then(|t| t.split_once('?').map(|(_, q)| q))
            .unwrap_or("");
        // Not the callback (e.g. a browser favicon probe) — keep waiting.
        if !(query.contains("code=") || query.contains("error=")) {
            let _ = write_response(&mut stream, "Waiting for authorization…").await;
            continue;
        }
        return match parse_callback(&line, expected_state) {
            Ok(code) => {
                let _ = write_response(&mut stream, SUCCESS_PAGE).await;
                Ok(code)
            }
            Err(e) => {
                let _ = write_response(&mut stream, &error_page(&e.to_string())).await;
                Err(e)
            }
        };
    }
}

/// The request target (second whitespace-separated token) of an HTTP request
/// line, e.g. `/auth/callback?code=…` from `GET /auth/callback?code=… HTTP/1.1`.
fn request_target(request_line: &str) -> Option<&str> {
    request_line.split_whitespace().nth(1)
}

/// Read (best effort) the first line of the HTTP request. The request line is
/// the first thing on the wire and always fits in the first packet, so a single
/// read suffices here.
async fn read_request_line(stream: &mut TcpStream) -> Result<String> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf[..n]);
    Ok(text.lines().next().unwrap_or("").to_string())
}

/// Write a minimal `text/html` `200 OK` response and close the connection.
async fn write_response(stream: &mut TcpStream, body: &str) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Parse an OAuth callback request line into the authorization `code`, verifying
/// the returned `state` matches `expected_state`.
///
/// Errors on: an `error` query param (surfacing `error_description` when
/// present), a missing/empty `code`, or a `state` mismatch (possible CSRF).
/// Factored out of the server so the query + state logic is unit-testable.
fn parse_callback(request_line: &str, expected_state: &str) -> Result<String> {
    let target = request_target(request_line).ok_or_else(|| anyhow!("malformed request line"))?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);

    if let Some(err) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(err);
        bail!("authorization failed: {desc}");
    }
    let code = params
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("callback is missing the authorization code"))?;
    let state = params.get("state").map(String::as_str).unwrap_or("");
    if state != expected_state {
        bail!("state mismatch — possible CSRF (expected {expected_state:?}, got {state:?})");
    }
    Ok(code.clone())
}

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>hrdr — signed in</title></head>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;padding:3rem\">\
<h1>You're signed in ✓</h1><p>You can close this tab and return to hrdr.</p>\
</body></html>";

fn error_page(msg: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>hrdr — sign-in failed</title></head>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;padding:3rem\">\
<h1>Sign-in failed</h1><p>{}</p></body></html>",
        html_escape(msg)
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── OpenRouter flow (produces a normal API key) ─────────────────────────────

/// The OpenRouter authorization URL to open in the browser.
pub fn openrouter_authorize_url(callback_url: &str, challenge: &str) -> String {
    format!(
        "https://openrouter.ai/auth?callback_url={}&code_challenge={}&code_challenge_method=S256",
        form_encode(callback_url),
        form_encode(challenge),
    )
}

/// Exchange an OpenRouter authorization `code` for a plain API key (POST
/// `/api/v1/auth/keys`). The key is returned for the caller to persist.
pub async fn openrouter_exchange(code: &str, verifier: &str) -> Result<String> {
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "code_challenge_method": "S256",
    });
    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/auth/keys")
        .json(&body)
        .send()
        .await
        .context("requesting an OpenRouter API key")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenRouter key exchange failed ({status}): {text}");
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).context("parsing the OpenRouter key response")?;
    v.get("key")
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("OpenRouter response missing `key`: {text}"))
}

// ── OpenAI (Codex) flow ─────────────────────────────────────────────────────

/// Tokens returned by the OpenAI token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiTokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// The OpenAI authorization URL to open in the browser. Mirrors codex.ts's
/// `buildAuthorizeUrl`, with `originator=hrdr`.
pub fn openai_authorize_url(redirect: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", OPENAI_CLIENT_ID),
        ("redirect_uri", redirect),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "hrdr"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{OPENAI_ISSUER}/oauth/authorize?{query}")
}

/// Exchange an OpenAI authorization `code` for tokens (grant
/// `authorization_code`).
pub async fn openai_exchange(code: &str, redirect: &str, verifier: &str) -> Result<OpenAiTokens> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", OPENAI_CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .await
}

/// Refresh an OpenAI access token using a stored `refresh_token`.
pub async fn openai_refresh(refresh_token: &str) -> Result<OpenAiTokens> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_CLIENT_ID),
    ])
    .await
}

/// POST a form-encoded body to the OpenAI token endpoint and parse the tokens.
async fn post_token(params: &[(&str, &str)]) -> Result<OpenAiTokens> {
    let url = format!("{OPENAI_ISSUER}/oauth/token");
    let resp = reqwest::Client::new()
        .post(&url)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .form(params)
        .send()
        .await
        .context("requesting OpenAI tokens")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenAI token request failed ({status}): {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("parsing OpenAI token response: {text}"))
}

/// Extract the ChatGPT account id from a JWT (id or access token), reading the
/// `chatgpt_account_id` claim, then the namespaced
/// `["https://api.openai.com/auth"].chatgpt_account_id`, then
/// `organizations[0].id`. Returns `None` when the token isn't a JWT or none of
/// the claims are present. Mirrors codex.ts's `extractAccountIdFromClaims`.
pub fn parse_account_id(id_token_or_access: &str) -> Option<String> {
    let claims = decode_jwt_claims(id_token_or_access)?;
    if let Some(id) = claims.get("chatgpt_account_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    if let Some(id) = claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    claims
        .get("organizations")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Decode the payload (middle segment) of a `header.payload.signature` JWT.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (_, payload, _sig) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None; // not exactly three segments
    }
    // JWTs are base64url without padding; be tolerant of padded inputs too.
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ── OAuth credential store (OpenAI tokens) ──────────────────────────────────

/// Stored OAuth credentials for one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCreds {
    pub access: String,
    pub refresh: String,
    /// Absolute expiry of `access`, in epoch milliseconds.
    pub expires_ms: u64,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAccess {
    pub access: String,
    pub account_id: Option<String>,
    pub persistence_warning: Option<String>,
}

#[derive(Default)]
struct CredentialState {
    generation: u64,
    completion: u64,
    latest: Option<OAuthCreds>,
    refreshing: bool,
    last_result: Option<(u64, Result<OAuthAccess, String>)>,
}

struct CredentialCoordinator {
    // A synchronous mutex: the guard is never held across an `.await` (every
    // await point sits between an explicit `drop` and the next `lock`), so this
    // stays cheap and, crucially, lets [`RefreshGuard::drop`] reset state from a
    // sync `Drop` on cancellation.
    state: std::sync::Mutex<CredentialState>,
    changed: Notify,
}

impl CredentialCoordinator {
    /// Lock the state, recovering from poisoning — a panic mid-critical-section
    /// must not permanently wedge token acquisition.
    fn lock(&self) -> std::sync::MutexGuard<'_, CredentialState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Clears the in-flight `refreshing` flag if a refresher future is dropped
/// (cancelled) before it records a terminal result — otherwise `refreshing`
/// would stay true and every later waiter would park forever. Disarmed once the
/// refresher reaches its own terminal bookkeeping.
struct RefreshGuard<'a> {
    coordinator: &'a CredentialCoordinator,
    completion: u64,
    armed: bool,
}

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.coordinator.lock();
            if state.refreshing {
                state.refreshing = false;
                state.completion = self.completion.wrapping_add(1);
                state.last_result = None;
            }
        }
        self.coordinator.changed.notify_waiters();
    }
}

fn coordinator() -> &'static Arc<CredentialCoordinator> {
    static COORDINATOR: OnceLock<Arc<CredentialCoordinator>> = OnceLock::new();
    COORDINATOR.get_or_init(|| {
        Arc::new(CredentialCoordinator {
            state: std::sync::Mutex::new(CredentialState::default()),
            changed: Notify::new(),
        })
    })
}

/// Path to the OAuth token store (`~/.config/hrdr/oauth.json`), if `HOME` is set.
pub fn oauth_file_path() -> Option<PathBuf> {
    Some(crate::config_dir()?.join("oauth.json"))
}

/// Persist `creds` for `provider` (atomic write, `0600` on unix), preserving any
/// other providers' entries. Returns the file path.
pub(crate) fn save_oauth(provider: &str, creds: &OAuthCreds) -> Result<PathBuf> {
    let path = oauth_file_path().ok_or_else(|| anyhow!("no config dir to locate oauth.json"))?;
    let key = canonical_provider_key(provider).unwrap_or(provider);
    save_oauth_at(&path, key, creds)?;
    Ok(path)
}

/// Install credentials from a completed browser login without allowing an
/// older in-flight refresh to overwrite them.
pub async fn save_oauth_coordinated(provider: &str, creds: OAuthCreds) -> Result<PathBuf> {
    if canonical_provider_key(provider).is_none() {
        bail!("provider does not support ChatGPT authorization");
    }
    let mut state = coordinator().lock();
    state.generation = state.generation.wrapping_add(1);
    state.latest = Some(creds.clone());
    let saved = save_oauth(provider, &creds);
    drop(state);
    coordinator().changed.notify_waiters();
    saved
}

/// The stored OAuth credentials for `provider`, if any.
pub fn load_oauth(provider: &str) -> Option<OAuthCreds> {
    let path = oauth_file_path()?;
    let key = canonical_provider_key(provider).unwrap_or(provider);
    load_oauth_at(&path, key).or_else(|| {
        (key != provider)
            .then(|| load_oauth_at(&path, provider))
            .flatten()
    })
}

/// Whether stored credentials can authenticate synchronously: a safely valid
/// access token, or a refresh token capable of replacing an expired one.
pub fn has_oauth_credentials(provider: &str) -> bool {
    load_oauth(provider).is_some_and(|creds| oauth_credentials_usable(&creds, now_ms()))
}

fn oauth_credentials_usable(creds: &OAuthCreds, now: u64) -> bool {
    (!creds.access.trim().is_empty() && !access_expired(creds.expires_ms, now))
        || !creds.refresh.trim().is_empty()
}

/// Path-based core of [`save_oauth`].
fn save_oauth_at(path: &Path, provider: &str, creds: &OAuthCreds) -> Result<()> {
    let mut map = load_all(path);
    map.insert(provider.to_string(), creds.clone());
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let json = serde_json::to_vec_pretty(&map).context("serializing oauth.json")?;
    crate::write_atomic(path, &json).with_context(|| format!("writing {}", path.display()))
}

/// Path-based core of [`load_oauth`].
fn load_oauth_at(path: &Path, provider: &str) -> Option<OAuthCreds> {
    load_all(path).remove(provider)
}

/// All `provider → creds` entries (empty on any read/parse failure).
fn load_all(path: &Path) -> HashMap<String, OAuthCreds> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// A valid access token for `provider`, refreshing it first if it has (or is
/// about to) expire. Returns `(access, account_id)`, or `None` when there are no
/// stored credentials or a needed refresh fails.
pub async fn valid_access_token_result(provider: &str) -> Result<OAuthAccess> {
    if canonical_provider_key(provider).is_none() {
        bail!("provider does not support ChatGPT authorization");
    }
    let coordinator = coordinator().clone();
    // Outcome of one locked decision: wait on an in-flight refresh (carrying the
    // completion counter to gate the result) or become the refresher.
    enum Step {
        Wait(u64),
        Refresh(OAuthCreds, u64, u64),
    }
    loop {
        let disk_creds = load_oauth(provider);
        let notified = coordinator.changed.notified();
        tokio::pin!(notified);
        // Decide under the lock, then drop the (sync, non-`Send`) guard at the
        // block boundary — no guard may be alive across the awaits below.
        let step = {
            let mut state = coordinator.lock();
            let creds = state.latest.clone().or(disk_creds);
            let Some(creds) = creds else {
                bail!("no saved ChatGPT authorization");
            };
            if !access_expired(creds.expires_ms, now_ms()) && !creds.access.trim().is_empty() {
                state.latest = Some(creds.clone());
                return Ok(OAuthAccess {
                    access: creds.access,
                    account_id: creds.account_id,
                    persistence_warning: None,
                });
            }
            if creds.refresh.trim().is_empty() {
                bail!("saved ChatGPT authorization is expired and cannot be refreshed");
            }
            if state.refreshing {
                // Register with `changed` while still holding the lock. We
                // observed `refreshing == true`, so the refresher has not yet
                // entered its terminal section — and that section must re-acquire
                // this same lock before it can `notify_waiters()`. Enabling under
                // the lock therefore happens-before any notify that carries our
                // result, closing the window where a notify firing between our
                // unlock and our first poll would be lost. (`notify_waiters()`
                // stores no permit, so an unregistered waiter would otherwise park
                // until the next refresh — possibly a token lifetime away.)
                notified.as_mut().enable();
                Step::Wait(state.completion)
            } else {
                state.refreshing = true;
                Step::Refresh(creds, state.generation, state.completion)
            }
        };

        let (creds, generation, completion) = match step {
            Step::Wait(completion) => {
                notified.await;
                let state = coordinator.lock();
                // Prefer creds installed while we waited (e.g. a newer browser
                // login) over a stale `last_result`, so we never hand back a
                // superseded or errored token when a valid one is now available.
                if let Some(creds) = &state.latest
                    && !access_expired(creds.expires_ms, now_ms())
                    && !creds.access.trim().is_empty()
                {
                    return Ok(OAuthAccess {
                        access: creds.access.clone(),
                        account_id: creds.account_id.clone(),
                        persistence_warning: None,
                    });
                }
                if let Some((finished, result)) = &state.last_result
                    && *finished > completion
                {
                    return result.clone().map_err(anyhow::Error::msg);
                }
                continue;
            }
            Step::Refresh(creds, generation, completion) => (creds, generation, completion),
        };

        // Arm a guard over the one cancellable await below: if this future is
        // dropped during `refresh_creds`, the guard clears `refreshing` and
        // wakes waiters instead of stranding them. Disarmed the instant we
        // re-lock to record our own terminal result.
        let mut guard = RefreshGuard {
            coordinator: &coordinator,
            completion,
            armed: true,
        };
        let fresh = refresh_creds(&creds).await;
        let mut state = coordinator.lock();
        if state.generation != generation {
            // A newer login superseded this refresh: discard our result. Disarm
            // only after the state writes, so a panic mid-write still lets the
            // guard recover `refreshing` (Drop's own `refreshing` check keeps a
            // late fire a harmless no-op).
            state.refreshing = false;
            state.completion = completion.wrapping_add(1);
            state.last_result = None;
            guard.armed = false;
            drop(state);
            coordinator.changed.notify_waiters();
            continue;
        }
        let result = match fresh {
            Ok(fresh) => {
                state.latest = Some(fresh.clone());
                let persistence_warning = save_oauth(provider, &fresh).err().map(|e| {
                    format!("refreshed authorization is active but could not be saved: {e}")
                });
                Ok(OAuthAccess {
                    access: fresh.access,
                    account_id: fresh.account_id,
                    persistence_warning,
                })
            }
            Err(error) => Err(error.to_string()),
        };
        state.refreshing = false;
        state.completion = completion.wrapping_add(1);
        state.last_result = Some((state.completion, result.clone()));
        guard.armed = false;
        drop(state);
        coordinator.changed.notify_waiters();
        return result.map_err(anyhow::Error::msg);
    }
}

async fn refresh_creds(creds: &OAuthCreds) -> Result<OAuthCreds> {
    let tokens = openai_refresh(&creds.refresh).await?;
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(parse_account_id)
        .or_else(|| parse_account_id(&tokens.access_token))
        .or_else(|| creds.account_id.clone());
    Ok(OAuthCreds {
        access: tokens.access_token,
        refresh: tokens.refresh_token,
        expires_ms: now_ms() + tokens.expires_in.unwrap_or(3600) * 1000,
        account_id,
    })
}

pub async fn valid_access_token(provider: &str) -> Option<(String, Option<String>)> {
    valid_access_token_result(provider)
        .await
        .ok()
        .map(|token| (token.access, token.account_id))
}

/// Whether an access token expiring at `expires_ms` should be treated as
/// expired at `now_ms` — true within a 60-second safety margin, so a token
/// about to lapse is refreshed before it's used.
fn access_expired(expires_ms: u64, now_ms: u64) -> bool {
    now_ms + 60_000 >= expires_ms
}

/// Current time in epoch milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── URL encoding helpers ────────────────────────────────────────────────────

/// `application/x-www-form-urlencoded` encoding, matching JS `URLSearchParams`:
/// alphanumerics and `*-._` pass through, space becomes `+`, everything else is
/// percent-encoded with uppercase hex.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'*' | b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse a `k=v&k2=v2` query string into a map, percent-decoding both keys and
/// values (and treating `+` as space).
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decode a query component (`+` → space, `%XX` → byte).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PKCE ────────────────────────────────────────────────────────────────

    #[test]
    fn pkce_verifier_length_and_charset() {
        let (verifier, challenge) = generate_pkce();
        assert!(verifier.len() >= 43, "verifier must be at least 43 chars");
        assert!(
            verifier.bytes().all(|b| PKCE_CHARSET.contains(&b)),
            "verifier must be drawn from the unreserved set"
        );
        // The challenge is the S256 of exactly this verifier.
        assert_eq!(challenge, pkce_challenge(&verifier));
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(verifier), expected);
    }

    #[test]
    fn pkce_pairs_are_random() {
        assert_ne!(generate_pkce().0, generate_pkce().0);
        assert_ne!(generate_state(), generate_state());
    }

    // ── Callback parsing ─────────────────────────────────────────────────────

    #[test]
    fn parse_callback_valid_code_and_state() {
        let line = "GET /auth/callback?code=abc123&state=xyz HTTP/1.1";
        assert_eq!(parse_callback(line, "xyz").unwrap(), "abc123");
    }

    #[test]
    fn parse_callback_url_decodes_the_code() {
        let line = "GET /auth/callback?code=a%2Fb%2Bc&state=xyz HTTP/1.1";
        assert_eq!(parse_callback(line, "xyz").unwrap(), "a/b+c");
    }

    #[test]
    fn parse_callback_rejects_wrong_state() {
        let line = "GET /auth/callback?code=abc&state=bad HTTP/1.1";
        let err = parse_callback(line, "expected").unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "got: {err}");
    }

    #[test]
    fn parse_callback_surfaces_error_param() {
        let line = "GET /auth/callback?error=access_denied&error_description=User+said+no HTTP/1.1";
        let err = parse_callback(line, "xyz").unwrap_err().to_string();
        assert!(err.contains("User said no"), "got: {err}");
    }

    #[test]
    fn parse_callback_rejects_missing_code() {
        let line = "GET /auth/callback?state=xyz HTTP/1.1";
        let err = parse_callback(line, "xyz").unwrap_err().to_string();
        assert!(err.contains("missing the authorization code"), "got: {err}");
    }

    // ── URL builders ─────────────────────────────────────────────────────────

    #[test]
    fn openrouter_authorize_url_is_pinned() {
        let url = openrouter_authorize_url("http://localhost:8976/callback", "chal_abc");
        assert_eq!(
            url,
            "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A8976%2Fcallback\
             &code_challenge=chal_abc&code_challenge_method=S256"
        );
    }

    #[test]
    fn openai_authorize_url_is_pinned() {
        let url = openai_authorize_url(OPENAI_REDIRECT_URI, "chal_abc", "state_xyz");
        assert_eq!(
            url,
            "https://auth.openai.com/oauth/authorize?response_type=code\
             &client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &scope=openid+profile+email+offline_access\
             &code_challenge=chal_abc&code_challenge_method=S256\
             &id_token_add_organizations=true&codex_cli_simplified_flow=true\
             &state=state_xyz&originator=hrdr"
        );
    }

    // ── JWT account id ───────────────────────────────────────────────────────

    /// Build a `header.payload.sig` JWT whose payload is `claims`.
    fn jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn parse_account_id_top_level_claim() {
        let token = jwt(serde_json::json!({ "chatgpt_account_id": "acct_top" }));
        assert_eq!(parse_account_id(&token), Some("acct_top".to_string()));
    }

    #[test]
    fn parse_account_id_namespaced_claim() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_ns" }
        }));
        assert_eq!(parse_account_id(&token), Some("acct_ns".to_string()));
    }

    #[test]
    fn parse_account_id_organizations_fallback() {
        let token = jwt(serde_json::json!({
            "organizations": [{ "id": "org_first" }, { "id": "org_second" }]
        }));
        assert_eq!(parse_account_id(&token), Some("org_first".to_string()));
    }

    #[test]
    fn parse_account_id_none_when_absent_or_malformed() {
        assert_eq!(
            parse_account_id(&jwt(serde_json::json!({ "email": "x" }))),
            None
        );
        assert_eq!(parse_account_id("not-a-jwt"), None);
        assert_eq!(parse_account_id("only.two"), None);
    }

    // ── Expiry logic ─────────────────────────────────────────────────────────

    #[test]
    fn access_expired_honours_60s_margin() {
        // Comfortably in the future → still valid.
        assert!(!access_expired(1_000_000, 900_000));
        // Already past → expired.
        assert!(access_expired(900_000, 1_000_000));
        // Within the 60s margin → treated as expired (refresh early).
        assert!(access_expired(1_000_000, 950_000));
        // Exactly 60s + 1ms out → still valid.
        assert!(!access_expired(1_000_000, 939_999));
    }

    #[test]
    fn oauth_readiness_is_time_aware() {
        let creds = |access: &str, refresh: &str, expires_ms| OAuthCreds {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_ms,
            account_id: None,
        };
        assert!(oauth_credentials_usable(
            &creds("access", "", 1_000_000),
            900_000
        ));
        assert!(oauth_credentials_usable(&creds("", "refresh", 0), 900_000));
        assert!(oauth_credentials_usable(
            &creds("expired", "refresh", 1),
            900_000
        ));
        assert!(!oauth_credentials_usable(&creds("expired", "", 1), 900_000));
        assert!(!oauth_credentials_usable(&creds("", "", 0), 900_000));
    }

    #[test]
    fn chatgpt_aliases_share_a_canonical_key() {
        for alias in ["chatgpt", "codex", "openai-oauth", "ChatGPT"] {
            assert_eq!(canonical_provider_key(alias), Some("chatgpt"));
        }
        assert_eq!(canonical_provider_key("openai"), None);
    }

    #[tokio::test]
    async fn coordinator_rejects_non_chatgpt_provider() {
        let error = valid_access_token_result("custom").await.unwrap_err();
        assert!(error.to_string().contains("does not support"));
    }

    #[tokio::test]
    async fn dropped_refresher_clears_refreshing_and_wakes_waiters() {
        // A refresher whose future is cancelled mid-refresh must not leave
        // `refreshing` stuck true — otherwise every later waiter parks forever.
        let coord = Arc::new(CredentialCoordinator {
            state: std::sync::Mutex::new(CredentialState::default()),
            changed: Notify::new(),
        });
        {
            let mut state = coord.lock();
            state.refreshing = true;
            state.completion = 5;
        }
        // Simulate the cancellation: an armed guard dropped without disarming.
        drop(RefreshGuard {
            coordinator: &coord,
            completion: 5,
            armed: true,
        });
        let state = coord.lock();
        assert!(!state.refreshing, "cancel-drop clears the refreshing flag");
        assert_eq!(
            state.completion, 6,
            "completion advances so waiters re-eval"
        );
        assert!(state.last_result.is_none());
    }

    #[tokio::test]
    async fn disarmed_refresher_guard_is_a_noop() {
        // The normal terminal path disarms the guard; its drop must not clobber
        // the result the refresher just recorded.
        let coord = Arc::new(CredentialCoordinator {
            state: std::sync::Mutex::new(CredentialState::default()),
            changed: Notify::new(),
        });
        {
            let mut state = coord.lock();
            state.refreshing = true;
            state.completion = 5;
        }
        let mut guard = RefreshGuard {
            coordinator: &coord,
            completion: 5,
            armed: true,
        };
        guard.armed = false;
        drop(guard);
        let state = coord.lock();
        assert!(state.refreshing, "disarmed guard leaves state untouched");
        assert_eq!(state.completion, 5);
    }

    // ── Store round-trip ─────────────────────────────────────────────────────

    #[test]
    fn oauth_store_round_trips_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");

        let openai = OAuthCreds {
            access: "acc-1".to_string(),
            refresh: "ref-1".to_string(),
            expires_ms: 111,
            account_id: Some("acct_1".to_string()),
        };
        let other = OAuthCreds {
            access: "acc-2".to_string(),
            refresh: "ref-2".to_string(),
            expires_ms: 222,
            account_id: None,
        };
        save_oauth_at(&path, "openai", &openai).unwrap();
        save_oauth_at(&path, "other", &other).unwrap();

        assert_eq!(load_oauth_at(&path, "openai"), Some(openai));
        assert_eq!(load_oauth_at(&path, "other"), Some(other.clone()));

        // Re-saving one entry leaves the other intact.
        let openai2 = OAuthCreds {
            access: "acc-1b".to_string(),
            refresh: "ref-1b".to_string(),
            expires_ms: 333,
            account_id: None,
        };
        save_oauth_at(&path, "openai", &openai2).unwrap();
        assert_eq!(load_oauth_at(&path, "openai"), Some(openai2));
        assert_eq!(load_oauth_at(&path, "other"), Some(other));
    }

    #[test]
    fn oauth_store_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_oauth_at(&dir.path().join("nope.json"), "openai"), None);
    }

    // ── encoding helpers ─────────────────────────────────────────────────────

    #[test]
    fn form_encode_matches_urlsearchparams() {
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("http://x/y"), "http%3A%2F%2Fx%2Fy");
        assert_eq!(form_encode("keep-._*"), "keep-._*");
    }
}
