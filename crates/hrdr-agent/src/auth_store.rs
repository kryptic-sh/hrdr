//! The unified credential store: a single `$XDG_CONFIG_HOME/hrdr/auth.json`.
//!
//! One flat JSON map, provider name → a tagged [`AuthEntry`] that is *either* a
//! raw API key or a set of OAuth tokens:
//!
//! ```json
//! {
//!   "openrouter": { "type": "key",   "key": "sk-or-..." },
//!   "chatgpt":    { "type": "oauth", "access": "...", "refresh": "...",
//!                   "expires_ms": 1750000000000, "account_id": "..." }
//! }
//! ```
//!
//! This replaces the two former stores (`auth.toml` for keys, `oauth.json` for
//! OAuth). A one-time [`migrate_if_needed_at`] folds any old files into the new
//! one on first access and then removes them, so callers only ever see
//! `auth.json`.
//!
//! ## Discipline (shared with the old stores, reused not reinvented)
//!
//! * Writes go through [`crate::write_atomic`] — `0600` on unix, atomic rename,
//!   a reader never sees a partial write.
//! * The containing directory is tightened to `0700`
//!   ([`crate::auth::create_dir_owner_only`]).
//! * Every mutating operation (and the migration) holds the cross-process
//!   [`StoreLock`](crate::store_lock::StoreLock) on `auth.json` across the whole
//!   read-modify-write, so concurrent writers serialize instead of racing on a
//!   stale snapshot. One lock on `auth.json` now serializes *all* credential
//!   writes (keys and OAuth alike).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::OAuthCreds;
use crate::store_lock::StoreLock;

/// One stored credential for a provider: either a raw API key or OAuth tokens.
///
/// Internally tagged on `type` so the two shapes coexist in one flat map and a
/// reader can tell them apart. `account_id` is optional (`#[serde(default)]`) so
/// an OAuth entry saved without it still parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum AuthEntry {
    /// A raw API key (formerly `auth.toml`'s `provider = "key"`).
    Key { key: String },
    /// OAuth tokens (formerly an `oauth.json` entry). Mirrors [`OAuthCreds`].
    Oauth {
        access: String,
        refresh: String,
        /// Absolute expiry of `access`, in epoch milliseconds.
        expires_ms: u64,
        #[serde(default)]
        account_id: Option<String>,
    },
}

impl From<OAuthCreds> for AuthEntry {
    fn from(c: OAuthCreds) -> Self {
        AuthEntry::Oauth {
            access: c.access,
            refresh: c.refresh,
            expires_ms: c.expires_ms,
            account_id: c.account_id,
        }
    }
}

impl AuthEntry {
    /// The raw API key, if this is a [`AuthEntry::Key`].
    fn as_key(&self) -> Option<&str> {
        match self {
            AuthEntry::Key { key } => Some(key),
            AuthEntry::Oauth { .. } => None,
        }
    }

    /// The OAuth credentials, if this is an [`AuthEntry::Oauth`].
    fn as_oauth(&self) -> Option<OAuthCreds> {
        match self {
            AuthEntry::Oauth {
                access,
                refresh,
                expires_ms,
                account_id,
            } => Some(OAuthCreds {
                access: access.clone(),
                refresh: refresh.clone(),
                expires_ms: *expires_ms,
                account_id: account_id.clone(),
            }),
            AuthEntry::Key { .. } => None,
        }
    }
}

/// Path to the unified credential store (`~/.config/hrdr/auth.json`), if `HOME`
/// is set.
pub(crate) fn store_path() -> Option<PathBuf> {
    Some(crate::config_dir()?.join("auth.json"))
}

// ── Whole-map read/write ────────────────────────────────────────────────────

