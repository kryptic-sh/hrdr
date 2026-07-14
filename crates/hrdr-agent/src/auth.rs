//! Dedicated credential store, kept out of `config.toml` so API keys never land
//! in a file users commit or share. Plaintext TOML at
//! `$XDG_CONFIG_HOME/hrdr/auth.toml` (`0600` on unix), a flat map of provider
//! name → API key. Written by the `/login` wizard, read at startup and on a
//! live provider switch (the `/model` picker or `/login`).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::{ProviderName, config_dir};

/// Path to the credential store (`~/.config/hrdr/auth.toml`), if `HOME` is set.
pub fn auth_file_path() -> Option<PathBuf> {
    Some(config_dir()?.join("auth.toml"))
}

/// All stored `provider → api_key` pairs. Empty when the file is missing or
/// unreadable — credentials are best-effort and never fail a load.
pub fn load_auth_tokens() -> HashMap<String, String> {
    auth_file_path()
        .map(|p| load_tokens_at(&p))
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
/// [`auth_key`] first, then (for pre-unification stores) the raw provider name.
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
pub fn save_auth_token(provider: &str, token: &str) -> Result<PathBuf> {
    let path =
        auth_file_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the auth file"))?;
    save_token_at(&path, auth_key(provider), token)?;
    Ok(path)
}

/// Parse a credential file at `path` into a `provider → token` map (empty on any
/// read/parse failure). The path-based core of [`load_auth_tokens`].
fn load_tokens_at(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return HashMap::new();
    };
    doc.as_table()
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.as_str()?.to_string())))
        .collect()
}

/// Write `data` to `path` atomically: write to a temp file in the same
/// directory, fsync, then rename over the target — a concurrent reader never
/// sees a partial write. On unix the temp file is created with `0o600`
/// permissions from the start so there is no window where it exists with
/// broader permissions.
///
/// The parent directory must already exist. A rename failure removes the temp
/// so no stray file is left behind.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    // Write to a temp file in the same directory, then rename atomically.
    // tempfile is a dev-dependency only, so we build the temp name manually.
    let tmp = tmp_path(path, parent);

    #[cfg(unix)]
    let create_file = || -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
    };
    #[cfg(not(unix))]
    let create_file = || -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
    };

    // `create_new` guarantees we own `tmp`; a failure here means someone else's
    // temp collided, so we must not clean it up. Everything after gets a
    // cleanup-on-error guard so a failed save never leaves a stray temp behind
    // (notably: a rename that fails still removes the temp we wrote).
    let mut f = create_file()?;
    let result = (|| -> std::io::Result<()> {
        f.write_all(data)?;
        // Flush + fsync so the data is on disk before the rename.
        f.flush()?;
        #[cfg(unix)]
        f.sync_all()?;
        Ok(())
    })();
    drop(f);
    let result = result.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    // No non-unix permission tightening: the Windows read-only *attribute*
    // doesn't restrict reads (access is by ACL) and would make the file
    // un-replaceable by the next atomic rename. Unix already got 0600 above;
    // proper Windows hardening (per-user ACL) is a follow-up.
    result
}

/// Write `provider = "token"` into the file at `path`, preserving other entries
/// and ensuring owner-only (`0600`) permissions on unix. The path-based core of
/// [`save_auth_token`].
///
/// The write is done through [`write_atomic`] — a concurrent reader never sees a
/// partial write.
fn save_token_at(path: &Path, provider: &str, token: &str) -> Result<()> {
    let mut doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .unwrap_or_default();
    doc[provider] = toml_edit::value(token);
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let content = doc.to_string();
    write_atomic(path, content.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// A unique temp-file path inside `parent` (same filesystem as `path`, so the
/// subsequent rename is atomic). The name includes a timestamp and PID so
/// concurrent writes don't collide.
pub(crate) fn tmp_path(path: &Path, parent: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    // Keep the original file stem so temp files are recognisable.
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or(std::borrow::Cow::Borrowed("auth"));
    parent.join(format!(".{stem}.{stamp}.{pid}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        save_token_at(&path, "zen", "sk-zen-1").unwrap();
        save_token_at(&path, "openai", "sk-oai-2").unwrap();
        // A re-save updates one entry without dropping the other.
        save_token_at(&path, "zen", "sk-zen-3").unwrap();

        let tokens = load_tokens_at(&path);
        assert_eq!(tokens.get("zen").map(String::as_str), Some("sk-zen-3"));
        assert_eq!(tokens.get("openai").map(String::as_str), Some("sk-oai-2"));
    }

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
        let path = dir.path().join("auth.toml");
        save_token_at(&path, auth_key("zen"), "sk-opencode").unwrap();
        let tokens = load_tokens_at(&path);
        assert_eq!(
            tokens.get(auth_key("go")).map(String::as_str),
            Some("sk-opencode"),
            "go finds the credential saved under zen"
        );
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_tokens_at(&dir.path().join("nope.toml")).is_empty());
    }

    #[test]
    fn save_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        save_token_at(&path, "zen", "sk").unwrap();
        save_token_at(&path, "openai", "sk2").unwrap();
        // The only file in the directory is the credential file itself — the
        // atomic-write temp is renamed away, never orphaned.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "auth.toml")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files left: {leftovers:?}");
    }

    #[test]
    fn write_atomic_produces_content_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write_atomic(&path, b"hello world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        save_token_at(&path, "zen", "sk").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential file must be 0600");
    }
}
