//! `hrdr-agent` — the agentic loop.
//!
//! Drives an OpenAI-compatible model through tool calls until a coding task is
//! complete: stream a turn, execute any requested tools, feed the results back,
//! repeat. Emits [`AgentEvent`]s for a UI (or stdout) to render live.

mod prompt;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use hrdr_llm::{Accumulator, ChatMessage, Client, Role};
use hrdr_tools::{TodoItem, ToolContext, ToolRegistry};

pub use prompt::render_system;

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
    /// User-defined providers from `[providers.<name>]` in config, keyed by name.
    pub providers: HashMap<String, ProviderConfig>,
}

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

    /// Load config with precedence: env > `~/.config/hrdr/config.toml` > built-in defaults.
    /// Does NOT auto-write a config file when one is missing.
    pub fn load() -> Self {
        // Start with defaults, layer file, then layer env on top.
        let mut cfg = Self::default();

        // Layer 1: file config.
        if let Some(path) = config_path()
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(fc) = toml::from_str::<FileConfig>(&text)
        {
            if let Some(v) = fc.base_url {
                cfg.base_url = v;
            }
            if let Some(v) = fc.api_key {
                cfg.api_key = Some(v);
            }
            if let Some(v) = fc.model {
                cfg.model = v;
            }
            if let Some(v) = fc.temperature {
                cfg.temperature = Some(v);
            }
            if let Some(v) = fc.vim {
                cfg.vim_mode = v;
            }
            if let Some(v) = fc.provider {
                cfg.provider = Some(v);
            }
            if let Some(v) = fc.theme {
                cfg.theme = Some(v);
            }
            if let Some(v) = fc.context_window {
                cfg.context_window = Some(v);
            }
            if let Some(v) = fc.effort {
                cfg.effort = Some(v);
            }
            if !fc.providers.is_empty() {
                cfg.providers = fc.providers;
            }
        }

        // Layer 2: env vars override file.
        if let Ok(v) = std::env::var("HRDR_PROVIDER") {
            cfg.provider = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_THEME") {
            cfg.theme = Some(v);
        }
        if let Ok(v) = std::env::var("HRDR_BASE_URL") {
            cfg.base_url = v;
        }
        if let Ok(v) = std::env::var("HRDR_MODEL") {
            cfg.model = v;
        }
        let env_key = std::env::var("HRDR_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();
        if env_key.is_some() {
            cfg.api_key = env_key;
        }

        cfg
    }
}

/// A running agent: model client + tools + conversation state.
pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    ctx: ToolContext,
    messages: Vec<ChatMessage>,
    max_steps: usize,
}

impl Agent {
    /// Construct an agent, seeding the system prompt for the default tool set.
    pub fn new(config: AgentConfig) -> Result<Self> {
        let tools = ToolRegistry::with_defaults();
        let ctx = ToolContext::new(config.cwd.clone());
        let system = render_system(&tools, &config.cwd)?;

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
        })
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Reset the conversation, keeping only the system prompt.
    pub fn clear(&mut self) {
        self.messages.truncate(1);
    }

    /// Switch the model for subsequent turns.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.client.model = model.into();
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

    /// Run one user turn to completion, emitting events as it goes.
    pub async fn run<F>(&mut self, user_input: impl Into<String>, mut on_event: F) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        self.messages.push(ChatMessage::user(user_input.into()));
        let defs = self.tools.defs();

        for _ in 0..self.max_steps {
            // Stream one assistant turn, accumulating text + tool calls.
            let mut stream = self
                .client
                .chat_stream(self.messages.clone(), defs.clone())
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
                    .run_tool(&call.function.name, &call.function.arguments)
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

    async fn run_tool(&self, name: &str, raw_args: &str) -> Result<String> {
        let args: serde_json::Value = if raw_args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(raw_args)
                .map_err(|e| anyhow::anyhow!("invalid tool arguments JSON: {e}"))?
        };
        self.tools.execute(name, args, &self.ctx).await
    }
}

// Re-exports consumers need without reaching into sub-crates.
pub use hrdr_llm::ChatMessage as Message;
pub use hrdr_llm::Role as MessageRole;
pub use hrdr_tools::TodoItem as Todo;

/// Convenience: the role of the last assistant message, for callers inspecting
/// transcript state.
pub fn is_assistant(m: &ChatMessage) -> bool {
    m.role == Role::Assistant
}

#[cfg(test)]
mod tests {
    use super::{AgentConfig, ProviderConfig, builtin_provider};

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
