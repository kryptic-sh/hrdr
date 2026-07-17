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
- **P3 `--max-cost` with unpriced models** — opt-in `allow_unpriced` config +
  `--allow-unpriced` CLI flag: unpriced calls proceed uncounted, the cap still
  enforces on priced usage, and any total that excluded unpriced usage renders
  as `≥ $X.XX (excludes unpriced usage)` (bare figure only when complete);
  NDJSON usage events carry `cost_partial`.
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
- **P2 unbounded internal channels** — TUI event channel bounded at
  `TUI_EVENT_CAP = 1024` behind a coalescing `EventSender` (adjacent
  text/reasoning/tool-output deltas merge into the backlog tail; control events
  queue losslessly; turn-end `drain().await` flushes before `Done`); MCP stdio
  writer bounded at 64 with `send().await` backpressure; shutdown drops
  receivers so blocked senders error instead of deadlocking. Also deflaked the
  session-reservation tests (env-lock race on `XDG_DATA_HOME`).
- **P2 config validation** — accumulated named diagnostics instead of silent
  fallbacks: config-file boundary/semantic errors (zero caps and limits,
  malformed file, `context_window` < compaction reserve) refuse startup with
  every problem reported together; invalid `HRDR_*` env values warn and keep the
  current value; unknown UI enum values warn naming valid options (surfaced via
  startup notice in the TUI, stderr headless).
- **P2 headless/PTY integration coverage** — real-binary tests against a
  std-only mock endpoint (`apps/hrdr/tests/{common,headless}.rs`): streamed text
  turn, tool round trip, full NDJSON contract (every stdout line JSON, one final
  `done`, error event + nonzero exit), network failure, max-cost stop; PTY
  harness extended with prompt-submit/streamed reply, Esc-cancel, resize, and
  Ctrl+D clean exit. The stdout/stderr/exit contract is documented in the
  `headless.rs` module header.
- **Monolith, first slices** — `config.rs` (~1.5k lines), `budget.rs` (114
  lines), lifecycle `hooks.rs` (54 lines), turn input/delivery state
  (`turn_state.rs`, 155 lines), turn execution (`turn_loop.rs`, ~1.1k lines),
  and compaction/context management (`compaction.rs`, 691 lines) extracted with
  API preserved.

## Priority map (open items)

| Priority | Finding                                                | Main impact                          |
| -------- | ------------------------------------------------------ | ------------------------------------ |
| P1       | `hrdr-agent/src/lib.rs` remains a ~13.6k-line monolith | High change cost and coupled testing |

---

## P1: `hrdr-agent/src/lib.rs` is a multi-domain monolith

(Promoted from P2 on review: in practice every change to the agent — prompts,
delegation, pruning, tools — routes through this one file, so its cost is paid
on every task, not occasionally. `config.rs`, `budget.rs`, lifecycle `hooks.rs`,
turn input/delivery, and turn execution slices are extracted; ~13.6k lines
remain.)

### Remaining extractions, in order

1. `delegation` and `worktree` (the `task*` tool family, spawn paths,
   transcripts)

### Verification strategy

For each extraction:

- Move-only commits; no behavior change in the same commit.
- Full tests and clippy remain green.
- Public API preserved via an explicit re-export list where exports move
  (`config.rs` is the template).
- Follow-up behavior changes target the newly isolated module.

---

## Audit caveats

- Source inspection plus post-implementation review; no dynamic penetration
  testing, fuzzing, or fault injection.
- Line references match the repository state on 2026-07-17 and may shift.
- Cross-process locking and shell-word parsing may require a dependency; project
  rules require user approval before adding one.
