//! The `/login` setup wizard: pick a provider, enter an API key, save it to the
//! dedicated credential store ([`hrdr_agent::save_auth_token`]), and make it the
//! default. Shared by both frontends — each keeps an `Option<LoginWizard>` in a
//! modal slot and, while it's `Some`, routes every submitted line to
//! [`LoginWizard::step`] instead of the model or the slash dispatcher.

use crate::commands::{CommandHost, apply_provider};

/// A running `/login` conversation. Cloneable so the GUI can hold it in a
/// reactive signal.
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
        "openrouter" => "OpenRouter",
        "claude" => "Anthropic (Claude)",
        "local" => "self-hosted, no key",
        _ => "",
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
    fn provider_prompt_lists_all_builtins() {
        let p = provider_prompt();
        for name in hrdr_agent::BUILTIN_PROVIDERS {
            assert!(p.contains(name), "prompt should mention {name}");
        }
        assert!(p.contains("/cancel"), "prompt should note how to abort");
    }
}
