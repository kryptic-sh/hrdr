//! `hrdr` — herder: an agentic coding harness.
//!
//! No subcommand launches the interactive TUI. `hrdr run <task>` runs a single
//! turn headlessly, streaming to stdout (scriptable, pipeable).
//! `hrdr models` lists available models from the configured endpoint.
//!
//! hrdr talks to any running OpenAI-compatible endpoint; choose one with
//! `--base-url` or a `--provider` preset. It does not manage a model server —
//! start your own (infr, llama.cpp, vLLM, …) or point at a hosted provider.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hrdr_agent::{Agent, AgentConfig, AgentEvent};

/// The `hrdr` wordmark: printed above `--help`, and animated in the TUI's
/// session header (passed to [`hrdr_tui::run`], which embeds no art of its own).
const LOGO_ART: &str = include_str!("../art.txt");

#[derive(Parser)]
#[command(
    name = "hrdr",
    version,
    about = "hrdr — herder: a fast, agentic coding harness for OpenAI-compatible models.",
    before_help = LOGO_ART,
    // `hrdr run …` / `hrdr models` are subcommands; anything else trailing is a
    // command for the TUI to run at startup. They are mutually exclusive, so
    // `hrdr /model` can't be mistaken for a malformed subcommand invocation.
    args_conflicts_with_subcommands = true,
)]
struct Cli {
    /// OpenAI-compatible base URL (default: $HRDR_BASE_URL or http://localhost:8080/v1).
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Model id (default: $HRDR_MODEL).
    #[arg(long, global = true)]
    model: Option<String>,

    /// Provider preset: zen (OpenCode Zen), openai, or local. Sets the endpoint
    /// and API-key env.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Use vim keybindings in the input pane (default: plain claude-style input).
    #[arg(long, global = true)]
    vim: bool,

    /// Path to an hjkl theme TOML for the TUI (default: bundled dark theme).
    #[arg(long, global = true)]
    theme: Option<String>,

    /// Reasoning effort for reasoning models: minimal, low, medium, or high
    /// (sent as `reasoning_effort`; other values are status-bar labels only).
    #[arg(long, global = true)]
    effort: Option<String>,

    /// Model for delegated sub-agents (the `task` tool). Same provider as the
    /// main agent; defaults to the main model. E.g. Opus main + Sonnet subs.
    #[arg(long = "subagent-model", global = true)]
    subagent_model: Option<String>,

    /// Run the main agent AS a named agent (a built-in like `explore`/`plan`, a
    /// discovered `.claude`/`.opencode`/`.hrdr` agent file, or a `[[subagent]]`):
    /// adopt its system prompt, tool scope, model/provider, and knobs.
    #[arg(long = "agent", global = true, value_name = "NAME")]
    agent: Option<String>,

    /// Override the base memory directory (default `<XDG data>/memory`) — point
    /// hrdr at another tool's memory store. `projects/<cwd>/` + `global/` still
    /// apply beneath it.
    #[arg(long = "memory-dir", global = true, value_name = "DIR")]
    memory_dir: Option<std::path::PathBuf>,

    /// Auto-compact on/off toggle (the trigger point is set by
    /// --compaction-reserved). Accepts `true`/`false` and, for backward
    /// compatibility, the old fractional spelling (`0.85` → on, `0` → off).
    #[arg(long, global = true)]
    auto_compact: Option<String>,

    /// Tokens reserved below the context window before auto-compaction fires
    /// (default 20000); compaction triggers at context_window − this.
    #[arg(long, global = true)]
    compaction_reserved: Option<u32>,

    /// Most read-only sub-agents that may run at once (default 5).
    #[arg(long, global = true, value_name = "N")]
    max_readonly_subagents: Option<usize>,

    /// Most write-capable sub-agents that may run at once (default 2) — they
    /// share the working tree, so interleaved edits race.
    #[arg(long, global = true, value_name = "N")]
    max_write_subagents: Option<usize>,

    /// Prune old tool output from the model context before each request (on|off; default on).
    #[arg(long = "auto-prune", global = true, value_name = "on|off")]
    auto_prune: Option<String>,

