# Agent Model Introspection and Delegation Inheritance Specification

## Summary

Add agent-facing runtime model introspection and make default sub-agent
delegation inherit the main agent's current model instead of retaining a
startup-time provider fallback.

The change lets the agent answer three questions accurately during a live
session:

1. Which provider, model, and reasoning effort are active now?
2. Which models are available for selection or delegation?
3. Which model will an unpinned sub-agent use?

## Problem

hrdr currently knows the active model and effort internally, and the TUI can
build a model selector, but this information is not exposed to the model through
messages or callable tools.

The `task` tool exposes `model` as a free-form override. It exposes named agent
profiles through an enum, but does not expose valid model IDs or the resolved
default. Consequently, the agent must guess model names and cannot verify its
current model or effort.

Sub-agent defaults are also captured when `Agent::new` constructs
`SubagentTool`. `subagent_base_config` selects `subagent_model` or the main
startup model at that point. Later `/model` changes update `Agent.client.model`
but not the captured sub-agent base config. On the built-in ChatGPT provider,
the startup provider default is `gpt-5.5`, so unpinned delegation can use that
stale fallback after the main agent has switched models.

## Goals

- Let the main agent retrieve its current provider, model, and reasoning effort.
- Let the main agent retrieve model choices available through hrdr's existing
  model-discovery sources.
- Ensure returned runtime values reflect `/model`, provider, and `/effort`
  changes made during the current session.
- Make unpinned sub-agents inherit the main agent's current model at task
  execution time.
- Preserve explicit model-selection precedence for sub-agents.
- Make model-discovery failures explicit without making introspection itself
  fail unnecessarily.
- Keep runtime metadata free of API keys, OAuth tokens, and other credentials.

## Non-goals

- Allow the agent to change the active model or effort through the new
  interface.
- Automatically choose the cheapest model based on pricing.
- Infer that a model is cheaper, faster, or more capable without catalog data
  supporting that claim.
- Replace the user-facing `/model` or `/effort` selectors.
- Guarantee every catalog model is authorized for the current account.
- Change ChatGPT's built-in startup default for the main agent.
- Permit nested sub-agent delegation.

## Proposed Interface

Add a read-only agent tool named `model_info`.

### Input

Use an optional mode to avoid returning a large catalog when only current state
is needed:

```json
{
  "mode": "current"
}
```

Supported modes:

- `current` — current runtime state and resolved default sub-agent model.
- `available` — current state plus available model choices.

Default: `current`.

### Output

Return structured JSON encoded as the tool result. Example:

```json
{
  "provider": "chatgpt",
  "model": "gpt-5.6-codex",
  "effort": "high",
  "effective_effort": "high",
  "delegation_enabled": true,
  "default_subagent_model": "gpt-5.6-codex",
  "available_models": [
    {
      "provider": "chatgpt",
      "model": "gpt-5.6-codex",
      "label": "GPT-5.6 Codex",
      "source": "account_catalog"
    }
  ],
  "warnings": []
}
```

Rules:

- `provider` is nullable when hrdr uses a raw `--base-url` without a named
  provider.
- `effort` is the selected display/configuration label and is nullable when
  default effort is selected. Add `effective_effort`, also nullable, for the
  recognized/normalized effort level — `crate::normalize_effort` applied to the
  label — which is null when the label is unrecognized or display-only (e.g.
  `off`). This is computed identically for every backend and is independent of
  how each backend transmits it: OpenAI chat and Codex/Responses send it as a
  `reasoning_effort` / `reasoning.effort` field, while the native Anthropic
  backend converts the same level into an extended-thinking token budget. It
  distinguishes an actively-recognized effort from a display-only or
  unrecognized label.
- `delegation_enabled` reports whether the `task` tool is registered for this
  agent. When false, `default_subagent_model` is null because no default task
  can be launched; this is not a configuration warning.
- `default_subagent_model` reports the model an unprofiled, unoverridden `task`
  call would use now, resolved exactly as `subagent_base_config` does per the
  §Sub-agent Model Resolution precedence (explicit `subagent_model` if set,
  otherwise the current live main model). It is nullable: when that resolution
  yields the sentinel literal `"default"` (e.g. a keyless `local` endpoint whose
  main model was never pinned and no explicit `subagent_model`), set
  `default_subagent_model` to `null` and populate a non-empty `warnings` array
  stating that no sub-agent model is configured. Never emit the bare
  `"default"`: the task path rejects `task.model == "default"` as unconfigured
  (`crates/hrdr-agent/src/lib.rs:647-652`), so an unprofiled, unoverridden task
  would fail rather than run, and emitting `"default"` here would hand the agent
  the same non-id its own copy-into-`task.model` path refuses — the identical
  contradiction §Model Availability Semantics already forbids for
  `available_models`. Do not substitute a fabricated replacement id. Keys
  strictly on the RESOLVED default, so an explicit real `subagent_model` is
  unaffected.
