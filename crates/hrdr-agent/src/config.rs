//! Configuration types and loading for `hrdr-agent`.
//!
//! Defines [`AgentConfig`], [`FileConfig`], and all supporting config types
//! (e.g. [`ProviderConfig`], [`ResolvedProvider`]), plus the full config-loading
//! pipeline: built-in defaults, config-file parsing, environment-variable
//! application, and the validation checks that refuse stale config forms.
//!
//! # Validation policy — error vs. warn
//!
//! Config problems are *accumulated* into a [`ConfigDiagnostics`] and surfaced
//! together, so a user fixes everything in one pass rather than one boot at a
//! time. The loader never panics on bad input and never silently substitutes a
//! default for a value the user wrote — every rejected value produces a
//! diagnostic that names the field, the value, and the accepted range.
//!
//! Whether a bad value is a hard **error** (refuse to start) or a **warning**
//! (report, then fall back to the default) turns on *where the user wrote it*:
//!
//! - **Config-file values are hard errors.** A value in `config.toml` is a
//!   deliberate statement of intent; a nonsensical one (a malformed table, a
//!   zero sub-agent cap, a compaction reserve larger than the context window) is
//!   a mistake worth stopping for — as is a key hrdr does not have, which
//!   [`FileConfig`]'s `deny_unknown_fields` refuses outright. These are collected
//!   by [`FileConfig::validate`] (per-field bounds) and
//!   [`AgentConfig::validate_semantics`] (cross-field checks on the merged
//!   result). `main` prints them and exits non-zero.
//! - **Environment-variable values are warnings.** A `HRDR_*` override is an
//!   ad-hoc tweak, often exported for one run or inherited from a shell; a typo
//!   there should not brick a session. An unparseable or out-of-range env value
//!   is reported and the current value is kept. These are collected by
//!   [`AgentConfig::apply_env`].
//!
//! Two deliberate exceptions to "zero is nonsense": `request_timeout = 0` is the
//! documented sentinel for *disable the timeout* (see
//! [`AgentConfig::request_timeout`]), and `compaction_reserved = 0` /
//! `preserve_recent_tokens = 0` mean *no buffer / no verbatim tail* — all valid
//! choices, so they are accepted. The semantic check only fires when a reserve
//! exceeds the window it is carved out of.
//!
//! (Typed non-zero fields like `NonZeroUsize` were considered, but the caps and
//! limits are plain `usize`/`u32` threaded through many call sites; converting
//! them would churn far past the silent-failure paths this hardening targets, so
//! validation guards the values instead.)

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::auth::auth_token;
use crate::model_ref::{ModelRef, ModelSpec, ProviderName};
use crate::models::model_for_provider;
use crate::oauth::has_oauth_credentials;

use hrdr_llm::CacheMode;
use hrdr_llm::{RetryPolicy, is_anthropic_backend, url_host};
use hrdr_tools::{DEFAULT_MAX_OUTPUT, DEFAULT_MAX_OUTPUT_LINES, SandboxMode};

// ── Constants ───────────────────────────────────────────────────────────────

/// Default cap on concurrently running read-only sub-agents.
///
/// **Two.** They change nothing, so they cannot race each other — but they are
/// not free: every one holds a model context, spends tokens, and lands a report
/// the parent has to read and verify. Five at once was a fan-out the parent
/// could not review carefully, which is the failure that makes a wide read
/// fan-out worse than a narrow one. Raise it deliberately.
pub const DEFAULT_MAX_READONLY_SUBAGENTS: usize = 2;
/// Default cap on concurrently running write-capable sub-agents.
///
/// **One.** Every sub-agent shares the parent's working tree, so two writers are
/// two processes editing the same files with nothing between them — one's
/// formatter run or `git checkout` can undo the other's work mid-flight. The cap
/// is the only mechanism enforcing that; the disjoint-write-set rule in
/// `delegate.md` is a brief-writing convention, and a convention is not a lock.
///
/// Raise it (`max_write_subagents`, `HRDR_MAX_WRITE_SUBAGENTS`, or the CLI flag)
/// when the work genuinely partitions and you want the parallelism — that is the
/// user's call to make, not the model's.
pub const DEFAULT_MAX_WRITE_SUBAGENTS: usize = 1;

/// Turns a completed TODO stays in the agent's list before it ages out.
pub const DEFAULT_TODO_TTL_TURNS: u64 = 5;

/// Session-retention defaults (seconds): compress after a week idle, purge an
/// auto-named session after a month idle (see [`sweep_sessions`](crate::sweep_sessions)).
pub const DEFAULT_SESSION_COMPRESS_AFTER: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_SESSION_PURGE_AFTER: u64 = 30 * 24 * 60 * 60;

/// Auto-compaction on by default. The *trigger point* is set by
/// [`AgentConfig::compaction_reserved`], not by this toggle.
pub const DEFAULT_AUTO_COMPACT: bool = true;

/// Default token buffer reserved below the context window before auto-compaction
/// fires — compaction triggers once usage reaches `context_window − reserved`,
/// leaving room for the next turn's output. Matches pi's `reserveTokens` default.
pub const DEFAULT_COMPACTION_RESERVED: u32 = 16_384;

/// The identity a config with nothing set runs on: an OpenAI-compatible server
/// the user runs themselves (`http://localhost:8080/v1`), serving whatever model
/// it was started with (`default` — let the endpoint pick).
pub const DEFAULT_MODEL_REF: &str = "local://default";

/// The endpoint a config with nothing set talks to — the `local` provider's, since
/// `local` IS "the OpenAI-compatible server I run".
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// The model id meaning "whatever this endpoint serves" — not a model name.
/// A remote provider needs a real id; this sentinel is only ever right for a
/// local server that serves one model.
pub const DEFAULT_MODEL: &str = "default";

/// The canonical Codex OAuth endpoint for built-in ChatGPT subscription login.
/// Single owner of the endpoint literal — the auth-derived `openai` endpoint
/// switch, refresh trust gating, catalog requests, and tests all reference this
/// constant.
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// The default ChatGPT (Codex) model slug — the one the account-catalog fallback
/// offers when the endpoint is unreachable or the account is not entitled.
pub const CHATGPT_DEFAULT_MODEL: &str = "gpt-5.5";

/// The account catalog's context window for the default ChatGPT model
/// ([`CHATGPT_DEFAULT_MODEL`]). A last-resort floor only: per-model windows are
/// resolved live from the account catalog cache
/// (`chatgpt_models::cached_context_window`) — the endpoint 401s on `/v1/models`
/// and models.dev lists the differently-windowed *API* models, so neither can be
/// trusted here.
pub const CHATGPT_DEFAULT_CONTEXT_WINDOW: u32 = 272_000;

/// Canonical built-in provider names, in the order the `/login` wizard offers
/// them. Each resolves through [`builtin_provider`]; `local` needs no API key.
/// `openai` is one provider: an API key talks to `api.openai.com`, an OpenAI
/// OAuth credential talks to the Codex endpoint (see the auth-derived switch in
/// [`resolve::oauth_derived`](crate::oauth_derived)).
pub const BUILTIN_PROVIDERS: &[&str] = &[
    "zen",
    "go",
    "openai",
    "openrouter",
    "claude",
    "deepseek",
    "local",
];

/// The spellings that fold onto the built-in `openai` provider's OAuth/Codex
/// login. Sole owner of the alias set: the `/login` route and the `/model`
/// selector's catalog merge ask [`is_chatgpt_provider_name`] rather than
/// re-encoding the list, so they cannot drift apart. Resolution folds these onto
/// `openai` via [`ProviderName`](crate::ProviderName); this set only names the
/// spellings the OAuth-specific surfaces still recognise.
pub const CHATGPT_PROVIDER_ALIASES: &[&str] = &["chatgpt", "codex", "openai-oauth"];

/// Default recent turns kept verbatim through compaction (`tail_turns`).
/// Matches opencode's `DEFAULT_TAIL_TURNS`.
pub const DEFAULT_TAIL_TURNS: usize = 2;
/// Default token budget for the verbatim tail kept through compaction
/// (`preserve_recent_tokens`). Matches opencode's `MAX_PRESERVE_RECENT_TOKENS`.
pub const DEFAULT_PRESERVE_RECENT_TOKENS: u32 = 8_000;

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// Every problem found while loading config, accumulated so the user can fix
/// them all at once instead of one boot at a time.
///
/// See the [module docs](self) for the error-vs-warning policy: `errors` come
/// from config-file values (refuse to start), `warnings` from `HRDR_*` env
/// overrides (report, then keep the current value).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostics {
    /// Config-file problems. Each is a full line naming the field, the offending
    /// value, and the accepted range. A non-empty list should refuse startup.
    pub errors: Vec<String>,
    /// Environment-variable problems. Each names the var and value; the current
    /// value was kept. Reported but non-fatal.
    pub warnings: Vec<String>,
}

impl ConfigDiagnostics {
    /// No errors and no warnings.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }

    /// The accumulated errors as one multi-line message, or `None` when there
    /// are none. Suitable for a single `bail!`/`eprintln!` that lists everything.
    pub fn error_message(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        Some(format!(
            "hrdr: invalid configuration:\n  {}",
            self.errors.join("\n  ")
        ))
    }

    /// The accumulated warnings as one multi-line message, or `None` when there
    /// are none.
    pub fn warning_message(&self) -> Option<String> {
        if self.warnings.is_empty() {
            return None;
        }
        Some(format!(
            "hrdr: configuration warnings:\n  {}",
            self.warnings.join("\n  ")
        ))
    }
}

// ── Config structs ──────────────────────────────────────────────────────────

