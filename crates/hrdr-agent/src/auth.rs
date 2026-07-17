//! Dedicated credential store, kept out of `config.toml` so API keys never land
//! in a file users commit or share. Plaintext TOML at
//! `$XDG_CONFIG_HOME/hrdr/auth.toml` (`0600` on unix; on Windows no explicit
//! ACL — hrdr relies on the default ACLs of the containing per-user profile
//! directory, which is user-scoped by default), a flat map of provider name →
//! API key. Written by the `/login` wizard, read at startup and on a live
//! provider switch (the `/model` picker or `/login`).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// Write `provider = "token"` into the file at `path`, preserving other entries
/// and ensuring owner-only (`0600`) permissions on unix. The path-based core of
/// [`save_auth_token`].
///
/// The write is done through [`write_atomic`] — a concurrent reader never sees a
/// partial write.
///
/// Concurrent *writers* are serialized by a cross-process lock
/// ([`StoreLock`](crate::store_lock::StoreLock)): the read-modify-write happens
/// entirely under the lock, so a second process re-reads the merged store the
/// first one wrote instead of racing on a stale snapshot. Two writers adding
/// *different* providers both survive; two writers targeting the *same* provider
/// are last-writer-wins (the later login is the fresher credential).
fn save_token_at(path: &Path, provider: &str, token: &str) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Acquire the write lock BEFORE the read, and hold it across the whole
    // read-modify-write. `_lock` releases on drop (normal return, `?`, panic).
    let _lock = crate::store_lock::StoreLock::acquire(path)?;
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing existing credential store {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::default(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading credential store {}", path.display()));
        }
    };
    doc[provider] = toml_edit::value(token);
    let content = doc.to_string();
    write_atomic(path, content.as_bytes()).with_context(|| format!("writing {}", path.display()))
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
    fn save_refuses_to_replace_a_malformed_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        let original = b"openai = [not valid toml\n";
        std::fs::write(&path, original).unwrap();

        let err = save_token_at(&path, "zen", "sk-new")
            .unwrap_err()
            .to_string();

        assert!(err.contains("parsing existing credential store"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
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

    /// Concurrent writers adding DIFFERENT providers all survive: the
    /// cross-process lock serializes the read-modify-write, so each writer
    /// re-reads the store the previous one wrote and merges its own key in
    /// rather than clobbering with a stale snapshot.
    #[test]
    fn concurrent_writers_different_providers_all_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        // Seed the file so every writer starts from a real (parseable) store.
        save_token_at(&path, "seed", "sk-seed").unwrap();

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let provider = format!("p{i}");
                    let token = format!("sk-{i}");
                    save_token_at(&path, &provider, &token).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let tokens = load_tokens_at(&path);
        assert_eq!(tokens.get("seed").map(String::as_str), Some("sk-seed"));
        for i in 0..n {
            assert_eq!(
                tokens.get(&format!("p{i}")).map(String::as_str),
                Some(format!("sk-{i}").as_str()),
                "writer {i}'s entry survived the concurrent writes"
            );
        }
    }

    /// Same-provider concurrent writers are last-writer-wins: they serialize on
    /// the lock, and exactly one of the written values remains. (The policy is
    /// documented on `save_token_at` — the later login is the fresher key.) The
    /// store is never corrupted: it always parses and always holds one value.
    #[test]
    fn concurrent_writers_same_provider_last_writer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save_token_at(&path, "shared", &format!("sk-{i}")).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let tokens = load_tokens_at(&path);
        // Exactly one value survived, and it is one of the values written — no
        // torn write, no corruption.
        let got = tokens.get("shared").expect("the key exists");
        let valid: Vec<String> = (0..n).map(|i| format!("sk-{i}")).collect();
        assert!(
            valid.contains(got),
            "last-writer value is one written: {got}"
        );
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
