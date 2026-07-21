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
//!   are stored as `oauth` entries in the unified credential store
//!   ([`crate::auth_store`], `auth.json`) and transparently refreshed.
//!
//! The wire details (endpoints, params, constants) match opencode's
//! `packages/opencode/src/plugin/openai/codex.ts` and
//! `packages/llm/src/providers/openrouter.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use hrdr_llm::capped_read::{MAX_DIAGNOSTIC_BYTES, read_capped_text};

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
/// The OpenRouter callback server gives up after this long (matches codex.ts).
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Generous outer backstop for the whole ChatGPT browser login (callback +
/// token exchange + save): the subscription flow can involve MFA and account
/// pickers, so it is not held to the 5-minute callback deadline. Bounded so a
/// user who abandons the browser doesn't leave a listener + task alive forever.
pub const CHATGPT_LOGIN_BACKSTOP: Duration = Duration::from_secs(60 * 60);

/// Total-request timeout for token HTTP requests (OpenRouter key exchange,
/// OpenAI token exchange/refresh). Without this a black-holed network mid-
/// refresh wedges the single-flight refresher forever, parking every caller
/// (see [`coordinated_oauth_access`]). Mirrors `chatgpt_models.rs`'s
/// `CATALOG_HTTP_TIMEOUT`.
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared `reqwest::Client` used for token HTTP requests, with
/// [`OAUTH_HTTP_TIMEOUT`] applied. `Client::builder().build()` can fail (e.g.
/// TLS backend init), so this returns a recoverable error rather than
/// `.unwrap()`-ing a panic where `Client::new()` used to be infallible.
fn oauth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(OAUTH_HTTP_TIMEOUT)
        .build()
        .context("building the OAuth HTTP client")
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
    await_oauth_code_within(port, expected_state, CALLBACK_TIMEOUT).await
}

/// Like [`await_oauth_code`] but with a caller-chosen deadline. OpenRouter keeps
/// the 5-minute [`CALLBACK_TIMEOUT`]; ChatGPT passes a larger bound because its
/// whole flow is wrapped in the [`CHATGPT_LOGIN_BACKSTOP`] by the caller.
pub async fn await_oauth_code_within(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port} for the OAuth callback"))?;
    match tokio::time::timeout(timeout, accept_callback(&listener, expected_state)).await {
        Ok(res) => res,
        Err(_) => bail!(
            "timed out waiting for the OAuth callback ({}s)",
            timeout.as_secs()
        ),
    }
}

/// Accept connections until one is an OAuth callback carrying the state this
/// login minted. Callback-shaped requests with another state are local probes,
/// not authoritative provider responses, so reject them without letting them
/// terminate the real login.
async fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("accepting an OAuth callback connection")?;
        let line = tokio::time::timeout(Duration::from_secs(2), read_request_line(&mut stream))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let query = request_target(&line)
            .and_then(|t| t.split_once('?').map(|(_, q)| q))
            .unwrap_or("");
        let params = parse_query(query);
        // Not the callback (e.g. a browser favicon probe) — keep waiting.
        if !(params.contains_key("code") || params.contains_key("error")) {
            let _ = write_response(&mut stream, "Waiting for authorization…").await;
            continue;
        }
        // A process which did not start this login does not know its state. It
        // must not be able to end the listener before the browser gets here.
        if params.get("state").map(String::as_str).unwrap_or("") != expected_state {
            let err = parse_callback(&line, expected_state)
                .unwrap_err()
                .to_string();
            let _ = write_response(&mut stream, &error_page(&err)).await;
            continue;
        }
        match parse_callback(&line, expected_state) {
            Ok(code) => {
                let _ = write_response(&mut stream, SUCCESS_PAGE).await;
                return Ok(code);
            }
            Err(e) => {
                let _ = write_response(&mut stream, &error_page(&e.to_string())).await;
                // A provider error with our state is authoritative. A malformed
                // code callback is not: keep listening for the browser retry.
                if params.contains_key("error") {
                    return Err(e);
                }
            }
        }
    }
}

/// The request target (second whitespace-separated token) of an HTTP request
/// line, e.g. `/auth/callback?code=…` from `GET /auth/callback?code=… HTTP/1.1`.
fn request_target(request_line: &str) -> Option<&str> {
    request_line.split_whitespace().nth(1)
}