- `available_models` appears only in `available` mode.
- `warnings` is always an array of `{ "code": string, "message": string }`
  objects. It is empty on a clean result and may contain multiple independent
  discovery, configuration, or truncation warnings.
- Results must not include endpoint credentials or configured headers.
- Lists must be bounded by normal tool-output limits. Preserve complete model
  IDs; if truncation is required, truncate rows rather than individual IDs and
  report truncation.

## Model Availability Semantics

"Available" means discoverable by hrdr, not guaranteed authorized for every
request.

Reuse existing discovery behavior rather than create a third independent
catalog:

1. For authenticated ChatGPT OAuth, use the account-specific Codex model catalog
   and its existing cache/fallback behavior.
2. For configured providers represented in the models.dev cache, use the same
   choices built for `/model`.
3. For custom or local providers without catalog rows, include their configured
   model **only when one is configured**. A provider with no catalog rows and no
   configured model (e.g. the keyless `local` preset, or a model-less custom
   `[providers.*]`) has no known model id to advertise; do not emit a row for it
   in `available_models`.
4. Do not add generic live `/models` probing in this PR. Existing models.dev,
   configured-provider, and authenticated ChatGPT catalog sources define the
   returned set. This keeps `available` deterministic and avoids
   provider-specific auth/shape behavior.
5. In particular, never use generic `/models` probing for trusted ChatGPT OAuth;
   that backend rejects the generic probe.

- The `/model` choice assembly (`choices_from` / `model_choices`) synthesizes a
  placeholder `model` of the literal string `"default"` for any provider it
  reaches with no catalog rows and no configured model. When reusing those
  choices to build `available_models`, an implementer MUST filter out every row
  whose `model == "default"` before returning them. This sentinel is not a real
  model id: the task path rejects `task.model == "default"` as unconfigured
  (`crates/hrdr-agent/src/lib.rs:647-652`), so advertising it would offer the
  agent an id that its own copy-into-`task.model` path immediately refuses —
  contradicting §Error Handling ("do not invent model IDs") and §Sub-agent Model
  Resolution ("do not use the sentinel `\"default\"`"). Never emit `"default"`
  as a selectable model id, and do not substitute a fabricated replacement id;
  simply omit the row. The `available_models[].model` field therefore remains a
  real, non-null model id in every returned row.

Each row identifies its source with this stable enum: `account_catalog`,
`models_dev`, or `configured`. Existing `ModelChoice` does not carry provenance,
so the reusable discovery layer must add it or return a separate typed
introspection row; do not infer provenance from display labels. Deduplicate by
`(provider, model)` with source priority `account_catalog` > `models_dev` >
`configured`, preserving the highest-confidence row and any known label/context
metadata.

Model discovery also needs an immutable discovery context containing provider
definitions and authentication-state access plus the live provider name from the
public runtime view. This context is separate from the private delegation view;
it contains no token values. Reuse the existing credential-aware provider
eligibility and ChatGPT catalog adapter rather than storing credentials in model
rows or tool output.

Current catalog APIs intentionally collapse missing cache and fetch failures.
This PR does not need to redesign global catalog error provenance. Return
`warnings` for failures or fallback states that existing sources can identify,
especially the ChatGPT account catalog. For models.dev cache absence, return
configured fallback rows and note that only configured models are known; do not
claim a network failure. Discovery failure must not hide current model and
effort.

## Live Runtime State

The source of truth must be live `Agent`/`Client` state, not the original
`AgentConfig`, because model and effort can change after startup.

Maintain one shared delegation projection because `SubagentTool` cannot borrow
its owning `Agent` during tool execution. `Agent`/`Client` remain authoritative;
the projection is a deliberately duplicated, sanitized execution snapshot
published through one method after relevant mutations.

Split the projection into two typed views:

- Public runtime view, available to `model_info`: provider name, model, and
  effort.
- Private delegation view, available only to `SubagentTool`: base URL, resolved
  non-OAuth API key, API version, provider-configured headers, provider name,
  model, and effort.

Never include ephemeral OAuth access tokens or live effective authorization
headers. `Agent::new` re-derives trusted provider identity from provider name
and injects ChatGPT OAuth per request. Context window is excluded from this
shared projection: it is UI/compaction and catalog metadata, not required to
route a sub-agent request.

Publish the complete projection from a single helper after initialization and
after relevant mutations:

- Model-only and effort-only changes publish after `Agent::set_model` and
  `Agent::set_effort`.
- Replace the frontend's multi-setter provider sequence with one `Agent` method
  that applies endpoint, key, API version, configured headers, provider
  identity/name, and optional/explicit model, then publishes once. Both
  `apply_provider` and `apply_choice` call it while holding the existing agent
  lock.
