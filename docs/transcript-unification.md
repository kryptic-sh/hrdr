# Transcript unification: hrdr-agent owns recording, frontends only render

Status: **approved design, implementation pending.**

## The problem

There are **three** codepaths that turn an agent's `AgentEvent` stream into a
transcript, and they disagree on completeness:

1. **Main transcript** —
   `hrdr-app/src/pane.rs::apply_event(&mut Vec<Entry>, &AgentEvent)` folds the
   stream into rich `Entry`/`EntryKind` values (tool `args`, `result`, `ok`,
   diffs, reasoning). Persisted in the session as `Vec<Entry>`; rendered by the
   TUI and by `transcript_to_text`.
2. **On-disk sub-agent transcript** — `hrdr-agent/src/subagent_transcript.rs`
   plus `delegation.rs::subagent_event_for` project the SAME `AgentEvent` stream
   down to a lossy `Event` enum: `Tool { name }` only — **no args, no result**.
   This is what a delegated `task` writes to
   `sessions/<cwd>/subagents/<id>/<n>-<label>.jsonl`.
3. **Live sub-agent peek** — `delegation.rs::render_events_peek(&[AgentEvent])`
   renders events to text for `task_output`, again dropping args/results.

Because (2) is lossy, a completed sub-agent run leaves **no record of which
files or paths its tools touched** — so when a sub-agent misbehaves (e.g. edits
the parent working dir), there is nothing on disk to diagnose it from. Yet
hrdr-agent _already_ records the complete stream in memory: every agent's
`LiveSubagent.events` (an `EventLog` of raw `AgentEvent`s, args and results
intact) — the frontend replays it to build (1). The completeness exists; it just
isn't the thing persisted or shared.

## Root cause

The transcript **model** (`Entry`/`EntryKind`) and the **builder**
(`apply_event`) live in `hrdr-app` (the UI-agnostic app core), one layer above
`hrdr-agent`. Since core can't depend on the app layer, the sub-agent path
(which runs in `hrdr-agent`) couldn't reuse them and grew its own lossy copy.
Both the model and the builder are already frontend-agnostic — `transcript.rs`
has zero `ratatui` and `EntryKind::Tool` already carries `args`/`result`/`ok` —
so nothing about them actually needs to be in the app layer.

## Target architecture

**hrdr-agent owns the transcript: the model, the builder, and the text renderer.
Frontends only render it (the TUI to ratatui).**

- **Model** — move `Entry`, `EntryKind`, `Entry` constructors, `content_hash`,
  and the `/goto` resolver into `hrdr-agent` (e.g.
  `hrdr-agent/src/transcript.rs`).
- **Builder** — move `apply_event(&mut Vec<Entry>, &AgentEvent)` and its helpers
  (`open_tool`, `finish_reasoning`) into `hrdr-agent`. This becomes the ONE
  place an event stream becomes a transcript, for any agent.
- **Text renderer** — move `transcript_to_text(&[Entry])` into `hrdr-agent`;
  both the peek and any headless output use it.
- **Persistence** — one representation for both:
  - The **main session** keeps `Vec<Entry>` in its `SessionState` (now importing
    `Entry` from hrdr-agent).
  - The **sub-agent transcript** becomes an append-only JSONL of **raw
    `AgentEvent`s** (matching the in-memory `EventLog`), crash-safe as today. On
    read, the shared `apply_event` folds it into `Vec<Entry>` — identical to how
    the main transcript is built. This makes it complete and removes the lossy
    `Event` enum entirely.
  - `task_output`'s live peek builds `Vec<Entry>` from the same events and calls
    the shared `transcript_to_text` — deleting `render_events_peek`.

- **Frontends** — `hrdr-app` keeps only the pieces that consume the model for a
  frontend (pane/view state, `Entry` → ratatui rendering in `hrdr-tui`). They
  import `Entry`/`apply_event`/`transcript_to_text` from `hrdr-agent`.

After this, main and sub-agent transcripts are **one recording path and one
render path**, and the sub-agent's on-disk record is as complete as the main
agent's.

## Migration steps

1. Move `Entry`/`EntryKind` + constructors + `content_hash` +
   `transcript_to_text`
   - `/goto` resolver from `hrdr-app/src/transcript.rs` to
     `hrdr-agent/src/transcript.rs`; re-export from `hrdr-app` so downstream
     `use`s keep working during the move.
2. Move `apply_event` + helpers from `hrdr-app/src/pane.rs` into the new
   `hrdr-agent` transcript module. `Pane`/`PaneSet` (view state) stay in
   `hrdr-app`, now calling the moved builder.
3. Repoint `SessionState.transcript: Vec<Entry>` (in `hrdr-app/src/session.rs`)
   to the hrdr-agent `Entry`.
4. Replace `subagent_transcript.rs`'s lossy `Event` with an append-only log of
   raw `AgentEvent`s; on read, fold with `apply_event`. Update `delegation.rs`
   write sites (drop `subagent_event_for`) and the `task_output` peek (drop
   `render_events_peek`, use the shared builder + `transcript_to_text`).
5. Delete the now-unused lossy paths; keep the JSONL location/naming so existing
   `subagents/*.jsonl` files remain discoverable (pre-feature files are readable
   as before / tolerated as best-effort).
6. Verify: main TUI transcript unchanged (e2e), a delegated write task's on-disk
   transcript now shows tool args + results, `task_output` peek matches.

## Note (no migration, pre-1.0)

Existing lossy `subagents/*.jsonl` files won't retro-fill args/results — only
new runs are complete. Per `no-migration-pre-1.0`, that's acceptable; readers
tolerate the old shape or ignore it.