/// Read the first HTTP request line, tolerating TCP fragmentation. The fixed
/// buffer bounds local memory and the caller supplies a short socket deadline.
async fn read_request_line(stream: &mut TcpStream) -> Result<String> {
    let mut buf = [0u8; 8192];
    let mut used = 0;
    while used < buf.len() {
        let n = stream.read(&mut buf[used..]).await?;
        if n == 0 {
            break;
        }
        used += n;
        if buf[..used].windows(2).any(|bytes| bytes == b"\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..used]);
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

    let state = params.get("state").map(String::as_str).unwrap_or("");
    if state != expected_state {
        // Do NOT echo `expected_state` — this message is written into the HTML
        // page returned to whoever hit the localhost port and into the
        // transcript, so disclosing our own CSRF token to a local prober is
        // gratuitous. The received value is enough to diagnose.
        bail!("state mismatch — possible CSRF (got {state:?})");
    }
    if let Some(err) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(err);
        bail!("authorization failed: {desc}");
    }
    let code = params
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("callback is missing the authorization code"))?;
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
    // `&` MUST be replaced first so the `&` it introduces is not re-escaped by
    // the later replacements.
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ── OpenRouter flow (produces a normal API key) ─────────────────────────────

/// The OpenRouter loopback callback URL, carrying the CSRF `state` token.
///
/// OpenRouter's OAuth PKCE flow supports `state` by preserving a `state` query
/// parameter embedded in the `callback_url` you hand it (per OpenRouter's OAuth
/// upgrade — the flow has no separate top-level `state` param the way OpenAI's
/// does). It appends `code` to this URL on redirect, so the callback returns as
/// `…/auth/callback?state=<state>&code=<code>` and [`parse_callback`] verifies
/// the round-trip. `state` is base64url ([`generate_state`]), so it is already
/// URL-safe and needs no extra encoding here.
pub fn openrouter_callback_url(port: u16, state: &str) -> String {
    format!("http://localhost:{port}/auth/callback?state={state}")
}

/// The OpenRouter authorization URL to open in the browser. `callback_url` is
/// the loopback URL (see [`openrouter_callback_url`], which embeds the CSRF
/// `state`); it is form-encoded whole, so any query it carries survives.
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
    let resp = oauth_http_client()?
        .post("https://openrouter.ai/api/v1/auth/keys")
        .json(&body)
        .send()
        .await
        .context("requesting an OpenRouter API key")?;
    let status = resp.status();
    if !status.is_success() {
        // Read capped diagnostic but never include the body in the error
        // message: the OpenRouter key-exchange response body IS the API key
        // (`{"key":"sk-or-v1-..."}`), or at minimum contains partial secrets.
        let _ = read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
        bail!("OpenRouter key exchange failed ({status})");
    }
    let v: serde_json::Value =
        serde_json::from_str(&read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await)
            .context("parsing the OpenRouter key response")?;
    v.get("key")
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("OpenRouter key exchange response missing `key`"))
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
    let resp = oauth_http_client()?
        .post(&url)
        .form(params)
        .send()
        .await
        .context("requesting OpenAI tokens")?;
    let status = resp.status();
    let text = read_capped_text(resp, MAX_DIAGNOSTIC_BYTES).await;
    if !status.is_success() {
        bail!("{}", sanitized_token_error(status, &text));
    }
    // Never echo the response body: it carries access/refresh tokens.
    serde_json::from_str(&text).map_err(|_| anyhow!("could not parse the OpenAI token response"))
}

/// A bounded, secret-free diagnostic for a failed token request. The raw body
/// carries access/refresh tokens, authorization codes, and PKCE verifiers, so
/// only the short OAuth `error` code (e.g. `invalid_grant`) is surfaced — never
/// the body itself.
fn sanitized_token_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.chars().take(64).collect::<String>())
        });
    match code {
        Some(c) => format!("OpenAI token request failed ({status}): {c}"),
        None => format!("OpenAI token request failed ({status})"),
    }
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
///
/// This is the public type callers use. In the unified store it is persisted as
/// an [`AuthEntry::Oauth`](crate::auth_store) `oauth` entry, with a lossless
/// conversion in both directions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCreds {
    pub access: String,
    pub refresh: String,
    /// Absolute expiry of `access`, in epoch milliseconds.
    pub expires_ms: u64,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// Path to the credential store (`~/.config/hrdr/auth.json`), if `HOME` is set.
