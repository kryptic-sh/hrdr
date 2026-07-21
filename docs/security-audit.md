# Security Audit — Open Items

- **Last pruned:** 2026-07-22 (against HEAD, version `0.7.0`)
- **Scope:** only the still-open items. Two audit passes ran against this
  codebase; every **confirmed** finding is fixed. The full record of the 24
  pass-1 findings and the 5 pass-2 findings, with per-finding resolving commits,
  is preserved in git history (see commit `2072045`, the last version of this
  file before pruning).

One item remains — an architecture/UX change, not a security finding.

## Unify `openai` + `chatgpt` into one provider — GitHub issue #21

Collapse the separate `chatgpt` (OAuth, Codex endpoint) and `openai` (API key,
standard endpoint) providers into a single `openai` that authenticates with
**either** a key or OAuth, never both. The complication is that endpoint, model
catalog, and `ResolvedProviderKind` currently differ by credential type, so a
unified provider must derive base URL / kind / catalog from whichever credential
is stored.

Full design (touch points, alias/back-compat, the both-credentials collision) is
captured in **issue #21**. Depends on the unified `auth.json` credential store,
which has already landed.

---

_The OpenRouter OAuth `state` finding (pass 2, finding 3) was resolved — the
flow now mints a random `state`, embeds it in the callback URL, and validates it
on the callback (OpenRouter's OAuth PKCE upgrade added `state` support). Pending
one live-login sanity check of the round-trip._
