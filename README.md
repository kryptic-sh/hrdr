# hrdr

[![CI](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml)

**Herder** — a fast, agentic coding harness for OpenAI-compatible models.

hrdr drives a model through native tool calls to complete software-engineering
tasks in a terminal. It is provider-agnostic: point it at any
`/v1/chat/completions` endpoint — [`infr`](https://github.com/kryptic-sh/infr),
OpenAI, llama.cpp, OpenRouter — and it streams tokens and runs tools until the
job is done.

> Active development, released as **v0.2.x**. The agent loop, adaptive tool set,
> sub-agents, sessions, file checkpoints, config hot-reload, and a rich TUI are
> in place. hrdr connects to any running OpenAI-compatible endpoint — a hosted
> provider or a server you run yourself
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
  and any MCP-server tools. The file-mutating tools are **checkpointed** (so
  `/undo` reverts them), and every file tool — read, search, and write alike —
  is **confined** to the project (paths outside it are refused), with
  credential/secret files off-limits to the read tools — unlike the same access
  through the shell, which has neither guard. Token-bounded outputs and
  line-numbered reads for precise edits — and when `bash`/`grep` output
  overflows, the **full** result is saved to a temp file and the model is
  pointed at it (`read`/`grep`) instead of losing the overflow. Tools that shell
  out are **presence-aware**: the shell tool is `bash` and/or `powershell`
  depending on what's installed, and `grep` uses ripgrep → POSIX grep → a
  built-in walker — so the model is only ever offered tools it can actually run.
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

# cap the estimated spend (USD, incl. sub-agents; priced from models.dev)
hrdr run --max-cost 0.50 "audit the error handling"
```

For debugging harness ⇄ server disagreements, `HRDR_LOG_REQUESTS=<path>` appends
every chat request body, raw SSE line, and non-2xx response to the file as
JSON-per-line.

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
wins. Optional `name:` / `description:` frontmatter (plus `args: [a, b]` —
candidate argument values the completion popup offers after `:name `); the file
stem names it otherwise. `/skills` opens a picker over what's loaded (Enter
inserts `:name ` into the input); the transcript shows the raw `:name args` you
typed while the model receives the expanded prompt.

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
  `/revert` + `/checkpoints` (file undo), `/tools`, `/expand`, `/paste`
- **Reply** — `/copy [code|all|msg N]`, `/export [--json]`, `/retry [model]`,
  `/undo`, `/cost` (alias `/usage`; session tokens + estimated USD, priced from
  the models.dev catalog, sub-agents included)
- **Appearance** — `/theme` (picker with live preview; 5 built-in palettes +
  `~/.config/hrdr/themes/*.toml`), `/timestamps [none|relative|exact]`,
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

(`claude` / `anthropic` talks to Anthropic's **native Messages API**
(`/v1/messages`, `x-api-key` auth) rather than its OpenAI-compat endpoint — that
unlocks native **prompt caching** and **extended thinking** on Claude. Backend
selection is automatic from the endpoint host, so pointing `--base-url` at
`api.anthropic.com` works too. On this backend, `/effort` turns on a `thinking`
budget (scaled from `max_tokens`; streamed to the reasoning pane), and
`max_tokens` (config / `$HRDR_MAX_TOKENS`, default 8192) caps output — raise it
for longer replies and deeper thinking. `local` needs no key.)

```bash
export OPENCODE_API_KEY=sk-...
hrdr models --provider zen                 # list OpenCode Zen models
hrdr --provider zen --model grok-build-0.1 # chat against a Zen model
```

`--base-url` / `$HRDR_BASE_URL` still override a provider's endpoint.

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

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
key_env = "OPENROUTER_API_KEY"
[providers.openrouter.headers]     # extra headers sent with every request
HTTP-Referer = "https://your.app"  # OpenRouter attribution / ranking
X-Title = "your-app"
```

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
# Per-tool output caps: over either limit, bash/grep output is truncated and the
# full text saved to a temp file the model can read/grep.
[tool_output]
max_lines = 2000
max_bytes = 51200

# Prune: clear old tool-output bodies from the model context before each request
# (keeps a recent window; the UI transcript keeps everything). Cheap, no model call.
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

Sub-agents run **detached by default**: the `task` call returns immediately with
a task id, so a sub-agent never blocks the main conversation — the model keeps
working, and you keep talking to it. The sub-agent's result is **delivered back
into the conversation automatically** when it finishes; if the agent is idle at
that moment, the result wakes it so it reacts without you typing anything.
Detached sub-agents show live in the same panel (with a ✓ on completion).

Pass **`background: false`** when the model needs the sub-agent's answer before
its next step — the call then blocks and the result comes back inline.
Sub-agents running in an isolated worktree (`isolation = "worktree"`) always
block; they can't detach yet.

Four **built-in agents** ship out of the box, selected with the `task` tool's
`agent` argument:

- **`explore`** — a read-only code investigator (read/search tools only, no
  write/edit/shell). Traces files, types, and call paths and reports back.
- **`review`** — a read-only code reviewer. Audits code or a change for bugs,
  edge cases, and security issues, with `path:line` findings.
- **`plan`** — a planner. Investigates read-only, then writes a step-by-step
  plan to disk as a **Markdown file** — it can create/edit `.md` files only, no
  other file changes.
- **`general`** — full tool access for open-ended, multi-step tasks (explore and
  modify). The same agent you get from `task` with no `agent` argument.

Each runs on the main provider (respecting `subagent_model`) with a specialized
system prompt and a scoped tool set — `explore`/`review` are read-only, `plan`
adds Markdown-only writes, `general` gets everything.

`explore` and `review` are **proactive** — the main agent reaches for them on
its own (explore for broad investigation, review after non-trivial changes)
without being asked. You can also **`@name`-mention** an agent in a message
(`@explore find the auth flow`) to route that turn to it; an `@token` that isn't
a known agent stays a normal `@file` mention.

A sub-agent can run on a **different model on the same provider** — e.g. an Opus
main agent delegating implementation to a cheaper/faster Sonnet:

```toml
subagent_model = "claude-sonnet-4-6"   # default for delegated sub-agents
# subagents = false                    # disable the task tool entirely
```

Or on an **entirely different provider** via named `[[subagent]]` profiles —
e.g. Opus on Anthropic manages, while implementation/exploration runs on another
provider's model. Each profile pins a `provider` (a built-in or
`[providers.<name>]`) + `model`; the model selects one with the `task` tool's
`agent` argument:

```toml
[[subagent]]
name = "implementer"
provider = "openrouter"
model = "moonshotai/kimi-k2"
description = "focused implementation"

[[subagent]]
name = "explorer"
provider = "zen"
model = "grok-code"
description = "read-only codebase exploration"
```

The sub-agent runs on that profile's provider (its own endpoint, key, headers,
and Azure/Anthropic quirks). The model can also override the model per call
(`model` argument); also `$HRDR_SUBAGENT_MODEL` / `--subagent-model` for the
default.

A profile can also carry a **custom system prompt** and a **scoped tool set** —
this is how the built-in `explore`/`review` agents are defined, and a user
profile of the same name overrides the built-in:

```toml
[[subagent]]
name = "review"
description = "security-focused review"
read_only = true                 # scope to read/grep/find/ls/web — no write/edit/shell
prompt = "You are a security reviewer. Focus on authn, injection, and secrets…"
# tools = ["read", "grep"]       # or an explicit allow-list (overrides read_only)
```

`prompt` is appended to the sub-agent's system prompt (its role); `read_only`
scopes it to the read-only tools; `write_ext` grants the read-only tools plus
file writes limited to those extensions (e.g. `write_ext = ["md"]`, how `plan`
is built); `tools` is an explicit allow-list that takes precedence over both.
`isolation = "worktree"` runs the sub-agent in a fresh git worktree on a scratch
branch — auto-removed if it made no changes, otherwise kept with a pointer to
the branch to review and merge.

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
don't spawn MCP servers. Their file edits aren't captured by the parent's
`/revert` yet — use git.

#### Agents as files

Beyond inline `[[subagent]]` config, hrdr discovers agents from **Markdown
files** — one agent per file, the body is its system prompt, the frontmatter
carries the fields above (`description`, `model`, `provider`, `read_only`,
`tools`, `write_ext`, `temperature`, `effort`, `max_steps`; the `name` defaults
to the filename). It reads both the **Claude Code** and **opencode** locations
so existing agents work as-is:

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
agent's system prompt, tool scope, model/provider, and knobs, instead of only
being able to delegate to it. The name resolves from the same set as the `task`
tool (built-ins, discovered files, `[[subagent]]` config):

```bash
hrdr --agent explore            # a read-only session for spelunking a codebase
hrdr --agent plan "design the migration"   # investigate, then write PLAN.md
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

Every file tool is confined to the working directory. `read`, `grep`, `ls`, and
`tree` refuse paths outside it, and `write`/`edit` are limited to it too (writes
may also use the system temp dir for scratch); set `allow_outside_cwd = true` in
config (or `$HRDR_ALLOW_OUTSIDE_CWD`) to lift the confinement. On top of that,
the read tools refuse known **credential/secret files** — SSH and other private
keys, `.env`, cloud credentials (AWS/GCP/kube/Docker), `.netrc`/`.npmrc`/
`.pypirc`/`.git-credentials`, keystores, and the like — so prompt-injected
content can't have the agent read them out. And `fetch` blocks
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
# Veto risky bash commands with your own policy script:
[[hooks]]
event = "pre_tool"
on = "bash"                  # tool-name filter (* = any tool)
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
sub-agents). Built-ins: `rust-analyzer` (.rs), `typescript-language-server`
(.ts/.tsx/.js/…), `pyright-langserver` (.py), `gopls` (.go), `clangd`
(.c/.cpp/…). Diagnostics run on what's actually on disk — after any formatter
hooks. Each edit waits at most `wait_ms` (default 2000 ms) for the server; a
slow or dead server degrades to "no diagnostics", never to a failed edit.

```toml
[lsp]
enabled = true   # default; `false` (or HRDR_LSP=0) turns it off
wait_ms = 2000   # per-edit diagnostics wait

# Custom servers are consulted before the built-ins:
[[lsp.servers]]
command = "zls"
extensions = ["zig"]
```

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

`SEARXNG_URL` (optional) points `search` at a SearXNG instance for more reliable
results than the zero-config DuckDuckGo default.

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
- [x] Adaptive tool set (files, `fetch`/`search`, presence-aware shell + grep)
      with live output streaming
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
