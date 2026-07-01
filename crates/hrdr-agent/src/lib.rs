//! `hrdr-agent` — the agentic loop.
//!
//! Drives an OpenAI-compatible model through tool calls until a coding task is
//! complete: stream a turn, execute any requested tools, feed the results back,
//! repeat. Emits [`AgentEvent`]s for a UI (or stdout) to render live.

mod prompt;
mod session;

pub use session::{
    Session, SessionMeta, cwd_slug, list_sessions, resolve_session, session_dir, sessions_dir,
    unique_session_id,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use hrdr_llm::{Accumulator, ChatMessage, ChatStream, Client, Role, ToolDef};
use hrdr_tools::{Checkpoints, TodoItem, ToolContext, ToolRegistry};

pub use hrdr_tools::{CheckpointInfo, Checkpoints as FileCheckpoints};

pub use prompt::{gather_agent_docs, render_system};

/// Events emitted as a turn progresses.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A streamed delta of model "thinking" (reasoning channel).
    Reasoning(String),
    /// A streamed delta of assistant text.
    Text(String),
    /// A tool call is about to run.
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    /// A chunk of live output streamed by a running tool (e.g. `bash`).
    ToolOutput { id: String, chunk: String },
    /// A tool call finished.
    ToolEnd {
        id: String,
        name: String,
        result: String,
        ok: bool,
    },
    /// Token usage reported for the latest model call (when the server sends it).
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
    },
    /// An out-of-band notice from the agent (e.g. a retry or auto-compaction),
    /// surfaced to the user as a system line.
    Notice(String),
    /// The model produced a final answer with no further tool calls.
    TurnDone,
}

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    pub temperature: Option<f32>,
    /// Safety bound on tool-call iterations per user turn.
    pub max_steps: usize,
    /// Input discipline for the TUI: `true` = vim (hjkl), `false` = plain
    /// claude-style input (default).
    pub vim_mode: bool,
    /// Named provider preset (e.g. `zen`, `openai`, `local`). Resolved by the
    /// binary into `base_url`/`api_key`/backend behaviour via [`resolve_provider`].
    pub provider: Option<String>,
    /// Path to an hjkl theme TOML for the TUI; `None` uses the bundled default.
    pub theme: Option<String>,
    /// Model context window in tokens, for the status bar's "X of Y" display.
    /// Derived from the spawned backend when local; set in config for remotes.
    pub context_window: Option<u32>,
    /// Reasoning-effort label shown in the status bar (e.g. `low`/`medium`/`high`).
    pub effort: Option<String>,
    /// Auto-compaction trigger as a fraction of the context window (`0.0`–`1.0`);
    /// `0` (or any value outside that range) disables it. Default
    /// [`DEFAULT_AUTO_COMPACT`].
    pub auto_compact: f64,
    /// On TUI startup, resume the most recent session for the cwd. Default `true`.
    pub auto_resume: bool,
    /// Ring the terminal bell when a turn finishes (after a short minimum
    /// duration, so quick turns stay quiet). Default `true`.
    pub bell: bool,
    /// Icon set for the TUI: `nerd` (default), `unicode`, or `ascii`. `None`
    /// resolves to nerd (there's no portable way to probe the terminal font).
    pub icons: Option<String>,
    /// Per-message timestamp style: `none`, `relative` (e.g. `2m ago`), or
    /// `exact` (`HH:MM`). `None` resolves to `relative` (the default).
    pub timestamps: Option<String>,
    /// Status-bar mode: `none` (hidden), `truncate` (drop sections to fit), or
    /// `wrap` (use multiple rows). `None` resolves to `truncate` (the default).
    pub statusbar: Option<String>,
    /// File checkpointing: `on`, `off`, or `auto` (default) — `auto` enables it
    /// only outside a git repo (git already provides revert).
    pub checkpoints: Option<String>,
    /// How many turns a completed TODO item stays visible before it's pruned
    /// from the panel. Default [`DEFAULT_TODO_TTL`].
    pub todo_ttl: u64,
    /// Show the model's `<think>` reasoning blocks. Default `true`. Toggled at
    /// runtime by `/thinking` (aka `/reasoning`); set via `show_thinking` in
    /// config, `--show-thinking`, or `$HRDR_SHOW_THINKING`.
    pub show_thinking: bool,
    /// User-defined providers from `[providers.<name>]` in config, keyed by name.
    pub providers: HashMap<String, ProviderConfig>,
}

/// Default auto-compaction trigger: 85% of the context window (leaves headroom
/// so the next turn doesn't overflow).
pub const DEFAULT_AUTO_COMPACT: f64 = 0.85;

/// Default lifetime (in turns) a completed TODO item stays visible before it's
/// pruned: the turn it finishes plus four more.
pub const DEFAULT_TODO_TTL: u64 = 5;

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080/v1".to_string(),
            api_key: None,
            model: "default".to_string(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temperature: None,
            max_steps: 50,
            vim_mode: false,
            provider: None,
            theme: None,
            context_window: None,
            effort: None,
            auto_compact: DEFAULT_AUTO_COMPACT,
            auto_resume: true,
            bell: true,
            icons: None,
            timestamps: None,
            statusbar: None,
            checkpoints: None,
            todo_ttl: DEFAULT_TODO_TTL,
            show_thinking: true,
            providers: HashMap::new(),
        }
    }
}

impl AgentConfig {
    /// Resolve a provider name to a preset: a `[providers.<name>]` entry from
    /// config takes precedence over the built-ins (`zen`/`openai`/`local`).
    pub fn resolve_provider(&self, name: &str) -> Option<ResolvedProvider> {
        let lname = name.trim().to_ascii_lowercase();
        if let Some((_, c)) = self
            .providers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lname)
        {
            return Some(ResolvedProvider {
                base_url: c.base_url.clone(),
                key_env: c.key_env.clone(),
                api_key: c.api_key.clone(),
                model: c.model.clone(),
                remote: c.remote.unwrap_or(true),
                context_window: c.context_window,
            });
        }
        builtin_provider(name)
    }
}

