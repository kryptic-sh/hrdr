//! The `/login` setup wizard: pick a provider, enter an API key, save it to the
//! dedicated credential store ([`hrdr_agent::save_auth_token`]), and make it the
//! default. Shared by both frontends — each keeps an `Option<LoginWizard>` in a
//! modal slot and, while it's `Some`, routes every submitted line to
//! [`LoginWizard::step`] instead of the model or the slash dispatcher.

use crate::commands::{CommandHost, apply_provider};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_LOGIN_ID: AtomicU64 = AtomicU64::new(1);

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
/// API key.
fn is_oauth_login(name: &str, kind: hrdr_agent::ResolvedProviderKind) -> bool {
    kind == hrdr_agent::ResolvedProviderKind::ChatGptOAuth
        || (kind == hrdr_agent::ResolvedProviderKind::BuiltIn && name == "openrouter")
}

/// Milliseconds since the Unix epoch, for OAuth token expiry.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        let Some(p) = host.resolve_provider(&name) else {
            host.info(format!(
                "unknown provider '{name}' — pick a number, or a built-in / configured name."
            ));
            return false;
        };
        // OAuth providers log in through the browser, not a pasted key.
        if is_oauth_login(&name, p.kind) {
            return start_oauth_login(&name, host);
        }
        // A keyless (self-hosted) endpoint needs no API key — apply and finish.
        if !p.remote {
            match apply_provider(host, &name, None) {
                Ok(p) => {
                    host.persist_setting("provider", hrdr_agent::ConfigValue::Str(&name));
                    host.info(format!(
                        "✓ using {name} ({}). No API key needed; set as your default provider.",
                        p.base_url
                    ));
                }
                Err(e) => host.info(e),
            }
            return true;
        }
        host.info(format!(
            "Enter your API key for {name} ({}).\n\
             ⚠ It will be saved in PLAINTEXT at {} — anyone who can read that file can use \
             the key.\nPaste the key, or /cancel to abort.",
            provider_label(&name),
            auth_location(),
        ));
        self.step = Step::Key { name };
        false
    }

    /// Key step: save the credential, switch the live session, and persist the
    /// provider as the default.
    fn enter_key(&mut self, name: &str, line: &str, host: &mut dyn CommandHost) -> bool {
        if line.is_empty() {
            host.info("paste your API key, or /cancel to abort.".to_string());
            return false;
        }
        // Save first so the credential survives even if the live switch races a
        // busy turn; report the exact path back to the user.
        let saved = match hrdr_agent::save_auth_token(name, line) {
            Ok(path) => path.display().to_string(),
            Err(e) => {
                host.info(format!("couldn't save the API key: {e}"));
                return true;
            }
        };
        match apply_provider(host, name, Some(line.to_string())) {
            Ok(p) => {
                host.persist_setting("provider", hrdr_agent::ConfigValue::Str(name));
                host.info(format!(
                    "✓ logged in to {name} ({}). Key saved to {saved}; set as your default \
                     provider.",
                    p.base_url
                ));
            }
            Err(e) => host.info(format!("key saved to {saved}, but the switch failed: {e}")),
        }
        true
    }
}

/// Kick off an OAuth browser login for `name`: print the authorize URL, open the
/// browser, and spawn the flow (callback server → token exchange → save the
/// credential and persist the provider as default). The wizard finishes
/// immediately; the spawned task posts the outcome as a system line.
fn start_oauth_login(name: &str, host: &mut dyn CommandHost) -> bool {
    let (verifier, challenge) = hrdr_agent::generate_pkce();
    let label = provider_label(name);

    if name == "openrouter" {
        // OpenRouter's PKCE flow has no `state`; any local callback port works.
        const PORT: u16 = 1456;
        let callback = format!("http://localhost:{PORT}/auth/callback");
        let url = hrdr_agent::openrouter_authorize_url(&callback, &challenge);
        begin_browser_login(name, label, url, host, async move {
            openrouter_oauth_flow(PORT, &verifier).await
        });
        return true;
    }

    // ChatGPT (Codex) subscription login.
    let state = hrdr_agent::generate_state();
    let redirect = hrdr_agent::OPENAI_REDIRECT_URI.to_string();
    let url = hrdr_agent::openai_authorize_url(&redirect, &challenge, &state);
    host.info(
        "This signs in with your ChatGPT subscription for use in a third-party tool.".to_string(),
    );
    begin_browser_login("chatgpt", label, url, host, async move {
        chatgpt_oauth_flow(&verifier, &state, &redirect).await
    });
    true
}

fn begin_browser_login<F>(
    provider: &str,
    label: &str,
    url: String,
    host: &mut dyn CommandHost,
    flow: F,
) where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let login_id = NEXT_LOGIN_ID.fetch_add(1, Ordering::Relaxed);
    let provider = provider.to_string();
    let outcome_provider = provider.clone();
    let label = label.to_string();
    let accepted = host.begin_browser_login(crate::BrowserLoginStart {
        login_id,
        provider,
        authorization_url: url.clone(),
        future: Box::pin(async move {
            match tokio::time::timeout(std::time::Duration::from_secs(5 * 60), flow).await {
                Ok(Ok(())) => crate::BrowserLoginOutcome {
                    login_id,
                    provider: outcome_provider,
                    token_saved: true,
                    error: None,
                },
                Ok(Err(error)) => crate::BrowserLoginOutcome {
                    login_id,
                    provider: outcome_provider,
                    token_saved: false,
                    error: Some(error.to_string()),
                },
                Err(_) => crate::BrowserLoginOutcome {
                    login_id,
                    provider: outcome_provider,
                    token_saved: false,
                    error: Some(format!("{label} authorization timed out")),
                },
            }
        }),
    });
    if accepted {
        let _ = crate::open_system_handler(std::path::Path::new(&url));
    }
}

/// OpenRouter: wait for the callback code, exchange it for a normal API key, and
/// save it to the credential store like any other key.
async fn openrouter_oauth_flow(port: u16, verifier: &str) -> anyhow::Result<()> {
    let code = hrdr_agent::await_oauth_code_unbounded(port, "").await?;
    let key = hrdr_agent::openrouter_exchange(&code, verifier).await?;
    hrdr_agent::save_auth_token("openrouter", &key)?;
    Ok(())
}

/// ChatGPT: wait for the callback code, exchange it for the access/refresh token
/// set, and store it in the OAuth credential store.
async fn chatgpt_oauth_flow(verifier: &str, state: &str, redirect: &str) -> anyhow::Result<()> {
    let code = hrdr_agent::await_oauth_code_unbounded(hrdr_agent::OPENAI_OAUTH_PORT, state).await?;
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
    hrdr_agent::save_oauth_coordinated("chatgpt", creds).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn oauth_login_requires_trusted_builtin_identity() {
        use hrdr_agent::ResolvedProviderKind;
        assert!(is_oauth_login(
            "chatgpt",
            ResolvedProviderKind::ChatGptOAuth
        ));
        assert!(is_oauth_login("openrouter", ResolvedProviderKind::BuiltIn));
        assert!(!is_oauth_login("chatgpt", ResolvedProviderKind::Custom));
        assert!(!is_oauth_login("openrouter", ResolvedProviderKind::Custom));
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