/// The model identity — WHICH model at WHICH provider — is the single
/// [`model`](Self::model) field. `base_url` / `api_key` / `api_version` /
/// `headers` are **derived** from it: they are the cached output of
/// [`resolve`] for that identity, not four independently-authoritative settings.
/// Writing one of them by hand does not change which model is in force; changing
/// the identity re-derives all of them together.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// DERIVED from [`model`](Self::model) (see the struct docs): the endpoint
    /// [`resolve`] produced for its provider — a built-in preset, or the
    /// `[providers.<name>]` table that defines the provider. Nothing outside a
    /// provider definition can set it: there is no `--base-url`, no
    /// `$HRDR_BASE_URL`, and no free-floating `base_url` in config.toml. That is
    /// what makes it impossible for a provider's API key to travel to an endpoint
    /// that is not its own.
    pub base_url: String,
    /// DERIVED from [`model`](Self::model): the credential [`resolve_api_key`]
    /// found for its provider (inline → `key_env` → the `/login` store).
    pub api_key: Option<String>,
    /// The model identity: the provider AND the model id, as ONE value. A
    /// mismatched pair (an OpenRouter model id against the Anthropic endpoint) is
    /// not representable. Defaults to `local://default` — "the OpenAI-compatible
    /// server I run, and whatever model it serves".
    pub model: ModelRef,
    pub cwd: PathBuf,
    pub temperature: Option<f32>,
    /// Safety bound on tool-call iterations per user turn.
    pub max_steps: usize,
    /// How hard to retry a failing model call: ten attempts over ~6¼ minutes.
    ///
    /// Only the attempt count is exposed, and only through
    /// `$HRDR_RETRY_ATTEMPTS` — the backoff schedule is not a knob, because a
    /// tunable one is how a fleet of agents turns a provider's bad minute into
    /// a retry storm. It is a field rather than a constant so tests (unit and
    /// process-level alike) can drive the real retry loops without sleeping
    /// through minutes of backoff.
    pub retry: RetryPolicy,
    /// Cost budget in USD: the turn loop stops before the next model call once
    /// the session's estimated spend (incl. sub-agents) reaches it. `None` =
    /// unlimited. Estimates come from the models.dev catalog; a capped run
    /// refuses an unpriced model because its ceiling cannot be enforced.
    pub max_cost: Option<f64>,
    /// Opt-in escape hatch for [`max_cost`](Self::max_cost) on an unpriced model
    /// (a local server the catalog can't price). Default `false` keeps the
    /// fail-closed behavior: a capped run refuses an unpriced model. When
    /// `true`, calls on an unpriced model proceed and are simply *not counted*
    /// toward the cap — priced usage still counts and the cap still enforces on
    /// it. Any run that skipped unpriced calls this way reports its cost total
    /// as a floor ("≥ $X"), never a complete-looking figure.
    pub allow_unpriced: bool,
    /// The USER-CONFIGURED context window (`context_window` in config.toml, or a
    /// `[providers.<name>].context_window`), in tokens — for the status bar's
    /// "X of Y" and the auto-compaction trigger.
    ///
    /// An override, and the top of the precedence: it wins over the window derived
    /// from `(endpoint, model)` (see [`ResolvedModel::context_window`]) and over an
    /// endpoint probe. `None` = nothing configured; the agent derives or probes one.
    pub context_window: Option<u32>,
    /// Output-token cap (`max_tokens`). Required by the native Anthropic backend
    /// (`None` uses its 8192 default); on the OpenAI path it's sent only when set.
    pub max_tokens: Option<u32>,
    /// Nucleus-sampling `top_p`, sent only when set.
    pub top_p: Option<f32>,
    /// Determinism `seed`, sent only when set (provider support varies).
    pub seed: Option<i64>,
    /// Stop sequences, sent only when non-empty.
    pub stop: Vec<String>,
    /// Ask for streamed token usage (`stream_options.include_usage`); default
    /// `true`. A few strict/old servers reject it — set `false` to omit.
    pub stream_usage: bool,
    /// Connect + idle-read timeout in seconds for model requests. Defaults to
    /// five minutes; each received stream chunk resets the idle deadline.
    /// Configure `0` to disable the timeout explicitly.
    pub request_timeout: Option<u64>,
    /// Session retention: compress a session file whose mtime is older than this
    /// many seconds (zstd). `None` or `0` disables compression. Default one week.
    /// Swept by [`sweep_sessions`](crate::sweep_sessions).
    pub session_compress_after: Option<u64>,
    /// Session retention: purge an AUTO-NAMED session whose mtime is older than
    /// this many seconds. `None` or `0` disables purging. User-named sessions are
    /// never purged. Default one month.
    pub session_purge_after: Option<u64>,
    /// Prompt-cache TTL: `5m` (default) or `1h`. `1h` emits a longer
    /// `cache_control` TTL (Anthropic native + OpenRouter) — cheaper for stable
    /// prompts reused across a longer window. Only meaningful when caching is on.
    pub prompt_cache_ttl: Option<String>,
    /// Reasoning-effort label shown in the status bar (e.g. `low`/`medium`/`high`).
    pub effort: Option<String>,
    /// Whether auto-compaction is enabled. Default [`DEFAULT_AUTO_COMPACT`].
    /// The *trigger point* is set by
    /// [`compaction_reserved`](Self::compaction_reserved), not here — this is a
    /// plain on/off toggle. For backward compatibility the config parser still
    /// accepts the old fractional spelling (`auto_compact = 0.85`): any number
    /// `> 0` reads as `true`, `0` as `false`.
    pub auto_compact: bool,
    /// Token buffer reserved below the context window: auto-compaction fires when
    /// usage reaches `context_window − compaction_reserved` (opencode's reserved
    /// model). Default [`DEFAULT_COMPACTION_RESERVED`].
    pub compaction_reserved: u32,
    /// Most read-only sub-agents that may run at once (`max_readonly_subagents`,
    /// `HRDR_MAX_READONLY_SUBAGENTS`, `--max-readonly-subagents`). A `task` beyond
    /// the cap is refused with a message telling the model to wait.
    pub max_readonly_subagents: usize,
    /// Most write-capable sub-agents that may run at once
    /// (`max_write_subagents`, `HRDR_MAX_WRITE_SUBAGENTS`,
    /// `--max-write-subagents`). Lower than the read-only cap: they share the
    /// main agent's working tree.
    pub max_write_subagents: usize,
    /// Ceiling on ONE attachment (image or PDF), in **base64-encoded** bytes —
    /// the unit every provider states its own limits in, and roughly 4/3 the
    /// size of the file on disk (`max_attachment_bytes`,
    /// `HRDR_MAX_ATTACHMENT_BYTES`).
    ///
    /// `None` — the default — gives each type the cap of the provider hrdr's
    /// dialects are tightest against: 10 MB for an image (Anthropic's per-image
    /// limit), the 32 MB request budget for a PDF (which Anthropic does not cap
    /// separately). Set it for an endpoint with different limits — OpenAI allows
    /// 50 MB per file, a self-hosted server whatever it was built for — and it
    /// then applies to images and PDFs alike, because it is a ceiling the user
    /// stated rather than one hrdr inferred.
    ///
    /// Enforced in `hrdr_llm::media::check_attachments`, the gate every request
    /// passes through. A tiny value is legal and does mean "effectively no
    /// attachments"; `0` is refused, since it reads as a forgotten field rather
    /// than a choice.
    pub max_attachment_bytes: Option<usize>,
    /// User-defined providers from `[providers.<name>]` in config, keyed by name.
    pub providers: HashMap<String, ProviderConfig>,
    /// Extra shell guardrails from `[[guardrails]]` in config, applied on top
    /// of the built-in rules.
    pub guardrails: Vec<GuardrailConfig>,
    /// Post-edit hooks from `[[hooks]]` in config (formatters, mostly).
    pub hooks: Vec<HookConfig>,
    /// Post-edit LSP diagnostics (default `true`): after a mutating tool
    /// writes a file, its language server (spawned lazily, only when
    /// installed) checks it and any errors ride back with the tool result.
    /// `[lsp] enabled = false` / `$HRDR_LSP=0` turns it off.
    pub lsp: bool,
    /// Per-edit diagnostics wait in seconds (`[lsp] wait_secs`; default 2).
    pub lsp_wait_secs: Option<u64>,
    /// Custom `[[lsp.servers]]`, consulted before the built-in registry.
    pub lsp_servers: Vec<LspServerEntry>,
    /// Internal (sub-agents): the tool context receives the parent's shared
    /// `LspRegistry` after construction — register the LSP tools, but don't
    /// build a registry of our own (`lsp` is `false` alongside this).
    #[doc(hidden)]
    pub lsp_shared: bool,
    /// Per-tool output byte cap before truncation (`[tool_output] max_bytes`).
    /// Larger `bash`/`grep` output is truncated and the full text saved to disk.
    pub tool_max_bytes: usize,
    /// Per-tool output line cap before truncation (`[tool_output] max_lines`),
    /// applied alongside [`tool_max_bytes`](Self::tool_max_bytes).
    pub tool_max_lines: usize,
    /// Recent turns kept verbatim through compaction (`compaction_tail_turns`).
    /// Default [`DEFAULT_TAIL_TURNS`].
    pub compaction_tail_turns: usize,
    /// Token budget for the verbatim tail kept through compaction
    /// (`preserve_recent_tokens`). Default [`DEFAULT_PRESERVE_RECENT_TOKENS`].
    pub preserve_recent_tokens: u32,
    /// MCP servers from `[[mcp]]` config; connected by [`Agent::connect_mcp`].
    pub mcp: Vec<McpServerConfig>,
    /// Prompt-caching mode: `off`, `on` (alias `ephemeral`), or `auto` (default).
    /// `auto` emits `cache_control` breakpoints for remote endpoints and skips
    /// them for a local server (which may reject the content-parts form). `None`
    /// means `auto`. See [`resolve_cache_mode`].
    pub prompt_cache: Option<String>,
    /// DERIVED from [`model`](Self::model): the extra HTTP headers of its provider
    /// (from `[providers.<name>].headers`), sent with every request.
    pub headers: Vec<(String, String)>,
    /// DERIVED from [`model`](Self::model): its provider's Azure OpenAI API version
    /// (see [`ProviderConfig::api_version`]); enables the Azure URL + auth quirks.
    pub api_version: Option<String>,
    /// Expose the `task` tool so the model can delegate self-contained sub-tasks
    /// to a fresh sub-agent. Default `true`; forced `false` inside a sub-agent so
    /// it can't spawn its own (bounding recursion to one level).
    pub subagents: bool,
    /// Expose the `memory` tool and auto-load saved notes into the system prompt.
    /// Default `true`; `$HRDR_MEMORY`. Storage lives under the XDG data dir
    /// (project-scoped by cwd, plus a shared global scope).
    pub memory: bool,
    /// Turns a completed TODO stays in the list before it is aged out. The list is
    /// agent state the model re-reads every turn, so this is the agent's business:
    /// without it a headless run (or any sub-agent) accumulates finished items
    /// forever and pays for them in context.
    pub todo_ttl: u64,
    /// This agent is a **delegated sub-agent**, not the session's own agent.
    ///
    /// The seam between the two. A sub-agent is transient and task-scoped: it
    /// exists to answer one question and be released. Anything that belongs to
    /// the *session* — durable memory that outlives it, lifecycle hooks that mark
    /// the user's turn, proactive compaction of a conversation it does not own —
    /// is gated on this and must not fire for it. Safety-scoped machinery
    /// (guardrails, pre/post-tool hooks, the cost ceiling) deliberately still
    /// applies: those constrain *tool calls*, and a sub-agent makes those too.
    ///
    /// Set only by [`subagent_base_config`]; never configurable.
    pub delegated: bool,
    /// Override the base memory directory (default `<XDG data>/memory`) — point
    /// hrdr at another tool's memory store. The `projects/<cwd-slug>/` and
    /// `global/` scope subdirectories still apply beneath it. Config
    /// `memory_dir`, `--memory-dir`, `$HRDR_MEMORY_DIR`.
    pub memory_dir: Option<PathBuf>,
    /// Explicit `(project, global)` memory roots that override the cwd-derived
    /// ones. Set for a delegated sub-agent so it shares the **parent's** project
    /// memory rather than keying the project scope by its own cwd — a write
    /// sub-agent may run in a narrower `cwd` than the parent, whose slug would
    /// otherwise resolve to a
    /// throwaway, empty project scope instead of the repo's. `None` = derive from
    /// cwd (the main agent's path). Runtime-only; not read from config.
    pub memory_roots: Option<(PathBuf, PathBuf)>,
    /// Default model for delegated sub-agents. A bare id is that model on the main
    /// agent's provider/endpoint — the "Opus drives, Sonnet implements" knob; a
    /// `provider://model` moves them to another provider entirely. `None` reuses the
    /// main agent's identity; the `task` tool's `model` argument overrides per call.
    pub subagent_model: Option<ModelSpec>,
    /// Named sub-agent profiles from `[[subagent]]` config, each pinning a
    /// provider + model. The `task` tool's `agent` argument selects one, letting
    /// a sub-agent run on a **different provider** than the main agent.
    pub subagent_profiles: Vec<SubagentProfile>,
    /// Persona appended to this (sub-)agent's system prompt (its role). Set from
    /// a sub-agent profile's `prompt`; `None` for the main agent.
    pub agent_prompt: Option<String>,
    /// Tool allow-list scoping this (sub-)agent's registry. `None` = the full
    /// default set. Takes precedence over [`read_only`](Self::read_only).
    pub allowed_tools: Option<Vec<String>>,
    /// Scope this (sub-)agent to the read-only tools (see
    /// [`ToolRegistry::read_only_names`]). Ignored when `allowed_tools` is set.
    pub read_only: bool,
    /// Filesystem confinement for this *session* (`sandbox`, `$HRDR_SANDBOX`,
    /// `--sandbox`/`--no-sandbox`). This is the session **default**, not the mode
    /// any one agent runs under: each agent — main or sub — derives its own from
    /// this plus its [`read_only`](Self::read_only) flag via
    /// [`effective_sandbox`], once, in `Agent::new`.
    pub sandbox: SandboxMode,
    /// The mode this agent's **profile declared**, if it declared one, and the
    /// session mode it displaced — kept only so `Agent::new` can say that it did.
    ///
    /// A pair rather than a bool, because the notice has to name both: "declares
    /// `jail`, overriding the session's `none`" is actionable where "sandbox
    /// overridden" is a puzzle. `None` for every agent that derives normally, which
    /// is all of them but `prisoner`.
    pub declared_sandbox: Option<SandboxMode>,
    /// The session's own mode, before a profile's declaration displaced it — see
    /// [`declared_sandbox`](Self::declared_sandbox).
    pub session_sandbox: SandboxMode,
    /// Extra directories a confined agent may write to on top of the built-in
    /// writable roots (cwd, temp, scratch, tool-output, git metadata, the
    /// package-manager caches) — the escape hatch for a **bespoke** layout the
    /// defaults do not cover, not the mechanism by which mainstream tooling works.
    /// Absolute paths only; a relative entry is a config-file error.
    pub sandbox_writable_roots: Vec<PathBuf>,
    /// Wrap every tool result in an untrusted-content envelope
    /// (`wrap_tool_results = true`). Default `false`; always on in
    /// [`SandboxMode::Jail`] regardless, where the content under audit is the thing
    /// being distrusted.
    ///
    /// One bool rather than a mode, because wanting provenance markers on tool
    /// output does not mean wanting a read-only agent with no shell. The cost is
    /// tokens and a little noise on every result, which is why it is off by
    /// default.
    pub wrap_tool_results: bool,
    /// Shared cell holding the parent session's sub-agent transcript directory
    /// (`sessions/<slug>/subagents/<id>/`), resolved lazily because the session
    /// id is assigned on first autosave, not at construction. The `task` tool
    /// reads it at spawn: `None` (outer) = feature off; `Some` with an inner
    /// `None` = id not yet assigned (pre-first-save) so that spawn is not
    /// persisted. Cleared for sub-agent base configs (subs don't spawn subs).
    pub child_transcript_dir: Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>,
}

/// A named sub-agent profile (`[[subagent]]`): a model the `task` tool can
/// delegate to, so a sub-agent can run on a different model — or a different
/// **provider** — than the main agent (e.g. Opus on Anthropic manages, a model on
/// another provider implements).
#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SubagentProfile {
    /// Name the model refers to (the `task` tool's `agent` argument).
    pub name: String,
    /// The model this sub-agent runs on, as ONE key: a bare id (`kimi-k2`) is that
    /// model on the main agent's provider; a `provider://model`
    /// (`openrouter://deepseek/deepseek-chat`) names the provider too, and the
    /// endpoint, key and headers follow it. Omit (or `model: inherit` in an
    /// `agents/*.md` file) to run on the main agent's identity unchanged.
    ///
    /// There is no separate `provider` key — that pair could always disagree, and a
    /// config still carrying one is refused at startup.
    #[serde(default)]
    pub model: Option<ModelSpec>,
    /// One-line hint shown to the model so it can pick the right sub-agent.
    #[serde(default)]
    pub description: Option<String>,
    /// Persona / operating instructions appended to the sub-agent's system
    /// prompt (its role). Omit to reuse the main agent's prompt unchanged.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Restrict this sub-agent to the read-only tool set — the observing tools
    /// **plus a shell**, since the confinement that makes it read-only is the
    /// sandbox (`SandboxMode::Read` grants no writable root at all), not the absence
    /// of a shell. Without one, an `explore` or `review` agent could not run the
    /// thing reviewing a change is mostly made of: `git log`, `git diff`, a linter,
    /// a test. Ignored when `tools` is set explicitly.
    ///
    /// `None` means "not specified by this profile" — distinct from `Some(false)` —
    /// so overlaying a profile onto a built-in (e.g. pinning `review`'s model)
    /// doesn't silently clear a built-in's `read_only = true`. Use
    /// [`is_read_only`](Self::is_read_only) to read the effective value.
    #[serde(default)]
    pub read_only: Option<bool>,
    /// Filesystem confinement this agent runs under, **regardless of the session's
    /// mode** — including `--yolo`.
    ///
    /// > **A declared mode is absolute; an undeclared one derives from the session
    /// > exactly as [`effective_sandbox`] does.**
    ///
    /// Not "strictest of both": a `--sandbox jail` session with a write-capable
    /// agent would resolve to jail and the agent could not write at all, and the
    /// write floor in `effective_sandbox` exists for good reason. And not
    /// "loosest of both" either, which is the consequence worth stating: **a
    /// declared mode ignores `--yolo`.** `--yolo` plus the `prisoner` agent gives
    /// you a contained prisoner, because containment is what that agent *is* — you
    /// spawned it precisely to contain something. That reverses "session `none` wins
    /// everywhere", so it is not done quietly: overriding the session emits a
    /// notice.
    ///
    /// Therefore: **declare a mode only when containment is part of the agent's
    /// identity.** `prisoner` declares `jail`. `coder`, `explore`, `review`, `plan`
    /// and `general` declare nothing and keep deriving, so `--yolo` still means yolo
    /// for them.
    #[serde(default)]
    pub sandbox: Option<SandboxMode>,
    /// Explicit tool allow-list for this sub-agent (overrides `read_only`).
    /// Omit for the full default tool set.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Sampling temperature for this sub-agent. Omit to inherit the main agent's.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Reasoning effort (`minimal`/`low`/`medium`/`high`) for this sub-agent.
    /// Omit to inherit the main agent's — e.g. `high` for a careful reviewer.
    #[serde(default)]
    pub effort: Option<String>,
    /// Tool-call iteration cap for this sub-agent. Omit to inherit the main
    /// agent's `max_steps` — e.g. a small cap on a quick focused sub-task.
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Nudge the main agent to **delegate matching work here on its own** (rather
    /// than only when told). The `task` tool lists proactive agents with a
    /// stronger call-to-action so the model reaches for them when a sub-task fits
    /// their `description`. `None` means "not specified" — see
    /// [`is_proactive`](Self::is_proactive) for the effective value.
    #[serde(default)]
    pub proactive: Option<bool>,
}

