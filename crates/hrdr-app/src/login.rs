//! The `/login` setup wizard: pick a provider, enter an API key, save it to the
//! dedicated credential store ([`hrdr_agent::save_auth_token`]), and make it the
//! default. Shared by both frontends — each keeps an `Option<LoginWizard>` in a
//! modal slot and, while it's `Some`, routes every submitted line to
//! [`LoginWizard::step`] instead of the model or the slash dispatcher.

use crate::commands::{
    BrowserLoginOutcome, BrowserLoginStart, CommandHost, apply_provider_or_pick,
};

/// A running `/login` conversation. Cloneable so a frontend can hold it in
/// whatever state cell it uses.
#[derive(Clone)]
pub struct LoginWizard {
    step: Step,
}

#[derive(Clone)]
enum Step {
    /// Waiting for the provider pick (number or name).
    Provider,
    /// Waiting for the API key for the chosen provider.
    Key { name: String },
}

/// What a wizard picker line resolved to.
enum ProviderPick {
    /// A 1-based index into the CHOICE list — carries the row (and its route).
    Choice(LoginProviderChoice),
    /// A free-form provider name (lower-cased): a built-in typed by name, or a
    /// custom `[providers.<name>]`. Its route is derived when picked.
    Name(String),
}

/// Resolve a wizard picker line against the CHOICE list: a valid 1-based index
/// selects that row (so its explicit [`LoginRoute`] is carried, distinguishing the
/// two rows of a dual-auth provider); anything else is a free-form provider name.
///
/// Indexing the choice list — not `BUILTIN_PROVIDERS` — is what keeps the number a
/// user sees in the prompt aligned with the row it selects now that `openai` and
/// `openrouter` each contribute two rows.
fn parse_provider_pick(line: &str, choices: &[LoginProviderChoice]) -> ProviderPick {
    if let Ok(n) = line.parse::<usize>()
        && (1..=choices.len()).contains(&n)
    {
        return ProviderPick::Choice(choices[n - 1].clone());
    }
    ProviderPick::Name(line.to_ascii_lowercase())
}

/// Friendly label for a built-in provider name (used by the key-entry warning
/// and the browser-open copy — the picker rows carry their own labels, see
/// [`login_provider_choices`]).
fn provider_label(name: &str) -> &'static str {
    match name {
        "zen" => "OpenCode Zen",
        "go" => "OpenCode Go",
        "openai" => "OpenAI",
        "openrouter" => "OpenRouter",
        "claude" => "Anthropic (Claude)",
        "chatgpt" | "codex" | "openai-oauth" => "ChatGPT subscription",
        "local" => "self-hosted, no key",
        _ => "",
    }
}

/// Whether `name` authenticates via an OAuth browser flow rather than a pasted
/// API key. The ChatGPT aliases are owned by `hrdr_agent`, not re-listed here.
fn is_oauth_login(name: &str) -> bool {
    name == "openrouter" || hrdr_agent::is_chatgpt_provider_name(name)
}

/// Milliseconds since the Unix epoch, for OAuth token expiry.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One login ROUTE the flow offers (one modal picker row). A provider can expose
/// more than one — `openai` and `openrouter` each get a key row and a browser
/// row — so the row carries its own [`route`](Self::route): the picker dispatches
/// on the chosen row, never by re-deriving from the (shared) `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProviderChoice {
    /// The provider this row configures. Not unique across rows: both `openai`
    /// rows carry `"openai"`, distinguished only by [`route`](Self::route).
    pub name: String,
    /// Friendly label ("OpenAI (API key)", "ChatGPT subscription (browser login)", …).
    pub label: String,
    /// How this row authenticates: "browser login", "API key", or "no key needed".
    pub detail: String,
    /// The login flow this row drives — carried explicitly so the two rows of a
    /// dual-auth provider dispatch differently despite the shared `name`.
    pub route: LoginRoute,
}

