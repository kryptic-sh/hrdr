# Security and Correctness Audit

- **Audit date:** 2026-07-18
- **Re-verified:** 2026-07-21 (against HEAD `df714b8`)
- **Depth:** high
- **Method:** static, read-only source review with caller and failure-scenario
  tracing.

## Note on scope

The original draft of this audit listed 19 findings. Independent re-verification
against the current tree dropped 14 of them:

- **Stale** — already fixed in the current tree: LSP secret-read bypass, LSP
  rename workspace confinement, regex-replacement OOM, unbounded attachment
  reads, and session-file permissions (now `0600` via `write_atomic`).
- **False / not present** — cited symbols (`ModelRef::parse().unwrap()`,
  `PersistedMessageInner`, `is_path_in_workspace`) do not exist in this repo's
  history; the real deserialize path already uses
  `map_err(serde::de::Error::custom)?`. `write_atomic` symlink "overwrite" does
  not work because `rename(2)` replaces a symlink at the destination rather than
  following it. DNS-rebinding, memory-scope traversal, and MCP/`watch`
  "arbitrary command" claims describe either code that returns only filtered
  addresses, path validation that rejects non-`Normal` components, or the
  intended, documented threat model (guardrails are "a safety net against model
  mistakes, not a security boundary").

The five findings below are what remained after verification: two actionable
(owner-only file/dir permissions), one defense-in-depth, and two marginal.

## Resolution status (2026-07-21)

| #   | Finding                        | Status                       |
| --- | ------------------------------ | ---------------------------- |
| 1   | History file perms             | **Fixed** — `0610404`        |
| 2   | Credential directory perms     | **Fixed** — `b182502`        |
| 3   | OpenRouter OAuth `state`       | **Deferred** — see finding 3 |
| 4   | Wire-log symlink TOCTOU        | **Fixed** — `fb0ca0c`        |
| 5   | OAuth error-page HTML escaping | **Fixed** — `f36034b`        |

Finding 3 is deferred, not skipped: strict `state` validation would break
OpenRouter login if the provider does not echo `state` back in its callback, and
the existing code comment states it does not. opencode — checked as a reference
implementation — authenticates OpenRouter with a plain bearer API key and has no
PKCE/`state` flow at all, so it could not confirm the provider's behavior.
Revisit if OpenRouter's `state`-echo behavior is confirmed (e.g. from their docs
or a live callback capture); the code already has `generate_state()` ready to
use.

## Findings

### 1. Medium — Input history file uses umask-dependent permissions ✅ FIXED (`0610404`)

**Location:** `crates/hrdr-app/src/history.rs:116-129`

`persist_history` creates the directory with `std::fs::create_dir_all(parent)`
(:122) and writes the file with `std::fs::write(path, body)` (:129). Neither
sets an explicit mode, so the history file lands at `0666 & ~umask` — typically
`0644`. Input history contains the user's own prompts.

**Concrete scenario:** On a multi-user system with umask `022`, other local
users can read the prompt history, which may include pasted secrets or sensitive
context.

**Fix:** Write the file owner-only on Unix (`OpenOptions` with `.mode(0o600)`,
or route through the same `write_atomic` helper the session path already uses,
which creates its temp with `.mode(0o600)` before renaming).

### 2. Medium — Credential directory created with umask, not 0700 ✅ FIXED (`b182502`)

**Location:** `crates/hrdr-agent/src/auth.rs:186`;
`crates/hrdr-agent/src/oauth.rs:556`

`save_token_at` and `save_oauth_at` both call bare
`std::fs::create_dir_all(parent)` with no follow-up `set_permissions`, so
`~/.config/hrdr/` lands at `0777 & ~umask` — commonly `0755`. The credential
files themselves are correctly `0600` (via `write_atomic`, `auth.rs:119`), so
their contents are not exposed, but the directory is world-listable.

**Concrete scenario:** On a permissive-umask system, another local user can list
the directory and see `auth.toml` / `oauth.json` filenames and timestamps — a
metadata leak revealing which providers you have authenticated.

**Fix:** `set_permissions(dir, 0o700)` after `create_dir_all`. The repo already
does exactly this at `crates/hrdr-agent/src/subagent_transcript.rs:89-95`
(`create_dir_all` then `0o700`, and `0o600` on the file at `:102`) — a two-line
copy of an existing pattern.