impl SubagentProfile {
    /// The effective read-only-ness: unset (`None`) means not restricted.
    pub fn is_read_only(&self) -> bool {
        self.read_only.unwrap_or(false)
    }
    /// The effective proactive-ness: unset (`None`) means opt-in only.
    pub fn is_proactive(&self) -> bool {
        self.proactive.unwrap_or(false)
    }
}

/// A user-defined provider from `[providers.<name>]` in config.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// OpenAI-compatible base URL (including the `/v1` suffix).
    pub base_url: String,
    /// Environment variable holding the API key (preferred over an inline key).
    #[serde(default)]
    pub key_env: Option<String>,
    /// Inline API key (avoid in shared config; prefer `key_env`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default model for this provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Whether this is a remote/hosted provider that needs an API key
    /// (default: true). A local/self-hosted endpoint sets `false` to silence
    /// the missing-key and missing-model warnings.
    #[serde(default)]
    pub remote: Option<bool>,
    /// Model context window (for the status bar's "X of Y").
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Extra HTTP headers sent with every request to this provider (e.g.
    /// OpenRouter's `HTTP-Referer`/`X-Title`, or a custom auth/routing header).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Azure OpenAI API version. When set, requests append `?api-version=<v>` and
    /// auth via an `api-key` header instead of `Bearer` (point `base_url` at
    /// `https://<resource>.openai.azure.com/openai/deployments/<deployment>`).
    #[serde(default)]
    pub api_version: Option<String>,
}

/// One user-defined shell guardrail from a `[[guardrails]]` config entry:
/// commands matching `pattern` (a regex) are rejected with `message`. Applied
/// on top of the built-in rules (`hrdr_tools::default_guardrails`).
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GuardrailConfig {
    pub pattern: String,
    pub message: String,
}

/// One hook from a `[[hooks]]` config entry.
///
/// Without `event` it is a **post-edit file hook**: after tool `on` (`edit`,
/// `write`, or `*`) successfully mutates a file matching `glob`, run shell
/// command `run` (`{path}` is substituted). Formatters, mostly. Failures
/// surface as warnings in the tool result, never as errors.
///
/// With `event` it is a **lifecycle hook** (`pre_tool`, `post_tool`,
/// `user_prompt`, `turn_end`, `session_start`, `session_end`): `run` receives
/// the event payload as JSON on stdin; for the tool events `on` filters by
/// tool name. Exit 2 blocks the tool call / prompt; other failures warn. See
/// [`hrdr_tools::run_event_hooks`].
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Lifecycle event name; absent = a post-edit file hook.
    #[serde(default)]
    pub event: Option<String>,
    /// Triggering tool; defaults to `*` (any file-mutating tool, or — for a
    /// lifecycle tool event — any tool).
    #[serde(default = "default_hook_on")]
    pub on: String,
    /// File filter (matched against name and cwd-relative path); absent =
    /// every file. File hooks only.
    #[serde(default)]
    pub glob: Option<String>,
    /// Shell command template; `{path}` becomes the quoted file path (file
    /// hooks), and lifecycle hooks read the JSON payload from stdin.
    pub run: String,
    /// Per-run timeout in seconds (default 30). `0` means the default rather
    /// than "kill it instantly", which is never what anyone means.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// **Removed**, kept only to reject it. Timeouts are seconds now, and serde
    /// ignores unknown fields — so a config still saying `timeout_ms = 120000`
    /// would silently run on the 30-second default instead of the two minutes it
    /// asked for. Fail loudly with the converted value instead.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub(crate) fn default_hook_on() -> String {
    "*".to_string()
}

/// The `[lsp]` config table: post-edit diagnostics from language servers.
#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LspFileConfig {
    /// Master switch (default on; servers only spawn when installed anyway).
    pub enabled: Option<bool>,
    /// Per-edit wait for diagnostics, in seconds (default 2). `0` skips the
    /// wait entirely.
    pub wait_secs: Option<u64>,
    /// **Removed**, kept only to reject it — see [`HookEntry::timeout_ms`].
    pub wait_ms: Option<u64>,
    /// Custom servers (`[[lsp.servers]]`), consulted before the built-ins so
    /// they win for their extensions.
    #[serde(default)]
    pub servers: Vec<LspServerEntry>,
}

/// One custom language server from `[[lsp.servers]]`.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LspServerEntry {
    /// Executable on PATH.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions (no dot) routed to it.
    pub extensions: Vec<String>,
    /// Server-specific settings sent as `initializationOptions` in the LSP
    /// `initialize` request — the server's own config object (e.g.
    /// `{ cargo = { targetDir = true } }` for rust-analyzer). Omitted: none.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
}

/// Trust identity stamped onto a [`ResolvedProvider`] by
/// [`AgentConfig::resolve_provider`] — the SOLE trust gate. A provider that
/// matches a user's `[providers.<name>]` entry resolves to `Custom` BEFORE the
/// built-in fallback runs, so a custom provider spelled `chatgpt`/`codex`/
/// `openai-oauth` can never earn `ChatGptOAuth` trust by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedProviderKind {
    /// A user-defined `[providers.<name>]` entry. Never OAuth-trusted.
    Custom,
    /// A built-in preset that authenticates with an API key.
    BuiltIn,
    /// The built-in ChatGPT subscription login (Codex OAuth). The only kind that
    /// may read the canonical `chatgpt` OAuth credential slot or receive the
    /// `Authorization`/`ChatGPT-Account-Id` header injection.
    ChatGptOAuth,
}

/// Whether a resolved provider is ready to use, and how it authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthState {
    /// An API key is available (inline, env, saved `/login`, or shared parent).
    Key,
    /// Trusted ChatGPT OAuth with usable or refreshable credentials.
    OAuth,
    /// A remote provider that serves *some* of its models to callers with no
    /// account at all — today only OpenCode Zen, which takes the literal key
    /// `public` and answers for its zero-cost models (see [`public_api_key`]).
    /// Usable, but only for the free subset: [`crate::model_choices`] narrows
    /// the picker to those rows, since a priced one would 401.
    Anonymous,
    /// A keyless local endpoint (`remote = false`); no credential needed.
    Keyless,
    /// A remote provider with no key and no usable OAuth credential.
    Missing,
}

/// A fully-resolved provider preset.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub base_url: String,
    pub key_env: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub remote: bool,
    pub context_window: Option<u32>,
    /// Extra HTTP headers to send with every request to this provider.
    pub headers: HashMap<String, String>,
    /// Azure OpenAI API version, if this is an Azure endpoint.
    pub api_version: Option<String>,
    /// Trust identity — set only by [`AgentConfig::resolve_provider`]. See
    /// [`ResolvedProviderKind`].
    pub kind: ResolvedProviderKind,
}

/// `[tool_output]` config table: per-tool truncation thresholds. Mirrors
/// opencode's `tool_output`. Output over either limit is truncated and (for
/// `bash`/`grep`) the full text is saved to disk.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolOutputConfig {
    /// Max output lines before truncation (default 2000).
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// Max output bytes before truncation (default 51200).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Subset of config.toml we parse; all fields are optional.
///
/// The model identity is ONE key — `model = "openrouter://deepseek/deepseek-chat"`
/// — deserialized as a [`ModelSpec`] (a bare id is that model on the provider in
/// effect). There is no top-level `provider = …` selector and no free-floating
/// `base_url = …`: **the endpoint belongs to the provider**, and lives in the
/// `[providers.<name>]` table that defines it (or in a built-in preset). Either
/// would be a second, independent way to say where a request goes, free to
/// disagree with the provider actually in force — and a free-floating endpoint
/// relocated whichever provider was in effect, taking that provider's API key with
/// it.
///
/// Neither has a bespoke "you wrote the old form" message: `deny_unknown_fields`
/// refuses them as the unknown keys they are. hrdr is pre-1.0 and carries no
/// back-compat.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileConfig {
    pub(crate) api_key: Option<String>,
    pub(crate) model: Option<ModelSpec>,
    pub(crate) temperature: Option<f32>,
    pub(crate) context_window: Option<u32>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) top_p: Option<f32>,
    pub(crate) seed: Option<i64>,
    #[serde(default)]
    pub(crate) stop: Vec<String>,
    pub(crate) stream_usage: Option<bool>,
    pub(crate) request_timeout: Option<u64>,
    pub(crate) session_compress_after: Option<u64>,
    pub(crate) session_purge_after: Option<u64>,
    pub(crate) prompt_cache_ttl: Option<String>,
    pub(crate) max_cost: Option<f64>,
    pub(crate) allow_unpriced: Option<bool>,
    pub(crate) subagents: Option<bool>,
    pub(crate) memory: Option<bool>,
    pub(crate) memory_dir: Option<String>,
    pub(crate) subagent_model: Option<ModelSpec>,
    #[serde(default)]
    pub(crate) subagent: Vec<SubagentProfile>,
    pub(crate) effort: Option<String>,
    #[serde(default, deserialize_with = "de_bool_or_num")]
    pub(crate) auto_compact: Option<bool>,
    pub(crate) compaction_reserved: Option<u32>,
    pub(crate) max_readonly_subagents: Option<usize>,
    pub(crate) max_write_subagents: Option<usize>,
    /// Per-attachment ceiling in base64-encoded bytes; see
    /// [`AgentConfig::max_attachment_bytes`].
    pub(crate) max_attachment_bytes: Option<usize>,
    /// `sandbox = "write" | "read" | "none"`. A misspelling is a hard TOML parse
    /// error (the enum's serde derive), matching the file-values-are-errors rule.
    pub(crate) sandbox: Option<SandboxMode>,
    #[serde(default)]
    pub(crate) sandbox_writable_roots: Vec<String>,
    /// `wrap_tool_results = true` — mark every tool result as untrusted content.
    pub(crate) wrap_tool_results: Option<bool>,
    #[serde(default)]
    pub(crate) providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub(crate) guardrails: Vec<GuardrailConfig>,
    #[serde(default)]
    pub(crate) hooks: Vec<HookConfig>,
    pub(crate) tool_output: Option<ToolOutputConfig>,
    pub(crate) compaction_tail_turns: Option<usize>,
    pub(crate) preserve_recent_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) mcp: Vec<McpServerConfig>,
    pub(crate) prompt_cache: Option<String>,
    pub(crate) lsp: Option<LspFileConfig>,

    // ── The FRONTEND's keys ─────────────────────────────────────────────────
    //
    // One `config.toml`, two readers: the agent parses this struct, and the
    // frontend parses `hrdr_app::UiFileConfig` from the same file. Each has
    // always ignored the other's keys — but `deny_unknown_fields` cannot tell
    // "a key that belongs to the other layer" from "a typo", and turning it on
    // made every display setting a fatal startup error: a config with
    // `statusbar = "truncate"` in it refused to start, naming a list of valid
    // keys that did not include the one the user had written.
    //
    // So they are declared, and ignored. That keeps `deny_unknown_fields`
    // meaning what it says — a key neither layer knows is still refused — and
    // the split stays honest rather than lenient. `hrdr-app` cannot be a
    // dependency here (the dependency runs the other way), so
    // `hrdr_app::config`'s `the_agent_accepts_every_ui_key` test pins the two
    // lists together: add a UI key without adding it here and it fails.
    #[serde(default, rename = "vim")]
    pub(crate) _ui_vim: Option<toml::Value>,
    #[serde(default, rename = "theme")]
    pub(crate) _ui_theme: Option<toml::Value>,
    #[serde(default, rename = "icons")]
    pub(crate) _ui_icons: Option<toml::Value>,
    #[serde(default, rename = "statusbar")]
    pub(crate) _ui_statusbar: Option<toml::Value>,
    #[serde(default, rename = "bell")]
    pub(crate) _ui_bell: Option<toml::Value>,
    #[serde(default, rename = "auto_resume")]
    pub(crate) _ui_auto_resume: Option<toml::Value>,
    #[serde(default, rename = "todo_ttl")]
    pub(crate) _ui_todo_ttl: Option<toml::Value>,
    #[serde(default, rename = "scrollback")]
    pub(crate) _ui_scrollback: Option<toml::Value>,
}

