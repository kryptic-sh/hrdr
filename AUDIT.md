# Security & Correctness Audit

**Date:** 2026-07-22 (verified 2026-07-23) **Depth:** High **Scope:** Full
codebase — all crates (`hrdr-tools`, `hrdr-llm`, `hrdr-agent`, `hrdr-app`,
`hrdr-editor`, `hrdr-tui`, `hrdr` binary) and all source files.

## Methodology

The attack surface was mapped by identifying entry points: HTTP handlers
(`fetch`, `search`, MCP HTTP/SSE transports), CLI args (`clap` in `main.rs`),
file parsers (read/write/edit/replace/grep tools), IPC (MCP stdio/HTTP, LSP),
and environment reads (`HRDR_*` env vars, `HOME`/`XDG` paths). Each class of
vulnerability was checked systematically against every source file: injection,
memory/resource, crypto, AuthZ/AuthN, data integrity, error handling, and
concurrency.

Findings were verified by re-reading surrounding code, tracing callers, and
constructing concrete trigger scenarios. Uncertain findings are marked as such.

---

## Findings (most-severe first)

### ~~H1 — HIGH: MCP SSE `endpoint` event allows unguarded SSRF (FIXED)~~

> **Fixed in commit `ab2f1b7`.** `connect_sse` now validates the server-supplied
> `endpoint` URL's host against the configured base host, rejecting cross-host
> steering. `build_http` uses `redirect(Policy::none())`. The host-matching
> check blocks the attack scenario; `is_blocked_host` is intentionally not used
> (would break legitimate local MCP servers on loopback/private IPs).

---

### ~~H2 — HIGH: LSP `check_confined` `..` escape bypass (FIXED)~~

> **Fixed in commit `98a86b3`.** The fallback now uses
> `canonicalize_nearest(path).starts_with(canonicalize_nearest(cwd))` — exactly
> the fix the audit recommended.

---

### M1 — MEDIUM: `read`/`guard_secret_read` TOCTOU via symlink replacement

**`crates/hrdr-tools/src/tools/read.rs:53-65` +
`crates/hrdr-tools/src/lib.rs:684-694`**

`guard_secret_read` canonicalizes the path and rejects known-secret
destinations, then `read_to_string(&path)` (line 65) re-resolves the
_un-canonicalized_ path. A symlink swap between the two calls — replace
`/tmp/x.txt` (checked, non-secret) with a symlink to `~/.ssh/id_rsa` — yields
the secret's contents in the tool output.

**Scenario:** An attacker who can both prompt-inject and race the filesystem
swaps a non-secret file for a symlink to a credential file between the secret
check and the `read_to_string`. The model receives the secret's contents. The
window is narrow, but the secret-denylist is explicitly advertised as the
exfiltration guardrail.

**Fix:** Open the file once (`File::open`) before the secret check and read
through that handle, mirroring `read_attach_file`'s Unix `dev`/`ino` identity
re-check (`lib.rs:483-494`); reject if the opened descriptor's canonicalized
target is a secret file.

---

### M2 — MEDIUM: Guardrail depth cap (`depth < 4`) bypassable by deeper `sh -c` nesting

**`crates/hrdr-tools/src/guardrails.rs:251-257`**

`extract_shell_c_args` is only re-scanned recursively up to depth 4. A payload
with five levels of `sh -c` nesting (e.g.
`sh -c 'sh -c "sh -c \"sh -c \"sh -c 'git push --force'\"\"\"\"'`) defeats the
re-scan: the outer four levels are unpacked, the deepest `git push --force` is
never reached.

**Scenario:** The model constructs a five-level nested shell command wrapping a
blocked pattern. The guardrail re-scans four levels, misses the fifth, and the
blocked command runs. The guardrails are explicitly documented as a "safety net
against model mistakes, not a security boundary" — but the depth cap is low
enough to defeat by hand.

**Fix:** Drop the depth cap and bound by total extracted length instead (refuse
re-scan once cumulative payload size exceeds, e.g., 64 KiB) — bounds work and
recursion without an arbitrary depth stop.

---

