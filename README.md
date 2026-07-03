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
> GUI with full command parity** are in place. hrdr connects to any running
> OpenAI-compatible endpoint — a hosted provider or a server you run yourself
> ([`infr`](https://github.com/kryptic-sh/infr), llama.cpp, vLLM, …). See the
> roadmap for what's next.

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
  precise edits — and when `bash`/`grep` output overflows, the **full** result
  is saved to a temp file and the model is pointed at it (`read_file`/`grep`)
  instead of losing the overflow. Tools that shell out are **presence-aware**:
  the shell tool is `bash` and/or `powershell` depending on what's installed,
  and `grep` uses ripgrep → POSIX grep → a built-in walker — so the model is
  only ever offered tools it can actually run.
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

# scripting/CI: NDJSON events, no chrome, bounded budget
hrdr run --json --max-steps 20 "bump the patch version" | jq -r 'select(.type=="text").text'
hrdr run --quiet "summarize the failing tests"
```

For debugging harness ⇄ server disagreements, `HRDR_LOG_REQUESTS=<path>` appends
every chat request body, raw SSE line, and non-2xx response to the file as
JSON-per-line.

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
- **Model** — `/model`, `/models`, `/provider`, `/login` (guided provider + key
  setup), `/temp`, `/effort`, `/reasoning`
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

### Model endpoint

hrdr does **not** manage a model server — it talks to any running
OpenAI-compatible `/v1` endpoint. Point it at one with `--base-url` /
`$HRDR_BASE_URL`, or use a `--provider` preset (below). The default endpoint is
`http://localhost:8080/v1`, so a locally-running server needs no flags.

To serve a model locally, run your own — for native tool calling either works:

```bash
infr serve <model> --addr 127.0.0.1:8080          # infr (native tools/tool_calls, SSE)
llama-server -hf <hf-ref> --jinja --port 8080     # llama.cpp (--jinja enables tool calls)

hrdr                                              # then just launch hrdr
hrdr --base-url http://localhost:1234/v1          # or point at any other endpoint
```

### Providers

`--provider <name>` (or `provider = "..."` in config, or `$HRDR_PROVIDER`)
selects a preset endpoint + API-key env:

Built-in presets:

| Provider               | Endpoint                       | API key env          |
| ---------------------- | ------------------------------ | -------------------- |
| `zen` / `opencode`     | `https://opencode.ai/zen/v1`   | `OPENCODE_API_KEY`   |
| `openai`               | `https://api.openai.com/v1`    | `OPENAI_API_KEY`     |
| `openrouter`           | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `claude` / `anthropic` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY`  |
| `local` / `infr`       | `http://localhost:8080/v1`     | `HRDR_API_KEY`       |

(`claude` uses Anthropic's OpenAI-compatible endpoint. `local` needs no key.)

```bash
export OPENCODE_API_KEY=sk-...
hrdr models --provider zen                 # list OpenCode Zen models
hrdr --provider zen --model grok-build-0.1 # chat against a Zen model
```

`--base-url` / `$HRDR_BASE_URL` still override a provider's endpoint.

#### `/login` — guided setup

Rather than exporting an env var, run **`/login`** in the TUI or GUI: pick a
provider, paste its API key, and hrdr saves it as your default. The key is
resolved at startup in the order **inline config → `key_env` → saved
credential**, so a running server or an exported env var still wins.

Credentials are stored **separately from `config.toml`**, in a dedicated
`~/.config/hrdr/auth.toml` (`0600` on unix) — a flat `provider = "key"` map. The
wizard prints the exact path and a plaintext-storage warning before it saves.
Keeping keys out of `config.toml` means you can share or version that file
without leaking secrets.

#### Custom providers

Define your own in `~/.config/hrdr/config.toml` under `[providers.<name>]` — a
custom entry shadows a built-in of the same name. Each can carry its own model
and context window, so switching is a single `--provider <name>`:

```toml
provider = "mylocal"            # default provider for this config

[providers.mylocal]
base_url = "http://localhost:8080/v1"
model = "Qwen3-30B-A3B"
remote = false                  # self-hosted: no API key required
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
`/props` `n_ctx`, etc.). Set it explicitly to override detection — the OpenAI
API doesn't expose context length, and some servers (including infr today) don't
advertise it. It drives the status bar's "X of Y" and the auto-compaction
threshold.

### Context management

hrdr keeps context under control in three layers (modeled on opencode), all
tunable in `config.toml`:

```toml
# Per-tool output caps: over either limit, bash/grep output is truncated and the
# full text saved to a temp file the model can read_file/grep.
[tool_output]
max_lines = 2000
max_bytes = 51200

# Prune: clear old tool-output bodies from the model context before each request
# (keeps a recent window; the UI transcript keeps everything). Cheap, no model call.
auto_prune = true

# Compaction: when context fills, summarize the old head and keep the recent tail.
auto_compact = 0.85            # trigger at 85% of the context window (0 disables)
compaction_tail_turns = 2      # recent turns kept verbatim through a compaction
preserve_recent_tokens = 8000  # …bounded by this token budget
```

`auto_prune` also honors `$HRDR_AUTO_PRUNE` / `--auto-prune on|off`.

### Guardrails

The shell tools mechanically reject the classic foot-guns before they run —
blanket staging (`git add -A` / `--all` / `.`), force-push (`--force-with-lease`
is allowed), hook skipping (`--no-verify`), destructive git commands
(`reset --hard`, `clean -f`, `checkout/restore .`), interactive commands that
need a TTY, whole-tree deletes (`rm -rf /`, `~`, `.`, `*` — with or without
`sudo`; specific paths stay allowed), and piping downloaded scripts into a shell
(`curl … | sh` → save to a temp file, review, then run). The model gets a
corrective error instead ("stage the files you actually changed"), which is far
more reliable than a prompt rule alone. `sudo` itself is allowed — installing
system packages at the user's request is the user's call — but it can't launder
an otherwise-blocked command.

File mutations (`write_file`/`edit`) are confined to the working directory (the
system temp dir is always allowed for scratch); set `allow_outside_cwd = true`
in config (or `$HRDR_ALLOW_OUTSIDE_CWD`) to lift that.

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

### Post-edit hooks

Run a shell command automatically after the agent edits or writes a matching
file — formatters, mostly. The tool re-reads the file after hooks run, so the
diff the model sees (and the text its next edit must match) is the post-hook
content. A failing or hung hook becomes a warning in the tool result, never an
error.

```toml
[[hooks]]
on = "edit"                 # edit | write_file | * (default: *)
glob = "*.rs"               # optional; name or cwd-relative path
run = "cargo fmt -- {path}" # {path} = quoted file path
timeout_ms = 30000          # optional (default 30000)

[[hooks]]
glob = "*.{md,ts,json}"
run = "prettier --write {path}"
```

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

| Tool                           | Why                                                                                                               |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| **bash** and/or **PowerShell** | The shell tool. At least one lets the model run builds/tests/commands. `bash` on unix; `pwsh` runs anywhere.      |
| **ripgrep** (`rg`)             | Fastest `grep` backend. Falls back to POSIX `grep`, then a built-in walker — but `rg` is best.                    |
| **git**                        | Repo awareness (branch in the status bar). In a git repo, file checkpoints auto-disable since git covers it.      |
| **`$EDITOR` / `$VISUAL`**      | Used by `Ctrl+G` and `/edit` (falls back to `vi`).                                                                |
| A **Nerd Font**                | Status-bar icons. Otherwise set `icons = unicode` or `ascii` (config / `--icons` / `$HRDR_ICONS`).                |
| **infr** or **llama.cpp**      | Only to self-host a model locally — run one yourself (infr or `llama-server`). Not needed with a hosted provider. |

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
- [x] Tool-output pruning: old tool results are cleared from the model context
      (recent window + last 2 turns kept) before compaction — cheap, no model
      call (`auto_prune`, on by default)
- [x] Config file with persistence + OS-level hot-reload
- [x] Cross-platform CI (Linux/macOS/Windows)
- [x] Provider-agnostic: presets (zen/openai/openrouter/claude/local) + custom
      `[providers.*]`, or any `--base-url`; bring your own OpenAI-compatible
      server
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