impl FileConfig {
    /// Per-field bounds check on the raw config-file values, accumulating a hard
    /// error for each one that is out of range (see the [module docs](self) for
    /// why file values are errors, not warnings). Values the file does not set
    /// (`None`) and values in range produce nothing.
    ///
    /// Only the *nonsense* boundaries are rejected — a zero that silently
    /// disables a whole subsystem. Documented sentinels (`request_timeout = 0`
    /// disables the timeout; a zero compaction reserve / preserve budget) are
    /// left to [`AgentConfig::validate_semantics`] or accepted outright.
    pub(crate) fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut require_min1 = |value: Option<u64>, field: &str, what: &str| {
            if value == Some(0) {
                errors.push(format!(
                    "{field} = 0 is invalid ({what}); it must be at least 1"
                ));
            }
        };
        require_min1(
            self.max_readonly_subagents.map(|v| v as u64),
            "max_readonly_subagents",
            "it would refuse every read-only sub-agent",
        );
        require_min1(
            self.max_write_subagents.map(|v| v as u64),
            "max_write_subagents",
            "it would refuse every write-capable sub-agent",
        );
        require_min1(
            self.max_attachment_bytes.map(|v| v as u64),
            "max_attachment_bytes",
            "it would refuse every attachment",
        );
        if let Some(to) = &self.tool_output {
            require_min1(
                to.max_lines.map(|v| v as u64),
                "tool_output.max_lines",
                "it would truncate all tool output to nothing",
            );
            require_min1(
                to.max_bytes.map(|v| v as u64),
                "tool_output.max_bytes",
                "it would truncate all tool output to nothing",
            );
        }
        require_min1(
            self.context_window.map(|v| v as u64),
            "context_window",
            "the model would have no room to run",
        );
        require_min1(
            self.max_tokens.map(|v| v as u64),
            "max_tokens",
            "the model could emit no output",
        );
        // A relative writable root cannot be checked against a canonicalized
        // path, and `~` never expands here — both would silently widen or
        // narrow the sandbox, so they are refused outright.
        for root in &self.sandbox_writable_roots {
            if !std::path::Path::new(root).is_absolute() {
                errors.push(format!(
                    "sandbox_writable_roots entries must be absolute paths: {root:?}"
                ));
            }
        }
        // The millisecond spellings are gone. Serde ignores unknown fields, so
        // leaving them unmentioned would silently run a config that asked for two
        // minutes on the 30-second default — the exact hazard the model-facing
        // `timeout_ms` guard exists for. Name the field and do the division, so the
        // fix is the message.
        let ms_gone = |value: Option<u64>, old: &str, new: &str| -> Option<String> {
            value.map(|ms| {
                format!(
                    "`{old}` is gone — timeouts are seconds now; use `{new} = {}`",
                    (ms / 1000).max(1)
                )
            })
        };
        for hook in &self.hooks {
            errors.extend(ms_gone(hook.timeout_ms, "hooks.timeout_ms", "timeout_secs"));
        }
        if let Some(lsp) = &self.lsp {
            errors.extend(ms_gone(lsp.wait_ms, "lsp.wait_ms", "wait_secs"));
        }
        errors
    }
}

/// One MCP server from a `[[mcp]]` config entry, registered with its tools
/// namespaced `<name>_<tool>`. Three transports: **stdio** (set `command`)
/// spawns `command args…` with `env`; **HTTP** (set `url`) POSTs to a
/// Streamable-HTTP endpoint with `headers` (e.g. auth); **legacy HTTP+SSE**
/// (set `url` and `transport = "sse"`) opens a persistent SSE stream and POSTs
/// to the server-advertised endpoint. Exactly one of `command`/`url` is
/// required. `disabled = true` keeps the entry but skips it.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Short name; namespaces the server's tools and labels its errors.
    pub name: String,
    /// stdio transport: executable to spawn (found on `PATH`).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the `command` process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: the endpoint URL. Streamable-HTTP by default; legacy
    /// two-endpoint HTTP+SSE when `transport = "sse"`.
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP transport selector: `"http"` (Streamable-HTTP, default) or `"sse"`
    /// (legacy HTTP+SSE). Ignored for the stdio transport.
    #[serde(default)]
    pub transport: Option<String>,
    /// Extra HTTP headers sent with every request (e.g. `Authorization`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Keep the entry but don't connect.
    #[serde(default)]
    pub disabled: bool,
}

/// A value to persist into the user config file.
pub enum ConfigValue<'a> {
    Str(&'a str),
    Bool(bool),
    Float(f64),
    Int(i64),
}

// ── Defaults ────────────────────────────────────────────────────────────────

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            // `local` IS the default endpoint (`http://localhost:8080/v1`) and
            // `default` IS the default model id — the pair the two old fields
            // carried, now spelled as the one identity they always were.
            model: DEFAULT_MODEL_REF.parse().expect("a valid default identity"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temperature: None,
            max_steps: 300,
            retry: RetryPolicy::default(),
            max_cost: None,
            allow_unpriced: false,
            context_window: None,
            max_tokens: None,
            top_p: None,
            seed: None,
            stop: Vec::new(),
            stream_usage: true,
            request_timeout: Some(300),
            session_compress_after: Some(DEFAULT_SESSION_COMPRESS_AFTER),
            session_purge_after: Some(DEFAULT_SESSION_PURGE_AFTER),
            prompt_cache_ttl: None,
            effort: None,
            auto_compact: DEFAULT_AUTO_COMPACT,
            compaction_reserved: DEFAULT_COMPACTION_RESERVED,
            max_readonly_subagents: DEFAULT_MAX_READONLY_SUBAGENTS,
            max_write_subagents: DEFAULT_MAX_WRITE_SUBAGENTS,
            max_attachment_bytes: None,
            providers: HashMap::new(),
            guardrails: Vec::new(),
            hooks: Vec::new(),
            tool_max_bytes: DEFAULT_MAX_OUTPUT,
            tool_max_lines: DEFAULT_MAX_OUTPUT_LINES,
            compaction_tail_turns: DEFAULT_TAIL_TURNS,
            preserve_recent_tokens: DEFAULT_PRESERVE_RECENT_TOKENS,
            mcp: Vec::new(),
            prompt_cache: None,
            headers: Vec::new(),
            api_version: None,
            subagents: true,
            memory: true,
            todo_ttl: DEFAULT_TODO_TTL_TURNS,
            delegated: false,
            memory_dir: None,
            memory_roots: None,
            subagent_model: None,
            subagent_profiles: Vec::new(),
            agent_prompt: None,
            allowed_tools: None,
            read_only: false,
            sandbox: SandboxMode::Write,
            declared_sandbox: None,
            session_sandbox: SandboxMode::Write,
            sandbox_writable_roots: Vec::new(),
            wrap_tool_results: false,
            child_transcript_dir: None,
            lsp: true,
            lsp_wait_secs: None,
            lsp_servers: Vec::new(),
            lsp_shared: false,
        }
    }
}

// ── Provider resolution ─────────────────────────────────────────────────────

impl AgentConfig {
    /// Resolve a provider name to a preset: a `[providers.<name>]` entry from
    /// config takes precedence over the built-ins (`zen`/`openai`/`local`).
    pub fn resolve_provider(&self, name: &str) -> Option<ResolvedProvider> {
        resolve_provider_in(&self.providers, name)
    }

    /// Whether the identity in force is the `default` model sentinel — i.e. no
    /// real model id was ever named (see [`DEFAULT_MODEL`]).
    pub fn has_default_model(&self) -> bool {
        self.model.model() == DEFAULT_MODEL
    }

    /// Cross-field checks on the fully-merged config: a value that is fine alone
    /// but incompatible with another. Returns a hard error per incompatible pair
    /// (these reflect the user's config-file/CLI intent — see the
    /// [module docs](self)). Per-field bounds live in [`FileConfig::validate`].
    ///
    /// A reserve carved out of the context window must be *smaller* than it: the
    /// auto-compaction trigger is `context_window − compaction_reserved`, and a
    /// verbatim tail of `preserve_recent_tokens` has to fit too. When the window
    /// is unset (`None`) it is derived or probed later, so nothing is checked.
    pub(crate) fn validate_semantics(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(window) = self.context_window {
            if self.compaction_reserved >= window {
                errors.push(format!(
                    "compaction_reserved ({}) must be smaller than context_window ({window}); \
                     the auto-compaction trigger (context_window − compaction_reserved) would \
                     leave no room to run",
                    self.compaction_reserved,
                ));
            }
            if self.preserve_recent_tokens >= window {
                errors.push(format!(
                    "preserve_recent_tokens ({}) must be smaller than context_window ({window}); \
                     the verbatim tail kept through compaction cannot exceed the window",
                    self.preserve_recent_tokens,
                ));
            }
        }
        errors
    }
}

/// [`AgentConfig::resolve_provider`] against a bare provider table — the form
/// [`resolve_in`] and a live [`Agent`] need (neither holds a whole config).
///
/// BOTH sides of the lookup go through [`ProviderName`], the one owner of what a
/// provider name *is*. The map is keyed canonically ([`canonical_providers`] rekeys
/// it at config load) and the name being looked up is folded here, so a
/// `[providers.anthropic]` entry is found by a `claude://…` identity — and, the
/// other way, a `[providers.codex]` entry SHADOWS the built-in ChatGPT preset
/// instead of missing the map and handing the user's own endpoint the account's
/// OAuth bearer. Folding on only one side is exactly that bug.
pub fn resolve_provider_in(
    providers: &HashMap<String, ProviderConfig>,
    name: &str,
) -> Option<ResolvedProvider> {
    let canonical = ProviderName::new(name);
    if let Some((_, c)) = providers
        .iter()
        .find(|(k, _)| ProviderName::new(k) == canonical)
    {
        return Some(ResolvedProvider {
            base_url: c.base_url.clone(),
            key_env: c.key_env.clone(),
            api_key: c.api_key.clone(),
            model: c.model.clone(),
            remote: c.remote.unwrap_or(true),
            context_window: c.context_window,
            headers: c.headers.clone(),
            api_version: c.api_version.clone(),
            // A user-defined entry is Custom — never OAuth-trusted, even when
            // spelled `chatgpt`/`codex`/`openai-oauth`. This branch runs
            // BEFORE `builtin_provider`, so it shadows the built-in name.
            kind: ResolvedProviderKind::Custom,
        });
    }
    builtin_provider(name)
}

/// Rekey a raw `[providers.*]` map by the CANONICAL provider name, so the table
/// and every lookup into it live in the same namespace.
///
/// The TOML keys are whatever the user typed (`anthropic`, `Codex`, `opencode`);
/// every identity that reaches the table is a [`ProviderName`], which has already
/// folded those onto `claude` / `chatgpt` / `zen`. Rekeying here is the other half
/// of the fold in [`resolve_provider_in`] — with both, no consumer of the map (the
/// `/model` picker, `subagent_usage`, the auth gate) can disagree with `resolve()`
/// about which entry a name means.
///
/// Two spellings of ONE provider are a collision, and are refused at startup by
/// [`provider_alias_collision_error`]; this function is total, so it settles them
/// deterministically (first by original key order) rather than by `HashMap` luck.
pub fn canonical_providers(
    raw: HashMap<String, ProviderConfig>,
) -> HashMap<String, ProviderConfig> {
    let mut keys: Vec<String> = raw.keys().cloned().collect();
    keys.sort();
    let mut out: HashMap<String, ProviderConfig> = HashMap::new();
    for k in keys {
        let canonical = ProviderName::new(&k).as_str().to_string();
        let Some(c) = raw.get(&k) else { continue };
        out.entry(canonical).or_insert_with(|| c.clone());
    }
    out
}

/// The startup refusal for a config naming ONE provider twice — `[providers.anthropic]`
/// beside `[providers.claude]`, `[providers.codex]` beside `[providers.chatgpt]`, and
/// so on. `Some(message)` names both spellings and the one name they fold onto.
///
/// The aliases are not two providers; they are two ways to write one. Before the
/// fold, a table could carry both and hrdr would pick whichever the `HashMap`
/// handed it. Now they are the same key — so the config is asking for two different
/// endpoints under one identity, and the only honest answer is to stop and say so.
pub fn provider_alias_collision_error(text: &str, path: &std::path::Path) -> Option<String> {
    // `toml::Table`, not `toml::Value`: since toml 1.0 a `Value`'s `FromStr`
    // parses a single VALUE, so a whole document fails at the first table header
    // ("unexpected content, expected nothing"). That failure is silent here — the
    // `.ok()?` would turn it into "no collision found" and this refusal would stop
    // refusing anything.
    let root = text.parse::<toml::Table>().ok()?;
    let providers = root.get("providers")?.as_table()?;
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    for name in names {
        let canonical = ProviderName::new(name).as_str().to_string();
        if let Some(first) = seen.get(&canonical) {
            return Some(format!(
                "hrdr: {} defines the same provider twice.\n  \
                 [providers.{first}] and [providers.{name}] are both `{canonical}` — \
                 they are two spellings of one provider, not two providers.\n  \
                 Keep one of them (`[providers.{canonical}]` is the canonical spelling) \
                 and delete the other.",
                path.display(),
            ));
        }
        seen.insert(canonical, name.clone());
    }
    None
}

/// **The** Codex OAuth gate: the trusted [`ResolvedProviderKind::ChatGptOAuth`]
/// kind AND the canonical [`CHATGPT_CODEX_BASE_URL`] endpoint.
///
/// Both halves are required, and this is the only place the conjunction is
/// written. The kind alone is not enough — a `chatgpt` identity sitting at any
/// other URL must not have the OAuth bearer or the `ChatGPT-Account-Id` header
/// injected into it. The URL alone is not enough — a `[providers.*]` entry aimed
/// at the Codex URL resolves `Custom` and never earns the account's credentials.
///
/// [`ResolvedModel::is_codex_oauth`] is the same test, asked of a resolved model.
pub fn is_codex_oauth(kind: ResolvedProviderKind, base_url: &str) -> bool {
    kind == ResolvedProviderKind::ChatGptOAuth && base_url == CHATGPT_CODEX_BASE_URL
}

/// Resolve the API key for a provider: an inline key wins, then the provider's
/// `key_env` variable, then a credential saved by `/login`, then (only when
/// `parent_base_url` names the *same* endpoint as `p.base_url`) the calling
/// agent's own key. `None` when none is available (a keyless local endpoint, or
/// a remote that hasn't been set up).
///
/// The `parent_base_url` guard matters for sub-agent profiles: a profile can
/// name a *different* provider than the main agent's. Falling back to the
/// parent's key unconditionally would send that credential to a different
/// host's endpoint — a cross-provider key leak. The fallback is only safe when
/// the sub-agent resolves to the same base URL the parent is already
/// authenticated against (e.g. an unprofiled/default sub-agent, or a profile
/// that just changes the model on the same provider).
pub fn resolve_api_key(
    provider: &str,
    p: &ResolvedProvider,
    parent_key: Option<&str>,
    parent_base_url: Option<&str>,
) -> Option<String> {
    p.api_key
        .clone()
        .or_else(|| p.key_env.as_ref().and_then(|e| std::env::var(e).ok()))
        .or_else(|| auth_token(provider))
        .or_else(|| {
            let same_endpoint = parent_base_url
                .is_some_and(|u| u.trim_end_matches('/') == p.base_url.trim_end_matches('/'));
            if same_endpoint {
                parent_key.map(String::from)
            } else {
                None
            }
        })
}

/// The key OpenCode Zen reads as "no account": the literal string `public`.
///
/// Zen's gateway maps this to an anonymous, IP-rate-limited caller and serves it
/// every model flagged for anonymous use — which is exactly its zero-cost ones
/// ([`hrdr_llm::catalog::is_free_model`]); a priced model answers 401 "Missing
/// API key". opencode's own client does the same thing (`if (!hasKey)
/// provider.request.body.apiKey = "public"`, then it disables the priced rows),
/// and this is hrdr's half of that arrangement.
const ZEN_PUBLIC_API_KEY: &str = "public";

