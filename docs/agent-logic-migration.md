# Agent-logic migration: hrdr-agent owns all agent logic, hrdr-app is only glue

Status: **Phase 1 complete** (on main). Phase 2 pending.

## Progress

- `3c49a2c` — slice 1: session persistence → hrdr-agent.
- `14b313b` — slice 2: Pane/PaneSet → hrdr-agent.
- `7a54f47` — slice 3: sub-agent persists its own SessionState
  (`Session::save_to_path`) on every round + at completion.
- `d8c2afc` — perf: session files written as compact JSON (not pretty).
- `51bf3eb` — perf: sub-agent snapshot stores messages + metadata only; its
  transcript lives in the sibling jsonl (rebuilt via `read_transcript` on load),
  so a round no longer re-serializes the whole transcript.

## What persists from the transcript (decided)

Only the **`AgentEvent` fold** persists — User (`Steered`), Assistant (`Text`),
Reasoning text, Tool (args+results), agent `Notice`→`System`. Frontend-pushed
**chrome is NOT persisted** (slash-command `System` output, `/diff` `Diff`,
per-turn `Stats`, `Header`, `Reasoning.took_ms`): it is display-only, not
context, and not needed to resume. So the transcript is a pure fold of the event
stream for every agent — no `Pushed` record, no `record_pushed` (removed).

Progress since: `Record::Pushed` removed; sub-agent transcript writer now lives
on the `LiveSubagent` entry and is driven by `LiveSubagents::record`, so a
sub-agent's **steered** turns persist too (not just its delegated run).

## Remaining: W2 — main agent onto the event-fold jsonl (the O(n²) fix + 1:1 finish)

The MAIN agent still embeds `Vec<Entry>` in its `.json` and full-rewrites the
whole multi-MB session on every tool round (frontend `persist_mid_turn`, UI
thread) — O(n²). Give it the sub-agent model: transcript appended to a jsonl per
event, `.json` carries only messages+metadata, resume rebuilds via
`read_transcript`. Two parts, because of one constraint found in the code:

1. **Unify main user-input onto the event stream.** Main user messages today
   enter the transcript via `push_entry(Entry::user)` (`app.rs`
   `launch_turn_shown`), NOT as events — so an event-fold jsonl would drop them.
   Record the submission as `AgentEvent::Steered` via `record(MAIN_KEY, …)` (as
   sub-agents already do their prompt) and let the sync-fold render it; drop the
   direct `push_entry`.
2. **Flip the format + rebuild on resume.** Main gets a
   `sessions/<cwd>/<id>.jsonl` writer (created on id assignment), `.json` drops
   the transcript, and the resume path rebuilds `transcript` from the jsonl
   (listing stays cheap — no rebuild). Add a resume round-trip test on a real
   multi-round session.

Risk: touches the main submit path + resume format — its own slice(s), tested
against a genuine session before landing.

## Principle

`hrdr-agent` owns **all agent logic**. `hrdr-app`'s only job is **agent↔TUI
communication** — the slash-command dispatch layer, input completion, and the
render-facing view models. Anything that is about an agent's _state,
persistence, or lifecycle_ belongs in the core crate.

This is the same move the transcript unification made (model + builder →
hrdr-agent); this finishes it for session state, panes, and the rest.

## Why (the divergence it fixes)

Sub-agents run **inside** `hrdr-agent` (`delegation.rs`). The main agent's
persistence and lifecycle plumbing — `SessionState`, `save_session`,
`autosave`/`persist_mid_turn`, `Pane`/`PaneSet` — lives **above** it in
`hrdr-app`. So a sub-agent cannot reach it: its `History` events are dropped,
its `messages` never persist, it can't be revived, steered when finished, or
survive a resume. Move the persistence/lifecycle into core and a sub-agent
becomes just "a Pane whose id isn't `Main`" — persistence, revive, steer, resume
all fall out of one shared path.

## Boundary

**Move to hrdr-agent (agent logic):**

- `session.rs` — `SessionState`, `Session`, save/load, retention sweep, locks,
  path/dir helpers, `resolve_session`, `list_sessions`, `unique_session_id`.
- from `sessions.rs` — `save_session` + `SaveOutcome` (persistence). Its
  listing/fuzzy-filter/`session_diagnostics` (presentation) stay in hrdr-app.
- from `util.rs` — `session_name_from` (derives a name from message history).
- `pane.rs` — `Pane`, `PaneSet`, `PaneView`
  (manage-a-set-of-agent-conversations; view fields ride along as plain data the
  TUI reads/writes).
- (Phase 2) skills model, login auth/provider core, agent-side `util` helpers.

**Stays in hrdr-app (glue / frontend):**

- `commands/` (the agent↔TUI command layer), `completion.rs`, `subagents.rs`
  (panel view model), `status.rs`, `format.rs`, `effort.rs`, `highlight.rs`,
  `palette.rs`, `themes.rs`, `history.rs`, `config.rs` (`UiConfig`),
  `is_subsequence` and other display helpers, `sessions.rs` listing/diagnostics.

hrdr-app re-exports every moved item (`pub use hrdr_agent::…`) so downstream
`use`s keep compiling through the migration — the same trick the transcript
slices used. New hrdr-agent deps that follow `session.rs`: `zstd`, `filetime`
(`hjkl-xdg` is already there).

## Slices (one at a time: opus impl → clippy+test+fmt review → commit → push)

**Phase 1 — the divergence fix**

1. **session persistence → hrdr-agent.** Move `session.rs` (whole) +
   `save_session`/`SaveOutcome` (from `sessions.rs`) + `session_name_from` (from
   `util.rs`) into a hrdr-agent `session` module. Relocate the
   `session_diagnostics` _test_ to `sessions.rs` (app). Re-export from hrdr-app.
2. **panes → hrdr-agent.** Move `pane.rs` (`Pane`/`PaneSet`/`PaneView`).
   Re-export.
3. **sub-agent SessionState persistence.** In `delegation.rs`, capture each
   sub-agent's `History` events and persist its own `SessionState`
   (`subagents/<main-id>/<sub-id>.json`) with the same code the main agent uses
   — the actual parity fix. (Unblocks `task_revive` + disk-aware `task_list`
   later.)

**Phase 2 — finish the principle**

4. Sweep the rest: skills model, login auth/provider core, agent-side `util`,
   leaving hrdr-app as pure glue.

## No-migration note (pre-1.0)

Existing on-disk session files are unaffected — the types move crates but their
serde shape is unchanged, so files round-trip exactly as before.
