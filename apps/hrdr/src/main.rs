//! `hrdr` — herder: an agentic coding harness.
//!
//! No subcommand launches the interactive TUI. `hrdr run <task>` runs a single
//! turn headlessly, streaming to stdout (scriptable, pipeable).
//! `hrdr models` lists available models from the configured endpoint.
//!
//! hrdr talks to any running OpenAI-compatible endpoint; name the model you want
//! as `provider://model` (`--model chatgpt://gpt-5.5`), or point `--base-url` at a
//! server you run. It does not manage a model server — start your own (infr,
//! llama.cpp, vLLM, …) or point at a hosted provider.

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

    /// The model to run, as `provider://model` (`chatgpt://gpt-5.5`,
    /// `openrouter://deepseek/deepseek-chat`) — which also sets the provider's
    /// endpoint and key — or a bare model id (`gpt-5.5`), which is that model on
    /// the provider already in effect. Default: $HRDR_MODEL.
    #[arg(long, global = true, value_name = "PROVIDER://MODEL|MODEL")]
    model: Option<String>,

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

    /// Model for delegated sub-agents (the `task` tool), as `provider://model` or
    /// a bare id (the main agent's provider, a cheaper model — Opus main + Sonnet
    /// subs). Defaults to the main model.
    #[arg(
        long = "subagent-model",
        global = true,
        value_name = "PROVIDER://MODEL|MODEL"
    )]
    subagent_model: Option<String>,

    /// Run the main agent AS a named agent (a built-in like `explore`/`plan`, a
    /// discovered `.claude`/`.opencode`/`.hrdr` agent file, or a `[[subagent]]`):
    /// adopt its system prompt, tool scope, model, and knobs.
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

/// The identity this process runs on, from the sources that can name it.
///
/// `specs` are the `ModelSpec`s the sources named, **lowest precedence first**
/// (config.toml, `$HRDR_MODEL`, `--model`), each applied onto what the layer below
/// settled — so a bare model id always means "that model, on the provider already in
/// effect", whichever layer wrote it.
///
/// The provider in effect *before any of them* is the store's last-used identity: what
/// the user last switched to interactively (the `/model` picker, `/login`). That is the
/// launch fallback.
///
/// **THE INTERACTIVE POLICY** for a `provider://` spec (`hrdr --model 'openai://'`,
/// `model = "openai://"`) lives here too: [`hrdr_agent::model_for_provider_in`] — the
/// model you last used on THAT provider, else the one it declares, else an error naming
/// the fix. This is the launch edge, where "carry on with what I was using" is precisely
/// what the user means.
///
/// A delegation never gets this policy (see `strict_spec_ref` in `hrdr-agent`): a `task`
/// must resolve identically on every machine and in CI, so it reads no store — which is
/// why `ModelSpec::apply` refuses to answer for `ProviderOnly` at all, and each caller
/// has to say which policy it wants.
fn settle_identity(
    store: &hrdr_agent::LastModels,
    specs: &[hrdr_agent::ModelSpec],
    config: &AgentConfig,
) -> Result<hrdr_agent::ModelRef> {
    let mut identity = store.last.clone().unwrap_or_else(|| {
        hrdr_agent::DEFAULT_MODEL_REF
            .parse()
            .expect("a valid default identity")
    });
    for spec in specs {
        identity = match spec.apply(&identity) {
            Some(r) => r,
            // `provider://` — the interactive chain answers, or nobody does.
            None => {
                let provider = spec.provider().expect("ProviderOnly names a provider");
                hrdr_agent::model_for_provider_in(store, provider, config)?
            }
        };
    }
    Ok(identity)
}

/// The endpoint to talk to: the provider's `preset` URL, unless something
/// **relocates** it.
///
/// `flag` (`--base-url` / `$HRDR_BASE_URL`) always wins — it is this run's decision.
/// `file` (a free-floating `base_url =` in config.toml) is the endpoint of whatever
/// provider the user wrote it for, so a provider named anywhere else
/// (`provider_named`) supersedes it. A relocation keeps the identity: it is still that
/// provider, at another address.
fn settle_base_url(
    flag: Option<String>,
    file: Option<String>,
    preset: &str,
    provider_named: bool,
) -> String {
    flag.or_else(|| file.filter(|_| !provider_named))
        .unwrap_or_else(|| preset.to_string())
}