/// The anonymous key `provider` accepts in place of a credential, if it accepts
/// one. `Some` only for the BUILT-IN `zen` preset: a user-defined
/// `[providers.zen]` shadow (kind `Custom`) points somewhere else entirely and
/// must never be handed a key it did not ask for, and `go` — which shares Zen's
/// account — has no anonymous tier.
pub fn public_api_key(provider: &str, p: &ResolvedProvider) -> Option<&'static str> {
    (p.kind == ResolvedProviderKind::BuiltIn && ProviderName::new(provider).as_str() == "zen")
        .then_some(ZEN_PUBLIC_API_KEY)
}

/// [`resolve_api_key`], falling back to the provider's anonymous key
/// ([`public_api_key`]) when no real credential resolves.
///
/// This is what goes ON THE WIRE; `resolve_api_key` stays "a credential this user
/// holds", which is what [`provider_auth_state`] must ask about (otherwise every
/// logged-out Zen session would report itself as authenticated and the picker
/// would offer it models it cannot run).
pub fn resolve_api_key_or_public(
    provider: &str,
    p: &ResolvedProvider,
    parent_key: Option<&str>,
    parent_base_url: Option<&str>,
) -> Option<String> {
    resolve_api_key(provider, p, parent_key, parent_base_url)
        .or_else(|| public_api_key(provider, p).map(str::to_string))
}

/// The environment variable the effective API key is drawn FROM, if any — i.e.
/// no inline preset key shadows it and `key_env`'s variable is set in the
/// environment. `None` when the key is inline, from the `/login` store, a
/// parent, or absent. Mirrors [`resolve_api_key`]'s precedence (inline beats
/// env beats store), so a `Some` here means the key hrdr will actually use came
/// from the environment — worth surfacing so a stray `OPENAI_API_KEY` silently
/// overriding a `/login` credential is visible rather than mysterious.
pub fn api_key_env_source(p: &ResolvedProvider) -> Option<String> {
    if p.api_key.is_some() {
        return None; // an inline key wins over the environment
    }
    let var = p.key_env.as_ref()?;
    std::env::var(var).ok().map(|_| var.clone())
}

/// Whether `(kind, name)` may authenticate via the OpenAI OAuth (Codex) store —
/// the ONLY providers allowed to report [`ProviderAuthState::OAuth`] or receive
/// the auth-derived Codex endpoint switch:
///
/// * a resolved [`ResolvedProviderKind::ChatGptOAuth`] (already the trusted kind,
///   e.g. after [`oauth_derived`](crate::oauth_derived) fired), or
/// * the built-in `openai` provider — [`ResolvedProviderKind::BuiltIn`] whose
///   canonical name is `openai` — BEFORE the switch has run.
///
/// A user-defined `[providers.*]` entry (kind `Custom`), however it is spelled
/// (`openai`, `chatgpt`, `codex`), is excluded: it can never read the account's
/// OAuth credential.
pub fn is_openai_oauth_capable(kind: ResolvedProviderKind, name: &str) -> bool {
    kind == ResolvedProviderKind::ChatGptOAuth
        || (kind == ResolvedProviderKind::BuiltIn && ProviderName::new(name).as_str() == "openai")
}

/// Unified readiness for a resolved provider: how it authenticates, or that it
/// is unconfigured. Precedence, matching the existing key resolution:
///
/// 1. an API key ([`resolve_api_key`]) → [`ProviderAuthState::Key`];
/// 2. an OpenAI OAuth credential on an OAuth-capable provider
///    ([`is_openai_oauth_capable`] + [`has_oauth_credentials`]) →
///    [`ProviderAuthState::OAuth`];
/// 3. a keyless local endpoint (`remote = false`) → [`ProviderAuthState::Keyless`];
/// 4. otherwise → [`ProviderAuthState::Missing`].
///
/// A `key` beats `oauth`: a resolvable API key wins even if an OAuth credential
/// is also stored (the `/login` flow keeps them mutually exclusive). OAuth is
/// gated on [`is_openai_oauth_capable`], so a custom provider spelled `openai`
/// (kind `Custom`) can never report `OAuth`.
pub fn provider_auth_state(
    name: &str,
    resolved: &ResolvedProvider,
    parent_key: Option<&str>,
    parent_base_url: Option<&str>,
) -> ProviderAuthState {
    // The OAuth store is only consulted for an OAuth-capable provider; passing
    // the real store result into the pure core keeps the core deterministically
    // testable (no HOME dependency). The credential lives in the fixed `openai`
    // slot, so `has_oauth_credentials` is asked with the trusted kind.
    let oauth_ready = is_openai_oauth_capable(resolved.kind, name)
        && has_oauth_credentials(ResolvedProviderKind::ChatGptOAuth, name);
    provider_auth_state_with(name, resolved, parent_key, parent_base_url, oauth_ready)
}

/// Pure core of [`provider_auth_state`]: `oauth_ready` is the caller-supplied
/// OpenAI-OAuth readiness bit (see [`has_oauth_credentials`]). Only honored when
/// [`is_openai_oauth_capable`], so a custom shadow can never report `OAuth` even
/// if a caller passed `true`.
pub(crate) fn provider_auth_state_with(
    name: &str,
    resolved: &ResolvedProvider,
    parent_key: Option<&str>,
    parent_base_url: Option<&str>,
    oauth_ready: bool,
) -> ProviderAuthState {
    if resolve_api_key(name, resolved, parent_key, parent_base_url).is_some() {
        return ProviderAuthState::Key;
    }
    if is_openai_oauth_capable(resolved.kind, name) && oauth_ready {
        return ProviderAuthState::OAuth;
    }
    // Below a real credential, above "unconfigured": a provider that serves part
    // of its catalog to anonymous callers is *usable*, and saying `Missing` here
    // is what hid free Zen from a user who had not logged in to anything.
    if public_api_key(name, resolved).is_some() {
        return ProviderAuthState::Anonymous;
    }
    if !resolved.remote {
        return ProviderAuthState::Keyless;
    }
    ProviderAuthState::Missing
}

/// Whether `name` is one of the known ChatGPT-provider spellings (`chatgpt`,
/// `codex`, `openai-oauth`).
pub fn is_chatgpt_provider_name(name: &str) -> bool {
    CHATGPT_PROVIDER_ALIASES
        .iter()
        .any(|a| a.eq_ignore_ascii_case(name))
}

/// Resolve a built-in provider name (no config file) to its endpoint and env key.
///
/// `openai` (and its OAuth spellings, which fold onto it) resolves to the
/// STANDARD OpenAI endpoint: `api.openai.com` + `OPENAI_API_KEY` +
/// [`ResolvedProviderKind::BuiltIn`]. The Codex/OAuth endpoint and
/// [`ResolvedProviderKind::ChatGptOAuth`] kind are NOT a static preset any more —
/// they are produced by the auth-derived switch
/// [`oauth_derived`](crate::oauth_derived), which fires only when the built-in
/// `openai` has no resolvable API key but a stored OpenAI OAuth credential.
pub fn builtin_provider(name: &str) -> Option<ResolvedProvider> {
    let (base_url, key_env, remote) = match name.trim().to_ascii_lowercase().as_str() {
        "zen" | "opencode" | "opencode-zen" => {
            ("https://opencode.ai/zen/v1", "OPENCODE_API_KEY", true)
        }
        "go" | "opencode-go" => ("https://opencode.ai/zen/go/v1", "OPENCODE_API_KEY", true),
        "openai" | "chatgpt" | "codex" | "openai-oauth" => {
            ("https://api.openai.com/v1", "OPENAI_API_KEY", true)
        }
        "openrouter" => ("https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", true),
        // Anthropic's own host → hrdr uses the native Messages API (`x-api-key`),
        // which unlocks prompt caching (the OpenAI-compat endpoint can't cache).
        "claude" | "anthropic" => ("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY", true),
        "deepseek" => ("https://api.deepseek.com", "DEEPSEEK_API_KEY", true),
        "local" | "infr" => ("http://localhost:8080/v1", "HRDR_API_KEY", false),
        _ => return None,
    };
    Some(ResolvedProvider {
        base_url: base_url.to_string(),
        key_env: Some(key_env.to_string()),
        api_key: None,
        model: None,
        remote,
        context_window: None,
        headers: HashMap::new(),
        api_version: None,
        kind: ResolvedProviderKind::BuiltIn,
    })
}

/// The model spec `$HRDR_MODEL` names, if it names one.
pub fn env_model_spec() -> Option<ModelSpec> {
    std::env::var("HRDR_MODEL").ok()?.parse().ok()
}

/// What config.toml and the environment NAMED for the model identity — ONE key
/// each, as [`ModelSpec`]s, **lowest precedence first** (config.toml, then
/// `$HRDR_MODEL`), for the ONE caller that needs them: the CLI startup edge, which
/// applies them in order onto the identity in effect and layers its own `--model`
/// on top (see the identity edge in `main.rs`).
///
/// They are a list, not one value, because they *compose*: a config naming
/// `openrouter://deepseek-chat` and a `$HRDR_MODEL=kimi-k2` mean "kimi-k2, on
/// openrouter" — a bare id rides on whatever provider is in effect, and only a
/// `provider://model` moves the provider.
///
/// [`AgentConfig::load`] has already applied both onto [`AgentConfig::model`]
/// (against the `local` default); this says what was actually *named*, which is what
/// the startup precedence turns on.
pub fn named_model_specs() -> Vec<ModelSpec> {
    read_config_file::<FileConfig>()
        .and_then(|fc| fc.model)
        .into_iter()
        .chain(env_model_spec())
        .collect()
}

/// [`provider_alias_collision_error`] against the user's real config file.
/// `Ok(())` when there is no config file, or it says one thing once: no provider
/// named twice under two spellings.
///
/// There is deliberately no check for *dead* config shapes here. hrdr is pre-1.0
/// and carries no back-compat: a key that no longer exists is refused by
/// `FileConfig`'s `deny_unknown_fields` like any other unknown key, and needs no
/// bespoke message explaining what it used to mean.
pub fn check_config_compat() -> Result<()> {
    let Some(path) = config_file_path() else {
        return Ok(());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    match provider_alias_collision_error(&text, &path) {
        Some(msg) => bail!(msg),
        None => Ok(()),
    }
}

/// `hrdr`'s config directory — `$XDG_CONFIG_HOME/hrdr`, default
/// `~/.config/hrdr` (cross-platform via home-dir detection). Shared by
/// `config.toml` loading and the global `AGENTS.md` lookup so the two can't
/// diverge.
pub fn config_dir() -> Option<std::path::PathBuf> {
    hjkl_xdg::config_dir("hrdr").ok()
}

/// Path to the user config file (`~/.config/hrdr/config.toml`), if `HOME` is set.
pub fn config_file_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

/// Read the config TOML file and deserialize it into `T`. Returns `None` when
/// the file is missing or unreadable.
/// Every hard problem in the config file at `path` — a parse failure (which
/// includes a key no layer of hrdr knows, since the schema denies unknown
/// fields) and every out-of-range value. Empty when the file is absent, or
/// wholly valid.
///
/// [`AgentConfig::load_diagnosed`] is the same check against the file at the
/// default location; this one takes a path, for a caller validating a file it
/// names rather than the user's own. Env layering, and the cross-field checks
/// that need the merged result, are deliberately not run here — this answers
/// "would hrdr refuse THIS file?", nothing more.
pub fn config_file_errors(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match toml::from_str::<FileConfig>(&text) {
        Ok(fc) => fc.validate(),
        Err(e) => vec![format!(
            "{}: could not parse config file: {e}",
            path.display()
        )],
    }
}

pub fn read_config_file<T: serde::de::DeserializeOwned>() -> Option<T> {
    let path = config_file_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&text).ok()
}

// ── Config loading (file + env) ─────────────────────────────────────────────

impl AgentConfig {
    /// Load config with precedence: env > `~/.config/hrdr/config.toml` > built-in
    /// defaults. Lenient: any invalid value (a malformed file, an out-of-range
    /// number) is dropped and its default kept — see [`load_diagnosed`] to also
    /// receive the diagnostics. Does NOT auto-write a config file when missing.
    ///
    /// [`load_diagnosed`]: Self::load_diagnosed
    pub fn load() -> Self {
        Self::load_diagnosed().0
    }

    /// Like [`load`](Self::load) but returns an error listing every hard problem
    /// (a config-file value out of range, a malformed file, an incompatible
    /// pair) — accumulated into one message — so a caller that should refuse to
    /// run can. `Ok` when the file is absent or wholly valid. Env-var warnings
    /// are *not* errors; reach for [`load_diagnosed`](Self::load_diagnosed) to
    /// see those.
    pub fn load_checked() -> Result<Self> {
        let (cfg, diags) = Self::load_diagnosed();
        match diags.error_message() {
            Some(msg) => bail!(msg),
            None => Ok(cfg),
        }
    }

    /// The one loader [`load`](Self::load) and [`load_checked`](Self::load_checked)
    /// delegate to: merge defaults ← file ← env and return the config alongside
    /// every problem found. Infallible — the config is always the best-effort
    /// merge (a rejected value keeps its default) — leaving the caller to decide
    /// what a hard [error](ConfigDiagnostics::errors) (refuse to start) versus a
    /// [warning](ConfigDiagnostics::warnings) (report and continue) should do.
    pub fn load_diagnosed() -> (Self, ConfigDiagnostics) {
        let mut cfg = Self::default();
        let mut diags = ConfigDiagnostics::default();
        // Read + parse the file directly (not via `read_config_file`, which folds a
        // parse error into "no file"): a malformed config is a diagnostic, not
        // silence. A missing/unreadable file is genuinely absent — no error.
        let file_spec = match config_file_path()
            .and_then(|p| std::fs::read_to_string(&p).ok().map(|text| (p, text)))
        {
            Some((path, text)) => match toml::from_str::<FileConfig>(&text) {
                Ok(fc) => {
                    // File values are hard errors (see the module docs): reject
                    // out-of-range ones by field, but still apply the rest so the
                    // report is complete rather than first-error-wins.
                    diags.errors.extend(fc.validate());
                    let spec = fc.model.clone();
                    cfg.apply_file(fc);
                    spec
                }
                Err(e) => {
                    diags.errors.push(format!(
                        "{}: could not parse config file: {e}",
                        path.display()
                    ));
                    None
                }
            },
            None => None,
        };
        // Env overrides are warnings: an invalid one is reported and the current
        // value kept.
        diags.warnings.extend(cfg.apply_env());
        // ONE key, layered by precedence: `$HRDR_MODEL` over config.toml's `model`.
        // A `provider://model` names the whole identity; a bare id is that model on
        // the provider in effect — which, here, is whatever the layer below settled
        // (the `local` default, until the CLI edge folds in the last-used identity).
        for spec in [file_spec, env_model_spec()].into_iter().flatten() {
            // A `provider://` here is the INTERACTIVE chain's business (the model you
            // last used on that provider, else its declared one). The CLI edge settles
            // it against the last-used store; a config that cannot be answered at all
            // simply leaves the identity as it was, and the CLI reports it.
            if let Some(reference) = spec
                .apply(&cfg.model)
                .or_else(|| model_for_provider(spec.provider()?, &cfg).ok())
            {
                cfg.model = reference;
            }
        }
        // Cross-field checks run on the merged result (both halves may come from
        // the file), so they join the hard errors.
        diags.errors.extend(cfg.validate_semantics());
        (cfg, diags)
    }