### M3 — MEDIUM: Guardrail tokenization fallback can miss blocked patterns behind unbalanced quotes

**`crates/hrdr-tools/src/guardrails.rs:276-285`**

When `shell_words::split` errors (unbalanced quotes), the fallback matches the
_raw_ command string. A command like `git push '"--force` (malformed but parsed
by bash as `--force`) falls back to raw `git push '"--force`; the force regex
needs a trailing delimiter (`--force(\s|$|['";&|])`), and `'"--force` ends at
EOF without one, so the rule does **not** fire. Bash parses `'"--force` as the
literal arg `--force` and runs the force-push.

**Scenario:** The model writes `git push '"--force` — a stray quote causes
`shell_words` to error, the raw fallback doesn't match the regex, and the
blocked command executes. Narrow, but a real bypass of a rule the code clearly
intends to enforce.

**Fix:** When `shell_words::split` errors, also strip leading/trailing
unbalanced quotes before matching, or always match the raw input in addition to
the tokenized form (defense-in-depth).

---

### M4 — MEDIUM: `OpenAiTokens` derives `Debug` and holds live OAuth tokens

**`crates/hrdr-agent/src/oauth.rs:343`**

`OpenAiTokens` (carrying `access_token`, `refresh_token`, `id_token`) derives
`Debug`. `OAuthCreds` (line 480) **also** derives `Debug` — both structs that
hold live bearer tokens leak them via `{:?}`. Only `OAuthAccess` (line 618-622)
correctly omits `Debug` with the comment: _"Deliberately NO `Debug` derive: it
holds a bearer token, and a `{:?}` (or `anyhow` context) must never leak it"_.
Any future `tracing::debug!("{:?}", tokens)`, `anyhow` context wrapping, or
`unwrap()`/`expect()` on a `Result<OpenAiTokens>` or `Result<OAuthCreds>` would
print live tokens into logs or panic messages.

**Scenario:** A future developer adds
`.context(|| format!("got tokens: {:?}", tokens))` or `.expect("refresh ok")` on
the `openai_refresh`/`openai_exchange` return, or accesses `OAuthCreds` through
a stored-credential path; the bearer + refresh tokens print to stderr/log. No
active leak path today, but both token-bearing structs are latent foot-guns.

**Fix:** Remove `Debug` from both `OpenAiTokens` and `OAuthCreds` (or implement
`Debug` manually with redaction).

---

### L1 — LOW: Secret-denylist gap: `write`/`edit` don't reject secret _targets_

**`crates/hrdr-tools/src/tools/write.rs:42-85` and `tools/edit.rs:96-186`**

`write` and `edit` resolve the target with `ctx.resolve` and gate on
`read_state`, but never call `guard_secret_read` / `secret_file_reason`. An
existing secret file can't be _over_-written because `read_state` requires a
prior `read` (which `read` refuses). But creating a _new_ file at a
secret-target path — `write` of `~/.aws/credentials` (non-existent in a fresh
setup) — is not mechanically refused by `write`. The zap list is meant to be a
single chokepoint per the docs at `lib.rs:557-559`.

**Fix:** Call `secret_file_reason(&canonicalize_nearest(&path))` at the top of
`write`/`edit` `execute` (before the read-state check) and bail with the same
message `read` uses.

---

### L2 — LOW: Integer overflow in OAuth refresh expiry arithmetic on malformed `expires_in`

**`crates/hrdr-agent/src/oauth.rs:609`** (same bug at `login.rs:680`)

`expires_in * 1000` is plain `u64` multiplication that wraps on values >
`u64::MAX / 1000` (~1.8e16); the subsequent `now_ms() + …` can wrap again,
corrupting `expires_ms`. The result is either a tiny value (token treated as
never-expiring → infinite refresh loop, local DoS wedging the
`RefreshCoordinator`) or a huge value (token never refreshed, stale bearer until
401). Both `oauth.rs:609` and `login.rs:680` have the unchecked arithmetic.

**Scenario:** The OpenAI token endpoint (or a transparent proxy/MITM if TLS is
bypassed) returns `"expires_in": 18446744073709551615`. The `* 1000` wraps to
garbage; `expires_ms` is corrupted. Not a privilege escalation, but a
denial-of-service / correctness bug on untrusted token responses.