/// A user-defined provider from `[providers.<name>]` in config.
#[derive(Debug, Clone, serde::Deserialize)]
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
    /// Whether hrdr should skip spawning a local backend (default: true).
    #[serde(default)]
    pub remote: Option<bool>,
    /// Model context window (for the status bar's "X of Y").
    #[serde(default)]
    pub context_window: Option<u32>,
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
}

/// Resolve a built-in provider name (case-insensitive).
///
/// - `zen` / `opencode` — OpenCode Zen gateway (`OPENCODE_API_KEY`).
/// - `openai` — OpenAI (`OPENAI_API_KEY`).
/// - `local` / `infr` — a local OpenAI-compatible server (spawned backend).
pub fn builtin_provider(name: &str) -> Option<ResolvedProvider> {
    let (base_url, key_env, remote) = match name.trim().to_ascii_lowercase().as_str() {
        "zen" | "opencode" | "opencode-zen" => {
            ("https://opencode.ai/zen/v1", "OPENCODE_API_KEY", true)
        }
        "openai" => ("https://api.openai.com/v1", "OPENAI_API_KEY", true),
        "openrouter" => ("https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", true),
        // Anthropic's OpenAI-compatible endpoint (Bearer auth via the compat layer).
        "claude" | "anthropic" => ("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY", true),
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
    })
}

/// Subset of config.toml we parse; all fields are optional.
#[derive(serde::Deserialize, Default)]
struct FileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    temperature: Option<f32>,
    vim: Option<bool>,
    provider: Option<String>,
    theme: Option<String>,
    context_window: Option<u32>,
    effort: Option<String>,
    auto_compact: Option<f64>,
    auto_resume: Option<bool>,
    bell: Option<bool>,
    icons: Option<String>,
    timestamps: Option<String>,
    statusbar: Option<String>,
    checkpoints: Option<String>,
    todo_ttl: Option<u64>,
    show_thinking: Option<bool>,
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,
}

fn config_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config")
            .join("hrdr")
            .join("config.toml"),
    )
}

impl AgentConfig {
    /// Load config with precedence: env > `~/.config/hrdr/config.toml` > built-in
    /// defaults. Lenient: a malformed config file is ignored (treated as absent).
    /// Does NOT auto-write a config file when one is missing.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Some(path) = config_path()
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(fc) = toml::from_str::<FileConfig>(&text)
        {
            cfg.apply_file(fc);
        }
        cfg.apply_env();
        cfg
    }

    /// Like [`load`](Self::load) but returns an error if the config file exists
    /// and fails to parse (for surfacing a warning + falling back to defaults).
    pub fn load_checked() -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(path) = config_path()
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let fc: FileConfig =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            cfg.apply_file(fc);
        }
        cfg.apply_env();
        Ok(cfg)
    }

    /// Layer file values over the current config.
    fn apply_file(&mut self, fc: FileConfig) {
        if let Some(v) = fc.base_url {
            self.base_url = v;
        }
        if let Some(v) = fc.api_key {
            self.api_key = Some(v);
        }
        if let Some(v) = fc.model {
            self.model = v;
        }
        if let Some(v) = fc.temperature {
            self.temperature = Some(v);
        }
        if let Some(v) = fc.vim {
            self.vim_mode = v;
        }
        if let Some(v) = fc.provider {
            self.provider = Some(v);
        }
        if let Some(v) = fc.theme {
            self.theme = Some(v);
        }
        if let Some(v) = fc.context_window {
            self.context_window = Some(v);
        }
        if let Some(v) = fc.effort {
            self.effort = Some(v);
        }
        if let Some(v) = fc.auto_compact {
            self.auto_compact = v;
        }
        if let Some(v) = fc.auto_resume {
            self.auto_resume = v;
        }
        if let Some(v) = fc.bell {
            self.bell = v;
        }
        if let Some(v) = fc.icons {
            self.icons = Some(v);
        }
        if let Some(v) = fc.timestamps {
            self.timestamps = Some(v);
        }
        if let Some(v) = fc.statusbar {
            self.statusbar = Some(v);
        }
        if let Some(v) = fc.checkpoints {
            self.checkpoints = Some(v);
        }
        if let Some(v) = fc.todo_ttl {
            self.todo_ttl = v;
        }
        if let Some(v) = fc.show_thinking {
            self.show_thinking = v;
        }
        if !fc.providers.is_empty() {
            self.providers = fc.providers;
        }
    }

    /// Layer environment variables over the current config. Every knob is one
    /// row in [`ENV_SETTERS`]; adding a new env var means adding a row there, not
    /// another `if let` here. `HRDR_API_KEY` is special-cased (it has a fallback
    /// var) below.
    fn apply_env(&mut self) {
        for (name, set) in ENV_SETTERS {
            if let Ok(v) = std::env::var(name) {
                set(self, v);
            }
        }
        let env_key = std::env::var("HRDR_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        if env_key.is_some() {
            self.api_key = env_key;
        }
    }
}

/// Parse a boolean-ish env value; `None` (leave the current value) if it's not
/// one of the recognized on/off spellings.
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

/// Applies an env var's string value to the config.
type EnvSetter = fn(&mut AgentConfig, String);

