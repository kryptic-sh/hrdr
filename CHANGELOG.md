# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

[Unreleased]: https://github.com/kryptic-sh/hrdr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kryptic-sh/hrdr/releases/tag/v0.1.0