- Same-provider model changes leave endpoint identity unchanged and publish the
  new model.

The existing outer `Arc<Mutex<Agent>>` serializes UI mutations against turns and
tool execution. The combined provider-switch method prevents intermediate
setters from publishing partial projections and gives future call sites one safe
API. Tests must enforce that every public mutation path publishes. Do not add
context-window probe completion as a publication dependency.

The `model_info` tool and `SubagentTool` read this same projection.
Introspection uses only its public view; delegation may use its private
execution fields. This keeps their provider/model answers aligned while making
accidental credential serialization structurally difficult.

## Sub-agent Model Resolution

Resolve a task's model at execution time with this precedence, highest first:

1. Non-empty `task.model` override.
2. Selected agent profile's explicit `model`.
3. Selected agent profile's provider default model, when the profile changes
   provider.
4. Explicit global `subagent_model` configuration or `--subagent-model`.
5. Main agent's current live model.

Additional rules:

- Steps 4 and 5 inherit the main agent's current live endpoint identity, not
  only its model string: base URL, resolved non-OAuth API key, API version,
  configured headers, and provider name. Step 4 pins `subagent_model` on that
  endpoint; step 5 also inherits the live model. A selected profile provider
  replaces this endpoint identity. A task model override changes only the model
  on the endpoint selected by the profile, or on the inherited main endpoint
  when no profile provider is selected.
- Invariant: an unpinned, unoverridden `task` launched AFTER a provider switch
  runs on the NEW provider's endpoint with the NEW model. It must never send the
  new model to the old provider's endpoint (new-model-on-old-endpoint), nor fall
  back to the old provider's model. A same-provider `/model` change (endpoint
  unchanged) is the degenerate case of the same rule: only the model field
  differs.
- Empty or whitespace-only `task.model` remains equivalent to no override.
- A profile that only changes persona, tools, effort, or read/write scope
  inherits the appropriate default from steps 4–5.
- A profile that switches provider must not inherit a model ID from the main
  provider unless that profile explicitly selects it. Its provider default
  remains authoritative.
- If a profile switches to a provider with no configured default model and has
  no explicit profile model, a non-empty `task.model` may supply the model and
  still wins by step 1. Otherwise fail task setup with a clear configuration
  error naming the profile/provider and requiring `task.model`, `profile.model`,
  or a provider default. Do not use global `subagent_model` across a profile
  provider boundary, do not inherit the main provider's model cross-provider,
  and do not use the sentinel `"default"`.
- Main-agent `/model` changes affect subsequent unpinned task calls, not tasks
  already running.
- Existing background tasks keep the model resolved when they started.
- Explicit global `subagent_model` remains pinned across main-agent model
  switches.

## Tool Schema Guidance

Keep `task.model` a string override. Do not populate it with a static enum
because:

- available models can change after authentication or account-catalog refresh;
- named profiles may use different providers;
- a JSON Schema enum captured at agent construction would become stale after
  provider changes.

Update the `task.model` description to direct the agent to `model_info` before
selecting an unfamiliar override. The task description should state the resolved
inheritance rule clearly.

## Error Handling

- Unknown `model_info.mode`: return a validation error naming supported modes.
- Current runtime state unavailable: treat as internal error; startup must
  initialize state.
- Model catalog unavailable: return current state and append a structured
  warning, for example `{ "code": "catalog_unavailable", "message": "..." }`.
- Empty discovered list: return an empty list plus a structured warning; do not
  invent model IDs.
- Default sub-agent model resolves to the sentinel `"default"`: return current
  state with `default_subagent_model = null` and a non-empty `warnings` array
  stating an unpinned/unoverridden task would fail with "no model configured"
  until a model is set via config.toml, `$HRDR_MODEL`, `--model`, or
  `--subagent-model`. Do not surface `"default"` as a usable model id.
- Invalid task model override: preserve provider error behavior. Discovery does
  not guarantee acceptance.

## Security and Privacy

- Tool is read-only.
- Never expose API keys, OAuth access/refresh tokens, account IDs, configured
  authorization headers, or raw credential paths.
- Avoid returning full base URLs by default because custom URLs may contain
  sensitive query data or internal hostnames. Provider name is sufficient for
  this PR.
- Existing credential-aware provider filtering remains in force.

## Implementation Boundaries

Expected areas:

- `crates/hrdr-agent/src/lib.rs`
  - shared live model metadata
  - `model_info` tool
  - live sub-agent resolution
  - mutation hooks and tests
- `crates/hrdr-agent/src/models.rs`
  - reusable model-choice assembly/deduplication if current UI APIs are
    insufficient
- `crates/hrdr-agent/src/chatgpt_models.rs`
  - reuse account-specific catalog; avoid duplicate fetch logic
