# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed

- **`hrdr-gui` (the floem desktop frontend).** hrdr is TUI-only going forward.
  The `apps/hrdr-gui` crate, its CI job, and the floem-only `cargo-deny`
  advisory exemptions (`paste`, `ttf-parser`, both `quick-xml` DoS advisories)
  are gone. `hrdr-app` remains the UI-agnostic core, shared by the TUI and the
  headless `hrdr run` path.

### Added

- **Session header.** A new `EntryKind::Header` opens every session: the `hrdr`
  wordmark, animated with `hjkl-splash`, beside the version, model, provider,
  effort, and cwd. It stores no data, so the details always reflect live session
  state. The art is owned by the binary (it doubles as the `--help` banner) and
  passed into `hrdr_tui::run`.
- **Full transcript persistence.** Session files now store the whole display
  transcript — the model's reasoning, system notices, the per-turn stats line,
  `/diff` output — plus the status-bar token counters and context window. A
  resume restores what was on screen rather than rebuilding a lossy
  approximation from the chat messages.

### Changed

- **`/clear` takes an optional session name** (as do its aliases `/new` and
  `/reset`): `/new Project X` starts a fresh conversation that saves under that
  name instead of one derived from its first message.
- **Fewer block surfaces.** The session banner, the model's output, and its
  thinking use the terminal's own background; a tool call shares the user
  prompt's; and fenced code inherits whatever block it sits in rather than
  painting a slab of its own. Only the prompt (and tool calls), command output,
  and the stats row are tinted.
- **Unified block rendering.** Every transcript entry renders through one
  `render_block`: two columns of padding either side and one blank row above and
  below, each kind with its own overridable background (header, user, assistant,
  tool, command, stats). Slash-command output renders as markdown in undimmed
  colors; reasoning shares the assistant colors, dimmer. The `#N you` /
  `#N assistant` labels close their block.
- **The `⠋ Thinking` and `Thought: 1.2s` labels render identically** — one label
  row, one blank row, then the thought. The elapsed time is now data on the
  entry (`EntryKind::Reasoning { text, took_ms }`) rather than a string spliced
  into the thought's text, so it no longer passes through markdown or gets
  persisted into the transcript.
- **Fenced code renders at its block's own indentation**, with no language tag
  row above it — it reads as the file's text rather than a framed widget.
- **Blank separator rows only between tinted blocks.** A prompt and the tool
  call it triggered, or two tool calls, would otherwise merge into one slab; a
  block on the terminal background already begins and ends in a blank row.
- **The input cursor blinks**: a bar while inserting, a block in vim's Normal
  mode. `EditorEngine::is_insert()` — long documented as a cursor-shape hint —
  finally drives it. The terminal's own shape is restored on exit and while
  `$EDITOR` has the screen.
- **A `┃` bar down the left of the user's own surfaces** — the prompt block and
  the input pane — in Tokyo Night Moon's magenta (`#c099ff`). Tool calls share
  the prompt's background but not its bar.
- **The footer no longer repeats the keybindings.** It keeps the editor's mode,
  the queue/scroll hints, and the draft's size; the keys moved into `/help`,
  which now lists the active input discipline's own bindings plus the mouse and
  scroll shortcuts.
- **The "follow output" button floats two rows above the input pane**, with an
  arrow at each end.
- **The status bar renders through the block renderer**: two columns of padding
  either side, a blank row above and below.
- **Thinking blocks lost their `⠋ Thinking` / `Thought: 1.2s` label.** The
  dimmer text already says whose voice it is, and the loader says a turn is
  running. The elapsed time is still recorded on the entry.
- **Shell tool blocks drop the `$ ` prompt.** The block's `bash` header says
  what it is; the command renders verbatim.
- **The input pane is borderless**, on the user prompt's background, with one
  blank row above and below and two columns either side — the same chrome a
  transcript block wears. The editor mode and the draft's size moved from the
  border to the help line below.
- **User prompts render like the model's output**: same markdown pipeline, same
  foreground colors. Only the block's background differs. Queued messages too.
- **Tool blocks show tool-specific detail**: the shell command and its output,
  `write`'s path and raw file contents, `edit`/`patch`'s diff, `read`'s tail.
- **One `SessionState` is the on-disk payload.** `Entry` is now `{ kind, time }`
  and doubles as the session file's record; the parallel timestamp vector and
  the duplicate `SavedEntry` type are gone. Saving is a serialize, resuming an
  assignment. `session.rs` moved from `hrdr-agent` to `hrdr-app`.
- **`model` / `provider` precedence is `flag > env > session > config`**,
  honored by `/resume` as well as startup auto-resume. A session never overrides
  a pinned model, and never supplies the endpoint.

### Fixed

- **Toggling a long tool block no longer scrolls the view.** `scroll_offset` is
  measured from the bottom, so collapsing a block kept the view the same
  distance from the end and the block jumped. Its top is now pulled to the top
  of the viewport, as `/goto` does; following the newest output is left pinned.
- **A text-less assistant turn no longer paints an empty block.** When the model
  thinks and calls a tool without emitting any output, the assistant entry has
  no text: it rendered as a lone `#N assistant` label floating over blank
  padding. The block is gone, but the label survives — it is a `/goto` jump
  point — appended to the last non-empty block. A whitespace-only thinking block
  renders nothing at all.
- **The per-turn stats line closes its turn's block** rather than opening one of
  its own, sitting just above the `#N assistant` label.
- **The bundled theme now uses the real Tokyo Night palette.** The purple was
  named `mauve` (a Catppuccin name) while the code looks up `magenta`, so
  `accent2` silently fell back to `blue` — identical to `accent`. `teal` held
  Tokyo Night's `cyan` value, and the six block backgrounds were invented rather
  than palette colors. Every chat role now resolves to an upstream value, and a
  test asserts it (`Theme::load` swallows a parse error and falls back to a
  different palette, so a typo would otherwise ship silently).
- **`--provider` was never recorded.** The preset was resolved and applied, but
  the name was dropped — the status bar showed no provider and every saved
  session recorded `provider: null`.
- **The model's thinking was dropped from session files.** `ChatMessage`'s
  `Serialize` is the OpenAI wire format, which omits `reasoning_content` and
  `anthropic_thinking_blocks`; the session file reused it. Losing the latter
  breaks a resumed Anthropic conversation whose last assistant turn has a
  pending `tool_use`. Session files now encode both, with the wire form
  untouched.
- **Config-watcher storm.** One editor save emits a burst of filesystem events
  (28 on inotify here) and each one reloaded the config and printed a notice.
  Events are now debounced on a 100ms trailing edge.
- **Session notices no longer accumulate.** The welcome banner,
  `resumed session …`, `session saved as …` and other lifecycle chrome are a new
  ephemeral `EntryKind::Notice` that is never persisted; previously each resume
  restored the old ones and appended a fresh copy.
- **Resuming no longer clobbers the endpoint.** `base_url` belongs to the
  process, which is what the "session endpoint was X (current: Y)" notice
  already claimed.
- Long lines wrap inside their block instead of breaking out to column 0.
- The per-turn stats line renders as a block (and lost its `└` prefix).

## [0.2.8] - 2026-07-05

### Added

