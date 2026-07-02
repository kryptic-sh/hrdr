# hrdr

[![CI](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml)

**Herder** — a fast, agentic coding harness for OpenAI-compatible models.

hrdr drives a model through native tool calls to complete software-engineering
tasks in a terminal. It is provider-agnostic: point it at any
`/v1/chat/completions` endpoint — [`infr`](https://github.com/kryptic-sh/infr),
OpenAI, llama.cpp, OpenRouter — and it streams tokens and runs tools until the
job is done.

> Active development, released as **v0.1.x**. The agent loop, adaptive tool set,
> sessions, file checkpoints, config hot-reload, a rich TUI, **and a floem-based
> GUI with full command parity** are in place. The default local backend is
> [`infr`](https://github.com/kryptic-sh/infr) (with a `llama-server` fallback);
> see the roadmap for what's next.

## Install

Prebuilt binaries for Linux (gnu + musl, x86_64/aarch64), macOS (Apple Silicon +
Intel), and Windows ship with every
[GitHub Release](https://github.com/kryptic-sh/hrdr/releases), alongside `.deb`,
`.rpm`, and Alpine `.apk` packages.

```bash
# cargo (any platform with Rust)
cargo install hrdr

# Homebrew (macOS)
brew install kryptic-sh/tap/hrdr

# AUR (Arch Linux)
yay -S hrdr-bin

# Scoop (Windows)
scoop bucket add kryptic-sh https://github.com/kryptic-sh/scoop-bucket
scoop install hrdr

# Debian/Ubuntu · Fedora — grab the .deb / .rpm from the latest release
sudo dpkg -i hrdr_*.deb
sudo rpm -i hrdr-*.rpm
```

The desktop GUI (`hrdr-gui`, [floem](https://github.com/lapce/floem)-based)
builds from source: `cargo run -p hrdr-gui --release`.

## Design

- **Provider-agnostic client.** Speaks clean OpenAI chat-completions with native
  `tools`/`tool_calls` and SSE streaming. The server owns chat-template
  application; hrdr only ever sends structured `messages[]` + `tools[]`.
- **Efficient, adaptive tool set.** Fewer, more powerful tools beat a big menu:
  `read_file`, `write_file`, `edit`, `grep`, `glob`, `todo_write`, `web_fetch`,
  `web_search`, plus a shell. Token-bounded outputs and line-numbered reads for
  precise edits. Tools that shell out are **presence-aware**: the shell tool is
  `bash` and/or `powershell` depending on what's installed, and `grep` uses
  ripgrep → POSIX grep → a built-in walker — so the model is only ever offered
  tools it can actually run.
- **Pluggable input discipline.** Default is a plain, claude-style input (always
  typing; `Enter` sends, `Shift+Enter` / `\`+`Enter` insert a newline, `Ctrl+G`
  opens `$EDITOR`, readline-ish `Ctrl+A`/`Ctrl+E`/`Ctrl+W`). `--vim` swaps in a
  real [hjkl](https://github.com/kryptic-sh/hjkl) vim editor. Both are
  `EditorEngine` impls behind an **FSM-agnostic** seam, so a future hjkl
  VSCode/Helix discipline drops in with zero churn.
- **Jinja prompt templating.** hrdr's own system prompt is assembled with
  minijinja templates — editable without a recompile.

## Workspace

| Crate         | Role                                                            |
| ------------- | --------------------------------------------------------------- |
| `hrdr-llm`    | OpenAI-compatible client: types, streaming, tool-call assembly. |
| `hrdr-tools`  | The tool set + registry + file checkpoints.                     |
| `hrdr-agent`  | The agent loop + minijinja system prompt.                       |
| `hrdr-editor` | FSM-agnostic hjkl embedding (`EditorEngine` seam).              |
| `hrdr-app`    | UI-agnostic app core: shared slash commands, sessions, status.  |
| `hrdr-tui`    | Ratatui UI: transcript + vim input pane, live streaming.        |
| `hrdr`        | Binary: TUI by default, `hrdr run <task>` for headless.         |
| `hrdr-gui`    | floem desktop GUI (full command parity with the TUI).           |

## Usage

```bash
# interactive TUI (see keybindings + slash commands below)
hrdr

# vim keybindings in the input pane instead
hrdr --vim

# one-shot headless run, streamed to stdout
hrdr run "add a --json flag to the status command"
```

In the TUI, type a message and press `Enter` to send. `@path` attaches a file
(with completion), and typing `/` opens a slash-command menu.

### Keybindings

| Key                       | Action                                                    |
| ------------------------- | --------------------------------------------------------- |
| `Enter`                   | Send (queues a follow-up if a reply is already running)   |
| `Alt+Enter` / `\`+`Enter` | Insert a newline (`Shift+Enter` too, where supported)     |
| `Up` / `Down`             | Recall previous inputs (single-line); drive the `/` menu  |
| `@path`                   | Attach a file to the message                              |
| `Ctrl+G`                  | Edit the input in `$EDITOR` / `$VISUAL`                   |
| `PageUp/Down`, mouse      | Scroll the transcript; `End` follows the newest output    |
| `Ctrl+L`                  | Clear + repaint the screen                                |
| `Esc` / `Ctrl+C`          | Interrupt the running turn                                |
| `Ctrl+C` twice / `Ctrl+D` | Quit (`Ctrl+D` on an empty input); `Ctrl+Q` quits at once |

Pass `--vim` for a full [hjkl](https://github.com/kryptic-sh/hjkl) vim editor in
the input pane instead of the default plain input.

### Slash commands

Type `/` to see the menu (fuzzy-matched, `Tab` to accept). Highlights:

- **Session** — `/clear`, `/sessions`, `/resume <id|name>`, `/rename`,
  `/compact`, `/info`, `/goto <N|5m|top|end>`, `/find <text>` (`/next` `/prev`)
- **Model** — `/model`, `/models`, `/provider`, `/temp`, `/effort`, `/reasoning`
- **Files** — `/init` (write `AGENTS.md`), `/add`, `/edit <file>`, `/diff`,
  `/revert` + `/checkpoints` (file undo), `/tools`, `/expand`, `/paste`
- **Reply** — `/copy [code|all|msg N]`, `/export [--json]`, `/retry [model]`,
  `/undo`
- **Appearance** — `/theme`, `/timestamps [none|relative|exact]`,
  `/statusbar [none|truncate|wrap]`, `/todo-ttl [turns]`
- **Other** — `/reload`, `/help`, `/exit`

Sessions auto-save per working directory and auto-resume on reopen. Project
instructions are read from `AGENTS.md` (the open [agents.md](https://agents.md)
standard) walking up from the cwd.

### Local backend

By default hrdr **spawns a local backend** and shuts it down on exit. It's
**presence-aware and infr-first**: if
[`infr`](https://github.com/kryptic-sh/infr) is on `PATH` it's launched as
`infr serve <model>` (native `tools`/`tool_calls`, SSE, GGUF Jinja chat
template); otherwise it falls back to **`llama-server`** (llama.cpp, started
with `--jinja` so tool calling works). If neither is installed, hrdr errors and
points you at `--no-backend`. See `apps/hrdr/src/backend.rs`.

```bash
hrdr                                   # spawns infr (or llama-server); default model Qwen3-8B
hrdr --backend-model unsloth/Qwen3-14B-GGUF:Q4_K_M       # pick a bigger model (HF ref or .gguf path)
hrdr --backend-arg=-ngl --backend-arg=99                 # GPU offload passthrough (llama.cpp fallback)
hrdr --no-backend                      # use an endpoint you started yourself
```

If a backend is already answering at `--base-url`, hrdr reuses it instead of
spawning. Spawn logs go to `~/.cache/hrdr/infr-serve.log` (or
`llama-server.log`). infr tuning (sampling, max tokens) is via `INFR_*` env
vars; the same `--backend-model` ref works for both backends.

### Providers

`--provider <name>` (or `provider = "..."` in config, or `$HRDR_PROVIDER`)
selects a preset endpoint + API-key env, and remote providers skip the local
backend:

Built-in presets:

| Provider               | Endpoint                       | API key env          | Backend |
| ---------------------- | ------------------------------ | -------------------- | ------- |
| `zen` / `opencode`     | `https://opencode.ai/zen/v1`   | `OPENCODE_API_KEY`   | remote  |
| `openai`               | `https://api.openai.com/v1`    | `OPENAI_API_KEY`     | remote  |
| `openrouter`           | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | remote  |
| `claude` / `anthropic` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY`  | remote  |
| `local` / `infr`       | `http://localhost:8080/v1`     | `HRDR_API_KEY`       | spawned |

(`claude` uses Anthropic's OpenAI-compatible endpoint.)

```bash
export OPENCODE_API_KEY=sk-...
hrdr models --provider zen                 # list OpenCode Zen models
hrdr --provider zen --model grok-build-0.1 # chat against a Zen model
```

`--base-url` / `$HRDR_BASE_URL` still override a provider's endpoint.

#### Custom providers

Define your own in `~/.config/hrdr/config.toml` under `[providers.<name>]` — a
custom entry shadows a built-in of the same name. Each can carry its own model
and context window, so switching is a single `--provider <name>`:

```toml
provider = "mylocal"            # default provider for this config

[providers.mylocal]
base_url = "http://localhost:8080/v1"
model = "Qwen3-30B-A3B"
remote = false                  # hrdr may spawn/own a local backend
context_window = 16384

[providers.zen]
base_url = "https://opencode.ai/zen/v1"
key_env = "OPENCODE_API_KEY"    # or inline `api_key = "..."`
model = "grok-build-0.1"
context_window = 256000

[providers.chatgpt]
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"
model = "gpt-5.5"
```

`context_window` is optional: if you omit it, hrdr probes the endpoint on
startup and uses what it advertises (vLLM's `max_model_len`, llama.cpp's
`/props` `n_ctx`, etc.), falling back to the spawned backend's `--backend-ctx`
(default 16384). Set it explicitly to override detection — the OpenAI API
doesn't expose context length, and some servers (including infr today) don't
advertise it. It drives the status bar's "X of Y" and the auto-compaction
threshold.

### Guardrails

The shell tools mechanically reject the classic git foot-guns before they run —
blanket staging (`git add -A` / `--all` / `.`), force-push (`--force-with-lease`
is allowed), hook skipping (`--no-verify`), destructive commands
(`reset --hard`, `clean -f`, `checkout/restore .`), and interactive commands
that need a TTY. The model gets a corrective error instead ("stage the files you
actually changed"), which is far more reliable than a prompt rule alone.

Add project- or workflow-specific rules in config; they apply on top of the
built-ins:

```toml
[[guardrails]]
pattern = "\\bnpm\\s+publish\\b"
message = "publishing is manual — never publish from the agent"

[[guardrails]]
pattern = "\\bkubectl\\s+delete\\b"
message = "ask the user before deleting cluster resources"
```

Relatedly, `edit`/`write_file` refuse to mutate an existing file the model
hasn't read this session — blind edits against guessed content are the top
source of corrupt patches.

### Theme

The TUI colors come from an [hjkl](https://github.com/kryptic-sh/hjkl) theme.
`--theme <path>` (or `theme = "..."` in config / `$HRDR_THEME`) points at an
hjkl theme TOML (palette + `[ui]` styles); without one, hjkl's bundled dark
theme is used. hrdr maps the theme's palette onto its chat roles (user,
assistant, dim chrome, tool/loader accent, success/error), so any hjkl theme
works.

Configuration (CLI flags override env):

| Env             | Default                            | Meaning                     |
| --------------- | ---------------------------------- | --------------------------- |
| `HRDR_BASE_URL` | `http://localhost:8080/v1`         | OpenAI-compatible endpoint. |
| `HRDR_MODEL`    | `default`                          | Model id.                   |
| `HRDR_API_KEY`  | _(falls back to `OPENAI_API_KEY`)_ | Bearer token, if required.  |

## Recommended companion tools

hrdr works with zero extra tools installed, but the agent is more capable when
these are on `PATH`. It detects what's available and adapts.

| Tool                           | Why                                                                                                          |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| **bash** and/or **PowerShell** | The shell tool. At least one lets the model run builds/tests/commands. `bash` on unix; `pwsh` runs anywhere. |
| **ripgrep** (`rg`)             | Fastest `grep` backend. Falls back to POSIX `grep`, then a built-in walker — but `rg` is best.               |
| **git**                        | Repo awareness (branch in the status bar). In a git repo, file checkpoints auto-disable since git covers it. |
| **`$EDITOR` / `$VISUAL`**      | Used by `Ctrl+G` and `/edit` (falls back to `vi`).                                                           |
| A **Nerd Font**                | Status-bar icons. Otherwise set `icons = unicode` or `ascii` (config / `--icons` / `$HRDR_ICONS`).           |
| **infr** or **llama.cpp**      | The managed local backend (infr preferred, `llama-server` fallback). Not needed with a remote provider.      |

`SEARXNG_URL` (optional) points `web_search` at a SearXNG instance for more
reliable results than the zero-config DuckDuckGo default.

## Platform support

Built and tested in CI on **Linux, macOS, and Windows** (fmt + clippy + tests on
all three). The TUI, model streaming, web tools, theming, clipboard, config
hot-reload, sessions, and file checkpoints are cross-platform.

The shell and search tools adapt to the host:

- **Linux / macOS** — `bash` + ripgrep is the typical setup; everything works
  out of the box.
- **Windows** — PowerShell is always present, so the shell tool works, and the
  built-in `grep` fallback means search works with nothing extra installed. For
  parity with unix, optionally add **Git for Windows** (`bash`) and **ripgrep**
  to `PATH`.

## Status / roadmap

- [x] OpenAI client (streaming + tool calls) + agent loop
- [x] Adaptive tool set (files, `web_fetch`/`web_search`, presence-aware shell +
      grep) with live output streaming
- [x] TUI: markdown + syntax-highlighted code, diffs, `@file`, slash commands,
      search/goto, timestamps, configurable status bar, themes
- [x] Sessions (auto-save + auto-resume per cwd), `AGENTS.md` project
      instructions
- [x] File checkpoints + `/revert`; network retry + auto-compact on overflow
- [x] Config file with persistence + OS-level hot-reload
- [x] Cross-platform CI (Linux/macOS/Windows)
- [x] Managed local backend — infr-first (native tool calls), `llama-server`
      fallback
- [x] hjkl deps via crates.io registry pins (standalone CI)
- [x] Shared UI-agnostic core (`hrdr-app`): one implementation of every slash
      command, sessions, status bar, and transcript model for both frontends
- [x] floem desktop GUI with **full command parity** (TODO panel, timestamps,
      search/goto scrolling, live theme swap, multi-line input, queueing)
- [x] Release pipeline: 7-target binaries, GitHub Releases, crates.io, AUR,
      Homebrew, Scoop, Alpine
- [ ] MCP client + LSP diagnostics feedback
- [ ] Vim input discipline in the GUI (needs a render-agnostic `EditorEngine`
      seam)

## License

MIT
