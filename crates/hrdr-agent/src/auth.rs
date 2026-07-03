//! Dedicated credential store, kept out of `config.toml` so API keys never land
//! in a file users commit or share. Plaintext TOML at
//! `$XDG_CONFIG_HOME/hrdr/auth.toml` (`0600` on unix), a flat map of provider
//! name → API key. Written by the `/login` wizard, read at startup and on a
//! live `/provider` switch.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config_dir;

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

/// The stored API key for `provider`, if any.
pub fn auth_token(provider: &str) -> Option<String> {
    load_auth_tokens().remove(provider)
}

/// Store `provider = "token"` in the credential file (creating it, `0600` on
/// unix), preserving any other entries. Returns the file path.
pub fn save_auth_token(provider: &str, token: &str) -> Result<PathBuf> {
    let path =
        auth_file_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the auth file"))?;
    save_token_at(&path, provider, token)?;
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

/// Write `provider = "token"` into the file at `path`, preserving other entries
/// and ensuring owner-only (`0600`) permissions on unix. The path-based core of
/// [`save_auth_token`].
///
/// The write is done through a temp file in the same directory, then renamed
/// atomically over the target — a concurrent reader never sees a partial write.
/// On unix the temp file is created with `0o600` from the start, so there is no
/// window where the file exists with broader permissions.
fn save_token_at(path: &Path, provider: &str, token: &str) -> Result<()> {
    let mut doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .unwrap_or_default();
    doc[provider] = toml_edit::value(token);
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;

    // Write to a temp file in the same directory, then rename atomically.
    // tempfile is a dev-dependency only, so we build the temp name manually.
    let tmp = tmp_path(path, parent);
    let content = doc.to_string();

    #[cfg(unix)]
    let create_file = || -> Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))
    };
    #[cfg(not(unix))]
    let create_file = || -> Result<std::fs::File> {
        std::fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))
    };

    let mut f = create_file()?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    // Flush + fsync so the data is on disk before the rename.
    f.flush()?;
    #[cfg(unix)]
    f.sync_all()?;
    drop(f);

    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;

    // On non-unix, attempt to restrict permissions after the fact (best-effort).
    #[cfg(not(unix))]
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_readonly(true));
    }

    Ok(())
}

/// A unique temp-file path inside `parent` (same filesystem as `path`, so the
/// subsequent rename is atomic). The name includes a timestamp and PID so
/// concurrent writes don't collide.
fn tmp_path(path: &Path, parent: &Path) -> PathBuf {
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
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_tokens_at(&dir.path().join("nope.toml")).is_empty());
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