/// The startup gate: **refuse what we KNOW is wrong, warn about what looks wrong.**
///
/// Three questions, asked of the settled identity, in the order they can be
/// answered:
///
/// 1. **Is the model real?** ([`hrdr_agent::validate_identity`], then
///    [`hrdr_agent::confirm_identity`]) — the ChatGPT account catalog is the account's
///    own entitlement list, and the only thing allowed to refuse. A *cached* copy of
///    it may only prove PRESENCE (an entitlement list grows, so a stale absence proves
///    nothing) — so an absence is confirmed against a freshly fetched list before
///    anyone is refused, and a fetch that fails warns instead of blocking. models.dev
///    lags every release, so its silence is only ever a warning. Network-free unless
///    hrdr is about to refuse.
/// 2. **Did `--base-url` change something invisible?**
///    ([`hrdr_agent::relocation_warnings`]) — the wire protocol follows the host, and
///    the API key follows the URL. Both are legitimate for a proxy; both are worth
///    saying out loud. Network-free.
/// 3. **Does `default` still mean anything here?**
///    ([`hrdr_agent::validate_placeholder_model`]) — it is a placeholder for "whatever
///    you are serving", true only of a server with nothing to name. This is the one
///    question that needs the wire, and it is asked only when the model IS `default`,
///    so no other run pays for it. A failed probe FAILS OPEN: refusing a session over
///    a network blip would be hostile, and the unreachable-endpoint warning covers it.
///
/// `Err` exits non-zero; warnings go to stderr, as the missing-key notice already does.
///
/// `listing` is `hrdr models` — the command whose entire job is to answer "what may I
/// name?". Refusing it for not having named one would be a closed loop, so it is
/// exempt from (3) alone; the identity checks still run.
async fn startup_checks(config: &AgentConfig, listing: bool) -> Result<()> {
    let resolved = hrdr_agent::ResolvedModel::from_config(config);
    let verdict = hrdr_agent::validate_identity(&resolved, config);
    for w in hrdr_agent::confirm_identity(verdict).await? {
        eprintln!("{w}");
    }
    // The provider's OWN address — a relocation is only a relocation relative to
    // somewhere. Re-derived here (not captured earlier) because `--agent` may have
    // moved the identity onto another provider entirely.
    if let Some(canonical) = config.resolve_provider(resolved.reference().provider().as_str()) {
        for w in hrdr_agent::relocation_warnings(&resolved, &canonical.base_url) {
            eprintln!("{w}");
        }
    }
    if !listing && resolved.reference().model() == hrdr_agent::PLACEHOLDER_MODEL {
        let probe = hrdr_llm::Client::new(
            resolved.base_url().to_string(),
            resolved.api_key().map(str::to_string),
            hrdr_agent::PLACEHOLDER_MODEL.to_string(),
        );
        // Same 3s budget as the context-window probe: a firewall-DROPped endpoint
        // must not hold startup open, and a timeout is simply "we cannot know".
        let advertised = tokio::time::timeout(Duration::from_secs(3), probe.list_models())
            .await
            .ok()
            .and_then(Result::ok);
        hrdr_agent::validate_placeholder_model(resolved.reference(), advertised.as_deref())?;
    }
    Ok(())
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

    // A config still written in the old two-key form (`provider = …` beside
    // `model = …`) is refused outright: the pair could always disagree, and picking
    // one of a contradictory pair on the user's behalf is the whole class of bug
    // this design removes. Sessions migrate silently; config does not.
    if let Err(e) = hrdr_agent::check_config_compat() {
        eprintln!("{e}");
        std::process::exit(2);
    }

    // Precedence: CLI flag > env var > config file > built-in default. Display
    // knobs live in UiConfig (hrdr-app); model/endpoint/loop knobs in
    // AgentConfig (hrdr-agent) — both read the same config.toml + HRDR_* vars.
    let mut config = AgentConfig::load();
    let mut ui = hrdr_app::UiConfig::load();

    // ── The identity edge ───────────────────────────────────────────────────
    // config.toml, the environment and the CLI each name the model with ONE key —
    // `model = "provider://model"`, `$HRDR_MODEL`, `--model`. Each is a `ModelSpec`:
    // a `provider://model` names the whole identity, a bare id names a model on the
    // provider already in effect. They are layered here, lowest first, and what the
    // core sees is the one `ModelRef` they settle to.
    //
    // The provider "already in effect" at launch, when nothing above names one, is
    // the last-used identity (recorded by the `/model` picker and `/login`) — the
    // launch fallback, and the ONLY place startup consults that interactive store.
    // A delegation never does: it must resolve the same on every machine and in CI.
    let store = hrdr_agent::load_last_models();
    let last_used = store.last.clone();
    let cli_spec = cli
        .model
        .as_deref()
        .map(str::parse::<hrdr_agent::ModelSpec>)
        .transpose()
        .map_err(|e| anyhow::anyhow!("--model {}: {e}", cli.model.clone().unwrap_or_default()))?;
    let named_specs = hrdr_agent::named_model_specs();
    let specs: Vec<hrdr_agent::ModelSpec> =
        named_specs.iter().chain(cli_spec.iter()).cloned().collect();
    // `--model` / `$HRDR_MODEL` / config.toml settle the identity a NEW session
    // starts on — the default, not a pin. A session that already carries an
    // identity (it was resumed, or `/model` picked one) keeps its own: the model
    // and the provider are part of the conversation.
    let identity = settle_identity(&store, &specs, &config)?;

    // The endpoint the identity's provider resolves to — its key, headers and
    // api-version with it. A `--base-url` / `$HRDR_BASE_URL` **relocates** that
    // provider (it is still that provider, at another address); a bare `--base-url`
    // with nothing else named lands on `local`, which is exactly what `local` means:
    // an OpenAI-compatible server you run.
    let name = identity.provider().as_str().to_string();
    let p = config.resolve_provider(&name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{name}' (built-ins: {}; or define [providers.{name}] in config)",
            hrdr_agent::BUILTIN_PROVIDERS.join(", ")
        )
    })?;
    // A free-floating `base_url` in config.toml is the endpoint of whatever provider
    // the user wrote it for; a provider named ANYWHERE (a `provider://model` spec,
    // or the last-used identity) supersedes it, as it always has. A flag/env one
    // relocates unconditionally.
    let provider_named = last_used.is_some() || specs.iter().any(|s| s.provider().is_some());
    let flag_url = cli
        .base_url
        .clone()
        .or_else(|| std::env::var("HRDR_BASE_URL").ok());
    // `AgentConfig::load` has already layered config.toml's `base_url` (and
    // `$HRDR_BASE_URL`) into `config.base_url`; anything other than the default is a
    // value the user wrote somewhere.
    let file_url =
        (config.base_url != hrdr_agent::DEFAULT_BASE_URL).then(|| config.base_url.clone());
    // Remembered as the relocation it is: it moved THIS provider's endpoint. A
    // resume onto another provider leaves it behind, and says so.
    config.base_url_override = flag_url.clone();
    config.base_url = settle_base_url(flag_url, file_url, &p.base_url, provider_named);
    // Key precedence: inline > key_env var > credential saved by `/login`.
    // Unified readiness folds in trusted ChatGPT OAuth: a built-in ChatGPT login
    // with usable/refreshable credentials is `OAuth` (no key), so it must not draw
    // the missing-key warning. Only a genuinely unconfigured remote provider
    // (`Missing`) warns; the copy is unchanged.
    let auth_state = hrdr_agent::provider_auth_state(&name, &p, None, None);
    if let Some(key) = hrdr_agent::resolve_api_key(&name, &p, None, None) {
        config.api_key = Some(key);
    } else if config.api_key.is_none() && auth_state == hrdr_agent::ProviderAuthState::Missing {
        let env = p.key_env.as_deref().unwrap_or("HRDR_API_KEY");
        eprintln!("hrdr: provider '{name}' needs an API key — set ${env}, or run /login");
    }
    // Stamp the provider's flat preset — EXCEPT for the Codex endpoint, whose preset
    // is only right for its default model (gpt-5.5 = 272k) and would over-state a
    // smaller entitled model (a 128k codex model). Codex is resolved per-model below,
    // once the final model is known.
    if config.context_window.is_none() && p.base_url != hrdr_agent::CHATGPT_CODEX_BASE_URL {
        config.context_window = p.context_window;
    }
    config.headers = p.headers.into_iter().collect();
    config.api_version = p.api_version;
    let remote_provider = p.remote;

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
        config.subagent_model = Some(
            m.parse()
                .map_err(|e| anyhow::anyhow!("--subagent-model {m}: {e}"))?,
        );
    }
    if let Some(d) = cli.memory_dir {
        config.memory_dir = Some(d);
    }
    // `--agent NAME`: run the main loop AS that agent — adopt its prompt, tool
    // scope, model/provider, and knobs. Resolved from the same set as the `task`
    // tool (built-ins + discovered files + config), applied onto the main config
    // (delegation + MCP are kept, unlike a delegated sub-agent).
    if let Some(name) = cli.agent.as_deref() {
        let profiles = hrdr_agent::resolve_agent_profiles(&config)?;
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

    // ── Is the settled identity real? ───────────────────────────────────────
    // The identity is final here — every layer has spoken, `--agent` included — so
    // this is the first and only moment it can be checked as a whole. Everything
    // below is network-free except the one `default` probe, and nothing below
    // consults the interactive last-used store: validation is store-free, so a
    // `hrdr run` in CI validates exactly what it will send.
    let listing = matches!(cli.command, Some(Command::Models));
    if let Err(e) = startup_checks(&config, listing).await {
        eprintln!("hrdr: {e:#}");
        std::process::exit(2);
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
        let cli = Cli::parse_from(["hrdr", "--model", "zen://kimi-k2", "--vim", "/model"]);
        assert_eq!(cli.model.as_deref(), Some("zen://kimi-k2"));
        assert!(cli.vim);
        assert_eq!(cli.input.join(" "), "/model");
    }

    /// `--provider` is GONE: the provider is named in the model, or not at all.
    /// Passing it is an error, not a silently-ignored flag.
    #[test]
    fn the_provider_flag_no_longer_exists() {
        assert!(
            Cli::try_parse_from(["hrdr", "--provider", "zen"]).is_err(),
            "--provider must not parse — it is spelled `--model zen://<model>` now"
        );
    }

    /// `--model` takes a whole `provider://model` identity or a bare model id, and
    /// hands them to `ModelSpec` unchanged — `://` is the only separator, so a
    /// slashed or colon'd model id is never mistaken for a provider.
    #[test]
    fn the_model_flag_takes_a_spec_of_either_shape() {
        use hrdr_agent::{ModelRef, ModelSpec};
        let spec = |argv: [&str; 3]| -> ModelSpec {
            Cli::parse_from(argv)
                .model
                .expect("--model was passed")
                .parse()
                .expect("a valid spec")
        };
        let base: ModelRef = "zen://kimi-k2".parse().unwrap();

        // A URI sets the whole identity.
        let full = spec(["hrdr", "--model", "chatgpt://gpt-5.5"]);
        assert_eq!(full, ModelSpec::Full("chatgpt://gpt-5.5".parse().unwrap()));
        assert_eq!(
            full.apply(&base),
            Some("chatgpt://gpt-5.5".parse().unwrap())
        );

        // A bare id keeps the provider in effect — slashes and colons included.
        for (arg, want) in [
            ("gpt-5.5", "zen://gpt-5.5"),
            ("moonshotai/kimi-k2", "zen://moonshotai/kimi-k2"),
            ("llama3:8b", "zen://llama3:8b"),
        ] {
            let s = spec(["hrdr", "--model", arg]);
            assert!(matches!(s, ModelSpec::ModelOnly(_)), "{arg}");
            assert_eq!(s.apply(&base), Some(want.parse().unwrap()), "{arg}");
        }
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

    /// The launch identity, settled: a URI names the whole thing, a bare id rides on
    /// the provider already in effect, and nothing at all resumes the last-used one.
    #[test]
    fn the_model_spec_layers_settle_the_launch_identity() {
        use hrdr_agent::{LastModels, ModelRef, ModelSpec};
        let spec = |s: &str| s.parse::<ModelSpec>().unwrap();
        // The store, explicit — never the developer's real `last_model.json`.
        let store = |last: Option<&str>| LastModels {
            last: last.map(|s| s.parse::<ModelRef>().unwrap()),
            ..Default::default()
        };
        let cfg = AgentConfig::default();
        let got = |last: Option<&str>, specs: &[ModelSpec]| {
            settle_identity(&store(last), specs, &cfg)
                .expect("resolves")
                .to_string()
        };

        // `--model chatgpt://gpt-5.5` sets the WHOLE identity — it does not matter
        // what was in effect before.
        assert_eq!(
            got(Some("zen://kimi-k2"), &[spec("chatgpt://gpt-5.5")]),
            "chatgpt://gpt-5.5"
        );
        // `--model gpt-5.5` (bare) keeps the provider in effect.
        assert_eq!(
            got(Some("zen://kimi-k2"), &[spec("gpt-5.5")]),
            "zen://gpt-5.5"
        );
        // Nothing named: the last-used identity is resumed, whole.
        assert_eq!(got(Some("zen://kimi-k2"), &[]), "zen://kimi-k2");
        // Nothing named and nothing used: `local://default` — the server you run,
        // serving whatever it was started with. A bare `--base-url` run is this run.
        assert_eq!(got(None, &[]), hrdr_agent::DEFAULT_MODEL_REF);
        assert_eq!(
            settle_identity(&store(None), &[], &cfg)
                .unwrap()
                .provider()
                .as_str(),
            "local",
            "a bare --base-url run is a `local` run"
        );

        // The layers COMPOSE, lowest first: a config `openrouter://deepseek-chat`
        // under a `$HRDR_MODEL=kimi-k2` means kimi-k2 ON openrouter — a bare id never
        // drops the provider a lower layer named.
        assert_eq!(
            got(None, &[spec("openrouter://deepseek-chat"), spec("kimi-k2")]),
            "openrouter://kimi-k2"
        );
        // …and a URI at a higher layer replaces the lot.
        assert_eq!(
            got(
                Some("zen://kimi-k2"),
                &[spec("openrouter://deepseek-chat"), spec("local://qwen3")]
            ),
            "local://qwen3"
        );
    }

    /// `hrdr --model 'openai://'` — a provider named with NO model. This is the
    /// interactive edge, so it gets the interactive policy: the model you last used on
    /// THAT provider, else the one it declares, else an error naming the fix.
    ///
    /// Never the model you were using somewhere else: that one belongs to the provider
    /// you are leaving, and following you onto this one is the whole bug.
    #[test]
    fn a_provider_only_model_flag_resolves_through_the_interactive_chain() {
        use hrdr_agent::{LastModels, ModelRef, ModelSpec};
        let spec: ModelSpec = "openai://".parse().unwrap();
        let cfg = AgentConfig::default();

        // 1. The model last used ON OPENAI wins.
        let store = LastModels {
            last: Some("zen://kimi-k2".parse::<ModelRef>().unwrap()),
            by_provider: [("openai".to_string(), "gpt-5.1-codex".to_string())]
                .into_iter()
                .collect(),
        };
        assert_eq!(
            settle_identity(&store, std::slice::from_ref(&spec), &cfg)
                .unwrap()
                .to_string(),
            "openai://gpt-5.1-codex"
        );

        // 2. Nothing remembered on openai, and the preset declares no model → an
        //    error that names the fix. `kimi-k2` (the provider being LEFT) is never it.
        let store = LastModels {
            last: Some("zen://kimi-k2".parse::<ModelRef>().unwrap()),
            ..Default::default()
        };
        let err = settle_identity(&store, std::slice::from_ref(&spec), &cfg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider 'openai' needs a model"), "{err}");
        assert!(err.contains("openai://<model>"), "{err}");
        assert!(
            !err.contains("kimi-k2"),
            "never the old provider's model: {err}"
        );

        // 3. A provider that DECLARES a model answers with it, store or no store.
        let chatgpt: ModelSpec = "chatgpt://".parse().unwrap();
        assert_eq!(
            settle_identity(&LastModels::default(), &[chatgpt], &cfg)
                .unwrap()
                .to_string(),
            "chatgpt://gpt-5.5"
        );
    }

    /// The endpoint: the provider's, unless relocated. `--base-url` always relocates;
    /// a config-file `base_url` yields to a provider named anywhere.
    #[test]
    fn base_url_relocates_the_provider_it_never_replaces_it() {
        const PRESET: &str = "https://openrouter.ai/api/v1";
        // A `--base-url` / `$HRDR_BASE_URL` wins over everything.
        assert_eq!(
            settle_base_url(Some("http://x/v1".into()), None, PRESET, true),
            "http://x/v1"
        );
        // With no provider named, a bare `--base-url` is the `local` endpoint — the
        // whole point of `local` (keyless, `remote: false`, a server you run).
        assert_eq!(
            settle_base_url(
                Some("http://localhost:9099/v1".into()),
                None,
                hrdr_agent::DEFAULT_BASE_URL,
                false
            ),
            "http://localhost:9099/v1"
        );
        // A config-file `base_url` applies when nothing named a provider…
        assert_eq!(
            settle_base_url(None, Some("http://file/v1".into()), PRESET, false),
            "http://file/v1"
        );
        // …and yields to one that did: the preset belongs to the provider you named.
        assert_eq!(
            settle_base_url(None, Some("http://file/v1".into()), PRESET, true),
            PRESET
        );
        // Nothing at all: the provider's own endpoint.
        assert_eq!(settle_base_url(None, None, PRESET, true), PRESET);
    }

    /// No command → nothing to run at startup (the plain TUI).
    #[test]
    fn no_command_is_no_startup_input() {
        let cli = Cli::parse_from(["hrdr"]);
        assert!(cli.command.is_none());
        assert!(cli.input.is_empty());
    }
}