/// The built-in providers as login choices, in registry order. `openai` and
/// `openrouter` each emit TWO rows (an API-key row and a browser-login row); every
/// other built-in emits one (keyless for a non-remote endpoint, key otherwise).
pub fn login_provider_choices() -> Vec<LoginProviderChoice> {
    let mut out = Vec::new();
    for name in hrdr_agent::BUILTIN_PROVIDERS {
        match *name {
            // OpenAI: paste an API key (standard endpoint) OR a ChatGPT
            // subscription browser login (Codex OAuth). Same provider slot; one
            // credential replaces the other at resolve time (key beats OAuth).
            "openai" => {
                out.push(LoginProviderChoice {
                    name: "openai".to_string(),
                    label: "OpenAI (API key)".to_string(),
                    detail: "API key".to_string(),
                    route: LoginRoute::Key,
                });
                out.push(LoginProviderChoice {
                    name: "openai".to_string(),
                    label: "ChatGPT subscription (browser login)".to_string(),
                    detail: "browser login".to_string(),
                    route: LoginRoute::Browser,
                });
            }
            // OpenRouter: paste an API key OR a browser login that MINTS an API
            // key (PKCE) — both land as a key in the `openrouter` slot.
            "openrouter" => {
                out.push(LoginProviderChoice {
                    name: "openrouter".to_string(),
                    label: "OpenRouter (API key)".to_string(),
                    detail: "API key".to_string(),
                    route: LoginRoute::Key,
                });
                out.push(LoginProviderChoice {
                    name: "openrouter".to_string(),
                    label: "OpenRouter (browser login)".to_string(),
                    detail: "browser login".to_string(),
                    route: LoginRoute::Browser,
                });
            }
            other => {
                let keyless = hrdr_agent::builtin_provider(other).is_some_and(|p| !p.remote);
                out.push(LoginProviderChoice {
                    name: other.to_string(),
                    label: provider_label(other).to_string(),
                    detail: if keyless { "no key needed" } else { "API key" }.to_string(),
                    route: if keyless {
                        LoginRoute::Keyless
                    } else {
                        LoginRoute::Key
                    },
                });
            }
        }
    }
    out
}

/// Case-insensitive fuzzy filter over login choices (name + label + detail).
pub fn filter_login_providers(choices: &[LoginProviderChoice], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..choices.len()).collect();
    }
    choices
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = format!("{} {} {}", c.name, c.label, c.detail).to_lowercase();
            crate::is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

/// Outcome of picking a provider in the login flow.
pub enum LoginPick {
    /// The flow finished (keyless endpoint applied, OAuth flow launched, or
    /// the pick failed with a message already shown).
    Done,
    /// A remote key-based provider: prompt for its API key next.
    NeedsKey { name: String },
}

/// How a picked provider authenticates — decided by the RESOLVED trust kind, not
/// the provider spelling, so a `Custom` shadow named `openrouter`/`chatgpt` is
/// routed to key entry, never a browser flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginRoute {
    /// Trusted ChatGPT OAuth, or the built-in OpenRouter: a browser login.
    Browser,
    /// A keyless self-hosted endpoint: apply directly, no key.
    Keyless,
    /// A remote key-based provider (including any custom shadow): needs a key.
    Key,
}

/// Route a resolved provider to its login flow by trust kind.
///
/// - `ChatGptOAuth` (built-in ChatGPT only) → browser.
/// - `BuiltIn` + an OAuth name (`openrouter`) → browser. `is_oauth_login` is
///   name-based and true for a custom `openrouter` shadow too, so the `BuiltIn`
///   kind is what distinguishes the built-in from a shadow.
/// - keyless (`remote == false`) → keyless.
/// - everything else (remote key providers, all `Custom` shadows) → key.
pub fn login_route(name: &str, resolved: &hrdr_agent::ResolvedProvider) -> LoginRoute {
    use hrdr_agent::ResolvedProviderKind as K;
    match resolved.kind {
        K::ChatGptOAuth => LoginRoute::Browser,
        K::BuiltIn if is_oauth_login(name) => LoginRoute::Browser,
        _ if !resolved.remote => LoginRoute::Keyless,
        _ => LoginRoute::Key,
    }
}

/// Act on a provider pick BY NAME — the free-form typed path (a wizard line that
/// isn't a picker index), where there is no row to carry a route, so the route is
/// derived from the resolved trust kind ([`login_route`]). Launch the OAuth flow,
/// apply a keyless endpoint, or report that an API key is needed next.
pub fn login_pick_provider(name: &str, host: &mut dyn CommandHost) -> LoginPick {
    let name = name.to_ascii_lowercase();
    let Some(p) = host.resolve_provider(&name) else {
        host.info(format!(
            "unknown provider '{name}' — pick a built-in or configured name."
        ));
        return LoginPick::Done;
    };
    login_dispatch(&name, login_route(&name, &p), host)
}

/// Act on a picked CHOICE by its explicit [`route`](LoginProviderChoice::route) —
/// the picker path. This is the ONLY way to drive a dual-auth provider correctly:
/// its two rows share a `name` and differ only by route, so re-deriving the route
/// from the name (as [`login_pick_provider`] must, having only a name) would
/// collapse them. Launch the OAuth flow, apply a keyless endpoint, or report that
/// an API key is needed next.
pub fn login_pick_choice(choice: &LoginProviderChoice, host: &mut dyn CommandHost) -> LoginPick {
    let name = choice.name.to_ascii_lowercase();
    if host.resolve_provider(&name).is_none() {
        host.info(format!(
            "unknown provider '{name}' — pick a built-in or configured name."
        ));
        return LoginPick::Done;
    }
    login_dispatch(&name, choice.route, host)
}

