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
//! ## Discipline
//!
//! * Writes go through [`crate::write_atomic`] — `0600` on unix, atomic rename,
//!   a reader never sees a partial write.
//! * The containing directory is tightened to `0700`
//!   ([`crate::auth::create_dir_owner_only`]).
//! * Every mutating operation holds the cross-process
//!   [`StoreLock`](crate::store_lock::StoreLock) on `auth.json` across the whole
//!   read-modify-write, so concurrent writers serialize instead of racing on a
//!   stale snapshot. One lock on `auth.json` serializes *all* credential
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
    /// A raw API key.
    Key { key: String },
    /// OAuth tokens. Mirrors [`OAuthCreds`].
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

/// Load the whole `provider → AuthEntry` map at `auth_json`. Best-effort: an
/// empty map on a missing/unreadable/corrupt file — a load never fails.
fn load_map_at(auth_json: &Path) -> HashMap<String, AuthEntry> {
    std::fs::read_to_string(auth_json)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Insert/replace one `key → entry` in the store at `auth_json` (atomic,
/// `0600`), preserving every other entry. The locked-RMW core that backs every
/// save.
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
}
