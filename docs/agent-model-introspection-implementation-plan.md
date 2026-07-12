# Agent Model Introspection Implementation Plan

**Spec:** `docs/agent-model-introspection-spec.md`

**Goal:** Add read-only `model_info` introspection and make unpinned delegation
resolve against the main agent's live provider/model state while preserving
explicit override precedence.

**Architecture:** Keep `Agent`/`Client` authoritative. Publish one typed shared
projection after runtime mutations: a public credential-free view for
`model_info`, and a private endpoint view for `SubagentTool`. Reuse existing
model catalogs through a provenance-aware discovery adapter. Consolidate
provider switching into one `Agent` operation so endpoint state and projection
update together.

**Tech stack:** Rust 2024, Tokio, serde/serde_json, existing `hrdr-agent`,
`hrdr-llm`, and `hrdr-app` abstractions.

## Constraints

- No pricing or cost-based model ranking.
- No system-prompt injection of runtime model data.
- No generic live `/models` probing.
- Never serialize API keys, OAuth tokens, account IDs, base URLs, or configured
  headers into `model_info` output.
- Preserve task concurrency, background delivery, cost aggregation, and profile
  restrictions.
- Keep `task.model` free-form; do not build a static model enum.
- Filter synthetic model ID `"default"` from agent-visible model rows.
- Run `cargo fmt`, `cargo clippy`, and `cargo test` after Rust changes.

---

## Task 1: Add Live Runtime and Delegation Projection

**Files:**

- Modify: `crates/hrdr-llm/src/client.rs`
- Modify: `crates/hrdr-agent/src/lib.rs`
- Test: `crates/hrdr-llm/src/client.rs`
- Test: `crates/hrdr-agent/src/lib.rs`

**Interfaces:**

```rust
// hrdr-llm
pub fn effort(&self) -> Option<&str>;

// hrdr-agent, names may vary but responsibilities may not
#[derive(Clone)]
struct PublicModelRuntime {
    provider: Option<String>,
    model: String,
    effort: Option<String>,
}

#[derive(Clone)]
struct DelegationEndpoint {
    provider: Option<String>,
    model: String,
    effort: Option<String>,
    base_url: String,
    api_key: Option<String>,
    api_version: Option<String>,
    configured_headers: Vec<(String, String)>,
    provider_kind: ResolvedProviderKind,
}

struct DelegationRuntime {
    public: PublicModelRuntime,
    endpoint: DelegationEndpoint,
    explicit_subagent_model: Option<String>,
}

#[derive(Clone)]
struct DiscoveryProvider {
    name: String,
    catalog_key: String,
    configured_model: Option<String>,
}

struct ModelDiscoveryContext {
    // Immutable sanitized provider rows computed while AgentConfig is available.
    providers: Vec<DiscoveryProvider>,
    // No ProviderConfig, key, token, header, endpoint, or credential path is kept.
}
```

- [ ] Add `Client::effort()` returning selected/raw effort without changing
      normalization or wire behavior.
- [ ] Define private runtime projection types in `hrdr-agent`; do not derive
      `Serialize` for the private endpoint type.
- [ ] Capture explicit global `subagent_model` separately from the live model so
      both `SubagentTool` and `model_info` resolve the same default.
- [ ] Define a credential-free immutable discovery context before `AgentConfig`
      fields are moved. Compute sanitized provider name/catalog-key/configured-
      model rows from existing provider eligibility while config and credential
      resolvers are available; do not clone `ProviderConfig`, resolved keys,
      headers, endpoints, or credential paths into this context.
- [ ] Build the sanitized provider rows once with the same eligibility rules as
      `/model`. Include every configured custom provider and eligible built-in;
      provider switching only selects among that configured set, so discovery
      context remains immutable and needs no mutation lock.
- [ ] Store runtime projection behind short synchronous shared locking. Pass it
      and immutable discovery context to `ModelInfoTool`; pass only runtime
      projection to `SubagentTool`. Clone snapshots and release locks before any
      `.await`, network I/O, or delegated agent run.
- [ ] Initialize projection from fully resolved `AgentConfig` before moving
      config fields into `Client`/`Agent`.
- [ ] Preserve configured headers and trusted `provider_kind` only. Never copy
      effective OAuth-injected headers or ephemeral access/account data from
      `Client`. Keep `provider_kind` private: delegation needs it to reconstruct
      trust identity and available-mode discovery needs it to gate OAuth access.
      Exclude context window from projection.
