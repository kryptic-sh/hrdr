//! `HRDR_MODELS_PATH` pins the models.dev catalog to a local file: the lookup
//! reads it and never touches the network.
//!
//! Its own integration binary because it sets process-global env vars, which
//! would race the unit tests in the library's test binary. One test, not two,
//! for the same reason: cargo runs a binary's tests on parallel threads.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

/// A pinned catalog answers every lookup, and a missing one yields `None`
/// rather than falling through to a fetch — the whole point of pinning.
#[tokio::test]
async fn a_pinned_catalog_is_read_without_fetching() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.json");
    std::fs::write(
        &path,
        r#"{
             "opencode-go": {"models": {"deepseek-v4-flash": {"limit": {"context": 1000000}}}},
             "cortecs":     {"models": {"deepseek-v4-flash": {"limit": {"context": 1048576}}}}
           }"#,
    )
    .unwrap();

    // SAFETY: this binary runs only this test, and nothing else writes these.
    // The dead fetch address means an ignored pin fails the test instead of
    // silently reaching the real models.dev.
    unsafe {
        std::env::set_var("HRDR_MODELS_PATH", &path);
        std::env::set_var("HRDR_MODELS_URL", "http://127.0.0.1:1");
    }

    let window = |p: Option<&'static str>, m: &'static str| hrdr_llm::catalog::context_window(p, m);

    assert_eq!(
        window(Some("opencode-go"), "deepseek-v4-flash").await,
        Some(1_000_000),
        "the configured provider's own window"
    );
    assert_eq!(
        window(Some("cortecs"), "deepseek-v4-flash").await,
        Some(1_048_576),
    );
    // No provider: the smallest window on offer, never the largest.
    assert_eq!(
        window(None, "deepseek-v4-flash").await,
        Some(1_000_000),
        "the conservative choice"
    );
    // A model the catalog doesn't carry.
    assert_eq!(window(None, "no-such-model").await, None);

    // A pinned path that doesn't exist: `None`, and still no fetch.
    unsafe { std::env::set_var("HRDR_MODELS_PATH", "/nonexistent/models.json") };
    assert_eq!(window(None, "deepseek-v4-flash").await, None);
}
