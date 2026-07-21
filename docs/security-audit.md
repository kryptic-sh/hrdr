# Security Audit — Open Items

- **Last pruned:** 2026-07-22 (against HEAD, version `0.7.0`)
- **Scope:** only the still-open items. Two audit passes ran against this
  codebase; every **confirmed** finding is fixed. The full record of the 24
  pass-1 findings and the 5 pass-2 findings, with per-finding resolving commits,
  is preserved in git history (see commit `2072045`, the last version of this
  file before pruning).

Two items remain — both deferred on an external confirmation or a decision, not
on more engineering.

## 1. OpenRouter OAuth flow has no CSRF `state` parameter (Medium, defense-in-depth)

**Location:** `crates/hrdr-agent/src/oauth.rs:280-287`;
`crates/hrdr-app/src/login.rs:344,467`

`openrouter_authorize_url` builds the authorize URL from `callback_url` +
`code_challenge` + `code_challenge_method=S256` only — no `state`. The callback
is awaited with an empty `expected_state`, so `parse_callback`'s check passes
for any callback that omits `state`.

**Concrete scenario:** a local attacker delivers a crafted callback to the
loopback listener with an attacker-supplied `code` before the real browser
redirect arrives.

**Why it's deferred, not skipped:** strict `state` validation would break
OpenRouter login if the provider does not echo `state` back in its callback —
and the existing code comment says it does not. opencode, checked as a reference
implementation, authenticates OpenRouter with a plain bearer API key and has
**no** PKCE/`state` flow at all, so it could not confirm the provider's
behavior.

**Mitigations already present (why it is defense-in-depth):** the listener binds
`127.0.0.1` only, so a _local_ attacker is required; PKCE binds the exchange
(`openrouter_exchange` sends the `code_verifier`), and a code the attacker
obtained was issued against the attacker's own challenge, so exchanging it with
our verifier fails at OpenRouter. Worst realistic case is a local denial of the
login attempt, not credential substitution.

**Unblock + fix:** confirm OpenRouter's `state`-echo behavior from their docs or
a live callback capture. If it echoes `state`, mint and pass it exactly as the
ChatGPT/OpenAI flow already does — `generate_state()` (`oauth.rs:101`) exists
and is used there (verified via `await_oauth_code_within`).

## 2. Unify `openai` + `chatgpt` into one provider — GitHub issue #21

Not a security finding — a UX/architecture change, deferred by request. Collapse
the separate `chatgpt` (OAuth, Codex endpoint) and `openai` (API key, standard
endpoint) providers into a single `openai` that authenticates with **either** a
key or OAuth, never both. The complication is that endpoint, model catalog, and
`ResolvedProviderKind` currently differ by credential type, so a unified
provider must derive base URL / kind / catalog from whichever credential is
stored.

Full design (touch points, alias/back-compat, the both-credentials collision) is
captured in **issue #21**. Depends on the unified `auth.json` credential store,
which has already landed.