/// Dispatch a resolved `(name, route)` to its login flow — the shared core of
/// [`login_pick_provider`] and [`login_pick_choice`].
fn login_dispatch(name: &str, route: LoginRoute, host: &mut dyn CommandHost) -> LoginPick {
    match route {
        // Browser login: launch it (the wizard/non-TUI path persists + reports on
        // completion; the TUI intercepts this route before calling here and manages
        // its own typed pending state instead).
        LoginRoute::Browser => {
            start_oauth_login(name, host);
            LoginPick::Done
        }
        // A keyless (self-hosted) endpoint needs no API key — apply and finish.
        LoginRoute::Keyless => {
            apply_keyless(name, host);
            LoginPick::Done
        }
        LoginRoute::Key => LoginPick::NeedsKey {
            name: name.to_string(),
        },
    }
}

/// Apply a keyless (self-hosted) endpoint and persist it as the default provider.
fn apply_keyless(name: &str, host: &mut dyn CommandHost) {
    match apply_provider_or_pick(host, name) {
        Ok(p) => {
            host.persist_setting("provider", hrdr_agent::ConfigValue::Str(name));
            host.info(format!(
                "✓ using {name} ({}). No API key needed; set as your default provider.",
                p.base_url
            ));
        }
        // A provider that can't name a model has already opened the picker
        // (`apply_provider_or_pick`); anything else is a real failure.
        Err(e) => host.info(e.to_string()),
    }
}

/// The plaintext-storage warning shown before the key is entered.
pub fn login_key_warning(name: &str) -> String {
    format!(
        "Enter your API key for {name} ({}).\n⚠ It will be saved in PLAINTEXT at {} — anyone \
         who can read that file can use the key.",
        provider_label(name),
        auth_location(),
    )
}

/// Save the entered key and switch the live session to `name` (persisting it
/// as the default provider). Shared by the wizard and the TUI's login modal.
pub fn login_enter_key(name: &str, key: &str, host: &mut dyn CommandHost) {
    // Save first so the credential survives even if the live switch races a
    // busy turn; report the exact path back to the user.
    let saved = match hrdr_agent::save_auth_token(name, key) {
        Ok(path) => path.display().to_string(),
        Err(e) => {
            host.info(format!("couldn't save the API key: {e}"));
            return;
        }
    };
    match apply_provider_or_pick(host, name) {
        Ok(p) => {
            host.persist_setting("provider", hrdr_agent::ConfigValue::Str(name));
            host.info(format!(
                "✓ logged in to {name} ({}). Key saved to {saved}; set as your default \
                 provider.",
                p.base_url
            ));
        }
        // `NeedsModel` is not a failure — the picker is already open on this
        // provider's models, and the key is saved either way.
        Err(crate::ProviderSwitchError::NeedsModel { .. }) => {
            host.persist_setting("provider", hrdr_agent::ConfigValue::Str(name));
            host.info(format!("✓ logged in to {name}. Key saved to {saved}."));
        }
        Err(e) => host.info(format!("key saved to {saved}, but the switch failed: {e}")),
    }
}

/// The provider-picker prompt (numbered login rows + free-form name). The numbers
/// index the CHOICE list, so `openai`/`openrouter` each show their two rows.
fn provider_prompt() -> String {
    let mut s = String::from("🔑 /login — pick a login method:\n");
    for (i, c) in login_provider_choices().iter().enumerate() {
        s.push_str(&format!("  {}. {} — {}\n", i + 1, c.label, c.detail));
    }
    s.push_str("Type a number or a provider name. /cancel to abort.");
    s
}

/// Where credentials are written, for the on-wizard warning.
fn auth_location() -> String {
    hrdr_agent::auth_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the hrdr config directory".to_string())
}

impl LoginWizard {
    /// Whether the wizard is currently waiting for the API key line: the
    /// frontend should mask the input pane while this is `true` (the key
    /// itself is already kept out of history/transcript/session — see
    /// [`Self::enter_key`] — but it was still fully visible on screen as
    /// typed).
    pub fn wants_secret_input(&self) -> bool {
        matches!(self.step, Step::Key { .. })
    }

    /// Begin the wizard: emit the provider picker and return the initial state.
    pub fn start(host: &mut dyn CommandHost) -> Self {
        host.info(provider_prompt());
        Self {
            step: Step::Provider,
        }
    }

    /// Feed one submitted line to the wizard. Returns `true` when the flow is
    /// finished (completed or cancelled) and the frontend should drop its modal
    /// slot; `false` to keep capturing input.
    pub fn step(&mut self, line: &str, host: &mut dyn CommandHost) -> bool {
        let line = line.trim();
        if line.eq_ignore_ascii_case("/cancel") || line.eq_ignore_ascii_case("cancel") {
            host.info("login cancelled.".to_string());
            return true;
        }
        match &self.step {
            Step::Provider => self.pick_provider(line, host),
            Step::Key { name } => {
                let name = name.clone();
                self.enter_key(&name, line, host)
            }
        }
    }