**Fix:** Use `tokens.expires_in.unwrap_or(3600).saturating_mul(1000)` and
`now_ms().checked_add(...).unwrap_or(u64::MAX)` at both call sites.

---

### L3 — LOW: Catalog fetch uses unbounded `resp.json()` instead of `read_capped_json`

**`crates/hrdr-llm/src/catalog.rs:295`**

`fetch()` uses `resp.json::<Value>()` — unbounded — unlike the bounded
`read_capped_json` used elsewhere. A hostile or misconfigured `HRDR_MODELS_URL`
could return a many-MB body to inflate memory. No secret leak (no auth sent);
the result is parsed with `serde_json` (safe) and only used for display /
compaction thresholds.

**Fix:** Replace `resp.json::<Value>()` with
`read_capped_json(resp, MAX_STRUCTURED_JSON_BYTES)`.

---

### L4 — LOW: `extra_headers` can override the auth header

**`crates/hrdr-llm/src/client.rs:609-625`**

`auth()` applies the API key (`x-api-key` / `Bearer` / `api-key`) **then**
iterates `extra_headers` appending them _after_. `reqwest::header()` **appends**
(rather than replaces), so the real credential is typically read first by
servers. However, server behavior is not guaranteed — some read the last
occurrence — creating ambiguity. Headers come from `config.toml`
(operator-configured, not LLM-influenced), but the precedence is the opposite of
least-privilege: the credential should win unambiguously.

**Fix:** Apply `extra_headers` before the auth header, or skip/filter
`Authorization` / `x-api-key` / `api-key` names in `extra_headers`.

---

### L5 — LOW: `reqwest::Client::new()` (default) on main LLM client has no timeout and follows redirects

**`crates/hrdr-llm/src/client.rs:455`**

The bare `reqwest::Client::new()` has no connect/read timeout and follows
reqwest's default redirect policy (up to 10). In practice the default
`AgentConfig` supplies `request_timeout: Some(300)` (5 min), so a timeout exists
at the application level. Reqwest strips `Authorization`/`Cookie` on
cross-origin redirects (fetch spec), so API-key leakage to a different host is
_not_ possible. A hung/black-holed provider could still wedge a request for 5
minutes.

**Fix:** Build the client with a generous but finite default timeout in
`Client::new` as a fallback (so a missing config key is still guarded); consider
`redirect(Policy::none())` for chat-completions (always POSTs to a known URL,
never expected to redirect).

---

### L6 — LOW: JWT claims decoded without signature verification for account-id extraction

**`crates/hrdr-agent/src/oauth.rs:462`**

`decode_jwt_claims` base64-decodes the JWT payload and parses JSON without
verifying the HMAC/RSA signature. The extracted `chatgpt_account_id` is used as
the `ChatGPT-Account-Id` header directing which account the bearer token is
billed/routed to. Not an auth bypass — the bearer `access_token` is validated
server-side — but a token with an attacker-chosen `chatgpt_account_id` claim
(crafted without signing) could route API calls under a different account id
header. Mirrors `codex.ts` behavior (same design tradeoff).

**Fix:** This is an accepted design choice mirroring upstream opencode; document
it explicitly, or bind `account_id` only from the server's `id_token` after a
fresh exchange.

---

### L7 — LOW: OAuth `state` validation uses non-constant-time string comparison

**`crates/hrdr-agent/src/oauth.rs:239`**

The CSRF `state` token is compared with `!=` (non-constant-time `String`
partial-eq), which in theory leaks byte-by-byte timing on the 256-bit token.
Practicality is very low (localhost timing, network jitter, 32-byte random
state), but it diverges from best practice.

**Fix:** Use `subtle::ConstantTimeEq` or `constant_time_eq` for the state
comparison.

---

### L8 — LOW: Catalog cache write doesn't set `0600`

**`crates/hrdr-llm/src/catalog.rs:307-321`**