- **Detached background sub-agents.** `task` gained a `background: true` param:
  the sub-agent runs concurrently while the main agent keeps working — the tool
  returns immediately with a task id, and the result is **delivered into the
  conversation automatically** when it finishes (folded in before the next model
  request, mid-turn or at the next turn). Progress shows live in the sub-agent
  panel (with a ✓ on completion). Backed by a shared `background_tasks` registry
  on `ToolContext`; the run loop delivers + prunes finished tasks. (Background +
  worktree isolation together isn't supported yet.)
- **GUI sub-agent panel + `@agent` mention parity.** The floem GUI now shows the
  live sub-agent panel (blocking sub-agents + detached background tasks,
  click-to-expand) and routes `@agent` mentions to the matching sub-agent. The
  panel model was lifted into `hrdr-app` (`SubAgentPanel`, `PanelItem`,
  `panel_items`) and `prepare_outgoing`/`prepare_outgoing_via` is now the shared
  input→sent transform across the TUI, GUI, steering, and headless paths.
- **Credential guardrails on `read`/`grep`.** A mechanical deny-list refuses the
  hrdr auth store, `~/.ssh`, `.aws/credentials`, `gh/hosts.yml`,
  `*.pem`/`*.key`, and `.env` files
  (`.env.example`/`.sample`/`.template`/`.dist` stay readable), resolving
  `..`/symlink escapes first. `fetch` now caps the response body and blocks
  loopback / cloud-metadata (SSRF) hosts.

### Changed

- **`auto_compact` is now a plain on/off toggle** (config / `$HRDR_AUTO_COMPACT`
  / `--auto-compact` / both frontends). Legacy fractional spellings (`0.85`,
  `0`) still parse for backward compatibility.
- **One shared SSE decoder** (`hrdr_llm::SseDecoder`) backs the OpenAI,
  Anthropic, and MCP streaming paths, replacing three hand-rolled parsers; an
  EOF flush keeps line-lenient servers (ending `data: [DONE]\n` without a blank
  line) working.
- **`chat_stream` borrows the history** (`&[ChatMessage]`) instead of cloning
  the full `Vec` on every tool round and retry.
- Chat-endpoint errors now carry a typed
  `ChatError { status, retry_after, kind }`; retry/compaction match on the kind
  first and fall back to text scanning.
- The TUI batches redraws per streamed burst (was one redraw per token), caches
  transcript rendering, and the sub-agent panel scrolls instead of clipping the
  newest rows. `find` now respects `.gitignore`/`.ignore`.

### Fixed

- **Native Anthropic extended thinking + tool use no longer 400s** on the
  follow-up request: thinking blocks and their signatures are captured while
  streaming and re-emitted first in the assistant message. Two-phase Anthropic
  usage (prompt/cache in `message_start`, completion in `message_delta`) is
  merged instead of clobbered to zero.
- **Streaming resilience:** a transient mid-stream disconnect now retries (not
  just the connect), and a stream that ends without `[DONE]`/`message_stop` is
  treated as an incomplete-stream retry rather than a truncated answer whose
  half-streamed tool-call JSON executes.
- **Guardrails can't be bypassed** by wrapping a blocked command in a nested
  `bash -c '…'` — the payload is re-scanned (depth-capped).
- **The `bash` tool bounds output in memory** (head + tail ring, per-line cap)
  and spills the full output to an overflow file incrementally — no OOM on huge
  output.
- **Background sub-agents** get a read-only tool scope (they share the main
  cwd), keep abortable handles cleared on `/clear`/session reset, and no longer
  wedge as "running" on panic; finished handles are reaped. **Worktree
  sub-agents** clean up their worktree + branch on a cancelled turn (`Drop`) and
  stale worktrees are pruned at startup.
- **Session autosave is atomic** (temp file + fsync + rename). **Checkpoints**
  prune by turn cap + age, GC orphan blobs, and serialize concurrent instances
  with a journal lock (legacy records survive the upgrade).
- **MCP** removes the pending-request id on a send failure (no leak). The
  `$EDITOR` draft uses a `0600` tempfile; a panic hook restores the terminal so
  the message survives the alt screen; async notices no longer yank the scroll;
  the steering queue is cleared on cancel; and startup no longer hangs forever
  on the context-window probe (3s timeout).

## [0.2.7] - 2026-07-05

### Added

- **Live sub-agent panel (TUI).** While `task` sub-agents run (including several
  in parallel), a panel lists each one with its live streaming output.
  Collapsed, a sub-agent shows the tail of its output (a header + last few
  lines); click it to expand the full log, click again to collapse. Finished
  sub-agents drop from the panel (their result lands in the transcript). The
  sub-agent now also streams its answer text to the panel, not just
  tool-activity markers.
- **Worktree isolation for sub-agents.** A profile with `isolation = "worktree"`
  runs its sub-agent in a fresh git worktree on a scratch branch off `HEAD`, so
  its edits don't touch the working tree. If the sub-agent made no changes the
  worktree is torn down automatically; otherwise it's kept and the result points
  at the branch/path to review and merge. Requires a git repo. Config
  `[[subagent]]` and agent files take the `isolation` field.
- **`@agent` mentions.** Typing `@name` where `name` matches a known sub-agent
  (built-in, discovered file, or config) routes that message to the agent — the
  main agent handles it by delegating via the `task` tool. Context-aware: an
  `@token` that isn't a known agent stays a normal `@file` mention, so file
  attach is unaffected. Works in the TUI and `hrdr run`.
- **Proactive sub-agent delegation.** Agents can be marked `proactive` so the
  main agent reaches for them on its own when a sub-task fits their role, rather
  than only when told. The built-in `explore` and `review` agents are proactive
  (explore for broad investigation, review after non-trivial changes); the
  `task` tool lists proactive agents with a ★ and a stronger call-to-action.
  `[[subagent]]` profiles and agent files take a `proactive` flag.

- **Persistent memory (`memory` tool).** The agent can save durable notes that
  survive across sessions, in two scopes — **project** (per working directory)
  and **global** (all projects). Storage is plain Markdown under the XDG data
  dir (`~/.local/share/hrdr/memory/`): an index — `MEMORY.md` (Claude Code
  style) or `index.md` (OKF style), both recognized so memory copied from either
  ecosystem loads without renaming — plus topic files, greppable and
  git-diffable. The bounded index (≤200 lines / 25 KB per scope, like Claude
  Code) auto-loads into the system prompt each session and re-loads after
  `/clear` and compaction, so memory survives context resets; topic files are
  read on demand via `read`/`grep`. Tool actions
  `view`/`write`/`append`/`delete` are confined to the memory store (path
  -traversal guarded). Override the location with `memory_dir` / `--memory-dir`
  / `$HRDR_MEMORY_DIR` to point hrdr at another tool's store; disable with
  `memory = false` / `$HRDR_MEMORY=0`. Distinct from `AGENTS.md`, which stays
  the human-authored, read-only instructions.
- **`--agent <name>` primary-agent mode.** Run the main loop as a named agent —
  it adopts that agent's system prompt, tool scope, model/provider, and knobs,
  rather than only being available for delegation. Resolves from the same set as
  the `task` tool (built-ins + discovered files + config); unlike a delegated
  sub-agent, a primary agent keeps delegation and MCP. E.g.
  `hrdr --agent explore` for a read-only session, or `hrdr --agent plan "…"` to
  investigate and write a plan. New `hrdr_agent::resolve_agent_profiles` /
  `config_for_agent_profile` (the latter renamed from the internal
  `subagent_config_for_profile`).
- **Agents as discoverable files.** hrdr now loads sub-agent definitions from
  Markdown files (frontmatter + body-as-system-prompt), reading both the Claude
  Code and opencode locations plus its own: project `.hrdr/agents/`,
  `.claude/agents/`, `.opencode/agent/` and the matching user dirs
  (`~/.config/hrdr/agents/`, `~/.claude/agents/`, `~/.config/opencode/agent/`).
  Frontmatter maps to the profile fields (name/description/model/provider/
  read_only/tools/write_ext/temperature/effort/max_steps, with
  `maxTurns`/`steps` and `reasoningEffort` aliases). The same agent found in
  multiple locations is **registered once** (first in precedence order wins:
  project before user, hrdr → claude → opencode); overall precedence is
  `[[subagent]]` config > project files > user files > built-ins. No new
  dependencies — a small frontmatter parser, opencode's boolean `tools:` map is
  ignored.
- **Per-agent runtime knobs.** `[[subagent]]` profiles gained `temperature`,
  `effort` (`minimal`/`low`/`medium`/`high`), and `max_steps` (tool-call
  iteration cap) — each inheriting the main agent's value when omitted. Lets a
  profile run, e.g., a careful `high`-effort reviewer or a tightly capped quick
  sub-task.
- **Four built-in agents.** The `task` tool now always offers `explore` (a
  read-only code investigator — trace files, types, and call paths), `review` (a
  read-only code reviewer — bugs, edge cases, security), `plan` (investigates
  read-only, then persists a step-by-step plan as a **Markdown file** — can
  create/edit `.md` only), and `general` (full tool access; the same agent as
  `task` with no `agent`). Each runs on the main provider with a specialized
  system prompt and a scoped tool set.
- **Custom sub-agent personas + tool scoping.** `[[subagent]]` profiles gained
  `prompt` (a system-prompt persona appended to the sub-agent's role),
  `read_only` (scope to the read-only tools), `write_ext` (read-only tools plus
  writes limited to the given file extensions — e.g. `["md"]`), and `tools` (an
  explicit allow-list, overriding the others). A user profile named
  `explore`/`review`/`plan`/`general` overrides the matching built-in. New
  `ToolRegistry::retain_only` / `read_only_names` and
  `ToolContext::write_allow_ext` back the scoping.
- **Parallel sub-agents.** Issuing several `task` calls in one turn now runs the
  sub-agents concurrently (e.g. explore several areas at once), each streaming
  into its own tool block. A new `Tool::concurrent()` signal (defaults to
  `read_only()`) drives the tool-batcher: `task` opts in while staying
  non-read-only; the parent's own file-mutating tools stay a sequential barrier.
- **Large sub-agent results spill to a file.** A concise `task` result is still
  returned inline, but a large one is now saved to a temp file (over the
  `[tool_output]` caps, the same as `bash`/`grep`) and the parent gets a bounded
  preview + a pointer to `read` (with offset/limit) or `grep` it — so a big
  sub-agent report doesn't flood the main context.

- **Sub-agents (`task` tool).** The model can delegate a self-contained sub-task
  to a fresh sub-agent with its own context, keeping the main conversation clean
  — broad exploration, or a focused piece of implementation. Crucially the
  sub-agent can run on a **different model on the same provider** (via the
  tool's `model` argument, or a `subagent_model` / `$HRDR_SUBAGENT_MODEL` /
  `--subagent-model` default) — e.g. an Opus main agent delegating
  implementation to Sonnet. The sub-agent gets the normal tools but can't itself
  delegate (recursion bounded to one level) and doesn't spawn MCP servers; its
  tool activity streams to the parent as live output and its final summary
  becomes the tool result. Disable with `subagents = false` / `$HRDR_SUBAGENTS`.
  (Sub-agent file edits aren't captured by the parent's `/revert` yet — use
  git.)
- **Cross-provider sub-agents (`[[subagent]]` profiles).** A sub-agent can now
  run on an **entirely different provider** than the main agent, not just a
  different model. Named profiles pin a `provider` (built-in or
  `[providers.<name>]`) + `model`; the model selects one with the `task` tool's
  `agent` argument, and the sub-agent runs on that provider's endpoint, key,
  headers, and Azure/Anthropic quirks — e.g. Opus-on-Anthropic manages while
  implementation runs on a model from OpenRouter/Zen. The profiles are listed in
  the tool description so the model knows what it can delegate to.

## [0.2.6] - 2026-07-05

### Added

- **`max_completion_tokens` for OpenAI reasoning models.** `max_tokens` set on
  an o-series or `gpt-5` model is now sent as `max_completion_tokens` (which
  those models require), so setting an output cap no longer 400s on them.
- **1-hour prompt-cache TTL.** `prompt_cache_ttl = "1h"`
  (`$HRDR_PROMPT_CACHE_TTL`) emits the extended `cache_control` TTL — cheaper
  for a stable prompt reused across a longer gap (native Anthropic adds the
  `extended-cache-ttl` beta; OpenRouter passes it through). Default stays
  ~5-minute ephemeral.
- **Request timeout.** `request_timeout` (seconds, `$HRDR_REQUEST_TIMEOUT`) sets
  a connect + idle-read timeout so a hung or stalled provider fails the request
  instead of blocking forever; a slow-but-progressing stream isn't killed.
  Default: no timeout.

- **Opt-in request parameters.** `max_tokens` (now also sent on the OpenAI path,
  not just Anthropic), `top_p`, `seed`, and `stop` are configurable (config /
  `$HRDR_MAX_TOKENS` / `$HRDR_TOP_P` / `$HRDR_SEED`), and `stream_usage = false`
  (`$HRDR_STREAM_USAGE`) omits `stream_options` for the few servers that reject
  it. All default to **not sent**, so no strict provider 400s on an unexpected
  field. (The `reasoning:{}` object form is intentionally not added — hrdr's
  providers accept the `reasoning_effort` field it already sends, and emitting
  both risks a conflict.)

- **Cache-hit and reasoning-token visibility.** Usage now parses the providers'
  `prompt_tokens_details.cached_tokens` and
  `completion_tokens_details.reasoning_tokens` (and Anthropic's
  `cache_read_input_tokens`), and the per-turn stats line shows them — e.g.
  `… · ctx 1200 (in/out 1200/400, 3.0:1) · 900 cached · 120 reasoning` — so you
  can see the prompt cache and extended thinking actually working. Also exposed
  on `hrdr run --json` usage events.

- **Azure OpenAI support.** Set `api_version` on a provider and hrdr appends
  `?api-version=<v>` to requests and authenticates with an `api-key` header
  instead of `Bearer` (Azure is the OpenAI chat-completions wire, just a
  different URL + auth). Point `base_url` at
  `https://<resource>.openai.azure.com/openai/deployments/<deployment>`. Applied
  at startup and on a `/provider` switch.

## [0.2.5] - 2026-07-04

### Added

- **Custom per-provider HTTP headers.** `[providers.<name>.headers]` sends
  arbitrary headers with every request to that provider (e.g. OpenRouter's
  `HTTP-Referer`/`X-Title`, or a custom auth/routing header). Applied at startup
  and on a `/provider` switch, on both the OpenAI and native Anthropic backends.
- **Truncation warning.** When a reply hits the model's output cap
  (`finish_reason: "length"`, or Anthropic's `max_tokens`), hrdr now surfaces a
  notice so a silently cut-off answer or edit isn't mistaken for a complete one.
- **`Retry-After` is honored.** On a `429`/`503`/`529`, hrdr now backs off for
  the server-requested `Retry-After` seconds (clamped to 60s) instead of its
  fixed exponential schedule, reducing repeat rate-limit hits.
- **Extended thinking on the native Anthropic backend.** `/effort`
  (`minimal`/`low`/`medium`/`high`) now turns on a Claude `thinking` budget that
  scales with `max_tokens` (always leaving room for the answer); the
  interleaved-thinking beta is enabled alongside tools so Claude can reason
  between tool calls, and `thinking_delta`s stream to hrdr's reasoning pane.
  Temperature is sent only when thinking is off (Anthropic requires the default
  while thinking).
- **`max_tokens` config knob** (`max_tokens` / `$HRDR_MAX_TOKENS`, default 8192)
  caps output on the native Anthropic backend — raise it for longer replies and
  deeper thinking (the OpenAI path ignores it).
- **`529 Overloaded` retries.** Anthropic's overloaded errors (HTTP `529` and
  the mid-stream `overloaded_error`) are now treated as transient, so they back
  off and retry instead of failing the turn.

## [0.2.4] - 2026-07-04

### Added

- **Native Anthropic Messages API backend.** hrdr now talks to Claude over
  Anthropic's native `/v1/messages` API (auto-selected when the endpoint host is
  `api.anthropic.com`) instead of its OpenAI-compat endpoint. A new backend in
  `hrdr-llm` translates hrdr's OpenAI-shaped history to the Anthropic wire
  format (`system` hoisted to top-level blocks, `text`/`tool_use`/`tool_result`
  content blocks with consecutive same-role turns coalesced, `tools` with
  `input_schema`, required `max_tokens`, `x-api-key` + `anthropic-version`
  headers) and normalizes the streaming response back into the same
  `ChatChunk`/accumulator the agent already uses, so the loop and both frontends
  are unchanged. This **unlocks native prompt caching on Claude**:
  `cache_control` breakpoints land on the system prompt, the last tool, and the
  last message, and `prompt_cache = "auto"` now enables caching for the native
  Anthropic backend (as well as OpenRouter). Extended thinking is a planned
  follow-up.

## [0.2.3] - 2026-07-04

### Fixed

- **Prompt caching no longer breaks non-caching providers by default.** In 0.2.2
  `prompt_cache = "auto"` enabled `cache_control` breakpoints for _every_ remote
  endpoint, but OpenAI, Groq, and xAI **reject** an unknown `cache_control`
  field with a `400` (and OpenCode Zen does for GLM/Zhipu models), so the
  default could break every request on those providers. `auto` now enables the
  marker **only for OpenRouter** — the one endpoint that consumes it safely (it
  strips the field for models that don't accept it). Other providers cache
  automatically or ignore the marker; force it anywhere with
  `prompt_cache = "on"`. Also corrected the docs: Anthropic caching is only on
  its native Messages API, not the OpenAI-compatible endpoint hrdr uses.

## [0.2.2] - 2026-07-04

### Added

- **Reasoning effort is now sent to the model.** `/effort`, `--effort`, and
  `effort` in config set `reasoning_effort` on each request when the value names
  a reasoning level (`minimal`/`low`/`medium`/`high`) — previously it was only a
  status-bar label. Other labels stay display-only; the value follows model and
  provider switches, and `/effort` reports whether it's actually sent. `/info`
  unchanged.
- **Prompt caching.** hrdr now marks `cache_control` breakpoints on each request
  — one on the system prompt, one rolling on the last message — so the stable
  system+tools prefix and the growing conversation prefix are cached across
  turns (Anthropic natively, or Anthropic/Gemini via OpenRouter; other providers
  ignore the marker). Controlled by `prompt_cache = "auto" | "on" | "off"`
  (config), `$HRDR_PROMPT_CACHE`, or `--prompt-cache`; `auto` (default) enables
  it for remote endpoints and skips local servers. `/info` shows the active
  state.
- **MCP resources & prompts.** When a server advertises `resources` / `prompts`
  capabilities, hrdr exposes them as extra tools: `<name>_list_resources` +
  `<name>_read_resource` (`resources/list` + `resources/read`) and
  `<name>_list_prompts` + `<name>_get_prompt` (`prompts/list` + `prompts/get`).
- **MCP legacy HTTP+SSE transport.** Set `url` plus `transport = "sse"` to use
  the two-endpoint SSE flow: hrdr opens the persistent GET stream, waits for the
  server's `endpoint` event, then POSTs requests there and routes responses back
  off the stream by id.
- **MCP over HTTP (Streamable-HTTP transport).** `[[mcp]]` servers can now be
  remote: set `url` (instead of `command`) plus optional `[mcp.headers]` for
  auth. hrdr POSTs JSON-RPC to the endpoint, handling both `application/json`
  and SSE (`text/event-stream`) responses and echoing the server's
  `Mcp-Session-Id`. stdio and HTTP share one client behind a transport
  abstraction; `command` is now optional (exactly one of `command`/`url`).
- **MCP end-to-end tests across all transports.** A mock server exercises
  tools + resources + prompts over stdio, Streamable-HTTP, and legacy HTTP+SSE —
  including error-tool (`isError`) propagation, JSON-RPC `error`-object
  propagation, non-read-only tools, non-text (image) tool content, binary (blob)
  resources, concurrent id-routing, `Mcp-Session-Id` resend, capability gating
  (absent `resources`/`prompts` omit their op-tools), and empty-list
  placeholders.

- **`patch` tool — apply a unified diff across multiple files in one call.**
  Takes git/patch format (`--- a/… / +++ b/… / @@` hunks; `/dev/null` to
  create/delete), applied via `diffy` with hrdr's confinement, read-before-edit
  gate, checkpoints, and hooks. **Atomic**: if any file's hunks don't apply,
  nothing is written. Far fewer round-trips than repeated `edit` for multi-site
  changes.

- **`ls` tool** — list one directory's entries (dirs get `/`, symlinks `@`).
  Complements `find` (tree search by glob).

- **MCP client (stdio transport).** Connect
  [Model Context Protocol](https://modelcontextprotocol.io) servers via
  `[[mcp]]` config entries (`name`, `command`, `args`, `env`, `disabled`); hrdr
  spawns each at startup, runs the JSON-RPC handshake, discovers its tools
  (`tools/list`), and registers them namespaced `<name>_<tool>` so the model can
  call them (`tools/call`) alongside the built-ins. Tools flagged `readOnlyHint`
  batch concurrently. A failing server is skipped with a status line; the rest
  still load. Works in the TUI, GUI, and `hrdr run`. v1 is stdio-only
  (HTTP/SSE + resources/prompts are follow-ups).

- **Steering — course-correct a running turn (pi-style).** Submit a message
  while a reply is in flight and it's now delivered to the model **after the
  current tool round**, instead of waiting for the whole turn to finish. The
  running `Agent::run` drains a shared steering queue between rounds and
  continues after a text response if you've steered; a new `Steered` event marks
  delivery (surfaced on `hrdr run --json` as a `steer` event). Works in the TUI
  and GUI; messages submitted mid-compaction still queue as before.

- **Provider-aware context-overflow detection.** `is_context_overflow` now
  recognizes ~20 backends' "prompt too long" wordings (Anthropic, OpenAI,
  Gemini, xAI, Groq, OpenRouter, Together, Mistral, Kimi, z.ai, Copilot, …),
  ported from pi's `overflow.ts`, so overflow-triggered compaction fires on far
  more servers. Rate-limit/throttling errors are now explicitly excluded so a
  429 retries instead of compacting.

- **Context management brought to parity with opencode.** Building on
  tool-output pruning and the truncate-to-file layer: (1) per-tool truncation
  now caps on **lines and bytes** (whichever first), both configurable via
  `[tool_output]` `max_lines` (2000) / `max_bytes` (51200); (2) the prune
  protect/minimum windows now match opencode's 40k/20k; (3) compaction keeps the
  recent tail by **turns and a token budget** (`compaction_tail_turns` = 2,
  `preserve_recent_tokens` = 8000) instead of a fixed message count; (4)
  auto-compaction now triggers on a **reserved token buffer** —
  `context_window − compaction_reserved` (default 16384, `--compaction-reserved`
  / `$HRDR_COMPACTION_RESERVED`) rather than a fixed 85% fraction
  (`auto_compact` is now the on/off toggle). The reserve is clamped to a quarter
  of the window so small-context models still get a sane trigger. All tunable in
  `config.toml`.

- **Truncated `bash`/`grep` output is saved, not discarded.** When output
  exceeds the per-tool cap, the full result is written to a temp file
  (`<tmp>/hrdr-tool-output/`, read-whitelisted so the cwd-confined tools can
  reach it) and the truncated reply points the model at it — "read_file it (with
  offset/limit) or grep it for the rest, don't re-run." Previously the overflow
  was lost, forcing a re-run to recover the tail. Files older than 7 days are
  pruned on write. (`bash` keeps head+tail, `grep` keeps the head.)

- **Tool-output pruning — bound context without a model call.** Before each
  request, tool-call _output_ older than a recent window is cleared from the
  model history (replaced with a short placeholder; the tool call + args stay).
  The most recent `PRUNE_PROTECT_TOKENS` (16k) of tool output and the last 2
  turns are always kept, and pruning only fires when it would reclaim at least
  `PRUNE_MINIMUM_TOKENS` (8k). This is the cheap first line of defence against
  tool results ballooning context, ahead of the (expensive) auto-compaction.
  Only the model-facing history is touched — the TUI/GUI transcript keeps the
  full output. On by default; toggle with `auto_prune` in config,
  `HRDR_AUTO_PRUNE`, or `--auto-prune on|off`.

- **`/login` — a guided provider + API-key wizard.** Run `/login` in the TUI or
  GUI, pick a provider from the list, and paste its API key; hrdr switches to it
  live and makes it the default for next launch. Keys are stored **separately
  from `config.toml`**, in a dedicated `~/.config/hrdr/auth.toml` (`0600` on
  unix); the wizard shows the exact path and a plaintext-storage warning before
  saving, and the entered key never touches the transcript or input history.
  Startup key resolution is now **inline config → `key_env` → saved
  credential**. Shared core, so both frontends get the same flow.

- **`/info` now shows a `messages: N` line** — the raw conversation-history
  length (system prompt + every turn and tool result). Surfaced through the
  shared command core, so it appears in both the TUI and the GUI.

- **First-run guidance when the endpoint is unreachable.** The startup
  health-check warning now explains how to get hrdr talking to a model — start a
  local server (`infr serve …` / `llama-server …`) listening at the configured
  URL, or switch to a hosted provider with `/provider <name>` after setting its
  API key. Shared by the TUI and GUI. Fills the onboarding gap left by removing
  the built-in server spawner.

### Changed

- **Tool names shortened (breaking).** `read_file`→`read`, `write_file`→`write`,
  `web_fetch`→`fetch`, `web_search`→`search`, `todo_write`→`todo`, and `glob` is
  replaced by `find`. Update any `[[hooks]]` `on = "write_file"` to
  `on = "write"`, and any custom prompts.

- **Internal DRY/YAGNI cleanup of `hrdr-agent` (no behaviour change).** The
  single-call and concurrent tool paths are now one path (a lone mutating call
  is a one-element batch); a shared `drain_stream` helper backs the turn loop,
  the wrap-up round, and the silent compaction call; `config_file_path`,
  `load()`, and the `is_transient`/`is_context_overflow` error classifiers were
  deduplicated; and the unused `run_tool_streaming`, `session_dir` export, and
  `CheckpointInfo`/`FileCheckpoints` re-exports were removed. Net ~160 fewer
  lines.

### Removed

- **Local model-server spawning (breaking).** hrdr no longer launches or manages
  a model server — the `infr serve` / `llama-server` bootstrap and its
  `apps/hrdr/src/backend.rs` module are gone, along with the `--no-backend`,
  `--backend-model`, `--backend-bin`, `--backend-ctx`, and `--backend-arg`
  flags. hrdr now only talks to an already-running OpenAI-compatible endpoint:
  select one with a `--provider` preset or `--base-url`, and start your own
  server (infr, llama.cpp, vLLM, …) if you want one locally. The `local` /
  `infr` preset still defaults to `http://localhost:8080/v1`, so a
  locally-running server needs no flags.

### Fixed

- **The endpoint's advertised max context is honored everywhere.** The GUI now
  probes the model's context window at startup like the TUI/headless paths did
  (previously it only used a configured value), and both frontends **re-probe**
  after a `/model`, `/retry <model>`, or `/provider` switch so the
  auto-compaction threshold and the "X of Y" gauge track the current model's
  real limit instead of a stale one. An explicit `context_window` (config or
  provider preset) still wins.

- **Verbatim-retry breaker messages no longer contain stray whitespace.** The
  refusal and nudge strings had runs of literal spaces (missing line
  continuations), so the model saw
  `…failed 2 times                      in a row…`. Cleaned up to normal prose.

## [0.2.1] - 2026-07-03

### Fixed

- **Alpine package builds again.** `abuild` rejects uncompressed man pages; the
  `APKBUILD` now gzips `hrdr.1` on install. (The 0.2.0 apk publish failed on
  this; every other channel shipped.)

## [0.2.0] - 2026-07-03

### Added

- **GUI tool blocks: the whole block is the click target.** Clicking anywhere on
  a tool call (header, output, result) toggles its expansion — previously only
  the header line was clickable — with a hover background as the affordance.
  Matches the TUI, where any visible part of a tool block has always been
  clickable.

- **`grep` gains a `context` param (`-C` style).** `context: 2–3` returns the
  lines around each match — matches as `path:NN:line`, context as
  `path-NN-line`, `--` between groups (the standard grep/rg `-C` format, all
  three backends; the built-in walker merges overlapping windows). Saves the
  follow-up `read_file` round-trip per investigated hit. With context on, the
  match cap drops 200 → 50 (each match is a whole window), and the cap now
  counts only match lines, so context lines never eat the budget.

- **Verbatim-retry breaker.** When the exact same tool call (name + args) fails
  twice in a row, the second failure carries a "change the input or approach"
  nudge and a third attempt is refused without executing — the classic
  small-model loop (same wrong `old_string`, forever) now self-terminates. Any
  intervening different call or a success resets the streak, so legitimate
  `test → edit → test` retry cycles are never blocked. Applies to both the
  sequential and the concurrent (read-only batch) paths.

- **Headless `hrdr run` grows scripting flags.** `--json` streams
  newline-delimited JSON events on stdout (`text`/`reasoning`/`tool_start`/
  `tool_output`/`tool_end`/`notice`/`usage`/`done`, plus `error` before a
  non-zero exit); `--quiet` suppresses the stderr tool/usage chrome;
  `--max-steps <N>` bounds the tool-round budget per run.

- **Wire-level debug logging (`HRDR_LOG_REQUESTS=<path>`).** Every chat request
  body, raw SSE data line, and non-2xx response body is appended to the file as
  one JSON object per line — for debugging harness ⇄ server disagreements
  (tool-call framing, stream shape). Off unless the env var is set.

- **Compaction keeps the recent tail verbatim.** `/compact` (and
  auto-compaction) now summarizes only the older part of the conversation and
  keeps the last ~6 messages word-for-word after the summary — compaction
  usually fires mid-task, and a summary alone loses exactly the detail the model
  is working with. The split never separates a tool result from its assistant
  `tool_calls` message (strict servers reject orphans), and when everything is
  already recent the pass is a no-op instead of churn.

- **Post-edit hooks (`[[hooks]]` in config).** Run a shell command after
  `edit`/`write_file` mutates a matching file — formatters, mostly
  (`on`/`glob`/`run` with `{path}` substitution + per-hook `timeout_ms`). The
  tool re-reads the file after hooks run, so the diff the model sees (and the
  text its next `old_string` must match) is the post-hook content. Failing or
  hung hooks become warnings in the tool result, never errors; hook changes land
  in the same per-turn checkpoint, so `/revert` undoes both.

- **Shell completions + man page** (mirroring gpur's packaging helpers). Hidden
  `--completions <bash|zsh|fish|powershell|elvish|nushell>` and `--man` flags
  emit to stdout; the release pipeline attaches a `completions-man.tar.gz` to
  every GitHub Release; the AUR package installs bash/zsh/fish completions +
  `hrdr(1)` generated from the shipped binary, and the Homebrew formula does the
  same via `generate_completions_from_executable`. The CI smoke job verifies all
  six shells + the man page generate cleanly on every PR. The `.deb` and `.rpm`
  packages carry bash/zsh/fish completions + the man page as assets (generated
  in CI before packaging; zsh lands in `vendor-completions` on Debian,
  `site-functions` on rpm), the Alpine `APKBUILD` installs them from the shipped
  musl binary like the AUR package, and the Scoop manifest's install notes show
  how to enable PowerShell completions from `$PROFILE`.

- **Read-only tool calls run concurrently.** When the model requests several
  tools in one round, runs of consecutive read-only calls (`read_file`, `grep`,
  `glob`, `web_fetch`, `web_search`) now execute in parallel; a mutating call
  (`bash`, `edit`, `write_file`, `todo_write`) is a barrier and runs alone, so a
  read after a write still observes the write. Streamed output stays attributed
  per call and results land in call order. New `Tool::read_only` trait flag /
  `ToolRegistry::is_read_only`.

- **Graceful `max_steps` exhaustion.** With 3 tool rounds left in a turn the
  model is warned ("finish up and summarize", appended to the round's last tool
  result); when the budget runs out the harness runs one final **no-tools**
  round so the model must answer in text — the turn ends with a summary of where
  things stand instead of the old hard `agent exceeded max_steps` error.

- **`/prompt` and `/guardrails` introspection commands** (both frontends, via
  the shared layer). `/prompt` (alias `/system`) shows the rendered system
  prompt currently in effect — handy for tuning `AGENTS.md` and checking the
  OS/package-manager line. `/guardrails` (alias `/rails`) lists the active shell
  rules — built-ins plus `[[guardrails]]` config extras — with each pattern's
  corrective message.

- **Three more teaching fixes in the tools.** `bash` states that `cd` does not
  persist between calls (each call starts fresh in the cwd — chain `cd sub && …`
  in one command); `read_file` on a binary file explains itself ("not a text
  file — inspect via bash `file`/`hexdump`") instead of a raw UTF-8 error;
  `glob` says it's also the directory-listing tool (pattern `src/*`).

- **The system prompt names the actual platform.** The OS line now carries the
  distro (`PRETTY_NAME` from `/etc/os-release`) and the system package manager
  found on PATH — e.g. `linux (Arch Linux) — system package manager: pacman`,
  `macos — system package manager: brew`,
  `windows — system package manager: winget` — so "install X system-wide"
  reaches for the right tool instead of guessing apt everywhere.

- **The curl-pipe-shell guardrail is platform-aware, and covers PowerShell.**
  The recovery example is built at startup for the running machine — the real
  temp dir plus the OS-native fetch command
  (`curl -fsSL <url> -o /tmp/script.sh` on unix,
  `Invoke-WebRequest <url> -OutFile %TEMP%\script.ps1` on Windows). The
  PowerShell download-pipe-execute spellings
  (`iwr`/`irm`/`Invoke-WebRequest`/`Invoke-RestMethod` piped into
  `iex`/`Invoke-Expression`) are now blocked too, with the same message.

- **Shell output truncation now keeps the tail.** Long build/test output ends
  with the failure summary; the old head-only 30 KB cut dropped exactly what the
  model needed. `bash`/`powershell` results now keep ~1/5 head + ~4/5 tail with
  a `[… N bytes omitted from the middle …]` marker
  (`hrdr_tools::truncate_middle`). `read_file`/`grep` keep head-truncation
  (pageable, deterministic). Timeout kills now suggest the recovery ("raise
  timeout_ms or run a narrower command").

- **Grep match cap.** A single `grep` call returns at most 200 matches, ending
  with `… [N more matches — narrow the pattern or scope with path/glob]` instead
  of silently flooding the context (all three backends: ripgrep, POSIX grep,
  built-in walker).

- **More guardrails: whole-tree deletes and curl-pipe-shell.** `rm` aimed at
  `/`, `/*`, `~`, `$HOME`, `.`, `..`, or a bare `*` is rejected (specific paths
  like `rm -rf target/` stay allowed), with or without a `sudo` prefix — `sudo`
  itself stays permitted for user-requested system tasks, but can't launder a
  blocked command. `curl/wget … | sh` is rejected with the recovery spelled out:
  download to a temp file, review it, then run it.

- **File mutations confined to the working directory.** `write_file`/`edit`
  refuse paths outside the cwd (resolved through `..` and symlinks via
  nearest-existing-ancestor canonicalization); the system temp dir is always
  allowed for scratch. New config knob `allow_outside_cwd = true` /
  `$HRDR_ALLOW_OUTSIDE_CWD` lifts the restriction.

- **Edit near-match hint.** When `old_string` isn't found but a
  whitespace-normalized match exists, the error says so ("a near-match differing
  only in whitespace/indentation exists") instead of the generic stale-file
  message — the #1 edit-retry cause on small models.

- **System prompt: failure discipline + economy + safety.** New lines: never
  re-run an identical failed call; read only what you need (narrow greps,
  offset/limit); end with a short what-changed/how-verified summary. New Safety
  section stating the mechanical limits (cwd confinement, sudo only on user
  request, no curl-pipe-shell).

- **Shell guardrails — mechanical enforcement of the git rules.** The `bash` /
  `powershell` tools now reject the classic foot-guns before they run, each with
  a corrective error the model learns from at the moment it matters: blanket
  staging (`git add -A` / `--all` / `.` → "stage the files you actually
  changed"), force-push (`--force-with-lease` allowed), hook skipping
  (`--no-verify`, `commit -n`), destructive commands (`reset --hard`,
  `clean -f`, `checkout`/`restore .`), and interactive commands that need a TTY
  (`rebase -i`). Quoted arguments are blanked before matching so
  `rg 'git add -A'` doesn't false-positive. User rules stack on top via
  `[[guardrails]]` (`pattern` + `message`) in `config.toml`.

- **Read-before-edit gate.** `edit` and `write_file` refuse to mutate an
  existing file the model hasn't read this session ("call read_file first"),
  killing blind edits against guessed content — the top source of corrupt
  patches on small models. A file the model itself wrote counts as read; the
  gate resets on `/clear`, `/resume`, and compaction, since those drop the file
  contents from the model's context.

- **System prompt rewritten for small models.** Tool descriptions are no longer
  duplicated into the prompt (they already ship natively as function defs — the
  old template paid those tokens twice); only a one-line name list remains. In
  their place: an editing section (copy `old_string` exactly from `read_file`
  output, strip line-number prefixes, re-read on failure, don't re-read after
  success) and a git section stating exactly what the guardrails enforce. Tool
  descriptions and `edit` failure messages were sharpened to teach the same
  rules (`old_string not found` now says "re-read the file and copy the exact
  current text").

- **GUI finish nudge — desktop notification as the bell.** The GUI now honors
  the `bell` config knob: when a turn finishes (or fails) after running at least
  5 seconds, it posts a desktop notification (`notify-rust`: D-Bus/XDG on Linux,
  Notification Center on macOS, toasts on Windows) — the GUI's equivalent of the
  TUI's terminal `BEL`. The enabled-plus-minimum-duration gate is shared
  (`hrdr_app::should_bell` / `BELL_MIN_SECS`), the knob hot-reloads with the
  rest of the config, and quick replies stay silent in both frontends.

- **DRY audit follow-up — one code path for a dozen more TUI/GUI behaviors.**
  - `CommandHost` gained a `line_poster` channel primitive; `spawn_line` /
    `spawn_diff` (including the diff-vs-status routing rule) are now trait
    defaults, and `/compact` is a default over a new `start_compaction` hook —
    both frontends dropped their duplicated spawn/compact plumbing.
  - Shared helpers/strings: `cancel_message`, `session_saved_notice`,
    `clipboard_copy_status` / `clipboard_read_text`, `agent_cwd`,
    `expand_msg::*`, `startup_config_warning` + `PROJECT_DOCS_LOADED_MSG`,
    `RELOAD_MANUAL_MSG` / `RELOAD_HOT_MSG` / `reload_invalid_message`, and the
    `INPUT_MAX_ROWS` / `TOOL_ARGS_PREVIEW` layout constants (the GUI input now
    caps at 5 rows like the TUI).
  - The GUI shows the TUI's startup notices (invalid-config warning, "loaded
    project instructions from AGENTS.md").
  - GUI `/expand all` is sticky like the TUI: new tool calls spawn expanded
    until `/expand off`.
  - GUI `/reload` + hot-reload now re-apply the agent-side knobs too (effort,
    `auto_compact`, temperature) through one `apply_config_reload` path that —
    like the TUI — keeps current settings and warns on an invalid file instead
    of resetting to defaults.

- **Shared `/resume` core (`hrdr_app::resume_plan` + `RESUME_BUSY_MSG`).** One
  place decides the cwd to adopt and the notices to show (resumed line, `cwd →`,
  missing-cwd note, endpoint note); the shared dispatcher now guards `/resume`
  against a running turn in both frontends (the GUI previously let a mid-turn
  resume race the in-flight autosave). Resuming in the GUI also refreshes the
  dir/branch status chrome and invalidates the `@file` index when it follows the
  session's directory, like `/cwd`.

- **Shared `/find`/`/next`/`/prev`/`/goto` state machine
  (`hrdr_app::FindState` + `goto_action` + `FindAction`, unit-tested).** All
  parsing, match cycling, wrap-around, and status lines live in `hrdr-app`; each
  frontend only maps the resulting action to its scroll primitive
  (`pending_goto`/offset in the TUI, the ViewId registry in the GUI).
  `/goto end` now means the same thing in both: follow the very bottom of the
  transcript (the GUI used to stop at the last user/assistant message).

- **Shared color semantics (`hrdr_app::ThemeSlot`).** The status-bar role →
  color decisions (`status_role_style`), the diff-line coloring
  (`diff_kind_slot`), and the context-gauge level color (`ctx_level_slot`) are
  now single shared tables; each frontend keeps only one eight-line
  slot-to-theme-color map, so the two UIs can't drift on what a role looks like.

### Fixed

- **GUI:** a stale `Done` message from a just-cancelled turn no longer clobbers
  the next turn's state; cancelling an `/init` turn clears the pending
  doc-reload; per-turn token counts include reasoning tokens; `/reload` actually
  re-applies settings (a refactor had left it a no-op).
- **TUI:** completed-TODO aging is driven by finished turns again (it had
  stopped advancing after an event-loop refactor), and auto-compaction uses the
  shared threshold check.
- **TUI:** `/clear` clears the agent synchronously when it's idle (no more
  racing a spawned clear against the next autosave).

- **Colored `/diff` in the GUI — and one shared diff classifier.** The GUI
  renders `/diff` output as a monospace block on the code-panel background with
  +/− line coloring (adds green, removes red, `@@` hunks in the user accent,
  headers dim), routed through its `spawn_diff` override exactly like the TUI
  (status/error lines stay plain). The line classification is shared
  (`hrdr_app::classify_diff_line`/`DiffLineKind`, unit-tested); the TUI's color
  mapping now uses it too. GUI `/copy all` includes diff blocks, matching the
  shared transcript export.

- **`/info` unified at the TUI's richer level.** One shared implementation shows
  session id/name, model, endpoint, cwd + git branch, context used/window,
  session ↑/↓ tokens, temperature, and effort in both frontends (new read hooks:
  `session_label`, `context_usage`, `context_window`, `session_tokens`). The
  TUI's local `/info` arm is deleted; the GUI's short model/messages/cwd form is
  replaced by the full report.

- **Four more TUI behaviors unified into shared code paths — the GUI gains all
  of them:**
  - **Per-turn stats line** (`hrdr_app::turn_stats_line`, unit-tested): both
    frontends append `✓ N tok · tok/s · elapsed · ttft · ctx (in/out, ratio)`
    after every turn; the GUI counts streamed tokens per turn like the TUI.
  - **Config hot-reload** (`hrdr_app::watch_config` + `config_mtime`): one
    watcher — OS-level (inotify/FSEvents, catching atomic renames) with a 2s
    mtime-polling fallback — pings each frontend's channel; both dedup
    self-inflicted writes (persisting a setting) via the same mtime guard. The
    TUI's bespoke watcher + event-loop polling are deleted; the GUI now
    hot-reloads theme/thinking/timestamps/statusbar/todo-ttl on external edits,
    and its `/reload` shares the exact application path (`apply_ui_config`) and
    also refreshes `AGENTS.md`.
  - **Startup endpoint health check** (`hrdr_app::endpoint_health_warning`): the
    GUI now warns at launch when the endpoint is unreachable or doesn't
    advertise the configured model, with the TUI's exact messages.
  - **`/init` doc reload** (`hrdr_app::reload_project_docs` + a `mark_init_turn`
    host hook): the TUI's local `/init` arm is deleted — the shared command
    marks the turn in both frontends, and when it completes, both load the fresh
    `AGENTS.md` into the system prompt.

- **Unified compaction, and the GUI auto-compacts.** The compaction core is
  shared (`hrdr_app::run_compaction` + `compaction_message` +
  `should_auto_compact`): `/compact` now behaves identically in both frontends
  (runs like a turn — input queues behind it, Esc/Stop cancels it — then shows
  the same result line, drops stale context usage, autosaves, and resumes queued
  sends), and the GUI gains the TUI's **proactive auto-compaction**: when a turn
  ends with the context past the `auto_compact` fraction of the window, a
  summarization pass runs before the next queued message. The TUI's bespoke
  threshold check and result formatting were replaced by the shared versions
  (its local `/compact` arm is deleted; "nothing to compact yet" is now detected
  from the result instead of a pre-check).

- **Renderer-agnostic `EditorEngine` seam.** The editing-discipline trait no
  longer names a UI toolkit: keys arrive as `hrdr_editor::EditorKey` (hjkl's own
  toolkit-neutral `Input {key, ctrl, alt, shift}` DTO, re-exported), and the
  ratatui painting moved to a separate `TuiRender` half (the TUI hosts
  `dyn TuiEditorEngine = EditorEngine + TuiRender`). The terminal adapter is one
  function (`key_from_crossterm`, which also owns key-release filtering);
  `VimEngine` and `PlainEngine` no longer touch crossterm. This unblocks hosting
  the vim discipline in the GUI — a floem key adapter + render adapter is now
  all that's missing.

- CI lints `hrdr-gui` on Linux (own cache key + floem's system deps) — the GUI
  was excluded from every workspace job, so a TUI-side refactor could silently
  break it.

- README refresh: install section (cargo/Homebrew/AUR/Scoop/deb/rpm/apk +
  release binaries), `hrdr-app`/`hrdr-gui` in the workspace table, roadmap
  brought up to date (shared core, GUI parity, release pipeline).

- **GUI multi-line input.** The single-line `text_input` is replaced by floem's
  text editor (gutter hidden, auto-growing 1–6 rows like the TUI's input):
  **Enter sends; Shift+Enter / Alt+Enter — and Enter after a trailing `\` —
  insert a newline**, matching the TUI's plain-input conventions. Up/Down still
  recall history, but only while the input is single-line (multi-line editing
  keeps them as cursor moves — same rule as the TUI); Esc still cancels the
  running turn. The editor document syncs two-way with the `input` signal, so
  history recall, `/undo`, `/add`, `/paste`, and completion clicks keep working
  unchanged.

### Changed

- **Incremental code-block highlighting.** A streaming code block used to be
  re-highlighted in full by syntect on every frame (TUI) / every token (GUI). A
  shared `hrdr_app::HighlightCache` now resumes parser+highlight state across
  appends: only new complete lines are highlighted (the partial tail line is
  done on cloned state and redone next append), with a prefix-match LRU so
  finished blocks are pure cache hits. Both frontends use it; a test asserts the
  incremental path is span-identical to one-shot highlighting.

- **`@file` index builds off the UI thread.** The first `@` mention ran
  `walk_files` (up to 20k directory entries) synchronously on the UI thread in
  both frontends, stalling a frame. It now runs on a blocking task
  (`hrdr_app::spawn_file_index`) and lands via the frontend's channel; the popup
  fills in when ready, and `/cwd` / `/revert` re-arm the rebuild.

## [0.1.0] - 2026-07-02

### Added

- **Release pipeline**, mirroring gpur's: pushing a `v*` tag now builds the
  `hrdr` binary for 7 targets (Linux gnu/musl × x86_64/aarch64 via
  cargo-zigbuild, macOS arm/intel, Windows), packages tar.gz/zip (+ `.deb` and
  `.rpm` for Linux gnu) with sha256s, publishes a GitHub Release, then fans out:
  crates.io (all workspace crates in dependency order, idempotent), AUR
  (`hrdr-bin`), the Homebrew tap, the Scoop bucket, and an Alpine `.apk`
  attached to the release. Every main push dry-runs the build matrix so
  packaging breakage surfaces before a tag. New CI jobs also run `cargo-deny`
  and a build+`--version`/`--help` smoke on all three OSes; `deny.toml` gains
  `BSL-1.0` (clipboard-win/error-code) and the floem-tree unmaintained ignores
  (`paste` via wgpu/metal, `ttf-parser` via cosmic-text). Packaging templates
  live under `pkg/` (`aur`/`homebrew`/`scoop`/`alpine`), and `apps/hrdr` carries
  the `cargo-deb`/`cargo-generate-rpm` metadata.

- **`/theme` works in the GUI — full command parity reached.** The GUI theme is
  now a signal: `/theme <path>` live-swaps to an hjkl theme TOML and `/theme`
  resets to the bundled default, exactly like the TUI. Top-level chrome recolors
  reactively; transcript items, the TODO panel, and the status bar (whose colors
  are captured when their views are built) rebuild via a theme revision baked
  into their dyn_stack keys. The command moved to the shared dispatcher
  (`set_theme` + `unpersist_setting` host hooks, persisted to config); the GUI's
  `/reload` now applies a changed theme live too. **`TUI_ONLY_COMMANDS` is
  empty** — every registered command works in both frontends.

- **`/edit` works in the GUI**, opening the file in the system's default editor
  (`xdg-open` on Linux/BSD, `open` on macOS, `start` on Windows, detached). The
  command moved to the shared dispatcher with an `open_editor` host hook whose
  default is the OS opener (`hrdr_app::open_system_handler`); the TUI keeps its
  local terminal-suspending `$EDITOR` flow, unchanged.

- **Agnostic status bar.** The status-bar _content_ — which sections exist (cwd,
  branch, ↑/↓ session tokens, the context gauge with its green/amber/red fill,
  model, effort, ttft), their text, drop priorities, and color roles — now lives
  once in `hrdr-app` (`status_sections`/`StatusSeg`/`StatusRole`); each frontend
  only does layout and maps roles onto its theme. The GUI's status bar goes from
  a single dim text line to the TUI's full section set (including the context
  gauge and git branch, with new accent colors resolved from the theme), and
  `/statusbar` is now a **shared command** working in both frontends: `none`
  hides the bar, `truncate` keeps one row, `wrap` lets sections flow onto
  multiple rows (terminal width-fitting in the TUI, flex-wrap in the GUI). The
  TUI's bar additionally gains the ttft section the GUI already showed.
  `TUI_ONLY_COMMANDS` is down to `/theme` and `/edit`. The GUI renders the
  context gauge as a **real progress bar** (a rounded track with a
  fraction-width fill layer under the label) via the raw `CtxGauge` data the
  shared section model carries alongside its character-cell runs, so
  proportional fonts don't skew the fill boundary.

- **GUI feature parity, round two.** The GUI now covers everything but the
  genuinely terminal-bound commands:
  - **TODO panel** — the model's task list renders above the status bar (✓/▸/·
    glyphs), refreshed as `todo_write` runs and aged out after `todo_ttl` turns
    like the TUI's panel; `/todo-ttl` (shared implementation) adjusts and
    persists the lifetime, and `/clear` resets the list.
  - **Per-message timestamps** — user/assistant items get a `#N role · time`
    header (relative or `HH:MM`), controlled by the now-shared `/timestamps`
    command (persisted; `HRDR_TIMESTAMPS`/config honored at startup).
  - **`/find`, `/next`, `/prev`, `/goto`** — transcript search and jump with
    real scrolling: message numbers map to view ids at render time and the
    transcript scroll brings the target into view (`/goto` accepts
    `N | 5m | 1h | top | end`, using per-item timestamps for durations).
  - **`/provider`** — switch provider presets (built-ins + `[providers.<name>]`
    from config) with endpoint/model/context-window updates; the shared
    implementation now also drives the TUI.
  - **`/reload`** — re-reads the display config and applies what the GUI can
    change live (thinking, timestamps, todo-ttl; theme needs a restart).
  - `/timestamps`, `/todo-ttl`, and `/provider` moved into the shared dispatcher
    (TUI local copies deleted); `TUI_ONLY_COMMANDS` is down to `/theme`,
    `/statusbar`, and `/edit`.

- **GUI feature parity, round one.** Twelve more commands moved into the shared
  `hrdr-app` dispatcher behind new `CommandHost` capabilities (busy-guard,
  send-prompt, input editing, clipboard read, tool-expansion, rewind-last-turn,
  effort label, cwd/files-changed notifications, compaction) — the GUI gains
  `/compact`, `/temp`, `/effort`, `/cwd` (+`/cd`), `/expand`, `/add`, `/paste`,
  `/revert`, `/checkpoints`, `/retry`, `/undo`, and `/init`. The TUI drives the
  same shared implementations through its host adapter (its bespoke copies are
  deleted); only `/init`, `/compact`, and `/reload` keep richer TUI-local
  versions (pending-docs reload, compaction progress/queue machinery,
  hot-reload).

- GUI behavior parity with the TUI:
  - **Input queueing** — messages submitted while a turn runs are queued and
    sent FIFO as turns finish (previously all input was dead during a turn);
    cancel (Esc/Stop) discards the queue with a note, like the TUI.
  - **Slash commands work mid-turn** — `/help`, `/copy`, `/sessions`, … run
    while the model streams; turn-coupled commands (`/retry`, `/undo`,
    `/compact`, `/cwd`, …) busy-guard themselves.
  - **`/clear` cancels a running turn** (and drops queued messages) instead of
    being blocked.
  - **Startup auto-resume** — the GUI picks up the most recent saved session for
    the working directory (honoring the same `auto_resume` config /
    `$HRDR_AUTO_RESUME` knob); the lookup is shared
    (`hrdr_app::latest_session_for_cwd`, now also used by the TUI).
  - The status bar shows the `/effort` label; the `@file` index follows the
    agent's cwd (after `/cwd` or a resumed session) and is invalidated by
    `/revert`.
  - `/init`'s instruction prompt (`hrdr_app::INIT_PROMPT`) is shared.

### Changed

- Display/frontend knobs moved out of the core agent crate into
  `hrdr_app::UiConfig`: `vim` mode, `theme`, `icons`, `timestamps`, `statusbar`,
  `bell`, `auto_resume`, `todo_ttl`, and `show_thinking` no longer live on
  `hrdr_agent::AgentConfig`, which keeps only the model/endpoint/loop knobs
  (`base_url`, `api_key`, `model`, `cwd`, `temperature`, `max_steps`,
  provider(s), `context_window`, `effort`, `auto_compact`, `checkpoints`). **No
  user-facing change**: the config.toml keys, `HRDR_` env vars, CLI flags, and
  precedence (CLI > env > file > default) are all unchanged — both layers read
  the same file leniently. The TUI entry point is now
  `hrdr_tui::run(config, ui)`; config hot-reload re-reads both.
  `DEFAULT_TODO_TTL` moved to `hrdr-app`.

- More frontend plumbing deduplicated into `hrdr-app`:
  - **Highlighting** — the syntect syntax set, theme (base16-ocean.dark), and
    panel background were set up byte-identically in the TUI and GUI; both now
    use shared `hrdr_app::{syntax_set, syntect_theme, panel_bg_rgb}` (the
    span→color rendering stays per-frontend).
  - **Theme role mapping** — which hjkl palette entries feed which chat role
    (teal→user, gutter→dim, diagnostic_error→error, …) now lives once in
    `hrdr_app::ChatPalette`; the TUI applies ANSI fallbacks, the GUI RGB
    fallbacks. `hrdr-tui` drops its `hjkl-theme`/`hjkl-theme-tui` deps.
  - **Input history browsing** — `hrdr_app::HistoryBrowser` (dup-skip, cap,
    persist, Up/Down recall with draft stash/restore) replaces the two
    hand-rolled implementations.
  - Small: one `hrdr_tools::unix_now()` (was duplicated in sessions +
    checkpoints), one `run_search_cmd` postlude for the rg/grep backends, one
    `ShellArgs`/`shell_parameters` for the bash/powershell tools, the GUI's
    `one_line` replaced by `hrdr_tools::truncate_inline`, and the checkpoints
    `on/off/auto` spellings now derive from `parse_env_bool` (+
    `always`/`never`).

- The slash-command registry is now **capability-tagged**
  (`hrdr_app::TUI_ONLY_COMMANDS` + `is_tui_only`/`is_known_command`), fixing the
  GUI's biggest UX hole: ~23 advertised-but-unimplemented commands (`/compact`,
  `/retry`, `/undo`, `/goto`, `/theme`, `/cwd`, …) were offered by the GUI's
  completion dropdown and `/help` but fell through to the model as chat text.
  The GUI's completion and `/help` now list only what it implements, and typing
  a known-but-unported command gets a "isn't available in the GUI yet" notice
  instead of confusing the model. The TUI is unchanged (it implements the full
  registry).

- More command logic moved into the shared `hrdr-app` layer:
  - **`/copy`** — one shared implementation including `msg N[-M]` (previously
    TUI-only despite being advertised to both); the GUI gains message-range
    copy. A `last_code_block` host hook lets the TUI keep its
    search-back-through-history behavior for `/copy code`.
  - **`/diff`** — the TUI's local reimplementation is deleted; a `spawn_diff`
    host capability routes a real diff to the TUI's colored `Entry::Diff`
    rendering (status/error lines stay plain), defaulting to a system line in
    the GUI.
  - **Transcript rebuild** — `hrdr_app::messages_to_entries` is the single
    source for reconstructing a display transcript from a restored session; the
    TUI and GUI each had a near-identical copy (a divergence-drift magnet).
  - **Auto-save** — `hrdr_app::save_agent_session` (lock, snapshot, persist)
    replaces the GUI's two hand-rolled copies.

- Slash commands now have a **shared implementation** in `hrdr-app` behind a
  `CommandHost` trait, so the TUI and GUI drive one dispatcher
  (`hrdr_app::dispatch`) instead of each reimplementing commands — a new command
  benefits both frontends for free. The shared set is `/help`, `/clear`,
  `/model`, `/models`, `/tools`, `/info`, `/copy`, `/export`, `/rename`,
  `/diff`, `/thinking`, `/sessions`, `/resume`; async work (network, subprocess,
  filesystem, agent lock) is expressed as a future the host spawns and reports.
  As a result the **GUI gains `/export`** (write the conversation as Markdown or
  `--json`), **`/rename`** (name the session; later auto-saves reuse it), and
  **`/diff`** (the working-tree `git diff`). Frontend-coupled or richer commands
  stay local (the TUI keeps its `msg N[-M]` `/copy`, detailed `/info`, and
  colored `/diff`, plus scrolling/find/goto/expand/theme/editor). New shared
  cores: `git_working_diff` and `export_conversation`
  (`conversation_to_markdown`/`_json`).

- Showing the model's `<think>` reasoning is now a first-class setting:
  `show_thinking` in config, `--show-thinking on|off|1|0`, and
  `$HRDR_SHOW_THINKING` (default on). A new `/thinking [on|off|1|0]` slash
  command toggles it at runtime and persists to config (no arg flips it);
  `/reasoning` is now an alias of it. Both frontends honor the config value at
  startup; the TUI also re-reads it on config hot-reload. The bool parser
  (`1`/`0`, `on`/`off`, `true`/`false`, `yes`/`no`) is exposed as
  `hrdr_agent::parse_env_bool`.

- Tool output in the TUI now renders on a distinct panel background (the same
  shade as fenced code blocks) so each tool call reads as a self-contained
  block, and **clicking a tool block toggles its full output** — a per-entry
  `/expand` by mouse. The truncation hint reflects it
  (`… (+N more lines · click or /expand)` /
  `⌃ (click or /expand off to collapse)`); the click is hit-tested against the
  tool's on-screen rows (accounting for wrapping + scroll).

- Internal: the TUI `App` is now render- and terminal-I/O-agnostic — a first
  step toward a GUI frontend sharing the same core. The ratatui event loop +
  terminal ownership moved out of `impl App` into a new `tui` driver module;
  `App`'s only ratatui type (`Rect` for the follow-button hit-box) became a
  plain `HitRect`. `App` is now a drivable state machine (input in, view-state
  out); its sole remaining UI-lib dependency is `crossterm`'s
  `KeyEvent`/`MouseEvent` as input DTOs. No behavior change.

- CI now mirrors the kryptic-sh canonical layout (referenced from hjkl): `fmt`,
  `clippy` (3 OSes), `cargo-machete` (unused-deps lint), `test` (nextest +
  doctests on 3 OSes), and a cross-platform release `build` job. No release/
  packaging jobs yet.

- The context bar and auto-compaction keep working when the server reports no
  token usage. hrdr asks for usage (`stream_options.include_usage`), but servers
  that ignore it left the "used" count stale at 0. Turns now fall back to a
  rough `~4 chars/token` estimate of the prompt + completion when the server
  sends no usage chunk, so the status bar and the auto-compact threshold still
  track context growth (the overflow-retry path still covers any
  under-estimate).

- The managed local backend is now **infr-first**. If
  [`infr`](https://github.com/kryptic-sh/infr) is on `PATH`, hrdr spawns
  `infr serve <model> --addr <ip:port>` (native `tools`/`tool_calls`, SSE, GGUF
  Jinja chat template) as the default backend; it falls back to `llama-server`
  (llama.cpp, `--jinja`) when infr isn't installed, and errors clearly if
  neither is present. A backend already answering at `--base-url` is still
  reused. The `--backend-model` ref works for both;
  `--backend-arg`/`--backend-ctx` apply to the llama.cpp fallback (infr is tuned
  via `INFR_*` env vars). Spawn logs go to `~/.cache/hrdr/infr-serve.log` or
  `llama-server.log`. Dropped the "temporary" framing — infr's serve path now
  has full tool support. The default spawned model is now `Qwen3-8B` (Q4_K_M),
  down from the 30B-A3B MoE, for a smaller download and faster startup.
- Finished TODO items now age out of the panel. A completed item stays visible
  for the turn it finishes plus four more (five turns total), then it's pruned —
  so the list keeps showing recent progress without accreting stale checkmarks.
  Pending / in-progress items are never pruned, and an item re-completed after
  being reopened ages from scratch. The lifetime is configurable via `todo_ttl`
  in config, `--todo-ttl <turns>`, `$HRDR_TODO_TTL`, or the `/todo-ttl [turns]`
  slash command (which persists to config); no arg reports the current value.
  Default 5; hot-reloadable like the other display settings.
- The status-bar context readout is simpler — just `{used} of {max}` (no
  percentage or `ctx` label). The used/free fill bar and its green→amber→red
  escalation are unchanged (they already convey the fraction visually).
- Time-to-first-token (TTFT) is now reported — how long the provider took to
  send the first streamed token. The TUI shows `ttft {n.nn}s` on the generating
  loader (live) and on the persistent per-turn `✓` stats line; the GUI shows it
  in the status bar (measured from send to the first `Text`/`Reasoning` event,
  kept until the next turn).
- hjkl dependencies now come from crates.io (registry pins `hjkl-* = "0.33"`)
  instead of `../hjkl/...` path deps against the sibling repo. hjkl was
  published to crates.io at 0.33.3. CI is now standalone — the second checkout
  of `kryptic-sh/hjkl` alongside hrdr is gone; each job checks out hrdr only.
- The status bar has a configurable mode — `truncate` (default), `wrap`, or
  `none` — via `statusbar` in config, `--statusbar <mode>`, `$HRDR_STATUSBAR`,
  or `/statusbar [none|truncate|wrap]` (no arg cycles). `truncate` drops the
  least-important sections (effort, then in/out tokens, then git branch, then
  model) until it fits one row, keeping the cwd and context bar and showing a
  trailing `…`; `wrap` packs every section across up to four rows; `none` hides
  the bar entirely.
- Quitting now requires a double Ctrl+C: the first idle Ctrl+C arms a confirm
  (any other key/mouse action disarms it) and shows a "Press Ctrl+C again to
  quit" banner on the input box's top border (taking priority over the follow
  button); a second consecutive Ctrl+C quits. While a turn is running the first
  Ctrl+C still interrupts it. Ctrl+Q remains an immediate quit.

### Fixed

- GUI: per-message signals no longer leak. Every assistant/tool item created its
  reactive signals on the app-root scope, so a long-lived window accumulated a
  few orphaned signals per message across every `/clear`, `/resume`, and turn.
  Items now get a child scope that is disposed when the transcript is cleared or
  rebuilt.

- GUI: `/thinking` now persists to config like the TUI (it only flipped the
  in-memory signal, so the setting was lost on restart).

- GUI `/resume` now follows the resumed session's working directory (matching
  the TUI): the agent's cwd switches when the directory still exists (with a
  note when it doesn't), `@file` mentions and tools resolve against it, and an
  endpoint mismatch is called out. Previously the GUI ignored `session.cwd`
  entirely, so tools operated in whatever directory the GUI was launched from.

- GUI input is trimmed before command detection, so `" /help"` runs the command
  instead of being sent to the model (matching the TUI).

- Overflow-triggered auto-compaction can no longer overflow itself. The
  summarization request re-sent the entire history (saving only the `tools[]`
  block versus the request that failed), so against the same model it usually
  hit the context limit too and killed the turn. On overflow the summarizer
  input now shrinks and retries: bulky tool-result bodies are elided first, then
  only the most recent half/quarter/eighth of the conversation is kept (windows
  aligned so no `role:"tool"` result is orphaned from its `tool_calls` message).

- A stray `OPENAI_API_KEY` in the environment no longer overrides a config-file
  `api_key` (it silently hijacked auth for local/OpenRouter/zen endpoints).
  `HRDR_API_KEY` still always wins; `OPENAI_API_KEY` is now only a last-resort
  fallback when no other key is set.

- GUI: live tool output/results now update the right entry. `find_tool` scanned
  oldest-first without checking `done`, so backends that restart tool-call ids
  each turn (`call_0`, `call_1`, …) updated a finished tool from an earlier turn
  while the new one spun forever. It now scans newest-first and matches only
  unfinished tools.

- GUI: a `/clear` racing an in-flight turn's auto-save can no longer resurrect
  the old session id (which made the next conversation overwrite the old
  session's file). Saves carry a generation stamp; `/clear` and `/resume` bump
  it and stale `Saved` notifications are dropped. `/clear` and `/resume` also
  apply agent changes synchronously when the lock is free, so an immediately
  following send can't win the agent lock first, and `/clear` now resets the
  status bar's leftover ttft.

- The session file's `created` timestamp is preserved across auto-saves (every
  save rebuilt the session, so `created` always equaled the last save time).

- Config directory resolution is now XDG-aware and shared: `config.toml` and the
  global `AGENTS.md` both live in `hjkl_xdg::config_dir("hrdr")`
  (`$XDG_CONFIG_HOME/hrdr`, default `~/.config/hrdr`). Previously the two built
  the path differently (`HOME`-only vs `HOME`/`USERPROFILE`), so on Windows the
  global `AGENTS.md` silently never loaded, and `$XDG_CONFIG_HOME` was ignored
  everywhere.

- `glob` works when the working directory itself contains glob metacharacters
  (`[`, `*`, `?`) — the cwd prefix is now escaped so only the pattern argument
  is interpreted as glob syntax.

- `web_search` (DuckDuckGo) snippet extraction is bounded to each result's own
  block; a snippet-less result no longer steals the next result's snippet.

- `/help` derives its column width from the longest command name — `/timestamps`
  and `/checkpoints` no longer run into their descriptions.

- Status-bar git branch detection follows relative `gitdir:` pointers
  (submodules, worktrees) relative to the repo, not the process cwd.

- TUI: a failed compaction's error line went around the timestamp bookkeeping,
  shifting every later entry's displayed time (and `/goto 5m` targets) by one.

- Sessions-dir fallback when no home directory can be resolved is an absolute
  path under the system temp dir; the old relative fallback scattered
  `.local/share/hrdr` into whatever directory hrdr ran in. A poisoned todo lock
  now recovers instead of silently reporting success with a stale list.

- Streamed responses no longer corrupt multibyte UTF-8 split across network
  chunks. The SSE decoder ran `from_utf8_lossy` per raw chunk, so an emoji/CJK
  codepoint straddling a chunk boundary became U+FFFD replacement characters
  inside the streamed text (and was baked into the saved history). The decoder
  now buffers raw bytes and only decodes complete `data:` lines.

- A timed-out `bash`/`powershell` command no longer leaks a running process. The
  tool reported "command timed out" but never killed the child, so a hung
  `cargo test` or dev server kept running orphaned. The child is now killed on
  timeout (and `kill_on_drop` covers turn interruption), and the output the
  command produced before the timeout is returned to the model instead of being
  discarded.

- Pasting in `--vim` mode while in Normal mode no longer executes the pasted
  text as vim commands (`d`, `x`, `:`, … mutated or clobbered the input buffer).
  `VimEngine` now inserts pastes directly into the buffer outside Insert mode;
  the key-feed path is kept in Insert mode.

- `/clear` during a running turn now cancels the turn first. Previously the
  agent-history clear was a silent `try_lock` no-op while the transcript and
  session id were reset anyway — the still-running turn then streamed into the
  emptied view and its autosave wrote the _uncleared_ history into a brand-new
  session file.

- `/resume` is now rejected while a turn is running. The message swap was a
  silent `try_lock` no-op but the session id was adopted anyway, so the
  in-flight turn's autosave overwrote the resumed session's file on disk with
  the previous, unrelated conversation.

- `todo_write` now tolerates the malformed argument shapes smaller models emit
  instead of failing with `invalid todo_write args`. The schema is unchanged
  (`{"todos": [{content, status}, …]}`), but the parser now also accepts the
  common schema-echo mistake `{"todos": {"items": […]}}` (the model copies the
  JSON-Schema `items` keyword into the value), a dropped/renamed wrapper
  (`{"items": …}` / `{"tasks": …}`), a bare top-level array, and a single item
  object. Per-item it accepts `task`/`text`/`title` aliases for `content` and
  normalizes a range of status spellings (`done`/`complete` → `completed`,
  `doing`/`wip`/`active` → `in_progress`, case/space/hyphen-insensitive) with
  unknown statuses falling back to `pending` rather than erroring.

- Pasting from the OS clipboard in `--vim` mode now works. The editor Host's
  `read_clipboard` returned a cache that was only filled by a
  `refresh_clipboard_cache` call that existed nowhere, so vim clipboard-register
  paste (`"+p`) always got nothing (yank-out already worked). It now reads the
  OS clipboard directly via `hjkl_clipboard::get` — exactly like the TUI's
  `/paste` — and the dead cache/`refresh`/`cursor_shape`/`set_cancel` machinery
  is gone.

- No more panics on multibyte (non-ASCII) text. Three sites sliced a `&str` at a
  fixed byte offset without landing on a char boundary — `read_file` (long
  lines), the web-fetch HTML sniff, and `@file` mention expansion — so a UTF-8
  codepoint straddling the cut would panic. All now use a shared
  `hrdr_tools::floor_char_boundary` helper (reused by `truncate` too).

- Interrupting a turn mid tool-call no longer corrupts the conversation. A turn
  pushes the assistant `tool_calls` message before running the tools, so
  cancelling (Esc) during tool execution left the history ending with an
  assistant message whose `role:"tool"` results were missing — strict servers
  (OpenAI, infr) then reject the next request. The next turn now backfills a
  `[interrupted]` stub result for each unanswered call id before sending
  (`repair_dangling_tool_calls`).

- Tool calls whose server omits the `id` field now get stable synthesized ids
  (`call_0`, `call_1`, …) in `Accumulator::into_message`, so the assistant
  message and its `role:"tool"` results correlate and multiple calls in one turn
  don't collide on an empty id (which breaks the follow-up request on stricter
  servers).

- Multi-turn conversations with reasoning models (Qwen3 via `infr`, etc.) no
  longer degenerate into repetition/gibberish on the second turn. The assistant
  history message was serializing its `reasoning_content` (the `<think>` block)
  back into the request — reasoning models are trained to have prior-turn
  thinking stripped from the prompt, and feeding it back drove the model
  off-distribution. `reasoning_content` is now `skip_serializing` (never sent),
  matching its documented "received-only" intent; it's still kept for display
  and still parses on the way in.

- `/clear` (and its `/new` alias) now fully resets to a fresh session. It
  previously kept the original system prompt, so an `AGENTS.md` that was updated
  or removed after startup lingered in context forever. `Agent::clear()` now
  drops all history and **re-reads `AGENTS.md`** for the current cwd, and the
  TUI handler also clears the TODO list and any pending find/goto/expand state —
  so `/clear` behaves exactly like reopening the session.

- Scrolling up in the transcript now stays pinned to the content you scrolled to
  while output streams in. `scroll_offset` is measured from the bottom, so as
  new rows were appended the view drifted downward; the draw now bumps the
  offset by however much the content grew since the last frame, keeping the
  from-top position fixed. Following the newest output (offset 0) is unaffected.

- Status-bar context size no longer drops to 0 between turns: `last_usage` is
  kept across turns (only the live per-turn counters reset), so the displayed
  context persists until the next turn's usage refreshes it.

- Scrollbar thumb position: it now reaches the bottom when following the output
  (was stuck midway) — `content_length` is the number of scroll positions, not
  the raw line total, matching ratatui's `position` mapping.

### Added

- **`hrdr-app` — a shared application-core crate.** The first slice of logic
  that the TUI and GUI both use now lives in one place instead of being
  duplicated: the slash-command registry (`SLASH_COMMANDS`), help groupings,
  alias resolution (`resolve_alias`), and quit-command detection
  (`is_quit_command`). The TUI's `/help`, completion, dispatch, and quit-on-type
  use it; the GUI uses `is_quit_command` so typing `exit`/`quit`/`:q` closes the
  window. Also pulled in: the representation-independent helpers `resolve_under`
  (path resolution), `display_dir`/`git_branch` (status-bar strings),
  `walk_files`/`walk_files_gitignore` (gitignore-aware `@file` discovery),
  `parse_duration` (`/goto` time specs), `parse_msg_range` (`/copy msg N-M`),
  and `last_fenced_block` (`/copy code`) — with their tests — so the TUI now
  imports them from `hrdr-app` instead of owning private copies (`ignore` moved
  with them). A further batch followed: the completion logic
  (`slash_completions`, `active_file_token`, `rank_file_matches`), the display
  formatters (`fmt_count`, `relative_time`), the `help_body` command listing
  (the TUI appends its own keybinding tips), `session_name_from`
  (first-user-line session titles), and the config-value enums
  `TimestampStyle`/`StatusBarMode` (now with an `as_config_str` for round-trip
  persistence). All the TUI-only copies are gone; `hrdr-app` grew `chrono` for
  the relative-time formatter. Then the transcript model itself was lifted: the
  `Entry` enum (one rendered conversation item) and the
  representation-independent queries over `&[Entry]` — `find_hits` (`/find`),
  `message_count`, `nth_message_text`, `first_message_since` (`/goto <time>`),
  and the export builders `transcript_to_text`/`transcript_to_json`
  (`/copy all`, `/export`) — now live in `hrdr-app` (which grew `serde_json` for
  the JSON export; the TUI dropped it). The TUI re-exports `Entry` and delegates
  those methods, so a GUI transcript can reuse the exact same search/export
  semantics. Also lifted: `@file` mention expansion (`expand_mentions`, so both
  frontends attach file contents identically), the input-history persistence
  (`load_history`/`persist_history`/`MAX_HISTORY` over
  `$XDG_DATA_HOME/hrdr/history`, which moved `hjkl-xdg` to `hrdr-app`), and the
  TODO-panel aging (`age_completed_todos`, with its tests). The streaming
  reducer stays per-frontend for now — the TUI is immediate-mode with plain
  strings, the GUI retained-mode with per-field reactive signals.
- **`hrdr-gui` — a floem desktop frontend (proof-of-concept).** A new
  `apps/hrdr-gui` binary drives the same UI-agnostic core as the TUI
  (`hrdr_agent::Agent`): a chat window that streams a turn's `AgentEvent`s into
  a scrolling transcript via floem's `create_signal_from_tokio_channel` bridge.
  Renders assistant text + dim `<think>` reasoning, tool calls (a clickable
  header that collapses/expands the live streamed output, plus a
  pass/fail-colored result), and system/error lines; a status bar shows the
  model / context usage / output tokens and a "thinking" indicator; Enter or a
  Send button submits. Colors come from an **hjkl theme** (the same system the
  TUI uses — `theme` in config picks it), mapped onto chat roles + the window
  background. Per-message reactive signals stream tokens in place without
  rebuilding the list. **Slash commands** now work in the GUI: typing `/` shows
  a live completion dropdown (the shared `hrdr_app::slash_completions` ranker)
  whose rows fill the input on click, and submitting a `/…` runs it locally
  instead of sending it to the model — `/help` (the shared `help_body` listing),
  `/clear`, `/model [name]` (switches live; the status bar reflects it),
  `/models`, `/tools`, and `/info`, with aliases resolved via the shared
  `resolve_alias`. An unrecognized `/…` still falls through to the model (so a
  literal path works, matching the TUI); the quit-word family closes the window.
  **`@file` attachment** works too: the same dropdown shows ranked file matches
  while an `@…` mention is being typed (shared `active_file_token` +
  `rank_file_matches` over a lazily-built `walk_files` index), clicking one
  fills the `@path`, and on send the mention is expanded into the file's
  contents for the model via the newly-shared `hrdr_app::expand_mentions`
  (lifted out of the TUI, so both frontends attach files identically) while the
  transcript keeps the bare `@path`. **Input-history recall** (Up/Down) browses
  previously-submitted lines, stashing the live draft, and persists across runs
  via the shared `hrdr_app::load_history`/`persist_history`; **`/reasoning`**
  toggles the dim `<think>` blocks; **`/copy`** writes the last reply (or
  `/copy code` the last fenced block via the shared `last_fenced_block`, or
  `/copy all` the transcript) to the OS clipboard via `hjkl-clipboard`.
  **Session `/sessions` + `/resume`** land too: `/sessions` (`--all` for every
  directory) lists saved sessions via the newly-shared
  `hrdr_app::session_list_text` (the TUI's listing now delegates to it as well),
  and `/resume <id or name>` restores a saved conversation — rebuilding the GUI
  transcript from the message history (user/assistant text + each tool call
  paired with its result) and pushing the messages + model back into the agent.
  **Turn interruption**: the Send button becomes **Stop** while a turn runs, and
  Esc (or Stop) aborts the in-flight task — dropping its future releases the
  agent lock, late buffered events are discarded, and the next turn repairs any
  dangling tool calls. **Markdown rendering**: assistant replies now render as
  markdown instead of plain text — headings, bold/italic/inline-code, lists,
  blockquotes, and fenced code blocks syntax-highlighted with syntect on a panel
  background — via a floem `rich_text` renderer over `hjkl_markdown`'s event
  stream (the same stream the TUI's ratatui backend consumes). The blocks render
  through a `dyn_stack` keyed by per-block content hash, so a streaming reply
  only re-renders (and re-highlights) the changed tail block instead of the
  whole reply each token — earlier paragraphs and finished code blocks keep
  their already-rendered views. **Session auto-save**: after each completed turn
  the GUI persists the conversation (via the newly-shared
  `hrdr_app::save_session`, which the TUI's continuous auto-save now also uses),
  assigning a stable file id on first save and notifying once
  (`session saved as '…' — /resume …`); `/resume` adopts the id so later saves
  update the same file, and `/clear` detaches it. TUI-shared logic continues to
  move into the shared `hrdr-app` crate as GUI features land. Excluded from CI
  for now (floem's large X11/Wayland dep tree + Linux system libs — wiring it in
  is a follow-up).
- Weekly `cargo-deny` scan (advisories / licenses / bans / sources) via a
  scheduled `cron.yml` workflow (Monday 06:00 UTC, matching hjkl), plus a
  `deny.toml` config. Two syntect-transitive unmaintained advisories are ignored
  (`yaml-rust`, `bincode` 1.x — no safe upgrade) and `webpki-roots`' CDLA data
  license is allowed as a scoped exception.

- Auto-detect the server's context window. On startup, when `context_window`
  isn't set explicitly (config/provider), hrdr probes the endpoint and uses what
  it advertises — a non-standard field on the `/v1/models` entry (vLLM's
  `max_model_len`, LM Studio's `max_context_length`, …) or llama.cpp's
  `GET /props` (`n_ctx`). Precedence: explicit config/provider →
  server-advertised → the spawned backend's `--backend-ctx` (default 16384) →
  unknown. The OpenAI spec doesn't expose context length, and infr doesn't
  advertise it yet, so those fall back; a server that does advertise is now
  honored for the status bar's "X of Y" and the auto-compaction threshold. New
  `Client::context_window()`.
- End-to-end TUI tests + a mock provider. A tiny in-process OpenAI-compatible
  server (`GET /v1/models` + streamed SSE `POST /v1/chat/completions`, with
  scriptable text / tool-call / multi-chunk / reasoning replies) lets tests
  drive a real `App` through its `on_key`/`on_turn_msg` seams and assert on
  transcript state + the rendered ratatui `TestBackend` buffer — no network, no
  live model. Covers a streamed text reply, single- and multi-call tool
  round-trips, a failing/unknown tool call (surfaced but non-fatal, turn
  recovers), multi-chunk stream assembly, usage capture, `<think>` reasoning
  display + `/reasoning` toggle, `/statusbar` and `/timestamps` state changes,
  `/clear` wiping the transcript, and a locally-handled slash command. Lives in
  `crates/hrdr-tui/src/app/e2e.rs`.
- Broad unit-test hardening across the loop internals: `Accumulator` edge cases
  (usage-only chunk, reasoning accumulation, content+tool-calls in one turn),
  `ChatRequest` serialization (empty `tools` / `None` temperature omitted),
  context-window field parsing, `truncate` boundaries (exact size, UTF-8
  multibyte safety), the file-checkpoint store (blob round-trip, dedup, per-turn
  record, `revert_to`), config resolution (the `ENV_SETTERS` table,
  `apply_file`, provider precedence), transient/overflow error classification,
  `repair_dangling_tool_calls`, token estimation, and `in_git_repo`/`cwd_slug`.
  The suite is ~106 tests.
- Presence-aware shell tools: the `bash` tool is now only offered to the model
  when `bash` is actually on `PATH`, and a new `powershell` tool is offered when
  `pwsh`/`powershell` is available (PowerShell 7 runs on Linux/macOS too). So
  the model always gets a shell it can actually use — bash on unix, PowerShell
  on Windows (or both), and no phantom shell where neither exists. Both stream
  output like before.
- Presence-aware `grep`: the search tool now picks the best available backend —
  ripgrep (`rg`) if installed, else POSIX `grep`, else a built-in pure-Rust
  walker (honors `.gitignore`, filters by glob, matches with the `regex` crate).
  So content search works even on a machine with neither `rg` nor `grep`.
- File checkpoints + `/revert`: the agent's file edits (`edit`/`write_file`) are
  now snapshotted per turn, so `/revert` undoes the last turn's file changes
  (restoring modified files and deleting ones the agent created), and
  `/checkpoints` lists the revertible turns. Storage is git-like and incremental
  — each changed file's pre-image is SHA-256 content-addressed (identical
  content stored once) and deflate-compressed, with a journal recording which
  turn touched which file, kept under `$XDG_DATA_HOME/hrdr/checkpoints/<cwd>/`
  so revert survives restarts. Only files the agent modifies are snapshotted, so
  it's fast and small. Checkpointing is **auto-disabled inside a git repo** (git
  already provides revert); set `checkpoints = on` in config,
  `--checkpoints on`, or `$HRDR_CHECKPOINTS=on` to force it (or `off` to disable
  entirely).
- Expandable tool output: tool results are previewed (head/live tail) with a
  `… (+N more lines · /expand)` hint; `/expand` toggles the most recent result
  to full, `/expand all` shows every tool result in full, and `/expand off`
  collapses everything back to previews.
- Network resilience: the model connection is now retried with exponential
  backoff (up to 4 attempts) on transient failures — connection errors, 429, and
  5xx — instead of failing the turn. Each retry posts a system notice.
- Auto-compact on context overflow: if the server rejects a request because the
  context window is exceeded, hrdr automatically compacts the conversation once
  and retries the turn (with a notice) rather than erroring out.
- Live tool output streaming: long-running tools (notably `bash`) now stream
  their stdout/stderr into the transcript line-by-line as it's produced, instead
  of showing nothing until the tool finishes — the running tool entry shows the
  live tail (with a count of earlier lines). Plumbed via a per-call output sink
  on `ToolContext` and a new `AgentEvent::ToolOutput`; headless `run` streams it
  to stderr.
- Config persistence + hot reload: changing a preference in the client
  (`/timestamps`, `/statusbar`, `/theme`, `/effort`, `/temp`) now writes it to
  `~/.config/hrdr/config.toml` (format/comment-preserving via `toml_edit`). hrdr
  watches the config file with an OS-level notifier (`notify` —
  inotify/FSEvents/kqueue) and hot-reloads live settings when it changes —
  whether edited by hand or by another running session (falling back to mtime
  polling only if a watcher can't be created). Loading is fault tolerant: an
  invalid config never crashes the client; at startup it warns and falls back to
  defaults, and on hot-reload it keeps the last known-good settings and warns.
  New `AgentConfig::load_checked()` + `config_file_path()` +
  `persist_setting`/`remove_setting`.
- Syntax highlighting for fenced code blocks in assistant messages: code blocks
  are pulled out of the markdown and highlighted with `syntect` (lightweight,
  pure-Rust fancy-regex) on a distinct dark background, with a small language
  tag bar. Highlighted output is cached per (language, content, width) so the
  live redraw stays cheap. Prose still renders via `hjkl-markdown`.
- Per-message timestamps + numbers: each user/assistant message gets a dim
  header (`#3 you · 2m ago`) showing its number and send time. A single
  `timestamps` setting picks the style — `none`, `relative` (default; `now`,
  `2m ago`, `1h30m ago`, `2d3h ago`), or `exact` (`HH:MM`) — via config,
  `--timestamps <style>`, or `$HRDR_TIMESTAMPS`. Change it live with
  `/timestamps [none|relative|exact]` (no arg toggles off/relative). Relative
  times use compound units past an hour (`1h30m`, `2d3h`).
- `/find <text>` jumps the transcript to the next message containing `text`
  (case-insensitive) and highlights every match across the transcript; repeat
  `/find` with no argument to cycle through matches. Reports the match position
  and count; `/next` and `/prev` cycle forward/backward through the matches
  (wrapping); `/find clear` (or `off`/`discard`) drops the search + highlight,
  and `/clear` clears it too.
- The inference loader shows when the current turn started (`started 2m ago` /
  `started 14:32`), respecting the timestamp style (hidden when set to `none`).
- `/goto <N | 5m | 1h | top | end>` scrolls the transcript to a message number,
  to the message nearest a relative time ago (e.g. `5m`, `1h`, `2d`), or to the
  top/latest. The target message is placed at the top of the viewport.
- `/copy msg N` copies a specific numbered message (the `#N` shown by the
  timestamp headers), and `/copy msg N-M` copies an inclusive range, alongside
  the existing `/copy`, `/copy code`, `/copy all`.
- `/export [--json] [file]` writes the transcript to a file as text (default) or
  JSON (`{n, role, time, content}` per message); with no file argument it writes
  a timestamped `hrdr-transcript-<date>.md` / `.json` in the working directory.
- `/reload` re-reads `AGENTS.md` and the config file, applying the bits that can
  change live (theme, icons, effort, toggles, temperature) without a restart.
- `/paste` inserts the system clipboard into the input — and if the clipboard
  holds a path to an existing file, attaches it as an `@mention` instead.
- `/help` is now grouped by category (Session, Model & sampling, Files &
  context, Reply, Appearance, Other) with aligned descriptions and a tips line,
  instead of one flat list.
- `Ctrl+D` on an empty input quits the client (shell-style EOF). In vim Normal
  mode `Ctrl+D` still half-page scrolls the transcript (it only quits when the
  input is empty and you're not in Normal mode).
- `Ctrl+L` clears and repaints the screen, to recover from terminal corruption
  (e.g. after a stray external write or a garbled resize).
- Startup endpoint health check: on launch hrdr probes the endpoint in the
  background and warns in the transcript if it's unreachable, or if the
  configured model isn't among the endpoint's advertised models (listing a few
  available ones). Silent on success.
- `/copy` variants: `/copy` (last reply, as before), `/copy code` (the most
  recent fenced code block), and `/copy all` (the whole transcript as text).
- `/edit <file>` opens a file (relative to the cwd, created if missing) in
  `$EDITOR`/`$VISUAL`, suspending the TUI while you edit.
- `/retry [model]` re-runs the last turn, optionally switching to `model` first
  (for that turn and subsequent ones) to compare outputs.
- Input draft size estimate: while you type, the input box's bottom-right border
  shows a rough token count and character count (`~123 tok · 480 ch`), so you
  can gauge how big a message (or paste) is before sending.
- Icon set is configurable: `icons = nerd` (default), `unicode`, or `ascii` in
  config, `--icons <set>`, or `$HRDR_ICONS`. Non-nerd modes drop the status-bar
  Nerd-Font glyphs (folder, git branch) so they don't render as tofu without a
  patched font. Uses `hjkl-icons`' `IconMode`.
- Terminal bell on turn completion: when a turn finishes after running at least
  a few seconds, hrdr rings the bell so you can tab away during long tasks and
  be notified when it's done. Disable with `bell = false` in config,
  `--no-bell`, or `$HRDR_BELL=0`.
- Status-bar context usage now shows a percentage of the window and colors it by
  fill level — dim under 70%, amber at 70%+, and red once it reaches the
  auto-compact threshold — so you can see compaction coming.
- `/init` has the model author an `AGENTS.md` (Claude Code / opencode style): it
  sends the model an instruction to explore the repo with its tools — READMEs,
  build/manifest files, source layout — and write a concise, repo-specific
  `AGENTS.md`, improving an existing one rather than discarding it. Shown as
  `/init` in the transcript while the model works; when the turn finishes the
  new `AGENTS.md` is reloaded into the system prompt automatically.
- Input history: Up/Down in the input recalls previous submissions
  (readline-style), restoring your in-progress draft when you pass the newest.
  Active only for single-line input, so multi-line editing keeps normal cursor
  movement; the completion popup still owns Up/Down while it's open. History
  persists across runs at `$XDG_DATA_HOME/hrdr/history` (last 200 single-line
  entries).
- Auto-resume on startup: the TUI restores the most recent saved session for the
  current working directory (history + transcript + model), so reopening hrdr in
  a project picks up where you left off; `/clear` starts fresh. If no session
  exists for the directory, a new one is started. Disable with
  `auto_resume = false` in config, `--no-auto-resume`, or `$HRDR_AUTO_RESUME=0`.
- Slash-command aliases for users switching from other agents: `/new` and
  `/reset` → `/clear`, `/cd` → `/cwd`, `/status` → `/info`, `/continue` →
  `/resume`, `/summarize` → `/compact`, and `/commands` / `/?` → `/help`
  (case-insensitive). They resolve to the canonical command and appear in the
  completion popup. (Quit words `/quit` `/bye` `/q` already exit.)
- Web tools: `web_fetch` (GET a URL and return its content as text — HTML is
  reduced to readable text, scripts/styles/markup stripped, with an optional
  `max_chars` cap) and `web_search` (top results as title/URL/snippet). Search
  uses DuckDuckGo's HTML endpoint with zero configuration, or a SearXNG instance
  when `SEARXNG_URL` is set (a JSON API — more robust). Both are in the default
  tool set, so the model can look things up and read pages.
- `@file` mentions with autocompletion: type `@` in the input to get a popup of
  matching project files (Up/Down to select, Tab or Enter to insert the path);
  the file index is built lazily from the cwd. In a git repo it honors
  `.gitignore`/`.ignore` at every level (nested subdirectory ignore files
  included, plus parents/global) and `.git/info/exclude` via the `ignore` crate;
  outside a git repo it falls back to skipping known VCS/build and hidden
  directories. On send, each `@path` is expanded into the referenced file's
  contents for the model (bounded to 100 KB/file), while the transcript still
  shows the message exactly as typed. Complements `/add`.
- Project instructions via the open `AGENTS.md` standard (https://agents.md): on
  startup (and whenever the working directory changes) hrdr gathers `AGENTS.md`
  files walking from the cwd up to the filesystem root, plus an optional global
  `~/.config/hrdr/AGENTS.md`, and injects them into the system prompt
  (less-specific files first, so nearer ones take precedence). The TUI notes
  when project instructions were loaded.
- Context compaction (Claude Code / opencode style): `/compact [instructions]`
  asks the model for a structured summary of the conversation (intent, technical
  context, files & code, commands, errors & fixes, current state, pending tasks)
  and replaces the message history with the system prompt + that summary, so the
  context shrinks while continuity is preserved. Optional trailing text steers
  the summary's focus. Compaction also runs automatically once the prompt size
  reaches a configurable fraction of the model's context window (default 85%,
  leaving headroom before the next turn can overflow): set `auto_compact` in
  config, `--auto-compact <ratio>`, or `$HRDR_AUTO_COMPACT` (0 disables). The
  on-screen scrollback is left intact for the user; only what the model sees is
  compacted.
- Session persistence with continuous auto-save: every non-empty conversation is
  saved as JSON under `$XDG_DATA_HOME/hrdr/sessions` (default
  `~/.local/share/hrdr/sessions`, via `hjkl-xdg`), partitioned by working
  directory as `sessions/<cwd-slug>/<name-slug>.json` for easy manual
  management. The session `name` derives from the first user message and a
  stable file id is assigned on first save. Auto-saves after each completed turn
  and after `/undo`/`/retry`. Commands `/sessions` (list this directory's
  sessions; `--all` for every directory, grouped with their cwd),
  `/resume <id-or-name>` (restore history + transcript; prefers the current
  directory, then matches any session's file id or display name, e.g. after
  `/rename`), `/rename <name>` (rename the session; persisted). `/clear` starts
  a fresh session. (No `/save` — saving is automatic.) `/info` shows the current
  session id + name, and a notice prints the id when a session is first saved.
  Resuming a session recorded in a different directory switches hrdr's tools to
  that directory (in-process only — the parent shell is untouched); if it no
  longer exists, hrdr stays put and says so.
- More slash commands: `/models` (list endpoint models), `/cwd [path]` (show or
  change the tools' working directory), `/tools` (list tools), `/reasoning`
  (toggle showing `<think>` blocks), `/theme [path]` (live theme switch),
  `/info` (session summary), `/temp [n]`, `/effort [level]`, `/add <file>`
  (attach a file's contents to the next message), `/diff` (git diff of the
  working tree, colored), and `/undo` (drop the last turn and restore it to the
  input for editing).
- Slash-command autocompletion: typing `/` shows a popup of matching commands
  above the input — Up/Down to select, Tab to accept, and Enter accepts the
  selected (best) match and runs it. Matches the query against both the command
  name and its description (so `/list` surfaces `/help`).
- Slash commands (typed in the input): `/clear` (reset the conversation),
  `/model [id]` (show or switch model), `/provider <name>` (switch provider
  preset mid-session), `/copy` (last reply → clipboard), `/retry` (re-run the
  last turn), `/help`. Unknown `/…` input is still sent to the model.
- Diff rendering: `edit` and `write_file` now return a unified diff (also fed to
  the model), shown in the TUI with additions green, deletions red, and hunk
  headers in the accent color. New-file writes show a concise create summary.
- Markdown now renders the full GFM set — tables, task lists, nested lists,
  blockquotes, strikethrough, images — via the upgraded `hjkl-markdown(-tui)`.
- Markdown rendering of assistant messages (headings, bold/italic, lists,
  inline/code spans, links, rules) via `hjkl-markdown` + `hjkl-markdown-tui`,
  themed from the active hjkl theme. (Per-language syntax highlighting of code
  blocks is a follow-up.)
- Custom providers in config: define `[providers.<name>]` (with `base_url`,
  `key_env`/`api_key`, optional `model`, `remote`, `context_window`) and select
  with `--provider <name>` (config entries shadow built-ins of the same name).
- Built-in `openrouter` and `claude`/`anthropic` provider presets (the latter
  via Anthropic's OpenAI-compatible endpoint).
- Status bar above the help line showing working directory, git branch, session
  input/output token totals, context size (current / window), model, and a
  reasoning-effort label. Context window comes from the spawned backend (or
  `context_window` in config); effort from `--effort`/config.
- Theming via the hjkl theme system: `--theme <path>` (or `theme` in config /
  `$HRDR_THEME`) loads an hjkl theme TOML and maps its palette/`[ui]` styles
  onto hrdr's chat roles (user, assistant, dim chrome, tool/loader accent,
  success/error); hjkl's bundled dark theme is the default. Uses `hjkl-theme` +
  `hjkl-theme-tui`'s `ToRatatui`.
- Transcript scrollbar on the right edge showing total session length and the
  current scroll position within it.
- `Home` jumps the transcript to the top of the session (and `End` back to
  following the newest output); both fall through to the editor at the extremes.
- The input box has one column of left/right padding for breathing room.
- Paste support: bracketed-paste text is inserted into the input at the cursor
  (newlines kept literal, so a multi-line paste no longer submits early).
- A final per-turn stats line (`✓ N tok · X tok/s · Ys · ctx … (in/out …)`) is
  appended below the model's last output when a turn completes.
- Quit commands: submitting a common quit word exits the session instead of
  being sent to the model — bare `exit`/`quit`/`q`/`bye`, the `/exit` `/quit`
  `/bye` slash family, and vim's `:q`/`:qa`/`:wq`/`:x` family
  (case-insensitive).
- Provider presets via `--provider` (or `provider` in config /
  `$HRDR_PROVIDER`): `zen`/`opencode` (OpenCode Zen, `OPENCODE_API_KEY`),
  `openai`, and `local`/`infr`. A preset sets the base URL + API-key env, and
  remote providers skip the local llama-server backend.
  `--base-url`/`$HRDR_BASE_URL` still override the endpoint.
- Queued messages now float as a dimmed "— queued —" block at the bottom,
  following the output, and are committed into history only when actually sent
  (rather than being pinned at their typed position mid-conversation).
- Auto-growing input box: starts at one row and expands with content up to five
  rows (then scrolls internally); plain input wraps long lines to match.
- Inference loader above the input while a turn runs: an animated spinner with
  live stats — context size, input/output token ratio, and throughput (tok/s) —
  driven by streamed `usage` (via `stream_options.include_usage`).
- Chat scrolling: mouse wheel scrolls the transcript, `PageUp`/`PageDown` page
  through it, and `End` resumes following the newest output. While scrolled up,
  a "Press END to follow output" button appears on the input box's top border —
  clicking it (or pressing `End`) re-pins to the bottom. (Mouse capture is
  enabled, which takes over the terminal's native text selection.)

### Fixed

- Transcript auto-follow now accounts for line wrapping: it scrolls by the
  rendered (wrapped) row count, so a newly sent message or streamed reply no
  longer hides below the fold until the next message bumped it into view.

### Added

- Initial scaffold: a Cargo workspace for an agentic coding harness driving
  OpenAI-compatible models.
- `hrdr-llm`: provider-agnostic `/v1/chat/completions` client with SSE streaming
  and tool-call reassembly (`Accumulator`).
- `hrdr-tools`: the locked MVP tool set — `read_file`, `write_file`, `edit`,
  `bash`, `grep`, `glob`, `todo_write` — with a registry and token-bounded
  outputs.
- `hrdr-agent`: the tool-calling agent loop with a minijinja system prompt.
- `hrdr-editor`: FSM-agnostic `EditorEngine` seam embedding the hjkl vim engine,
  projected from hjkl's `CoarseMode` so future disciplines plug in without
  churn.
- `hrdr-tui`: ratatui UI with a streaming transcript and a vim-keybound input
  pane.
- `hrdr` binary: interactive TUI by default, `hrdr run <task>` for headless,
  scriptable single-turn runs.
- `AgentConfig::load()`: layered config from `~/.config/hrdr/config.toml` with
  precedence CLI flag > env var > file > built-in default (never auto-written).
- `hrdr models` subcommand + `Client::list_models()` over `GET /models`.
- TUI: in-flight turn cancellation (`Esc` in Normal or `Ctrl+C` while running),
  transcript scrolling (`Ctrl+U`/`Ctrl+D`, `PageUp`/`PageDown`) with bottom
  auto-follow, and a live TODO panel driven by the `todo_write` tool.
- ANSI banner shown in `hrdr --help`.
- Offline unit tests for the tool set and the streaming `Accumulator`.
- **Temporary** managed backend: hrdr spawns a local `llama-server` (with
  `--jinja` for tool calling) by default, reuses an already-running endpoint if
  present, and tears it down on exit. Configurable via `--backend-model`,
  `--backend-bin`, `--backend-ctx`, `--backend-arg`; disable with
  `--no-backend`. To be removed once infr's serve path supports agentic tool
  use.

- Plain claude-style input discipline (`PlainEngine`), now the **default** input
  mode: always typing, `Enter` sends, `Shift+Enter` / `\`+`Enter` insert a
  newline, `Ctrl+G` opens `$EDITOR`/`$VISUAL`, with readline-style `Ctrl+A` /
  `Ctrl+E` / `Ctrl+W` / `Ctrl+U`. Vim keybindings remain available via `--vim`
  (or `vim = true` in config). The submit key and status hint are now decided by
  the `EditorEngine`, keeping the FSM-agnostic seam intact.

- Message queueing: submitting while a turn is running enqueues the message and
  runs it (FIFO) once the current turn finishes; the queued count shows in the
  status bar and `Ctrl+C` discards the queue along with the in-flight turn.
- Newline gestures in plain input now also accept **Alt+Enter** (reported by far
  more terminals than Shift+Enter); Shift+Enter still works where the terminal
  reports it, and `\`+Enter works everywhere.

[Unreleased]: https://github.com/kryptic-sh/hrdr/compare/v0.2.6...HEAD
[0.2.6]: https://github.com/kryptic-sh/hrdr/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/kryptic-sh/hrdr/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/kryptic-sh/hrdr/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/kryptic-sh/hrdr/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/kryptic-sh/hrdr/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/kryptic-sh/hrdr/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/kryptic-sh/hrdr/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/kryptic-sh/hrdr/releases/tag/v0.1.0