    /// Provider step: resolve the pick against the choice list (a number carries
    /// the row's route; a name derives it), skip the key for a keyless endpoint or
    /// launch the browser flow, else advance to the key prompt.
    fn pick_provider(&mut self, line: &str, host: &mut dyn CommandHost) -> bool {
        if line.is_empty() {
            host.info("pick a number or a provider name, or /cancel.".to_string());
            return false;
        }
        let pick = match parse_provider_pick(line, &login_provider_choices()) {
            // A numbered row carries its own route — dispatch on that.
            ProviderPick::Choice(choice) => login_pick_choice(&choice, host),
            // A typed name derives its route from the resolved trust kind.
            ProviderPick::Name(name) => {
                if host.resolve_provider(&name).is_none() {
                    host.info(format!(
                        "unknown provider '{name}' — pick a number, or a built-in / configured \
                         name."
                    ));
                    return false;
                }
                login_pick_provider(&name, host)
            }
        };
        match pick {
            LoginPick::Done => true,
            LoginPick::NeedsKey { name } => {
                host.info(format!(
                    "{}\nPaste the key, or /cancel to abort.",
                    login_key_warning(&name)
                ));
                self.step = Step::Key { name };
                false
            }
        }
    }

    /// Key step: save the credential, switch the live session, and persist the
    /// provider as the default.
    fn enter_key(&mut self, name: &str, line: &str, host: &mut dyn CommandHost) -> bool {
        if line.is_empty() {
            host.info("paste your API key, or /cancel to abort.".to_string());
            return false;
        }
        login_enter_key(name, line, host);
        true
    }
}

/// Launch a browser OAuth login for `name`, returning a [`BrowserLoginStart`]
/// whose `future` performs ONLY the callback + token exchange + credential save
/// (no provider switch, no default persistence, no UI). The URL is printed +
/// opened here (via `open_browser`) and deliberately not carried in the returned
/// value. `login_id` lets the caller reject a stale/duplicate login's result.
///
/// `None` only when `name` is not a browser-login provider (caller should have
/// routed via the chosen row's [`LoginRoute`] first; see [`browser_login_provider`]).
pub fn browser_login_start(
    name: &str,
    login_id: u64,
    host: &mut dyn CommandHost,
) -> Option<BrowserLoginStart> {
    // A browser login has only two shapes, keyed off the target slot.
    let target = browser_login_provider(name)?;
    let (verifier, challenge) = hrdr_agent::generate_pkce();

    if target == "openrouter" {
        let label = "OpenRouter";
        // OpenRouter's OAuth PKCE flow carries `state` in the callback URL and
        // echoes it back with `code` — mint one for CSRF defence, so a local
        // prober can't inject a forged callback with an attacker's code.
        const PORT: u16 = 1456;
        let state = hrdr_agent::generate_state();
        let callback = hrdr_agent::openrouter_callback_url(PORT, &state);
        let url = hrdr_agent::openrouter_authorize_url(&callback, &challenge);
        open_browser(&url, label, "5 minutes", host);
        let future = Box::pin(async move {
            let (token_saved, error) =
                match openrouter_exchange_and_save(PORT, &verifier, &state).await {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                };
            BrowserLoginOutcome {
                login_id,
                provider: "openrouter".to_string(),
                token_saved,
                error,
            }
        });
        return Some(BrowserLoginStart {
            login_id,
            provider: "openrouter".to_string(),
            future,
        });
    }

    // The merged `openai` slot (target == "openai"): a ChatGPT (Codex)
    // subscription login. The whole callback+exchange+save is wrapped in the
    // 60-minute backstop (not the 5-minute OpenRouter deadline).
    let label = "ChatGPT subscription";
    let state = hrdr_agent::generate_state();
    let redirect = hrdr_agent::OPENAI_REDIRECT_URI.to_string();
    let url = hrdr_agent::openai_authorize_url(&redirect, &challenge, &state);
    host.info(
        "⚠ This signs in with your ChatGPT subscription for use in a third-party tool.".to_string(),
    );
    open_browser(&url, label, "60 minutes", host);
    let future = Box::pin(async move {
        let flow = chatgpt_exchange_and_save(&verifier, &state, &redirect);
        let (token_saved, error) =
            match tokio::time::timeout(hrdr_agent::CHATGPT_LOGIN_BACKSTOP, flow).await {
                Ok(Ok(())) => (true, None),
                Ok(Err(e)) => (false, Some(e.to_string())),
                Err(_) => (false, Some("ChatGPT login timed out".to_string())),
            };
        // The OAuth credential is stored in — and resolved from — the merged
        // `openai` slot, so the login outcome (and the switch it drives) targets
        // `openai`, not the old `chatgpt` spelling.
        BrowserLoginOutcome {
            login_id,
            provider: "openai".to_string(),
            token_saved,
            error,
        }
    });
    Some(BrowserLoginStart {
        login_id,
        provider: "openai".to_string(),
        future,
    })
}

