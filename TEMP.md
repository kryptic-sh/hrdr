# hrdr Audit Findings

Date: 2026-07-17 (audit) · Last updated: 2026-07-18 (turn-loop extraction and
verification refresh)

Scope: read-only inspection of the Rust workspace, concentrating on correctness,
security, persistence, network boundaries, architecture, tests, and user-facing
workflow. Findings were verified against source before implementation, and the
implementation was reviewed commit-by-commit afterward.

## Status

Phases 1–3 of the original plan, plus slices of 4–5, have shipped on `main`. All
completed findings have been removed from this document; what remains below is
open work plus new findings from implementation review and source
re-verification.

Completed and verified (with review fixes folded in):

- **P0 OAuth wrong-state termination** — state checked before error authority,
  wrong-state callbacks keep the listener alive; bonus hardening: per-socket
  read deadline, fragmented request-line reads.
- **P0 API-key credential-store wipe** — `auth.toml` mutation is fail-closed on
  malformed or unreadable stores; byte-preservation regression test. OAuth
  mutation now has matching fail-closed behavior and regression coverage.
- **P1 SSE overflow** — first consumer-side checks in all four consumers, then
  API-hardened: `push`/`finish` return `Result<_, SseOverflow>` and
  `overflowed()` was removed, so future consumers cannot repeat the bug.
- **P1 autosave truthfulness** — `save_session` returns `Result`; the TUI
  surfaces a deduplicated failure notice and keeps the state dirty for retry.
- **P1 request timeout** — default `Some(300)`, applied as connect + idle-read
  (streaming-safe), not a total deadline.
- **P1 unbounded HTTP bodies** — shared `capped_read` module across
  client/anthropic/codex/oauth; the OpenRouter key-exchange error no longer
  echoes the body (which is the API key).
- **P1 cost cap** — fail-closed on unpriced models; budget preflight before
  every model call including compaction/wrap-up summarizers, whose usage is now
  accounted; CLI rejects NaN/negative caps.
- **P1 wire logs** — `0600` on Unix, size cap, cap warning routed through the
  event channel instead of stderr under the TUI.
- **P1 corrupt sessions** — size limit with TOCTOU-safe bounded read, malformed
  model fields error instead of silently defaulting, `/resume` error rows,
  `/doctor` diagnostics, no more `eprintln!` under the TUI.
- **P1 session ID races** — `O_EXCL` reservation with PID+timestamp lock
  content, drop-based release, stale-lock reaping (review fixes: unparseable
  locks age by mtime; the fallback reservation loop is bounded so an unwritable
  session dir can no longer hang the app).
- **P2 parent-directory fsync** after atomic rename (Unix).
- **P2 tag-release status job** — a failed or skipped Publish on a tag run now
  fails the workflow loudly.
- **P2 watcher-test platform sensitivity** — asserts eventual reload, not raw
  event multiplicity.
- **P2 endpoint host classification** — `hrdr-agent` now consumes the public
  `hrdr_llm::url_host` helper instead of maintaining a duplicate classifier.
- **P3 README stale release line** — removed.
- **P2 credential write races** — `store_lock.rs` cross-process `O_EXCL`
  advisory lock (PID+timestamp, RAII release, stale reaping by dead PID or 60s
  age, bounded ~5s retry) held across the whole read-modify-write in
  `save_token_at` and `save_oauth_at`; concurrency tests for different- and
  same-provider writers on both stores.
- **P3 Windows file confidentiality** — documented honestly (README, module
  docs, `write_atomic`/`open_wire_log` comments): Unix enforces 0600 every
  write; Windows relies on default ACLs of the per-user profile dir
  (`~/.config/hrdr` under `%USERPROFILE%`, not `%APPDATA%`); plus an
  any-platform warning that a world-readable `HRDR_LOG_REQUESTS` target leaks
  request data. Per-user Win32 ACL code would need a new dependency and was
  deliberately not added.
- **P3 wire-log rotation** — at the cap the log rotates to `<name>.1` (newest
  window kept, ≤2× cap on disk, 0600 preserved on both files); rotation failure
  falls back to stop-at-cap with a one-shot warning; README documents the bound.
- **P2 `$EDITOR` quoting** — hand-rolled zero-dep shell-word splitter
  (`split_shell_words`) replaces `split_whitespace()`; quotes, escapes, and
  unterminated-quote recovery covered by 12 unit tests.
- **Monolith, first slices** — `config.rs` (~1.5k lines), `budget.rs` (114
  lines), lifecycle `hooks.rs` (54 lines), turn input/delivery state
  (`turn_state.rs`, 155 lines), and turn execution (`turn_loop.rs`, ~1.1k lines)
  extracted with API preserved.

## Priority map (open items)

| Priority | Finding                                                | Main impact                            |
| -------- | ------------------------------------------------------ | -------------------------------------- |
| P1       | `hrdr-agent/src/lib.rs` remains a ~13.6k-line monolith | High change cost and coupled testing   |
| P2       | Unbounded internal channels                            | Memory growth under sustained overload |
| P2       | Headless/PTY integration coverage is narrow            | Regressions escape unit tests          |
| P2       | Configuration validation is permissive and fragmented  | Silent misconfiguration                |
| P3       | `--max-cost` unusable with unpriced/local models       | Capped local runs impossible           |

