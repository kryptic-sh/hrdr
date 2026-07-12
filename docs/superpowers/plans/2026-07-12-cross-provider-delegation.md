# Ad-hoc Cross-Provider Delegation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `provider` argument to the `task` tool so the main agent can
delegate to any configured + authenticated provider/model at delegation time,
not only via a predefined `[[subagent]]` profile.

**Architecture:** Extract the provider-repoint block already inside
`config_for_agent_profile` into a private `repoint_to_provider` helper
(endpoint + auth + model + provider identity, no persona). Reuse it from both
the profile path and a new ad-hoc `task` path. The ad-hoc path adds a fail-fast
auth gate via `provider_auth_state`; named profiles keep their current
behaviour.

**Tech Stack:** Rust (edition 2024), `anyhow`, sea-of-`serde_json` tool schemas.
All changes live in `crates/hrdr-agent/src/lib.rs` (+ its inline
`#[cfg(test)] mod tests`), plus `README.md` and `CHANGELOG.md`.

## Global Constraints

- **Base branch:** `feat/cross-provider-delegation`, off
  `fix-chatgpt-oauth-model-discovery-v2` (PR #7). Already created.
- **Rust CI gate (must pass before every commit that touches Rust):**
  `cargo fmt`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --locked`.
- **Regression guard (hard requirement):** with `provider` omitted, `task`
  resolution is byte-identical to today. A named `[[subagent]]` profile's
  behaviour is unchanged (the auth gate is ad-hoc-path only).
- **Commits:** Conventional Commits. No AI attribution anywhere.
- **Markdown:** run `prettier --write` on any changed `.md`.
- Spec: `docs/superpowers/specs/2026-07-12-cross-provider-delegation-design.md`.

## File Structure

- `crates/hrdr-agent/src/lib.rs` — all runtime changes:
  - new private `fn repoint_to_provider` (Task 1)
  - refactor `pub fn config_for_agent_profile` to call it (Task 1)
  - new private `fn apply_task_overrides` (Task 2)
  - `SubagentTool::parameters` — add `provider` prop, reword `model` (Task 2)
  - `SubagentTool::execute` — parse `provider`/`model`, call
    `apply_task_overrides` (Task 2)
  - tests in the same file's `mod tests` (Tasks 1 & 2)
- `README.md` — delegation section (Task 3)
- `CHANGELOG.md` — `[Unreleased]` (Task 3)

---

### Task 1: `repoint_to_provider` helper + provider-identity fix

Extract the repoint logic and fix the latent gap where a repointed sub kept the
parent's `provider` name (and thus a wrong derived `provider_kind`).
Behaviour-preserving for existing profiles **except** it now sets
`cfg.provider`.

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs` — add helper near
  `config_for_agent_profile` (~`:400`); refactor its provider block
  (`:405-434`).
- Test: `crates/hrdr-agent/src/lib.rs` `mod tests` — extend
  `subagent_profile_repoints_to_a_different_provider` (`:4614`); add
  `repoint_to_provider_sets_identity_and_model`.

**Interfaces:**

- Produces:
  `fn repoint_to_provider(cfg: &mut AgentConfig, pname: &str, model_override: Option<&str>) -> anyhow::Result<()>`
  — resolves `pname`, overwrites `cfg.base_url`, `cfg.api_key`,
  `cfg.api_version`, `cfg.headers`, `cfg.context_window`, sets
  `cfg.provider = Some(pname)`, and sets `cfg.model` to `model_override` (else
  the provider's preset default, else unchanged). Errors only on unknown
  provider.

- [ ] **Step 1: Add the identity assertion to the existing repoint test
      (failing).**

In `subagent_profile_repoints_to_a_different_provider` (`:4614`) add two
assertions — each **after the variable it references is bound**. (Inserting both
right after the `sub.base_url` assert would reference `same` before its `let` at
`:4645`, an `E0425` compile error — not the intended assertion failure.)

Add immediately after
`assert_eq!(sub.agent_prompt.as_deref(), Some("Implement precisely."));`
(`:4643`):

```rust
        // Identity: the sub is now *on* openrouter, not the parent provider.
        assert_eq!(sub.provider.as_deref(), Some("openrouter"));
```

Add immediately after `assert_eq!(same.model, "claude-haiku");` (`:4665`):

```rust
        // The no-provider profile keeps the parent's provider identity (None here).
        assert_eq!(same.provider.as_deref(), None);
```

(The test's `cfg` is built with `..Default::default()`, so `provider` is `None`;
`subagent_base_config` clones without touching it, and the no-provider profile
must leave it `None`.)

- [ ] **Step 2: Run it, verify it fails.**

Run:
`cargo test -p hrdr-agent subagent_profile_repoints_to_a_different_provider --locked`
Expected: FAIL — `sub.provider` is currently `None` (repoint never sets it).

- [ ] **Step 3: Add the `repoint_to_provider` helper.**

Insert immediately **above** `pub fn config_for_agent_profile` (`:400`):

```rust
/// Repoint `cfg` at `pname`'s endpoint + auth (and its default or the given
/// model). Endpoint/identity only — does NOT touch persona/tool-scope, so it is
/// safe to layer on top of an already-resolved agent profile.
///
/// `model_override` wins over the provider's preset default; when neither is
/// present `cfg.model` is left unchanged. Reads the caller's (parent's)
/// `cfg.api_key` / `cfg.base_url` as the key-inheritance context *before*
/// overwriting them.
fn repoint_to_provider(
    cfg: &mut AgentConfig,
    pname: &str,
    model_override: Option<&str>,
) -> Result<()> {
    let p = cfg.resolve_provider(pname).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{pname}' (built-ins: {}, or define [providers.{pname}])",
            BUILTIN_PROVIDERS.join(", ")
        )
    })?;
    // Resolve the key from the PARENT's context first, before mutating cfg.
    let key = resolve_api_key(pname, &p, cfg.api_key.as_deref(), Some(cfg.base_url.as_str()));
    cfg.base_url = p.base_url.clone();
    cfg.api_key = key;
    cfg.api_version = p.api_version.clone();
    cfg.headers = p
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    cfg.context_window = p.context_window;
    cfg.provider = Some(pname.to_string());
    if let Some(m) = model_override {
        cfg.model = m.to_string();
    } else if let Some(m) = &p.model {
        cfg.model = m.clone();
    }
    Ok(())
}
```

- [ ] **Step 4: Refactor `config_for_agent_profile`'s provider block to use
      it.**

Replace the current block (`:405-434`):

```rust
    if let Some(pname) = profile.provider.as_deref() {
        let p = base.resolve_provider(pname).ok_or_else(|| {
            anyhow::anyhow!(
                "subagent '{}': unknown provider '{pname}' (built-ins: {}, or define \
                 [providers.{pname}])",
                profile.name,
                BUILTIN_PROVIDERS.join(", ")
            )
        })?;
        cfg.base_url = p.base_url.clone();
        cfg.api_key = resolve_api_key(
            pname,
            &p,
            base.api_key.as_deref(),
            Some(base.base_url.as_str()),
        );
        cfg.api_version = p.api_version.clone();
        cfg.headers = p
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cfg.context_window = p.context_window;
        if let Some(m) = &p.model {
            cfg.model = m.clone();
        }
    }
    if let Some(m) = &profile.model {
        cfg.model = m.clone();
    }
```

with:

```rust
    if let Some(pname) = profile.provider.as_deref() {
        repoint_to_provider(&mut cfg, pname, profile.model.as_deref())?;
    } else if let Some(m) = &profile.model {
        cfg.model = m.clone();
    }
```

(`repoint_to_provider` applies `profile.model` as the override, else the
provider default — identical to the two old blocks — and additionally sets
`cfg.provider`.)

- [ ] **Step 5: Add a direct helper unit test.**

Add to `mod tests`:

```rust
    #[test]
    fn repoint_to_provider_sets_identity_and_model() {
        use super::repoint_to_provider;
        // Start on a fake parent endpoint; repoint to the `local` built-in.
        let mut cfg = AgentConfig {
            base_url: "https://api.anthropic.com/v1".to_string(),
            api_key: Some("parent-key".to_string()),
            model: "claude-opus".to_string(),
            provider: Some("claude".to_string()),
            ..Default::default()
        };
        repoint_to_provider(&mut cfg, "local", Some("my-local-model")).unwrap();
        assert_eq!(cfg.base_url, "http://localhost:8080/v1");
        assert_eq!(cfg.provider.as_deref(), Some("local"));
        assert_eq!(cfg.model, "my-local-model");
        // Unknown provider errors.
        assert!(repoint_to_provider(&mut cfg, "nope", Some("m")).is_err());
    }
```

Do **not** assert `cfg.api_key == None` here: `local`'s `key_env` is
`HRDR_API_KEY`, which `resolve_api_key` reads from the environment
(`lib.rs:1568`), so on a dev box with that variable exported the key would be
`Some(...)` and the assertion would flake. Key-inheritance safety is already
covered hermetically by
`resolve_api_key_does_not_leak_parent_key_across_providers` (`lib.rs:4691`).

- [ ] **Step 6: Run the gate.**

Run:

```bash
cargo test -p hrdr-agent --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt
```

Expected: all tests pass (the extended repoint test + the new one), clippy
clean.

- [ ] **Step 7: Commit.**

```bash
git add crates/hrdr-agent/src/lib.rs
git commit -m "refactor(agent): extract repoint_to_provider + fix sub provider identity"
```

---

### Task 2: `provider` argument on the `task` tool

Add the schema property and the ad-hoc resolution path (auth gate + repoint),
wired into `SubagentTool::execute`.

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs` — add `fn apply_task_overrides`;
  `SubagentTool::parameters` (`:561`); `SubagentTool::execute` model-override
  region (`:640-646`).
- Test: `crates/hrdr-agent/src/lib.rs` `mod tests` — add
  `apply_task_overrides_*` tests.

**Interfaces:**

- Consumes: `repoint_to_provider` (Task 1).
- Produces:
  `fn apply_task_overrides(cfg: &mut AgentConfig, provider: Option<&str>, model: Option<&str>) -> anyhow::Result<()>`
  — with `provider`: auth-gate then repoint (`model` is the override); without:
  apply `model` on the current provider.

- [ ] **Step 1: Write failing tests for `apply_task_overrides`.**

Add to `mod tests`:

```rust
    #[test]
    fn apply_task_overrides_provider_repoints_and_gates() {
        use super::{ProviderConfig, apply_task_overrides};
        use std::collections::HashMap;
        let mut base = AgentConfig {
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key: None,
            model: "gpt-5.6-sol".to_string(),
            provider: Some("chatgpt".to_string()),
            ..Default::default()
        };
        // A custom remote provider with NO key anywhere → Missing → gate errors.
        base.providers.insert(
            "ghost".to_string(),
            ProviderConfig {
                base_url: "https://ghost.example/v1".to_string(),
                key_env: None,
                api_key: None,
                model: None,
                remote: Some(true),
                context_window: None,
                headers: HashMap::new(),
                api_version: None,
            },
        );

        // (a) un-authenticated provider → fail fast, no repoint.
        let mut cfg = base.clone();
        let err = apply_task_overrides(&mut cfg, Some("ghost"), Some("m"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not configured"), "got: {err}");
        assert_eq!(cfg.base_url, base.base_url); // unchanged on error

        // (b) keyless `local` (built-in) with a model → repoints + identity.
        let mut cfg = base.clone();
        apply_task_overrides(&mut cfg, Some("local"), Some("deepseek-x")).unwrap();
        assert_eq!(cfg.base_url, "http://localhost:8080/v1");
        assert_eq!(cfg.provider.as_deref(), Some("local"));
        assert_eq!(cfg.model, "deepseek-x");

        // (c) provider without a default model and no model arg → error.
        let mut cfg = base.clone();
        let err = apply_task_overrides(&mut cfg, Some("local"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires an explicit model"), "got: {err}");

        // (d) unknown provider → error.
        let mut cfg = base.clone();
        assert!(apply_task_overrides(&mut cfg, Some("nope"), Some("m")).is_err());

        // (e) no provider, just a model → override on the current provider.
        let mut cfg = base.clone();
        apply_task_overrides(&mut cfg, None, Some("gpt-5.5")).unwrap();
        assert_eq!(cfg.base_url, base.base_url); // still chatgpt endpoint
        assert_eq!(cfg.model, "gpt-5.5");
        assert_eq!(cfg.provider.as_deref(), Some("chatgpt"));

        // (f) neither → no-op.
        let mut cfg = base.clone();
        apply_task_overrides(&mut cfg, None, None).unwrap();
        assert_eq!(cfg.model, "gpt-5.6-sol");
    }

    // Spec Testing #4 — precedence: an ad-hoc provider/model override layered on
    // a resolved agent profile wins on endpoint + model, while the profile's
    // persona survives (repoint is persona-preserving).
    #[test]
    fn apply_task_overrides_wins_over_profile_but_keeps_persona() {
        use super::{SubagentProfile, apply_task_overrides, config_for_agent_profile, subagent_base_config};
        let parent = AgentConfig {
            base_url: "https://api.anthropic.com/v1".to_string(),
            api_key: Some("parent-key".to_string()),
            model: "claude-opus".to_string(),
            provider: Some("claude".to_string()),
            ..Default::default()
        };
        // Resolve a profile with a persona + its own model, no provider (stays
        // on the parent endpoint).
        let prof = SubagentProfile {
            name: "reviewer".to_string(),
            provider: None,
            model: Some("claude-sonnet".to_string()),
            description: None,
            prompt: Some("Review only.".to_string()),
            read_only: true,
            tools: None,
            write_ext: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: false,
            isolation: None,
        };
        let mut cfg = config_for_agent_profile(&subagent_base_config(&parent), &prof).unwrap();
        // Ad-hoc override to a different provider + model.
        apply_task_overrides(&mut cfg, Some("local"), Some("adhoc-model")).unwrap();
        // Endpoint + model come from the ad-hoc override.
        assert_eq!(cfg.base_url, "http://localhost:8080/v1");
        assert_eq!(cfg.provider.as_deref(), Some("local"));
        assert_eq!(cfg.model, "adhoc-model");
        // Persona from the profile survives the override.
        assert_eq!(cfg.agent_prompt.as_deref(), Some("Review only."));
        assert!(cfg.read_only);
    }
```

- [ ] **Step 2: Run, verify failure.**

Run: `cargo test -p hrdr-agent apply_task_overrides --locked` Expected: FAIL
(both tests) — `apply_task_overrides` does not exist.

- [ ] **Step 3: Add `apply_task_overrides`.**

Insert directly **below** `repoint_to_provider`:

```rust
/// Apply the `task` tool's ad-hoc `provider`/`model` arguments on top of an
/// already-resolved config (post agent-profile). With `provider`: auth-gate
/// the target (fail fast before spawning) and repoint. Without: `model`
/// overrides on the current provider — today's behaviour.
fn apply_task_overrides(
    cfg: &mut AgentConfig,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    if let Some(pname) = provider {
        // Resolve + gate against the PARENT's key/base_url, before repoint.
        let p = cfg.resolve_provider(pname).ok_or_else(|| {
            anyhow::anyhow!(
                "task: unknown provider '{pname}' (built-ins: {}, or define [providers.{pname}])",
                BUILTIN_PROVIDERS.join(", ")
            )
        })?;
        if provider_auth_state(pname, &p, cfg.api_key.as_deref(), Some(cfg.base_url.as_str()))
            == ProviderAuthState::Missing
        {
            let env = p.key_env.as_deref().unwrap_or("HRDR_API_KEY");
            bail!("task: provider '{pname}' is not configured — set ${env}, or run /login");
        }
        if model.is_none() && p.model.is_none() {
            bail!("task: provider '{pname}' requires an explicit model (it has no default)");
        }
        repoint_to_provider(cfg, pname, model)?;
    } else if let Some(m) = model {
        cfg.model = m.to_string();
    }
    Ok(())
}
```

- [ ] **Step 4: Run the new tests, verify pass.**

Run:
`cargo test -p hrdr-agent apply_task_overrides_provider_repoints_and_gates --locked`
Expected: PASS.

- [ ] **Step 5: Add the `provider` property to the schema and reword `model`.**

In `SubagentTool::parameters` (`:561`), locate the `model` property and replace
its description, and add a `provider` property. The `model` block currently
reads `"Optional model override (on the selected provider). …"`; change to:

```rust
            "model": {
                "type": "string",
                "description": "Optional model override. Defaults to the profile's / configured subagent model, else the main model."
            },
            "provider": {
                "type": "string",
                "description": "Optional provider for the sub-agent: a built-in (zen, openai, openrouter, claude, local, chatgpt) or a [providers.*] from config. Must be configured and authenticated. Omit to keep the current provider; when set, pass `model` too (unless the provider has a default)."
            },
```

- [ ] **Step 6: Wire `apply_task_overrides` into `execute`.**

In `SubagentTool::execute`, replace the model-override block (`:640-646`):

```rust
        if let Some(m) = args
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|m| !m.trim().is_empty())
        {
            cfg.model = m.trim().to_string();
        }
```

with:

```rust
        let provider_arg = args
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let model_arg = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        apply_task_overrides(&mut cfg, provider_arg, model_arg)?;
```

(`cfg.cwd = ctx.cwd.clone();` above it and the `cfg.model == "default"` bail
below it are unchanged.)

- [ ] **Step 7: Run the full gate.**

Run:

```bash
cargo test -p hrdr-agent --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt
```

Expected: all pass. In particular
`task_tool_present_only_when_subagents_enabled` and other `SubagentTool` tests
still pass (schema addition is additive; `provider` omitted preserves
behaviour).

- [ ] **Step 8: Commit.**

```bash
git add crates/hrdr-agent/src/lib.rs
git commit -m "feat(agent): let task delegate to any configured provider ad-hoc"
```

---

### Task 3: Docs, changelog, and live end-to-end verification

**Files:**

- Modify: `README.md` (delegation section), `CHANGELOG.md` (`[Unreleased]`).

- [ ] **Step 1: README — document the `provider` arg.**

Find the `task` / sub-agent delegation section in `README.md` (search for
`subagent` / `task` / `[[subagent]]`). Add a short paragraph after the existing
delegation description:

```markdown
The `task` tool also accepts an optional `provider` argument to delegate to a
different, already-configured provider without defining a `[[subagent]]` profile
— e.g. a ChatGPT-hosted main agent delegating to
`provider = "openrouter", model = "deepseek/deepseek-chat"`. The target provider
must be configured and authenticated (a built-in with its key/OAuth set, or a
`[providers.*]` entry); an unconfigured provider is rejected before the
sub-agent starts.
```

- [ ] **Step 2: CHANGELOG — add entries under `[Unreleased]`.**

Under `## [Unreleased]`, add to the appropriate sections (create them if
absent):

```markdown
### Added

- `task` tool: an optional `provider` argument for ad-hoc cross-provider
  delegation — delegate to any configured + authenticated provider/model at
  delegation time, not only via a predefined `[[subagent]]` profile. The target
  provider is auth-gated before the sub-agent spawns.

### Fixed

- Sub-agents repointed to another provider now carry that provider's identity
  (`config.provider`), so their derived provider kind matches their endpoint
  instead of inheriting the parent's.
```

- [ ] **Step 3: Format the markdown.**

Run: `prettier --write README.md CHANGELOG.md`

- [ ] **Step 4: Commit docs.**

```bash
git add README.md CHANGELOG.md
git commit -m "docs(agent): document ad-hoc cross-provider delegation"
```

- [ ] **Step 5: Live end-to-end verification (drive the real binary).**

Use the `verify` skill. Concretely:

```bash
cargo build -p hrdr
# find a valid openrouter slug (models CLI fixed in #7):
./target/debug/hrdr --provider openrouter models | head
```

Then, with the ChatGPT provider configured as the main agent (the current
`~/.config/hrdr/config.toml`), run a headless delegation that forces a
cross-provider sub and reports the sub's resolved model. The `task` tool
defaults to `background: true` (detached; result delivered after the turn may
end), so the prompt must force `background: false` to block until the sub
finishes — otherwise the one-shot headless run can end before the sub replies:

```bash
./target/debug/hrdr run "Use the task tool with background: false (block until it finishes) to delegate to provider 'openrouter', model '<valid-slug>', prompt: 'Reply with only your active model id.' Then report exactly what the sub returned."
```

Expected: the sub runs on openrouter (its reply is the openrouter model id, not
a gpt-5.* id), proving the request hit openrouter with the openrouter key.
Capture the output in the task notes. If the run ends without the sub's reply,
confirm `background: false` was honored (isolated-worktree subs are always
blocking; a plain sub needs the explicit flag).

- [ ] **Step 6: Negative check — unconfigured provider fails fast.**

```bash
./target/debug/hrdr run "Use the task tool with background: false to delegate to provider 'zen', model 'x', prompt: 'hi'. Report any error verbatim."
```

Expected (assuming no `OPENCODE_API_KEY`/saved zen login): the delegation is
rejected with `provider 'zen' is not configured …` **before** a sub-agent
starts. If zen happens to be configured on this machine, substitute any built-in
whose key is unset.

---

## Self-Review

**1. Spec coverage:**

- Interface (`provider` arg) → Task 2 Steps 5-6. ✓
- `provider` omitted = byte-identical → Task 2 test (e)/(f) + unchanged model
  block semantics. ✓
- Repoint helper, persona-preserving → Task 1 (helper) + verified by the
  precedence test below. ✓
- Provider-identity fix → Task 1 Steps 1-4. ✓
- Auth gate, ad-hoc only → Task 2 Step 3 (`apply_task_overrides`); profiles
  untouched (Task 1 helper has no gate). ✓
- Precedence (ad-hoc override on a resolved profile wins on endpoint/model while
  persona survives) → Task 2 test
  `apply_task_overrides_wins_over_profile_but_keeps_persona`. ✓
- Ordering (gate/resolve on parent values before repoint) → helper resolves key
  before mutation; gate runs before `repoint_to_provider`. ✓
- No-default-model error → Task 2 test (c) + impl. ✓
- Model applies once → `apply_task_overrides` is the single application site;
  old unconditional block removed. ✓
- Docs/CHANGELOG/reword → Task 3. ✓
- Live e2e → Task 3 Steps 5-6. ✓

**2. Placeholder scan:** `<valid-slug>` in Task 3 is a runtime value discovered
by the preceding `models` command, not a plan placeholder. No TBD/TODO/"add
error handling". ✓

**3. Type consistency:**
`repoint_to_provider(&mut AgentConfig, &str, Option<&str>) -> Result<()>` and
`apply_task_overrides(&mut AgentConfig, Option<&str>, Option<&str>) -> Result<()>`
used identically in call sites and tests. `ProviderAuthState::Missing`,
`ProviderConfig` fields (8: base_url, key_env, api_key, model, remote,
context_window, headers, api_version) match the struct at `lib.rs:1382`. ✓