///
/// OAuth credentials now share the single unified store with API keys, so this
/// returns the same `auth.json` path as [`crate::auth_file_path`]. The name is
/// kept for the callers that display it.
pub fn oauth_file_path() -> Option<PathBuf> {
    crate::auth_store::store_path()
}

/// Persist `creds` for `provider` (atomic write, `0600` on unix), preserving any
/// other providers' entries. Returns the file path.
pub fn save_oauth(provider: &str, creds: &OAuthCreds) -> Result<PathBuf> {
    let path = oauth_file_path().ok_or_else(|| anyhow!("no config dir to locate auth.json"))?;
    crate::auth_store::save_oauth_entry_at(&path, provider, creds)?;
    Ok(path)
}

/// The stored OAuth credentials for `provider`, if any.
pub fn load_oauth(provider: &str) -> Option<OAuthCreds> {
    crate::auth_store::load_oauth_entry_at(&oauth_file_path()?, provider)
}

/// The store key for a provider's OAuth credentials, canonicalized ONLY for
/// trusted OpenAI OAuth. Returns the fixed `"openai"` slot when — and only
/// when — `kind == ChatGptOAuth`, so the built-in `openai` OAuth login stores
/// its credential in the same slot the merged `openai` provider reads. For every
/// other kind it returns the exact `name` unchanged.
///
/// The canonicalization is driven by the trusted [`ResolvedProviderKind`],
/// never by the provider spelling or its `base_url`. A custom-shadow call
/// (kind `Custom`) therefore never resolves to the built-in `openai` slot,
/// even when spelled `openai`/`chatgpt` — it reads/writes its own exact-name
/// entry.
pub fn canonical_oauth_key(kind: crate::ResolvedProviderKind, name: &str) -> &str {
    match kind {
        crate::ResolvedProviderKind::ChatGptOAuth => "openai",
        _ => name,
    }
}

/// Kind-gated wrapper over [`save_oauth`]: computes the store key via
/// [`canonical_oauth_key`] before handing it to the exact-key store helper, so
/// the exact-key core is never given a canonicalized alias by an untrusted path.
pub fn save_oauth_for(
    kind: crate::ResolvedProviderKind,
    name: &str,
    creds: &OAuthCreds,
) -> Result<PathBuf> {
    save_oauth(canonical_oauth_key(kind, name), creds)
}

/// Kind-gated wrapper over [`load_oauth`]; see [`save_oauth_for`].
pub fn load_oauth_for(kind: crate::ResolvedProviderKind, name: &str) -> Option<OAuthCreds> {
    load_oauth(canonical_oauth_key(kind, name))
}

/// Whether trusted ChatGPT OAuth has usable or refreshable credentials for
/// `name` — a synchronous readiness check with NO network refresh and NO secret
/// return. Returns `false` for any kind other than `ChatGptOAuth` (so the
/// built-in credential-presence bit is not a spelling-reachable oracle for a
/// custom shadow), and resolves the store key via the kind-gated
/// [`canonical_oauth_key`], never the raw name.
///
/// Time-aware: ready when a non-expired access token exists OR a non-empty
/// refresh token exists (refreshable). Expired access with no refresh, empty
/// tokens, and unrelated providers are all not ready.
pub fn has_oauth_credentials(kind: crate::ResolvedProviderKind, name: &str) -> bool {
    match oauth_file_path() {
        Some(path) => has_oauth_credentials_at(&path, kind, name),
        None => false,
    }
}

/// Path-based core of [`has_oauth_credentials`].
fn has_oauth_credentials_at(path: &Path, kind: crate::ResolvedProviderKind, name: &str) -> bool {
    if kind != crate::ResolvedProviderKind::ChatGptOAuth {
        return false;
    }
    let Some(creds) = crate::auth_store::load_oauth_entry_at(path, canonical_oauth_key(kind, name))
    else {
        return false;
    };
    let has_valid_access = !creds.access.is_empty() && !access_expired(creds.expires_ms, now_ms());
    has_valid_access || !creds.refresh.is_empty()
}

