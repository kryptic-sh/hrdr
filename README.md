# hrdr

[![CI](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml)

**Herder** — a fast, agentic coding harness for OpenAI-compatible models.

hrdr drives a model through native tool calls to complete software-engineering
tasks in a terminal. It is provider-agnostic: point it at any
`/v1/chat/completions` endpoint — [`infr`](https://github.com/kryptic-sh/infr),
OpenAI, llama.cpp, OpenRouter — and it streams tokens and runs tools until the
job is done.

**hrdr targets UNIX workflows.** The `shell` tool runs `bash` (or POSIX `sh`),
and the guidance the model is given assumes a POSIX shell — where LLMs are
strongest. Linux and macOS work out of the box. On Windows, run hrdr under
**WSL** or install **Git Bash**; without one of those there is no shell tool and
the agent can't run commands. PowerShell is intentionally not supported.

> Active development. The agent loop, adaptive tool set, sub-agents, sessions,
> config hot-reload, and a rich TUI are in place. hrdr connects to any running
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
paru -S hrdr-bin

# Scoop (Windows)
scoop bucket add kryptic-sh https://github.com/kryptic-sh/scoop-bucket
scoop install hrdr

# Debian/Ubuntu · Fedora — grab the .deb / .rpm from the latest release
sudo dpkg -i hrdr_*.deb
sudo rpm -i hrdr-*.rpm
```

## Design

- **Provider-agnostic client.** Speaks clean OpenAI chat-completions with native
  `tools`/`tool_calls` and SSE streaming, plus a **native Anthropic Messages
  API** backend (auto-selected for `api.anthropic.com`) that translates the same
  internal history to Claude's wire format — unlocking native prompt caching.
  The server owns chat-template application; hrdr only ever sends structured
  `messages[]` + `tools[]`.
- **Efficient, adaptive tool set.** `read`, `write`, `edit`, `patch` (multi-file
  unified-diff), `replace` (project-wide substitution with a diff and a
  `dry_run`), `move`, `copy`, `delete`, `find`, `ls`, `tree`, `grep`, `git`
  (read-only: status/diff/log/show/blame/…), `todo`, `fetch`, `search`, a shell,
  and any MCP-server tools. The read tools keep **credential/secret files**
  off-limits — unlike the same access through the shell, which has no such
  guard. The file tools otherwise have full filesystem access (hrdr runs in a
  codebase you trust); a process-level sandbox mode is planned. Token-bounded
  outputs and line-numbered reads for precise edits — and when
  `shell`/`grep`/`git` output overflows, the **full** result is saved to a temp
  file and the model is pointed at it (`read`/`grep`) instead of losing the
  overflow. Tools that shell out are **presence-aware**: the single `shell` tool
  runs `bash` (falling back to POSIX `sh`), and `grep` uses ripgrep → POSIX grep
  → a built-in walker — so the model is only ever offered tools it can actually
  run.
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
| `hrdr-tools`  | The tool set + registry.                                        |
| `hrdr-agent`  | The agent loop + minijinja system prompt.                       |
| `hrdr-editor` | FSM-agnostic hjkl embedding (`EditorEngine` seam).              |
| `hrdr-app`    | UI-agnostic app core: shared slash commands, sessions, status.  |
| `hrdr-tui`    | Ratatui UI: transcript + vim input pane, live streaming.        |
| `hrdr`        | Binary: TUI by default, `hrdr run <task>` for headless.         |

## Usage

```bash
# interactive TUI (see keybindings + slash commands below)
hrdr

# vim keybindings in the input pane instead
hrdr --vim

# start the TUI with a command already run — anything the input box takes:
hrdr /new                     # a fresh session, not the auto-resumed one
hrdr /model                   # open the model picker on the way in
hrdr /resume                  # pick a session to come back to
hrdr ':review src/lib.rs'     # invoke a skill
hrdr '!git status'            # run a shell escape, output into the transcript
hrdr "why is the build slow"  # open the session with a message to the model

# one-shot headless run, streamed to stdout
hrdr run "add a --json flag to the status command"

# scripting/CI: NDJSON events, no chrome, bounded budget
hrdr run --json --max-steps 20 "bump the patch version" | jq -r 'select(.type=="text").text'
hrdr run --quiet "summarize the failing tests"

# cap the estimated spend (USD, incl. sub-agents; priced from models.dev)
hrdr run --max-cost 0.50 "audit the error handling"
```

For debugging harness ⇄ server disagreements, `HRDR_LOG_REQUESTS=<path>` appends
every chat request body, raw SSE line, and non-2xx response to the file as
JSON-per-line. On Unix the file is created 0600 (owner-only). On Windows no
explicit ACL is set — the file inherits the ACLs of the directory you point it
at, so choose a user-scoped location. The log contains raw request/response data
including anything sent to or returned by the provider; pointing
`HRDR_LOG_REQUESTS` at a world-readable directory leaks that data on **any**
platform, so keep it under a directory only you can read. When it reaches 10 MiB
it rotates: the active file is renamed to `<path>.1` (replacing any previous
`.1`) and a fresh active file is started, so the newest entries are always
captured rather than dropped and on-disk use stays bounded at 2× the cap (≈20
MiB).

In the TUI, type a message and press `Enter` to send. `@` completes sub-agent
names (routing the message to that agent) and file paths (attaching the file),
typing `/` opens a slash-command menu, `:` invokes a custom skill, and `!` runs
a shell command directly (`!git status` — output streams into the transcript as
a tool block and is recorded into the model's context, so the next turn knows
what you ran and saw; your `!` commands skip hrdr's shell guardrails). All share
one popup: at most five rows (scroll for more), anchored above the token being
completed. After a command name + space the popup completes the **argument** too
— enum values (`/thinking on`, `/timestamps relative`), theme names, session ids
for `/resume`, file paths for `/edit`/`/add`, and a skill's declared `args:`
values.

### Skills

A skill is a reusable prompt template: a Markdown file whose body is sent to the
model when you type `:name [arguments]`. `$ARGUMENTS` in the body is replaced
with everything after the name (no placeholder → arguments append on their own
line), and the template's own `@file` / `@agent` mentions expand as usual. Files
are discovered from `.hrdr/skills/`, `.claude/commands/`, and
`.opencode/command/` in the project, then `~/.config/hrdr/skills/`,
`~/.claude/commands/`, and `~/.config/opencode/command/` — first match by name
wins. Optional YAML frontmatter — `name:`, `description:` (multi-line and block
scalars both work), and `args:` (a YAML list or a comma-separated string) —
candidate argument values the completion popup offers after `:name `; the file
stem names it otherwise. `/skills` opens a picker over what's loaded (Enter
inserts `:name ` into the input); the transcript shows the raw `:name args` you
typed while the model receives the expanded prompt.

hrdr ships three built-in skills that work with zero setup — `:commit` (stage
and commit with a Conventional Commit message), `:review [low|high]` (verify-
before-report bug review of the pending diff), and
`:release [patch|minor|major]` (bump, changelog, commit, tag, push). They sit
last in the discovery order, so a project or user skill file with the same name
overrides them.

```markdown
## <!-- .hrdr/skills/review.md -->

## description: focused diff review

Review the working-tree diff. Focus on: $ARGUMENTS
```

### Keybindings

| Key                       | Action                                                                                                   |
| ------------------------- | -------------------------------------------------------------------------------------------------------- |
| `Enter`                   | Send; **while a reply runs, queues it** (delivered with the next tool result, else sent as its own turn) |
| `Alt+Enter` / `\`+`Enter` | Insert a newline (`Shift+Enter` too, where supported)                                                    |
| `Up` / `Down`             | Recall previous inputs (single-line); drive the `/` menu                                                 |
| `@name` / `@path`         | Mention a sub-agent (routes to it) or attach a file                                                      |
| `Ctrl+G`                  | Edit the input in `$EDITOR` / `$VISUAL`                                                                  |
| `PageUp/Down`, mouse      | Scroll the transcript; `End` follows the newest output                                                   |
| `Ctrl+L`                  | Clear + repaint the screen                                                                               |
| `Esc` / `Ctrl+C`          | Interrupt the running turn; Esc also cancels a running `!command`                                        |
| `Ctrl+C` twice / `Ctrl+D` | Quit (`Ctrl+D` on an empty input); `Ctrl+Q` quits at once                                                |

Pass `--vim` for a full [hjkl](https://github.com/kryptic-sh/hjkl) vim editor in
the input pane instead of the default plain input.

### Slash commands

Type `/` to see the menu (fuzzy-matched, `Tab` to accept). Highlights:

- **Session** — `/new [name]` (aliases `/clear`, `/reset`), `/resume` (aliases
  `/continue`, `/sessions`; a fuzzy-searchable picker of saved sessions, newest
  first — or `/resume <id|name>` directly), `/rename`, `/compact`, `/status`
  (alias `/info`), `/goto <N|5m|top|end>`, `/find <text>` (`/next` `/prev`)
- **Model** — `/model` (picker: switches model _and_ provider, includes the
  keyless `local` endpoint), `/login` (modal: pick a provider from a fuzzy list,
  then enter the API key in a masked field — OAuth and keyless providers finish
  straight from the list), `/temp`, `/effort` (picker: the reasoning levels the
  current model actually accepts — per the models.dev catalog — highest first,
  "Default" on top to clear the override; sent as `reasoning_effort` to
  OpenAI-style reasoning models, or a `thinking` budget on the native Anthropic
  backend), `/reasoning`
- **Files** — `/init` (write `AGENTS.md`), `/add`, `/edit <file>`, `/diff`,
  `/tools`, `/expand`, `/paste`
- **Reply** — `/copy [code|all|msg N]`, `/export [--json]`, `/cost` (alias
  `/usage`; session tokens + estimated USD, priced from the models.dev catalog,
  sub-agents included)
- **Appearance** — `/theme` (picker with live preview; 5 built-in palettes +
  `~/.config/hrdr/themes/*.toml`), `/timestamps [none|relative|exact]`,
  `/statusbar [none|truncate|wrap]`, `/todo-ttl [turns]`
- **Other** — `/reload`, `/help`, `/exit`

Sessions auto-save per working directory and auto-resume on reopen. Project
instructions are read from `AGENTS.md` (the open [agents.md](https://agents.md)
standard) walking up from the cwd.

### Model endpoint

hrdr does **not** manage a model server — it talks to any running
OpenAI-compatible `/v1` endpoint. Name the model you want as `provider://model`
(below). The default provider, `local`, is `http://localhost:8080/v1`, so a
server running there needs no flags at all.

To serve a model locally, run your own — for native tool calling either works:

```bash
infr serve <model> --addr 127.0.0.1:8080          # infr (native tools/tool_calls, SSE)
llama-server -hf <hf-ref> --jinja --port 8080     # llama.cpp (--jinja enables tool calls)

hrdr                                              # then just launch hrdr
```

**The endpoint belongs to the provider.** There is no `--base-url` flag and no
`$HRDR_BASE_URL`: an endpoint comes from a built-in preset, or from the
`[providers.<name>]` table that defines the provider, and from nowhere else. So
a server at another address is a provider you **define** — in
`~/.config/hrdr/config.toml`:

```toml
[providers.myserver]
base_url = "http://localhost:1234/v1"
```

```bash
hrdr --model 'myserver://qwen'                    # …and name it like any other
```

Why: an endpoint that could be moved from outside the provider could carry that
provider's API key to an address that isn't its own (`--base-url` +
`claude://sonnet` sent your Anthropic key wherever the flag pointed). Tie the
two together and the mismatch is not representable.

### Providers — the model names one

A model belongs to a provider, so hrdr names them **together, as one value**:

```
provider://model         # chatgpt://gpt-5.5, openrouter://deepseek/deepseek-chat, local://llama3:8b
```

That one string is the whole identity, and it is what every model-naming surface
takes — `--model`, `$HRDR_MODEL`, `model = "..."` in config, `/model`, a
`[[subagent]]` profile, the `task` tool. Naming the provider **switches to it**:
its endpoint, API key, headers and context window all follow.

A **bare model id** (`gpt-5.5`, `deepseek/deepseek-chat`, `llama3:8b`) means
"that model, on the provider I am already on" — the separator is `://` and
nothing else, so a slashed or colon'd model id is never mistaken for a provider.

A **provider alone** (`openai://`, note the trailing `://`) says "switch me to
this provider and pick the model for me". Interactively (`--model 'openai://'`,
`/model openai://`, `/login`) that means the model you last used on _that_
provider, else the one it declares. Programmatically (a `[[subagent]]` profile,
the `task` tool) it means only the model the provider itself declares — a
delegation must resolve to the same model on every machine and in CI, so it
never reads what a human last picked. Either way it is never the model you were
using on the provider you are _leaving_.

There is no `--provider` flag and no `provider =` config key: a provider and a
model that can be set independently are a pair that can disagree, and hrdr would
have had to guess which half you meant. For the same reason there is no
`--base-url`, no `$HRDR_BASE_URL` and no top-level `base_url =` in config — the
endpoint is a property of the provider (see above). A config still carrying
either dead key is refused at startup, with the line that replaces it.

Built-in presets:

| Provider               | Endpoint                       | API key env          |
| ---------------------- | ------------------------------ | -------------------- |
| `zen` / `opencode`     | `https://opencode.ai/zen/v1`   | `OPENCODE_API_KEY`   |
| `openai`               | `https://api.openai.com/v1`    | `OPENAI_API_KEY`     |
| `openrouter`           | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `claude` / `anthropic` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY`  |
| `local` / `infr`       | `http://localhost:8080/v1`     | `HRDR_API_KEY`       |

(`claude` / `anthropic` talks to Anthropic's **native Messages API**
(`/v1/messages`, `x-api-key` auth) rather than its OpenAI-compat endpoint — that
unlocks native **prompt caching** and **extended thinking** on Claude. Backend
selection is automatic from the endpoint host, so a `[providers.*]` pointed at
`api.anthropic.com` gets it too. On this backend, `/effort` turns on a
`thinking` budget (scaled from `max_tokens`; streamed to the reasoning pane),
and `max_tokens` (config / `$HRDR_MAX_TOKENS`, default 8192) caps output — raise
it for longer replies and deeper thinking. `local` needs no key.)

```bash
export OPENCODE_API_KEY=sk-...
hrdr --model zen://grok-build-0.1     # chat against a Zen model
hrdr models                           # list the current provider's models
hrdr --model grok-code                # a bare id: same provider, another model
```

With nothing named at all, `hrdr` is `local://default`: the OpenAI-compatible
server you run at `http://localhost:8080/v1`, keyless, serving whatever it was
started with.

#### `/login` — guided setup

Rather than exporting an env var, run **`/login`** in the TUI: pick a provider,
paste its API key, and hrdr saves it as your default. The key is resolved at
startup in the order **inline config → `key_env` → saved credential**, so a
running server or an exported env var still wins.

Credentials are stored **separately from `config.toml`**, in a dedicated
`~/.config/hrdr/auth.toml` (`0600` on unix) — a flat `provider = "key"` map. The
wizard prints the exact path and a plaintext-storage warning before it saves.
Keeping keys out of `config.toml` means you can share or version that file
without leaking secrets.

**File confidentiality.** `auth.toml`, the OAuth token store (`oauth.json`), and
the request log all live under `~/.config/hrdr` on every platform — hrdr does
not use `%APPDATA%`; on Windows `~` resolves to your user profile
(`%USERPROFILE%`, e.g. `C:\Users\you`). On Unix these files are created 0600
(owner-only) and hrdr enforces that mode on every write. On Windows hrdr sets no
explicit ACL: it relies on the default ACLs of the containing per-user profile
directory, which is user-scoped by default. That is the platform default rather
than an hrdr-enforced guarantee, so if you have loosened the ACLs on your
profile directory (or override `XDG_CONFIG_HOME` to a shared location) these
files are only as private as that directory.

#### Custom providers

Define your own in `~/.config/hrdr/config.toml` under `[providers.<name>]` — a
custom entry shadows a built-in of the same name. Each can carry its own model
and context window, so switching to it is a single `--model mylocal://<model>`:

```toml
model = "mylocal://Qwen3-30B-A3B"   # the identity: one key, provider AND model

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

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
key_env = "OPENROUTER_API_KEY"
[providers.openrouter.headers]     # extra headers sent with every request
HTTP-Referer = "https://your.app"  # OpenRouter attribution / ranking
X-Title = "your-app"
```

A `[providers.<name>]` table's own `model` is a **bare model id** — the provider
is the table name, so a URI there would just repeat it. It is the model hrdr
falls back to when something names that provider without a model (a `/login`
switch, or a `provider://` spec).

Each provider can carry `[providers.<name>.headers]` — arbitrary HTTP headers
sent on every request (OpenRouter's `HTTP-Referer`/`X-Title`, or a custom
auth/routing header). They apply at startup and follow a provider switch (the
`/model` picker or `/login`).

**Azure OpenAI:** set `api_version` — hrdr then appends `?api-version=<v>` to
requests and authenticates with an `api-key` header (instead of `Bearer`). Point
`base_url` at the deployment:

```toml
[providers.azure]
base_url = "https://<resource>.openai.azure.com/openai/deployments/<deployment>"
key_env = "AZURE_OPENAI_API_KEY"
api_version = "2024-10-21"
model = "<deployment>"
```

`context_window` is optional: if you omit it, hrdr detects one — at startup
**and** again after a `/model` switch, so the compaction threshold always tracks
the current model's real max. Detection tries, in order:

1. **What the endpoint advertises** — vLLM's `max_model_len`, LM Studio's
   `max_context_length`, llama.cpp's `/props` `n_ctx`, and similar.
2. **The [models.dev](https://models.dev) catalog**, keyed `provider/model`.
   Most OpenAI-compatible APIs — OpenAI itself, opencode zen — publish nothing
   on the wire, so without this the status bar has no "of Y" and auto-compaction
   has no threshold. hrdr downloads `https://models.dev/api.json` (a public,
   unauthenticated static file — the request carries no key, model name or
   prompt), caches it at `$XDG_CACHE_HOME/hrdr/models.json`, and refetches only
   when that copy is over a day old. A failed fetch reuses the stale cache.

   When no provider is configured, the **smallest** window any provider lists
   for that model id is used: compacting early is recoverable, overflowing the
   model's real context isn't.

   Three env vars control it: `HRDR_DISABLE_MODELS_FETCH` (never fetch; use the
   cache if present), `HRDR_MODELS_PATH` (read this file instead, never fetch),
   `HRDR_MODELS_URL` (fetch from your own mirror).

Set `context_window` explicitly to override detection entirely. It drives the
status bar's "X of Y" and the auto-compaction threshold.

### Context management

hrdr keeps context under control in three layers (modeled on opencode), all
tunable in `config.toml`:

```toml
# Per-tool output caps: over either limit, shell/grep/git output is truncated and
# the full text saved to a temp file the model can read/grep. ~24 KB is ~6k
# tokens — enough for a normal diff/status inline, small enough to catch a build
# wall. Raise for fewer file hand-offs, lower for a leaner context.
[tool_output]
max_lines = 1500
max_bytes = 24576

# Prune: when context nears the compaction trigger, replace old tool-output
# bodies and background-task delivery reports with a pointer at a file holding
# the original (keeps a recent window + the last 2 turns verbatim; the UI
# transcript keeps everything) — but only when the reclaim buys enough runway
# to be worth it. ON by default — rewriting history still invalidates the
# prompt cache, but the gating makes a triggered prune strictly cheaper than
# the compaction it defers (compaction nukes the same cache, PLUS pays for a
# summarizer call, PLUS loses the information for good). Turn off to keep
# history verbatim and lean on compaction alone.
auto_prune = true

# Compaction: when context fills, summarize the old head and keep the recent tail.
auto_compact = true            # on/off toggle (legacy 0<x≤1 still enables; 0 disables)
compaction_reserved = 16384    # fire at context_window − this many tokens
compaction_tail_turns = 2      # recent turns kept verbatim through a compaction
preserve_recent_tokens = 8000  # …bounded by this token budget

# Sub-agents: how many may run at once. Write-capable ones are capped lower —
# they share the main agent's working tree, so interleaved edits race. A `task`
# beyond the cap is refused, and the model is told to wait or do the work itself.
max_readonly_subagents = 5     # HRDR_MAX_READONLY_SUBAGENTS, --max-readonly-subagents
max_write_subagents = 2        # HRDR_MAX_WRITE_SUBAGENTS, --max-write-subagents

# Cost budget: stop before the next model call once the session's estimated
# spend (USD, priced from the models.dev catalog, sub-agents included) reaches
# the cap. `hrdr run --max-cost <USD>` overrides per run. Unset = unlimited.
max_cost = 5.0

# By default a capped run refuses an unpriced model (a local server the catalog
# can't price), since its ceiling can't be enforced. Set this (or pass
# `hrdr run --allow-unpriced`) to let those calls run UNCOUNTED while priced
# usage is still capped — the reported total is then a floor ("≥ $X").
allow_unpriced = false
```

`auto_prune` also honors `$HRDR_AUTO_PRUNE` / `--auto-prune on|off`.

### Prompt caching

hrdr can mark `cache_control` breakpoints on each request so the stable
system+tools prefix and the growing conversation prefix are cached across turns
— cutting cost and latency on endpoints that consume the marker: **OpenRouter**
(for its Anthropic/Gemini/Qwen models) and the **native Anthropic Messages API**
(breakpoints on system, the last tool, and the last message).

```toml
prompt_cache = "auto"   # auto (default) | on | off
```

`auto` enables it **for OpenRouter and the native Anthropic backend** only,
because sending an unknown `cache_control` field isn't universally safe: OpenAI,
Groq, and xAI **reject it with a 400**, while others (DeepSeek, Gemini, and
OpenAI itself) already cache automatically. Set `prompt_cache = "on"` to force
it on an endpoint you know accepts it (env `$HRDR_PROMPT_CACHE`, flag
`--prompt-cache off|on|auto`); `/status` shows whether it's currently active.

### Sampling & limits

Opt-in request parameters, all off (not sent) by default so no strict provider
rejects an unexpected field:

```toml
temperature = 0.2
top_p = 0.9
seed = 42                # best-effort determinism (provider support varies)
max_tokens = 8192        # output cap; sent as max_completion_tokens for o-series/gpt-5
stop = ["<END>"]         # stop sequences
stream_usage = true      # set false only if a server rejects stream_options
prompt_cache_ttl = "5m"  # or "1h" for the extended cache TTL
request_timeout = 120    # seconds; connect + idle-read timeout (default: none)
```

Scalars also honor `$HRDR_MAX_TOKENS` / `$HRDR_TOP_P` / `$HRDR_SEED` /
`$HRDR_STREAM_USAGE` / `$HRDR_PROMPT_CACHE_TTL` / `$HRDR_REQUEST_TIMEOUT`.

### MCP servers

Connect [Model Context Protocol](https://modelcontextprotocol.io) servers to add
their tools to the model's tool set. Each `[[mcp]]` entry connects at startup;
its tools are namespaced `<name>_<tool>` and a status line is shown per server.
A server that fails to connect is skipped (the rest still load). Three
transports — set `command` for a local **stdio** server, or `url` for a remote
HTTP one (**Streamable-HTTP** by default, or the legacy two-endpoint
**HTTP+SSE** with `transport = "sse"`):

```toml
# stdio: a spawned local process
[[mcp]]
name = "fs"                                                    # tools appear as fs_*
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/project"]
[mcp.env]                   # extra env for the server process
FOO = "bar"

# HTTP: a remote Streamable-HTTP endpoint
[[mcp]]
name = "remote"
url = "https://mcp.example.com/mcp"
[mcp.headers]               # sent with every request (auth, etc.)
Authorization = "Bearer ghp_…"

# legacy HTTP+SSE: a persistent SSE stream + server-advertised POST endpoint
[[mcp]]
name = "sse"
url = "https://mcp.example.com/sse"
transport = "sse"

[[mcp]]
name = "github"
command = "github-mcp-server"
disabled = true             # keep the entry but skip connecting
```

If a server advertises `resources` or `prompts` capabilities, hrdr exposes them
as extra tools too: `<name>_list_resources` / `<name>_read_resource` and
`<name>_list_prompts` / `<name>_get_prompt`. Tools flagged `readOnlyHint` are
batched concurrently like the built-in read tools; everything else runs
sequentially. (The Streamable-HTTP transport handles both `application/json` and
SSE responses and carries the server's session id.)

### Sub-agents

The model can delegate a self-contained sub-task to a fresh **sub-agent** via
the `task` tool — useful for broad exploration or a focused piece of
implementation, so the main conversation stays clean. The sub-agent has its own
context and the normal tools, runs to completion, and returns its summary as the
result (its tool activity streams live). A concise summary comes back inline; a
large report is instead **saved to a file** and the parent gets a preview + a
pointer to `read`/`grep` it — so it doesn't flood the main context. Issuing
several `task` calls in one turn runs the sub-agents **in parallel** — e.g.
explore several areas of the codebase at once. While they run, the TUI shows a
**live sub-agent panel**: one row per running sub-agent. Each row is a link —
**click a sub-agent to jump to its `task` call** in the transcript, where its
output streams. Finished sub-agents drop from the panel and their result lands
in the transcript.

Every sub-agent runs **detached**: the `task` call returns immediately with a
task id, so a sub-agent never blocks the main conversation — the model keeps
working, and you keep talking to it. The sub-agent's result is **delivered back
into the conversation automatically** when it finishes; if the agent is idle at
that moment, the result wakes it so it reacts without you typing anything.
Detached sub-agents show live in the same panel (with a ✓ on completion). There
is no foreground mode — if the model needs the answer before its next step, it
says so and ends its turn, and the delivered result wakes it.

Five **built-in agents** ship out of the box, selected with the `task` tool's
`agent` argument:

- **`explore`** — a read-only code investigator (read/search tools only, no
  write/edit/shell). Traces files, types, and call paths and reports back.
- **`review`** — a read-only code reviewer. Audits code or a change for bugs,
  edge cases, and security issues, with `path:line` findings.
- **`plan`** — a read-only planner. Investigates with the read/search tools,
  then returns a concrete, step-by-step implementation plan in its report.
  Changes nothing; use it to design the work before delegating the change.
- **`coder`** — a write-capable implementer. Hand it a precise, self-contained
  spec (exact files, symbols, before→after) and it implements exactly that,
  verifies, and commits — no drive-by refactors or scope creep.
- **`general`** — full tool access for open-ended, multi-step tasks (explore and
  modify). The same agent you get from `task` with no `agent` argument.

Each runs on the main provider (respecting `subagent_model`) with a specialized
system prompt and a scoped tool set — `explore`/`review`/`plan` are read-only,
`coder`/`general` get everything. Without an explicit `subagent_model`, later
delegations inherit the main agent's current provider, model, and effort,
including changes made through `/model` or `/effort`.

The read-only `models` tool lets an agent inspect its current provider, model,
effort and resolved default sub-agent model, and—using
`{"mode":"available"}`—the models this session can reach, as
`{provider, model, label, current}` rows. The row the agent is itself running on
is flagged `current: true`.

That is what makes **"delegate this to a model by name"** work: say
`@explore the codebase using big pickle` and the agent resolves that human name
to an id through `models`, then runs the `task` on it — staying on the provider
it is already authenticated and billed on (a bare model id) unless that provider
doesn't offer the model, in which case it names the other one
(`provider://model`) and tells you. Availability is best-effort and does not
guarantee account authorization; hrdr does not rank models by price.

`explore`, `review`, and `coder` are **proactive** — the main agent reaches for
them on its own (explore for broad investigation, review after non-trivial
changes, coder for well-scoped implementation work) without being asked. You can
also **`@name`-mention** an agent in a message (`@explore find the auth flow`)
to route that turn to it; an `@token` that isn't a known agent stays a normal
`@file` mention.

A sub-agent can run on a **different model on the same provider** — e.g. an Opus
main agent delegating implementation to a cheaper/faster Sonnet:

```toml
subagent_model = "claude-sonnet-4-6"   # default for delegated sub-agents
# subagents = false                    # disable the task tool entirely
```

Or on an **entirely different provider** — name it in the model, and the
sub-agent's endpoint, key and headers follow: e.g. Opus on Anthropic manages,
while implementation/exploration runs on another provider's model. A
`[[subagent]]` profile carries one `model` key, and the agent selects the
profile with the `task` tool's `agent` argument:

```toml
[[subagent]]
name = "implementer"
model = "openrouter://moonshotai/kimi-k2"   # another provider, its own key
description = "focused implementation"

[[subagent]]
name = "explorer"
model = "zen://grok-code"
description = "read-only codebase exploration"

[[subagent]]
name = "cheap"
model = "claude-haiku-4-5"                  # bare id: the main provider
description = "small, fast sub-tasks"
```

The sub-agent runs on that profile's provider (its own endpoint, key, headers,
and Azure/Anthropic quirks). `$HRDR_SUBAGENT_MODEL` / `--subagent-model` set the
default for un-profiled delegations, and take the same two shapes.

The `task` tool's own `model` argument overrides per call, and takes the same
one value — so the agent can delegate to a different, already-configured
provider without a profile: `model = "openrouter://deepseek/deepseek-chat"`. The
target provider must be configured and authenticated (a built-in with its
key/OAuth set, or a `[providers.*]` entry); an unconfigured one is rejected
before the sub-agent starts. `model = "openrouter://"` uses that provider's own
configured model, and errors if it declares none — the model you were using
belongs to the provider you are leaving, and never follows you. An explicit
`model` always wins, including over a named profile's.

A profile can also carry a **custom system prompt** and a **scoped tool set** —
this is how the built-in `explore`/`review` agents are defined, and a user
profile of the same name overlays the built-in **field by field**: whatever the
profile sets wins, and whatever it leaves out (e.g. pinning just `model`) still
inherits the built-in's prompt, `read_only` scope, and description rather than
losing them:

```toml
[[subagent]]
name = "review"
description = "security-focused review"
read_only = true                 # scope to read/grep/find/ls/web — no write/edit/shell
prompt = "You are a security reviewer. Focus on authn, injection, and secrets…"
# tools = ["read", "grep"]       # or an explicit allow-list (overrides read_only)
```

`prompt` is appended to the sub-agent's system prompt (its role); `read_only`
scopes it to the read-only tools; `tools` is an explicit allow-list that takes
precedence over `read_only`. Every write-capable sub-agent automatically runs in
a fresh git worktree on a scratch branch — auto-removed if it made no changes,
otherwise kept with a pointer to the branch to review and merge; there's no
per-profile setting for this.

A profile can also tune the sub-agent's runtime knobs, each inheriting the main
agent's when omitted: `temperature`, `effort` (`minimal`/`low`/`medium`/`high`),
and `max_steps` (the tool-call iteration cap) — e.g. a careful `high`-effort
reviewer, or a tightly capped quick task:

```toml
[[subagent]]
name = "reviewer"
read_only = true
effort = "high"        # think harder than the main agent
temperature = 0.1
# max_steps = 20       # cap the sub-agent's tool-call rounds
```

Sub-agents can't themselves delegate (recursion is bounded to one level) and
don't spawn MCP servers.

#### Agents as files

Beyond inline `[[subagent]]` config, hrdr discovers agents from **Markdown
files** — one agent per file, the body is its system prompt, the frontmatter
carries the fields above (`description`, `model`, `read_only`, `tools`,
`temperature`, `effort`, `max_steps`; the `name` defaults to the filename).
`model:` is the same one key (`model: zen://grok-code`, or a bare id for the
main provider; Claude's `model: inherit` means the main agent's identity). It
reads both the **Claude Code** and **opencode** locations so existing agents
work as-is:

| Scope   | hrdr                     | Claude Code         | opencode                    |
| ------- | ------------------------ | ------------------- | --------------------------- |
| project | `.hrdr/agents/`          | `.claude/agents/`   | `.opencode/agent/`          |
| user    | `~/.config/hrdr/agents/` | `~/.claude/agents/` | `~/.config/opencode/agent/` |

```markdown
---
name: security-reviewer
description: Reviews changes for auth/injection/secret bugs
read_only: true
effort: high
---

You are a security reviewer. Focus on authn, injection, and secrets…
```

The same agent found in more than one location is **registered once**: the first
match in precedence order wins — project before user, and `hrdr` → `claude` →
`opencode` within a scope. Overall precedence is `[[subagent]]` config > project
files > user files > built-ins, so any layer overrides a same-named agent from
the one below it. (opencode's boolean `tools:` map is ignored — only an
allow-list `tools` is honored.)

#### Running as an agent (`--agent`)

`--agent <name>` runs the **main** loop as a named agent — it adopts that
agent's system prompt, tool scope, model (provider and all), and knobs, instead
of only being able to delegate to it. The name resolves from the same set as the
`task` tool (built-ins, discovered files, `[[subagent]]` config):

```bash
hrdr --agent explore            # a read-only session for spelunking a codebase
hrdr --agent plan "design the migration"   # investigate, return a plan
```

Unlike a delegated sub-agent, a primary agent keeps delegation (the `task` tool)
and its MCP servers — it's a full session, just wearing the agent's persona and
scope. An unknown name lists the available agents.

### Memory

The agent has a **`memory` tool** for durable notes that persist across sessions
— project conventions, decisions and their rationale, your stable preferences,
gotchas — so it doesn't re-derive them next time. Two scopes:

- **project** — this working directory (default).
- **global** — shared across all projects (e.g. personal preferences).

Storage is plain Markdown under the XDG data dir (`~/.local/share/hrdr/memory/`)
— an **index** (`MEMORY.md`, or OKF-style `index.md`) plus topic files,
greppable, git-diffable, human-editable. Both index names are recognized, so
memory copied from Claude Code (`MEMORY.md`) or an OKF bundle (`index.md`) loads
without renaming. At session start the bounded index (≤200 lines / 25 KB, like
Claude Code) is loaded into the prompt for each scope; the agent reads topic
files on demand with `read`/`grep`, and the index re-loads after `/clear` and
`/compact` so memory survives context resets. The tool actions are `view` (list
a scope, or read a file), `write`, `append`, and `delete`; writes are confined
to the memory store.

Override the storage location with `memory_dir` in config, `--memory-dir`, or
`$HRDR_MEMORY_DIR` — point hrdr at another tool's memory store (the
`projects/<cwd>/` and `global/` scope subdirectories still apply beneath it).
Disable entirely with `memory = false` in config or `$HRDR_MEMORY=0`. Memory is
distinct from `AGENTS.md`, which stays the human-authored, read-only project
instructions.

### Guardrails

The shell tool mechanically rejects the classic foot-guns before they run —
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

The file tools have full filesystem access — hrdr is meant to run in a codebase
you trust, and a working directory that also let the model reach a sibling repo
or a generated file just upstream removes a whole class of needless friction. On
top of that, the read tools refuse known **credential/secret files** — SSH and
other private keys, `.env`, cloud credentials (AWS/GCP/kube/Docker),
`.netrc`/`.npmrc`/ `.pypirc`/`.git-credentials`, keystores, and the like — so
prompt-injected content can't have the agent read them out. And `fetch` blocks
internal/loopback/private and cloud-metadata hosts (SSRF), re-checking on every
redirect hop and at connect time so a DNS rebind can't slip through.

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

Relatedly, `edit`/`write` refuse to mutate an existing file the model hasn't
read this session — blind edits against guessed content are the top source of
corrupt patches.

### Post-edit hooks

Run a shell command automatically after the agent edits or writes a matching
file — formatters, mostly. The tool re-reads the file after hooks run, so the
diff the model sees (and the text its next edit must match) is the post-hook
content. A failing or hung hook becomes a warning in the tool result, never an
error.

```toml
[[hooks]]
on = "edit"                 # edit | write | * (default: *)
glob = "*.rs"               # optional; name or cwd-relative path
run = "cargo fmt -- {path}" # {path} = quoted file path
timeout_ms = 30000          # optional (default 30000)

[[hooks]]
glob = "*.{md,ts,json}"
run = "prettier --write {path}"
```

### Lifecycle hooks

A `[[hooks]]` entry with an `event` runs on agent lifecycle events instead of
file edits. The command receives one JSON object on **stdin** describing the
event (plus `HRDR_HOOK_EVENT` / `HRDR_HOOK_TOOL` in its environment) and speaks
through its exit code: **0** proceeds, **2 blocks** the tool call or prompt
(stderr becomes the reason the model sees), and any other failure is a
non-blocking warning. Hooks run sequentially, each bounded by its own
`timeout_ms`.

| `event`         | Fires                                          | Payload extras                 | Special powers                               |
| --------------- | ---------------------------------------------- | ------------------------------ | -------------------------------------------- |
| `pre_tool`      | before a tool call (`on` filters by tool name) | `tool`, `args`                 | exit 2 vetoes the call                       |
| `post_tool`     | after a tool call                              | `tool`, `args`, `ok`, `result` | failures ride back to the model              |
| `user_prompt`   | when a message is submitted                    | `prompt`                       | exit 2 blocks it; stdout injected as context |
| `turn_end`      | after each turn                                | —                              | —                                            |
| `session_start` | when the session opens                         | —                              | —                                            |
| `session_end`   | on quit (after the final save)                 | —                              | —                                            |

```toml
# Veto risky shell commands with your own policy script:
[[hooks]]
event = "pre_tool"
on = "shell"                 # tool-name filter (* = any tool)
run = "./scripts/check-command.py"   # reads the JSON payload from stdin

# Remind the model of house rules on every prompt:
[[hooks]]
event = "user_prompt"
run = "echo 'Remember: conventional commits only.'"

# Ping when a turn finishes:
[[hooks]]
event = "turn_end"
run = "notify-send hrdr 'turn done'"
```

Sub-agents inherit the same hooks, so a `pre_tool` policy also governs delegated
work.

### LSP diagnostics

After `edit`/`write`/`patch`/`replace` mutate a file, its language server checks
the result and any **errors** ride back to the model appended to the tool result
— a wrong edit is caught in the same round it was made, not at the next build.
Warnings and hints are dropped (signal over lint noise).

It's presence-aware, like the rest of the tool set: a server only spawns if its
binary is on PATH, then stays warm for the session (shared with delegated
sub-agents). The project's primary language server(s) are **pre-warmed at
session start** (detected from root manifests — `Cargo.toml`, `package.json`,
`go.mod`, `pyproject.toml`, …), so indexing-heavy servers like rust-analyzer
overlap their warm-up with your first prompt instead of missing the first edit's
diagnostics. `/doctor` shows each server's status. Built-ins: `rust-analyzer`
(.rs), `typescript-language-server` (.ts/.tsx/.js/…), `pyright-langserver`
(.py), `gopls` (.go), `clangd` (.c/.cpp/…). Diagnostics run on what's actually
on disk — after any formatter hooks. Each edit waits at most `wait_ms` (default
2000 ms) for the server; a slow or dead server degrades to "no diagnostics",
never to a failed edit. Files outside the workspace the servers were initialized
against — a worktree-isolated sub-agent's tree, temp-dir scratch files — are
deliberately skipped rather than left to server-dependent behavior.

```toml
[lsp]
enabled = true   # default; `false` (or HRDR_LSP=0) turns it off
wait_ms = 2000   # per-edit diagnostics wait

# Custom servers are consulted before the built-ins:
[[lsp.servers]]
command = "zls"
extensions = ["zig"]
```

The same warm servers back three **model tools**: `definition` and `references`
(read-only lookups — the model gives a file, a 1-based line, and the symbol text
on that line; results come back as `path:line:col`), and `rename`, which applies
the server-computed workspace edit through the normal write path — so formatter
hooks and post-edit diagnostics run per touched file, and edits are validated
atomically before anything is written. Read-only sub-agents (`explore`,
`review`) get the lookups; `rename` is pruned with the other writers.

### Theme

The TUI colors come from an [hjkl](https://github.com/kryptic-sh/hjkl) theme.
Five popular palettes ship baked into the binary — `tokyonight` (the default),
`catppuccin-mocha`, `dracula`, `gruvbox-dark`, and `nord` — and `/theme` opens a
picker over them plus any TOMLs in `~/.config/hrdr/themes/`, live-previewing the
highlighted theme (Enter applies + persists, Esc restores).
`--theme <name-or-path>` (or `theme = "..."` in config / `$HRDR_THEME`) sets one
directly: a built-in name or the path of an hjkl theme TOML (palette + `[ui]`
styles). hrdr maps the theme's palette onto its chat roles (user, assistant, dim
chrome, tool/loader accent, success/error), so any hjkl theme works.

Configuration (CLI flags override env):

| Env            | Default                            | Meaning                                                                                                           |
| -------------- | ---------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `HRDR_MODEL`   | `local://default`                  | The model, as `provider://model` (switches provider + model) or a bare id (that model, on the provider in force). |
| `HRDR_API_KEY` | _(falls back to `OPENAI_API_KEY`)_ | Bearer token, if required.                                                                                        |

(There is no `HRDR_BASE_URL`: the endpoint is a property of the provider — a
built-in preset or a `[providers.<name>]` table — and nothing outside a provider
definition can move it.)

## Recommended companion tools

hrdr works with zero extra tools installed, but the agent is more capable when
these are on `PATH`. It detects what's available and adapts.

| Tool                      | Why                                                                                                               |
| ------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| **bash** (or POSIX `sh`)  | Backs the `shell` tool — lets the model run builds/tests/commands. Required on Windows via WSL or Git Bash.       |
| **ripgrep** (`rg`)        | Fastest `grep` backend. Falls back to POSIX `grep`, then a built-in walker — but `rg` is best.                    |
| **git**                   | Repo awareness (branch in the status bar).                                                                        |
| **`$EDITOR` / `$VISUAL`** | Used by `Ctrl+G` and `/edit` (falls back to `vi`).                                                                |
| A **Nerd Font**           | Status-bar icons. Otherwise set `icons = unicode` or `ascii` (config / `--icons` / `$HRDR_ICONS`).                |
| **infr** or **llama.cpp** | Only to self-host a model locally — run one yourself (infr or `llama-server`). Not needed with a hosted provider. |

`SEARXNG_URL` (optional) points `search` at a SearXNG instance for more reliable
results than the zero-config DuckDuckGo default.

## Platform support

Built and tested in CI on **Linux, macOS, and Windows** (fmt + clippy + tests on
all three). The TUI, model streaming, web tools, theming, clipboard, config
hot-reload, and sessions are cross-platform.

The shell and search tools adapt to the host:

- **Linux / macOS** — `bash` + ripgrep is the typical setup; everything works
  out of the box.
- **Windows** — hrdr targets UNIX workflows, so run it under **WSL**, or install
  **Git for Windows** so the `shell` tool has `bash`. Without one of those there
  is no shell tool and the agent can't run commands (the rest of the TUI still
  works). The built-in `grep` fallback means search works with nothing extra;
  add **ripgrep** for speed.

## Status / roadmap

- [x] OpenAI client (streaming + tool calls) + agent loop
- [x] Adaptive tool set (files, `fetch`/`search`, presence-aware shell + grep)
      with live output streaming
- [x] TUI: markdown + syntax-highlighted code, diffs, `@file`, slash commands,
      search/goto, timestamps, configurable status bar, themes
- [x] Sessions (auto-save + auto-resume per cwd), `AGENTS.md` project
      instructions
- [x] Network retry + auto-compact on overflow
- [x] Tool-output pruning: pressure-gated and ROI-checked — old tool results and
      background-task delivery reports are replaced with a file pointer (recent
      window + last 2 turns kept) only once compaction is imminent and the
      reclaim is worth it, deferring the costlier compaction fallback
      (`auto_prune`, **on by default**: a ROI-met prune is strictly cheaper than
      the compaction it defers)
- [x] Config file with persistence + OS-level hot-reload
- [x] Cross-platform CI (Linux/macOS/Windows)
- [x] Provider-agnostic: presets (zen/openai/openrouter/claude/local) + custom
      `[providers.*]` at any endpoint; bring your own OpenAI-compatible server
- [x] hjkl deps via crates.io registry pins (standalone CI)
- [x] Shared UI-agnostic core (`hrdr-app`): one implementation of every slash
      command, sessions, status bar, and transcript model
- [x] Release pipeline: 7-target binaries, GitHub Releases, crates.io, AUR,
      Homebrew, Scoop, Alpine
- [x] MCP client (stdio + Streamable-HTTP + legacy HTTP+SSE) — `[[mcp]]`
      servers' tools, resources, and prompts join the set
- [x] LSP diagnostics feedback: post-edit errors from the file's language server
      (presence-aware, lazy-spawned, session-warm) ride back with the tool
      result — see "LSP diagnostics"

## License

MIT