/// Load the whole `provider → AuthEntry` map at `auth_json`, migrating any old
/// stores first. Best-effort: an empty map on a missing/unreadable/corrupt
/// file — a load never fails.
fn load_map_at(auth_json: &Path) -> HashMap<String, AuthEntry> {
    // Fold any pre-unification files in first; a migration hiccup must not fail
    // a read (best-effort), so its error is deliberately ignored here.
    let _ = migrate_if_needed_at(auth_json);
    std::fs::read_to_string(auth_json)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Insert/replace one `key → entry` in the store at `auth_json` (atomic,
/// `0600`), preserving every other entry. The migrate-then-locked-RMW core that
/// backs every save.
///
/// Concurrent *writers* are serialized by the [`StoreLock`]: the read-modify-
/// write runs entirely under the lock, so a second process re-reads the merged
/// store the first one wrote rather than racing on a stale snapshot. Two writers
/// adding *different* providers both survive; two targeting the *same* provider
/// are last-writer-wins (the later login is the fresher credential).
///
/// Refuses to clobber a store it cannot parse: an existing but malformed
/// `auth.json` yields an error and is left byte-for-byte intact, so a corrupt
/// file is never silently overwritten.
fn save_entry_at(auth_json: &Path, key: &str, entry: AuthEntry) -> Result<()> {
    let parent = auth_json.parent().unwrap_or(Path::new("."));
    crate::auth::create_dir_owner_only(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    // Migrate before we take our own lock (migration takes and releases the same
    // lock internally); after it, `auth.json` is the single source of truth.
    migrate_if_needed_at(auth_json)?;
    // Acquire the write lock BEFORE the read and hold it across the whole
    // read-modify-write. `_lock` releases on drop (normal return, `?`, panic).
    let _lock = StoreLock::acquire(auth_json)?;
    let mut map: HashMap<String, AuthEntry> = match std::fs::read_to_string(auth_json) {
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!("parsing existing credential store {}", auth_json.display())
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading credential store {}", auth_json.display()));
        }
    };
    map.insert(key.to_string(), entry);
    let json = serde_json::to_vec_pretty(&map).context("serializing auth.json")?;
    crate::write_atomic(auth_json, &json)
        .with_context(|| format!("writing {}", auth_json.display()))
}

// ── Key entries ─────────────────────────────────────────────────────────────

/// All `provider → api_key` pairs in the store (OAuth entries excluded).
pub(crate) fn load_keys_at(auth_json: &Path) -> HashMap<String, String> {
    load_map_at(auth_json)
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_key()?.to_string())))
        .collect()
}

/// Store `provider`'s raw API `token`, preserving other entries.
pub(crate) fn save_key_at(auth_json: &Path, provider: &str, token: &str) -> Result<()> {
    save_entry_at(
        auth_json,
        provider,
        AuthEntry::Key {
            key: token.to_string(),
        },
    )
}

// ── OAuth entries ───────────────────────────────────────────────────────────

/// The stored OAuth credentials for `provider`, if the entry exists and is an
/// OAuth (not key) entry.
pub(crate) fn load_oauth_entry_at(auth_json: &Path, provider: &str) -> Option<OAuthCreds> {
    load_map_at(auth_json)
        .get(provider)
        .and_then(AuthEntry::as_oauth)
}

/// Store `provider`'s OAuth `creds`, preserving other entries.
pub(crate) fn save_oauth_entry_at(
    auth_json: &Path,
    provider: &str,
    creds: &OAuthCreds,
) -> Result<()> {
    save_entry_at(auth_json, provider, AuthEntry::from(creds.clone()))
}

// ── Migration from the two old stores ───────────────────────────────────────

/// The result of trying to read one old store file.
enum OldFile<T> {
    /// The file does not exist — nothing to migrate, nothing to delete.
    Absent,
    /// The file was read and parsed — its entries migrate, and it is safe to
    /// delete once `auth.json` has been written.
    Parsed(T),
    /// The file exists but could not be read or parsed. Its entries are NOT
    /// migrated and the file is NOT deleted — it is left in place for the user
    /// to recover, never silently dropped.
    Unparsable,
}