/// Env var → setter table used by [`AgentConfig::apply_env`]. Adding a knob is a
/// single row here (non-capturing closures coerce to `fn` pointers). Values that
/// need parsing (numbers, bools) silently keep the current value on a bad parse.
const ENV_SETTERS: &[(&str, EnvSetter)] = &[
    ("HRDR_PROVIDER", |c, v| c.provider = Some(v)),
    ("HRDR_THEME", |c, v| c.theme = Some(v)),
    ("HRDR_BASE_URL", |c, v| c.base_url = v),
    ("HRDR_MODEL", |c, v| c.model = v),
    ("HRDR_ICONS", |c, v| c.icons = Some(v)),
    ("HRDR_TIMESTAMPS", |c, v| c.timestamps = Some(v)),
    ("HRDR_STATUSBAR", |c, v| c.statusbar = Some(v)),
    ("HRDR_CHECKPOINTS", |c, v| c.checkpoints = Some(v)),
    ("HRDR_AUTO_COMPACT", |c, v| {
        if let Ok(f) = v.parse() {
            c.auto_compact = f;
        }
    }),
    ("HRDR_AUTO_RESUME", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.auto_resume = b;
        }
    }),
    ("HRDR_BELL", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.bell = b;
        }
    }),
    ("HRDR_TODO_TTL", |c, v| {
        if let Ok(n) = v.parse() {
            c.todo_ttl = n;
        }
    }),
    ("HRDR_SHOW_THINKING", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.show_thinking = b;
        }
    }),
];

/// A value to persist into the user config file.
pub enum ConfigValue<'a> {
    Str(&'a str),
    Bool(bool),
    Float(f64),
    Int(i64),
}

/// Path to the user config file (`~/.config/hrdr/config.toml`), if `HOME` is set.
pub fn config_file_path() -> Option<std::path::PathBuf> {
    config_path()
}

/// Set `key = value` in the user config file (creating it if needed), preserving
/// existing keys, ordering, and comments. Returns the file path.
pub fn persist_setting(key: &str, value: ConfigValue) -> Result<std::path::PathBuf> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the config file"))?;
    let mut doc = read_config_doc(&path);
    match value {
        ConfigValue::Str(s) => doc[key] = toml_edit::value(s),
        ConfigValue::Bool(b) => doc[key] = toml_edit::value(b),
        ConfigValue::Float(f) => doc[key] = toml_edit::value(f),
        ConfigValue::Int(i) => doc[key] = toml_edit::value(i),
    }
    write_config_doc(&path, &doc)?;
    Ok(path)
}

/// Remove `key` from the user config file (if present). Returns the file path.
pub fn remove_setting(key: &str) -> Result<std::path::PathBuf> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no HOME to locate the config file"))?;
    let mut doc = read_config_doc(&path);
    doc.remove(key);
    write_config_doc(&path, &doc)?;
    Ok(path)
}

/// Whether `cwd` (or an ancestor) is inside a git repo. `.git` may be a
/// directory (normal) or a file (worktrees/submodules).
pub fn in_git_repo(cwd: &std::path::Path) -> bool {
    cwd.ancestors().any(|d| d.join(".git").exists())
}

/// Directory for this cwd's file checkpoints (`…/hrdr/checkpoints/<cwd-slug>`).
fn checkpoint_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    hjkl_xdg::data_dir("hrdr")
        .ok()
        .map(|d| d.join("checkpoints").join(cwd_slug(&cwd.to_string_lossy())))
}

fn read_config_doc(path: &std::path::Path) -> toml_edit::DocumentMut {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
        .unwrap_or_default()
}

fn write_config_doc(path: &std::path::Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// System prompt for the one-off compaction (summarization) call.
const COMPACT_SYSTEM: &str = "\
You are summarizing a software-engineering conversation between a user and an AI \
coding agent so it can continue in a fresh context with nothing important lost. \
Be precise, technical, and exhaustive about concrete details — vague summaries are \
useless here.";

/// User-turn instruction that triggers the structured summary.
const COMPACT_TRIGGER: &str = "\
Summarize the conversation so far. The summary REPLACES the full history, so it must \
let the agent continue seamlessly. Use these sections:

1. **Intent & requirements** — what the user asked for, in their own terms, including \
   explicit constraints and preferences.
2. **Technical context** — languages, frameworks, key APIs, architecture decisions.
3. **Files & code** — every file created or modified (with paths) and the gist of the \
   changes; include important snippets, signatures, and config values verbatim.
4. **Commands & results** — notable commands run and their outcomes (builds, tests, \
   commits, pushes).
5. **Errors & fixes** — problems hit and how they were resolved.
6. **Current state** — what is done and verified vs. in progress.
7. **Pending tasks & next step** — what remains, and the single most immediate next \
   action.

Be specific: prefer exact names, paths, and values over paraphrase. Output only the \
summary.";

/// A running agent: model client + tools + conversation state.
pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    ctx: ToolContext,
    messages: Vec<ChatMessage>,
    max_steps: usize,
    /// Gathered `AGENTS.md` project instructions for the current cwd, if any.
    project_docs: Option<String>,
    /// File checkpoint store (per-turn pre-images), if a store dir is available.
    checkpoints: Option<Arc<Mutex<Checkpoints>>>,
}

impl Agent {
    /// Construct an agent, seeding the system prompt for the default tool set.
    pub fn new(config: AgentConfig) -> Result<Self> {
        let tools = ToolRegistry::with_defaults();
        let mut ctx = ToolContext::new(config.cwd.clone());
        let project_docs = gather_agent_docs(&config.cwd);
        let system = render_system(&tools, &config.cwd, project_docs.as_deref())?;

        // File checkpoint store, keyed by working directory (like sessions).
        // `auto` (default) enables it only outside a git repo — git already
        // provides revert, so checkpointing there is redundant.
        let enable_checkpoints = match config
            .checkpoints
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("on" | "true" | "yes" | "1" | "always") => true,
            Some("off" | "false" | "no" | "0" | "never") => false,
            _ => !in_git_repo(&config.cwd),
        };
        let checkpoints = enable_checkpoints
            .then(|| checkpoint_dir(&config.cwd))
            .flatten()
            .and_then(|dir| Checkpoints::open(dir).ok())
            .map(|c| Arc::new(Mutex::new(c)));
        ctx.checkpoints = checkpoints.clone();

        let mut client = Client::new(config.base_url, config.api_key, config.model);
        if let Some(t) = config.temperature {
            client = client.with_temperature(t);
        }

