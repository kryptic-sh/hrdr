# Security & Correctness Audit

**Date:** 2026-07-22 · **Remediated & re-reviewed:** 2026-07-23 · **Depth:**
High · **Scope:** Full codebase — all crates (`hrdr-tools`, `hrdr-llm`,
`hrdr-agent`, `hrdr-app`, `hrdr-editor`, `hrdr-tui`, `hrdr` binary) and all
source files.

## Methodology

The attack surface was mapped by identifying entry points: HTTP handlers
(`fetch`, `search`, MCP HTTP/SSE transports), CLI args (`clap` in `main.rs`),
file parsers (read/write/edit/replace/grep tools), IPC (MCP stdio/HTTP, LSP),
and environment reads (`HRDR_*` env vars, `HOME`/`XDG` paths). Each class of
vulnerability was checked systematically against every source file: injection,
memory/resource, crypto, AuthZ/AuthN, data integrity, error handling, and
concurrency.

Findings were verified by re-reading surrounding code, tracing callers, and
constructing concrete trigger scenarios. The original pass found 16 issues; the
remediation and this re-review track them below.

---

## Resolved

Detailed entries pruned — each original finding is recorded here with the commit
that fixed it. Items with a residual left after the fix are tracked in **Open**.

| ID  | Finding                                                       | Fixed in  | Residual |
| --- | ------------------------------------------------------------- | --------- | -------- |
| H1  | MCP SSE `endpoint` SSRF — host validated against the base     | `ab2f1b7` | —        |
| H2  | LSP `check_confined` `..` escape — canonicalized fallback     | `98a86b3` | —        |
| M1  | `read` secret-denylist TOCTOU — open → dev/ino verify → read  | `e314853` | O3       |
| M2  | Guardrail depth cap — replaced with a 64 KiB cumulative bound | `e314853` | —        |
| L1  | `write`/`edit` didn't reject secret _targets_                 | `e314853` | —        |
| L2  | OAuth expiry overflow — `saturating_add`/`saturating_mul`     | `65a425d` | —        |
| L3  | Catalog fetch unbounded — `read_capped_json`                  | `910ccee` | —        |
| L4  | `extra_headers` auth precedence — applied before auth header  | `910ccee` | O4       |
| L5  | LLM client had no default timeout — 300 s fallback            | `910ccee` | —        |
| L6  | JWT claims unverified — documented as a routing hint only     | `65a425d` | —        |
| L7  | OAuth `state` non-constant-time — `constant_time_eq`          | `65a425d` | —        |
| L8  | Catalog cache not `0600` — `OpenOptionsExt::mode(0o600)`      | `910ccee` | —        |
| L9  | Hooks docs misleading — noted they bypass the guardrails      | `e314853` | —        |
| L10 | Windows hook path quotes unescaped — `"` → `""`               | `e314853` | O5       |
| O1  | Force-push guardrail bypass via `'"--force` mid-command quote  | `5a2f644` | —        |

---

## Open findings (from the 2026-07-23 remediation re-review, most-severe first)

---

### O2 — MEDIUM: `AuthEntry` still derives `Debug` over live tokens (M4 incomplete)

**`crates/hrdr-agent/src/auth_store.rs:40`**

The M4 fix (commit `65a425d`) removed `Debug` from `OpenAiTokens` and
`OAuthCreds`, but `AuthEntry` — the **persisted** credential enum — still
`#[derive(Debug, …)]`, and its `Oauth` variant holds the same live secrets
(`access: String`, `refresh: String`). A `{:?}` on an `AuthEntry` (or `anyhow`
context / `unwrap` on a `Result<AuthEntry>`) leaks the bearer + refresh tokens —
the exact latent foot-gun the fix set out to remove. The original audit named
only the two `oauth.rs` structs and missed this one, so the fix's intent (no
token-bearing struct derives `Debug`) is not achieved.