`write_cache` writes via `std::fs::write(&tmp, s)` with no `0600` permission
hardening, unlike `auth.rs::write_atomic` and `client.rs::open_wire_log` which
enforce owner-only on Unix. The catalog data is public model metadata (not
secret), but the temp file is created with umask-default perms and briefly
world-readable; inconsistent with the documented `0600` discipline elsewhere.

**Fix:** Use `OpenOptionsExt::mode(0o600)` directly in `write_cache` — the
`write_atomic` function in `hrdr-agent::auth` cannot be called from `hrdr-llm`
(wrong dependency direction; `hrdr-agent` depends on `hrdr-llm`, not the
reverse). The `open_wire_log` at `client.rs:102-117` in the same crate already
demonstrates the `0600` pattern.

---

### L9 — LOW: Module docs sell hooks as "mechanical like the guardrails" but hooks bypass guardrails

**`crates/hrdr-tools/src/hooks.rs:74-83` and `hooks.rs:245-253`**

`hook.run` is fed to `bash -c` (or `cmd /C`) without passing through
`check_guardrails`. A configured hook template `rm -rf {path}` or
`git push --force` runs unimpeded. Hooks are operator-configured (not
LLM-influenced), so this is expected — but the module-level docs sell hooks as
"mechanical like the guardrails", which can mislead a reader into assuming
they're guarded.

**Fix:** Either route hook commands through `check_guardrails` for consistency,
or add a one-line doc note that hooks intentionally skip the guardrails.

---

### L10 — LOW: Windows `render_command` doesn't escape embedded quotes in file paths

**`crates/hrdr-tools/src/hooks.rs:57-65`**

The Windows arm builds `"\"{}\""` with `path.display()` verbatim — no escaping
of embedded `"`, `%`, or `^` characters. A file path containing a literal `"`
(legal on NTFS) breaks the `cmd.exe` tokenization and can splice an extra
command. The POSIX arm correctly does `'\''` escaping.

**Fix:** On Windows, escape `"` → `""` (cmd.exe convention) inside the quotes,
or use `CreateProcess`-style argument escaping.

---

## Summary

| Severity  | Count                              |
| --------- | ---------------------------------- |
| Critical  | 0                                  |
| High      | 0 (2 already fixed)                |
| Medium    | 4                                  |
| Low       | 10                                 |
| **Total** | **14 unfixed** (16 found, 2 fixed) |

**Overall risk: Medium.** The security-critical paths are well-built:
`fetch`/SSRF guard uses a TOCTOU-free DNS resolver; `SseDecoder` is properly
memory-bounded; the credential store uses atomic write + `0600` + cross-process
locking; PKCE uses a CSPRNG-backed verifier with SHA-256 S256; the untrusted
content envelope uses a verified-absent nonce; secret-denylist coverage is broad
(`read`, `grep`, `git`, `replace`, `fileops`, `lsp_nav`); the
`canonicalize_nearest` helper exists exactly to prevent `..` path escapes;
shell-guardrail command tokenization is robust; process-tree killing is correct
on both platforms; hook templates handle TUI-stdin issues. No critical
pathologies were found: no MD5/SHA1, no hardcoded secrets, no panics on
untrusted SSE input, no buffer overflows, no data races, no unbounded allocation
in hot paths.

The two HIGH findings are already fixed: MCP SSE endpoint validation (`ab2f1b7`)
and LSP `check_confined` fallback (`98a86b3`). The remaining gaps cluster
around: **guardrail bypasses** (depth cap M2, unbalanced-quote fallback M3),
**secret-denylist coverage** (read TOCTOU M1, write/edit gap L1), and **OAuth
hygiene** (Debug leaks M4, overflow L2, timing L7).

**Top 3 remaining fixes (by impact):**

1. **M1 — Make `read` open-then-validate-then-read** through one file
   descriptor, closing the symlink-swap TOCTOU that bypasses the secret
   denylist.
2. **M2 — Drop the guardrail depth cap** and bound by total extracted length
   instead, closing the 5-level `sh -c` nesting bypass.
3. **M3 — Fix the guardrail unbalanced-quote fallback** — strip unbalanced
   quotes before matching, or always match raw input as defense-in-depth.