/// One-time, idempotent migration of the two pre-unification stores
/// (`auth.toml`, `oauth.json`) into `auth.json`, run before the first read or
/// write.
///
/// Ordering is strict — **read old → write new → delete old** — and never
/// deletes on a failed write:
/// 1. If `auth.json` already exists, do nothing (already migrated).
/// 2. Otherwise best-effort read `auth.toml` (keys) and `oauth.json` (OAuth).
/// 3. Build the unified map and write `auth.json` *atomically*.
/// 4. **Only after** that write succeeds, delete each old file that was
///    successfully parsed. A file that failed to parse is left untouched.
///
/// Concurrency-safe: the whole migration runs under the `auth.json`
/// [`StoreLock`], and re-checks `auth.json` *inside* the lock, so two processes
/// racing on first start migrate exactly once (the loser sees the file the
/// winner wrote and no-ops).
pub(crate) fn migrate_if_needed_at(auth_json: &Path) -> Result<()> {
    // Fast path: already migrated. Cheap check outside the lock.
    if auth_json.exists() {
        return Ok(());
    }
    let parent = auth_json.parent().unwrap_or(Path::new("."));
    let toml_path = parent.join("auth.toml");
    let json_path = parent.join("oauth.json");
    // A fresh install with no old files: nothing to do. The first real save
    // creates `auth.json`. (Avoids creating the dir / lock needlessly.)
    if !toml_path.exists() && !json_path.exists() {
        return Ok(());
    }
    crate::auth::create_dir_owner_only(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    // Serialize the whole migration against concurrent writers/migrators.
    let _lock = StoreLock::acquire(auth_json)?;
    // Re-check under the lock: another process may have migrated while we waited.
    if auth_json.exists() {
        return Ok(());
    }

    let keys = read_old_keys(&toml_path);
    let oauth = read_old_oauth(&json_path);

    let mut map: HashMap<String, AuthEntry> = HashMap::new();
    if let OldFile::Parsed(m) = &keys {
        for (k, v) in m {
            map.insert(k.clone(), AuthEntry::Key { key: v.clone() });
        }
    }
    // OAuth entries second: on the (unexpected) chance a provider appears in both
    // stores, the OAuth credential wins. The openai/chatgpt provider merge that
    // would actually collide is deliberately out of scope, so no collision is
    // expected in practice.
    if let OldFile::Parsed(m) = &oauth {
        for (k, v) in m {
            map.insert(k.clone(), AuthEntry::from(v.clone()));
        }
    }

    // If every old file that is present is unparsable, do NOT write `auth.json`:
    // leave the old files exactly as they are so a user who fixes them gets them
    // migrated on a later run, rather than stranding their (unreadable) data
    // behind an empty new store.
    let any_parsed = matches!(keys, OldFile::Parsed(_)) || matches!(oauth, OldFile::Parsed(_));
    if !any_parsed {
        return Ok(());
    }

    // Write the unified store. If this fails, the old files are left intact and
    // the error is surfaced — we NEVER delete an old file on a failed write.
    let json = serde_json::to_vec_pretty(&map).context("serializing migrated auth.json")?;
    crate::write_atomic(auth_json, &json)
        .with_context(|| format!("writing migrated {}", auth_json.display()))?;

    // The write landed. Delete only the sources we actually parsed; an
    // unparsable source is preserved for the user. Best-effort: a delete failure
    // does not undo a successful migration.
    if matches!(keys, OldFile::Parsed(_)) {
        let _ = std::fs::remove_file(&toml_path);
    }
    if matches!(oauth, OldFile::Parsed(_)) {
        let _ = std::fs::remove_file(&json_path);
    }
    Ok(())
}

/// Best-effort read of the old `auth.toml` key store into `provider → key`.
/// A missing file is [`OldFile::Absent`]; an unreadable or unparsable one is
/// [`OldFile::Unparsable`] (kept, not deleted).
fn read_old_keys(path: &Path) -> OldFile<HashMap<String, String>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OldFile::Absent,
        Err(_) => OldFile::Unparsable,
        Ok(text) => match text.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => OldFile::Parsed(
                doc.as_table()
                    .iter()
                    .filter_map(|(k, v)| Some((k.to_string(), v.as_str()?.to_string())))
                    .collect(),
            ),
            Err(_) => OldFile::Unparsable,
        },
    }
}

