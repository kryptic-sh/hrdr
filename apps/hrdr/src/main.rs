//! `hrdr` — herder: an agentic coding harness.
//!
//! No subcommand launches the interactive TUI. `hrdr run <task>` runs a single
//! turn headlessly, streaming to stdout (scriptable, pipeable).
//! `hrdr models` lists available models from the configured endpoint.
//!
//! By default hrdr spawns a local `llama-server` backend (see [`backend`] —
//! a TEMPORARY stopgap until infr's tool-calling serve path lands). Pass
//! `--no-backend` to use an already-running endpoint at `--base-url`.

mod backend;

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hrdr_agent::{Agent, AgentConfig, AgentEvent, resolve_provider};
use hrdr_llm::Client;

use backend::{Backend, BackendConfig};

#[derive(Parser)]
#[command(
    name = "hrdr",
    version,
    about = "hrdr — herder: a fast, agentic coding harness for OpenAI-compatible models.",
    before_help = include_str!("../art.txt"),
)]
struct Cli {
    /// OpenAI-compatible base URL (default: $HRDR_BASE_URL or http://localhost:8080/v1).
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Model id (default: $HRDR_MODEL).
    #[arg(long, global = true)]
    model: Option<String>,

    /// Provider preset: zen (OpenCode Zen), openai, or local. Sets the endpoint,
    /// API key env, and (for remote providers) skips the local backend.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Use vim keybindings in the input pane (default: plain claude-style input).
    #[arg(long, global = true)]
    vim: bool,

    /// Path to an hjkl theme TOML for the TUI (default: bundled dark theme).
    #[arg(long, global = true)]
    theme: Option<String>,

    /// Don't spawn a local llama-server; use the endpoint at --base-url.
    #[arg(long, global = true)]
    no_backend: bool,

    /// [temporary] Model ref (HF org/repo:quant or .gguf path) for the spawned backend.
    #[arg(long, global = true)]
    backend_model: Option<String>,

    /// [temporary] llama-server binary to spawn (default: llama-server).
    #[arg(long, global = true)]
    backend_bin: Option<String>,

    /// [temporary] Context window size for the spawned backend.
    #[arg(long, global = true)]
    backend_ctx: Option<u32>,

    /// [temporary] Extra arg passed verbatim to llama-server (repeatable), e.g. --backend-arg=-ngl --backend-arg=99.
    #[arg(long = "backend-arg", global = true)]
    backend_args: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single task to completion headlessly, streaming output to stdout.
    Run {
        /// The task prompt (all trailing words are joined).
        #[arg(trailing_var_arg = true, required = true)]
        prompt: Vec<String>,
    },
    /// List available models from the configured endpoint.
    Models,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    // Precedence: CLI flag > env var > config file > built-in default.
    let mut config = AgentConfig::load();

    // Apply a provider preset (CLI > config/env) before explicit CLI overrides.
    let mut remote_provider = false;
    let provider_name = cli.provider.clone().or_else(|| config.provider.clone());
    if let Some(name) = &provider_name {
        let p = resolve_provider(name).ok_or_else(|| {
            anyhow::anyhow!("unknown provider '{name}' (known: zen, openai, local)")
        })?;
        // Provider sets the endpoint unless an explicit --base-url / $HRDR_BASE_URL wins.
        let base_overridden = cli.base_url.is_some() || std::env::var_os("HRDR_BASE_URL").is_some();
        if !base_overridden {
            config.base_url = p.base_url.to_string();
        }
        if let Ok(key) = std::env::var(p.key_env) {
            config.api_key = Some(key);
        } else if p.remote && config.api_key.is_none() {
            eprintln!(
                "hrdr: provider '{name}' needs an API key — set ${}",
                p.key_env
            );
        }
        remote_provider = p.remote;
    }

    if let Some(u) = cli.base_url {
        config.base_url = u;
    }
    if let Some(m) = cli.model {
        config.model = m;
    }
    if cli.vim {
        config.vim_mode = true;
    }
    if let Some(t) = cli.theme {
        config.theme = Some(t);
    }

    if remote_provider && config.model == "default" {
        eprintln!(
            "hrdr: set a model with --model (run `hrdr models` to list this provider's models)"
        );
    }

    // TEMPORARY: bring up a local llama-server backend unless told not to. Remote
    // providers never spawn one. Held for the command; dropping it kills the server.
    let _backend = if cli.no_backend || remote_provider {
        None
    } else {
        let mut bcfg = BackendConfig::default();
        if let Some(m) = cli.backend_model {
            bcfg.model = m;
        }
        if let Some(b) = cli.backend_bin {
            bcfg.bin = b;
        }
        if let Some(c) = cli.backend_ctx {
            bcfg.ctx = c;
        }
        bcfg.extra_args = cli.backend_args;
        Some(Backend::ensure(&bcfg, &config.base_url).await?)
    };

    match cli.command {
        Some(Command::Run { prompt }) => run_headless(config, prompt.join(" ")).await,
        Some(Command::Models) => list_models(config).await,
        None => hrdr_tui::run(config).await,
    }
}

/// Headless single-turn run: stream events to stdout.
async fn run_headless(config: AgentConfig, prompt: String) -> Result<()> {
    let mut agent = Agent::new(config)?;
    agent
        .run(prompt, |ev| match ev {
            AgentEvent::Text(t) => {
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolStart { name, args, .. } => {
                eprintln!("\x1b[33m⚙ {name}\x1b[0m {}", truncate_inline(&args, 120));
            }
            AgentEvent::ToolEnd { name, ok, .. } => {
                let mark = if ok {
                    "\x1b[32m✓\x1b[0m"
                } else {
                    "\x1b[31m✗\x1b[0m"
                };
                eprintln!("{mark} {name}");
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                eprintln!("\x1b[90m[usage] ctx {prompt_tokens} · out {completion_tokens}\x1b[0m");
            }
            AgentEvent::TurnDone => println!(),
        })
        .await?;
    Ok(())
}

/// Print available model ids, one per line.
async fn list_models(config: AgentConfig) -> Result<()> {
    let client = Client::new(config.base_url, config.api_key, config.model);
    let models = client.list_models().await?;
    for m in models {
        println!("{m}");
    }
    Ok(())
}

fn truncate_inline(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}…")
    }
}