    /// Layer file values over the current config. The identity's one key
    /// (`model = "provider://model"`) is picked up by
    /// [`load_checked`](Self::load_checked) and applied there, since it layers
    /// against the environment.
    pub(crate) fn apply_file(&mut self, fc: FileConfig) {
        if let Some(v) = fc.api_key {
            self.api_key = Some(v);
        }
        if let Some(v) = fc.temperature {
            self.temperature = Some(v);
        }
        if let Some(v) = fc.context_window {
            self.context_window = Some(v);
        }
        if let Some(v) = fc.max_tokens {
            self.max_tokens = Some(v);
        }
        if let Some(v) = fc.top_p {
            self.top_p = Some(v);
        }
        if let Some(v) = fc.seed {
            self.seed = Some(v);
        }
        if !fc.stop.is_empty() {
            self.stop = fc.stop;
        }
        if let Some(v) = fc.stream_usage {
            self.stream_usage = v;
        }
        if let Some(v) = fc.request_timeout {
            self.request_timeout = Some(v);
        }
        if let Some(v) = fc.session_compress_after {
            self.session_compress_after = Some(v);
        }
        if let Some(v) = fc.session_purge_after {
            self.session_purge_after = Some(v);
        }
        if let Some(v) = fc.prompt_cache_ttl {
            self.prompt_cache_ttl = Some(v);
        }
        if let Some(v) = fc.max_cost {
            self.max_cost = Some(v);
        }
        if let Some(v) = fc.allow_unpriced {
            self.allow_unpriced = v;
        }
        if let Some(v) = fc.subagents {
            self.subagents = v;
        }
        if let Some(v) = fc.memory {
            self.memory = v;
        }
        if let Some(v) = fc.memory_dir {
            self.memory_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = fc.subagent_model {
            self.subagent_model = Some(v);
        }
        if !fc.subagent.is_empty() {
            self.subagent_profiles = fc.subagent;
        }
        if let Some(v) = fc.effort {
            self.effort = Some(v);
        }
        if let Some(v) = fc.auto_compact {
            self.auto_compact = v;
        }
        if let Some(v) = fc.compaction_reserved {
            self.compaction_reserved = v;
        }
        if let Some(v) = fc.max_readonly_subagents {
            self.max_readonly_subagents = v;
        }
        if let Some(v) = fc.max_write_subagents {
            self.max_write_subagents = v;
        }
        if let Some(v) = fc.max_attachment_bytes {
            self.max_attachment_bytes = Some(v);
        }
        if let Some(v) = fc.sandbox {
            self.sandbox = v;
        }
        if let Some(wrap) = fc.wrap_tool_results {
            self.wrap_tool_results = wrap;
        }
        if !fc.sandbox_writable_roots.is_empty() {
            self.sandbox_writable_roots = fc
                .sandbox_writable_roots
                .into_iter()
                .map(PathBuf::from)
                .collect();
        }
        if !fc.providers.is_empty() {
            // Rekey by the canonical name: `[providers.anthropic]` IS `claude`'s
            // table, and every lookup arrives already folded.
            self.providers = canonical_providers(fc.providers);
        }
        if !fc.guardrails.is_empty() {
            self.guardrails = fc.guardrails;
        }
        if !fc.hooks.is_empty() {
            self.hooks = fc.hooks;
        }
        if let Some(to) = fc.tool_output {
            if let Some(v) = to.max_lines {
                self.tool_max_lines = v;
            }
            if let Some(v) = to.max_bytes {
                self.tool_max_bytes = v;
            }
        }
        if let Some(v) = fc.compaction_tail_turns {
            self.compaction_tail_turns = v;
        }
        if let Some(v) = fc.preserve_recent_tokens {
            self.preserve_recent_tokens = v;
        }
        if !fc.mcp.is_empty() {
            self.mcp = fc.mcp;
        }
        if let Some(v) = fc.prompt_cache {
            self.prompt_cache = Some(v);
        }
        if let Some(l) = fc.lsp {
            if let Some(e) = l.enabled {
                self.lsp = e;
            }
            if l.wait_secs.is_some() {
                self.lsp_wait_secs = l.wait_secs;
            }
            if !l.servers.is_empty() {
                self.lsp_servers = l.servers;
            }
        }
    }

    /// Layer environment variables over the current config, returning a warning
    /// for each `HRDR_*` value that could not be applied (unparseable or out of
    /// range) — the current value is kept in that case (see the module docs:
    /// env tweaks are warnings, not hard errors). Every knob is one row in
    /// [`ENV_SETTERS`]; adding a new env var means adding a row there, not
    /// another `if let` here. `HRDR_API_KEY` is special-cased (it has a fallback
    /// var) below.
    pub(crate) fn apply_env(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, set) in ENV_SETTERS {
            if let Ok(v) = std::env::var(name)
                && let Err(reason) = set(self, &v)
            {
                warnings.push(env_warning(name, &v, &reason));
            }
        }
        // HRDR_API_KEY always wins. OPENAI_API_KEY — commonly exported for
        // unrelated tools — is only a last-resort fallback when nothing else
        // set a key; it must not override a config-file key destined for a
        // non-OpenAI endpoint.
        if let Ok(k) = std::env::var("HRDR_API_KEY") {
            self.api_key = Some(k);
        } else if self.api_key.is_none()
            && let Ok(k) = std::env::var("OPENAI_API_KEY")
        {
            self.api_key = Some(k);
        }
        warnings
    }
}

// ── Sandbox mode derivation ─────────────────────────────────────────────────

/// The mode this agent actually runs under: the session default
/// ([`AgentConfig::sandbox`]), floored/capped by what the agent *is*.
///
/// Called ONCE, in `Agent::new`, for every agent — main or sub, since both come
/// through the same constructor and a sub-agent's config is a clone of the base
/// with `read_only` already final. There is deliberately no delegation-side
/// branch: a revived sub-agent, a `--agent explore` main agent, and a write
/// sub-agent all derive here, from the same two inputs.
///
/// - Session `none` (aka `yolo`) is an explicit global opt-out and wins
///   everywhere.
/// - A read-only agent takes `read`: broad reads, no writable root anywhere.
///   That is what makes it read-only now that it has a shell — the tool set no
///   longer does it.
/// - …unless the session asked for `jail`, which adds confined reads AND a
///   read-only tool set with no shell. Only a read-only agent can honour it; see
///   the next rule.
/// - A write-capable agent must be able to write, so it floors at `write` even
///   when the session default is `read` or `jail` — its writable roots are what
///   confine it. A `jail` session therefore means "jail for the agents that can
///   be, `write` for the ones that must write", not "nothing can run".
///
///   That floor is a real footgun for `jail` specifically, since somebody typing
///   it is asking to be contained and would otherwise be handed a full-write
///   session without being told. `Agent::new` emits a notice when it bites, and
///   points at the `prisoner` agent — whose own declared mode is absolute and
///   therefore never floored.
pub fn effective_sandbox(session: SandboxMode, read_only: bool) -> SandboxMode {
    match (session, read_only) {
        (SandboxMode::None, _) => SandboxMode::None,
        (SandboxMode::Jail, true) => SandboxMode::Jail,
        (_, true) => SandboxMode::Read,
        (_, false) => SandboxMode::Write,
    }
}

// ── Env-var parsing ─────────────────────────────────────────────────────────

/// Parse a boolean setting from a config/CLI/env string, accepting the common
/// spellings (`1`/`0`, `true`/`false`, `on`/`off`, `yes`/`no`,
/// case-insensitive). Returns `None` for anything unrecognized so callers can
/// leave the current value unchanged.
pub fn parse_env_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

/// Parse a toggle that historically accepted a fraction (`auto_compact = 0.85`)
/// and now reads as a bool: the standard bool spellings, or any number where
/// `> 0` means enabled. Returns `None` for anything unrecognized.
pub fn parse_toggle_or_num(v: &str) -> Option<bool> {
    parse_env_bool(v).or_else(|| v.trim().parse::<f64>().ok().map(|n| n > 0.0))
}

/// Deserialize a config toggle that may be spelled as a bool (`true`) or, for
/// backward compatibility, as the old fractional number (`0.85` → `true`,
/// `0` → `false`). Used for `auto_compact`.
pub(crate) fn de_bool_or_num<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum BoolOrNum {
        Bool(bool),
        Num(f64),
    }
    Ok(Option::<BoolOrNum>::deserialize(d)?.map(|v| match v {
        BoolOrNum::Bool(b) => b,
        BoolOrNum::Num(n) => n > 0.0,
    }))
}

/// The standard warning line for an env var that could not be applied: names
/// the var, echoes the value, gives the reason, and states the current value was
/// kept. One owner so every env diagnostic reads the same.
fn env_warning(name: &str, value: &str, reason: &str) -> String {
    format!("${name} = \"{value}\": {reason}; keeping the current value")
}

/// Parse an env value into `T`, mapping a parse failure to a warning reason that
/// names what was expected.
fn env_parse<T: std::str::FromStr>(v: &str, expected: &str) -> Result<T, String> {
    v.trim()
        .parse::<T>()
        .map_err(|_| format!("expected {expected}"))
}

/// Parse a boolean env value, mapping an unrecognized spelling to a warning
/// reason listing the accepted forms.
fn env_bool(v: &str) -> Result<bool, String> {
    parse_env_bool(v)
        .ok_or_else(|| "expected a boolean (true/false, on/off, yes/no, 1/0)".to_string())
}

/// Applies an env var's string value to the config, returning `Err(reason)` when
/// the value was invalid so [`AgentConfig::apply_env`] can warn and keep the
/// current value.
pub(crate) type EnvSetter = fn(&mut AgentConfig, &str) -> Result<(), String>;

/// Env var → setter table used by [`AgentConfig::apply_env`]. Adding a knob is a
/// single row here (non-capturing closures coerce to `fn` pointers). A setter
/// returns `Err(reason)` for an unparseable or out-of-range value; the caller
/// keeps the current value and reports the reason as a warning.
///
/// `$HRDR_MODEL` is deliberately NOT here: it names the one identity, as a
/// [`ModelSpec`] layered against config.toml's `model` (see
/// [`AgentConfig::load_checked`]), rather than a field assigned in isolation.
///
/// Nor is `$HRDR_BASE_URL` — there is no such var. [`AgentConfig::base_url`] is
/// DERIVED from the identity's provider, and an environment variable that could
/// move it would be an endpoint that belongs to nobody.
pub(crate) const ENV_SETTERS: &[(&str, EnvSetter)] = &[
    ("HRDR_AUTO_COMPACT", |c, v| {
        c.auto_compact = parse_toggle_or_num(v)
            .ok_or_else(|| "expected a boolean (or a number, > 0 = on)".to_string())?;
        Ok(())
    }),
    ("HRDR_MAX_READONLY_SUBAGENTS", |c, v| {
        let n: usize = env_parse(v, "a whole number ≥ 1")?;
        if n == 0 {
            return Err(
                "must be at least 1 (0 would refuse every read-only sub-agent)".to_string(),
            );
        }
        c.max_readonly_subagents = n;
        Ok(())
    }),
    ("HRDR_MAX_WRITE_SUBAGENTS", |c, v| {
        let n: usize = env_parse(v, "a whole number ≥ 1")?;
        if n == 0 {
            return Err(
                "must be at least 1 (0 would refuse every write-capable sub-agent)".to_string(),
            );
        }
        c.max_write_subagents = n;
        Ok(())
    }),
    ("HRDR_MAX_ATTACHMENT_BYTES", |c, v| {
        let n: usize = env_parse(v, "a whole number of base64 bytes ≥ 1")?;
        if n == 0 {
            return Err("must be at least 1 (0 would refuse every attachment)".to_string());
        }
        c.max_attachment_bytes = Some(n);
        Ok(())
    }),
    ("HRDR_COMPACTION_RESERVED", |c, v| {
        c.compaction_reserved = env_parse(v, "a whole number of tokens")?;
        Ok(())
    }),
    ("HRDR_SANDBOX", |c, v| {
        // `SandboxMode::from_str`'s error is already the warning reason.
        c.sandbox = v.parse()?;
        Ok(())
    }),
    ("HRDR_LSP", |c, v| {
        c.lsp = env_bool(v)?;
        Ok(())
    }),
    ("HRDR_PROMPT_CACHE", |c, v| {
        c.prompt_cache = Some(v.to_string());
        Ok(())
    }),
    ("HRDR_MAX_TOKENS", |c, v| {
        let n: u32 = env_parse(v, "a whole number ≥ 1")?;
        if n == 0 {
            return Err("must be at least 1 (0 would allow no output)".to_string());
        }
        c.max_tokens = Some(n);
        Ok(())
    }),
    ("HRDR_TOP_P", |c, v| {
        c.top_p = Some(env_parse(v, "a number")?);
        Ok(())
    }),
    ("HRDR_SEED", |c, v| {
        c.seed = Some(env_parse(v, "a whole number")?);
        Ok(())
    }),
    ("HRDR_STREAM_USAGE", |c, v| {
        c.stream_usage = env_bool(v)?;
        Ok(())
    }),
    ("HRDR_RETRY_ATTEMPTS", |c, v| {
        // The one part of the retry policy worth exposing: how long a failing
        // provider is worth waiting for is a property of the operator's
        // patience, not of the code. The *schedule* stays fixed (5s doubling to
        // 60s) — a tunable backoff is how you get a retry storm.
        let n: usize = env_parse(v, "a whole number ≥ 1 (1 disables retrying)")?;
        if n == 0 {
            return Err("must be at least 1 (the first attempt is not a retry)".to_string());
        }
        c.retry.max_attempts = n;
        Ok(())
    }),
    ("HRDR_REQUEST_TIMEOUT", |c, v| {
        // 0 is the documented "disable the timeout" sentinel — accepted.
        c.request_timeout = Some(env_parse(v, "a whole number of seconds (0 disables)")?);
        Ok(())
    }),
    ("HRDR_SESSION_COMPRESS_AFTER", |c, v| {
        c.session_compress_after = Some(env_parse(v, "a whole number of seconds (0 disables)")?);
        Ok(())
    }),
    ("HRDR_SESSION_PURGE_AFTER", |c, v| {
        c.session_purge_after = Some(env_parse(v, "a whole number of seconds (0 disables)")?);
        Ok(())
    }),
    ("HRDR_PROMPT_CACHE_TTL", |c, v| {
        c.prompt_cache_ttl = Some(v.to_string());
        Ok(())
    }),
    ("HRDR_SUBAGENT_MODEL", |c, v| {
        c.subagent_model = Some(env_parse(v, "a provider://model or bare model id")?);
        Ok(())
    }),
    ("HRDR_SUBAGENTS", |c, v| {
        c.subagents = env_bool(v)?;
        Ok(())
    }),
    ("HRDR_MEMORY", |c, v| {
        c.memory = env_bool(v)?;
        Ok(())
    }),
    ("HRDR_MEMORY_DIR", |c, v| {
        if !v.trim().is_empty() {
            c.memory_dir = Some(PathBuf::from(v));
        }
        Ok(())
    }),
];

