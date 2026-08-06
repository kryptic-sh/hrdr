# Watch tool plan — non-blocking background watch with result delivery

Status: plan (not implemented). Written 2026-08-06; reviewed by a sub-agent
(`review`, read-only) before implementation. Every finding was re-verified
against the tree by the author before folding in — the blocking items (the two
tests pinning watch's absence, the shell.md "no polling tool" text) and the
should-fix items are incorporated below, marked with their review tag.

## Problem

The release procedure (`write_main.md` Releasing, and `:release` which defers to
it) requires **watching the tag's CI run to completion and confirming it
published** — a red build skips the publish jobs silently, so "tagged and
pushed" is not "released". But hrdr has no non-blocking primitive for it. The
model either blocks (a `sleep`-poll loop, or `gh run watch`, which is blocking
by design) — which is explicitly the wrong shape per the harness rules ("there
is no polling tool") — or it ends the turn and the release flow stalls until a
human notices the run finished and prompts it. The incident that motivated this:
the model improvised a blocking watch on the v0.11.x tag run and the user had to
cancel it; the run takes 12–19 min.

A tool is the answer: **spawn-return-deliver**, the same contract `task` already
has — call it, get an id immediately, end the turn, be woken when the condition
flips. The old `watch` tool (deleted 2026-07-30 for "available and ignored", 4
calls) was a repeat-a-command-and-show-output loop; this is a different job
(notify-on-condition), and it ships with its consumer wired (the Releasing step)
so it does not rot the way the old one did.

## Binding decisions

Made 2026-08-06 by the owner. Alternatives are recorded under
[Considered and declined](#considered-and-declined).

1. **General check-command watcher, not CI-specific.** `watch { check, … }`
   re-runs a shell command until it exits 0. The CI case is one consumer
   (`check: "gh run view <id> --json status -q .status | grep -qx completed"`);
   watching a deploy's health endpoint or a build artifact appearing is the same
   primitive. A `watch_gh_run` with a fixed vocabulary would be safer but
   single-use.
2. **The result wakes the agent's turn** — delivered exactly like a finished
   background sub-agent's report (a user-role context message plus a Notice), so
   the model can finish the release ("enumerate the jobs, confirm the artifact
   landed, report released") on wake. If it only toasted, the model could not
   complete the flow.
3. **Model-invocable**, with the end-turn contract in the tool description.
4. **The watcher reports "the run finished", never "the release is published".**
   The result carries the last check's output and how long it watched; the
   confirmation (enumerate jobs, check artifacts) stays a model step on wake, as
   the Releasing section already prescribes. Bake the confirmation into the
   watcher and it becomes a lie detector that cannot lie.

## Design

### Schema (tool-shape rules: one noun-tool, flat args, time-is-seconds)

```
watch {
  check:        string, required  — a shell command; exit 0 = condition met
  interval_secs: int,  default 30, min 15  — poll period
  timeout_secs:  int,  default 600, max 3600 — whole-watch ceiling
}
```

No removed/renamed params, so nothing to poison.

### Lifecycle

1. The handler validates args, mints an id, pushes a
   `BackgroundTask { id, tool_id: Some(<this call>), label, log, done: false, result: None, delivered: false, cancelled: false }`
   onto `ToolContext.background_tasks`, spawns a tokio poller task, and
   **returns immediately** (push → spawn → ack, the same order
   `spawn_background` uses so the id is addressable as soon as the call
   returns):
   `watching #N — polls <check> every 30s, up to 10 min; I'll be woken when it completes. End your turn. Cancel with task_cancel N.`
2. The poller loop:
   - Run `check` **once immediately** (a run already finished is an instant
     answer), then every `interval_secs`.
   - Each check runs through the same confined path a `shell` call takes:
     `crate::check_guardrails(&check, &ctx.guardrails)` first (shell.rs:346),
     then build the command with
     `crate::sandbox::sandboxed_shell_command(shell, &check, &ctx.sandbox, &ctx.sandbox_notices)`
     - `cmd.current_dir(&ctx.cwd)` (as `ShellTool` does, shell.rs:351-357) and
       run it via `run_streamed_command(cmd, &check, CHECK_TIMEOUT, false, ctx)`
       (shell.rs:476, `pub(crate)`, reused from within the same crate). A
       `CommandRun.passed == true` ends the watch as a success. **[S4]** The
       plan's first draft wrote a `Shell` into `run_streamed_command` — its
       first argument is the built `tokio::process::Command`; skipping the
       sandboxed builder silently unconfines the check.
   - **Staleness pairing, like `ShellTool`** **[S3]**: snapshot
     `ctx.tracked_sigs()` before the run and call
     `ctx.note_modifying_command(&before, &check)` after (shell.rs:374, 392), so
     a mutating check names itself as the culprit for the read-before-edit guard
     — the plan's own `read_only() -> false` concedes checks may mutate.
   - Per-check timeout `CHECK_TIMEOUT = 60s` (a stuck `gh` must not hold a poll
     slot for the standard 5-min tool timeout). An `Err` from
     `run_streamed_command` (the check was killed at its deadline) counts as
     "failed this round", never "watch failed" — only the whole-watch
     `timeout_secs` ends the watch.
   - Append each check's output to the entry's `log` for the panel, **ring-
     buffered** **[S7]**: up to 240 polls × 5120 bytes ≈ 1.2 MB worst case
     otherwise, held in the registry and rendered every frame. A fixed cap (e.g.
     the last ~16 KB) is enough — the final result carries the last output
     verbatim.
   - **The poller must not stream into the finished call's channel** **[S6]**:
     `run_streamed_command` calls `ctx.emit` per line, and the poller's `ctx`
     clone still holds the `watch` call's stream channel (kept alive by the
     poller's tx for up to `timeout_secs`). Bounded and lossy (`try_send`), but
     the progress the panel shows should come from the entry `log` — null the
     `ctx.stream` in the poller's context before the first check.
   - Success → `done = true`, `result = truncate_middle(last output + elapsed)`.
     Whole-watch `timeout_secs` exceeded → `done = true`, result names the
     timeout and the last output. The result cap is **hrdr-tools' own constant**
     — `BACKGROUND_REPORT_MAX_BYTES` is `pub(crate)` in hrdr-agent and not
     importable from `watch.rs`.
3. **Self-termination, no handle plumbing.** Before each poll the poller
   re-locks the registry and looks up its own entry; if the entry is gone or
   `cancelled`, it stops. That covers every shutdown path with zero new
   machinery:
   - `task_cancel N` sets `cancelled = true` **and `done = true`**
     (delegation.rs:1777-1778) **[S5a]** — the poller sees it next poll and
     stops; the result is discarded, never delivered (`drain_background` skips
     `cancelled`, turn_state.rs:102). Note the poller is **not** in
     `bg_handles`, so `task_cancel` cannot abort an in-flight check — a check
     subprocess already running when cancel lands runs out its `CHECK_TIMEOUT`;
     worst-case stray life is one interval plus one in-flight check, not "one
     interval" as the first draft claimed **[S5b]**. `task_cancel`'s success
     message ("anything it had already written is still there — check
     `git diff`") is sub-agent wording and misdescribes a cancelled watch — step
     4 fixes that wording too.
   - `Agent::abort_background_tasks` clears the whole registry
     (delegation.rs:2203-2207) — `/clear` (the only non-test caller,
     lib.rs:2095) and agent teardown; the poller stops at the next poll, and
     `timeout_secs ≤ 3600` bounds it absolutely. Process exit kills the runtime;
     nothing to clean.
   - `drain_background` prunes the entry after delivery (turn_state.rs:115) —
     the poller stops once delivered.
4. **Delivery needs no changes**: `drain_background` (turn_state.rs:86) already
   runs before every model request, folds `done && !delivered && !cancelled`
   entries into the conversation as user-role messages with a Notice, and
   appends `BACKGROUND_ARRIVAL_REMINDER`. A watch is just another
   `BackgroundTask` producer. **Wake is already answered** **[S8]**: the TUI's
   `maybe_deliver_background` (hrdr-tui/src/app.rs:2941-2960) fires every frame
   when idle, is generic over `done && !delivered`, and launches the opener-less
   turn (turn_loop.rs:505-518) that `drain_background` fills — a finished watch
   wakes the model exactly like a finished sub-agent, with zero extra work.

### Id space

Task ids come from `BG_SEQ` (delegation.rs:16). Task and watch entries share one
registry, and `drain_background`/`task_cancel`/the TUI wake all match on `id`,
so the two must share one counter. Move the counter next to the type: add
`BackgroundTask::next_id()` in hrdr-tools (a static `AtomicU64` beside the
struct, lib.rs:117), and make delegation's `BG_SEQ` delegate to it (keeping the
name `BG_SEQ` — the only other dependents are `spawn_background`
(delegation.rs:186) and two doc comments). **Preserve the `fetch_add(1) + 1`
convention** so ids start at 1 and the id-dependent tests do not shift.

### Registration and gating

`WatchTool` in hrdr-tools, registered in `ToolRegistry::with_defaults`
(lib.rs:1607) **only when `r.shell()` is `Some`**, right after `VerifyTool`
(lib.rs:1623-1625). It holds a `Shell` like `VerifyTool::new(shell)`.
Consequences: no shell on PATH → no `watch` (it is all subprocesses); `jail` has
no shell → no `watch` in jail (consistent with "jail runs nothing"); a read-only
agent gets **shell but not `watch`** **[S1]** — the read-only set is
`read_only_names()` plus an explicit `shell_tool_names` extension
(lib.rs:1673-1694), and `watch` is not in `read_only_names` (it is
`read_only() -> false`). Accept that `explore`/`review`/`plan` lack watch —
matches the deleted `watch`'s fate and keeps
`assert_eq!(tools("explore"), readers)` (lib.rs:6220) green; do not add watch to
the keep-list.

### Tool trait overrides

- `read_only() -> false` — a check command may mutate the tree; it must not be
  treated as an observation.
- `concurrent() -> true` — each watch is self-contained (its own entry, its own
  task); two watches are safe in parallel, same argument as `task`
  (lib.rs:1314-1328).
- `repeatable() -> true` — the third identical `watch` call must not trigger the
  `RepeatGuard` nudge; the trait doc already names "waiting on an external end
  state" as exactly this case (lib.rs:1330-1342).
- `timeout_secs() -> None` **[S2]** — **mandatory**, not optional: the tree
  already documents `watch` as a self-managed deadline tool in three places
  ("Only `shell` and `watch` do that", lib.rs:1376-1380; "`shell`, `watch` — the
  self-managed ones, whose own descriptions explain what expiry means",
  lib.rs:1469-1470; "a tool that manages its own deadline (`shell`, `watch`)
  reports `None` and is awaited untouched", lib.rs:1758-1760), and
  `timed_parameters` special-cases the schema wording for exactly these two. The
  dispatcher otherwise applies its own reading of the model's `timeout_secs`
  (`call_timeout_secs`, lib.rs:1509). Without the override the same
  `timeout_secs` means "whole-watch ceiling" in the handler and "per-call
  deadline" in the dispatcher — two semantics for one field.

### Confinement of `check`

Identical to `shell`: guardrails on the command line (shell.rs:346), the
sandboxed spawn (S4 above), the secret-file filter + diff redactor and per-line
caps inside `run_streamed_command`'s ingest path, and the staleness pairing (S3
above). Check output reaching the panel and the result is bounded
(`truncate_middle`, capped by **hrdr-tools' own constant** — see Lifecycle). The
description tells the model the check runs under the same guardrails as `shell`.
Two ledger effects to be aware of, not to fix: every check run is `record`ed
into `ctx.verification` (shell.rs:800-806) exactly like a `shell` run —
consistent with shell, but confirm the ledger's semantics never read a watch
check as a project-verification pass — and `run_streamed_command`'s
`prior_spool` note can surface "you already have this output" text inside a
delivered watch result (harmless; the model reads it as context).

## Files

| File                                            | Change                                                                                                                                                                |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/hrdr-tools/src/tools/watch.rs` (new)    | `WatchTool` — schema, handler, poller                                                                                                                                 |
| `crates/hrdr-tools/src/tools/mod.rs`            | register the module                                                                                                                                                   |
| `crates/hrdr-tools/src/lib.rs`                  | `BackgroundTask::next_id()`; register `WatchTool` in `with_defaults` (shell-gated)                                                                                    |
| `crates/hrdr-agent/src/delegation.rs`           | `BG_SEQ` → `BackgroundTask::next_id()`; `task_cancel`'s "no task #id" message names `watch` too, and its success message is de-sub-agented for a watch **[S5]**       |
| `crates/hrdr-agent/src/lib.rs`                  | rewrite `each_builtin_subagent_gets_exactly_the_tools_it_should` — `"watch"` leaves the `gone` list (lib.rs:6236) **[B1]**                                            |
| `crates/hrdr-agent/src/prompt.rs`               | rewrite `the_prompt_says_to_end_the_turn_rather_than_wait` — it asserts no `watch` def and documents its removal (prompt.rs:1966, 1940-1947) **[B1]**                 |
| `crates/hrdr-agent/src/templates/shell.md`      | the "There is no polling tool … end your turn; the user runs it" block (shell.md:82-88) becomes "call `watch` for an external end state, then end your turn" **[B2]** |
| `crates/hrdr-agent/src/templates/write_main.md` | Releasing step 143-148: "WATCH THE TAG'S RUN" → call `watch` on the run, end the turn, on wake enumerate jobs + confirm artifacts                                     |
| `CHANGELOG.md`                                  | one `Added` entry under Unreleased (user-facing: a new tool)                                                                                                          |
| `docs/backlog.md`                               | no change until shipped, then archive this plan per the convention                                                                                                    |

No change to `turn_state.rs` (delivery), the TUI's wake
(`maybe_deliver_background` already fires for any `done && !delivered` entry),
or the background-panel rendering.

## Steps (each verified before the next)

1. **`BackgroundTask::next_id()`** in hrdr-tools; delegation's `BG_SEQ` becomes
   a thin delegate (same name, `fetch_add(1) + 1` convention). Verify:
   `cargo test -p hrdr-agent` (id-dependent tests), ids unique across task +
   watch.
2. **WatchTool skeleton**: module, registration (shell-gated), schema,
   description, all four overrides (`repeatable`/`concurrent`/
   `read_only`/`timeout_secs -> None`). Verify: a tool test pins the schema
   bounds (`interval_secs < 15` and `timeout_secs > 3600` refused, `check`
   required) and the end-turn contract wording.
3. **Handler + poller**: validate → mint id → push entry → spawn poller → ack.
   Poller per [Lifecycle](#lifecycle) (sandboxed build, guardrails, staleness
   pairing, ring-buffered log, nulled stream). Verify: the tests below, each red
   against the partial implementation it guards (not against the absent tool —
   every watch test fails on no-tool trivially).
4. **`task_cancel` for watches**: the "no task #id" message names `watch` too,
   and the success message stops claiming the cancelled thing had "written
   something to check with `git diff`" when the id is a watch's. Verify: its
   existing tests plus the cancel test below.
5. **Prompt rewiring** — the three prompt touches together **[B1+B2]**:
   `shell.md`'s "no polling tool" block, the Releasing step in `write_main.md`
   (and `release.md`'s reference stays accurate), and the two tests that pin the
   absence (`prompt.rs`'s `the_prompt_says_to_end_the_turn_rather_than_wait` and
   `lib.rs`'s `each_builtin_subagent_gets_exactly_the_tools_it_should` —
   `"watch"` leaves the `gone` list). Verify: the prompt-corpus tests
   (`says`/`unwrapped`) pass; prettier on the templates per the standing
   markdown rule.
6. **Changelog + docs**. Verify: prettier.
7. **Full gate** — `cargo fmt --all --check`, clippy workspace, build, nextest.
   Then commit per the project convention. **The gate is the check that B1 was
   actually fixed** — it goes red on the two pin-the-absence tests otherwise.

## Tests

The red-first discipline differs for a brand-new tool: every watch test fails on
"no tool" trivially, so the meaningful reds are against **partial
implementations** — each test below names the partial it guards (a missing
guardrails call, a blocking first poll, an ignored cancel). Each also ships with
a "delivered exactly once" assertion.

1. `watch_returns_immediately_with_an_id` — calling the tool returns fast with
   `#N` in the ack and a `done: false` registry entry; it must not block on the
   check (a `sleep 5 && exit 0` check must not delay the return). Red against a
   poller that awaits the first check before acking.
2. `watch_completes_instantly_when_the_check_passes_first` — `check: "true"` →
   entry done with a success result shortly after; `drain_background` delivers
   it as a message, prunes the entry, and a **second drain delivers nothing**
   (exactly-once).
3. `watch_delivers_after_the_condition_flips` — a check that fails twice then
   passes (a small script) → after the third poll the result lands via
   `drain_background`; the entry is pruned; a second drain adds nothing.
4. `watch_times_out_with_a_failure_result` — `check: "false"` with a tiny
   timeout → done, result names the timeout.
5. `watch_stops_when_cancelled` — `task_cancel` mid-watch → entry cancelled,
   poller stops, nothing delivered even after the check would have passed. Red
   against an implementation that ignores `cancelled`.
6. `watch_check_runs_under_guardrails` — a guardrailed command (`git add -A`) →
   the watch reports the guardrail refusal, does not run it. Red against a
   poller that skips `check_guardrails`.
7. `watch_schema_bounds` — sub-floor interval, over-ceiling timeout, missing
   `check` all refused with instructive errors.
8. Prompt test — the Releasing section names `watch` for the tag-run step, and
   `shell.md` no longer says "no polling tool" (word-tracked via
   `says`/`unwrapped`, not `contains`).
9. e2e — a watching row appears in the background panel and flips to done,
   waking the model (the harness behind `e2e.rs:6218-6260`, which proves the
   wake for sub-agents, is generic over `BackgroundTask` and works for a watch's
   entry unmodified).
10. Not automated: a real tag-run watch against GitHub (needs a live run) —
    manual smoke, same status as the DeepSeek provider smoke in the backlog.

## Risks / unknowns

- **`run_streamed_command` is `pub(crate)`** — fine, WatchTool is in the same
  crate. It returns `Ok` for a non-zero exit; success is `.passed`, and a
  timeout returns `Err` (the run was destroyed). The poller must treat `Err` as
  "check failed this round", not "watch failed" — only the whole-watch
  `timeout_secs` ends the watch.
- **Watch lost across processes** (`/resume` of a fresh process, session
  restore): `BackgroundTask` lives in `ToolContext`, not `SessionState`, so a
  watch does not survive a restart. Consistent with running sub-agents (also
  process-local); the release flow re-watches. Within-process `/resume` and
  `/clear`-without-new-process **keep** the registry (only
  `abort_background_tasks` clears it), so a watch survives a same-process resume
  — the earlier draft's "lost on resume" was only half true.
- **Orphan-entry window**: an abort between the registry push and the poller
  spawn leaves a `done: false` entry with no worker, delivered never, until
  `/clear`. The same window `spawn_background` already has (push at
  delegation.rs:308-319, spawn at 330) — accepted shape; the poller can adopt
  its entry (set a marker) to close it if cheap.
- **The old `watch` lesson**: the tool ships _with_ the prompt rewiring (step 5)
  in the same slice, so the release procedure drives adoption — and the "no
  polling tool" instruction that would contradict it is gone in the same change
  **[B2]**.
- **One more tool def** on the wire (~200-400 tokens) for every shell-bearing
  agent. Acceptable; the def is small and cached like the rest of the tools
  block.
- **A model could misuse `watch` to re-implement the deleted polling** (e.g.
  `watch` on a local file's mtime). Not a defect — that is the same primitive
  doing its job; the description should steer toward external async state (CI,
  deploys) so it is not offered as a general repeat-command tool.
- **Two tests pin watch's absence today** and must be rewritten in the same
  slice or the gate goes red:
  `each_builtin_subagent_gets_exactly_the_tools_it_should` (lib.rs:6236,
  `"watch"` in the `gone` list) and
  `the_prompt_says_to_end_the_turn_rather_than_wait` (prompt.rs:1966, asserts no
  `watch` def and documents the deleted tool at prompt.rs:1940-1947) **[B1]**.

## Considered and declined

- **`watch_gh_run` (CI-specific)**: safer (no arbitrary command), but one use
  case; the general form costs nothing extra because the check runs through the
  existing `shell` confinement anyway.
- **Notify-only (toast, no wake)**: the model could not finish the release;
  rejected per decision 2.
- **A TUI-only watch panel** (no tool): the release flow is model-driven; a
  panel alone leaves the confirmation to the user.
- **Reusing `gh run watch` in `shell`**: blocking by design; the incident.
- **Baking the publish confirmation into the watcher**: see decision 4.