/// The provider slot a browser login targets, or `None` when `name` is not a
/// browser-login provider (the caller should have routed via the chosen row's
/// [`LoginRoute`] first; this is also the guard that keeps an unexpected name from
/// silently launching an OAuth flow):
///
/// * `openrouter` → the `openrouter` key slot (PKCE mints an API key);
/// * `openai` (and the `chatgpt`/`codex`/`openai-oauth` aliases, for a typed
///   name) → the merged `openai` OAuth slot (the Codex subscription flow).
fn browser_login_provider(name: &str) -> Option<&'static str> {
    if name == "openrouter" {
        Some("openrouter")
    } else if name == "openai" || hrdr_agent::is_chatgpt_provider_name(name) {
        Some("openai")
    } else {
        None
    }
}

/// A sanitized completion line for a finished browser login, and (on success)
/// persist the provider as the default. Shared by the non-TUI wizard path; the
/// TUI additionally performs a live switch + model refresh.
pub fn browser_login_completion_line(outcome: &BrowserLoginOutcome) -> String {
    if outcome.token_saved {
        // Seed a usable default model before persisting, so the next start (this
        // path does no live switch) lands on a talkable model rather than stalling.
        record_oauth_default_model(&outcome.provider);
        let _ = hrdr_agent::persist_setting(
            "provider",
            hrdr_agent::ConfigValue::Str(&outcome.provider),
        );
        match outcome.provider.as_str() {
            "openrouter" => "✓ logged in to OpenRouter. Key saved and set as your default \
                             — pick a model with /model to use it now."
                .to_string(),
            _ => format!(
                "✓ signed in with ChatGPT. Tokens saved and set as your default (model \
                 {}) — /model to switch models.",
                hrdr_agent::CHATGPT_DEFAULT_MODEL
            ),
        }
    } else {
        format!(
            "{} login failed: {}",
            outcome.provider,
            outcome.error.as_deref().unwrap_or("unknown error")
        )
    }
}

/// Seed the post-login default model for a browser login that landed OAuth
/// credentials in the merged `openai` slot. That provider declares no default
/// model, and a fresh ChatGPT OAuth login has none recorded on it either — so
/// without this the provider switch stalls on `NeedsModel` and the session is left
/// pointed at a provider it can't talk to. Records the ChatGPT subscription
/// default ([`hrdr_agent::CHATGPT_DEFAULT_MODEL`], `gpt-5.5`) as the model last
/// used on `openai`, so the switch (and the next start) lands on a talkable model.
///
/// Only `openai` (OAuth) gets a seeded default: OpenRouter mints an API key and
/// keeps the pick-a-model prompt, matching how its key-entry row lands.
pub fn record_oauth_default_model(provider: &str) {
    if provider != "openai" {
        return;
    }
    if let Ok(r) = hrdr_agent::ModelRef::new(
        hrdr_agent::ProviderName::new("openai"),
        hrdr_agent::CHATGPT_DEFAULT_MODEL,
    ) {
        hrdr_agent::record_last_model(&r);
    }
}

/// Non-TUI browser login: launch it and post the sanitized completion line when
/// the exchange/save future resolves. The TUI does not use this — it owns the
/// typed pending state and the live switch.
fn start_oauth_login(name: &str, host: &mut dyn CommandHost) -> bool {
    let Some(start) = browser_login_start(name, 0, host) else {
        return true;
    };
    host.spawn_line(Box::pin(async move {
        let outcome = start.future.await;
        browser_login_completion_line(&outcome)
    }));
    true
}

/// Print the authorize URL (a fallback if the browser can't open) and launch it.
///
/// `deadline` is the flow's own wait limit — the flows differ (OpenRouter holds
/// the 5-minute callback deadline, ChatGPT the 60-minute backstop), so it is
/// passed in rather than hardcoded into copy that would be wrong for one of them.
/// Esc always abandons the wait; `/cancel` additionally works in the typed
/// wizard, but the login modal swallows keys other than Esc, so Esc is what the
/// copy leads with.
fn open_browser(url: &str, label: &str, deadline: &str, host: &mut dyn CommandHost) {
    host.info(format!(
        "🔑 Opening your browser to authorize {label}…\n\
         If it doesn't open, visit:\n{url}\n\
         Waiting for you to finish in the browser (up to {deadline}). Esc aborts."
    ));
    let _ = crate::open_system_handler(std::path::Path::new(url));
}

