//! `hrdr` — herder: an agentic coding harness.
//!
//! No subcommand launches the interactive TUI. `hrdr run <task>` runs a single
//! turn headlessly, streaming to stdout (scriptable, pipeable).
//! `hrdr models` lists available models from the configured endpoint.
//!
//! hrdr talks to any running OpenAI-compatible endpoint; name the model you want
//! as `provider://model` (`--model chatgpt://gpt-5.5`). The endpoint is a property
//! of the PROVIDER — a built-in preset, or a `[providers.<name>]` table in
//! config.toml — so a server of your own is a provider you define, not a flag. It
//! does not manage a model server — start your own (infr, llama.cpp, vLLM, …) or
//! point at a hosted provider.

// Every test in this crate — including one written tomorrow by someone who read none
// of this — runs with `$HOME` and the XDG roots pointed at a throwaway directory. The
// `extern crate` is what links `hrdr-test-support`'s life-before-main ctor into this
// test binary; rustc drops a dependency nothing references, and a dropped ctor is a
// test writing the developer's real sessions. Do not remove it.
#[cfg(test)]
extern crate hrdr_test_support;

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hrdr_agent::{Agent, AgentConfig, AgentEvent};

/// The `hrdr` wordmark: printed above `--help`, and animated in the TUI's
/// session header (passed to [`hrdr_tui::run`], which embeds no art of its own).
const LOGO_ART: &str = include_str!("../art.txt");

/// Whether a headless run colours its stderr chrome.
///
/// Decided once, from three things, and all three have to agree:
///
/// * **stderr is a terminal.** `hrdr run … 2>build.log` should leave a log a
///   person can read, not one with escape codes wrapped round every line. This
///   is the case that actually bites — a headless run is the thing people pipe.
/// * **`NO_COLOR` is unset or empty**, per <https://no-color.org>. hrdr already
///   sets this on every subprocess it spawns (`hrdr_tools`'s shell), so honouring
///   it for hrdr's own output is consistency, not a new convention.
/// * **`TERM` is not `dumb`.**
///
/// Colour itself goes out through crossterm rather than as bytes written by
/// hand: on a Windows console that cannot enable VT processing, crossterm sets
/// the attribute through the WinAPI instead, where a literal escape sequence
/// would have printed as garbage.
fn colour_stderr() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
    })
}

/// One line of headless chrome on stderr: `styled` in `colour`, then `rest`
/// unstyled, then a newline. Uncoloured, it is the same text with nothing
/// around it — the layout never depends on the colour being there.
fn chrome_line(colour: crossterm::style::Color, styled: &str, rest: &str) {
    use crossterm::style::{Print, ResetColor, SetForegroundColor};
    let mut err = std::io::stderr();
    if colour_stderr() {
        let _ = crossterm::execute!(
            err,
            SetForegroundColor(colour),
            Print(styled),
            ResetColor,
            Print(rest),
            Print("\n"),
        );
    } else {
        let _ = writeln!(err, "{styled}{rest}");
    }
}

/// Strip control characters that could act on the user's terminal — an OSC
/// sequence (`ESC ] 52 ; …` writes the clipboard), title spoofing, cursor
/// motion — from text headed for a headless stdout/stderr sink, keeping the
/// layout whitespace (`\t`, `\n`). The TUI path never needs this (ratatui
/// drops control-char graphemes), but `hrdr run` / `hrdr -p` print raw
/// strings: a file the model reproduces verbatim, or a hostile provider's
/// reply, would otherwise reach the terminal unfiltered. Borrowed when the
/// text is already clean, so the hot path allocates nothing.
fn sanitize_terminal_text(text: &str) -> std::borrow::Cow<'_, str> {
    let keep = |c: char| c == '\t' || c == '\n' || !c.is_control();
    if text.chars().all(keep) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().filter(|&c| keep(c)).collect())
}

/// A chrome fragment with no newline — streamed tool output, which arrives in
/// chunks that must not each become a line.
fn chrome_fragment(colour: crossterm::style::Color, text: &str) {
    use crossterm::style::{Print, ResetColor, SetForegroundColor};
    let mut err = std::io::stderr();
    if colour_stderr() {
        let _ = crossterm::execute!(err, SetForegroundColor(colour), Print(text), ResetColor);
    } else {
        let _ = write!(err, "{text}");
    }
    let _ = err.flush();
}

#[derive(Parser)]
#[command(
    name = "hrdr",
    version,
    about = "hrdr — herder: a fast, agentic coding harness for OpenAI-compatible models.",
    before_help = LOGO_ART,
    // `hrdr run …` / `hrdr models` are subcommands; anything else trailing is a
    // command for the TUI to run at startup. Subcommand names always win — even
    // after a global flag (`hrdr --model X run "hi"` is a headless run), which
    // is why `args_conflicts_with_subcommands` is NOT set: clap then stops
    // recognizing subcommand names once any flag has been parsed. The mutual
    // exclusion survives anyway: once the trailing `input` starts consuming,
    // clap's `trailing_var_arg` swallows every later word, so a TUI command and
    // a subcommand can never both be present.
    subcommand_precedence_over_arg = true,
)]
struct Cli {
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

    /// Most read-only sub-agents that may run at once (default 2).
    #[arg(long, global = true, value_name = "N")]
    max_readonly_subagents: Option<usize>,