/// Best-effort read of the old `oauth.json` store into `provider → OAuthCreds`.
/// A missing file is [`OldFile::Absent`]; an unreadable or unparsable one is
/// [`OldFile::Unparsable`] (kept, not deleted).
fn read_old_oauth(path: &Path) -> OldFile<HashMap<String, OAuthCreds>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OldFile::Absent,
        Err(_) => OldFile::Unparsable,
        Ok(text) => match serde_json::from_str::<HashMap<String, OAuthCreds>>(&text) {
            Ok(m) => OldFile::Parsed(m),
            Err(_) => OldFile::Unparsable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth(access: &str, refresh: &str, expires_ms: u64, account: Option<&str>) -> OAuthCreds {
        OAuthCreds {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_ms,
            account_id: account.map(str::to_string),
        }
    }

    // ── Schema round-trips ───────────────────────────────────────────────────

    #[test]
    fn key_entry_round_trips() {
        let e = AuthEntry::Key {
            key: "sk-abc".to_string(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"type":"key","key":"sk-abc"}"#);
        assert_eq!(serde_json::from_str::<AuthEntry>(&json).unwrap(), e);
    }

    #[test]
    fn oauth_entry_round_trips() {
        let e = AuthEntry::from(oauth("acc", "ref", 123, Some("acct")));
        let json = serde_json::to_string(&e).unwrap();
        let back: AuthEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        assert_eq!(
            back.as_oauth(),
            Some(oauth("acc", "ref", 123, Some("acct")))
        );
    }

    #[test]
    fn oauth_entry_without_account_id_still_parses() {
        // `account_id` is `#[serde(default)]` — an entry missing it deserializes
        // with `account_id: None`.
        let json = r#"{"type":"oauth","access":"a","refresh":"r","expires_ms":9}"#;
        let e: AuthEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.as_oauth(), Some(oauth("a", "r", 9, None)));
    }

    #[test]
    fn mixed_map_round_trips() {
        let mut map: HashMap<String, AuthEntry> = HashMap::new();
        map.insert(
            "openrouter".to_string(),
            AuthEntry::Key {
                key: "sk-or".to_string(),
            },
        );
        map.insert(
            "chatgpt".to_string(),
            AuthEntry::from(oauth("acc", "ref", 1, Some("acct"))),
        );
        let json = serde_json::to_vec_pretty(&map).unwrap();
        let back: HashMap<String, AuthEntry> = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, map);
    }

    // ── Save / load ──────────────────────────────────────────────────────────

    #[test]
    fn save_and_load_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_key_at(&path, "openai", "sk-oai").unwrap();
        // A second key coexists; re-saving one preserves the other.
        save_key_at(&path, "openrouter", "sk-or").unwrap();
        save_key_at(&path, "openai", "sk-oai-2").unwrap();
        let keys = load_keys_at(&path);
        assert_eq!(keys.get("openai").map(String::as_str), Some("sk-oai-2"));
        assert_eq!(keys.get("openrouter").map(String::as_str), Some("sk-or"));
    }

    #[test]
    fn save_and_load_oauth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let creds = oauth("acc", "ref", 42, Some("acct"));
        save_oauth_entry_at(&path, "chatgpt", &creds).unwrap();
        assert_eq!(load_oauth_entry_at(&path, "chatgpt"), Some(creds));
        // A key entry is not returned as OAuth, and vice-versa.
        save_key_at(&path, "openai", "sk-oai").unwrap();
        assert_eq!(load_oauth_entry_at(&path, "openai"), None);
        assert_eq!(load_keys_at(&path).get("chatgpt"), None);
    }

    #[test]
    fn keys_and_oauth_share_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_key_at(&path, "openrouter", "sk-or").unwrap();
        save_oauth_entry_at(&path, "chatgpt", &oauth("acc", "ref", 1, None)).unwrap();
        // Both live in one map: the key survives an OAuth write and vice-versa.
        assert_eq!(
            load_keys_at(&path).get("openrouter").map(String::as_str),
            Some("sk-or")
        );
        assert_eq!(load_oauth_entry_at(&path, "chatgpt").unwrap().access, "acc");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(load_keys_at(&path).is_empty());
        assert_eq!(load_oauth_entry_at(&path, "chatgpt"), None);
    }

    #[test]
    fn save_refuses_to_replace_a_malformed_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let original = b"{ not valid json\n";
        std::fs::write(&path, original).unwrap();
        let err = save_key_at(&path, "openai", "sk").unwrap_err().to_string();
        assert!(err.contains("parsing existing credential store"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn save_leaves_no_temp_or_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_key_at(&path, "openai", "sk").unwrap();
        save_oauth_entry_at(&path, "chatgpt", &oauth("a", "r", 1, None)).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "auth.json")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files left: {leftovers:?}");
    }

    // ── Permissions (unix) ───────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // A nested dir that does not exist yet, so the save creates it 0700.
        let cfg_dir = tmp.path().join("hrdr");
        let path = cfg_dir.join("auth.json");
        save_key_at(&path, "openai", "sk").unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600, "credential file must be 0600");
        let dir_mode = std::fs::metadata(&cfg_dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "credential dir must be 0700");
    }

    // ── Concurrency ──────────────────────────────────────────────────────────

    #[test]
    fn concurrent_writers_different_providers_all_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_key_at(&path, "seed", "sk-seed").unwrap();
        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save_key_at(&path, &format!("p{i}"), &format!("sk-{i}")).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let keys = load_keys_at(&path);
        assert_eq!(keys.get("seed").map(String::as_str), Some("sk-seed"));
        for i in 0..n {
            assert_eq!(
                keys.get(&format!("p{i}")).map(String::as_str),
                Some(format!("sk-{i}").as_str()),
                "writer {i}'s entry survived"
            );
        }
    }

    // ── Migration ────────────────────────────────────────────────────────────

    fn seed_old_toml(dir: &Path, pairs: &[(&str, &str)]) {
        let body: String = pairs
            .iter()
            .map(|(k, v)| format!("{k} = \"{v}\"\n"))
            .collect();
        std::fs::write(dir.join("auth.toml"), body).unwrap();
    }

    fn seed_old_oauth(dir: &Path, provider: &str, creds: &OAuthCreds) {
        let mut m: HashMap<String, OAuthCreds> = HashMap::new();
        m.insert(provider.to_string(), creds.clone());
        std::fs::write(
            dir.join("oauth.json"),
            serde_json::to_vec_pretty(&m).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn migration_folds_both_old_files_and_deletes_them() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        seed_old_toml(d, &[("openrouter", "sk-or"), ("openai", "sk-oai")]);
        seed_old_oauth(d, "chatgpt", &oauth("acc", "ref", 777, Some("acct")));
        let path = d.join("auth.json");

        migrate_if_needed_at(&path).unwrap();

        // auth.json now holds all three with the right tags.
        assert_eq!(
            load_keys_at(&path).get("openrouter").map(String::as_str),
            Some("sk-or")
        );
        assert_eq!(
            load_keys_at(&path).get("openai").map(String::as_str),
            Some("sk-oai")
        );
        assert_eq!(
            load_oauth_entry_at(&path, "chatgpt"),
            Some(oauth("acc", "ref", 777, Some("acct")))
        );
        // Both old files are gone.
        assert!(
            !d.join("auth.toml").exists(),
            "auth.toml deleted after migration"
        );
        assert!(
            !d.join("oauth.json").exists(),
            "oauth.json deleted after migration"
        );
    }

    #[test]
    fn migration_only_toml_present() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        seed_old_toml(d, &[("openai", "sk-oai")]);
        let path = d.join("auth.json");
        migrate_if_needed_at(&path).unwrap();
        assert_eq!(
            load_keys_at(&path).get("openai").map(String::as_str),
            Some("sk-oai")
        );
        assert!(!d.join("auth.toml").exists());
        assert!(!d.join("oauth.json").exists());
    }

    #[test]
    fn migration_only_oauth_present() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        seed_old_oauth(d, "chatgpt", &oauth("acc", "ref", 5, None));
        let path = d.join("auth.json");
        migrate_if_needed_at(&path).unwrap();
        assert_eq!(
            load_oauth_entry_at(&path, "chatgpt"),
            Some(oauth("acc", "ref", 5, None))
        );
        assert!(!d.join("oauth.json").exists());
    }

    #[test]
    fn migration_neither_present_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        migrate_if_needed_at(&path).unwrap();
        assert!(!path.exists(), "no auth.json created on a fresh install");
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        seed_old_toml(d, &[("openai", "sk-oai")]);
        let path = d.join("auth.json");
        migrate_if_needed_at(&path).unwrap();
        let after_first = std::fs::read(&path).unwrap();
        // A second (and third) run is a no-op: auth.json is unchanged, no error.
        migrate_if_needed_at(&path).unwrap();
        migrate_if_needed_at(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), after_first);
    }

    #[test]
    fn migration_does_not_delete_a_corrupt_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // A good toml plus a corrupt oauth.json.
        seed_old_toml(d, &[("openai", "sk-oai")]);
        let corrupt = b"{ not valid json";
        std::fs::write(d.join("oauth.json"), corrupt).unwrap();
        let path = d.join("auth.json");

        migrate_if_needed_at(&path).unwrap();

        // The parseable source migrated and was deleted...
        assert_eq!(
            load_keys_at(&path).get("openai").map(String::as_str),
            Some("sk-oai")
        );
        assert!(!d.join("auth.toml").exists());
        // ...the corrupt one survives, and its (unreadable) provider is absent —
        // never silently dropped AND deleted.
        assert!(
            d.join("oauth.json").exists(),
            "corrupt oauth.json is preserved"
        );
        assert_eq!(std::fs::read(d.join("oauth.json")).unwrap(), corrupt);
        assert_eq!(load_oauth_entry_at(&path, "chatgpt"), None);
    }

    #[test]
    fn migration_all_sources_corrupt_writes_nothing_and_keeps_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("auth.toml"), b"= not valid toml").unwrap();
        std::fs::write(d.join("oauth.json"), b"{ not valid json").unwrap();
        let path = d.join("auth.json");
        migrate_if_needed_at(&path).unwrap();
        // Nothing parseable → no auth.json written, both files left for the user.
        assert!(!path.exists(), "no auth.json when every source is corrupt");
        assert!(d.join("auth.toml").exists());
        assert!(d.join("oauth.json").exists());
    }

    #[test]
    fn migration_preexisting_auth_json_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // An already-migrated store, plus a stray old file.
        save_key_at(&d.join("auth.json"), "openai", "sk-current").unwrap();
        let existing = std::fs::read(d.join("auth.json")).unwrap();
        seed_old_toml(d, &[("openai", "sk-STALE")]);
        migrate_if_needed_at(&d.join("auth.json")).unwrap();
        // auth.json untouched (never overwritten by migration).
        assert_eq!(std::fs::read(d.join("auth.json")).unwrap(), existing);
        assert_eq!(
            load_keys_at(&d.join("auth.json"))
                .get("openai")
                .map(String::as_str),
            Some("sk-current")
        );
    }
}