- `crates/hrdr-llm/src/client.rs`
  - add a read-only effort accessor used to build the public runtime view
- `crates/hrdr-tui/src/app/commands.rs` and
  `crates/hrdr-app/src/commands/model.rs`
  - verify existing mutations flow through updated `Agent` methods; avoid
    UI-only state divergence
  - the provider-switch path in `crates/hrdr-app/src/commands/model.rs`
    currently mutates only the live `Agent`; it must also publish the new
    endpoint identity into the shared state / `SubagentTool` base so unpinned
    delegation picks it up

No implementation belongs in the system prompt template. Runtime state would
become stale after model or effort changes and would add permanent token
overhead.

## Testing

### Runtime introspection

- Startup values report configured provider, model, and effort.
- Null effort reports correctly.
- Display-only or invalid effort labels report the selected label while
  `effective_effort` remains null.
- `set_model` changes subsequent `model_info` output.
- A provider/model switch applied under the agent lock is observed by a
  subsequent `model_info` / task read (also under the agent lock) as the
  fully-updated tuple; the agent mutex serializes them so no partial tuple is
  observable.
- `set_provider` and provider choice changes update provider output.
- `set_effort` and effort clearing update output.
- Output contains no key/header/token fields.

### Available models

- Catalog-backed provider returns expected model IDs and labels.
- Custom provider without catalog contributes configured model.
- Duplicate provider/model rows collapse.
- ChatGPT OAuth uses account catalog rather than generic `/models`.
- Catalog failure returns current state with a structured warning.
- `current` mode does not perform model-list network work.

### Delegation inheritance

- No overrides: task uses current main model.
- Main model switched after `Agent::new`: later task uses switched model.
- Explicit `subagent_model`: remains selected after main model switch.
- Explicit `task.model`: wins.
- Explicit profile model: wins over global sub-agent model.
- Profile provider default: wins when profile changes provider.
- Profile changes provider AND sets an explicit `model`: resolves to the
  explicit profile model (precedence step 2), overriding the new provider's
  default model (step 3), on the new provider's endpoint — the guard for the
  assign-provider-default-then-overwrite ordering in `config_for_agent_profile`
  (`lib.rs:428-434`). Use a provider that actually carries a default model (e.g.
  a custom `[providers.*]` with `model` set, or the `chatgpt` builtin whose
  default is `gpt-5.5`), since the existing `openrouter` case has a `None`
  default and cannot exercise the override.
- Profile provider without a default or explicit profile model: an explicit
  non-empty task model succeeds on that provider; otherwise task setup fails
  clearly instead of inheriting `subagent_model` or the main model
  cross-provider.
- Persona-only profile inherits global sub-agent model or current main model.
- Main agent running on the sentinel model `"default"` (keyless `local`,
  unpinned) with no `subagent_model`: `model_info` returns
  `default_subagent_model = null` plus a non-empty `warnings` array; it does NOT
  return the bare `"default"`. (Confirms the field never advertises an id the
  task path at `lib.rs:647-652` rejects.)
- Sentinel `"default"` model — whether resolved from an unconfigured base or
  supplied as an explicit `task.model` override — is rejected with the "no model
  configured" configuration error (`lib.rs:647`), never sent to a provider.
- Empty or whitespace-only `task.model` (e.g. `"   "`) is treated as no override
  and resolves to the inherited default (both hit the `.trim()` filter at
  `lib.rs:643`, not just `is_empty()`).
- Unpinned sub-agent launched after a main-agent PROVIDER switch uses the new
  provider's endpoint identity AND model (the live tuple), not the new model on
  the old provider's endpoint — the regression guard for model-only inheritance.
- Already-running task is unaffected by later model switch.

### Regression

- Existing `/model` selector behavior remains unchanged.
- Existing `/effort` behavior remains unchanged.
- Existing task concurrency, background delivery, cost aggregation, and profile
  tool restrictions remain unchanged.

## Acceptance Criteria

- Agent can accurately report current provider, model, and effort through a
  read-only tool.
- Agent can request a bounded list of discoverable model choices with source
  information.
- Values reflect live model/provider/effort changes without rebuilding the
  agent.
- Unpinned sub-agents launched after a model switch use the main agent's new
  model.
- Explicit `subagent_model`, profile provider/model, and task model overrides
  retain documented precedence.
- Trusted ChatGPT OAuth model listing does not call unsupported generic
  `/models` probing.
- No credentials or sensitive headers appear in tool output.
- Rust formatting, Clippy, unit tests, and workspace tests pass.

## Open Decision

Choose final tool name before implementation:

- `model_info` — explicit and unlikely to conflict.
- `runtime` — extensible but broader than this PR.
- `models` — concise but may imply mutation or collide conceptually with CLI
  commands.

Recommended: `model_info`.