---

## P1: `hrdr-agent/src/lib.rs` is a multi-domain monolith

(Promoted from P2 on review: in practice every change to the agent — prompts,
delegation, pruning, tools — routes through this one file, so its cost is paid
on every task, not occasionally. `config.rs`, `budget.rs`, lifecycle `hooks.rs`,
turn input/delivery, and turn execution slices are extracted; ~13.6k lines
remain.)

### Remaining extractions, in order

1. `compaction` and context management (pruning, elision, tail windows)
2. `delegation` and `worktree` (the `task*` tool family, spawn paths,
   transcripts)

### Verification strategy

For each extraction:

- Move-only commits; no behavior change in the same commit.
- Full tests and clippy remain green.
- Public API preserved via an explicit re-export list where exports move
  (`config.rs` is the template).
- Follow-up behavior changes target the newly isolated module.

---

## P2: Internal unbounded channels can grow memory indefinitely

### Evidence

- TUI event channel: `crates/hrdr-tui/src/app.rs` uses
  `mpsc::unbounded_channel()`.
- MCP stdio writer: `crates/hrdr-tools/src/mcp/client.rs` uses an unbounded
  string channel.
- Other core tool streaming paths already use bounded channels, demonstrating
  the available pattern.

### Impact

A sustained producer faster than the UI or subprocess consumer can allocate
without bound: token-by-token events from fast local models, paused terminals,
or a stalled MCP child.

### Recommendation

- Bounded channels; coalesce adjacent text/reasoning chunks before enqueue where
  ordering permits.
- Backpressure MCP requests rather than buffering unlimited serialized JSON.
- Define which low-value display events may be dropped or merged; never drop
  tool completion, error, or state-transition events.

### Required tests

- Fast producer / slow consumer stays within bounded capacity.
- Text coalescing preserves exact visible content and ordering.
- Cancellation/shutdown cannot deadlock while a producer awaits capacity.
- MCP child exit releases blocked senders.

---

## P2: Headless process-level coverage is incomplete

### Evidence

- `apps/hrdr/tests/smoke.rs` checks version/help/missing-prompt, not a complete
  headless model turn.
- `apps/hrdr/src/main.rs` headless behavior (prompt prep, MCP setup, lifecycle
  hooks, NDJSON, usage, exit codes) is tested below the process boundary.
- PTY tests (`apps/hrdr/tests/tui_pty.rs`) cover launch/quit and
  unreachable-endpoint resilience only.

### Impact

Regressions in CLI wiring, stdout/stderr policy, event serialization, exit
codes, terminal restoration, and resume flows can pass unit tests.

### Recommendation

Run the real binary against a scripted local HTTP server: plain streamed text,
tool round trip, NDJSON stream, timeout/network failure, max-step wrap-up,
max-cost enforcement, hook/MCP notices. Define the `--json` contract explicitly
(today MCP/hook notices go decorated to stderr while events are NDJSON on stdout
— automation must consume two formats; prefer JSON events for successful
lifecycle notices). Extend the PTY harness with prompt-submit, Escape-cancel,
`/new`, save/exit/resume, resize, and EOF/panic restoration.

### Required assertions

- Every stdout line in JSON mode parses as JSON; ordering and required fields
  stable; defined stderr behavior; errors produce a JSON error event and nonzero
  exit.

---

## P2: Configuration validation is permissive and fragmented

### Evidence

- File application and env parsing (now in `crates/hrdr-agent/src/config.rs`)
  copy many raw values directly and often ignore invalid values.
- Edge values: zero sub-agent limits, zero timeout, zero output limits,
  compaction reserves incompatible with context windows.
- UI enum-like settings fall back silently in `crates/hrdr-app/src/config.rs`.

### Impact

Typos and nonsensical boundary values silently alter behavior or disable
features; users get defaults rather than diagnostics.

### Recommendation

Deserialize into `RawConfig`, validate into runtime config with typed values
(`NonZeroUsize`, bounded durations). Accumulate diagnostics so users fix all
invalid fields at once. The `config.rs` extraction makes this a natural
follow-up in the isolated module.

### Required tests

- Zero/max boundary values, invalid env strings, unknown enum values, context
  window smaller than compaction reserve, conflicting provider/global settings,
  multiple simultaneous errors reported together.

---

## P3: `--max-cost` is unusable with unpriced/local models

(New finding from the implementation review.)

### Evidence

- The cost cap is now fail-closed: `budget_preflight` errors on any unpriced
  model when `max_cost` is set. The audit's suggested `--allow-unpriced` escape
  hatch was not implemented.

### Impact

Strictly safe, but a user running a local model (never in the pricing catalog)
cannot use `--max-cost` at all — even though sub-agent calls on priced providers
could still be capped. They must drop the cap entirely.

### Recommendation

Add `--allow-unpriced` (or `max_cost_partial = true`): unpriced calls proceed
uncounted, the cap applies to priced usage, and reported totals say
"partial/unknown", never a complete-looking `$0.00`.

---

## Audit caveats

- Source inspection plus post-implementation review; no dynamic penetration
  testing, fuzzing, or fault injection.
- Line references match the repository state on 2026-07-17 and may shift.
- Cross-process locking and shell-word parsing may require a dependency; project
  rules require user approval before adding one.
