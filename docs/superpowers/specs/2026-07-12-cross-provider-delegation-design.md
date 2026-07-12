# Ad-hoc cross-provider sub-agent delegation

**Status:** design approved, pending spec review **Base branch:**
`fix-chatgpt-oauth-model-discovery-v2` (PR #7). Depends on #7, **not** on #8
(introspection). Ships as a second PR stacked on #7; rebase onto `main` when #7
merges. **Date:** 2026-07-12

## Goal

Let the main agent delegate a `task` to **any provider + model that is already
configured and authenticated**, chosen at delegation time — without first
defining a `[[subagent]]` profile for it.

Today cross-provider delegation is possible **only** via a named `[[subagent]]`
profile (`config_for_agent_profile`, `crates/hrdr-agent/src/lib.rs:400`). The
`task` tool's `model` argument overrides the model **on the parent's provider
only** (`lib.rs:640`), so e.g. a chatgpt-hosted main agent cannot delegate to
`openrouter/deepseek-*` ad-hoc. This was never a feature (verified against
`upstream/main` and `fix-...v2`: the `model` arg is documented "on the selected
provider" and the `task` schema has no `provider` argument).

## Non-goals (YAGNI)

- **No** `--subagent-provider` CLI flag or `subagent_provider` config key. Scope
  is the in-turn `task` tool only (decided during brainstorming).
- **No** TUI affordance for a live default sub-provider.
- **No** model pre-validation. A bad model id is passed through; the provider's
  API rejects it on the sub's first request (decided during brainstorming — a
  pre-check adds a round-trip and would wrongly reject models a provider does
  not enumerate). The provider itself, being resolvable + auth'd, is checked
  before spawn (see Auth gate).

## Interface

Add one optional argument, `provider`, to the `task` tool
(`SubagentTool::parameters`, `lib.rs:561`):

```jsonc
task(
  prompt:     "...",                 // required, unchanged
  provider:   "openrouter",          // NEW, optional
  model:      "deepseek/deepseek-chat",
  agent:      "...",                 // optional, unchanged
  description:"...",                 // optional, unchanged
  background: true                   // optional, unchanged
)
```

`provider` accepts any built-in provider name (`zen`, `openai`, `openrouter`,
`claude`, `local`, `chatgpt`/`codex`) or a `[providers.<name>]` defined in
`config.toml` — the same set `resolve_provider` already accepts.

Also reword the `model` arg description: drop "(on the selected provider)",
since the provider is now selectable.

## Semantics & precedence

- **`provider` omitted** → behaviour is byte-identical to today (sub inherits
  the parent endpoint; `model` overrides the model on that same provider). This
  is a hard regression-guard requirement, not a nicety.
- **`provider` present** → repoint the sub to that provider (endpoint + auth +
  headers + context window), run `model` there.
- **`provider` present, `model` omitted:**
  - provider has a preset default model (only `chatgpt` does today → `gpt-5.5`)
    → use it.
  - provider has no default (openrouter, openai, zen, claude, local all have
    `model: None`) → **hard error** before spawn:
    `task: provider 'X' requires an explicit model (it has no default)`. This is
    a **new** check on the #7 base, which has only the generic
    `model == "default"` bail (`lib.rs:649`) — that fires on the sentinel, not
    on "provider has no default", so it would not catch this case.
- **`provider` + `agent` profile together** → the profile resolves first (its
  persona + provider + model), then `provider`/`model` apply as overrides on
  top. Ad-hoc always wins. This is why the repoint must be persona-preserving
  (see below).

## Resolution design

### Extract a persona-preserving repoint helper

The provider-repoint block inside `config_for_agent_profile` (`lib.rs:405-430`:
`resolve_provider` → `base_url`, `resolve_api_key`, `api_version`, `headers`,
`context_window`, `model`) must be reused by the ad-hoc path. It **cannot** be
reused by passing a transient `SubagentProfile` through
`config_for_agent_profile`, because that function also overwrites the persona
fields (`agent_prompt`, `allowed_tools`, `read_only`, `write_ext`,
`lib.rs:437-440`) — which would wipe a profile's persona when `provider` is an
override on top of `agent`.

Extract the endpoint/auth/model repoint into a focused helper:

```rust
/// Repoint `cfg` at `pname`'s endpoint + auth (+ its default or the given
/// model). Endpoint/identity only — does NOT touch persona/tool-scope, so it is
/// safe to layer on top of an already-resolved agent profile.
fn repoint_to_provider(
    cfg: &mut AgentConfig,
    pname: &str,
    model_override: Option<&str>,
) -> Result<()>;
```

`config_for_agent_profile` calls it for its provider branch; the `task` call
path calls it for the ad-hoc `provider` argument. One repoint implementation,
one place to get right.

### Provider identity fix (correctness)

The current repoint sets `base_url` + `api_key` but **never sets
`cfg.provider`** (`config_for_agent_profile` goes from `lib.rs:430` straight to
the model assignment; there is no `cfg.provider = …`). `AgentConfig` has no
stored `provider_kind`; the sub's `Agent::new` **derives** it from
`cfg.provider`. So a repointed sub currently keeps the _parent's_ provider name
and therefore a _wrong_ derived `provider_kind` (e.g. a chatgpt→openrouter sub
would derive `ChatGptOAuth`).

`repoint_to_provider` fixes this: after repointing, set
`cfg.provider = Some(pname.to_string())` so the derived `provider_kind` matches
the endpoint. This corrects the existing profile path (a latent bug) as well as
the new ad-hoc path — an in-scope fix because the feature depends on correct
provider identity (e.g. the chatgpt-as-target OAuth path keys off it).

### Auth gate (the "configured/authenticated" requirement)

The gate lives in the **ad-hoc `task` path only**, not inside the shared
`repoint_to_provider` helper — so existing `[[subagent]]` profiles keep their
current behaviour unchanged. (Folding the gate into the shared helper would also
gate every named profile, changing shipped #7 behaviour and breaking
`subagent_profile_repoints_to_a_different_provider` (`lib.rs:4614`), which
repoints to openrouter with no configured auth and expects success. Out of scope
here; a follow-up could unify.)

Before the ad-hoc path calls `repoint_to_provider`, resolve the target provider
and gate:

- **Unknown provider** → hard error listing built-ins + `[providers.*]`. (The
  same `resolve_provider(...).ok_or_else(...)` inside the helper also rejects an
  unknown provider at `lib.rs:406-413`; the ad-hoc gate reuses that path.)
- **Un-authenticated remote provider** → compute
  `provider_auth_state(pname, &p, cfg.api_key.as_deref(), Some(&cfg.base_url))`
  (`lib.rs:1593`), where `cfg` still holds the **parent's** key/base_url at this
  point. If `Missing` → hard error **before spawn**:
  `task: provider 'X' is not configured — set $<KEY_ENV>, or run /login`.
  `OAuth` (chatgpt), `Keyless` (local), and `Key` all pass.

**Ordering (required):** run the auth gate on the parent's `cfg.api_key` /
`cfg.base_url` (the inheritance context) **before** calling
`repoint_to_provider`, which overwrites those fields. Gating after the repoint
would test the target provider's key against its own freshly-set endpoint —
wrong result. `resolve_api_key` inside the helper reads the same parent values
via its own arguments, so pass them from the pre-repoint `cfg`.

### Model-argument reconciliation

The existing unconditional `model` override (`lib.rs:640-646`) applies after the
agent-match. With `provider` present, `model` is consumed by
`repoint_to_provider` instead. The implementation must apply `model` **once**:
via the repoint when `provider` is present, via the existing block otherwise.
(Detail for the implementation plan; the observable rule is "model applies
exactly once, on the resolved provider".)

## Edge cases

- **chatgpt as target** (non-chatgpt main → chatgpt sub): the repoint sets
  `base_url = CHATGPT_CODEX_BASE_URL`; the codex/Responses backend is selected
  by base URL and injects the OAuth bearer at request time
  (`crates/hrdr-llm/src/client.rs`, codex backend) regardless of `api_key`. The
  provider-identity fix keeps `provider_kind = ChatGptOAuth` consistent.
  Requires #7's OAuth (hence #7 is the base).
- **Unchanged by design:** isolation (profile-only; ad-hoc sets none),
  background/foreground detach, cost tracking (priced by model id from the
  models.dev catalog — a cross-provider model is priced by its own id),
  concurrency (`concurrent()` stays true; each sub builds its own client),
  reasoning effort (inherited from the parent; ignored by models that do not use
  it).

## Testing

All auth/endpoint resolution is **local** — `resolve_provider`,
`resolve_api_key`, `provider_auth_state` need no network — so these are unit
tests:

1. **Repoint:** `repoint_to_provider(cfg, "openrouter", Some("m"))` yields the
   openrouter base_url, an api_key resolved from `auth.toml`/`key_env`,
   `cfg.provider == Some("openrouter")`, and a derived `provider_kind` matching
   openrouter.
2. **Auth gate:** a remote provider with no key/oauth → error before spawn (no
   network); unknown provider name → error; provider-without-default + no
   `model` → the "requires an explicit model" error.
3. **Regression guard:** `provider` omitted → resolution byte-identical to the
   pre-change path (assert against `self.base.clone()` + existing model
   override).
4. **Precedence:** `provider` + `agent` profile → endpoint/model come from the
   ad-hoc override while the profile's persona (`agent_prompt`, `read_only`)
   survives (guards the persona-preservation requirement).
5. **Identity fix (existing path):** a profile that names a provider now yields
   a matching `cfg.provider` + derived kind.

**Live end-to-end (the `verify` step, uses the real openrouter key in
`~/.config/hrdr/auth.toml`):** chatgpt main agent delegates with
`task(provider: "openrouter", model: <valid slug>, …)`; confirm the sub actually
runs on openrouter (the sub reports its resolved model). Find a valid slug with
`hrdr --provider openrouter models` (the CLI fixed in #7).

## Surfaces / docs to update

- `SubagentTool::parameters` (`lib.rs:561`) — add the `provider` property;
  reword the `model` description.
- The `task` tool top-level description (mentions provider selection).
- `README.md` delegation section.
- `CHANGELOG.md` under `[Unreleased]` — feature entry (ad-hoc cross-provider
  delegation) + the provider-identity correctness fix. No existing-profile
  behaviour change (the auth gate is ad-hoc-only).

## Integration

New branch `feat/cross-provider-delegation` off
`fix-chatgpt-oauth-model-discovery-v2`. PR into upstream, stacked on #7 (sibling
to #8, independent of it). Merge into the local `personal` integration branch
for daily use. When #7 lands on `main`, rebase this branch onto `main`.
