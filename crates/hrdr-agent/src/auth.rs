//! Raw-API-key access to the unified credential store, kept out of `config.toml`
//! so API keys never land in a file users commit or share. Keys live as `key`
//! entries in `$XDG_CONFIG_HOME/hrdr/auth.json` (`0600` on unix; on Windows no
//! explicit ACL — hrdr relies on the default ACLs of the containing per-user
//! profile directory, which is user-scoped by default). Written by the `/login`
//! wizard, read at startup and on a live provider switch (the `/model` picker or
//! `/login`).
//!
//! The store schema, its locked/atomic read-modify-write, and the one-time
//! migration from the old `auth.toml`/`oauth.json` files all live in
//! [`crate::auth_store`]; this module is the key-facing view over it. The
//! atomic-write and directory-permission primitives ([`write_atomic`],
//! [`create_dir_owner_only`]) live here because they are shared by every store.

use std::collections::HashMap;
use std::io::Write;
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

/// Write `data` to `path` atomically: write to a temp file in the same
/// directory, fsync, then rename over the target — a concurrent reader never
/// sees a partial write. On unix the temp file is created with `0o600`
/// permissions from the start so there is no window where it exists with
/// broader permissions.
///
/// Confidentiality guarantee, stated honestly: on Unix the file is owner-only
/// (`0600`), enforced on every write. On Windows hrdr sets **no** explicit ACL
/// — it relies on the default ACLs of the containing directory. In practice the
/// credential files land under `~/.config/hrdr` (see [`crate::config_dir`]),
/// which on Windows resolves to the per-user profile (`%USERPROFILE%`, not
/// `%APPDATA%`) and is user-scoped by default. hrdr does not add per-user ACLs
/// itself, so the guarantee is the platform default, not something enforced
/// here.
///
/// The parent directory is fsynced after a successful rename so that the
/// rename is crash-durable (the directory entry change is flushed to media).
/// A directory sync failure is **not** reported as an error: the rename
/// itself is atomic and the data is already on disk — a lost sync only
/// risks losing the rename itself if the machine crashes before the
/// directory metadata write completes.
///
/// The parent directory must already exist. A rename failure removes the temp
/// so no stray file is left behind.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // Write to a temp file in the same directory, then rename atomically.
    // tempfile is a dev-dependency only, so the temp name comes from the
    // shared sibling-temp scheme instead.
    let tmp = hrdr_llm::unique_sibling_path(path, "hrdr-tmp");

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
        return result;
    }
    // After a successful rename, sync the parent directory so the directory
    // entry change (the rename) is crash-durable on Unix.  A sync failure is
    // silently swallowed: the write is atomic and the data is on disk — the
    // only thing a lost directory sync risks is losing the rename itself in a
    // crash before the directory metadata flushes.
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    // No non-unix permission tightening: the Windows read-only *attribute*
    // doesn't restrict reads (access is by ACL) and would make the file
    // un-replaceable by the next atomic rename. Unix already got 0600 above.
    // On Windows we deliberately set no explicit ACL and rely on the default
    // ACLs of the containing per-user profile directory (~/.config/hrdr under
    // %USERPROFILE%), which is user-scoped by default. Setting a per-user ACL
    // would need the `windows`/`winapi` crate (a new dependency); the honest
    // documented guarantee is the platform default (see the doc comment above).
    Ok(())
}

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

    /// `write_atomic` syncs the parent directory after a successful rename so
    /// the directory entry change is crash-durable on Unix.  This test cannot
    /// verify the sync itself (it is a kernel-level durability guarantee), but
    /// it verifies that the sync does not break the happy path: the file is
    /// written, the content is correct, no stray temps are left, and the
    /// directory is still usable.
    #[test]
    fn write_atomic_with_dir_sync_completes_normally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");

        write_atomic(&path, b"sync test data").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"sync test data",
            "content is preserved"
        );

        // No temp files or other stray files were left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no stray files after write_atomic: {leftovers:?}"
        );

        // A second write_atomic on the same path also succeeds.
        write_atomic(&path, b"second write").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second write");
    }
}
