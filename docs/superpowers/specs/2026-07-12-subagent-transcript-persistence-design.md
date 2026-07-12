# Sub-agent transcript persistence — design

**Status:** approved for implementation planning **Scope:** WISHLIST #1
groundwork — the persistence _primitive only_. No recovery UI, no pruning, no
resume-into-sub-agent (all deferred). **Branch:** `feat/subagent-transcripts`
(worktree off `main` @ `9079f7f`).

## Problem

Sub-agents (`SubagentTool`, `crates/hrdr-agent/src/lib.rs`) spawn a bare `Agent`
with **no persistence of their own**. Their output only exists in memory:

- **Blocking** (`lib.rs:598` `execute`): text accumulates into a local `output`
  string and is live-emitted to the parent transcript entry. On error,
  `run.with_context()?` (`lib.rs:743`) returns the error — the partial `output`
  never reaches the tool result.
- **Background** (`lib.rs:229` `spawn_background`): text accumulates into
  `BackgroundTask.log` in an in-memory registry `Vec`. On failure the result is
  replaced with `"(background task failed: {e})"` (`lib.rs:302`) — the partial
  `out` is **discarded**.

Consequences:

1. No on-disk artifact per sub-agent → an app crash mid-run loses all sub-agent
   progress. The parent session JSON (`session.rs:287`) holds only the parent's
   own `Message` list; streamed sub-agent text is ephemeral emit, not a
   `Message`.
2. Failure paths drop partial work (background discards `out`; blocking returns
   the error, not the partial output).
3. API errors surface as an anyhow string, persisted nowhere structured.

## Goal

A durable, append-only, per-sub-agent transcript on disk that captures every
output chunk, tool start, and API error as it happens — so a sub-agent that dies
halfway leaves all completed work recoverable from its file, independent of the
parent transcript.

## Decisions (settled during brainstorming)

| Decision      | Choice                                                                     |
| ------------- | -------------------------------------------------------------------------- |
| Scope         | Persistence primitive only (recovery UI / pruning deferred)                |
| Format        | Append-only JSONL, one JSON object per line, flushed per event             |
| Location      | Nested: `sessions/<cwd-slug>/subagents/<parent-session-id>/<sub-id>.jsonl` |
| Retention     | Keep all (success + failure); pruning is a later item                      |
| `Start` event | Persists the **full** prompt (no truncation)                               |
| Wiring        | Option 2A — full thin slice: agent layer + app path helper + TUI cell      |

## Architecture

Three layers, clean separation — the agent layer never learns app-layer path
policy.

### Component 1 — `SubagentTranscript` writer (new, `hrdr-agent`)

New module `crates/hrdr-agent/src/subagent_transcript.rs`.

- Wraps an append-mode file: `OpenOptions::new().create(true).append(true)`.
- `Event` enum, serde-tagged `#[serde(tag = "t", rename_all = "snake_case")]`:

  ```rust
  enum Event {
      Start { model: String, label: String, kind: SpawnKind, prompt: String },
      Text  { chunk: String },
      Tool  { name: String },
      Error { msg: String },
      End   { status: EndStatus, bytes: usize },
  }
  enum SpawnKind { Blocking, Background }
  enum EndStatus { Ok, Failed, Panicked, Cancelled }
  ```

  (`kind`/`status` serialize snake_case: `blocking`, `background`, `ok`,
  `failed`, `panicked`, `cancelled`.)

- API:
  - `open(dir: &Path, id: &str) -> io::Result<SubagentTranscript>` — creates
    `dir` (`create_dir_all`) and opens `dir/<id>.jsonl` for append.
  - `write(&mut self, ev: &Event)` — serialize to a single line, write, `\n`,
    `flush()`. **Infallible from the caller's view: all I/O errors are
    swallowed** (best-effort logged). Persistence must never break the actual
    sub-agent run. No `fsync` per line — `flush()` to the OS is enough for the
    crash model we care about (process death, not power loss).
- Reader helper (minimal, for tests + the future recovery item):
  - `is_complete(path: &Path) -> bool` — true iff the file's last non-empty line
    parses as `Event::End`. A file with no `End` line = an orphan = a crashed or
    still-running sub-agent.