- [ ] Add one internal publication helper that replaces the complete projection.
- [ ] Update `Agent::set_model` and `Agent::set_provider` to invalidate
      `cost_rates` when their cache key changes, then publish. Update
      `Agent::set_effort` to publish. Discovery context remains immutable; live
      current model comes from runtime projection, not by rewriting configured
      catalog rows.
- [ ] Keep provider-switch publication for Task 2; do not make individual
      endpoint setters publish partial state.

**Tests:**

- [ ] Client effort getter returns `None`, selected known effort, display-only
      effort, and cleared state.
- [ ] Projection initializes with provider/model/effort, explicit sub-agent pin,
      and endpoint configuration.
- [ ] Discovery context retains custom provider metadata needed for choices but
      no resolved API key/OAuth token values.
- [ ] Public response serialization/snapshot tests prove private endpoint fields
      are absent; rely on type separation and code review rather than an
      infeasible runtime test that a Rust type has "no serialization path."
- [ ] Model and effort setters refresh public and private views consistently.
- [ ] ChatGPT initialization stores configured headers only and no OAuth
      token/account header.

---

## Task 2: Make Provider Switching Atomic at Agent API Boundary

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs`
- Modify: `crates/hrdr-app/src/commands/model.rs`
- Test: `crates/hrdr-agent/src/lib.rs`
- Test: `crates/hrdr-app/src/commands/model.rs` or existing command test host

**Interface:**

```rust
pub struct ProviderSwitch {
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_version: Option<String>,
    pub configured_headers: Vec<(String, String)>,
    pub provider: Option<String>,
    pub kind: ResolvedProviderKind,
    pub model: Option<String>,
}

impl Agent {
    pub fn apply_provider_switch(&mut self, switch: ProviderSwitch);
}
```

- [ ] Add one `Agent` method that updates endpoint, key, API version, configured
      headers, provider identity/name, and optional model.
- [ ] Publish projection once after every field is applied.
- [ ] Preserve cache-mode recalculation currently performed by `set_endpoint`.
- [ ] Preserve trusted-provider identity and configured-header handling
      currently performed by `set_provider_identity`.
- [ ] Change `apply_provider` and `apply_choice` to build `ProviderSwitch` and
      call the combined method under their existing `agent.lock().await` guard.
- [ ] Keep context-window probing and UI posting outside projection publication
      and behaviorally unchanged.
- [ ] Retain existing public setters only where other callers require them;
      document that provider transitions use the combined method.

**Tests:**

- [ ] Combined switch updates client endpoint/model and public provider/model.
- [ ] Combined switch publishes private endpoint and public runtime as one
      complete tuple.
- [ ] `apply_provider` preserves provider-default-model behavior when a default
      exists.
- [ ] `apply_choice` uses exact selected model.
- [ ] Same-provider `switch_model` updates model without changing endpoint
      identity.
- [ ] Cost-rate memo invalidates on model/provider switch; first later usage
      resolves the new `(provider, model)` card.
- [ ] Context-window probe/post behavior remains unchanged.

---

## Task 3: Implement Sub-agent Live Inheritance and Precedence

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs`
- Test: `crates/hrdr-agent/src/lib.rs`

- [ ] Pass shared delegation projection and an explicit-global-`subagent_model`
      marker into `SubagentTool`.
- [ ] At execution, start from immutable sub-agent base policy, overlay the
      current main endpoint/model/effort projection, then apply selected
      profile, then apply task model override. This ordering lets persona-only
      profiles inherit live effort while preserving an explicit profile effort.
- [ ] Extract endpoint/model precedence into a pure resolver returning either a
      fully resolved `AgentConfig` or typed configuration error. Test this
      resolver directly; keep foreground/background execution tests focused on
      snapshot timing and integration.
- [ ] Resolve endpoint first:
  1. profile provider endpoint when profile names a provider;
  2. otherwise current main endpoint projection.
- [ ] Resolve model in exact precedence:
  1. non-empty trimmed `task.model`;
  2. explicit profile model;
  3. selected profile provider default;
  4. explicit global `subagent_model`, only when no profile provider changes
     endpoint;
  5. current main model.
- [ ] For persona/tool/effort-only profiles, preserve inherited endpoint and
      model precedence.
- [ ] For provider profiles lacking profile/provider model, allow explicit
      `task.model`; otherwise return clear configuration error. Never cross
      provider boundary with global `subagent_model` or main model.
- [ ] Reject resolved literal `"default"` before constructing/running sub-agent.
- [ ] Clone resolved config before spawning foreground/background work so
      running tasks remain unaffected by later switches.
- [ ] Update `task.model` schema description with inheritance semantics and
      direction to call `model_info` for unfamiliar IDs.

