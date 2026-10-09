//! The `/model` picker offers every provider the machine is set up for — a
//! subscription login counts, not only an API key.
//!
//! The picker gated built-in providers on an API *key* resolving. A ChatGPT
//! subscription login stores OAuth credentials and no key at all, so a machine
//! signed in to it was offered no `openai` models — unless ChatGPT happened to be
//! the provider already in use, which is added regardless. That is what made the
//! selector look like it only knew about the current provider.
//!
//! Its own test binary on purpose: this writes a credential into the sandboxed
//! `HOME`, and `openai` holding an OAuth credential changes which endpoint that
//! provider resolves to. In the crate's shared unit-test binary that leaked into
//! unrelated tests, which then resolved to the Codex endpoint and failed.

extern crate hrdr_test_support;

use hrdr_agent::{AgentConfig, OAuthCreds, model_choices, save_oauth};

/// A catalog with an `openai` entry, so a listed provider has models to show.
fn pin_catalog() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("models.json");
    std::fs::write(
        &path,
        r#"{"openai":{"name":"OpenAI","models":{"gpt-5":{"name":"GPT-5"}}}}"#,
    )
    .expect("the catalog is written");
    // SAFETY: single-threaded test binary, before the harness starts any thread.
    unsafe { std::env::set_var("HRDR_MODELS_PATH", &path) };
    dir
}

#[test]
fn a_subscription_login_puts_its_provider_in_the_picker() {
    let _catalog = pin_catalog();
    let cfg = AgentConfig::default();
    let providers = |rows: &[hrdr_agent::ModelChoice]| -> Vec<String> {
        let mut names: Vec<String> = rows.iter().map(|c| c.provider.clone()).collect();
        names.sort();
        names.dedup();
        names
    };

    // Nothing is set up for `openai`: no key in the environment, none stored.
    let before = providers(&model_choices(&cfg, None));
    assert!(
        !before.contains(&"openai".to_string()),
        "no credential, no rows: {before:?}"
    );

    // Sign in the way `/login` does for a ChatGPT subscription — OAuth
    // credentials in the canonical slot, and deliberately no API key anywhere.
    save_oauth(
        "openai",
        &OAuthCreds {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            expires_ms: u64::MAX,
            account_id: Some("acct".into()),
        },
        false,
    )
    .expect("the credential is stored");

    let after = model_choices(&cfg, None);
    assert!(
        providers(&after).contains(&"openai".to_string()),
        "an OAuth-authenticated provider must be offered without being the active \
         one: {:?}",
        providers(&after)
    );
    assert!(
        after
            .iter()
            .any(|c| c.provider == "openai" && c.model == "gpt-5"),
        "…with its catalog models, not a bare placeholder: {after:?}"
    );
    assert!(
        hrdr_agent::load_auth_tokens().is_empty(),
        "and it got there with no API key stored — the whole point"
    );
}