### 3. Medium — OpenRouter OAuth flow has no CSRF state parameter ⏸ DEFERRED

**Location:** `crates/hrdr-agent/src/oauth.rs:280-287`;
`crates/hrdr-app/src/login.rs:344,467`

`openrouter_authorize_url` builds the authorize URL from `callback_url` +
`code_challenge` + `code_challenge_method=S256` only — no `state`. The callback
is awaited with an empty `expected_state` (`login.rs:467`), so
`parse_callback`'s check (`oauth.rs:238`) passes for any callback that omits
`state`.

**Concrete scenario:** A local attacker could deliver a crafted callback to the
loopback listener with an attacker-supplied `code` before the real browser
redirect arrives.

**Mitigations already present (why this is defense-in-depth, not
high-severity):** the listener binds `127.0.0.1` only (`oauth.rs:146`), so a
_local_ attacker is required; PKCE binds the exchange — `openrouter_exchange`
sends the `code_verifier` (`oauth.rs:291-296`), and a code the attacker obtained
was issued against the attacker's own challenge, so exchanging it with our
verifier fails at OpenRouter. The realistic worst case is a local denial of the
login attempt, not credential substitution.

**Fix:** Mint and pass `state` for OpenRouter as the ChatGPT/OpenAI flow already
does — `generate_state()` (`oauth.rs:101`) exists and is used there
(`openai_authorize_url`, verified via `await_oauth_code_within`). Provider-
specific gap, not systemic.

### 4. Low — Wire-log file opening has a symlink TOCTOU window ✅ FIXED (`fb0ca0c`)

**Location:** `crates/hrdr-llm/src/client.rs:77-122`

`open_wire_log` performs a `symlink_metadata` preflight rejecting symlinks and
non-regular files (:83-99), then separately opens the pathname with a
symlink-following `OpenOptions` (:101-107). The non-atomic window between the
two is real, and the source documents it verbatim (:78-82: "NOT an atomic
`O_NOFOLLOW` guarantee"). `rotate_wire_log` (:139-156) reuses `open_wire_log`,
so the same window applies to the post-rotation reopen.

**Why this is marginal:** the path is caller-chosen via the opt-in
`HRDR_LOG_REQUESTS` env var, so an attacker who can set it already controls the
process. The append opens with the hrdr user's own credentials (a symlink to a
root-owned file just yields `EACCES`), and there is a post-open descriptor check
(`is_file()`, :115-117). The realistic worst case is that a race landing on an
attacker-writable regular file would get chmod'd to `0600` and have JSON
appended to it — not overwriting a system file.

**Fix (optional):** open with `custom_flags(libc::O_NOFOLLOW)` on Unix to close
the window cleanly.

### 5. Low — HTML escaping incomplete in OAuth error page ✅ FIXED (`f36034b`)

**Location:** `crates/hrdr-agent/src/oauth.rs:262-276`

`html_escape` covers `&`, `<`, `>` but not `"` or `'`.

**Why this is theoretical:** the only caller is `error_page` (:268), which
interpolates the escaped value as element **text** content (`<p>{}</p>`, :267),
never inside an attribute. With no attribute context reachable, the missing
quote escapes cannot be exploited, and the existing `<`/`>` escaping already
blocks tag injection. Delivery would also require luring a victim onto the
ephemeral loopback listener during a live login.

**Fix (defense-in-depth):** add `"` → `&quot;` and `'` → `&#39;` so the helper
is correct for any future attribute-context use.

## Summary

| Severity | Count | Issues                                                           |
| -------- | ----- | ---------------------------------------------------------------- |
| Medium   | 3     | History file perms, credential dir perms, OpenRouter OAuth state |
| Low      | 2     | Wire-log symlink TOCTOU, OAuth error-page HTML escaping          |

**Total: 5 findings.** The two actionable items (findings 1 and 2) are
owner-only permission hardening, each a small copy of a pattern already used
elsewhere in the repo. Finding 3 is defense-in-depth against a local-only,
PKCE-mitigated CSRF window. Findings 4 and 5 are marginal — real but gated by
opt-in configuration or the absence of a reachable exploit context — and are
recorded for completeness.