**Tests:**

- [ ] No overrides uses current main endpoint/model.
- [ ] Main model switch after `Agent::new` affects later task.
- [ ] Main provider switch after `Agent::new` affects later task endpoint and
      model.
- [ ] Explicit global `subagent_model` pins model while inheriting current main
      endpoint.
- [ ] Task override wins on inherited endpoint.
- [ ] Profile model wins over global sub-agent model.
- [ ] Profile provider default wins on profile endpoint.
- [ ] Profile provider + explicit profile model uses profile model.
- [ ] Provider profile without defaults accepts explicit task model.
- [ ] Same profile without any model returns clear configuration error.
- [ ] Persona-only profile inherits global sub-agent model or current main
      model.
- [ ] Empty/whitespace task model behaves as absent.
- [ ] Explicit or resolved `"default"` fails before provider request.
- [ ] Background task captures model/endpoint at launch.

---

## Task 4: Add Provenance-aware Model Discovery for Introspection

**Files:**

- Modify: `crates/hrdr-agent/src/models.rs`
- Modify: `crates/hrdr-agent/src/chatgpt_models.rs`
- Modify: `crates/hrdr-agent/src/lib.rs`
- Test: `crates/hrdr-agent/src/models.rs`
- Test: `crates/hrdr-agent/src/chatgpt_models.rs`

**Interfaces:**

```rust
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelSource {
    AccountCatalog,
    ModelsDev,
    Configured,
}

#[derive(Clone, Serialize)]
struct AvailableModel {
    provider: String,
    model: String,
    label: String,
    source: ModelSource,
}
```

- [ ] Add a discovery adapter that accepts the immutable discovery context plus
      live provider name, reuses provider eligibility and catalog lookup, and
      retains row provenance.
- [ ] Keep existing `/model` ordering and behavior unchanged; avoid forcing UI
      `ModelChoice` consumers to understand introspection warnings unless
      sharing provenance is clean.
- [ ] Build base rows from sanitized configured providers and cached models.dev
      data. Independently ensure the live `(provider, model)` from runtime
      projection appears when provider is named: merge it as `configured` only
      when absent from stronger sources and model is not `"default"`. For an
      unnamed raw `--base-url`, current state remains visible but no
      `available_models` row is invented because the row schema requires a
      provider name. This covers named custom endpoints and post-start model
      switches without mutating discovery context.
- [ ] Label fallback configured-model rows as `configured`; omit provider rows
      with no real configured model.
- [ ] Filter every model ID exactly equal to `"default"`.
- [ ] For the active trusted ChatGPT provider only, obtain `OAuthAccess` through
      `coordinated_oauth_access(provider_kind, base_url)` and immediately pass
      it to `chatgpt_model_catalog`; never store it in discovery/runtime state.
      Do not fetch an account catalog merely because ChatGPT appears among
      inactive configured providers. Convert refresh/catalog failure into
      structured warnings while preserving non-ChatGPT rows.
- [ ] Merge account catalog rows and preserve `CatalogSource` warning/fallback
      information.
- [ ] Deduplicate `(provider, model)` using source priority
      `account_catalog > models_dev > configured`.
- [ ] Preserve best available label and optional context metadata internally,
      even though initial tool JSON need only expose
      provider/model/label/source.
- [ ] Define stable warning codes and assert them in tests:
      `no_default_subagent_model`, `models_truncated`,
      `models_dev_cache_unavailable`, `chatgpt_catalog_stale`, and
      `chatgpt_catalog_fallback`. Use `chatgpt_catalog_fallback` for auth,
      refresh, malformed, or unavailable cases that return the built-in row;
      preserve the existing sanitized catalog warning as message instead of
      inventing unverifiable subcategories. Warning messages remain
      human-readable; callers key on codes.
- [ ] Do not call generic `Client::list_models()`.

**Tests:**

- [ ] models.dev row receives `models_dev` source.
- [ ] configured fallback receives `configured` source.
- [ ] model-less provider and synthetic `"default"` are omitted.
- [ ] account catalog replaces lower-priority duplicate.
- [ ] Other providers survive ChatGPT merge.
- [ ] Fresh/stale/built-in ChatGPT result maps to rows plus appropriate
      warnings.
- [ ] Empty/missing models.dev cache returns configured rows and warning without
      claiming network failure.
- [ ] Ordering is deterministic after deduplication and truncation.
- [ ] Active named model switched after startup appears as configured fallback
      when no stronger catalog row exists; unnamed raw endpoint adds no row.

---

