# Security Audit — Open Items

- **Last pruned:** 2026-07-22 (against HEAD, version `0.7.0`)
- **Scope:** only the still-open items. Two audit passes ran against this
  codebase; every **confirmed** finding is fixed. The full record of the 24
  pass-1 findings and the 5 pass-2 findings, with per-finding resolving commits,
  is preserved in git history (see commit `2072045`, the last version of this
  file before pruning).

No open security findings remain.

---

_The OpenRouter OAuth `state` finding (pass 2, finding 3) was resolved — the
flow now mints a random `state`, embeds it in the callback URL, and validates it
on the callback (OpenRouter's OAuth PKCE upgrade added `state` support). Pending
one live-login sanity check of the round-trip._

_The `openai` + `chatgpt`/`codex` provider merge (was issue #21) shipped — one
`openai` provider derives endpoint / kind / catalog from the stored credential
(key **or** OAuth, XOR-enforced in `auth.json`)._
