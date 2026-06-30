# hrdr

[![CI](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/hrdr/actions/workflows/ci.yml)

**Herder** — a fast, agentic coding harness for OpenAI-compatible models.

hrdr drives a model through native tool calls to complete software-engineering
tasks in a terminal. It is provider-agnostic: point it at any
`/v1/chat/completions` endpoint — [`infr`](https://github.com/kryptic-sh/infr),
OpenAI, llama.cpp, OpenRouter — and it streams tokens and runs tools until the
job is done.

> Early WIP. The agent loop, tool set, OpenAI client, and a vim-keybound TUI are
> in place; see the roadmap below.

## Design

- **Provider-agnostic client.** Speaks clean OpenAI chat-completions with native
  `tools`/`tool_calls` and SSE streaming. The server owns chat-template
  application; hrdr only ever sends structured `messages[]` + `tools[]`.
- **Efficient, locked tool set.** Fewer, more powerful tools beat a big menu:
  `read_file`, `write_file`, `edit`, `bash`, `grep`, `glob`, `todo_write`.
  Token-bounded outputs, line-numbered reads for precise edits, ripgrep search.
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
| `hrdr-tools`  | The seven MVP tools + registry.                                 |
| `hrdr-agent`  | The agent loop + minijinja system prompt.                       |
| `hrdr-editor` | FSM-agnostic hjkl embedding (`EditorEngine` seam).              |
| `hrdr-tui`    | Ratatui UI: transcript + vim input pane, live streaming.        |
| `hrdr`        | Binary: TUI by default, `hrdr run <task>` for headless.         |

## Usage

```bash
# interactive TUI — plain input: type, Enter sends, Alt+Enter or \+Enter newline
# (Shift+Enter on supporting terminals), Ctrl+G opens $EDITOR, Ctrl+C quits.
# Submit while a reply is running to queue follow-up messages.
hrdr

# vim keybindings in the input pane instead
hrdr --vim

# one-shot headless run, streamed to stdout
hrdr run "add a --json flag to the status command"
```

### Backend (temporary)

By default hrdr **spawns a local `llama-server`** (llama.cpp, started with
`--jinja` so tool calling works) and shuts it down on exit. This is a
**stopgap** so the harness can be refined against a real tool-calling model — it
will be removed once [`infr`](https://github.com/kryptic-sh/infr)'s serve path
supports agentic tool use (today infr ignores the request's `tools` and only
forwards the last user message). See `apps/hrdr/src/backend.rs`.

```bash
hrdr                                   # spawns llama-server with the default model
hrdr --backend-model unsloth/Qwen3-30B-A3B-GGUF:Q4_K_M   # pick a model
hrdr --backend-arg=-ngl --backend-arg=99                 # GPU offload passthrough
hrdr --no-backend                      # use an endpoint you started yourself
```

If a backend is already answering at `--base-url`, hrdr reuses it instead of
spawning. Spawn logs go to `~/.cache/hrdr/llama-server.log`.

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

## Status / roadmap

- [x] OpenAI client (streaming + tool calls)
- [x] Tool set (read/write/edit/bash/grep/glob/todo)
- [x] Agent loop with tool execution
- [x] hjkl vim input pane (FSM-agnostic seam)
- [x] Interactive TUI + headless `run`
- [x] In-flight turn cancellation
- [x] TODO panel + transcript scrolling _(wrap-aware scroll still TODO)_
- [x] Config file (`~/.config/hrdr/config.toml`), `hrdr models`
- [x] Tool + client unit tests
- [x] Temporary managed `llama-server` backend
- [x] Wrap-aware transcript scrolling
- [x] Message queueing during a running turn
- [ ] infr serve path with tool calling (replaces the temporary backend)
- [ ] Switch hjkl path-deps to registry pins for standalone CI

## License

MIT
