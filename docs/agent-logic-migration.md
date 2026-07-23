# Agent-logic migration: hrdr-agent owns all agent logic, hrdr-app is only glue

Status: **Phase 1 complete** (on main). **W2 complete** (main agent on the
event-fold jsonl). Phase 2 pending.

## Progress

- `3c49a2c` — slice 1: session persistence → hrdr-agent.
- `14b313b` — slice 2: Pane/PaneSet → hrdr-agent.
- `7a54f47` — slice 3: sub-agent persists its own SessionState
  (`Session::save_to_path`) on every round + at completion.
- `d8c2afc` — perf: session files written as compact JSON (not pretty).
- `51bf3eb` — perf: sub-agent snapshot stores messages + metadata only; its
  transcript lives in the sibling jsonl (rebuilt via `read_transcript` on load),
  so a round no longer re-serializes the whole transcript.
- **W2** — main agent onto the event-fold jsonl: `SessionState::transcript` no
  longer serialized (skip_serializing), a `session_transcript_path` append
  writer attached to `MAIN_KEY`, `load_path` rebuilds from the sibling jsonl,
  purge cleans it up. The O(n²) per-round full-rewrite is gone; main and sub
  agents now share one on-disk transcript format and one resume path. See the W2
  section.

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

## W2 — main agent onto the event-fold jsonl (the O(n²) fix + 1:1 finish) — DONE

The MAIN agent embedded `Vec<Entry>` in its `.json` and full-rewrote the whole
multi-MB session on every tool round (frontend `persist_mid_turn`, UI thread) —
O(n²). It now uses the sub-agent model: the transcript is appended to a sibling
jsonl per event, the `.json` carries only messages+metadata, and resume rebuilds
via `read_transcript`. The two parts as landed:

1. **Main user-input on the event stream — already in place.** By the time this
   slice landed, main user messages no longer entered via
   `push_entry(Entry::user)`: `spawn_turn` enqueues the submission onto
   `MAIN_KEY`'s steering queue, `run` drains it and emits `AgentEvent::Steered`,
   and the frontend's `apply_event` folds it into the transcript AND calls
   `record(MAIN_KEY, …)`. So the user turn is an event like any other — nothing
   to change here.
2. **Format flipped + rebuild on resume.**
   - `SessionState::transcript` is `#[serde(default, skip_serializing)]` — never
     written to the `.json` (still _deserialized_ as a fallback for an older
     file that embeds one).
   - `LiveSubagents::attach_transcript(MAIN_KEY, session_transcript_path(cwd,id))`
     opens `sessions/<cwd>/<id>.jsonl` in **append** mode (stable id across
     resumes, unlike the sub-agent's exclusive `create`); `record(MAIN_KEY, ev)`
     then appends every event. Attached from the TUI's `refresh_subagent_dir`
     (post-id-assignment), detached on `/new` and `adopt_state` so a session
     switch re-opens against the new file.
   - `Session::load_path` rebuilds `state.transcript` from the sibling jsonl via
     `read_transcript` — the single resume/open loader, so `list_sessions`
     (metadata-only) stays cheap. This also fires for a sub-agent snapshot load,
     so both agents rebuild identically.
   - A `!command` run as the very first action reserves the session id
     (attaching the writer) before its tool block opens, so it lands in the
     jsonl too.
   - Retention purge removes the sibling `<id>.jsonl` and `subagents/<id>/` with
     the `.json`, so a deleted session leaves no orphans.

**Known limitation (shared with sub-agents):** per-entry timestamps are NOT
preserved on rebuild. `AgentEvent`s carry no wall-clock time and streaming
deltas coalesce, so a rebuilt entry is stamped at fold time. Content
(user/reasoning/ assistant/tool with args+results) is what the fold preserves.
Making it faithful would mean stamping each jsonl record and applying it during
the fold — a Record format change affecting both agents; deferred.

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
