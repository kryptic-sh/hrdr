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
    /// User-defined providers from `[providers.<name>]` in config, keyed by name.
    pub providers: HashMap<String, ProviderConfig>,
}

/// Default auto-compaction trigger: 85% of the context window (leaves headroom
/// so the next turn doesn't overflow).
pub const DEFAULT_AUTO_COMPACT: f64 = 0.85;

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
    /// Build from `HRDR_BASE_URL`, `HRDR_MODEL`, `HRDR_API_KEY` (falling back to
    /// `OPENAI_API_KEY`), defaulting to a local infr endpoint.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("HRDR_BASE_URL") {
            cfg.base_url = v;
        }
        if let Ok(v) = std::env::var("HRDR_MODEL") {
            cfg.model = v;
        }
        cfg.api_key = std::env::var("HRDR_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        cfg
    }

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
        if !fc.providers.is_empty() {
            self.providers = fc.providers;
        }
    }

    /// Layer environment variables over the current config.
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("HRDR_PROVIDER") {
            self.provider = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_THEME") {
            self.theme = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_BASE_URL") {
            self.base_url = v;
        }
        if let Ok(v) = std::env::var("HRDR_MODEL") {
            self.model = v;
        }
        if let Ok(v) = std::env::var("HRDR_AUTO_COMPACT")
            && let Ok(f) = v.parse::<f64>()
        {
            self.auto_compact = f;
        }
        if let Ok(v) = std::env::var("HRDR_AUTO_RESUME") {
            match v.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => self.auto_resume = false,
                "1" | "true" | "on" | "yes" => self.auto_resume = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("HRDR_BELL") {
            match v.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => self.bell = false,
                "1" | "true" | "on" | "yes" => self.bell = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("HRDR_ICONS") {
            self.icons = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_TIMESTAMPS") {
            self.timestamps = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_STATUSBAR") {
            self.statusbar = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_CHECKPOINTS") {
            self.checkpoints = Some(v);
        }
        let env_key = std::env::var("HRDR_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        if env_key.is_some() {
            self.api_key = env_key;
        }
    }
}

/// A value to persist into the user config file.
pub enum ConfigValue<'a> {
    Str(&'a str),
    Bool(bool),
    Float(f64),
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

            if let Some(u) = &acc.usage {
                on_event(AgentEvent::Usage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                });
            }

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

/// Convenience: the role of the last assistant message, for callers inspecting
/// transcript state.
pub fn is_assistant(m: &ChatMessage) -> bool {
    m.role == Role::Assistant
}

#[cfg(test)]
mod tests {
    use super::{
        Agent, AgentConfig, ProviderConfig, builtin_provider, is_context_overflow, is_transient,
    };

    fn system_prompt(agent: &Agent) -> String {
        agent.messages()[0].content.clone().unwrap_or_default()
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
}