    /// Most write-capable sub-agents that may run at once (default 1) — they
    /// share the working tree, so interleaved edits race.
    #[arg(long, global = true, value_name = "N")]
    max_write_subagents: Option<usize>,

    /// Filesystem confinement for this session: `write` (the default — reads
    /// unrestricted, writes confined to the working directory, temp/scratch,
    /// tool output and the package-manager caches), `read` (what read-only
    /// agents get: reads unrestricted, writes refused everywhere), `jail`
    /// (read-only tools only — no shell, no network, reads confined to the
    /// working directory; for auditing code you do not trust, and only a
    /// read-only agent can run under it, so a write-capable session floors at
    /// `write` and says so), or `none`, also spelled `yolo` (no confinement).
    #[arg(long, global = true, value_name = "write|read|jail|none")]
    sandbox: Option<String>,

    /// Extra directory the agent may write to; repeat for more than one.
    ///
    /// The "repeat" is load-bearing documentation, not a stylistic note: this is
    /// the only place a user learns the flag can be given twice, and
    /// `--sandbox-writable-root <PATH>` on its own reads as accepting exactly one.
    /// It is repeatable rather than multi-valued because `hrdr` has a greedy
    /// trailing positional for the startup command — a space-separated list would
    /// swallow it — and because comma-splitting makes a directory named `foo,bar`
    /// unrepresentable.
    ///
    /// Appends to the built-in package-manager caches and to
    /// `sandbox_writable_roots` in config; it never replaces them, or using it to
    /// add one path would silently break `cargo build`.
    #[arg(
        long = "sandbox-writable-root",
        global = true,
        value_name = "PATH",
        action = clap::ArgAction::Append
    )]
    sandbox_writable_root: Vec<std::path::PathBuf>,

    /// Run without filesystem confinement (alias for `--sandbox none`).
    #[arg(long = "no-sandbox", global = true, conflicts_with = "sandbox")]
    no_sandbox: bool,

    /// Run without filesystem confinement (alias for `--sandbox none`).
    #[arg(
        long = "yolo",
        global = true,
        conflicts_with_all = ["sandbox", "no_sandbox"]
    )]
    yolo: bool,

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

    /// Status-bar mode: none, truncate (default), or wrap.
    #[arg(long, global = true)]
    statusbar: Option<String>,

    /// Turns a completed TODO item stays visible before it's pruned (default 5).
    #[arg(long, global = true)]
    todo_ttl: Option<u64>,

    /// Compress a session file idle longer than this many seconds (0 disables;
    /// default one week).
    #[arg(long, global = true)]
    session_compress_after: Option<u64>,

    /// Purge an auto-named session idle longer than this many seconds (0
    /// disables; default one month). User-named sessions are never purged.
    #[arg(long, global = true)]
    session_purge_after: Option<u64>,

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
    /// a command (`hrdr :review src/lib.rs`), a shell escape (`hrdr '!git status'`),
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
        let shell = match self {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::Powershell => Shell::PowerShell,
            CompletionShell::Elvish => Shell::Elvish,
            // Nushell is a different generator crate, so it generates separately.
            CompletionShell::Nushell => {
                clap_complete::generate(clap_complete_nushell::Nushell, cmd, "hrdr", out);
                return;
            }
        };
        clap_complete::generate(shell, cmd, "hrdr", out)
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
        /// Let a `--max-cost` run proceed on an unpriced (local/uncatalogued)
        /// model: those calls run uncounted while priced usage is still capped.
        /// The reported total is then a floor ("≥ $X"). A harmless no-op without
        /// `--max-cost`.
        #[arg(long)]
        allow_unpriced: bool,
        /// The task prompt (all trailing words are joined).
        #[arg(trailing_var_arg = true, required = true)]
        prompt: Vec<String>,
    },
    /// List available models, as `provider://model`, across every provider this
    /// machine is set up for.
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

