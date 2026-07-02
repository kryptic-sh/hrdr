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
use hrdr_agent::{Agent, AgentConfig, AgentEvent};
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

    /// Reasoning-effort label shown in the status bar (e.g. low/medium/high).
    #[arg(long, global = true)]
    effort: Option<String>,

    /// Auto-compact trigger as a fraction of the context window (0.0–1.0; 0 disables).
    #[arg(long, global = true)]
    auto_compact: Option<f64>,

    /// Don't auto-resume the most recent session for the working directory.
    #[arg(long = "no-auto-resume", global = true)]
    no_auto_resume: bool,

    /// Don't ring the terminal bell when a turn finishes.
    #[arg(long = "no-bell", global = true)]
    no_bell: bool,

    /// Icon set for the TUI: nerd (default), unicode, or ascii.
    #[arg(long, global = true)]
    icons: Option<String>,

    /// Per-message timestamp style: none, relative (default), or exact.
    #[arg(long, global = true)]
    timestamps: Option<String>,

    /// Status-bar mode: none, truncate (default), or wrap.
    #[arg(long, global = true)]
    statusbar: Option<String>,

    /// File checkpointing: on, off, or auto (default; off inside a git repo).
    #[arg(long, global = true)]
    checkpoints: Option<String>,

    /// Turns a completed TODO item stays visible before it's pruned (default 5).
    #[arg(long, global = true)]
    todo_ttl: Option<u64>,

    /// Show the model's `<think>` reasoning: on/off/1/0 (default on).
    #[arg(long = "show-thinking", global = true, value_name = "on|off")]
    show_thinking: Option<String>,

    /// Don't spawn a local backend; use the endpoint at --base-url.
    #[arg(long, global = true)]
    no_backend: bool,

    /// Model ref (HF org/repo:quant or .gguf path) for the spawned backend.
    #[arg(long, global = true)]
    backend_model: Option<String>,

    /// llama.cpp fallback binary to spawn when infr isn't on PATH (default: llama-server).
    #[arg(long, global = true)]
    backend_bin: Option<String>,

    /// Context window size for the spawned backend (llama.cpp; display-only for infr).
    #[arg(long, global = true)]
    backend_ctx: Option<u32>,

    /// Extra arg passed verbatim to the llama.cpp fallback (repeatable), e.g. --backend-arg=-ngl --backend-arg=99.
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
    // Precedence: CLI flag > env var > config file > built-in default. Display
    // knobs live in UiConfig (hrdr-app); model/endpoint/loop knobs in
    // AgentConfig (hrdr-agent) — both read the same config.toml + HRDR_* vars.
    let mut config = AgentConfig::load();
    let mut ui = hrdr_app::UiConfig::load();

    // Apply a provider preset (CLI > config/env) before explicit CLI overrides.
    // Custom `[providers.<name>]` from config shadow the built-ins.
    let mut remote_provider = false;
    let provider_name = cli.provider.clone().or_else(|| config.provider.clone());
    if let Some(name) = &provider_name {
        let p = config.resolve_provider(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{name}' (built-ins: zen, openai, openrouter, claude, local; \
                 or define [providers.{name}] in config)"
            )
        })?;
        // Provider sets the endpoint unless an explicit --base-url / $HRDR_BASE_URL wins.
        let base_overridden = cli.base_url.is_some() || std::env::var_os("HRDR_BASE_URL").is_some();
        if !base_overridden {
            config.base_url = p.base_url.clone();
        }
        // Key: an inline key wins, else the provider's key_env.
        if let Some(key) = p.api_key.clone() {
            config.api_key = Some(key);
        } else if let Some(env) = &p.key_env {
            if let Ok(key) = std::env::var(env) {
                config.api_key = Some(key);
            } else if p.remote && config.api_key.is_none() {
                eprintln!("hrdr: provider '{name}' needs an API key — set ${env}");
            }
        }
        // Provider's default model, unless the user set one explicitly.
        let model_overridden = cli.model.is_some() || std::env::var_os("HRDR_MODEL").is_some();
        if !model_overridden && let Some(m) = p.model.clone() {
            config.model = m;
        }
        if config.context_window.is_none() {
            config.context_window = p.context_window;
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
        ui.vim_mode = true;
    }
    if let Some(t) = cli.theme {
        ui.theme = Some(t);
    }
    if let Some(e) = cli.effort {
        config.effort = Some(e);
    }
    if let Some(r) = cli.auto_compact {
        config.auto_compact = r;
    }
    if cli.no_auto_resume {
        ui.auto_resume = false;
    }
    if cli.no_bell {
        ui.bell = false;
    }
    if let Some(i) = cli.icons {
        ui.icons = Some(i);
    }
    if let Some(t) = cli.timestamps {
        ui.timestamps = Some(t);
    }
    if let Some(s) = cli.statusbar {
        ui.statusbar = Some(s);
    }
    if let Some(c) = cli.checkpoints {
        config.checkpoints = Some(c);
    }
    if let Some(n) = cli.todo_ttl {
        ui.todo_ttl = n;
    }
    if let Some(v) = cli
        .show_thinking
        .as_deref()
        .and_then(hrdr_agent::parse_env_bool)
    {
        ui.show_thinking = v;
    }

    if remote_provider && config.model == "default" {
        eprintln!(
            "hrdr: set a model with --model (run `hrdr models` to list this provider's models)"
        );
    }

    // Bring up a local backend unless told not to — infr if it's on PATH, else
    // a llama.cpp fallback. Remote providers never spawn one. Held for the
    // command; dropping it kills the server.
    let mut backend_ctx_fallback: Option<u32> = None;
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
        backend_ctx_fallback = Some(bcfg.ctx);
        Some(Backend::ensure(&bcfg, &config.base_url).await?)
    };

    // Resolve the context window (drives the status bar's "X of Y" + the
    // auto-compaction threshold). Precedence: explicit config/provider wins;
    // else ask the server (many OpenAI-compatible servers advertise it — vLLM's
    // `max_model_len`, llama.cpp's `/props` n_ctx, …); else the spawned backend's
    // configured ctx. Left unknown for a remote that advertises nothing.
    if config.context_window.is_none() {
        let probe = hrdr_llm::Client::new(
            config.base_url.clone(),
            config.api_key.clone(),
            config.model.clone(),
        );
        config.context_window = probe.context_window().await.or(backend_ctx_fallback);
    }

    match cli.command {
        Some(Command::Run { prompt }) => run_headless(config, prompt.join(" ")).await,
        Some(Command::Models) => list_models(config).await,
        None => hrdr_tui::run(config, ui).await,
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
                eprintln!(
                    "\x1b[33m⚙ {name}\x1b[0m {}",
                    hrdr_tools::truncate_inline(&args, 120)
                );
            }
            AgentEvent::ToolOutput { chunk, .. } => {
                eprint!("\x1b[90m{chunk}\x1b[0m");
                let _ = std::io::stderr().flush();
            }
            AgentEvent::Notice(text) => eprintln!("\x1b[90m[{text}]\x1b[0m"),
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
