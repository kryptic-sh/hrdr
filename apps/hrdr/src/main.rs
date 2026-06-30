//! `hrdr` — herder: an agentic coding harness.
//!
//! No subcommand launches the interactive TUI. `hrdr run <task>` runs a single
//! turn headlessly, streaming to stdout (scriptable, pipeable).
//! `hrdr models` lists available models from the configured endpoint.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hrdr_agent::{Agent, AgentConfig, AgentEvent};
use hrdr_llm::Client;

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
    if let Some(u) = cli.base_url {
        config.base_url = u;
    }
    if let Some(m) = cli.model {
        config.model = m;
    }

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