/// OpenRouter (5-minute callback deadline): wait for the callback code, exchange
/// it for a normal API key, and save it to the credential store. Exchange/save
/// only — the caller persists the default provider.
async fn openrouter_exchange_and_save(
    port: u16,
    verifier: &str,
    state: &str,
) -> anyhow::Result<()> {
    let code = hrdr_agent::await_oauth_code(port, state).await?;
    let key = hrdr_agent::openrouter_exchange(&code, verifier).await?;
    hrdr_agent::save_auth_token("openrouter", &key)?;
    Ok(())
}

/// ChatGPT: wait for the callback code (no 5-minute inner deadline — the caller
/// wraps the whole flow in the 60-minute backstop), exchange it for the
/// access/refresh token set, and store it in the OAuth credential store under the
/// merged `openai` slot (via the trust-gated [`hrdr_agent::save_oauth_for`], which
/// canonicalizes `ChatGptOAuth` onto `openai` — the same slot resolution reads).
/// Exchange/save only — the caller persists the default + performs the switch.
async fn chatgpt_exchange_and_save(
    verifier: &str,
    state: &str,
    redirect: &str,
) -> anyhow::Result<()> {
    let code = hrdr_agent::await_oauth_code_within(
        hrdr_agent::OPENAI_OAUTH_PORT,
        state,
        hrdr_agent::CHATGPT_LOGIN_BACKSTOP,
    )
    .await?;
    let tokens = hrdr_agent::openai_exchange(&code, redirect, verifier).await?;
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(hrdr_agent::parse_account_id)
        .or_else(|| hrdr_agent::parse_account_id(&tokens.access_token));
    let creds = hrdr_agent::OAuthCreds {
        access: tokens.access_token,
        refresh: tokens.refresh_token,
        expires_ms: now_ms() + tokens.expires_in.unwrap_or(3600) * 1000,
        account_id,
    };
    hrdr_agent::save_oauth_for(
        hrdr_agent::ResolvedProviderKind::ChatGptOAuth,
        "openai",
        &creds,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Route by resolved trust kind, not spelling: this is the isolation the
    /// whole login flow rests on.
    #[test]
    fn login_route_keys_on_trust_kind_not_spelling() {
        use hrdr_agent::AgentConfig;

        let cfg = AgentConfig::default();
        // Built-in ChatGPT → browser.
        assert_eq!(
            login_route("chatgpt", &cfg.resolve_provider("chatgpt").unwrap()),
            LoginRoute::Browser
        );
        // Built-in OpenRouter → browser.
        assert_eq!(
            login_route("openrouter", &cfg.resolve_provider("openrouter").unwrap()),
            LoginRoute::Browser
        );
        // Built-in OpenAI (key provider) → key.
        assert_eq!(
            login_route("openai", &cfg.resolve_provider("openai").unwrap()),
            LoginRoute::Key
        );
        // Keyless local → keyless.
        assert_eq!(
            login_route("local", &cfg.resolve_provider("local").unwrap()),
            LoginRoute::Keyless
        );

        // Custom shadows spelled like OAuth providers must NOT get a browser
        // flow — they resolve to Custom and route to key entry.
        let mut providers = std::collections::HashMap::new();
        for shadow in ["chatgpt", "openrouter", "codex"] {
            providers.insert(
                shadow.to_string(),
                hrdr_agent::ProviderConfig {
                    base_url: "https://evil.example/v1".to_string(),
                    key_env: None,
                    api_key: Some("k".to_string()),
                    model: None,
                    remote: None,
                    context_window: None,
                    headers: std::collections::HashMap::new(),
                    api_version: None,
                },
            );
        }
        let shadowed = AgentConfig {
            providers,
            ..Default::default()
        };
        for shadow in ["chatgpt", "openrouter", "codex"] {
            assert_eq!(
                login_route(shadow, &shadowed.resolve_provider(shadow).unwrap()),
                LoginRoute::Key,
                "custom shadow {shadow} must route to key entry"
            );
        }
    }

    /// The shared (non-TUI) completion line reports a failed browser login for
    /// either provider, without leaking the sanitized error's boundaries.
    #[test]
    fn browser_login_completion_reports_failure_for_both_providers() {
        for provider in ["chatgpt", "openrouter"] {
            let line = browser_login_completion_line(&BrowserLoginOutcome {
                login_id: 0,
                provider: provider.to_string(),
                token_saved: false,
                error: Some("authorization was rejected".to_string()),
            });
            assert!(line.contains(&format!("{provider} login failed")));
            assert!(line.contains("authorization was rejected"));
        }
    }

    /// The wizard's numeric pick indexes the CHOICE list (not `BUILTIN_PROVIDERS`)
    /// and carries the selected row's route — so it can name the right one of a
    /// dual-auth provider's two rows. A non-index line is a free-form name.
    #[test]
    fn provider_pick_indexes_the_choice_list_and_carries_route() {
        let choices = login_provider_choices();
        // Pick each row by its 1-based number and confirm it selects THAT row,
        // route and all — including both `openai`/`openrouter` rows, which a
        // `BUILTIN_PROVIDERS`-indexed pick could never tell apart.
        for (i, expected) in choices.iter().enumerate() {
            match parse_provider_pick(&(i + 1).to_string(), &choices) {
                ProviderPick::Choice(c) => {
                    assert_eq!(&c, expected, "row {} selects itself", i + 1);
                }
                ProviderPick::Name(_) => panic!("a valid index must select a choice row"),
            }
        }
        // A name passes through, lower-cased.
        match parse_provider_pick("OpenAI", &choices) {
            ProviderPick::Name(n) => assert_eq!(n, "openai"),
            ProviderPick::Choice(_) => panic!("a name is not an index"),
        }
        match parse_provider_pick("mycustom", &choices) {
            ProviderPick::Name(n) => assert_eq!(n, "mycustom"),
            ProviderPick::Choice(_) => panic!("a name is not an index"),
        }
        // Out-of-range numbers are treated as a literal name (not an index).
        for out in ["0", "999"] {
            match parse_provider_pick(out, &choices) {
                ProviderPick::Name(n) => assert_eq!(n, out),
                ProviderPick::Choice(_) => panic!("{out} is out of range"),
            }
        }
    }

    /// `openai` and `openrouter` each expose TWO rows (one key, one browser) with
    /// distinguishable labels/details; every other built-in exposes exactly one,
    /// with its existing route.
    #[test]
    fn login_choices_offer_key_and_browser_for_openai_and_openrouter() {
        let choices = login_provider_choices();
        for provider in ["openai", "openrouter"] {
            let rows: Vec<&LoginProviderChoice> =
                choices.iter().filter(|c| c.name == provider).collect();
            assert_eq!(rows.len(), 2, "{provider} offers two login rows");
            let key = rows.iter().find(|c| c.route == LoginRoute::Key);
            let browser = rows.iter().find(|c| c.route == LoginRoute::Browser);
            let (key, browser) = (
                key.unwrap_or_else(|| panic!("{provider} has a key row")),
                browser.unwrap_or_else(|| panic!("{provider} has a browser row")),
            );
            assert_eq!(key.detail, "API key");
            assert_eq!(browser.detail, "browser login");
            assert_ne!(
                key.label, browser.label,
                "{provider} rows are labelled apart"
            );
        }
        // Single-route built-ins keep exactly one row, with their existing route.
        for (provider, route) in [
            ("zen", LoginRoute::Key),
            ("go", LoginRoute::Key),
            ("claude", LoginRoute::Key),
            ("local", LoginRoute::Keyless),
        ] {
            let rows: Vec<&LoginProviderChoice> =
                choices.iter().filter(|c| c.name == provider).collect();
            assert_eq!(rows.len(), 1, "{provider} offers exactly one row");
            assert_eq!(rows[0].route, route, "{provider} route");
        }
    }

    /// A browser login for `openai` (and the ChatGPT aliases a typed name may use)
    /// targets the merged `openai` OAuth slot — the Codex flow; `openrouter`
    /// targets its own key slot — the PKCE flow; a non-browser name has none.
    #[test]
    fn browser_login_targets_the_right_slot() {
        assert_eq!(browser_login_provider("openai"), Some("openai"));
        for alias in ["chatgpt", "codex", "openai-oauth"] {
            assert_eq!(
                browser_login_provider(alias),
                Some("openai"),
                "{alias} folds onto the openai OAuth slot"
            );
        }
        assert_eq!(browser_login_provider("openrouter"), Some("openrouter"));
        for keyed in ["zen", "go", "claude", "local"] {
            assert_eq!(
                browser_login_provider(keyed),
                None,
                "{keyed} has no browser flow"
            );
        }
    }

    /// The picker dispatches on the CHOSEN row's route: a Key row → key entry (for
    /// the exact provider), a Keyless row → applied (its provider-model picker
    /// opens). Driven through [`login_pick_choice`], the picker's entry point.
    #[tokio::test]
    async fn login_pick_choice_routes_by_the_rows_route() {
        let choices = login_provider_choices();
        let key_row = |provider: &str| {
            choices
                .iter()
                .find(|c| c.name == provider && c.route == LoginRoute::Key)
                .cloned()
                .unwrap()
        };

        // Both dual-auth providers' Key rows go to key entry, for THAT provider.
        for provider in ["openai", "openrouter"] {
            let mut host = RouteTestHost::new();
            match login_pick_choice(&key_row(provider), &mut host) {
                LoginPick::NeedsKey { name } => assert_eq!(name, provider),
                LoginPick::Done => panic!("{provider} key row must go to key entry"),
            }
        }

        // The keyless `local` row applies and (declaring no model) opens its
        // provider-scoped model picker — never key entry, never a browser flow.
        let local = choices.iter().find(|c| c.name == "local").cloned().unwrap();
        let mut host = RouteTestHost::new();
        assert!(matches!(
            login_pick_choice(&local, &mut host),
            LoginPick::Done
        ));
        assert_eq!(
            host.model_picker_for.as_deref(),
            Some("local"),
            "a keyless provider with no model opens its model picker"
        );
    }

    /// The frontend masks the input pane only while the wizard is waiting for
    /// the actual API key — not during the provider pick, which is never
    /// secret.
    #[test]
    fn wants_secret_input_only_during_the_key_step() {
        let picking = LoginWizard {
            step: Step::Provider,
        };
        assert!(!picking.wants_secret_input());

        let entering_key = LoginWizard {
            step: Step::Key {
                name: "openai".to_string(),
            },
        };
        assert!(entering_key.wants_secret_input());
    }

    #[test]
    fn provider_prompt_lists_every_login_row() {
        let p = provider_prompt();
        // One numbered line per login row (both `openai`/`openrouter` methods
        // included), so the numbers align with `parse_provider_pick`'s indexing.
        for (i, c) in login_provider_choices().iter().enumerate() {
            let line = format!("  {}. {} — {}", i + 1, c.label, c.detail);
            assert!(p.contains(&line), "prompt should list row: {line}\n{p}");
        }
        assert!(
            p.contains("browser login") && p.contains("API key"),
            "both auth methods are shown"
        );
        assert!(p.contains("/cancel"), "prompt should note how to abort");
    }

    /// A minimal [`CommandHost`] for the routing tests: real provider resolution
    /// (so the built-ins resolve), a recording `begin_model_selector_for`, and a
    /// no-op `persist_setting` (never touch the real config). Everything else is a
    /// harmless stub — proving, by never being hit, that these tests exercise only
    /// the routing they mean to.
    struct RouteTestHost {
        cfg: hrdr_agent::AgentConfig,
        agent: std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>>,
        model: hrdr_agent::ModelRef,
        model_picker_for: Option<String>,
    }

    impl RouteTestHost {
        fn new() -> Self {
            let model: hrdr_agent::ModelRef = "local://test-model".parse().unwrap();
            let agent = hrdr_agent::Agent::new(hrdr_agent::AgentConfig {
                model: model.clone(),
                ..Default::default()
            })
            .unwrap();
            Self {
                cfg: hrdr_agent::AgentConfig::default(),
                agent: std::sync::Arc::new(tokio::sync::Mutex::new(agent)),
                model,
                model_picker_for: None,
            }
        }
    }

    impl CommandHost for RouteTestHost {
        fn info(&mut self, _line: String) {}
        fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
            self.cfg.resolve_provider(name)
        }
        fn begin_model_selector_for(&mut self, provider: &str) {
            self.model_picker_for = Some(provider.to_string());
        }
        // Never write to the real user config from a test.
        fn persist_setting(&mut self, _key: &str, _value: hrdr_agent::ConfigValue) {}
        fn agent(&self) -> std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>> {
            self.agent.clone()
        }
        fn cwd(&self) -> std::path::PathBuf {
            std::env::temp_dir()
        }
        fn base_url(&self) -> String {
            "http://test.invalid".to_string()
        }
        fn model_ref(&self) -> hrdr_agent::ModelRef {
            self.model.clone()
        }
        fn set_model_ref(&mut self, reference: hrdr_agent::ModelRef) {
            self.model = reference;
        }
        fn show_thinking(&self) -> bool {
            false
        }
        fn set_show_thinking(&mut self, _on: bool) {}
        fn clear_conversation(&mut self) {}
        fn session_id(&self) -> Option<String> {
            None
        }
        fn set_session_label(&mut self, _name: String) {}
        fn autosave(&mut self) {}
        fn resume(&mut self, _id: String, _session: crate::Session) {}
        fn copy_to_clipboard(&mut self, _text: &str, _label: &str) -> String {
            String::new()
        }
        fn last_reply(&self) -> Option<String> {
            None
        }
        fn transcript_text(&self) -> String {
            String::new()
        }
        fn nth_message_text(&self, _n: usize) -> Option<String> {
            None
        }
        fn line_poster(&self) -> Box<dyn Fn(crate::commands::LineKind, String) + Send> {
            Box::new(|_, _| {})
        }
        fn is_busy(&self) -> bool {
            false
        }
        fn send_prompt(&mut self, _prompt: String, _show_as_user: bool) {}
        fn set_input(&mut self, _text: String) {}
        fn prepend_input(&mut self, _text: String) {}
        fn insert_input(&mut self, _text: String) {}
        fn set_tool_expansion(&mut self, _mode: crate::commands::ExpandMode) -> String {
            String::new()
        }
        fn start_compaction(&mut self, _instructions: Option<String>) {}
    }
}