    /// Prompt caching: off, on, or auto (default; on for remote endpoints).
    #[arg(long = "prompt-cache", global = true, value_name = "off|on|auto")]
    prompt_cache: Option<String>,

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

    /// Print shell completions to stdout and exit
    #[arg(long, value_enum, value_name = "SHELL", hide = true)]
    completions: Option<CompletionShell>,

    /// Print the man page (troff) to stdout and exit
    #[arg(long, hide = true)]
    man: bool,

    #[command(subcommand)]
    command: Option<Command>,

    /// A command to run in the TUI as soon as it starts, exactly as if you had
    /// typed it into the input box: a slash command (`hrdr /new`, `hrdr /model`),
    /// a skill (`hrdr :review src/lib.rs`), a shell escape (`hrdr '!git status'`),
    /// or a plain message to open the session with. Put flags *before* it — every
    /// word after it is part of the command.
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    input: Vec<String>,
}

/// Shells `--completions` can generate for: clap_complete's five core shells
/// plus nushell (separate generator crate). Mirrors gpur's packaging helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
}

impl CompletionShell {
    fn generate(self, cmd: &mut clap::Command) {
        use clap_complete::Shell;
        let out = &mut std::io::stdout();
        match self {
            CompletionShell::Bash => clap_complete::generate(Shell::Bash, cmd, "hrdr", out),
            CompletionShell::Zsh => clap_complete::generate(Shell::Zsh, cmd, "hrdr", out),
            CompletionShell::Fish => clap_complete::generate(Shell::Fish, cmd, "hrdr", out),
            CompletionShell::Powershell => {
                clap_complete::generate(Shell::PowerShell, cmd, "hrdr", out)
            }
            CompletionShell::Elvish => clap_complete::generate(Shell::Elvish, cmd, "hrdr", out),
            CompletionShell::Nushell => {
                clap_complete::generate(clap_complete_nushell::Nushell, cmd, "hrdr", out)
            }
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run a single task to completion headlessly, streaming output to stdout.
    Run {
        /// Emit newline-delimited JSON events on stdout (for scripting/CI).
        #[arg(long)]
        json: bool,
        /// Suppress the tool/usage chrome on stderr; print only the reply text.
        #[arg(long)]
        quiet: bool,
        /// Override the tool-round budget for this run.
        #[arg(long, value_name = "N")]
        max_steps: Option<usize>,
        /// Stop before the next model call once the estimated session spend
        /// (USD, incl. sub-agents; priced from the models.dev catalog)
        /// reaches this cap.
        #[arg(long, value_name = "USD")]
        max_cost: Option<f64>,
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

    // Packaging helpers (hidden): emit completions / man page and exit.
    if let Some(shell) = cli.completions {
        use clap::CommandFactory;
        shell.generate(&mut Cli::command());
        return Ok(());
    }
    if cli.man {
        use clap::CommandFactory;
        clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        return Ok(());
    }

    // Precedence: CLI flag > env var > config file > built-in default. Display
    // knobs live in UiConfig (hrdr-app); model/endpoint/loop knobs in
    // AgentConfig (hrdr-agent) — both read the same config.toml + HRDR_* vars.
    let mut config = AgentConfig::load();
    let mut ui = hrdr_app::UiConfig::load();

    // ── The identity edge ───────────────────────────────────────────────────
    // config.toml, the environment and the CLI each spell the model identity as
    // TWO keys (`provider = …`, `model = …`). This is the only place that is true:
    // the halves are layered by precedence here, collapsed into one `ModelRef`, and
    // the core never sees a half again.
    let mut remote_provider = false;
    // Last-used fallback: when nothing names a provider/model, resume the identity
    // the user last switched to (recorded by the `/model` selector and `/login`).
    let last_used = hrdr_agent::load_last_model();
    // What config.toml + $HRDR_PROVIDER/$HRDR_MODEL named (`AgentConfig::load` has
    // already collapsed these into `config.model`; the startup precedence below
    // needs to know which halves were actually NAMED, and by whom).
    let named = hrdr_agent::named_model();
    let config_provider = named.provider.clone();
    let config_had_provider = named.provider.is_some();
    let config_had_model = named.model.is_some();
    let provider_name = cli
        .provider
        .clone()
        .or_else(|| named.provider.clone())
        .or_else(|| last_used.as_ref().map(|r| r.provider().to_string()));
    // A `model` in config.toml belongs to the provider config.toml names. It must
    // NOT follow the user onto a provider they switched to — `model = "sonnet"`
    // plus `hrdr --provider chatgpt` would otherwise suppress the ChatGPT preset's
    // default and send a Claude model id to the Codex endpoint. A config model
    // with no config provider is a global default, and so still yields to an
    // explicit `--provider`.
    let config_model_applies = config_had_model
        && match (&config_provider, &provider_name) {
            (Some(cp), Some(n)) => cp.eq_ignore_ascii_case(n),
            (Some(_), None) => true,
            (None, _) => cli.provider.is_none(),
        };
    let model_overridden = cli.model.is_some() || std::env::var_os("HRDR_MODEL").is_some();
    // The model half, settled: a flag/env or an applicable config model, else the
    // model the provider ITSELF answers with (the last one used on it, else one it
    // declares — `model_for_provider`), else the `default` sentinel.
    let mut identity = config.model.clone();
    if let Some(name) = &provider_name {
        let provider = hrdr_agent::ProviderName::new(name);
        let p = config.resolve_provider(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{name}' (built-ins: zen, openai, openrouter, claude, local; \
                 or define [providers.{name}] in config)"
            )
        })?;
        let model = if model_overridden || config_model_applies {
            // Whatever the flag/env/config named, on this provider.
            config.model.model().to_string()
        } else {
            // The provider is named but no model is: never carry the old one over.
            // The fallback chain answers, or nothing does — in which case we launch
            // on the `default` sentinel and say so below, exactly as before.
            hrdr_agent::model_for_resolved_provider(&provider, &p)
                .map(|r| r.model().to_string())
                .unwrap_or_else(|_| hrdr_agent::DEFAULT_MODEL.to_string())
        };
        identity = hrdr_agent::ModelRef::new(provider, &model)?;

        // Provider sets the endpoint unless an explicit --base-url / $HRDR_BASE_URL wins.
        let base_overridden = cli.base_url.is_some() || std::env::var_os("HRDR_BASE_URL").is_some();
        if !base_overridden {
            config.base_url = p.base_url.clone();
        }
        // Key precedence: inline > key_env var > credential saved by `/login`.
        // Unified readiness folds in trusted ChatGPT OAuth: a built-in ChatGPT
        // login with usable/refreshable credentials is `OAuth` (no key), so it
        // must not draw the missing-key warning. Only a genuinely unconfigured
        // remote provider (`Missing`) warns; the copy is unchanged.
        let auth_state = hrdr_agent::provider_auth_state(name, &p, None, None);
        if let Some(key) = hrdr_agent::resolve_api_key(name, &p, None, None) {
            config.api_key = Some(key);
        } else if config.api_key.is_none() && auth_state == hrdr_agent::ProviderAuthState::Missing {
            let env = p.key_env.as_deref().unwrap_or("HRDR_API_KEY");
            eprintln!("hrdr: provider '{name}' needs an API key — set ${env}, or run /login");
        }
        // Stamp the provider's flat preset — EXCEPT for the Codex endpoint, whose
        // preset is only right for its default model (gpt-5.5 = 272k) and would
        // over-state a smaller entitled model (a 128k codex model). Codex is
        // resolved per-model below, once the final model is known.
        if config.context_window.is_none() && p.base_url != hrdr_agent::CHATGPT_CODEX_BASE_URL {
            config.context_window = p.context_window;
        }
        config.headers = p.headers.into_iter().collect();
        config.api_version = p.api_version;
        remote_provider = p.remote;
    }

    // Restore the last-used identity as the final fallback: only when neither a
    // flag/env nor config named a provider *or* a model (the pure fresh case), so
    // the last-used identity beats a preset's default — whole, never half.
    if !model_overridden
        && !config_had_provider
        && !config_had_model
        && let Some(r) = &last_used
    {
        identity = r.clone();
    }

    if let Some(u) = cli.base_url {
        config.base_url = u;
    }
    // A `--model` / `--provider` flag outranks everything, including a resumed
    // session's (flag > env > session > config). `--model` is a bare model id here
    // (slice D teaches it `provider://model`), so it rides on the provider settled
    // above.
    // ONE pin for ONE identity: either flag names the thing this process runs on,
    // and a resumed session does not get to replace it.
    if cli.model.is_some() || cli.provider.is_some() {
        config.model_pinned = true;
    }
    if let Some(m) = cli.model {
        identity = hrdr_agent::ModelSpec::ModelOnly(m).apply(&identity);
    }
    config.model = identity;
    if cli.vim {
        ui.vim_mode = true;
    }
    if let Some(t) = cli.theme {
        ui.theme = Some(t);
    }
    if let Some(e) = cli.effort {
        config.effort = Some(e);
    }
    if let Some(m) = cli.subagent_model {
        config.subagent_model = Some(m);
    }
    if let Some(d) = cli.memory_dir {
        config.memory_dir = Some(d);
    }
    // `--agent NAME`: run the main loop AS that agent — adopt its prompt, tool
    // scope, model/provider, and knobs. Resolved from the same set as the `task`
    // tool (built-ins + discovered files + config), applied onto the main config
    // (delegation + MCP are kept, unlike a delegated sub-agent).
    if let Some(name) = cli.agent.as_deref() {
        let profiles = hrdr_agent::resolve_agent_profiles(&config);
        let profile = profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| {
                let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
                anyhow::anyhow!("unknown --agent '{name}' (available: {})", names.join(", "))
            })?;
        config = hrdr_agent::config_for_agent_profile(&config, profile)?;
    }
    if let Some(b) = cli
        .auto_compact
        .as_deref()
        .and_then(hrdr_agent::parse_toggle_or_num)
    {
        config.auto_compact = b;
    }
    if let Some(n) = cli.compaction_reserved {
        config.compaction_reserved = n;
    }
    if let Some(n) = cli.max_readonly_subagents {
        config.max_readonly_subagents = n;
    }
    if let Some(n) = cli.max_write_subagents {
        config.max_write_subagents = n;
    }
    if let Some(v) = cli
        .auto_prune
        .as_deref()
        .and_then(hrdr_agent::parse_env_bool)
    {
        config.auto_prune = v;
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
    if let Some(p) = cli.prompt_cache {
        config.prompt_cache = Some(p);
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

    if remote_provider && config.has_default_model() {
        eprintln!(
            "hrdr: set a model with --model (run `hrdr models` to list this provider's models)"
        );
    }

    // Resolve the context window (drives the status bar's "X of Y" + the
    // auto-compaction threshold). Precedence: explicit config/provider wins;
    // else ask the server (many OpenAI-compatible servers advertise it — vLLM's
    // `max_model_len`, llama.cpp's `/props` n_ctx, …). Left unknown for an
    // endpoint that advertises nothing.
    //
    // A 3-second timeout prevents a firewall-DROPped endpoint from hanging
    // startup forever before the TUI appears. Timeout ≡ no context window known.
    if config.context_window.is_none() {
        if config.base_url == hrdr_agent::CHATGPT_CODEX_BASE_URL {
            // The Codex endpoint 401s on `/v1/models`, so the server probe can't
            // read it. Resolve per-model from the account catalog cache instead —
            // now that the final model is known — falling back to the preset floor.
            config.context_window = hrdr_agent::context_window_for(
                Some(config.model.provider().as_str()),
                &config.base_url,
                config.model.model(),
            );
        } else {
            let probe = hrdr_llm::Client::new(
                config.base_url.clone(),
                config.api_key.clone(),
                config.model.model().to_string(),
            );
            config.context_window =
                tokio::time::timeout(Duration::from_secs(3), probe.context_window())
                    .await
                    .ok()
                    .flatten();
        }
    }

    match cli.command {
        Some(Command::Run {
            json,
            quiet,
            max_steps,
            max_cost,
            prompt,
        }) => {
            if let Some(n) = max_steps {
                config.max_steps = n;
            }
            if max_cost.is_some() {
                config.max_cost = max_cost;
            }
            run_headless(config, prompt.join(" "), json, quiet).await
        }
        Some(Command::Models) => list_models(config).await,
        // Trailing words are a command for the TUI to run at startup — the same
        // line the input box would take. Joined, so `hrdr /model gpt-5` and
        // `hrdr "/model gpt-5"` mean the same thing.
        None => {
            let command = (!cli.input.is_empty()).then(|| cli.input.join(" "));
            hrdr_tui::run(config, ui, LOGO_ART, command).await
        }
    }
}

/// Headless single-turn run. Default: reply text on stdout, tool/usage chrome
/// on stderr. `--json`: newline-delimited JSON events on stdout (scripting).
/// `--quiet`: text only. Exit code 0 on a completed turn, 1 on error.
async fn run_headless(config: AgentConfig, prompt: String, json: bool, quiet: bool) -> Result<()> {
    let mut agent = Agent::new(config)?;
    // Prepare the outgoing prompt: expand `@file` mentions and route any
    // `@agent` mention to the matching sub-agent (parity with the TUI).
    let prompt = hrdr_app::prepare_outgoing(&prompt, agent.agent_names(), &agent.cwd());
    // Connect any configured MCP servers before the turn (their tools join the
    // set); surface the per-server status on stderr unless quiet.
    for notice in agent.connect_mcp().await {
        if !quiet {
            eprintln!("\x1b[90m[{notice}]\x1b[0m");
        }
    }
    // A headless run is a one-turn session: session hooks bracket the turn.
    for note in agent
        .run_session_hooks(hrdr_tools::HookEvent::SessionStart)
        .await
    {
        if !quiet {
            eprintln!("\x1b[90m[{note}]\x1b[0m");
        }
    }
    // Headless runs have no interactive steering.
    let result = agent
        .run(prompt, hrdr_agent::steering_queue(), |ev| {
            if json {
                println!("{}", event_json(&ev));
                let _ = std::io::stdout().flush();
                return;
            }
            match ev {
                AgentEvent::Text(t) => {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::Reasoning(_) => {}
                AgentEvent::ToolStart { name, args, .. } if !quiet => {
                    eprintln!(
                        "\x1b[33m⚙ {name}\x1b[0m {}",
                        hrdr_tools::truncate_inline(&args, 120)
                    );
                }
                AgentEvent::ToolOutput { chunk, .. } if !quiet => {
                    eprint!("\x1b[90m{chunk}\x1b[0m");
                    let _ = std::io::stderr().flush();
                }
                AgentEvent::Notice(text) if !quiet => eprintln!("\x1b[90m[{text}]\x1b[0m"),
                AgentEvent::ToolEnd { name, ok, .. } if !quiet => {
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
                    cached_prompt_tokens,
                    reasoning_tokens,
                    session_cost_usd,
                    ..
                } if !quiet => {
                    let cached = cached_prompt_tokens
                        .map(|c| format!(" ({c} cached)"))
                        .unwrap_or_default();
                    let reasoning = reasoning_tokens
                        .map(|r| format!(" · reasoning {r}"))
                        .unwrap_or_default();
                    let cost = session_cost_usd
                        .map(|c| format!(" · est. {}", hrdr_app::fmt_cost(c)))
                        .unwrap_or_default();
                    eprintln!(
                        "\x1b[90m[usage] ctx {prompt_tokens}{cached} · out {completion_tokens}{reasoning}{cost}\x1b[0m"
                    );
                }
                AgentEvent::TurnDone => println!(),
                _ => {}
            }
        })
        .await;
    for note in agent
        .run_session_hooks(hrdr_tools::HookEvent::SessionEnd)
        .await
    {
        if !quiet {
            eprintln!("\x1b[90m[{note}]\x1b[0m");
        }
    }
    if let Err(e) = result {
        if json {
            println!(
                "{}",
                serde_json::json!({"type": "error", "message": e.to_string()})
            );
        }
        return Err(e);
    }
    Ok(())
}

/// One [`AgentEvent`] as a single-line JSON object (`hrdr run --json`).
fn event_json(ev: &AgentEvent) -> String {
    use serde_json::json;
    let v = match ev {
        AgentEvent::Text(t) => json!({"type": "text", "text": t}),
        AgentEvent::Reasoning(t) => json!({"type": "reasoning", "text": t}),
        AgentEvent::ToolStart { id, name, args } => {
            json!({"type": "tool_start", "id": id, "name": name, "args": args})
        }
        AgentEvent::ToolOutput { id, chunk } => {
            json!({"type": "tool_output", "id": id, "chunk": chunk})
        }
        AgentEvent::ToolEnd {
            id,
            name,
            result,
            ok,
        } => {
            json!({"type": "tool_end", "id": id, "name": name, "ok": ok, "result": result})
        }
        AgentEvent::History(msgs) => json!({"type": "history", "messages": msgs.len()}),
        AgentEvent::Notice(text) => json!({"type": "notice", "text": text}),
        AgentEvent::Steered(text) => json!({"type": "steer", "text": text}),
        AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            reasoning_tokens,
            cost_usd,
            session_cost_usd,
        } => {
            json!({
                "type": "usage",
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "cached_prompt_tokens": cached_prompt_tokens,
                "reasoning_tokens": reasoning_tokens,
                "cost_usd": cost_usd,
                "session_cost_usd": session_cost_usd,
            })
        }
        AgentEvent::TurnDone => json!({"type": "done"}),
    };
    v.to_string()
}

/// Print available model ids, one per line.
async fn list_models(config: AgentConfig) -> Result<()> {
    let models = hrdr_agent::list_provider_models(&config).await?;
    for m in models {
        println!("{m}");
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    /// A trailing command reaches the TUI as one line, whatever its syntax.
    ///
    /// `hrdr /new`, `hrdr /model`, `hrdr :review …`, `hrdr '!git status'` — none of
    /// these are subcommands, and none of them should be *mistaken* for one. They
    /// are the line the input box would have taken, handed over before the first
    /// frame.
    #[test]
    fn a_trailing_command_is_collected_for_the_tui() {
        for (argv, want) in [
            (vec!["hrdr", "/new"], "/new"),
            (vec!["hrdr", "/model"], "/model"),
            // Unquoted words after the command are part of it: `hrdr /model gpt-5`
            // must mean what `hrdr "/model gpt-5"` means.
            (vec!["hrdr", "/model", "gpt-5"], "/model gpt-5"),
            (vec!["hrdr", ":review", "src/lib.rs"], ":review src/lib.rs"),
            (vec!["hrdr", "!git status"], "!git status"),
            (vec!["hrdr", "fix the failing test"], "fix the failing test"),
        ] {
            let cli = Cli::parse_from(&argv);
            assert!(cli.command.is_none(), "{argv:?} is not a subcommand");
            assert_eq!(cli.input.join(" "), want, "{argv:?}");
        }
    }

    /// Flags still bind to hrdr, not to the command — as long as they come first.
    #[test]
    fn flags_before_the_command_still_reach_hrdr() {
        let cli = Cli::parse_from(["hrdr", "--provider", "zen", "--vim", "/model"]);
        assert_eq!(cli.provider.as_deref(), Some("zen"));
        assert!(cli.vim);
        assert_eq!(cli.input.join(" "), "/model");
    }

    /// The subcommands still win: adding a trailing command must not have turned
    /// `hrdr run …` or `hrdr models` into TUI input.
    #[test]
    fn subcommands_are_not_swallowed_by_the_trailing_command() {
        let cli = Cli::parse_from(["hrdr", "run", "fix", "the", "bug"]);
        match cli.command {
            Some(Command::Run { prompt, .. }) => assert_eq!(prompt.join(" "), "fix the bug"),
            _ => panic!("`hrdr run` must still be the run subcommand"),
        }
        assert!(cli.input.is_empty());

        let cli = Cli::parse_from(["hrdr", "models"]);
        assert!(matches!(cli.command, Some(Command::Models)));
    }

    /// No command → nothing to run at startup (the plain TUI).
    #[test]
    fn no_command_is_no_startup_input() {
        let cli = Cli::parse_from(["hrdr"]);
        assert!(cli.command.is_none());
        assert!(cli.input.is_empty());
    }
}
