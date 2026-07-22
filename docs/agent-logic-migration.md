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

**Remaining efficiency lever (not yet done):** the MAIN agent still
full-rewrites its whole session (multi-MB) on every tool round via the
frontend's `persist_mid_turn`, on the UI thread — O(n²) over a session. The 1:1
endgame is to give the main agent the same jsonl-transcript + compact
messages-snapshot model the sub-agent now uses (transcript appended per event,
`.json` snapshot carries only messages+metadata), which makes main == sub AND
turns the per-round cost from O(full doc) into O(one appended line). Larger
change (main session on-disk format) — tackle as its own slice.

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