**Why this gives crash recovery for free:** every `Text` chunk is flushed as it
arrives, so a killed process leaves all completed work on disk even though the
in-memory tool result (`output` / `out`) is discarded on the error path.

### Component 2 — lazy dir resolution (the shared cell)

The parent **session id is assigned lazily** — on first autosave, not at
`Agent::new` (`session.rs:48/77`, `adopt_state:137`; `e2e.rs:1498` "autosave
assigned a session id"). So the dir cannot be a fixed value threaded at build
time; it must be resolved **at spawn**.

- Cell type: `Option<Arc<Mutex<Option<PathBuf>>>>` holding the
  **fully-resolved** dir `sessions/<slug>/subagents/<id>/`.
  - Outer `Option`: whole feature off (headless, tests that don't opt in).
  - Inner `Option<PathBuf>`: id not yet assigned (pre-first-save) → inert.
- Added to `AgentConfig` as
  `subagent_transcript_dir: Option<Arc<Mutex<Option<PathBuf>>>>`, default
  `None`.
- `subagent_base_config` (`lib.rs:365`) **clears it to `None`** — sub-agents
  don't spawn sub-agents (`subagents = false` already), so they never write
  transcripts of their own.
- `SubagentTool::new` takes the cell (cloned from the parent config) and reads
  it **at spawn time**, not at construction.
- The cell is resolved **parent-side**, so a worktree-isolated sub-agent (its
  own `cwd`) still files under the parent session — correct.

Sub-agent id: `format!("{seq:03}-{slug}")` where `slug` is the sanitized `label`
(reuse `sanitize_name`-style rules) and `seq` comes from a single shared
`AtomicU64` `SUBAGENT_SEQ` used by **both** blocking and background spawns, so
ids are ordered and unique within a session dir. `SUBAGENT_SEQ` is a **new,
separate** counter; the existing `BG_SEQ` (`lib.rs:241`) is left untouched — it
keeps its current role generating the `BackgroundTask.id` / `task#{id}` panel
id, which is a distinct concern from the transcript file id.

### Component 3 — app path helper (`hrdr-app`)

One function beside `session_dir` in `crates/hrdr-app/src/session.rs`:

```rust
/// `sessions/<cwd-slug>/subagents/<session-id>/` — the dir a session's
/// sub-agent transcripts live in.
pub fn subagent_transcript_dir(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join("subagents").join(sanitize_name(id))
}
```

App owns path policy; the agent layer only ever receives the finished `PathBuf`.

### Component 4 — TUI wiring (`hrdr-tui`)

- In `App::new` (`app.rs:365`, before `Agent::new(config)`): construct
  `let cell = Arc::new(Mutex::new(None));`, set
  `config.subagent_transcript_dir = Some(cell.clone());`, and keep `cell` on the
  `App`.
- At the **three** existing id-assignment sites — `persist_mid_turn`
  (`session.rs:48`), `autosave` (`session.rs:77`), `adopt_state`
  (`session.rs:137`) — after `self.state.id = …`, update the cell:
  `*cell.lock() = Some(hrdr_app::subagent_transcript_dir(&cwd, &id))`. A tiny
  shared helper on `App` (e.g. `fn refresh_subagent_dir(&self)`) keeps the three
  call sites one line each and DRY.

### Component 5 — wire both spawn paths (`hrdr-agent`)

In `SubagentTool::execute` (blocking) and `spawn_background`:

- Resolve the dir from the cell at spawn. If `None`/inner-`None`, run exactly as
  today (no transcript) — the feature is transparent when off.
- If a dir is present: `open` the transcript, write
  `Start { model, label, kind, prompt }`.
- In the run callback: `AgentEvent::Text(t)` → `Text { chunk }`;
  `AgentEvent::ToolStart { name, .. }` → `Tool { name }` — **alongside** the
  existing parent emit, not replacing it.
- On completion:
  - success → `End { status: Ok, bytes }`,
  - error → `Error { msg }` then `End { status: Failed, bytes }`,
  - background panic (outer guard, `lib.rs:306`) →
    `End { status: Panicked, .. }`,
  - background cancel → `End { status: Cancelled, .. }`.
- The transcript handle must be reachable in the background **outer** guard task
  so a panicked inner task still writes its terminal `End`. Move the writer into
  the outer task; the inner task borrows/receives it (or the outer writes the
  terminal event after `inner.await`, using the writer it retained).

## Data flow

```
task tool call
   └─ SubagentTool::execute
        ├─ read cell → dir?  (None → run as today)
        ├─ open dir/<seq-slug>.jsonl  → write Start{full prompt}
        ├─ Agent::run(callback):
        │     Text  → parent emit  +  write Text
        │     Tool  → parent emit  +  write Tool
        ├─ Ok  → write End{ok}
        └─ Err → write Error{msg} + End{failed}     (bg panic/cancel → End{panicked|cancelled})
```

## Error handling

- **Transcript I/O errors never propagate.** `open` failure → skip persistence
  for that sub-agent (run continues). `write` failure → swallowed. Rationale: a
  full disk or a permissions issue must not kill useful sub-agent work.
- **API / model errors** are first-class transcript content: captured as
  `Error { msg }` before the terminal `End`, so a failed delegation's cause is
  recoverable from disk even though the tool result only carries a summary.

## Known behaviors (accepted, documented)

1. **Pre-first-save gap.** A `task` spawned in a brand-new, never-saved session
   — before the first autosave assigns an id — sees the cell's inner `None` and
   is not persisted. This affects only the very first sub-agent of a fresh
   session. **Accepted**; closing it (eager id assignment) is more invasive and
   out of scope. Documented in the module doc.
2. **Resume / rename.** On resume the id changes → the cell repoints → new
   sub-agents write under the resumed session's dir. The old dir (old id) keeps
   its files; they carry `End` lines, so `is_complete` does not mistake them for
   crashes. `/rename` changes only the display name, not the file id
   (`session.rs`: "the file id isn't stored … it _is_ the file name"), so an
   in-progress session's dir is stable across a rename.

## Testing

- **Unit (`subagent_transcript.rs`):**
  - `Event` serde round-trips with the `t` tag; each variant’s discriminant
    matches the documented snake_case spelling.
  - `write` emits exactly one line per event, each a valid standalone JSON
    object; the file is append-only (a second writer session appends, doesn’t
    truncate).
  - Simulated mid-write drop: write `Start` + two `Text`, drop the writer
    without `End`; `is_complete` returns `false` and every `Text` chunk is
    present and parseable (crash recovery).
  - `open` failure (unwritable dir) is swallowed and does not panic.
- **Integration (mock/stub agent, `hrdr-agent`):**
  - Success run → `Start → Text* → Tool? → End{ok}`.
  - Failing run → `Start → … → Error{msg} → End{failed}` with partial `Text`
    present before the error.
  - Cell = `None` (and inner `None`) → no file created, run identical to today.
  - Both blocking and background paths covered; background panic →
    `End{panicked}`.
- **App/TUI:** `subagent_transcript_dir` builds the expected nested path;
  `refresh_subagent_dir` populates the cell after an id is assigned (assert the
  cell holds the resolved dir after a first autosave).

## Out of scope (later WISHLIST work)

- Recovery UI: surfacing / listing orphaned transcripts on next launch.
- Pruning (age/count-based) of kept transcripts.
- Resume-into-sub-agent / replaying a transcript.
- Elapsed-time display (separate WISHLIST item).

## Affected files

- `crates/hrdr-agent/src/subagent_transcript.rs` — **new** (writer, `Event`,
  reader helper, unit tests).
- `crates/hrdr-agent/src/lib.rs` — `AgentConfig` field; `subagent_base_config`
  clears it; `SubagentTool::new` arg; both spawn paths; shared `SUBAGENT_SEQ`;
  module decl.
- `crates/hrdr-app/src/session.rs` — `subagent_transcript_dir` helper.
- `crates/hrdr-tui/src/app.rs` — build cell, set on config, hold on `App`.
- `crates/hrdr-tui/src/app/session.rs` — `refresh_subagent_dir`, call at the 3
  id-assignment sites.

## Verification

Per the workspace Rust rule: `cargo fmt`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` all
green. Then a live `verify`: run a real delegated `task`, confirm a `.jsonl`
appears under the session's `subagents/<id>/` dir with a `Start` line + terminal
`End`, and that killing a sub-agent mid-run leaves an orphan file with its
partial text intact.
