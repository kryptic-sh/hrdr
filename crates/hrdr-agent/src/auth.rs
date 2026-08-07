//! Raw-API-key access to the unified credential store, kept out of `config.toml`
//! so API keys never land in a file users commit or share. Keys live as `key`
//! entries in `$XDG_CONFIG_HOME/hrdr/auth.json` (`0600` on unix; on Windows no
//! explicit ACL — hrdr relies on the default ACLs of the containing per-user
//! profile directory, which is user-scoped by default). Written by the `/login`
//! wizard, read at startup and on a live provider switch (the `/model` picker or
//! `/login`).
//!
//! The store schema and its locked/atomic read-modify-write live in
//! [`crate::auth_store`]; this module is the key-facing view over it. The
//! atomic-write primitive lives in [`hrdr_llm::fs`] and is re-exported below as
//! [`write_atomic`]; the directory-permission primitive
//! ([`create_dir_owner_only`]) remains here because it is shared by every store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ProviderName;

/// Path to the credential store (`~/.config/hrdr/auth.json`), if `HOME` is set.
pub fn auth_file_path() -> Option<PathBuf> {
    crate::auth_store::store_path()
}

/// All stored `provider → api_key` pairs. Empty when the file is missing or
/// unreadable — credentials are best-effort and never fail a load.
pub fn load_auth_tokens() -> HashMap<String, String> {
    auth_file_path()
        .map(|p| crate::auth_store::load_keys_at(&p))
        .unwrap_or_default()
}

/// The credential-store key for `provider`. OpenCode's endpoints — `zen`, `go`,
/// and their `opencode*` aliases — all authenticate against the same OpenCode
/// account (the same `OPENCODE_API_KEY`), so they share one stored entry
/// (`opencode`): logging in to any of them covers them all. Every other provider
/// keys on its own name.
///
/// One source of truth for the sharing rule: [`ProviderName::auth_key`]. The
/// borrow is returned from `provider` itself for every non-OpenCode name, so a
/// custom provider keeps its own spelling.
pub fn auth_key(provider: &str) -> &str {
    const SHARED: &str = "opencode";
    if ProviderName::new(provider).auth_key() == SHARED {
        SHARED
    } else {
        provider
    }
}

/// The stored API key for `provider`, if any. Looks under the shared
/// [`auth_key`] first, then the raw provider name (covering a key saved before
/// the OpenCode-sharing rule collapsed the aliases onto one slot).
pub fn auth_token(provider: &str) -> Option<String> {
    let tokens = load_auth_tokens();
    tokens
        .get(auth_key(provider))
        .or_else(|| tokens.get(provider))
        .cloned()
}

/// Store `provider`'s `token` in the credential file (creating it, `0600` on
/// unix), preserving any other entries. Saved under the shared [`auth_key`], so
/// the OpenCode endpoints write one entry between them. Returns the file path.
pub fn save_auth_token(provider: &str, token: &str) -> anyhow::Result<PathBuf> {
    let path =
        auth_file_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the auth file"))?;
    crate::auth_store::save_key_at(&path, auth_key(provider), token)?;
    Ok(path)
}

pub use hrdr_llm::fs::write_atomic;

/// Create `dir` (and any missing parents) and, on Unix, tighten it to owner-only
/// (`0700`) so the credential filenames and timestamps it holds aren't
/// world-listable. `dir` is the hrdr config dir, so tightening the whole
/// directory to owner-only is the intended outcome.
///
/// The permission tightening is best-effort: a failure to `set_permissions` must
/// not stop a credential from being saved (the files inside are already `0600`).
pub(crate) fn create_dir_owner_only(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OpenCode-sharing rule ([`auth_key`]) collapses `zen`/`go`/`opencode*`
    /// onto one store slot while every other provider keeps its own name; a key
    /// saved while on `zen` resolves when the session is on `go`. Drives the
    /// real key store (`auth.json`) via [`crate::auth_store`].
    #[test]
    fn opencode_endpoints_share_one_credential_entry() {
        // All the OpenCode aliases collapse to a single store key…
        for name in [
            "zen",
            "go",
            "opencode",
            "opencode-zen",
            "opencode-go",
            "ZEN",
        ] {
            assert_eq!(auth_key(name), "opencode", "{name} → opencode");
        }
        // …while other providers keep their own name.
        assert_eq!(auth_key("openai"), "openai");
        assert_eq!(auth_key("mycustom"), "mycustom");

        // A key saved while on `zen` resolves when the session is on `go`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        crate::auth_store::save_key_at(&path, auth_key("zen"), "sk-opencode").unwrap();
        let tokens = crate::auth_store::load_keys_at(&path);
        assert_eq!(
            tokens.get(auth_key("go")).map(String::as_str),
            Some("sk-opencode"),
            "go finds the credential saved under zen"
        );
    }
}