**Fix:** drop `Debug` from `AuthEntry` too, or give it a manual `Debug` that
redacts `access`/`refresh` (mirroring `OAuthAccess`, which omits `Debug`
deliberately).

---

### O3 — LOW: `read` TOCTOU dev/ino re-check is Unix-only (M1 residual)

**`crates/hrdr-tools/src/tools/read.rs`** — the `#[cfg(unix)]` dev/ino block

The M1 fix opens the file first and reads through the handle, then re-checks the
opened descriptor's `dev`/`ino` against the canonical path — but only under
`#[cfg(unix)]`. On Windows there is no such re-check, so the narrow
open-secret-then-swap-to-non-secret race (open resolves to a secret, the path is
then repointed at a non-secret before `guard_secret_read` canonicalizes it) is
not caught, and the tool reads the pre-swap handle (the secret). Low: swapping a
file that is already open is much harder on Windows, and the audit's concrete
scenario was Unix symlinks.

**Fix:** add a Windows identity re-check (e.g. `BY_HANDLE_FILE_INFORMATION`
volume-serial + file-index via `GetFileInformationByHandle`), or document the
platform limitation.

---

### O4 — LOW: `extra_headers` can still emit a duplicate `Authorization` (L4 residual)

**`crates/hrdr-llm/src/client.rs`** — `auth()`

The L4 fix now applies `extra_headers` **before** the auth header, so the real
credential is the _last_ `Authorization`/`x-api-key`. But
`RequestBuilder::header` **appends**, so if `extra_headers` itself contains an
auth-type header the request still carries two — and which one a server/proxy
honors (first vs last) is undefined. The audit's second option (filter
`Authorization` / `x-api-key` / `api-key` names out of `extra_headers`) removes
the ambiguity entirely. Low — `extra_headers` is operator-configured, not
LLM-influenced.

**Fix:** skip auth-header names when applying `extra_headers`.

---

### O5 — LOW: Windows hook path escaping omits `%` and `^` (L10 residual)

**`crates/hrdr-tools/src/hooks.rs`** — `render_command` (Windows arm, ~L57-60)

The L10 fix escapes embedded `"` → `""` (closing the command-splice vector), but
a file path containing `%` (cmd.exe env-var expansion) or `^` (escape char) is
still substituted verbatim. Not a command-injection splice, but it can corrupt
the rendered command or expand an environment variable. Low.

**Fix:** also neutralize `%` and `^` (e.g. via delayed-expansion-safe quoting or
`CreateProcess`-style argument construction).

---

## Summary

| Severity  | Open  | Resolved |
| --------- | ----- | -------- |
| Critical  | 0     | 0        |
| High      | 0     | 2        |
| Medium    | 2     | 2        |
| Low       | 3     | 10       |
| **Total** | **5** | **14**   |

**Overall risk: Low.** The security-critical paths remain well-built:
`fetch`/SSRF guard uses a TOCTOU-free DNS resolver; `SseDecoder` is properly
memory-bounded; the credential store uses atomic write + `0600` + cross-process
locking; PKCE uses a CSPRNG-backed verifier with SHA-256 S256; the untrusted
content envelope uses a verified-absent nonce; secret-denylist coverage is broad
(`read`, `grep`, `git`, `replace`, `fileops`, `lsp_nav`, and now
`write`/`edit`); `canonicalize_nearest` prevents `..` path escapes. No critical
pathologies: no MD5/SHA1, no hardcoded secrets, no panics on untrusted SSE
input, no buffer overflows, no data races, no unbounded allocation in hot paths.

Both HIGH findings and the bulk of the Medium/Low set are fixed. What remains
after the remediation re-review:

1. **O1 (M3) — the guardrail quote-bypass fix is ineffective** and must be
   redone (strip all quotes, not just edges) with a test for the real example.
2. **O2 (M4) — `AuthEntry` still leaks tokens via `Debug`**; the fix must extend
   to it.
3. Three low residuals (O3–O5) left by otherwise-correct fixes.
