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

/// Resolve a picker line to a provider name: a valid 1-based index selects from
/// `builtins`; anything else is lower-cased and taken as a name (so a custom
/// `[providers.<name>]` or a built-in typed by name both work).
fn parse_provider_pick(line: &str, builtins: &[&str]) -> String {
    match line.parse::<usize>() {
        Ok(n) if (1..=builtins.len()).contains(&n) => builtins[n - 1].to_string(),
        _ => line.to_ascii_lowercase(),
    }
}

/// Friendly label for a built-in provider name (for the picker).
fn provider_label(name: &str) -> &'static str {
    match name {
        "zen" => "OpenCode Zen",
        "go" => "OpenCode Go",
        "openai" => "OpenAI",
        "openrouter" => "OpenRouter (browser login)",
        "claude" => "Anthropic (Claude)",
        "chatgpt" | "codex" | "openai-oauth" => "ChatGPT subscription (browser login)",
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

/// One provider the login flow offers (the modal picker's rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProviderChoice {
    /// The name `login_pick_provider` accepts.
    pub name: String,
    /// Friendly label ("OpenCode Zen", …).
    pub label: String,
    /// How it authenticates: "browser login", "API key", or "no key needed".
    pub detail: String,
}

/// The built-in providers as login choices, in registry order.
pub fn login_provider_choices() -> Vec<LoginProviderChoice> {
    hrdr_agent::BUILTIN_PROVIDERS
        .iter()
        .map(|name| {
            let detail = if is_oauth_login(name) {
                "browser login"
            } else if hrdr_agent::builtin_provider(name).is_some_and(|p| !p.remote) {
                "no key needed"
            } else {
                "API key"
            };
            LoginProviderChoice {
                name: (*name).to_string(),
                label: provider_label(name)
                    .trim_end_matches(" (browser login)")
                    .to_string(),
                detail: detail.to_string(),
            }
        })
        .collect()
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

/// Act on a provider pick: launch the OAuth flow, apply a keyless endpoint,
/// or report that an API key is needed next. Shared by the wizard (text
/// frontends) and the TUI's login modal.
pub fn login_pick_provider(name: &str, host: &mut dyn CommandHost) -> LoginPick {
    let name = name.to_ascii_lowercase();
    let Some(p) = host.resolve_provider(&name) else {
        host.info(format!(
            "unknown provider '{name}' — pick a built-in or configured name."
        ));
        return LoginPick::Done;
    };
    match login_route(&name, &p) {
        // Browser login: launch it (the wizard/non-TUI path persists + reports on
        // completion; the TUI manages its own typed pending state).
        LoginRoute::Browser => {
            start_oauth_login(&name, host);
            LoginPick::Done
        }
        // A keyless (self-hosted) endpoint needs no API key — apply and finish.
        LoginRoute::Keyless => {
            match apply_provider_or_pick(host, &name) {
                Ok(p) => {
                    host.persist_setting("provider", hrdr_agent::ConfigValue::Str(&name));
                    host.info(format!(
                        "✓ using {name} ({}). No API key needed; set as your default provider.",
                        p.base_url
                    ));
                }
                // A provider that can't name a model has already opened the picker
                // (`apply_provider_or_pick`); anything else is a real failure.
                Err(e) => host.info(e.to_string()),
            }
            LoginPick::Done
        }
        LoginRoute::Key => LoginPick::NeedsKey { name },
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

/// The provider-picker prompt (numbered built-ins + free-form name).
fn provider_prompt() -> String {
    let mut s = String::from("🔑 /login — pick a provider:\n");
    for (i, name) in hrdr_agent::BUILTIN_PROVIDERS.iter().enumerate() {
        s.push_str(&format!("  {}. {name} — {}\n", i + 1, provider_label(name)));
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

    /// Provider step: resolve the pick, skip the key for a keyless endpoint,
    /// else advance to the key prompt.
    fn pick_provider(&mut self, line: &str, host: &mut dyn CommandHost) -> bool {
        if line.is_empty() {
            host.info("pick a number or a provider name, or /cancel.".to_string());
            return false;
        }
        let name = parse_provider_pick(line, hrdr_agent::BUILTIN_PROVIDERS);
        // A typo'd name still errors inside login_pick_provider, but a valid
        // pick either finishes (keyless/OAuth) or advances to the key step.
        if host.resolve_provider(&name).is_none() {
            host.info(format!(
                "unknown provider '{name}' — pick a number, or a built-in / configured name."
            ));
            return false;
        }
        match login_pick_provider(&name, host) {
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
/// routed via [`login_route`] first).
pub fn browser_login_start(
    name: &str,
    login_id: u64,
    host: &mut dyn CommandHost,
) -> Option<BrowserLoginStart> {
    let (verifier, challenge) = hrdr_agent::generate_pkce();
    let label = provider_label(name);

    if name == "openrouter" {
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

    // Only the ChatGPT aliases reach the Codex flow below; any other name is not
    // a browser-login provider (callers route via `login_route` first, but guard
    // so an unexpected name never silently launches a ChatGPT OAuth flow).
    if !hrdr_agent::is_chatgpt_provider_name(name) {
        return None;
    }

    // ChatGPT (Codex) subscription login. The whole callback+exchange+save is
    // wrapped in the 60-minute backstop (not the 5-minute OpenRouter deadline).
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
        BrowserLoginOutcome {
            login_id,
            provider: "chatgpt".to_string(),
            token_saved,
            error,
        }
    });
    Some(BrowserLoginStart {
        login_id,
        provider: "chatgpt".to_string(),
        future,
    })
}

/// A sanitized completion line for a finished browser login, and (on success)
/// persist the provider as the default. Shared by the non-TUI wizard path; the
/// TUI additionally performs a live switch + model refresh.
pub fn browser_login_completion_line(outcome: &BrowserLoginOutcome) -> String {
    if outcome.token_saved {
        let _ = hrdr_agent::persist_setting(
            "provider",
            hrdr_agent::ConfigValue::Str(&outcome.provider),
        );
        match outcome.provider.as_str() {
            "openrouter" => "✓ logged in to OpenRouter. Key saved and set as your default \
                             — pick a model with /model to use it now."
                .to_string(),
            _ => "✓ signed in with ChatGPT. Tokens saved and set as your default \
                  — pick a model with /model to use it now."
                .to_string(),
        }
    } else {
        format!(
            "{} login failed: {}",
            outcome.provider,
            outcome.error.as_deref().unwrap_or("unknown error")
        )
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
/// access/refresh token set, and store it in the OAuth credential store.
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
    hrdr_agent::save_oauth("chatgpt", &creds)?;
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

    #[test]
    fn provider_pick_parses_number_or_name() {
        let b = &["zen", "openai", "local"];
        // In-range 1-based indices select from the list.
        assert_eq!(parse_provider_pick("1", b), "zen");
        assert_eq!(parse_provider_pick("3", b), "local");
        // A name passes through, lower-cased.
        assert_eq!(parse_provider_pick("OpenAI", b), "openai");
        assert_eq!(parse_provider_pick("mycustom", b), "mycustom");
        // Out-of-range numbers are treated as a literal name (not an index).
        assert_eq!(parse_provider_pick("0", b), "0");
        assert_eq!(parse_provider_pick("9", b), "9");
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
    fn provider_prompt_lists_all_builtins() {
        let p = provider_prompt();
        for name in hrdr_agent::BUILTIN_PROVIDERS {
            assert!(p.contains(name), "prompt should mention {name}");
        }
        assert!(p.contains("/cancel"), "prompt should note how to abort");
    }
}
