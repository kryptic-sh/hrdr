# Sub-agent Transcript Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every delegated `task` sub-agent its own durable, append-only
JSONL transcript on disk so a sub-agent that dies mid-run leaves all completed
work + its failure cause recoverable, independent of the parent session.

**Architecture:** A pure `SubagentTranscript` writer (append-only JSONL, one
event per flushed line) in `hrdr-agent`. A lazily-resolved shared cell
(`Arc<Mutex<Option<PathBuf>>>`) carries the parent session's transcript dir —
the app populates it when a session id is assigned; both `task` spawn paths read
it at spawn. The app owns all path policy; the agent layer only ever receives a
finished `PathBuf`.

**Tech Stack:** Rust (workspace), serde / serde_json (JSONL), tokio (async spawn
paths), tempfile (test I/O).

## Global Constraints

- **Design source:**
  `docs/superpowers/specs/2026-07-12-subagent-transcript-persistence-design.md`.
  On any ambiguity, that spec wins.
- **Scope:** persistence primitive only. NO recovery UI, NO pruning, NO
  resume-into-sub-agent, NO elapsed-time display.
- **CI gate (every commit):** `cargo fmt`,
  `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`
  must all pass. Clippy denies warnings — no dead/unused code may land.
- **No AI attribution** anywhere in commits/code/docs (per repo policy).
- **Persistence is best-effort:** a transcript I/O error MUST NEVER break or
  fail a sub-agent run. All write errors are swallowed.
- **Format:** append-only JSONL, one JSON object per line, flushed per event,
  `#[serde(tag = "t", rename_all = "snake_case")]`.
- **Layout:** `sessions/<cwd-slug>/subagents/<session-id>/<NNN-slug>.jsonl`.
- **Retention:** keep all files (success + failure); no deletion.
- **Full prompt** persisted in the `Start` event (no truncation).
- Run all commands from the worktree root
  `/home/shinobu/Projects/hrdr/.claude/worktrees/feat-subagent-transcripts`.
- **Testability note:** there is no mock-LLM harness in `hrdr-agent`. Tasks 1,
  2, 5 carry full automated tests. Tasks 3, 4, 6 are thin glue over
  already-tested helpers; their deliverable is build + clippy + existing tests
  green, and their runtime behavior is verified live in Task 7. This is
  intentional, not a coverage gap to paper over with fake tests.

---

### Task 1: `SubagentTranscript` writer + reader module

Self-contained new module in `hrdr-agent`. Pure I/O, no dependency on any other
task. This is the persistence primitive.

**Files:**

- Create: `crates/hrdr-agent/src/subagent_transcript.rs`
- Modify: `crates/hrdr-agent/src/lib.rs` (add `mod subagent_transcript;` near
  the other top-level `mod` declarations)

**Interfaces:**

- Produces (used by Tasks 3, 4):
  - `pub enum SpawnKind { Blocking, Background }` (serde snake_case)
  - `pub enum EndStatus { Ok, Failed, Panicked, Cancelled }` (serde snake_case)
  - `pub enum Event { Start{model:String,label:String,kind:SpawnKind,prompt:String}, Text{chunk:String}, Tool{name:String}, Error{msg:String}, End{status:EndStatus,bytes:usize} }`
    (serde `tag = "t"`, snake_case)
  - `pub struct SubagentTranscript` with
    `pub fn open(dir: &Path, id: &str) -> std::io::Result<SubagentTranscript>`
    and `pub fn write(&mut self, ev: &Event)` (infallible; errors swallowed)
  - `pub fn is_complete(path: &Path) -> bool`

- [ ] **Step 1: Write the module with its unit tests**

Create `crates/hrdr-agent/src/subagent_transcript.rs`:

```rust
//! Per-sub-agent transcript: an append-only JSONL log of one delegated `task`
//! run, written independently of the parent session so a sub-agent that dies
//! mid-run leaves all completed work recoverable on disk.
//!
//! Persistence is best-effort: every write error is swallowed, because writing
//! a transcript must never break the actual sub-agent run. A brand-new,
//! never-saved session has no id yet, so the very first sub-agent spawned
//! before the first autosave is not persisted (the dir cell is still empty).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How a sub-agent was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    Blocking,
    Background,
}

/// Terminal status of a sub-agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndStatus {
    Ok,
    Failed,
    Panicked,
    Cancelled,
}

/// One line in a sub-agent transcript. Serialized with a `t` discriminator so a
/// reader can dispatch on the event kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    Start {
        model: String,
        label: String,
        kind: SpawnKind,
        prompt: String,
    },
    Text {
        chunk: String,
    },
    Tool {
        name: String,
    },
    Error {
        msg: String,
    },
    End {
        status: EndStatus,
        bytes: usize,
    },
}

/// An open append-only transcript file for one sub-agent run.
pub struct SubagentTranscript {
    file: File,
}

impl SubagentTranscript {
    /// Open `dir/<id>.jsonl` for append, creating `dir` if needed.
    pub fn open(dir: &Path, id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{id}.jsonl")))?;
        Ok(Self { file })
    }

    /// Append one event as a JSON line and flush. All errors are swallowed: a
    /// failed transcript write must never break the sub-agent run.
    pub fn write(&mut self, ev: &Event) {
        if let Ok(mut line) = serde_json::to_string(ev) {
            line.push('\n');
            let _ = self.file.write_all(line.as_bytes());
            let _ = self.file.flush();
        }
    }
}

/// Whether a transcript file ends in an `End` event. A file with no `End` line
/// is an orphan: the sub-agent crashed or is still running.
pub fn is_complete(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            last = Some(line);
        }
    }
    match last {
        Some(l) => matches!(serde_json::from_str::<Event>(&l), Ok(Event::End { .. })),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_with_t_tag_and_snake_case() {
        let start = Event::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Background,
            prompt: "p".into(),
        };
        let s = serde_json::to_string(&start).unwrap();
        assert!(s.contains(r#""t":"start""#), "got {s}");
        assert!(s.contains(r#""kind":"background""#), "got {s}");
        // Round-trips.
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), start);

        let end = Event::End {
            status: EndStatus::Panicked,
            bytes: 3,
        };
        let s = serde_json::to_string(&end).unwrap();
        assert!(s.contains(r#""t":"end""#) && s.contains(r#""status":"panicked""#), "got {s}");
    }

    #[test]
    fn write_appends_one_line_per_event_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut t = SubagentTranscript::open(dir.path(), "001-x").unwrap();
            t.write(&Event::Start {
                model: "m".into(),
                label: "l".into(),
                kind: SpawnKind::Blocking,
                prompt: "p".into(),
            });
            t.write(&Event::Text { chunk: "hello".into() });
        }
        // Re-opening appends rather than truncating.
        {
            let mut t = SubagentTranscript::open(dir.path(), "001-x").unwrap();
            t.write(&Event::End {
                status: EndStatus::Ok,
                bytes: 5,
            });
        }
        let body = std::fs::read_to_string(dir.path().join("001-x.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event: {body:?}");
        for l in &lines {
            serde_json::from_str::<Event>(l).expect("each line is a standalone Event");
        }
    }

    #[test]
    fn is_complete_flags_orphan_and_preserves_partial_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("002-x.jsonl");
        {
            let mut t = SubagentTranscript::open(dir.path(), "002-x").unwrap();
            t.write(&Event::Start {
                model: "m".into(),
                label: "l".into(),
                kind: SpawnKind::Blocking,
                prompt: "p".into(),
            });
            t.write(&Event::Text { chunk: "done work".into() });
            // Drop without an End event: simulates a crash mid-run.
        }
        assert!(!is_complete(&path), "no End line => orphan");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("done work"), "partial work survives the crash");

        // Now finish it.
        {
            let mut t = SubagentTranscript::open(dir.path(), "002-x").unwrap();
            t.write(&Event::End {
                status: EndStatus::Failed,
                bytes: 9,
            });
        }
        assert!(is_complete(&path), "End line => complete");
    }

    #[test]
    fn is_complete_is_false_for_missing_file() {
        assert!(!is_complete(Path::new("/nonexistent/does/not/exist.jsonl")));
    }

    #[test]
    fn open_error_is_returned_not_panicked() {
        // A path whose parent cannot be created (a file where a dir is needed).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dir = blocker.join("subdir"); // parent is a file
        assert!(SubagentTranscript::open(&bad_dir, "id").is_err());
    }
}
```

Add the module declaration to `crates/hrdr-agent/src/lib.rs` alongside the other
top-level `mod` lines (e.g. near `mod paths;`):

```rust
mod subagent_transcript;
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --locked -p hrdr-agent subagent_transcript -- --nocapture`
Expected: all 5 tests PASS.

- [ ] **Step 3: Lint + format**