        Ok(Self {
            client,
            tools,
            ctx,
            messages: vec![ChatMessage::system(system)],
            max_steps: config.max_steps,
            project_docs,
            checkpoints,
        })
    }

    /// The file checkpoint store, if available (for `/revert` / `/checkpoints`).
    pub fn checkpoints(&self) -> Option<Arc<Mutex<Checkpoints>>> {
        self.checkpoints.clone()
    }

    /// The gathered `AGENTS.md` project instructions for the current cwd, if any.
    pub fn project_docs(&self) -> Option<&str> {
        self.project_docs.as_deref()
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Reset the conversation to a fresh state — as if the agent was just
    /// constructed for the current cwd. Drops all history and **re-reads
    /// `AGENTS.md`**, so an updated or removed project-instructions file is
    /// reflected (the old system prompt is not kept around).
    pub fn clear(&mut self) {
        self.messages.clear();
        self.refresh_system();
    }

    /// Re-gather `AGENTS.md` for the current cwd and rebuild the system prompt
    /// in `messages[0]` (seeding it if the history is empty). Shared by
    /// [`Self::clear`] and [`Self::set_cwd`].
    fn refresh_system(&mut self) {
        self.project_docs = gather_agent_docs(&self.ctx.cwd);
        let Ok(system) = render_system(&self.tools, &self.ctx.cwd, self.project_docs.as_deref())
        else {
            return;
        };
        if self.messages.first().map(|m| m.role == Role::System) == Some(true) {
            self.messages[0] = ChatMessage::system(system);
        } else {
            self.messages.insert(0, ChatMessage::system(system));
        }
    }

    /// A clone of the full message history (for saving a session).
    pub fn messages_owned(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }

    /// Replace the message history (for resuming a session).
    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
    }

    /// Switch the model for subsequent turns.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.client.model = model.into();
    }

    /// A clone of the model client (for out-of-band calls like `/models`).
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Working directory the tools operate in.
    pub fn cwd(&self) -> std::path::PathBuf {
        self.ctx.cwd.clone()
    }

    /// Change the tools' working directory. Reloads `AGENTS.md` for the new
    /// directory and refreshes the system prompt in place.
    pub fn set_cwd(&mut self, cwd: std::path::PathBuf) {
        self.ctx.cwd = cwd;
        self.refresh_system();
    }

    /// Registered tools as `(name, description)` pairs.
    pub fn tools(&self) -> Vec<(String, String)> {
        self.tools
            .defs()
            .into_iter()
            .map(|d| (d.function.name, d.function.description))
            .collect()
    }

    /// Sampling temperature, if set.
    pub fn temperature(&self) -> Option<f32> {
        self.client.temperature
    }

    /// Set (or clear) the sampling temperature.
    pub fn set_temperature(&mut self, t: Option<f32>) {
        self.client.temperature = t;
    }

    /// Repoint at a different OpenAI-compatible endpoint + key (provider switch).
    pub fn set_endpoint(&mut self, base_url: impl Into<String>, api_key: Option<String>) {
        self.client.set_base_url(base_url);
        self.client.set_api_key(api_key);
    }

    /// Drop the last user turn (and everything after it) from history, returning
    /// that user message's text so it can be re-sent (`/retry`).
    pub fn rewind_last_user(&mut self) -> Option<String> {
        let idx = self.messages.iter().rposition(|m| m.role == Role::User)?;
        let text = self.messages[idx].content.clone();
        self.messages.truncate(idx);
        text
    }

    /// Shared TODO list, mutated by the `todo_write` tool.
    pub fn todos(&self) -> Arc<Mutex<Vec<TodoItem>>> {
        self.ctx.todos.clone()
    }

    /// Number of messages currently in history (including the system prompt).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Compact the conversation: ask the model for a structured summary and
    /// replace the history with `[system prompt, summary]`, so the context
    /// shrinks while continuity is preserved (Claude Code / opencode style).
    ///
    /// `instructions` optionally steers the summary's focus. Returns
    /// `(messages_before, messages_after)`; a no-op when there's nothing beyond
    /// the system prompt and one message.
    pub async fn compact(&mut self, instructions: Option<&str>) -> Result<(usize, usize)> {
        let before = self.messages.len();
        if before <= 2 {
            return Ok((before, before));
        }

        // Build a one-off summarization request: a dedicated summarizer system
        // prompt + the conversation so far (minus its own system prompt) + the
        // trigger instruction. No tools — we only want prose back.
        let mut trigger = COMPACT_TRIGGER.to_string();
        if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
            trigger.push_str("\n\nAdditional instructions for the summary, follow them closely:\n");
            trigger.push_str(extra);
        }
        let mut req = Vec::with_capacity(before + 1);
        req.push(ChatMessage::system(COMPACT_SYSTEM.to_string()));
        req.extend(self.messages.iter().skip(1).cloned());
        req.push(ChatMessage::user(trigger));

        let mut stream = self.client.chat_stream(req, vec![]).await?;
        let mut acc = Accumulator::new();
        while let Some(chunk) = stream.next().await {
            acc.push(&chunk?);
        }
        let summary = acc.into_message().content.unwrap_or_default();
        if summary.trim().is_empty() {
            bail!("compaction produced an empty summary");
        }

        // Replace history: keep the original (coding) system prompt, then a
        // single user message carrying the summary as the continuation seed.
        let system = self.messages[0].clone();
        let continuation = format!(
            "This session is being continued from an earlier conversation that ran out of \
             context. The summary below captures everything that happened; continue from where \
             it left off without losing any detail.\n\n{summary}"
        );
        self.messages = vec![system, ChatMessage::user(continuation)];
        Ok((before, self.messages.len()))
    }

    /// Run one user turn to completion, emitting events as it goes.
    pub async fn run<F>(&mut self, user_input: impl Into<String>, mut on_event: F) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        // A previous turn interrupted mid tool-call can leave the history ending
        // with an assistant `tool_calls` message whose results are missing —
        // strict servers reject that. Backfill stubs before the new user turn.
        repair_dangling_tool_calls(&mut self.messages);
        self.messages.push(ChatMessage::user(user_input.into()));
        // Start a fresh file checkpoint for this turn's edits.
        if let Some(cp) = &self.checkpoints
            && let Ok(mut c) = cp.lock()
        {
            c.begin_turn();
        }
        let defs = self.tools.defs();
        // Allow one automatic compaction per turn when the context overflows.
        let mut overflow_compacted = false;

        for _ in 0..self.max_steps {
            // Stream one assistant turn, accumulating text + tool calls. The
            // connect is retried on transient errors and auto-compacted once on
            // a context-length overflow.
            let mut stream = self
                .connect_stream(&defs, &mut overflow_compacted, &mut on_event)
                .await?;
            let mut acc = Accumulator::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if let Some(choice) = chunk.choices.first()
                    && let Some(r) = &choice.delta.reasoning_content
                {
                    on_event(AgentEvent::Reasoning(r.clone()));
                }
                if let Some(text) = acc.push(&chunk) {
                    on_event(AgentEvent::Text(text));
                }
            }

            // Emit usage for the status bar + auto-compaction. Prefer the
            // server's reported counts; when it doesn't send any (e.g. a server
            // that ignores `stream_options.include_usage`), fall back to a rough
            // estimate so the context bar and compaction still work — an estimate
            // beats a stale/zero reading, and the overflow-retry path covers any
            // under-estimate.
            let (prompt_tokens, completion_tokens) = match &acc.usage {
                Some(u) => (u.prompt_tokens, u.completion_tokens),
                None => (
                    estimate_tokens_in_messages(&self.messages),
                    estimate_tokens(&acc.content),
                ),
            };
            on_event(AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            });

            let assistant = acc.into_message();
            let tool_calls = assistant.tool_calls.clone().unwrap_or_default();
            self.messages.push(assistant);

            if tool_calls.is_empty() {
                on_event(AgentEvent::TurnDone);
                return Ok(());
            }

            // Execute each requested tool, feeding results back.
            for call in tool_calls {
                on_event(AgentEvent::ToolStart {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    args: call.function.arguments.clone(),
                });

                let result = self
                    .run_tool_streaming(
                        &call.id,
                        &call.function.name,
                        &call.function.arguments,
                        &mut on_event,
                    )
                    .await;
                let (ok, body) = match result {
                    Ok(s) => (true, s),
                    Err(e) => (false, format!("Error: {e}")),
                };

                on_event(AgentEvent::ToolEnd {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    result: body.clone(),
                    ok,
                });
                self.messages.push(ChatMessage::tool_result(call.id, body));
            }
        }

        bail!("agent exceeded max_steps ({})", self.max_steps);
    }

    /// Open a chat stream, retrying transient network/server errors with
    /// exponential backoff and auto-compacting once on a context-length
    /// overflow. Emits `Notice` events for each recovery attempt.
    async fn connect_stream<F: FnMut(AgentEvent)>(
        &mut self,
        defs: &[ToolDef],
        overflow_compacted: &mut bool,
        on_event: &mut F,
    ) -> Result<ChatStream> {
        const MAX_RETRIES: usize = 4;
        let mut attempt = 0usize;
        loop {
            match self
                .client
                .chat_stream(self.messages.clone(), defs.to_vec())
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    // Context overflow → compact once, then retry.
                    if is_context_overflow(&e) && !*overflow_compacted && self.messages.len() > 2 {
                        on_event(AgentEvent::Notice(
                            "context window exceeded — compacting and retrying".to_string(),
                        ));
                        self.compact(None).await?;
                        *overflow_compacted = true;
                        continue;
                    }
                    // Transient network/server error → backoff and retry.
                    if is_transient(&e) && attempt < MAX_RETRIES {
                        attempt += 1;
                        let delay = retry_backoff(attempt);
                        on_event(AgentEvent::Notice(format!(
                            "network error — retrying in {:.0}s (attempt {attempt}/{MAX_RETRIES})",
                            delay.as_secs_f64()
                        )));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Execute a tool, forwarding any live output it streams as `ToolOutput`
    /// events while it runs.
    async fn run_tool_streaming<F: FnMut(AgentEvent)>(
        &self,
        id: &str,
        name: &str,
        raw_args: &str,
        on_event: &mut F,
    ) -> Result<String> {
        let args: serde_json::Value = if raw_args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(raw_args)
                .map_err(|e| anyhow::anyhow!("invalid tool arguments JSON: {e}"))?
        };
        // Attach a per-call output sink so the tool can stream progress.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut ctx = self.ctx.clone();
        ctx.stream = Some(tx);

        let fut = self.tools.execute(name, args, &ctx);
        tokio::pin!(fut);
        let result = loop {
            tokio::select! {
                r = &mut fut => break r,
                Some(chunk) = rx.recv() => on_event(AgentEvent::ToolOutput {
                    id: id.to_string(),
                    chunk,
                }),
            }
        };
        // Drain any chunks buffered between the last poll and completion.
        while let Ok(chunk) = rx.try_recv() {
            on_event(AgentEvent::ToolOutput {
                id: id.to_string(),
                chunk,
            });
        }
        result
    }
}

// Re-exports consumers need without reaching into sub-crates.
pub use hrdr_llm::ChatMessage as Message;
pub use hrdr_llm::Role as MessageRole;
pub use hrdr_tools::TodoItem as Todo;

/// Whether an error looks like a transient network/server failure worth
/// retrying (connection issues, 429, or 5xx).
fn is_transient(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("request failed")           // reqwest send() failure (network)
        || msg.contains("timed out")
        || msg.contains("connection")
        || msg.contains("reset")
        || msg.contains("broken pipe")
        || msg.contains("returned 429")       // rate limited
        || msg.contains("returned 500")
        || msg.contains("returned 502")
        || msg.contains("returned 503")
        || msg.contains("returned 504")
}

/// Whether an error is the server rejecting the request for exceeding the
/// model's context window.
fn is_context_overflow(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("context length")
        || msg.contains("context_length")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("context size")
        || msg.contains("too many tokens")
        || msg.contains("reduce the length")
}

/// Exponential backoff for retry `attempt` (1-based), capped at 8s.
fn retry_backoff(attempt: usize) -> std::time::Duration {
    let secs = 0.5 * 2f64.powi((attempt as i32 - 1).max(0));
    std::time::Duration::from_secs_f64(secs.min(8.0))
}

/// Repair a history left dangling by an interrupted turn. An assistant message
/// with `tool_calls` must be followed by a `role:"tool"` result for every call
/// id, or strict servers (OpenAI, and infr) reject the next request. If the most
/// recent such assistant message is missing any results (the turn was cancelled
/// mid tool-call), append a stub result for each unanswered id. No-op when the
/// last tool-calling turn is already complete.
fn repair_dangling_tool_calls(messages: &mut Vec<ChatMessage>) {
    let Some(idx) = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant && m.tool_calls.is_some())
    else {
        return;
    };
    let call_ids: Vec<String> = messages[idx]
        .tool_calls
        .as_ref()
        .map(|calls| calls.iter().map(|c| c.id.clone()).collect())
        .unwrap_or_default();
    let answered: std::collections::HashSet<&str> = messages[idx + 1..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let missing: Vec<String> = call_ids
        .into_iter()
        .filter(|id| !answered.contains(id.as_str()))
        .collect();
    for id in missing {
        messages.push(ChatMessage::tool_result(id, "[interrupted]"));
    }
}

/// Very rough token estimate (~4 characters per token) for `text`. Used only as
/// a fallback when the server reports no usage — good enough for the context bar
/// + auto-compaction, not for billing.
fn estimate_tokens(text: &str) -> u32 {
    (text.len() / 4) as u32
}

/// Estimate the prompt tokens of a whole request: each message's content and any
/// tool-call names/arguments, plus a small per-message overhead for the role and
/// structural tokens the chat template adds.
fn estimate_tokens_in_messages(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .map(|m| {
            let content = m.content.as_deref().map(str::len).unwrap_or(0);
            let calls = m
                .tool_calls
                .as_ref()
                .map(|tcs| {
                    tcs.iter()
                        .map(|c| c.function.name.len() + c.function.arguments.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            (content + calls) as u32 / 4 + 4
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        Agent, AgentConfig, ENV_SETTERS, FileConfig, ProviderConfig, builtin_provider,
        estimate_tokens, estimate_tokens_in_messages, in_git_repo, is_context_overflow,
        is_transient, parse_env_bool, repair_dangling_tool_calls,
    };
    use crate::cwd_slug;
    use hrdr_llm::{ChatMessage, FunctionCall, Role, ToolCall};

    fn system_prompt(agent: &Agent) -> String {
        agent.messages()[0].content.clone().unwrap_or_default()
    }

    fn assistant_with_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            tool_calls: Some(
                ids.iter()
                    .map(|id| ToolCall {
                        id: id.to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "t".to_string(),
                            arguments: "{}".to_string(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn repair_backfills_missing_tool_results_after_interrupt() {
        // Interrupted after the first of two calls got its result.
        let mut msgs = vec![
            ChatMessage::user("go"),
            assistant_with_calls(&["a", "b"]),
            ChatMessage::tool_result("a", "done a"),
        ];
        repair_dangling_tool_calls(&mut msgs);
        // A stub was appended for the unanswered "b" — history is now valid.
        assert_eq!(msgs.len(), 4);
        let last = msgs.last().unwrap();
        assert_eq!(last.role, Role::Tool);
        assert_eq!(last.tool_call_id.as_deref(), Some("b"));
        assert_eq!(last.content.as_deref(), Some("[interrupted]"));
    }

    #[test]
    fn repair_is_a_noop_when_all_calls_are_answered() {
        let mut msgs = vec![
            assistant_with_calls(&["a"]),
            ChatMessage::tool_result("a", "done"),
        ];
        let before = msgs.len();
        repair_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), before);
    }

    #[test]
    fn repair_ignores_history_with_no_tool_calls() {
        let mut msgs = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
        repair_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn token_estimate_scales_with_content() {
        use super::{estimate_tokens, estimate_tokens_in_messages};
        // ~4 chars/token.
        assert_eq!(estimate_tokens(&"x".repeat(40)), 10);
        assert_eq!(estimate_tokens(""), 0);
        // Per-message overhead + content; more content ⇒ strictly more tokens.
        let small = estimate_tokens_in_messages(&[ChatMessage::user("hi")]);
        let big = estimate_tokens_in_messages(&[ChatMessage::user("word ".repeat(100))]);
        assert!(big > small);
        assert!(small >= 4, "per-message overhead applies");
    }

    #[test]
    fn clear_rereads_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let agents_md = dir.path().join("AGENTS.md");
        std::fs::write(&agents_md, "ORIGINAL_MARKER").unwrap();

        let cfg = AgentConfig {
            cwd: dir.path().to_path_buf(),
            checkpoints: Some("off".to_string()), // keep the test hermetic
            ..Default::default()
        };
        let mut agent = Agent::new(cfg).unwrap();
        assert!(system_prompt(&agent).contains("ORIGINAL_MARKER"));

        // An updated AGENTS.md must be reflected after /clear (the bug: the old
        // system prompt was kept, so stale instructions lingered forever).
        std::fs::write(&agents_md, "UPDATED_MARKER").unwrap();
        agent.clear();
        let sys = system_prompt(&agent);
        assert!(sys.contains("UPDATED_MARKER"));
        assert!(!sys.contains("ORIGINAL_MARKER"));

        // Removing AGENTS.md drops it entirely on the next /clear.
        std::fs::remove_file(&agents_md).unwrap();
        agent.clear();
        assert!(!system_prompt(&agent).contains("UPDATED_MARKER"));
    }

    #[test]
    fn classifies_transient_and_overflow_errors() {
        let overflow = anyhow::anyhow!(
            "chat endpoint returned 400 Bad Request: This model's maximum context length is 8192 tokens"
        );
        assert!(is_context_overflow(&overflow));
        assert!(!is_transient(&overflow));

        let rate = anyhow::anyhow!("chat endpoint returned 429 Too Many Requests: slow down");
        assert!(is_transient(&rate));
        assert!(!is_context_overflow(&rate));

        let net = anyhow::anyhow!("chat stream request failed: connection refused");
        assert!(is_transient(&net));

        let plain = anyhow::anyhow!("chat endpoint returned 400 Bad Request: invalid tool schema");
        assert!(!is_transient(&plain));
        assert!(!is_context_overflow(&plain));
    }

    #[test]
    fn zen_builtin_is_remote_with_opencode_key() {
        let p = builtin_provider("ZEN").expect("zen resolves (case-insensitive)");
        assert_eq!(p.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(p.key_env.as_deref(), Some("OPENCODE_API_KEY"));
        assert!(p.remote);
        assert!(builtin_provider("opencode").is_some());
    }

    #[test]
    fn local_builtin_is_not_remote_and_unknown_is_none() {
        assert!(!builtin_provider("local").unwrap().remote);
        assert!(builtin_provider("nope").is_none());
    }

    #[test]
    fn config_provider_overrides_builtin() {
        let mut cfg = AgentConfig::default();
        cfg.providers.insert(
            "zen".to_string(),
            ProviderConfig {
                base_url: "https://my.zen/v1".to_string(),
                key_env: Some("MY_KEY".to_string()),
                api_key: None,
                model: Some("my-model".to_string()),
                remote: Some(true),
                context_window: Some(123),
            },
        );
        // Custom "zen" shadows the built-in; an unknown custom name resolves too.
        let p = cfg.resolve_provider("zen").unwrap();
        assert_eq!(p.base_url, "https://my.zen/v1");
        assert_eq!(p.model.as_deref(), Some("my-model"));
        assert_eq!(p.context_window, Some(123));
        // Built-ins still resolve when not shadowed.
        assert!(cfg.resolve_provider("openai").is_some());
        assert!(cfg.resolve_provider("nope").is_none());
    }

    // ---- parse_env_bool ----

    #[test]
    fn parse_env_bool_recognizes_all_spellings() {
        // falsy
        for s in ["0", "false", "off", "no", "FALSE", "OFF"] {
            assert_eq!(parse_env_bool(s), Some(false), "expected false for {s:?}");
        }
        // truthy
        for s in ["1", "true", "on", "yes", "TRUE", "YES"] {
            assert_eq!(parse_env_bool(s), Some(true), "expected true for {s:?}");
        }
        // unrecognized → None (leave current value unchanged)
        assert_eq!(parse_env_bool("maybe"), None);
        assert_eq!(parse_env_bool(""), None);
        assert_eq!(parse_env_bool("2"), None);
    }

    // ---- ENV_SETTERS ----

    fn find_setter(key: &str) -> fn(&mut AgentConfig, String) {
        ENV_SETTERS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, s)| *s)
            .unwrap_or_else(|| panic!("setter not found for {key}"))
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn env_setters_string_fields_directly() {
        // Exercise each string-typed setter in ENV_SETTERS by calling the fn
        // pointer directly, without touching process environment.
        let cases: &[(&str, fn(&AgentConfig) -> &str)] = &[
            ("HRDR_PROVIDER", |c| c.provider.as_deref().unwrap_or("")),
            ("HRDR_THEME", |c| c.theme.as_deref().unwrap_or("")),
            ("HRDR_BASE_URL", |c| &c.base_url),
            ("HRDR_MODEL", |c| &c.model),
            ("HRDR_ICONS", |c| c.icons.as_deref().unwrap_or("")),
            ("HRDR_TIMESTAMPS", |c| c.timestamps.as_deref().unwrap_or("")),
            ("HRDR_STATUSBAR", |c| c.statusbar.as_deref().unwrap_or("")),
            ("HRDR_CHECKPOINTS", |c| {
                c.checkpoints.as_deref().unwrap_or("")
            }),
        ];
        for (key, getter) in cases {
            let setter = find_setter(key);
            let mut cfg = AgentConfig::default();
            setter(&mut cfg, "test_value".to_string());
            assert_eq!(getter(&cfg), "test_value", "setter for {key} did not apply");
        }
    }

    #[test]
    fn env_setter_bool_ignores_bad_value() {
        // HRDR_BELL with an unrecognized value must leave `bell` unchanged.
        let setter = find_setter("HRDR_BELL");
        let mut cfg = AgentConfig::default();
        let original = cfg.bell;
        setter(&mut cfg, "maybe".to_string());
        assert_eq!(cfg.bell, original, "bad bool value should be ignored");
    }

    #[test]
    fn env_setter_numeric_ignores_bad_value() {
        // HRDR_TODO_TTL with a non-numeric string must leave `todo_ttl` unchanged.
        let setter = find_setter("HRDR_TODO_TTL");
        let mut cfg = AgentConfig::default();
        let original = cfg.todo_ttl;
        setter(&mut cfg, "notanumber".to_string());
        assert_eq!(
            cfg.todo_ttl, original,
            "bad numeric value should be ignored"
        );
    }

    #[test]
    fn env_setter_auto_compact_numeric() {
        let setter = find_setter("HRDR_AUTO_COMPACT");
        let mut cfg = AgentConfig::default();
        setter(&mut cfg, "0.5".to_string());
        assert!((cfg.auto_compact - 0.5).abs() < f64::EPSILON);
    }

    // ---- apply_file ----

    #[test]
    fn apply_file_sets_all_fields() {
        let mut cfg = AgentConfig::default();
        cfg.apply_file(FileConfig {
            base_url: Some("http://custom/v1".to_string()),
            api_key: Some("key123".to_string()),
            model: Some("gpt-4".to_string()),
            temperature: Some(0.5),
            vim: Some(true),
            provider: Some("zen".to_string()),
            theme: Some("dark".to_string()),
            context_window: Some(8192),
            effort: Some("high".to_string()),
            auto_compact: Some(0.7),
            auto_resume: Some(false),
            bell: Some(false),
            icons: Some("ascii".to_string()),
            timestamps: Some("exact".to_string()),
            statusbar: Some("wrap".to_string()),
            checkpoints: Some("on".to_string()),
            todo_ttl: Some(10),
            show_thinking: Some(false),
            providers: HashMap::new(),
        });
        assert_eq!(cfg.base_url, "http://custom/v1");
        assert_eq!(cfg.api_key.as_deref(), Some("key123"));
        assert_eq!(cfg.model, "gpt-4");
        assert_eq!(cfg.temperature, Some(0.5));
        assert!(cfg.vim_mode);
        assert_eq!(cfg.provider.as_deref(), Some("zen"));
        assert_eq!(cfg.theme.as_deref(), Some("dark"));
        assert_eq!(cfg.context_window, Some(8192));
        assert_eq!(cfg.effort.as_deref(), Some("high"));
        assert!((cfg.auto_compact - 0.7).abs() < f64::EPSILON);
        assert!(!cfg.auto_resume);
        assert!(!cfg.bell);
        assert_eq!(cfg.icons.as_deref(), Some("ascii"));
        assert_eq!(cfg.timestamps.as_deref(), Some("exact"));
        assert_eq!(cfg.statusbar.as_deref(), Some("wrap"));
        assert_eq!(cfg.checkpoints.as_deref(), Some("on"));
        assert_eq!(cfg.todo_ttl, 10);
        assert!(!cfg.show_thinking);
    }

    // ---- is_transient / is_context_overflow (additional variants) ----

    #[test]
    fn is_transient_more_variants() {
        for msg in [
            "chat stream request failed: connection timed out",
            "broken pipe",
            "chat endpoint returned 502 Bad Gateway: upstream down",
            "chat endpoint returned 503 Service Unavailable",
            "chat endpoint returned 504 Gateway Timeout",
            "connection reset by peer",
        ] {
            assert!(
                is_transient(&anyhow::anyhow!("{msg}")),
                "expected transient for: {msg}"
            );
        }
    }

    #[test]
    fn is_context_overflow_more_variants() {
        for msg in [
            "context window exceeded",
            "too many tokens in the prompt",
            "please reduce the length of the messages",
            "context size limit reached",
            "context_length exceeded",
        ] {
            assert!(
                is_context_overflow(&anyhow::anyhow!("{msg}")),
                "expected context overflow for: {msg}"
            );
        }
    }

    // ---- repair_dangling_tool_calls (additional cases) ----

    #[test]
    fn repair_no_op_when_all_answered_then_user_turn() {
        // A complete turn followed by a subsequent user message should not get
        // stubs appended — the tool results are all present.
        let mut msgs = vec![
            ChatMessage::user("first"),
            assistant_with_calls(&["a", "b"]),
            ChatMessage::tool_result("a", "done_a"),
            ChatMessage::tool_result("b", "done_b"),
            ChatMessage::user("second"),
        ];
        let before = msgs.len();
        repair_dangling_tool_calls(&mut msgs);
        assert_eq!(
            msgs.len(),
            before,
            "no stubs expected when all calls answered"
        );
    }

    #[test]
    fn repair_partially_answered_three_calls() {
        // Three tool calls; only first two answered → stub for third only.
        let mut msgs = vec![
            ChatMessage::user("go"),
            assistant_with_calls(&["x", "y", "z"]),
            ChatMessage::tool_result("x", "rx"),
            ChatMessage::tool_result("y", "ry"),
        ];
        repair_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 5, "exactly one stub expected");
        let stub = msgs.last().unwrap();
        assert_eq!(stub.role, Role::Tool);
        assert_eq!(stub.tool_call_id.as_deref(), Some("z"));
        assert_eq!(stub.content.as_deref(), Some("[interrupted]"));
    }

    // ---- estimate_tokens ----

    #[test]
    fn estimate_tokens_in_messages_per_message_overhead() {
        // Even a message with no content should contribute at least 4 tokens
        // (the per-message overhead the implementation adds).
        use hrdr_llm::Role;
        let msg = ChatMessage {
            role: Role::User,
            content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let estimate = estimate_tokens_in_messages(&[msg]);
        assert!(
            estimate >= 4,
            "per-message overhead must be at least 4, got {estimate}"
        );
    }

    #[test]
    fn estimate_tokens_monotonic_with_content_length() {
        let short = estimate_tokens("hi");
        let long = estimate_tokens(&"word ".repeat(1000));
        assert!(long > short, "longer text must produce more tokens");
    }

    // ---- in_git_repo ----

    #[test]
    fn in_git_repo_detects_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Without .git: not a git repo.
        assert!(!in_git_repo(dir.path()));
        // With .git directory: detected.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(in_git_repo(dir.path()));
    }

    #[test]
    fn in_git_repo_detected_via_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("a").join("b");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        // A nested subdirectory should also be considered inside the repo.
        assert!(in_git_repo(&subdir));
    }

    // ---- cwd_slug ----

    #[test]
    fn cwd_slug_sanitizes_path() {
        assert_eq!(cwd_slug("/home/me/projects/foo"), "home-me-projects-foo");
        assert_eq!(cwd_slug("/"), "root");
        assert_eq!(cwd_slug("  "), "root");
        // Consecutive separators collapse to a single dash.
        assert_eq!(cwd_slug("a//b"), "a-b");
    }
}