/// The startup gate: **refuse what we KNOW is wrong, warn about what looks wrong.**
///
/// Two questions, asked of the settled identity, in the order they can be
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
/// 2. **Does `default` still mean anything here?**
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
/// exempt from (2) alone; the identity checks still run.
async fn startup_checks(config: &AgentConfig, listing: bool) -> Result<()> {
    // Apply the auth-derived endpoint switch (reads the OAuth store) so a
    // keyless built-in `openai` with a stored OAuth credential validates and
    // probes against the Codex endpoint — not `api.openai.com`, which would
    // spuriously warn "no credential" and probe the wrong host.
    let resolved = hrdr_agent::oauth_derived(hrdr_agent::ResolvedModel::from_config(config));
    let verdict = hrdr_agent::validate_identity(&resolved, config);
    // Whatever the NETWORK-FREE pass already knew, `Agent::new` re-derives and shows
    // in the session itself (a stderr line is invisible under a TUI). Print only what
    // the confirmation step adds on top of it, so the two don't say the same thing
    // twice; `confirm_identity` passes a `Known` verdict straight through, so this is
    // exactly the set the agent will surface.
    let already_known = match &verdict {
        hrdr_agent::Identity::Known(w) => w.clone(),
        hrdr_agent::Identity::Unconfirmed(_) => Vec::new(),
    };
    for w in hrdr_agent::confirm_identity(verdict).await? {
        if !already_known.contains(&w) {
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

/// The Windows sandbox wrapper: `hrdr __sandbox-exec -- <program> <args…>`.
///
/// Lowers this process to Low integrity and runs the rest of the argv, so every
/// descendant inherits the confinement. Intercepted before `Cli::parse` because
/// it is not a user-facing command and clap must never see it.
///
/// Returns `None` when this is an ordinary invocation. Any failure to confine is
/// fatal: running the command unconfined while the backend reports itself active
/// is the one outcome worse than having no backend at all.
#[cfg(windows)]
fn run_sandbox_exec_wrapper() -> Option<Result<std::process::ExitCode>> {
    // Scoped to this function: `main.rs` imports only `anyhow::Result`, and a
    // top-level `use` would be an unused import on every non-Windows build.
    use anyhow::Context as _;

    let mut argv = std::env::args_os().skip(1);
    if argv.next()? != hrdr_tools::sandbox::SANDBOX_EXEC_ARG {
        return None;
    }
    Some((|| {
        let rest: Vec<std::ffi::OsString> = argv.skip_while(|a| a == "--").collect();
        let (program, args) = rest
            .split_first()
            .context("__sandbox-exec: no program after `--`")?;
        hrdr_tools::sandbox::lower_current_process_to_low_integrity()
            .context("__sandbox-exec: could not lower this process to Low integrity")?;
        let status = std::process::Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("__sandbox-exec: spawning {}", program.display()))?;
        // Propagate the child's code so the caller's exit-status handling is
        // unchanged by the extra process in between.
        Ok(std::process::ExitCode::from(
            u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
        ))
    })())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything else, including the tracing subscriber and clap: this
    // process may be a confinement wrapper rather than an hrdr session.
    #[cfg(windows)]
    if let Some(result) = run_sandbox_exec_wrapper() {
        let code = result?;
        // `ExitCode` cannot be returned from a `Result`-returning main, and the
        // wrapper must not fall through into a session.
        std::process::exit(match code == std::process::ExitCode::SUCCESS {
            true => 0,
            false => 1,
        });
    }

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
    let (mut config, config_diags) = AgentConfig::load_diagnosed();
    let (mut ui, ui_warnings) = hrdr_app::UiConfig::load_diagnosed();

    // Config-file values out of range or incompatible are hard errors, listed
    // together (accumulated, not first-error-wins) like the legacy-form refusal
    // above: refuse to start rather than silently substitute a default for a
    // value the user wrote. Env-var and UI-enum problems are warnings — carried
    // to `config_warnings` below and surfaced without stopping.
    if let Some(msg) = config_diags.error_message() {
        eprintln!("{msg}");
        std::process::exit(2);
    }
    let config_warnings: Vec<String> = config_diags
        .warnings
        .into_iter()
        .chain(ui_warnings)
        .collect();

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
    let cli_spec = cli
        .model
        .as_deref()
        .map(str::parse::<hrdr_agent::ModelSpec>)
        .transpose()
        .map_err(|e| {
            anyhow::anyhow!("--model {}: {e}", cli.model.as_deref().unwrap_or_default())
        })?;
    let named_specs = hrdr_agent::named_model_specs();
    let specs: Vec<hrdr_agent::ModelSpec> =
        named_specs.iter().chain(cli_spec.iter()).cloned().collect();
    // `--model` / `$HRDR_MODEL` / config.toml settle the identity a NEW session
    // starts on — the default, not a pin. A session that already carries an
    // identity (it was resumed, or `/model` picked one) keeps its own: the model
    // and the provider are part of the conversation.
    let identity = settle_identity(&store, &specs, &config)?;

    // The endpoint the identity's provider resolves to — its key, headers and
    // api-version with it. **The endpoint is a property of the provider**: it comes
    // from the built-in preset, or from the `[providers.<name>]` table that DEFINES
    // that provider, and from nowhere else. There is no flag, env var or free-floating
    // config key that can move a provider onto someone else's address — which is what
    // makes it impossible for a provider's API key to be sent to an endpoint that is
    // not its own. A server of your own is a provider you define.
    let name = identity.provider().as_str().to_string();
    let p = config.resolve_provider(&name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{name}' (built-ins: {}; or define [providers.{name}] in config)",
            hrdr_agent::BUILTIN_PROVIDERS.join(", ")
        )
    })?;
    config.base_url = p.base_url.clone();
    // Key precedence: inline > key_env var > credential saved by `/login`.
    // Unified readiness folds in trusted ChatGPT OAuth: a built-in ChatGPT login
    // with usable/refreshable credentials is `OAuth` (no key), so it must not draw
    // the missing-key warning. Only a genuinely unconfigured remote provider
    // (`Missing`) warns; the copy is unchanged. `_or_public` supplies Zen's
    // anonymous key when nothing else resolves, so a logged-out session can still
    // run its free models — `auth_state` stays `Anonymous` rather than `Key`, so
    // the picker knows to narrow to those.
    let auth_state = hrdr_agent::provider_auth_state(&name, &p, None, None);
    if let Some(key) = hrdr_agent::resolve_api_key_or_public(&name, &p, None, None) {
        config.api_key = Some(key);
    } else if config.api_key.is_none() && auth_state == hrdr_agent::ProviderAuthState::Missing {
        let env = p.key_env.as_deref().unwrap_or("HRDR_API_KEY");
        eprintln!("hrdr: provider '{name}' needs an API key — set ${env}, or run /login");
    }
    // Surface when the key hrdr will use comes from the environment — a stray
    // `OPENAI_API_KEY` silently overriding a `/login` credential should be
    // visible, not mysterious.
    if let Some(var) = hrdr_agent::api_key_env_source(&p) {
        eprintln!(
            "hrdr: using the API key from ${var} (environment) for '{name}' — overrides any /login credential"
        );
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
    if let Some(b) = cli.auto_compact.as_deref() {
        match hrdr_agent::parse_toggle_or_num(b) {
            Some(b) => config.auto_compact = b,
            // A mistyped `--auto-compact` is never dropped silently — the
            // failure mode is compaction left ON for a user who meant to
            // disable it. The `$HRDR_AUTO_COMPACT` env path warns through
            // `env_warning`; the flag mirrors the `--sandbox` arm below.
            None => eprintln!(
                "warning: --auto-compact: {:?} is not a boolean or a number (> 0 = on) — \
                 keeping {}",
                b, config.auto_compact
            ),
        }
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
    // Unlike the neighbours above, a mistyped sandbox mode is never dropped
    // silently: quietly running unconfined is the failure this flag exists to
    // prevent.
    if let Some(s) = cli.sandbox.as_deref() {
        match s.parse::<hrdr_tools::SandboxMode>() {
            Ok(m) => config.sandbox = m,
            Err(e) => eprintln!("warning: --sandbox: {e} — keeping {}", config.sandbox),
        }
    }
    // `--no-sandbox` and `--yolo` are the same switch under two names, both
    // exclusive with `--sandbox`, so order between them cannot matter.
    if cli.no_sandbox || cli.yolo {
        config.sandbox = hrdr_tools::SandboxMode::None;
    }
    // Appended, never assigned: config's own list stays, and both sit on top of
    // the built-in package-manager caches (`package_cache_roots`). A flag that
    // replaced them would make "allow one extra path" silently mean "and take
    // away every dependency cache".
    config
        .sandbox_writable_roots
        .extend(cli.sandbox_writable_root);
    if cli.no_auto_resume {
        ui.auto_resume = false;
    }
    if cli.no_bell {
        ui.bell = false;
    }
    if let Some(i) = cli.icons {
        ui.icons = Some(i);
    }
    if let Some(s) = cli.statusbar {
        ui.statusbar = Some(s);
    }
    if let Some(p) = cli.prompt_cache {
        config.prompt_cache = Some(p);
    }
    if let Some(n) = cli.todo_ttl {
        ui.todo_ttl = n;
    }
    if let Some(n) = cli.session_compress_after {
        config.session_compress_after = Some(n);
    }
    if let Some(n) = cli.session_purge_after {
        config.session_purge_after = Some(n);
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
    // auto-compaction threshold). Precedence: an explicit config/provider value
    // wins; else the models.dev catalog answers network-free (`Agent::new` picks
    // it up and publishes it before the first frame); else the endpoint's own
    // advertisement (vLLM's `max_model_len`, llama.cpp's `/props` n_ctx, …) is
    // learned in the background — the TUI probes it via `spawn_context_probe`,
    // and `hrdr run` awaits it inside `run_headless` before the turn.
    //
    // Nothing here waits on the network: a slow or firewall-DROPped endpoint must
    // not hold first paint open, and the probe that used to sit here cost every
    // launch a `GET /v1/models` (plus `/props`) round trip — up to the whole 3s
    // budget — for a window the catalog often already knows.
    if config.context_window.is_none() {
        // The Codex endpoint 401s on `/v1/models`, so a server probe can't read
        // it. Resolve per-model from the account catalog cache instead — now
        // that the final model is known — falling back to the preset floor.
        // Network-free.
        if config.base_url == hrdr_agent::CHATGPT_CODEX_BASE_URL {
            config.context_window = hrdr_agent::context_window_for(
                Some(config.model.provider().as_str()),
                &config.base_url,
                config.model.model(),
            );
        }
    }

    // Surface non-fatal config warnings. On the headless / models paths stderr
    // is the only channel, so print here; the TUI instead shows them as a
    // startup notice (`hrdr_app::startup_config_warning`), so don't double-report.
    if cli.command.is_some() {
        for w in &config_warnings {
            eprintln!("hrdr: {w}");
        }
    }

    // The working directory decides whether this session may be steered by files
    // in it. Answered before anything reads `AGENTS.md` or a project command —
    // `Agent::new` does both, and the TUI builds one immediately.
    match trust_gate(&config.cwd, cli.command.is_some(), ui.theme.as_deref()) {
        TrustGate::Proceed => {}
        TrustGate::Jail => {
            // Both, and the second is not optional: `jail` floors at `write` for a
            // write-capable session (it has no shell and no writers, so a writing
            // agent cannot run under it), which would hand an untrusted checkout a
            // write-capable session and load its `AGENTS.md` — the exact opposite
            // of the answer the user gave. Read-only is what makes the jail hold.
            config.sandbox = hrdr_tools::SandboxMode::Jail;
            config.read_only = true;
        }
        TrustGate::Stop => return Ok(()),
    }

    match cli.command {
        Some(Command::Run {
            json,
            quiet,
            max_steps,
            max_cost,
            allow_unpriced,
            prompt,
        }) => {
            if let Some(cost) = max_cost
                && (!cost.is_finite() || cost < 0.0)
            {
                anyhow::bail!("--max-cost must be a finite, non-negative number");
            }
            if let Some(n) = max_steps {
                config.max_steps = n;
            }
            if max_cost.is_some() {
                config.max_cost = max_cost;
            }
            // Accepted with or without `--max-cost` (a harmless no-op without a
            // cap); the flag only overrides config when actually passed.
            if allow_unpriced {
                config.allow_unpriced = true;
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

/// What the trust check decided for this session.
enum TrustGate {
    /// The directory is trusted: start normally.
    Proceed,
    /// Not trusted: start jailed — read the tree, run nothing out of it.
    Jail,
    /// The user cancelled. Start nothing.
    Stop,
}

/// Decide whether this working directory may steer the session, asking the user
/// the first time hrdr is opened in it.
///
/// `headless` covers every path with nobody at the keyboard (`hrdr run …`,
/// `hrdr models`). There is no one to answer, and the two silent answers are both
/// wrong: trusting by default makes the gate bypassable by adding a subcommand,
/// and refusing to start breaks every script in a fresh checkout. Jailing is the
/// third option — the script runs, on the restricted tool set, and says so.
fn trust_gate(cwd: &std::path::Path, headless: bool, theme: Option<&str>) -> TrustGate {
    trust_gate_with(cwd, headless, hrdr_agent::trust::is_trusted(cwd), |c| {
        ask_to_trust(c, theme)
    })
}

/// The decision itself, with the store read and the question already supplied —
/// so the table below is testable without moving this process's XDG roots or
/// finding a terminal to answer on.
fn trust_gate_with(
    cwd: &std::path::Path,
    headless: bool,
    trusted: bool,
    ask: impl FnOnce(&std::path::Path) -> hrdr_agent::trust::TrustChoice,
) -> TrustGate {
    use hrdr_agent::trust;

    if trusted {
        return TrustGate::Proceed;
    }
    if headless {
        eprintln!(
            "hrdr: {} is not a trusted directory — running in jail mode (read-only, no shell).\n\
             hrdr: open hrdr here interactively once to decide.",
            cwd.display()
        );
        return TrustGate::Jail;
    }
    match ask(cwd) {
        trust::TrustChoice::Trusted => {
            if let Err(e) = trust::trust(cwd) {
                // Recording failed, but the user did answer. Honour the answer for
                // this session and say the answer will not stick, rather than
                // silently downgrading them to a jail they did not ask for.
                eprintln!("hrdr: could not record this directory as trusted: {e:#}");
                eprintln!("hrdr: continuing for this session; you will be asked again.");
            }
            TrustGate::Proceed
        }
        trust::TrustChoice::Untrusted => {
            // The menu's screen is gone by now, so say what was chosen — a
            // session that is quietly missing its shell reads as a bug.
            eprintln!("hrdr: opening jailed — read-only tools, no shell, no project instructions.");
            TrustGate::Jail
        }
        trust::TrustChoice::Cancel => TrustGate::Stop,
    }
}

/// Put the question to the user, before the TUI starts.
///
/// The screen itself lives in `hrdr-tui`: it is drawn with ratatui so every
/// colour and attribute goes out through crossterm, which knows whether the
/// console can parse ANSI at all — a hand-written escape sequence does not, and
/// on a Windows console without VT processing it reaches the screen as literal
/// garbage. Drawing it there also shares the session's own theme and logo
/// animation rather than keeping a second copy that could drift.
fn ask_to_trust(cwd: &std::path::Path, theme: Option<&str>) -> hrdr_agent::trust::TrustChoice {
    hrdr_tui::ask_trust(cwd, LOGO_ART, theme)
}

/// Headless single-turn run. Default: reply text on stdout, tool/usage chrome
/// on stderr. `--json`: newline-delimited JSON events on stdout (scripting).
/// `--quiet`: text only. Exit code 0 on a completed turn, 1 on error.
async fn run_headless(config: AgentConfig, prompt: String, json: bool, quiet: bool) -> Result<()> {
    // The endpoint's advertised context window (drives the auto-compaction
    // threshold) is probed here, before the turn — but ONLY when the catalog
    // couldn't answer, and `context_window_for` is a network-free cache read.
    // A catalogued model (the usual `hrdr run`) thus never touches the network
    // at startup, exactly like the TUI; the one case that probes is an
    // uncatalogued model on a local server (llama.cpp/vLLM), whose endpoint is
    // on the same machine and answers in milliseconds.
    let mut config = config;
    if config.context_window.is_none()
        && config.base_url != hrdr_agent::CHATGPT_CODEX_BASE_URL
        && hrdr_agent::context_window_for(
            Some(config.model.provider().as_str()),
            &config.base_url,
            config.model.model(),
        )
        .is_none()
    {
        let probe = hrdr_llm::Client::new(
            config.base_url.clone(),
            config.api_key.clone(),
            config.model.model().to_string(),
        );
        // Same 3s budget as the startup probe that used to sit in `main` — a
        // firewall-DROPped endpoint must not hold the run open, and a timeout is
        // simply "we cannot know".
        config.context_window =
            tokio::time::timeout(Duration::from_secs(3), probe.context_window())
                .await
                .ok()
                .flatten();
    }
    let mut agent = Agent::new(config)?;
    // Prepare the outgoing prompt: expand `@file` mentions and route any
    // `@agent` mention to the matching sub-agent (parity with the TUI), and
    // expand `todo#N` / `task#N` references against this agent's own list.
    let todos = agent.todos_owned();
    let outgoing = hrdr_app::prepare_outgoing_tracked(
        &prompt,
        agent.agent_names(),
        &agent.cwd(),
        agent.project_instructions(),
        &todos,
    );
    // A fully inlined `@file` is content the model has already seen — tell the
    // read-before-edit guard so it doesn't demand a redundant re-read.
    agent.mark_files_read(outgoing.inlined());
    // Connect any configured MCP servers before the turn (their tools join the
    // set); surface the per-server status on stderr unless quiet.
    for notice in agent.connect_mcp().await {
        if !quiet {
            chrome_line(
                crossterm::style::Color::DarkGrey,
                &format!("[{notice}]"),
                "",
            );
        }
    }
    // A headless run is a one-turn session: session hooks bracket the turn.
    for note in agent
        .run_session_hooks(hrdr_tools::HookEvent::SessionStart)
        .await
    {
        if !quiet {
            chrome_line(crossterm::style::Color::DarkGrey, &format!("[{note}]"), "");
        }
    }
    // Headless runs have no interactive steering: enqueue the prompt as the
    // turn's opener (the same queue an interactive steer would use) and run.
    let steering = hrdr_agent::steering_queue();
    // Sent and displayed forms are the same here (nothing echoes a headless
    // prompt back), so the expanded text is both — plus any `@image.png` /
    // `@doc.pdf` attachments, which ride on the message rather than in it.
    let display = outgoing.text().to_string();
    steering
        .lock()
        .unwrap()
        .push_back(outgoing.into_steer(display));
    let result = agent
        .run(steering, |ev| {
            if json {
                println!("{}", event_json(&ev));
                let _ = std::io::stdout().flush();
                return;
            }
            match ev {
                AgentEvent::Text(t) => {
                    print!("{}", sanitize_terminal_text(&t));
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::Reasoning(_) => {}
                AgentEvent::ToolStart { name, args, .. } if !quiet => {
                    chrome_line(
                        crossterm::style::Color::DarkYellow,
                        &format!("⚙ {name}"),
                        &format!(" {}", hrdr_tools::truncate_inline(&args, 120)),
                    );
                }
                AgentEvent::ToolOutput { chunk, .. } if !quiet => {
                    chrome_fragment(
                        crossterm::style::Color::DarkGrey,
                        &sanitize_terminal_text(&chunk),
                    );
                    let _ = std::io::stderr().flush();
                }
                AgentEvent::Notice(text) if !quiet => chrome_line(crossterm::style::Color::DarkGrey, &format!("[{text}]"), ""),
                AgentEvent::ToolEnd { name, ok, .. } if !quiet => {
                    let (mark, colour) = if ok {
                        ("✓", crossterm::style::Color::DarkGreen)
                    } else {
                        ("✗", crossterm::style::Color::DarkRed)
                    };
                    chrome_line(colour, mark, &format!(" {name}"));
                }
                AgentEvent::Usage {
                    prompt_tokens,
                    completion_tokens,
                    cached_prompt_tokens,
                    reasoning_tokens,
                    session_cost_usd,
                    cost_partial,
                    ..
                } if !quiet => {
                    let cached = cached_prompt_tokens
                        .map(|c| format!(" ({c} cached)"))
                        .unwrap_or_default();
                    let reasoning = reasoning_tokens
                        .map(|r| format!(" · reasoning {r}"))
                        .unwrap_or_default();
                    let cost = session_cost_usd
                        .map(|c| {
                            format!(
                                " · est. {}",
                                hrdr_app::fmt_cost_maybe_partial(c, cost_partial)
                            )
                        })
                        .unwrap_or_default();
                    chrome_line(
                            crossterm::style::Color::DarkGrey,
                            &format!(
                                "[usage] ctx {prompt_tokens}{cached} · out {completion_tokens}{reasoning}{cost}"
                            ),
                            "",
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
            chrome_line(crossterm::style::Color::DarkGrey, &format!("[{note}]"), "");
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
        AgentEvent::TodoUpdated(todos) => json!({"type": "todo", "todos": todos}),
        AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            decode_ms,
            cached_prompt_tokens,
            cache_creation_tokens,
            reasoning_tokens,
            cost_usd,
            session_cost_usd,
            cost_partial,
        } => {
            json!({
                "type": "usage",
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "decode_ms": decode_ms,
                "cached_prompt_tokens": cached_prompt_tokens,
                "cache_creation_tokens": cache_creation_tokens,
                "reasoning_tokens": reasoning_tokens,
                "cost_usd": cost_usd,
                "session_cost_usd": session_cost_usd,
                "cost_partial": cost_partial,
            })
        }
        AgentEvent::TurnDone => json!({"type": "done"}),
    };
    v.to_string()
}

/// Print available model ids, one per line.
/// `hrdr models` — every model this machine can actually reach, one
/// `provider://model` identity per line, ready to paste into `--model`.
///
/// It lists ALL providers, not the one in effect: which provider you happen to be
/// on has nothing to do with which ones you are set up for, and the whole point of
/// the listing is to find the identity to switch to. The refresh is awaited here
/// (unlike the session's background one) because it IS the command — with fresh
/// per-provider caches it costs nothing, and on a cold one it is the difference
/// between a real answer and an empty list.
async fn list_models(config: AgentConfig) -> Result<()> {
    hrdr_agent::refresh_models(config.clone()).await;
    let active = config.model.provider().as_str().to_string();
    for m in hrdr_agent::available_models(&config, Some(&active)) {
        println!("{}://{}", m.provider, m.model);
    }
    Ok(())
}

#[cfg(test)]
mod trust_gate_tests {
    use super::*;
    use hrdr_agent::trust::TrustChoice;

    fn never_asked(_: &std::path::Path) -> TrustChoice {
        panic!("a trusted directory must not be asked about")
    }

    #[test]
    fn a_trusted_directory_is_never_asked_about() {
        let g = trust_gate_with(std::path::Path::new("/x"), false, true, never_asked);
        assert!(matches!(g, TrustGate::Proceed));
    }

    /// Headless has nobody to answer, so it takes the middle answer rather than
    /// the two bad ones — the script runs, jailed, and stderr says why.
    #[test]
    fn headless_in_an_unknown_directory_jails_instead_of_asking() {
        let g = trust_gate_with(std::path::Path::new("/x"), true, false, never_asked);
        assert!(matches!(g, TrustGate::Jail));
    }

    /// A trusted directory short-circuits ahead of the headless branch: adding a
    /// subcommand must not turn an answered directory into a jailed one.
    #[test]
    fn headless_still_honours_an_existing_answer() {
        let g = trust_gate_with(std::path::Path::new("/x"), true, true, never_asked);
        assert!(matches!(g, TrustGate::Proceed));
    }

    #[test]
    fn declining_jails_and_cancelling_starts_nothing() {
        let jailed = trust_gate_with(std::path::Path::new("/x"), false, false, |_| {
            TrustChoice::Untrusted
        });
        assert!(matches!(jailed, TrustGate::Jail));
        let stopped = trust_gate_with(std::path::Path::new("/x"), false, false, |_| {
            TrustChoice::Cancel
        });
        assert!(matches!(stopped, TrustGate::Stop));
    }
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

    /// `--allow-unpriced` is accepted on `run` both with and without
    /// `--max-cost` — without a cap it is a harmless no-op, not a parse error.
    #[test]
    fn allow_unpriced_parses_with_and_without_max_cost() {
        let run = |argv: &[&str]| match Cli::parse_from(argv).command {
            Some(Command::Run {
                allow_unpriced,
                max_cost,
                ..
            }) => (allow_unpriced, max_cost),
            _ => panic!("expected a Run command"),
        };
        // With a cap.
        assert_eq!(
            run(&["hrdr", "run", "--allow-unpriced", "--max-cost", "1.5", "hi"]),
            (true, Some(1.5))
        );
        // Without a cap: still parses, cap is None.
        assert_eq!(
            run(&["hrdr", "run", "--allow-unpriced", "hi"]),
            (true, None)
        );
        // Absent: defaults to false.
        assert_eq!(run(&["hrdr", "run", "hi"]), (false, None));
    }

    /// Flags still bind to hrdr, not to the command — as long as they come first.
    #[test]
    fn flags_before_the_command_still_reach_hrdr() {
        let cli = Cli::parse_from(["hrdr", "--model", "zen://kimi-k2", "--vim", "/model"]);
        assert_eq!(cli.model.as_deref(), Some("zen://kimi-k2"));
        assert!(cli.vim);
        assert_eq!(cli.input.join(" "), "/model");
    }

    /// A leading global flag must not push a subcommand into the TUI input:
    /// `hrdr --model X run "hi"` is a headless run, not the TUI command `run hi`.
    /// (clap's `args_conflicts_with_subcommands` used to stop recognizing
    /// subcommand names once any flag had been parsed.)
    #[test]
    fn a_subcommand_is_not_swallowed_by_a_leading_flag() {
        // `run` / `models` are subcommands; a leading global flag must not push
        // them into the TUI input.
        let cli = Cli::parse_from(["hrdr", "--model", "zen://kimi-k2", "run", "hi"]);
        assert_eq!(
            cli.model.as_deref(),
            Some("zen://kimi-k2"),
            "--model still binds"
        );
        match cli.command {
            Some(Command::Run { prompt, .. }) => assert_eq!(prompt.join(" "), "hi"),
            _ => panic!("`--model X run hi` must be the run subcommand"),
        }
        assert!(cli.input.is_empty());

        let cli = Cli::parse_from(["hrdr", "--vim", "run", "fix", "the", "bug"]);
        match cli.command {
            Some(Command::Run { prompt, .. }) => assert_eq!(prompt.join(" "), "fix the bug"),
            _ => panic!("`--vim run …` must be the run subcommand"),
        }
        assert!(cli.input.is_empty());

        let cli = Cli::parse_from(["hrdr", "--model", "zen://kimi-k2", "models"]);
        assert!(
            matches!(cli.command, Some(Command::Models)),
            "`--model X models`"
        );
        assert!(cli.input.is_empty());
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
        // what was in effect before. (`chatgpt` folds onto the merged `openai`.)
        assert_eq!(
            got(Some("zen://kimi-k2"), &[spec("chatgpt://gpt-5.5")]),
            "openai://gpt-5.5"
        );
        // `--model gpt-5.5` (bare) keeps the provider in effect.
        assert_eq!(
            got(Some("zen://kimi-k2"), &[spec("gpt-5.5")]),
            "zen://gpt-5.5"
        );
        // Nothing named: the last-used identity is resumed, whole.
        assert_eq!(got(Some("zen://kimi-k2"), &[]), "zen://kimi-k2");
        // Nothing named and nothing used: `local://default` — the server you run,
        // serving whatever it was started with. A bare `hrdr` is this run.
        assert_eq!(got(None, &[]), hrdr_agent::DEFAULT_MODEL_REF);
        assert_eq!(
            settle_identity(&store(None), &[], &cfg)
                .unwrap()
                .provider()
                .as_str(),
            "local",
            "a bare `hrdr` is a `local` run"
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

        // 3. A provider that DECLARES a model answers with it, store or no store. No
        //    built-in declares one now (the merged `openai` included), so this is
        //    shown with a `[providers.*]` entry that sets `model`.
        let mut cfg_declares = AgentConfig::default();
        cfg_declares.providers.insert(
            "declares".to_string(),
            hrdr_agent::ProviderConfig {
                base_url: "https://declares.example/v1".to_string(),
                key_env: None,
                api_key: None,
                model: Some("its-own-model".to_string()),
                remote: None,
                context_window: None,
                headers: std::collections::HashMap::new(),
                api_version: None,
            },
        );
        let declares: ModelSpec = "declares://".parse().unwrap();
        assert_eq!(
            settle_identity(&LastModels::default(), &[declares], &cfg_declares)
                .unwrap()
                .to_string(),
            "declares://its-own-model"
        );
    }

    /// **THE ENDPOINT BELONGS TO THE PROVIDER.** There is no `--base-url` flag, and
    /// no `$HRDR_BASE_URL`: an endpoint may come from a built-in preset or from the
    /// `[providers.<name>]` table that defines the provider, and from nowhere else.
    /// That is what makes it impossible for a provider's key to be sent to an
    /// endpoint that is not its own.
    #[test]
    fn there_is_no_endpoint_override_flag() {
        // The flag does not parse — clap refuses an argument it does not know.
        assert!(
            Cli::try_parse_from(["hrdr", "--base-url", "http://evil.example/v1"]).is_err(),
            "--base-url must not exist"
        );
        // (`$HRDR_BASE_URL` is gone from the env table too — asserted where that table
        // lives: `hrdr_base_url_is_not_a_knob` in hrdr-agent.)
    }

    /// A bare `hrdr` still lands on the `local` preset — the easy on-ramp is
    /// unchanged: `local://default` at `http://localhost:8080/v1`.
    #[test]
    fn the_default_run_resolves_to_the_local_preset() {
        let cfg = AgentConfig::default();
        let identity = settle_identity(&hrdr_agent::LastModels::default(), &[], &cfg).unwrap();
        assert_eq!(identity.to_string(), "local://default");
        let p = cfg
            .resolve_provider(identity.provider().as_str())
            .expect("`local` is a built-in");
        assert_eq!(p.base_url, hrdr_agent::DEFAULT_BASE_URL);
        assert_eq!(p.base_url, "http://localhost:8080/v1");
    }

    /// A user-defined provider IS an endpoint definition — the only one there is.
    /// `[providers.myserver] base_url = …` + `--model myserver://qwen` resolves to
    /// that address, and the model rides on it.
    #[test]
    fn a_user_defined_provider_supplies_its_own_endpoint() {
        use hrdr_agent::ProviderConfig;
        let mut cfg = AgentConfig::default();
        cfg.providers.insert(
            "myserver".to_string(),
            ProviderConfig {
                base_url: "http://localhost:1234/v1".to_string(),
                key_env: None,
                api_key: None,
                model: None,
                remote: None,
                context_window: None,
                headers: Default::default(),
                api_version: None,
            },
        );
        let spec: hrdr_agent::ModelSpec = "myserver://qwen".parse().unwrap();
        let identity = settle_identity(&hrdr_agent::LastModels::default(), &[spec], &cfg).unwrap();
        assert_eq!(identity.to_string(), "myserver://qwen");
        let p = cfg
            .resolve_provider(identity.provider().as_str())
            .expect("a [providers.*] table defines a provider");
        assert_eq!(p.base_url, "http://localhost:1234/v1");
    }

    /// No command → nothing to run at startup (the plain TUI).
    #[test]
    fn no_command_is_no_startup_input() {
        let cli = Cli::parse_from(["hrdr"]);
        assert!(cli.command.is_none());
        assert!(cli.input.is_empty());
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_terminal_text;
    use std::borrow::Cow;

    /// An OSC 52 clipboard-write sequence's ESC and BEL are control chars and
    /// drop; without the ESC prefix the payload `]52;c;ZGVtbw==` is inert
    /// printed text, so no clipboard write reaches the terminal. The
    /// surrounding text survives.
    #[test]
    fn osc_52_clipboard_write_is_stripped() {
        assert_eq!(
            sanitize_terminal_text("hi \x1b]52;c;ZGVtbw==\x07 there"),
            "hi ]52;c;ZGVtbw== there"
        );
    }

    /// Layout whitespace survives the filter.
    #[test]
    fn tab_and_newline_survive() {
        assert_eq!(sanitize_terminal_text("a\tb\nc"), "a\tb\nc");
    }

    /// Clean text is returned borrowed — the hot path allocates nothing.
    #[test]
    fn clean_text_is_borrowed() {
        assert!(matches!(
            sanitize_terminal_text("plain text"),
            Cow::Borrowed(_)
        ));
    }

    /// A standalone BEL and DEL are both dropped.
    #[test]
    fn bel_and_del_are_dropped() {
        assert_eq!(sanitize_terminal_text("a\x07b\x7fc"), "abc");
    }
}