Run:
`cargo fmt && cargo clippy --all-targets --locked -p hrdr-agent -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 4: Commit**

```bash
git add crates/hrdr-agent/src/subagent_transcript.rs crates/hrdr-agent/src/lib.rs
git commit -m "feat(agent): add append-only sub-agent transcript writer"
```

---

### Task 2: Config field + spawn-path helpers (pure, unit-tested)

Adds the shared-cell config field and the four pure helpers the spawn paths will
call. Keeping the logic here (not inline in the async paths) is what makes it
testable without a mock LLM.

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs`

**Interfaces:**

- Consumes (from Task 1): `subagent_transcript::{Event, SpawnKind}`
- Produces (used by Tasks 3, 4):
  - `AgentConfig` field
    `pub subagent_transcript_dir: Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>`
  - `type SubagentDirCell = Option<Arc<Mutex<Option<PathBuf>>>>` (module-local
    alias)
  - `static SUBAGENT_SEQ: AtomicU64`
  - `fn subagent_transcript_id(seq: u64, label: &str) -> String`
  - `fn resolve_subagent_dir(cell: &SubagentDirCell) -> Option<PathBuf>`
  - `fn subagent_event_for(ev: &AgentEvent) -> Option<subagent_transcript::Event>`
  - `subagent_base_config` now clears `subagent_transcript_dir` to `None`

- [ ] **Step 1: Add the `AgentConfig` field**

In `crates/hrdr-agent/src/lib.rs`, add to the `AgentConfig` struct (after the
`write_ext` field, the current last field before the closing `}` around
`lib.rs:930`):

```rust
    /// Shared cell holding the parent session's sub-agent transcript directory
    /// (`sessions/<slug>/subagents/<id>/`), resolved lazily because the session
    /// id is assigned on first autosave, not at construction. The `task` tool
    /// reads it at spawn: `None` (outer) = feature off; `Some` with an inner
    /// `None` = id not yet assigned (pre-first-save) so that spawn is not
    /// persisted. Cleared for sub-agent base configs (subs don't spawn subs).
    pub subagent_transcript_dir:
        Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>,
```

In `impl Default for AgentConfig` (`lib.rs:1252`), add to the returned struct
(anywhere in the field list, e.g. after `write_ext: None,`):

```rust
            subagent_transcript_dir: None,
```

- [ ] **Step 2: Clear the field in `subagent_base_config`**

In `subagent_base_config` (`lib.rs:365`), after the existing mutations of `base`
(e.g. after `base.subagents = false;`), add:

```rust
    // Sub-agents never spawn sub-agents, so they never write transcripts.
    base.subagent_transcript_dir = None;
```

- [ ] **Step 3: Add the helpers + counter**

Add near the top-level helpers of `crates/hrdr-agent/src/lib.rs` (e.g. just
after `spawn_background` ends, around `lib.rs:335`):

```rust
/// The shared, lazily-resolved sub-agent transcript directory cell (see
/// [`AgentConfig::subagent_transcript_dir`]).
type SubagentDirCell =
    Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>;

/// Monotonic counter for sub-agent transcript file ids, shared by the blocking
/// and background spawn paths so ids are ordered and unique within a session
/// dir. Separate from `BG_SEQ`, which numbers background-task registry entries.
static SUBAGENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A transcript file id: `NNN-<slug>`, where `slug` is the sanitized label.
/// `seq` is the pre-fetched counter value.
fn subagent_transcript_id(seq: u64, label: &str) -> String {
    let lowered: String = label
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = lowered
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = if slug.is_empty() {
        "task".to_string()
    } else {
        slug.chars().take(32).collect()
    };
    format!("{seq:03}-{slug}")
}

/// Read the resolved transcript dir from the shared cell, if the feature is on
/// and a session id has been assigned.
fn resolve_subagent_dir(cell: &SubagentDirCell) -> Option<std::path::PathBuf> {
    cell.as_ref()?.lock().ok()?.clone()
}

/// Map a live agent event to the transcript event to record, if any.
fn subagent_event_for(ev: &AgentEvent) -> Option<subagent_transcript::Event> {
    use subagent_transcript::Event;
    match ev {
        AgentEvent::Text(t) => Some(Event::Text { chunk: t.clone() }),
        AgentEvent::ToolStart { name, .. } => Some(Event::Tool { name: name.clone() }),
        _ => None,
    }
}
```

> NOTE: if `AgentEvent::Text` / `AgentEvent::ToolStart` shapes differ from
> `Text(String)` / `ToolStart { name, .. }`, match the actual variants — grep
> `enum AgentEvent` in `lib.rs`. As of `9079f7f` they match the code in
> `SubagentTool::execute` (`lib.rs:730,734`).

- [ ] **Step 4: Add unit tests**

Add to the existing `#[cfg(test)] mod tests` in `lib.rs` (or a new one if the
helpers are outside it):