// ── Cache-mode / endpoint helpers ───────────────────────────────────────────

/// Resolve the prompt-cache `setting` (`off`/`on`/`ephemeral`/`auto`; `None` =
/// `auto`) against the endpoint into a concrete [`CacheMode`].
///
/// `auto` enables `cache_control` breakpoints **only for endpoints that consume
/// them safely**:
/// - **OpenRouter**, which normalizes the marker and strips it for models that
///   don't accept it.
/// - **Anthropic's own host** (`api.anthropic.com`), which hrdr talks to over the
///   **native Messages API** — where `cache_control` is the real caching lever.
///
/// It stays **off** for every other endpoint because sending an unknown
/// `cache_control` field is not universally safe:
/// - **OpenAI, Groq, xAI** reject it with a `400` (strict field validation), so
///   a blanket default would break every request.
/// - **DeepSeek, Gemini, OpenAI, Groq, xAI** already cache automatically, so the
///   marker buys nothing.
/// - local servers may reject the content-parts form.
///
/// Force it anywhere with `prompt_cache = "on"` (the caller's responsibility to
/// know the endpoint accepts it).
pub fn resolve_cache_mode(setting: Option<&str>, base_url: &str) -> CacheMode {
    use hrdr_llm::CacheMode;
    match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off") | Some("none") | Some("false") | Some("no") => CacheMode::Off,
        Some("on") | Some("ephemeral") | Some("true") | Some("yes") => CacheMode::Ephemeral,
        _ if is_openrouter(base_url) || is_anthropic_native(base_url) => CacheMode::Ephemeral,
        _ => CacheMode::Off,
    }
}

/// Whether `base_url` is Anthropic's own host — hrdr speaks the native Messages
/// API there, so `cache_control` breakpoints actually cache (unlike the
/// OpenAI-compat endpoint, which drops them). The host-matching itself belongs to
/// hrdr-llm, which decides the wire protocol; this only names what that decision
/// means for caching.
pub(crate) fn is_anthropic_native(base_url: &str) -> bool {
    is_anthropic_backend(base_url)
}

/// Whether `base_url` points at a server on this machine (or an explicitly
/// keyless one): a local `llama-server` / `infr serve` / vLLM needs no
/// credential, so having none there is normal and a probe is worth making.
///
/// A *remote* endpoint with no credential is the opposite: the request is
/// guaranteed to 401, and the 401 says nothing about whether the endpoint is up.
///
/// The host-matching itself belongs to hrdr-llm, which also uses it to decide
/// request shape (an OpenAI routing hint means nothing to a local server); this
/// only names what that decision means for credentials.
pub fn is_local_endpoint(base_url: &str) -> bool {
    hrdr_llm::is_local_host(base_url)
}

/// Whether `base_url` points at OpenRouter — the one endpoint hrdr enables
/// `cache_control` for in `auto` mode (it accepts the marker for the models that
/// benefit and strips it for the rest). Also matches a custom provider pointed
/// at OpenRouter.
pub(crate) fn is_openrouter(base_url: &str) -> bool {
    let host = url_host(base_url);
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

// ── Config file persistence ─────────────────────────────────────────────────

/// Set `key = value` in the user config file (creating it if needed), preserving
/// existing keys, ordering, and comments. Returns the file path.
///
/// Errors (and changes nothing on disk) when the existing file is not valid
/// TOML — see [`persist_setting_at`].
pub fn persist_setting(key: &str, value: ConfigValue) -> Result<std::path::PathBuf> {
    let path =
        config_file_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the config file"))?;
    persist_setting_at(&path, key, value)?;
    Ok(path)
}

/// Remove `key` from the user config file (if present). Returns the file path.
///
/// Errors (and changes nothing on disk) when the existing file is not valid
/// TOML — see [`remove_setting_at`].
pub fn remove_setting(key: &str) -> Result<std::path::PathBuf> {
    let path =
        config_file_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the config file"))?;
    remove_setting_at(&path, key)?;
    Ok(path)
}

/// [`persist_setting`] against an explicit path — the whole read-modify-write,
/// run under the cross-process config lock.
///
/// The lock is taken *before* the read and held past the rename, so a second
/// process re-reads the file the first one wrote instead of overwriting a stale
/// snapshot (the lost-update this used to have). Contention is bounded: see
/// [`StoreLock::acquire`](crate::store_lock::StoreLock::acquire), which retries
/// briefly and then errors rather than blocking forever.
pub(crate) fn persist_setting_at(
    path: &std::path::Path,
    key: &str,
    value: ConfigValue,
) -> Result<()> {
    let _lock = lock_config(path)?;
    let mut doc = read_config_doc(path)?;
    match value {
        ConfigValue::Str(s) => doc[key] = toml_edit::value(s),
        ConfigValue::Bool(b) => doc[key] = toml_edit::value(b),
        ConfigValue::Float(f) => doc[key] = toml_edit::value(f),
        ConfigValue::Int(i) => doc[key] = toml_edit::value(i),
    }
    write_config_doc(path, &doc)
}

/// [`remove_setting`] against an explicit path, under the same lock and with the
/// same malformed-file refusal as [`persist_setting_at`].
pub(crate) fn remove_setting_at(path: &std::path::Path, key: &str) -> Result<()> {
    let _lock = lock_config(path)?;
    let mut doc = read_config_doc(path)?;
    doc.remove(key);
    write_config_doc(path, &doc)
}

/// Take the cross-process write lock for the config file, creating its parent
/// directory first (the lock is a sibling file, so the directory must exist).
fn lock_config(path: &std::path::Path) -> Result<crate::store_lock::StoreLock> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::store_lock::StoreLock::acquire(path, crate::store_lock::StoreKind::SmallFileRewrite)
}

/// Parse the config file into an editable document, preserving comments and
/// ordering.
///
/// A missing file yields an empty document — that is how the first
/// `/theme`-style command creates one. Anything else is an **error**, never a
/// silently-empty document: a file we could not read or could not parse may
/// still hold every setting the user wrote, and returning a default here is what
/// let the next write erase it. The malformed file is copied to a `.bak` sibling
/// as a safety net, but the original is left exactly as it was and the caller is
/// told to fix it.
pub(crate) fn read_config_doc(path: &std::path::Path) -> Result<toml_edit::DocumentMut> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(toml_edit::DocumentMut::default());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading config file {}", path.display()));
        }
    };
    match content.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => Ok(doc),
        Err(e) => {
            // Keep a copy so a hand-mangled file is still recoverable, then
            // refuse: overwriting it with a default document would drop every
            // setting it holds.
            let backup = hrdr_llm::sibling_with_suffix(path, ".bak");
            let saved = std::fs::copy(path, &backup).is_ok();
            let note = if saved {
                format!(" (a copy was saved to {})", backup.display())
            } else {
                String::new()
            };
            Err(anyhow::anyhow!(
                "config file {} is not valid TOML: {e}{note} — refusing to overwrite it; \
                 fix or remove the file and retry",
                path.display()
            ))
        }
    }
}

