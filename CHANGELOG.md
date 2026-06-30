# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Status-bar context size no longer drops to 0 between turns: `last_usage` is
  kept across turns (only the live per-turn counters reset), so the displayed
  context persists until the next turn's usage refreshes it.

- Scrollbar thumb position: it now reaches the bottom when following the output
  (was stuck midway) — `content_length` is the number of scroll positions, not
  the raw line total, matching ratatui's `position` mapping.

### Changed

- Quitting now requires a double Ctrl+C: the first idle Ctrl+C arms a confirm
  (any other key/mouse action disarms it) and shows a "Press Ctrl+C again to
  quit" banner on the input box's top border (taking priority over the follow
  button); a second consecutive Ctrl+C quits. While a turn is running the first
  Ctrl+C still interrupts it. Ctrl+Q remains an immediate quit.

### Added

- `/paste` inserts the system clipboard into the input — and if the clipboard
  holds a path to an existing file, attaches it as an `@mention` instead.
- `/help` is now grouped by category (Session, Model & sampling, Files &
  context, Reply, Appearance, Other) with aligned descriptions and a tips line,
  instead of one flat list.
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
  `.gitignore`/`.ignore` (and parents/global) plus `.git/info/exclude` via the
  `ignore` crate; outside a git repo it falls back to skipping known VCS/build
  and hidden directories. On send, each `@path` is expanded into the referenced
  file's contents for the model (bounded to 100 KB/file), while the transcript
  still shows the message exactly as typed. Complements `/add`.
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

[Unreleased]: https://github.com/kryptic-sh/hrdr/commits/main
