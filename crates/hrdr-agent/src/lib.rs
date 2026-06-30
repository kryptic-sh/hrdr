//! `hrdr-agent` — the agentic loop.
//!
//! Drives an OpenAI-compatible model through tool calls until a coding task is
//! complete: stream a turn, execute any requested tools, feed the results back,
//! repeat. Emits [`AgentEvent`]s for a UI (or stdout) to render live.

mod prompt;

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
        }
    }
}

/// A named, OpenAI-compatible model provider preset.
#[derive(Debug, Clone)]
pub struct Provider {
    /// OpenAI-compatible base URL (including the `/v1` suffix).
    pub base_url: &'static str,
    /// Environment variable holding the API key.
    pub key_env: &'static str,
    /// Remote provider — hrdr must NOT spawn a local backend for it.
    pub remote: bool,
}

/// Resolve a provider name (case-insensitive) to its preset.
///
/// - `zen` / `opencode` — OpenCode Zen gateway (`OPENCODE_API_KEY`).
/// - `openai` — OpenAI (`OPENAI_API_KEY`).
/// - `local` / `infr` — a local OpenAI-compatible server (spawned backend).
pub fn resolve_provider(name: &str) -> Option<Provider> {
    match name.trim().to_ascii_lowercase().as_str() {
        "zen" | "opencode" | "opencode-zen" => Some(Provider {
            base_url: "https://opencode.ai/zen/v1",
            key_env: "OPENCODE_API_KEY",
            remote: true,
        }),
        "openai" => Some(Provider {
            base_url: "https://api.openai.com/v1",
            key_env: "OPENAI_API_KEY",
            remote: true,
        }),
        "local" | "infr" => Some(Provider {
            base_url: "http://localhost:8080/v1",
            key_env: "HRDR_API_KEY",
            remote: false,
        }),
        _ => None,
    }
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
    use super::resolve_provider;

    #[test]
    fn zen_provider_is_remote_with_opencode_key() {
        let p = resolve_provider("ZEN").expect("zen resolves (case-insensitive)");
        assert_eq!(p.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(p.key_env, "OPENCODE_API_KEY");
        assert!(p.remote);
        assert!(resolve_provider("opencode").is_some());
    }

    #[test]
    fn local_provider_is_not_remote_and_unknown_is_none() {
        assert!(!resolve_provider("local").unwrap().remote);
        assert!(resolve_provider("nope").is_none());
    }
}