## Task 5: Implement and Register `model_info`

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs`
- Test: `crates/hrdr-agent/src/lib.rs`

**Schema:**

```json
{
  "type": "object",
  "properties": {
    "mode": {
      "type": "string",
      "enum": ["current", "available"],
      "default": "current"
    }
  },
  "additionalProperties": false
}
```

- [ ] Implement `ModelInfoTool` in `hrdr-agent` as `read_only() == true`.
- [ ] Register `model_info` before tool scoping; both initial prompt
      construction and later prompt rebuilds (`clear`/`set_cwd`) use the final
      retained registry. Main unrestricted agents keep it. Restricted agents
      keep it only when their existing read-only or write-extension scope
      includes read-only tools; an explicit `allowed_tools` list must name it.
- [ ] After all registrations and `retain_only` scoping finish, compute
      `delegation_enabled` from the final registry (for example,
      `tools.defs().iter().any(|d| d.function.name == "task")`) and store that
      value in public runtime/tool state. Do not equate it with
      `config.subagents`: an explicit allowlist or read-only scope can remove
      `task` after registration. Retained `model_info` on a sub-agent therefore
      reports false.
- [ ] `current` reads only public runtime projection and performs no
      catalog/network work.
- [ ] Compute `effective_effort` with existing `hrdr_llm::normalize_effort`.
- [ ] Compute `default_subagent_model` from the projection's explicit global
      sub-agent pin or live model; return null for disabled delegation or
      resolved `"default"`.
- [ ] `available` calls Task 4 discovery and appends rows/warnings.
- [ ] Always return `warnings` as an array of `{code, message}`.
- [ ] Bound available rows by serializing candidate responses against
      `ctx.max_output`, reserving room for a complete `models_truncated`
      warning. Drop whole tail rows until final JSON fits; avoid a hard-coded
      row count because model/label lengths vary.
- [ ] Use `serde_json::to_string` for compact valid JSON. If mandatory current
      fields plus warnings alone exceed `ctx.max_output`, return them intact
      rather than byte-truncating invalid JSON; row bounding handles the only
      unbounded field.
- [ ] Reject unknown modes with supported values in error text.

**Tests:**

- [ ] Tool schema and default mode are exact.
- [ ] `model_info` registration occurs before scoping but final system
      prompt/tool definitions contain it only when retained.
- [ ] Final registry removes `task` under explicit/read-only scoping and
      retained `model_info` reports `delegation_enabled = false`.
- [ ] Current response includes provider/model/raw effort/effective
      effort/delegation state/default sub-agent model.
- [ ] Display-only/invalid effort has null effective effort.
- [ ] Disabled delegation has null default without warning.
- [ ] Unconfigured `"default"` has null default and `no_default_subagent_model`
      warning.
- [ ] Available mode includes discovered rows and warnings.
- [ ] `current` mode does not invoke OAuth refresh/account catalog or read the
      models.dev cache.
- [ ] Truncation drops whole rows and emits warning.
- [ ] JSON contains no endpoint, key, header, token, account ID, or credential
      path fields.

---

## Task 6: Documentation and Regression Verification

**Files:**

- Modify: `README.md` if user-facing tool documentation exists there
- Modify: `CHANGELOG.md` under `Unreleased` if present
- Modify: generated/tool documentation only if repository workflow requires it

- [ ] Document `model_info` modes, availability caveat, and read-only behavior.
- [ ] Document sub-agent precedence and live main-provider/model inheritance.
- [ ] State that availability does not guarantee account authorization and no
      cost ranking occurs.
- [ ] Run Prettier on changed Markdown.
- [ ] Verify no unrelated untracked `docs/` files are staged for PR unless
      explicitly requested.

**Verification:**

```bash
npx prettier --write <changed-markdown-files>
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

- [ ] Inspect `git diff --check`.
- [ ] Inspect final diff for credential serialization and accidental endpoint
      disclosure.
- [ ] Confirm existing `/model`, `/effort`, provider login/switching, foreground
      task, and background task tests remain green.
- [ ] Confirm no pricing, generic `/models` probing, system-prompt metadata
      injection, or nested delegation entered scope.

## Completion Criteria

- `model_info current` accurately reflects live provider/model/effort after
  switches.
- `model_info available` returns bounded, provenance-labeled discoverable models
  without synthetic IDs.
- Unpinned tasks inherit current main endpoint/model at execution time.
- Explicit task/profile/global overrides follow documented precedence.
- Provider profiles never inherit incompatible cross-provider model IDs.
- Public output cannot serialize private delegation credentials.
- All formatting, Clippy, and workspace tests pass.