```rust
#[test]
fn subagent_transcript_id_slugifies_and_pads() {
    assert_eq!(subagent_transcript_id(0, "Explore the repo"), "000-explore-the-repo");
    assert_eq!(subagent_transcript_id(12, "  "), "012-task");
    assert_eq!(subagent_transcript_id(7, "!!!"), "007-task");
    let long = subagent_transcript_id(3, &"a".repeat(80));
    assert_eq!(long, format!("003-{}", "a".repeat(32)));
}

#[test]
fn resolve_subagent_dir_reads_the_cell() {
    use std::sync::{Arc, Mutex};
    use std::path::PathBuf;
    assert_eq!(resolve_subagent_dir(&None), None);
    let empty: SubagentDirCell = Some(Arc::new(Mutex::new(None)));
    assert_eq!(resolve_subagent_dir(&empty), None);
    let full: SubagentDirCell = Some(Arc::new(Mutex::new(Some(PathBuf::from("/x/y")))));
    assert_eq!(resolve_subagent_dir(&full), Some(PathBuf::from("/x/y")));
}

#[test]
fn subagent_base_config_clears_the_transcript_cell() {
    use std::sync::{Arc, Mutex};
    let mut cfg = AgentConfig::default();
    cfg.subagent_transcript_dir = Some(Arc::new(Mutex::new(Some("/x".into()))));
    let base = subagent_base_config(&cfg);
    assert!(base.subagent_transcript_dir.is_none());
}

#[test]
fn subagent_event_for_maps_text_and_tool_only() {
    use subagent_transcript::Event;
    assert_eq!(
        subagent_event_for(&AgentEvent::Text("hi".into())),
        Some(Event::Text { chunk: "hi".into() })
    );
    assert_eq!(
        subagent_event_for(&AgentEvent::ToolStart {
            id: "x".into(),
            name: "bash".into(),
            args: "{}".into(),
        }),
        Some(Event::Tool { name: "bash".into() })
    );
    // Reasoning / output / usage events are not recorded.
    assert_eq!(subagent_event_for(&AgentEvent::Reasoning("hmm".into())), None);
}
```

(`AgentEvent` variants as of `9079f7f`: `Reasoning(String)`, `Text(String)`,
`ToolStart { id, name, args }`, `ToolOutput { id, chunk }`,
`ToolEnd { id, name, result, ok }`, `Usage { .. }`.)

- [ ] **Step 5: Run tests + lint**

Run: `cargo test --locked -p hrdr-agent subagent -- --nocapture` Expected: Task
1 tests + the 4 new tests PASS. Run:
`cargo clippy --all-targets --locked -p hrdr-agent -- -D warnings` Expected:
clean. (The new `pub` field and helpers are all referenced by tests, so no
dead-code warning.)

- [ ] **Step 6: Commit**

```bash
git add crates/hrdr-agent/src/lib.rs
git commit -m "feat(agent): add transcript dir cell + spawn-path helpers"
```

---

### Task 3: Wire the blocking spawn path

Wire `SubagentTool` to open a transcript and record events on the **blocking**
(`background: false` / worktree) path. Thin glue over Task 1 + Task 2 helpers.

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs` (`SubagentTool` struct ~`lib.rs:451`,
  `SubagentTool::new` ~`lib.rs:476`, its registration call site `lib.rs:2521`,
  and `execute`'s blocking branch `lib.rs:716`–`762`)

**Interfaces:**

- Consumes:
  `subagent_transcript::{SubagentTranscript, Event, SpawnKind, EndStatus}`,
  `resolve_subagent_dir`, `subagent_transcript_id`, `subagent_event_for`,
  `SUBAGENT_SEQ`, `SubagentDirCell`
- Produces (used by Task 4): `SubagentTool` field
  `transcript_dir: SubagentDirCell` (the background branch reads it)

- [ ] **Step 1: Add the field to `SubagentTool` + its constructor**

In the `SubagentTool` struct (`lib.rs:451`), add after `lsp`:

```rust
    /// The parent session's transcript dir cell (see
    /// [`AgentConfig::subagent_transcript_dir`]); read at spawn.
    transcript_dir: SubagentDirCell,