/// A valid access token for `provider`, refreshing it first if it has (or is
/// about to) expire. Returns `(access, account_id)`, or `None` when there are no
/// stored credentials or a needed refresh fails.
///
/// This is the uncoordinated path, kept for non-ChatGPT callers. Trusted
/// ChatGPT OAuth goes through [`coordinated_oauth_access`], which single-flights
/// concurrent refreshes.
pub async fn valid_access_token(provider: &str) -> Option<(String, Option<String>)> {
    let creds = load_oauth(provider)?;
    if !access_expired(creds.expires_ms, now_ms()) {
        return Some((creds.access, creds.account_id));
    }
    // Expired (or within the safety margin): refresh, persist, and return fresh.
    let fresh = refresh_to_creds(&creds.refresh, creds.account_id)
        .await
        .ok()?;
    let _ = save_oauth(provider, &fresh);
    Some((fresh.access, fresh.account_id))
}

/// Refresh an access token from its refresh token, folding the account id out
/// of the new id/access JWT (falling back to `prev_account`).
async fn refresh_to_creds(refresh_token: &str, prev_account: Option<String>) -> Result<OAuthCreds> {
    let tokens = openai_refresh(refresh_token).await?;
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(parse_account_id)
        .or_else(|| parse_account_id(&tokens.access_token))
        .or(prev_account);
    Ok(OAuthCreds {
        access: tokens.access_token,
        refresh: tokens.refresh_token,
        expires_ms: now_ms() + tokens.expires_in.unwrap_or(3600) * 1000,
        account_id,
    })
}

// ── Cancel-safe refresh coordinator ─────────────────────────────────────────

/// A usable access token plus the optional ChatGPT account id — the output of
/// [`coordinated_oauth_access`].
///
/// Deliberately NO `Debug` derive: it holds a bearer token, and a `{:?}` (or
/// `anyhow` context) must never leak it. `account_id` is the account-digest and
/// `ChatGPT-Account-Id` header input; callers should not re-read the store for
/// it.
pub struct OAuthAccess {
    pub access: String,
    pub account_id: Option<String>,
}

/// Process-global single-flight coordinator for one ChatGPT credential slot.
/// Shared by main agents and all sub-agents so concurrent refreshes collapse to
/// one token request (concurrent writers would otherwise race on the atomic
/// `auth.json` replace and could strand a rotated refresh chain).
struct RefreshCoordinator {
    state: Mutex<CoordState>,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct CoordState {
    /// A refresh is in flight; other callers wait rather than start their own.
    refreshing: bool,
}

static CHATGPT_COORD: OnceLock<RefreshCoordinator> = OnceLock::new();

fn chatgpt_coord() -> &'static RefreshCoordinator {
    CHATGPT_COORD.get_or_init(|| RefreshCoordinator {
        state: Mutex::new(CoordState::default()),
        notify: tokio::sync::Notify::new(),
    })
}

/// RAII guard for the in-flight refresher: on drop (normal return, `?`, panic,
/// or task cancellation) it clears `refreshing` and wakes every waiter, so a
/// cancelled refresh can never wedge the gate or lose a wakeup.
struct RefresherGuard<'a> {
    coord: &'a RefreshCoordinator,
}

impl Drop for RefresherGuard<'_> {
    fn drop(&mut self) {
        {
            let mut st = self.coord.state.lock().unwrap_or_else(|p| p.into_inner());
            st.refreshing = false;
        }
        self.coord.notify.notify_waiters();
    }
}

/// A valid access token for trusted ChatGPT OAuth, single-flighting concurrent
/// refreshes. Gated on both the trusted [`ResolvedProviderKind::ChatGptOAuth`]
/// kind and the canonical [`CHATGPT_CODEX_BASE_URL`] — never callable for a
/// custom shadow or a different endpoint.
///
/// [`ResolvedProviderKind::ChatGptOAuth`]: crate::ResolvedProviderKind::ChatGptOAuth
/// [`CHATGPT_CODEX_BASE_URL`]: crate::CHATGPT_CODEX_BASE_URL
pub async fn coordinated_oauth_access(
    kind: crate::ResolvedProviderKind,
    base_url: &str,
) -> Result<OAuthAccess> {
    if kind != crate::ResolvedProviderKind::ChatGptOAuth
        || base_url != crate::CHATGPT_CODEX_BASE_URL
    {
        bail!("coordinated_oauth_access is only valid for trusted ChatGPT OAuth");
    }
    let path = oauth_file_path().ok_or_else(|| anyhow!("no config dir to locate auth.json"))?;
    coordinated_access_core(
        chatgpt_coord(),
        || crate::auth_store::load_oauth_entry_at(&path, "openai"),
        |c| {
            let _ = crate::auth_store::save_oauth_entry_at(&path, "openai", c);
        },
        |refresh_token, prev| async move { refresh_to_creds(&refresh_token, prev).await },
    )
    .await
}