/// The `.bak` sibling a malformed config is copied to before we refuse to write.
/// Write `doc` over `path` atomically: build it in a unique sibling temp file,
/// then rename that onto the target.
///
/// The temp is created owner-only from the start ([`hrdr_llm::owner_only_options`],
/// `0600` on unix), like every other hrdr-owned store: `config.toml` can hold an
/// inline `api_key` (a `[providers.*]` entry or the legacy top-level key), so a
/// plain umask-default write renamed over the target would silently widen a
/// user's `chmod 600` back to `0644`.
///
/// The temp name comes from [`hrdr_llm::unique_sibling_path`] (PID + a
/// process-wide counter), so two writers racing on the same config never build
/// their new contents in the same scratch file — a fixed `.tmp` name let one
/// writer's half-written bytes get renamed into place by the other.
pub(crate) fn write_config_doc(path: &std::path::Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    hrdr_llm::write_atomic(path, doc.to_string().as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Shorthand: a config with no `[providers.*]` entries — every provider name
/// resolves to a built-in.
#[cfg(test)]
pub(crate) fn cfg() -> AgentConfig {
    AgentConfig::default()
}

/// Shorthand: a config carrying one `[providers.<name>]` entry.
#[cfg(test)]
pub(crate) fn cfg_with(name: &str, p: ProviderConfig) -> AgentConfig {
    let mut c = AgentConfig::default();
    c.providers.insert(name.to_string(), p);
    c
}

/// Shorthand: a `[providers.*]` entry that defines nothing but its endpoint.
#[cfg(test)]
pub(crate) fn provider_config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: base_url.to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod cache_mode_tests {
    use super::*;

    /// The `auto` branch's Anthropic arm, pinned end to end. It is decided by
    /// hrdr-llm's backend detection (host-keyed), so the assertions below are the
    /// only thing that would notice if that seam stopped agreeing with what
    /// `is_anthropic_native` promises — `cache_control` breakpoints emitted only
    /// where the native Messages API will actually honour them.
    #[test]
    fn auto_enables_caching_on_the_anthropic_backend_only() {
        // Anthropic's own host, and a subdomain of it: native Messages API → on.
        assert_eq!(
            resolve_cache_mode(None, "https://api.anthropic.com/v1"),
            CacheMode::Ephemeral
        );
        assert_eq!(
            resolve_cache_mode(Some("auto"), "https://eu.anthropic.com/v1"),
            CacheMode::Ephemeral
        );
        assert!(is_anthropic_native("https://api.anthropic.com/v1"));

        // An OpenAI-compat gateway is not the Anthropic backend even when it
        // serves Claude models: the marker would be rejected or ignored → off.
        assert_eq!(
            resolve_cache_mode(None, "https://api.together.xyz/v1"),
            CacheMode::Off
        );
        assert_eq!(
            resolve_cache_mode(Some("auto"), "http://localhost:1234/v1"),
            CacheMode::Off
        );
        assert!(!is_anthropic_native("https://api.together.xyz/v1"));

        // DeepSeek caches automatically with no `cache_control` support — the
        // marker buys nothing, so the deliberate answer is `Off`: nothing sent.
        assert_eq!(
            resolve_cache_mode(None, "https://api.deepseek.com"),
            CacheMode::Off
        );

        // A lookalike host must not satisfy the `.anthropic.com` suffix check.
        assert_eq!(
            resolve_cache_mode(None, "https://notanthropic.com/v1"),
            CacheMode::Off
        );
    }
}

#[cfg(test)]
mod env_source_tests {
    use super::*;

    /// `api_key_env_source` reports the variable name only when the effective key
    /// is genuinely environment-sourced: `key_env` set to a variable that exists,
    /// and no inline key shadowing it. Uses `PATH` (always set) for the positive
    /// case and an unset name for the negative — no environment mutation, so this
    /// is hermetic and parallel-safe.
    #[test]
    fn api_key_env_source_reports_only_an_env_sourced_key() {
        let mut p = builtin_provider("openai").expect("built-in openai");

        // key_env → a set variable, no inline key: reported by its name.
        p.api_key = None;
        p.key_env = Some("PATH".to_string());
        assert_eq!(api_key_env_source(&p), Some("PATH".to_string()));

        // An inline key wins over the environment → not env-sourced.
        p.api_key = Some("sk-inline".to_string());
        assert_eq!(api_key_env_source(&p), None);

        // key_env naming an UNSET variable → not env-sourced.
        p.api_key = None;
        p.key_env = Some("HRDR_DEFINITELY_UNSET_VAR_XYZ".to_string());
        assert_eq!(api_key_env_source(&p), None);

        // No key_env at all → None.
        p.key_env = None;
        assert_eq!(api_key_env_source(&p), None);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn doc_of(path: &std::path::Path) -> toml_edit::DocumentMut {
        std::fs::read_to_string(path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap()
    }

    /// The lost-update this finding is about: two writers doing an unsynchronized
    /// read-modify-write each keep their own snapshot, and the later rename drops
    /// the earlier writer's key. Under the lock every key survives.
    #[test]
    fn concurrent_writers_do_not_lose_each_others_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "existing = \"kept\"\n").unwrap();

        let keys = ["theme", "model", "provider", "effort", "todo_ttl", "temp"];
        std::thread::scope(|s| {
            for key in keys {
                let path = path.clone();
                s.spawn(move || {
                    persist_setting_at(&path, key, ConfigValue::Str(key)).unwrap();
                });
            }
        });

        let doc = doc_of(&path);
        for key in keys {
            assert_eq!(
                doc.get(key).and_then(|v| v.as_str()),
                Some(key),
                "writer for '{key}' was lost; file:\n{}",
                std::fs::read_to_string(&path).unwrap()
            );
        }
        assert_eq!(doc["existing"].as_str(), Some("kept"));
    }

    /// A removal is serialized against a concurrent write the same way, and both
    /// effects survive.
    #[test]
    fn concurrent_write_and_remove_both_take_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"old\"\nkeep = 1\n").unwrap();

        std::thread::scope(|s| {
            let p = path.clone();
            s.spawn(move || persist_setting_at(&p, "model", ConfigValue::Str("m")).unwrap());
            let p = path.clone();
            s.spawn(move || remove_setting_at(&p, "theme").unwrap());
        });

        let doc = doc_of(&path);
        assert_eq!(doc["model"].as_str(), Some("m"));
        assert!(doc.get("theme").is_none(), "removal was lost");
        assert_eq!(doc["keep"].as_integer(), Some(1));
    }

    /// Two writers must never build their new contents in the same scratch file
    /// (the old fixed `*.toml.tmp`), and a completed write leaves none behind.
    #[test]
    fn temp_paths_are_unique_and_cleaned_up() {
        let path = std::path::Path::new("/cfg/config.toml");
        let a = hrdr_llm::unique_sibling_path(path, "hrdr-tmp");
        let b = hrdr_llm::unique_sibling_path(path, "hrdr-tmp");
        assert_ne!(a, b, "concurrent writers must not share a temp path");
        assert_eq!(a.parent(), path.parent(), "temp must be a sibling");
        assert_ne!(a, path.with_extension("toml.tmp"), "no fixed temp name");

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("config.toml");
        persist_setting_at(&real, "theme", ConfigValue::Str("dark")).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "stray files after write: {leftovers:?}"
        );
    }

    /// A settings write must not widen a permission-tightened config: the doc is
    /// built in an owner-only temp (`0600` on unix) and renamed over the target,
    /// so the inode replacement never reverts a user's `chmod 600` back to the
    /// umask-default `0644` (the file can hold an inline `api_key`).
    #[cfg(unix)]
    #[test]
    fn persist_setting_writes_the_config_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_setting_at(&path, "theme", ConfigValue::Str("dark")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a fresh config must be written 0600");
    }

    /// A malformed config is reported, not reset: the original bytes stay on
    /// disk and a `.bak` copy is left as a safety net.
    #[test]
    fn malformed_config_is_reported_and_left_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "theme = \"solarized\"\nthis is not = = toml\n";
        std::fs::write(&path, original).unwrap();

        let err = persist_setting_at(&path, "theme", ConfigValue::Str("dark"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not valid TOML"), "{err}");
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the malformed config must be left byte-for-byte intact"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            original,
            "a recoverable copy must be kept"
        );

        // A removal refuses the same way.
        assert!(remove_setting_at(&path, "theme").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// The happy paths still work: a missing file is created, and an existing
    /// file keeps its comments and unrelated keys.
    #[test]
    fn persist_creates_and_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        persist_setting_at(&path, "theme", ConfigValue::Str("dark")).unwrap();
        assert_eq!(doc_of(&path)["theme"].as_str(), Some("dark"));

        std::fs::write(&path, "# keep me\nother = true\ntheme = \"dark\"\n").unwrap();
        persist_setting_at(&path, "theme", ConfigValue::Str("light")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("other = true"), "{text}");
        assert_eq!(doc_of(&path)["theme"].as_str(), Some("light"));

        remove_setting_at(&path, "theme").unwrap();
        assert!(doc_of(&path).get("theme").is_none());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# keep me")
        );
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    /// Confinement is on unless the user turns it off: an unconfigured session
    /// runs `write`, so writes outside the working directory are refused by
    /// default. `--sandbox none` / `sandbox = "none"` is the opt-out.
    #[test]
    fn agent_config_defaults_to_write_sandbox() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.sandbox, SandboxMode::Write);
        assert!(cfg.sandbox_writable_roots.is_empty());
        // …and a write-capable agent derives `write` from it, a read-only one
        // `read` — the two rows an unconfigured session can produce.
        assert_eq!(effective_sandbox(cfg.sandbox, false), SandboxMode::Write);
        assert_eq!(effective_sandbox(cfg.sandbox, true), SandboxMode::Read);
        // A `strict` session hands its read-only agents the strict mode, and
        // still floors write-capable ones at `write` — otherwise a strict
        // session could not run a coder at all.
        assert_eq!(
            effective_sandbox(SandboxMode::Jail, true),
            SandboxMode::Jail
        );
        assert_eq!(
            effective_sandbox(SandboxMode::Jail, false),
            SandboxMode::Write
        );
        // `none`/`yolo` is the global opt-out and wins for every agent.
        assert_eq!(
            effective_sandbox(SandboxMode::None, true),
            SandboxMode::None
        );
        assert_eq!(
            effective_sandbox(SandboxMode::None, false),
            SandboxMode::None
        );
    }

    /// The config-file key parses the three spellings, and a misspelling is a
    /// hard TOML error (file values are errors — see the module docs).
    #[test]
    fn sandbox_config_key_parses_and_bad_value_is_a_hard_error() {
        for (text, want) in [
            ("sandbox = \"write\"", SandboxMode::Write),
            ("sandbox = \"read\"", SandboxMode::Read),
            ("sandbox = \"none\"", SandboxMode::None),
        ] {
            let fc: FileConfig = toml::from_str(text).expect("a valid sandbox mode");
            assert_eq!(fc.sandbox, Some(want));
            // …and it layers onto the config.
            let mut cfg = AgentConfig::default();
            cfg.apply_file(fc);
            assert_eq!(cfg.sandbox, want);
        }

        // Unset leaves the default alone.
        let fc: FileConfig = toml::from_str("").unwrap();
        assert_eq!(fc.sandbox, None);
        let mut cfg = AgentConfig::default();
        cfg.apply_file(fc);
        assert_eq!(cfg.sandbox, SandboxMode::Write, "the shipped default");

        // A misspelling never reaches the config: it fails to parse.
        let err = match toml::from_str::<FileConfig>("sandbox = \"wrote\"") {
            Ok(_) => panic!("a misspelled sandbox mode must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("wrote"), "{err}");

        // The extra writable roots come through as paths.
        let fc: FileConfig =
            toml::from_str("sandbox_writable_roots = [\"/home/me/.cargo\"]").unwrap();
        let mut cfg = AgentConfig::default();
        cfg.apply_file(fc);
        assert_eq!(
            cfg.sandbox_writable_roots,
            vec![PathBuf::from("/home/me/.cargo")]
        );
    }

    /// `$HRDR_SANDBOX` sets the mode; garbage is a warning that keeps the
    /// current value (env values are warnings, not errors).
    #[test]
    fn hrdr_sandbox_env_sets_mode_and_garbage_warns() {
        let setter = ENV_SETTERS
            .iter()
            .find(|(n, _)| *n == "HRDR_SANDBOX")
            .map(|(_, f)| *f)
            .expect("HRDR_SANDBOX is wired into ENV_SETTERS");

        let mut cfg = AgentConfig::default();
        setter(&mut cfg, "write").unwrap();
        assert_eq!(cfg.sandbox, SandboxMode::Write);
        setter(&mut cfg, " READ ").unwrap();
        assert_eq!(cfg.sandbox, SandboxMode::Read, "trimmed, case-insensitive");
        setter(&mut cfg, "none").unwrap();
        assert_eq!(cfg.sandbox, SandboxMode::None);
        setter(&mut cfg, "jail").unwrap();
        assert_eq!(cfg.sandbox, SandboxMode::Jail);
        // `yolo` and `off` are spellings of `none`, not modes of their own —
        // turning the sandbox off is one behavior, and it renders back as `none`.
        for spelling in ["yolo", "off", "YOLO"] {
            setter(&mut cfg, spelling).unwrap();
            assert_eq!(cfg.sandbox, SandboxMode::None, "{spelling}");
            assert_eq!(
                cfg.sandbox.as_str(),
                "none",
                "{spelling} renders canonically"
            );
        }

        setter(&mut cfg, "write").unwrap();
        let reason = setter(&mut cfg, "wrote").expect_err("garbage is reported");
        assert!(reason.contains("write, read, jail, or none"), "{reason}");
        assert_eq!(
            cfg.sandbox,
            SandboxMode::Write,
            "an unparseable value leaves the mode alone"
        );
    }

    /// A relative extra writable root cannot be matched against canonicalized
    /// paths, so the config file is refused rather than silently mis-scoped.
    ///
    /// The "valid" fixture has to be absolute *for the platform running the
    /// test*: `validate` asks [`std::path::Path::is_absolute`], and a config
    /// file is platform-local, so a drive-letter-less `/abs/ok` is correctly
    /// **not** absolute on Windows. Spelling it per-platform keeps the subject
    /// of the test the rejection of relative roots, not path syntax.
    #[cfg(windows)]
    const ABSOLUTE_ROOT: &str = r"C:\abs\ok";
    #[cfg(not(windows))]
    const ABSOLUTE_ROOT: &str = "/abs/ok";

    #[test]
    fn relative_sandbox_writable_roots_are_rejected() {
        let fc = FileConfig {
            sandbox_writable_roots: vec![
                ABSOLUTE_ROOT.to_string(),
                "relative/path".to_string(),
                "~/.cargo".to_string(),
            ],
            ..Default::default()
        };
        let errors = fc.validate();
        assert_eq!(errors.len(), 2, "{errors:?}");
        for entry in ["relative/path", "~/.cargo"] {
            assert!(
                errors.iter().any(|e| e
                    .contains("sandbox_writable_roots entries must be absolute paths")
                    && e.contains(entry)),
                "expected a diagnostic naming {entry}; got {errors:?}"
            );
        }

        // Absolute-only roots pass.
        let fc = FileConfig {
            sandbox_writable_roots: vec![ABSOLUTE_ROOT.to_string()],
            ..Default::default()
        };
        assert!(fc.validate().is_empty());
    }

    /// **A key nobody recognises is an error, not a shrug.** Serde ignores
    /// unknown fields by default, so a typo'd or stale key silently did nothing —
    /// the user's setting was simply absent, and the only clue was behaviour that
    /// did not match the file. Every table deserialized from the config now denies
    /// unknown fields, and the parse error names both the bad key and the
    /// alternatives.
    #[test]
    fn an_unknown_config_key_is_a_parse_error() {
        for (toml_text, bad_key) in [
            ("modle = \"claude://x\"\n", "modle"),
            ("[lsp]\nwiat_secs = 2\n", "wiat_secs"),
            (
                "[[hooks]]\nrun = \"x\"\nglob_pattern = \"*.rs\"\n",
                "glob_pattern",
            ),
            ("[tool_output]\nmax_line = 10\n", "max_line"),
        ] {
            let err = match toml::from_str::<FileConfig>(toml_text) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{bad_key} must be rejected"),
            };
            assert!(
                err.contains(bad_key),
                "the error names the offending key: {err}"
            );
        }

        // A valid file still parses — strictness must not reject what it documents.
        assert!(
            toml::from_str::<FileConfig>(
                "model = \"claude://x\"\n[lsp]\nwait_secs = 2\n[[hooks]]\nrun = \"fmt {path}\"\ntimeout_secs = 5\n",
            )
            .is_ok(),
            "documented keys must still parse"
        );
    }

    /// The millisecond spellings are *known* fields kept solely to reject them, so
    /// the message can do the division rather than leaving the user to notice their
    /// two-minute hook timeout had quietly become thirty seconds.
    #[test]
    fn the_millisecond_spellings_are_rejected_with_the_converted_value() {
        let fc: FileConfig =
            toml::from_str("[[hooks]]\nrun = \"x\"\ntimeout_ms = 120000\n").unwrap();
        let errors = fc.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("hooks.timeout_ms"), "{}", errors[0]);
        assert!(
            errors[0].contains("timeout_secs = 120"),
            "the fix is the message: {}",
            errors[0]
        );

        let fc: FileConfig = toml::from_str("[lsp]\nwait_ms = 2000\n").unwrap();
        let errors = fc.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("wait_secs = 2"), "{}", errors[0]);

        // Sub-second values still name a usable floor rather than `= 0`.
        let fc: FileConfig = toml::from_str("[lsp]\nwait_ms = 500\n").unwrap();
        assert!(
            fc.validate()[0].contains("wait_secs = 1"),
            "{:?}",
            fc.validate()
        );
    }

    /// Every cell of the decision table: the session default × what the agent
    /// is. Main agents, write sub-agents and revived sub-agents all arrive here
    /// as `read_only = false`; read-only profiles and read-only sub-agents as
    /// `read_only = true` — one shared derivation, no per-caller branch.
    #[test]
    fn effective_sandbox_matches_the_decision_table() {
        // Session `none` is a global opt-out: nothing is confined.
        assert_eq!(
            effective_sandbox(SandboxMode::None, false),
            SandboxMode::None
        );
        assert_eq!(
            effective_sandbox(SandboxMode::None, true),
            SandboxMode::None
        );

        // Session `write`: writers get write confinement, readers read.
        assert_eq!(
            effective_sandbox(SandboxMode::Write, false),
            SandboxMode::Write
        );
        assert_eq!(
            effective_sandbox(SandboxMode::Write, true),
            SandboxMode::Read
        );

        // Session `read`: a write-capable agent floors at `write` (it cannot
        // function under `read`, and its writable roots confine it anyway).
        assert_eq!(
            effective_sandbox(SandboxMode::Read, false),
            SandboxMode::Write
        );
        assert_eq!(
            effective_sandbox(SandboxMode::Read, true),
            SandboxMode::Read
        );

        // The sub-agent rows are those same two inputs: a sub-agent's config is
        // a clone of the base (so `sandbox` is inherited) with `read_only`
        // already final — including a revived sub-agent, which clones the base
        // the same way and takes a write slot.
        let base = AgentConfig {
            sandbox: SandboxMode::Write,
            ..Default::default()
        };
        let write_sub = AgentConfig {
            read_only: false,
            ..base.clone()
        };
        let read_sub = AgentConfig {
            read_only: true,
            ..base.clone()
        };
        let revived = write_sub.clone(); // task_revive: same clone, same derivation
        assert_eq!(
            effective_sandbox(write_sub.sandbox, write_sub.read_only),
            SandboxMode::Write
        );
        assert_eq!(
            effective_sandbox(read_sub.sandbox, read_sub.read_only),
            SandboxMode::Read
        );
        assert_eq!(
            effective_sandbox(revived.sandbox, revived.read_only),
            SandboxMode::Write
        );
    }
}