```

In `SubagentTool::new` (`lib.rs:476`), add a parameter and thread it into the
returned struct:

```rust
    fn new(
        base: AgentConfig,
        profiles: Vec<SubagentProfile>,
        bg_handles: BgHandles,
        cost_total: Arc<std::sync::Mutex<f64>>,
        lsp: Option<Arc<hrdr_tools::LspRegistry>>,
        transcript_dir: SubagentDirCell,
    ) -> Self {
```

and in the final `Self { … }` add `transcript_dir,`.

- [ ] **Step 2: Pass the parent cell at the registration site**

At `lib.rs:2521`, the tool is registered with `subagent_base_config(&config)` as
`base`. The transcript cell must come from the **parent** `config` (not the
cleared base). Update the call:

```rust
            tools.register(Arc::new(SubagentTool::new(
                subagent_base_config(&config),
                profiles,
                Arc::clone(&bg_handles),
                Arc::clone(&cost_total),
                lsp.clone(),
                config.subagent_transcript_dir.clone(),
            )));
```

- [ ] **Step 3: Open the transcript + write `Start` in the blocking branch**

In `execute`, immediately after the line
`ctx.emit(format!("↳ task ({model}): {label}\n"));` (`lib.rs:717`) and before
`let mut sub = Agent::new(cfg)…`, insert:

```rust
        let mut transcript = resolve_subagent_dir(&self.transcript_dir).and_then(|dir| {
            let seq = SUBAGENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = subagent_transcript_id(seq, &label);
            subagent_transcript::SubagentTranscript::open(&dir, &id).ok()
        });
        if let Some(t) = transcript.as_mut() {
            t.write(&subagent_transcript::Event::Start {
                model: model.clone(),
                label: label.clone(),
                kind: subagent_transcript::SpawnKind::Blocking,
                prompt: prompt.clone(),
            });
        }
```

(`prompt.clone()` here because `prompt` is moved into `sub.run(prompt, …)`
below.)

- [ ] **Step 4: Record stream events in the run callback**

Replace the existing callback body (`lib.rs:727`–`736`) so it also writes to the
transcript:

```rust
        let run = sub
            .run(prompt, steering, |ev| {
                if let Some(t) = transcript.as_mut()
                    && let Some(tev) = subagent_event_for(&ev)
                {
                    t.write(&tev);
                }
                match ev {
                    AgentEvent::Text(t) => {
                        output.push_str(&t);
                        ctx.emit(t);
                    }
                    AgentEvent::ToolStart { name, .. } => ctx.emit(format!("\n· {name}")),
                    _ => {}
                }
            })
            .await;
```

- [ ] **Step 5: Write the terminal event after the run**

After the worktree teardown block and BEFORE `run.with_context(|| …)?;`
(`lib.rs:743`), inspect `&run` and record the outcome (do not consume `run` —
the `?` below still needs it):

```rust
        if let Some(t) = transcript.as_mut() {
            match &run {
                Ok(()) => t.write(&subagent_transcript::Event::End {
                    status: subagent_transcript::EndStatus::Ok,
                    bytes: output.len(),
                }),
                Err(e) => {
                    t.write(&subagent_transcript::Event::Error {
                        msg: format!("{e:#}"),
                    });
                    t.write(&subagent_transcript::Event::End {
                        status: subagent_transcript::EndStatus::Failed,
                        bytes: output.len(),
                    });
                }
            }
        }
```

(`output` at this point is the full accumulated string; the
`let mut output = output.trim()…` rebinding happens after the `?`, so
`output.len()` here is the untrimmed byte count — acceptable for a size hint.)

- [ ] **Step 6: Build, lint, test**

Run: `cargo build --locked -p hrdr-agent` Expected: compiles. (If the compiler
flags an exhaustive `AgentConfig { … }` literal missing the new field, none is
expected — all known literals use `..Default::default()`; add the field there if
it appears.) Run:
`cargo clippy --all-targets --locked -p hrdr-agent -- -D warnings` Expected:
clean. Run: `cargo test --locked -p hrdr-agent` Expected: all existing + Task
1/2 tests PASS.

> No new automated test: exercising `execute` needs a live model. Behavior is
> verified live in Task 7. The helpers it calls are already tested in Task 2.

- [ ] **Step 7: Commit**

```bash
git add crates/hrdr-agent/src/lib.rs
git commit -m "feat(agent): record blocking sub-agent runs to a transcript"
```

---

### Task 4: Wire the background spawn path

Record transcript events on the **background** (`background: true`) path,
including the panic/cancel terminal statuses the outer guard task owns.

**Files:**

- Modify: `crates/hrdr-agent/src/lib.rs` (`spawn_background` `lib.rs:229`–`334`,
  and its call site in `execute` `lib.rs:688`)

**Interfaces:**

- Consumes: `SubagentTool.transcript_dir` (Task 3), all Task 1/2 helpers
- Produces: none (terminal task)

- [ ] **Step 1: Add a cell parameter to `spawn_background`**

Change the signature (`lib.rs:229`) to accept the parent cell:

```rust
fn spawn_background(
    cfg: AgentConfig,
    prompt: String,
    label: String,
    tool_id: Option<String>,
    slot: SubagentSlot,
    registry: &Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>>,
    handles: &BgHandles,
    cost_total: Arc<std::sync::Mutex<f64>>,
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    transcript_dir: SubagentDirCell,
) -> String {
```

At the call site in `execute` (`lib.rs:688`), pass `self.transcript_dir.clone()`
as the final argument.

- [ ] **Step 2: Open the transcript + write `Start` before spawning**

In `spawn_background`, after the registry `push` block (`lib.rs:253`) and before
`let reg = registry.clone();`, add a shared transcript handle:

```rust
    // Shared so the inner task records events and the outer guard can still
    // write a terminal `End` if the inner task panics.
    let transcript = std::sync::Arc::new(std::sync::Mutex::new(
        resolve_subagent_dir(&transcript_dir).and_then(|dir| {
            let seq = SUBAGENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let sid = subagent_transcript_id(seq, &label);
            subagent_transcript::SubagentTranscript::open(&dir, &sid).ok()
        }),
    ));
    if let Ok(mut g) = transcript.lock()
        && let Some(t) = g.as_mut()
    {
        t.write(&subagent_transcript::Event::Start {
            model: cfg.model.clone(),
            label: label.clone(),
            kind: subagent_transcript::SpawnKind::Background,
            prompt: prompt.clone(),
        });
    }
    let ts_inner = transcript.clone();
    let ts_outer = transcript.clone();
```

(`prompt.clone()` / `label.clone()` because both are moved into the task below;
`cfg.model.clone()` because `cfg` is moved.)

- [ ] **Step 3: Record stream events + Ok/Failed end in the inner task**

In the inner task's callback (`lib.rs:273`–`288`), after the existing registry
`t.log.push_str(&c)` handling, also write to the transcript. Update the callback
to record every event and keep the existing registry-log behavior:

```rust
                sub.run(prompt, steering_queue(), |ev| {
                    if let Ok(mut g) = ts_inner.lock()
                        && let Some(t) = g.as_mut()
                        && let Some(tev) = subagent_event_for(&ev)
                    {
                        t.write(&tev);
                    }
                    let chunk = match ev {
                        AgentEvent::Text(t) => {
                            out.push_str(&t);
                            Some(t)
                        }
                        AgentEvent::ToolStart { name, .. } => Some(format!("\n· {name}")),
                        _ => None,
                    };
                    if let Some(c) = chunk
                        && let Ok(mut v) = reg.lock()
                        && let Some(t) = v.iter_mut().find(|t| t.id == id)
                    {
                        t.log.push_str(&c);
                    }
                })
                .await?;
```

Then, where the inner task turns `result` into its return string (`lib.rs:293`–
`303`), write the Ok/Failed terminal event before returning:

```rust
            match result {
                Ok(()) => {
                    let o = out.trim().to_string();
                    if let Ok(mut g) = ts_inner.lock()
                        && let Some(t) = g.as_mut()
                    {
                        t.write(&subagent_transcript::Event::End {
                            status: subagent_transcript::EndStatus::Ok,
                            bytes: o.len(),
                        });
                    }
                    if o.is_empty() {
                        "(no text output)".to_string()
                    } else {
                        o
                    }
                }
                Err(e) => {
                    if let Ok(mut g) = ts_inner.lock()
                        && let Some(t) = g.as_mut()
                    {
                        t.write(&subagent_transcript::Event::Error {
                            msg: format!("{e:#}"),
                        });
                        t.write(&subagent_transcript::Event::End {
                            status: subagent_transcript::EndStatus::Failed,
                            bytes: out.len(),
                        });
                    }
                    format!("(background task failed: {e})")
                }
            }
```

> `ts_inner` must be moved into the inner `tokio::spawn` closure. The inner task
> is `async move`, so `ts_inner` is captured by move — ensure it is only used
> inside that task.

- [ ] **Step 4: Write panic/cancel end in the outer guard**

In the outer task, where the inner `JoinHandle` is awaited (`lib.rs:306`–`312`),
record the terminal event the inner task could not (it panicked or was
cancelled, so it never wrote its own `End`):

```rust
        let final_result = match inner.await {
            Ok(s) => s,
            Err(join_err) if join_err.is_panic() => {
                if let Ok(mut g) = ts_outer.lock()
                    && let Some(t) = g.as_mut()
                {
                    t.write(&subagent_transcript::Event::End {
                        status: subagent_transcript::EndStatus::Panicked,
                        bytes: 0,
                    });
                }
                format!("(background task panicked: {join_err})")
            }
            Err(_) => {
                if let Ok(mut g) = ts_outer.lock()
                    && let Some(t) = g.as_mut()
                {
                    t.write(&subagent_transcript::Event::End {
                        status: subagent_transcript::EndStatus::Cancelled,
                        bytes: 0,
                    });
                }
                "(background task was cancelled)".to_string()
            }
        };
```

- [ ] **Step 5: Build, lint, test**

Run: `cargo build --locked -p hrdr-agent` Expected: compiles. Watch for
`ts_inner`/`ts_outer` move errors — `ts_inner` belongs only to the inner spawn,
`ts_outer` only to the outer. Run:
`cargo clippy --all-targets --locked -p hrdr-agent -- -D warnings` Expected:
clean. Run: `cargo test --locked -p hrdr-agent` Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/hrdr-agent/src/lib.rs
git commit -m "feat(agent): record background sub-agent runs to a transcript"
```

---

### Task 5: App path helper

The app owns path policy. One pure function, fully unit-testable.

**Files:**

- Modify: `crates/hrdr-app/src/session.rs` (add beside `session_dir`
  `session.rs:258`)

**Interfaces:**

- Consumes: existing `session_dir`, `sanitize_name` (both in `session.rs`)
- Produces (used by Task 6):
  `pub fn subagent_transcript_dir(cwd: &str, id: &str) -> PathBuf`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/hrdr-app/src/session.rs`:

```rust
#[test]
fn subagent_transcript_dir_nests_under_session() {
    let dir = subagent_transcript_dir("/home/me/proj", "My Session");
    // sessions/<cwd-slug>/subagents/<sanitized-id>
    assert!(dir.ends_with("subagents/my-session"), "got {dir:?}");
    assert!(
        dir.to_string_lossy().contains("home-me-proj"),
        "keyed by cwd slug: {dir:?}"
    );
    // Shares the session's per-cwd directory.
    assert!(dir.starts_with(session_dir("/home/me/proj")));
}
```

- [ ] **Step 2: Run it, verify it fails**

Run: `cargo test --locked -p hrdr-app subagent_transcript_dir -- --nocapture`
Expected: FAIL — `cannot find function subagent_transcript_dir`.

- [ ] **Step 3: Implement the helper**

Add after `session_dir` (`session.rs:260`):

```rust
/// `sessions/<cwd-slug>/subagents/<session-id>/` — where a session's sub-agent
/// transcripts live (one `.jsonl` per delegated `task`).
pub fn subagent_transcript_dir(cwd: &str, id: &str) -> PathBuf {
    session_dir(cwd).join("subagents").join(sanitize_name(id))
}
```

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test --locked -p hrdr-app subagent_transcript_dir -- --nocapture`
Expected: PASS. Run:
`cargo clippy --all-targets --locked -p hrdr-app -- -D warnings` Expected:
clean.

- [ ] **Step 5: Commit**

```bash
git add crates/hrdr-app/src/session.rs
git commit -m "feat(app): add subagent_transcript_dir path helper"
```

---

### Task 6: TUI wiring — populate the cell

Build the shared cell, hand it to the agent config, and refresh it whenever the
session id is (re)assigned.

**Files:**

- Modify: `crates/hrdr-tui/src/app.rs` (`App` struct + `App::new`
  `app.rs:333`–`365`)
- Modify: `crates/hrdr-tui/src/app/session.rs` (id-assignment sites
  `session.rs:48,77,137`)

**Interfaces:**

- Consumes: `hrdr_app::subagent_transcript_dir` (Task 5),
  `AgentConfig::subagent_transcript_dir` (Task 2)
- Produces: none (terminal)

- [ ] **Step 1: Add the cell field to `App`**

In the `App` struct (`crates/hrdr-tui/src/app.rs`, near the other session fields
around `app.rs:47`), add:

```rust
    /// Shared cell for the sub-agent transcript dir, handed to the agent config
    /// and refreshed whenever the session id is assigned.
    subagent_dir: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
```

- [ ] **Step 2: Build the cell and set it on the config in `App::new`**

In `App::new`, before `let agent = Agent::new(config)?;` (`app.rs:365`):

```rust
        let subagent_dir = std::sync::Arc::new(std::sync::Mutex::new(None));
        config.subagent_transcript_dir = Some(subagent_dir.clone());
```

(`config` must be `mut`. It is consumed by `Agent::new(config)` on the next
line; the `mut` binding is already the parameter — add `mut` to the parameter
`config: AgentConfig` in the `App::new` signature if not already present.)

Add `subagent_dir` to the `Self { … }` construction of `App`.

- [ ] **Step 3: Add a refresh helper + call it at the three id sites**

In `crates/hrdr-tui/src/app/session.rs`, add a helper method on `App`:

```rust
    /// Point the shared sub-agent transcript cell at the current session's dir.
    /// Called after the session id is assigned; sub-agents spawned before this
    /// (a brand-new session's first turn) are simply not persisted.
    pub(super) fn refresh_subagent_dir(&self) {
        if let Some(id) = &self.state.id {
            let dir = hrdr_app::subagent_transcript_dir(&self.current_cwd(), id);
            if let Ok(mut cell) = self.subagent_dir.lock() {
                *cell = Some(dir);
            }
        }
    }
```

Call `self.refresh_subagent_dir();` immediately after each of the three
`self.state.id = …` assignments:

- `persist_mid_turn` after `self.state.id = Some(o.id);` (`session.rs:48`)
- `autosave` after `self.state.id = Some(o.id);` (`session.rs:77`)
- `adopt_state` after `self.state.id = id;` (`session.rs:137`)

> `current_cwd()` already exists on `App` (used in `persist_mid_turn`/
> `adopt_state`). Use it so the slug matches where the session file is written.

- [ ] **Step 4: Build, lint, test**

Run: `cargo build --locked -p hrdr-tui` Expected: compiles. Run:
`cargo clippy --all-targets --locked -p hrdr-tui -- -D warnings` Expected:
clean. Run: `cargo test --locked -p hrdr-tui` Expected: all existing e2e/tests
PASS (no behavior change to them).

> No new automated test here: the path computation is covered by Task 5, and the
> store is a one-line assignment. End-to-end population is verified in Task 7.

- [ ] **Step 5: Commit**

```bash
git add crates/hrdr-tui/src/app.rs crates/hrdr-tui/src/app/session.rs
git commit -m "feat(tui): populate sub-agent transcript dir on session id assignment"
```

---

### Task 7: Full verification + live drive

Whole-workspace gate plus a live run proving real transcripts appear — the
behavior Tasks 3/4/6 could not unit-test.

**Files:** none (verification only)

- [ ] **Step 1: Full CI gate**

Run: `cargo fmt --check` Run:
`cargo clippy --all-targets --locked -- -D warnings` Run: `cargo test --locked`
Expected: all green across the workspace.

- [ ] **Step 2: Live drive (the real proof)**

Invoke the `verify` skill (or run the TUI manually per the `run` skill). Drive a
real delegated `task`:

1. Start `hrdr` in a scratch project, send a prompt that triggers a `task`
   sub-agent (e.g. "delegate a quick task: list the files here and summarize").
2. Let it complete, then inspect
   `~/.local/share/hrdr/sessions/<cwd-slug>/subagents/<session-id>/`.
3. Confirm a `NNN-<slug>.jsonl` exists containing a `{"t":"start",…}` line with
   the full prompt and a terminal `{"t":"end","status":"ok",…}` line.
4. Crash/interrupt test: start a longer sub-agent and kill hrdr (Ctrl-C /
   SIGKILL) mid-run; confirm the `.jsonl` exists with `start` + partial `text`
   lines and **no** `end` line (an orphan `is_complete == false`), i.e.
   completed work is recoverable.
5. Background test: fire a `background: true` task; confirm its `.jsonl` gets a
   `"kind":"background"` start and a terminal end.

- [ ] **Step 3: Record the verification**

Note the observed evidence (paths seen, event lines present) in the final commit
body or PR description — observed output, not an assertion of success.

- [ ] **Step 4: Finish the branch**

Use the `superpowers:finishing-a-development-branch` skill to decide merge / PR.

---

## Notes for the implementer

- **`with_clippy` and the `let … && let …` chains** used above are let-chains
  (stable in the repo's edition — the codebase already uses them, e.g.
  `spawn_background`'s `if let … && let …`). Keep that style.
- **Ordering matters** in Tasks 3/4: write `Start` before moving `prompt` into
  `run`; clone anything moved into a spawned task.
- **Do not** add `fsync` per line — `flush()` is the agreed durability level
  (process-death recovery, not power-loss).
- **Do not** persist from sub-agents themselves — `subagent_base_config` clears
  the cell (Task 2, Step 2); verify a nested `task` never appears in the tree.