/// Injectable core of [`coordinated_oauth_access`]: `load`/`persist` abstract
/// the credential store and `refresh` the token request, so concurrency and
/// newer-credential-wins behaviour are testable without HOME or the network.
async fn coordinated_access_core<L, P, R, Fut>(
    coord: &RefreshCoordinator,
    load: L,
    persist: P,
    refresh: R,
) -> Result<OAuthAccess>
where
    L: Fn() -> Option<OAuthCreds>,
    P: Fn(&OAuthCreds),
    R: Fn(String, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<OAuthCreds>>,
{
    loop {
        // ── Decision section: synchronous lock only, never held across .await ──
        let wait = {
            let mut st = coord.state.lock().unwrap_or_else(|p| p.into_inner());
            match load() {
                None => bail!("no ChatGPT credentials; run /login"),
                Some(c) => {
                    // A currently-usable credential wins immediately — including
                    // one a browser login installed while we waited.
                    if !c.access.is_empty() && !access_expired(c.expires_ms, now_ms()) {
                        return Ok(OAuthAccess {
                            access: c.access,
                            account_id: c.account_id,
                        });
                    }
                    if c.refresh.is_empty() {
                        bail!(
                            "ChatGPT credentials are expired and have no refresh token; run /login"
                        );
                    }
                }
            }
            if st.refreshing {
                // Register the waiter WHILE holding the lock so the refresher's
                // `notify_waiters()` (which only wakes already-registered
                // waiters) cannot fire between our decision and our await.
                let mut fut = Box::pin(coord.notify.notified());
                fut.as_mut().enable();
                Some(fut)
            } else {
                st.refreshing = true;
                None
            }
        };

        if let Some(fut) = wait {
            fut.await;
            continue; // Re-decide: usually a fresh credential now exists.
        }

        // ── We are the refresher. The guard clears the gate + wakes waiters on
        //    every exit path (return, ?, panic, cancellation). ──
        let _guard = RefresherGuard { coord };
        let cur = load().ok_or_else(|| anyhow!("credentials vanished before refresh"))?;
        // A concurrent login may have installed a usable credential already.
        if !cur.access.is_empty() && !access_expired(cur.expires_ms, now_ms()) {
            return Ok(OAuthAccess {
                access: cur.access,
                account_id: cur.account_id,
            });
        }
        let fresh = refresh(cur.refresh.clone(), cur.account_id.clone()).await?;
        // Prefer a newer browser-installed credential over our refresh output if
        // the store changed under us during the request (login wins).
        if let Some(latest) = load()
            && latest.access != cur.access
            && !latest.access.is_empty()
            && !access_expired(latest.expires_ms, now_ms())
        {
            return Ok(OAuthAccess {
                access: latest.access,
                account_id: latest.account_id,
            });
        }
        persist(&fresh);
        return Ok(OAuthAccess {
            access: fresh.access,
            account_id: fresh.account_id,
        });
    }
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
        let line = "GET /auth/callback?error=access_denied&error_description=User+said+no&state=xyz HTTP/1.1";
        let err = parse_callback(line, "xyz").unwrap_err().to_string();
        assert!(err.contains("User said no"), "got: {err}");
    }

    #[test]
    fn parse_callback_rejects_missing_code() {
        let line = "GET /auth/callback?state=xyz HTTP/1.1";
        let err = parse_callback(line, "xyz").unwrap_err().to_string();
        assert!(err.contains("missing the authorization code"), "got: {err}");
    }

    // ── HTML escaping ────────────────────────────────────────────────────────

    #[test]
    fn html_escape_covers_all_five_entities() {
        assert_eq!(html_escape("&"), "&amp;");
        assert_eq!(html_escape("<"), "&lt;");
        assert_eq!(html_escape(">"), "&gt;");
        assert_eq!(html_escape("\""), "&quot;");
        assert_eq!(html_escape("'"), "&#39;");

        // Combined input exercises every entity together.
        assert_eq!(
            html_escape("he said \"<a href='x'>&\""),
            "he said &quot;&lt;a href=&#39;x&#39;&gt;&amp;&quot;"
        );

        // `&` is escaped exactly once — a literal `&amp;` round-trips to
        // `&amp;amp;`, not `&amp;amp;amp;...`.
        assert_eq!(html_escape("&amp;"), "&amp;amp;");
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

    /// The OpenRouter callback URL embeds the CSRF `state`, and the callback
    /// OpenRouter returns (`state` + appended `code`) round-trips through
    /// `parse_callback` — while a forged or missing `state` is refused. This is
    /// the CSRF defence the flow previously lacked (it awaited with an empty
    /// `expected_state`, accepting any callback).
    #[test]
    fn openrouter_callback_carries_state_that_parse_callback_validates() {
        // `generate_state` is base64url, so a token like this needs no encoding.
        let state = "Xy_9-abcDEF";
        assert_eq!(
            openrouter_callback_url(1456, state),
            "http://localhost:1456/auth/callback?state=Xy_9-abcDEF"
        );

        // OpenRouter redirects to that URL with `code` appended (with `&`, since
        // the callback already carries `?state=`): the round-trip validates.
        let good = format!("GET /auth/callback?state={state}&code=THE_CODE HTTP/1.1");
        assert_eq!(parse_callback(&good, state).unwrap(), "THE_CODE");

        // A forged callback with a different state is rejected as possible CSRF.
        let forged = "GET /auth/callback?state=attacker&code=EVIL HTTP/1.1";
        assert!(parse_callback(forged, state).is_err());

        // A callback with NO state (the pre-fix always-accept path) is rejected.
        let bare = "GET /auth/callback?code=THE_CODE HTTP/1.1";
        assert!(parse_callback(bare, state).is_err());
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

    // ── Kind-gated key + readiness ───────────────────────────────────────────
    use crate::ResolvedProviderKind as K;

    #[test]
    fn canonical_oauth_key_canonicalizes_only_for_chatgpt_oauth() {
        // Trusted OpenAI OAuth: every alias collapses to the one `openai` slot.
        assert_eq!(canonical_oauth_key(K::ChatGptOAuth, "openai"), "openai");
        assert_eq!(canonical_oauth_key(K::ChatGptOAuth, "chatgpt"), "openai");
        assert_eq!(canonical_oauth_key(K::ChatGptOAuth, "codex"), "openai");
        assert_eq!(
            canonical_oauth_key(K::ChatGptOAuth, "openai-oauth"),
            "openai"
        );
        // Any other kind keeps the exact name — no canonicalization by spelling.
        assert_eq!(canonical_oauth_key(K::Custom, "openai"), "openai");
        assert_eq!(canonical_oauth_key(K::Custom, "my-provider"), "my-provider");
        assert_eq!(canonical_oauth_key(K::BuiltIn, "openrouter"), "openrouter");
    }

    /// Store an `openai` OAuth entry with an `expires_ms` relative to real now.
    fn seed_chatgpt(path: &Path, access: &str, refresh: &str, expires_ms: u64) {
        crate::auth_store::save_oauth_entry_at(
            path,
            "openai",
            &OAuthCreds {
                access: access.to_string(),
                refresh: refresh.to_string(),
                expires_ms,
                account_id: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn readiness_valid_access_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "acc", "ref", now_ms() + 3_600_000);
        assert!(has_oauth_credentials_at(&path, K::ChatGptOAuth, "chatgpt"));
    }

    #[test]
    fn readiness_expired_access_with_refresh_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "acc", "ref", 1); // long past
        assert!(has_oauth_credentials_at(&path, K::ChatGptOAuth, "chatgpt"));
    }

    #[test]
    fn readiness_refresh_only_is_ready() {
        // Selectable with refresh-only credentials (empty access, refresh present).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "", "ref", 1);
        assert!(has_oauth_credentials_at(&path, K::ChatGptOAuth, "chatgpt"));
    }

    #[test]
    fn readiness_expired_access_without_refresh_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "acc", "", 1); // expired, no refresh
        assert!(!has_oauth_credentials_at(&path, K::ChatGptOAuth, "chatgpt"));
    }

    #[test]
    fn readiness_empty_tokens_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "", "", now_ms() + 3_600_000);
        assert!(!has_oauth_credentials_at(&path, K::ChatGptOAuth, "chatgpt"));
    }

    #[test]
    fn readiness_unrelated_provider_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "acc", "ref", now_ms() + 3_600_000);
        // Nothing stored under "openrouter"; and its kind is not ChatGptOAuth.
        assert!(!has_oauth_credentials_at(&path, K::BuiltIn, "openrouter"));
    }

    #[test]
    fn custom_shadow_cannot_read_builtin_oauth_credentials() {
        // Built-in ChatGPT creds are present in the slot...
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        seed_chatgpt(&path, "acc", "ref", now_ms() + 3_600_000);
        // ...but a custom provider spelled "chatgpt" resolves to kind Custom, and
        // the kind gate returns false before any load — not a spelling oracle.
        assert!(!has_oauth_credentials_at(&path, K::Custom, "chatgpt"));
    }

    // ── Refresh coordinator (Task 2) ─────────────────────────────────────────
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn new_coord() -> RefreshCoordinator {
        RefreshCoordinator {
            state: Mutex::new(CoordState::default()),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn expired_with_refresh() -> Arc<Mutex<Option<OAuthCreds>>> {
        Arc::new(Mutex::new(Some(OAuthCreds {
            access: "old".to_string(),
            refresh: "refresh-tok".to_string(),
            expires_ms: 1, // long past
            account_id: None,
        })))
    }

    fn store_load(store: &Arc<Mutex<Option<OAuthCreds>>>) -> impl Fn() -> Option<OAuthCreds> {
        let store = store.clone();
        move || store.lock().unwrap().clone()
    }

    fn store_persist(store: &Arc<Mutex<Option<OAuthCreds>>>) -> impl Fn(&OAuthCreds) {
        let store = store.clone();
        move |c: &OAuthCreds| *store.lock().unwrap() = Some(c.clone())
    }

    fn fresh_creds(access: &str) -> OAuthCreds {
        OAuthCreds {
            access: access.to_string(),
            refresh: "refresh-tok-2".to_string(),
            expires_ms: now_ms() + 3_600_000,
            account_id: Some("acct".to_string()),
        }
    }

    #[tokio::test]
    async fn coordinator_single_flights_concurrent_refreshes() {
        let coord = new_coord();
        let store = expired_with_refresh();
        let calls = Arc::new(AtomicUsize::new(0));

        let mk = || {
            let refresh = {
                let calls = calls.clone();
                move |_rt: String, _prev: Option<String>| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(fresh_creds("fresh"))
                    }
                }
            };
            coordinated_access_core(&coord, store_load(&store), store_persist(&store), refresh)
        };

        let (a, b, c) = tokio::join!(mk(), mk(), mk());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "concurrent callers share ONE refresh"
        );
        for r in [a, b, c] {
            assert_eq!(r.unwrap().access, "fresh");
        }
    }

    #[tokio::test]
    async fn coordinator_prefers_newer_browser_install_over_refresh_output() {
        let coord = new_coord();
        let store = expired_with_refresh();

        let refresh = {
            let store = store.clone();
            move |_rt: String, _prev: Option<String>| {
                let store = store.clone();
                async move {
                    // A browser login lands a NEWER credential mid-refresh.
                    *store.lock().unwrap() = Some(fresh_creds("browser-installed"));
                    // Our (now stale) refresh returns a different token.
                    Ok(fresh_creds("stale-refresh-output"))
                }
            }
        };
        let got =
            coordinated_access_core(&coord, store_load(&store), store_persist(&store), refresh)
                .await
                .unwrap();
        assert_eq!(
            got.access, "browser-installed",
            "a newer browser-installed credential wins over stale refresh output"
        );
    }

    #[tokio::test]
    async fn cancelled_refresher_clears_the_gate() {
        let coord = new_coord();
        let store = expired_with_refresh();

        // First caller's refresh never completes; force-cancel via timeout.
        let never = coordinated_access_core(
            &coord,
            store_load(&store),
            store_persist(&store),
            |_rt: String, _prev: Option<String>| std::future::pending::<Result<OAuthCreds>>(),
        );
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(20), never).await;
        assert!(timed_out.is_err(), "the refresher was cancelled");

        // The RAII guard must have cleared `refreshing`: a new caller can refresh.
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh = {
            let calls = calls.clone();
            move |_rt: String, _prev: Option<String>| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(fresh_creds("after-cancel"))
                }
            }
        };
        let got =
            coordinated_access_core(&coord, store_load(&store), store_persist(&store), refresh)
                .await
                .unwrap();
        assert_eq!(got.access, "after-cancel");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "gate was not wedged");
    }

    #[tokio::test]
    async fn coordinator_returns_valid_credential_without_refreshing() {
        let coord = new_coord();
        let store = Arc::new(Mutex::new(Some(fresh_creds("already-valid"))));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh = {
            let calls = calls.clone();
            move |_rt: String, _prev: Option<String>| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(fresh_creds("should-not-run"))
                }
            }
        };
        let got =
            coordinated_access_core(&coord, store_load(&store), store_persist(&store), refresh)
                .await
                .unwrap();
        assert_eq!(got.access, "already-valid");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no refresh when still valid"
        );
    }

    #[tokio::test]
    async fn coordinated_access_refuses_untrusted_kind_or_endpoint() {
        use crate::{CHATGPT_CODEX_BASE_URL, ResolvedProviderKind as PK};
        // Wrong kind (custom/built-in) or wrong endpoint → refuse before any I/O.
        assert!(
            coordinated_oauth_access(PK::Custom, CHATGPT_CODEX_BASE_URL)
                .await
                .is_err()
        );
        assert!(
            coordinated_oauth_access(PK::BuiltIn, CHATGPT_CODEX_BASE_URL)
                .await
                .is_err()
        );
        assert!(
            coordinated_oauth_access(PK::ChatGptOAuth, "https://evil.example/v1")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn await_oauth_code_within_honours_its_deadline() {
        // Bind a real port; no callback ever arrives, so only the caller-chosen
        // deadline ends the wait — proving the timeout is parameterized (ChatGPT
        // passes the 60-minute backstop, OpenRouter the 5-minute default).
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let res = await_oauth_code_within(port, "state", Duration::from_millis(120)).await;
        assert!(res.is_err(), "expired deadline yields a timeout error");
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn wrong_state_callback_cannot_end_the_real_login() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(async move { accept_callback(&listener, "right").await });

        for target in [
            "/auth/callback?code=forged&state=wrong",
            "/auth/callback?error=access_denied&state=wrong",
            "/auth/callback?code=&state=right",
        ] {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
        }

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /auth/callback?code=real&state=right HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        assert_eq!(waiter.await.unwrap().unwrap(), "real");
    }

    #[tokio::test]
    async fn oauth_http_client_has_a_bounded_timeout() {
        // Regression: `post_token`/`openrouter_exchange` used to build clients
        // via `reqwest::Client::new()`, which has NO total-request timeout — a
        // black-holed network mid-refresh would wedge the single-flight
        // coordinator (`coordinated_oauth_access`) forever, parking every
        // caller. Pin the constant and confirm the fallible builder succeeds.
        assert_eq!(OAUTH_HTTP_TIMEOUT, Duration::from_secs(30));
        oauth_http_client().expect("the OAuth HTTP client builds");

        // Prove the underlying mechanism (`.timeout(...)` on the client) really
        // bounds a hung request rather than hanging forever: a listener that
        // accepts the connection but never responds must still fail fast.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Accept and hold the connection open without ever responding.
            if let Ok((stream, _)) = listener.accept() {
                std::mem::forget(stream);
            }
        });
        let short_timeout_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let res = short_timeout_client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await;
        let err = res.expect_err("a black-holed server must time out, not hang forever");
        assert!(err.is_timeout(), "got: {err}");
    }

    #[test]
    fn sanitized_token_error_never_echoes_body_secrets() {
        let body = r#"{"error":"invalid_grant","access_token":"SENTINEL_ACCESS","refresh_token":"SENTINEL_REFRESH"}"#;
        let msg = sanitized_token_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(
            msg.contains("invalid_grant"),
            "surfaces the safe error code"
        );
        assert!(!msg.contains("SENTINEL_ACCESS"));
        assert!(!msg.contains("SENTINEL_REFRESH"));
        // Non-JSON body is not echoed at all.
        let msg2 = sanitized_token_error(
            reqwest::StatusCode::BAD_REQUEST,
            "authorization code SENTINEL_CODE verifier SENTINEL_VERIFIER",
        );
        assert!(!msg2.contains("SENTINEL_CODE"));
        assert!(!msg2.contains("SENTINEL_VERIFIER"));
    }

    // ── encoding helpers ─────────────────────────────────────────────────────

    #[test]
    fn form_encode_matches_urlsearchparams() {
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("http://x/y"), "http%3A%2F%2Fx%2Fy");
        assert_eq!(form_encode("keep-._*"), "keep-._*");
    }
}
