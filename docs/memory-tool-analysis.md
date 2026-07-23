# hrdr `memory` tool — gap analysis vs Claude Code

Comparison of hrdr's `memory` tool against Claude Code's memory system, to scope
a lifecycle rework. Analysis date: 2026-07-23.

**Sources.** hrdr: `crates/hrdr-tools/src/memory.rs` (the tool);
`crates/hrdr-agent/src/lib.rs:876-960` (`read_memory_index`/`gather_memory` +
the budget constants); `crates/hrdr-agent/src/config.rs:389` (storage roots).
Claude Code: <https://code.claude.com/docs/en/memory> and
<https://ianlpaterson.com/blog/claude-code-memory-architecture/>.

## What hrdr already matches

- **Plain-Markdown storage** — an `MEMORY.md`/`index.md` index plus topic files,
  OKF-flavored (`memory.rs:1-8`).
- **Two scopes** — `project` (this cwd) and `global` (all projects). Roots are
  supplied by the caller via `ToolContext::memory_project` / `memory_global`,
  laid out as `<XDG data>/hrdr/memory/projects/<cwd-slug>/` and `…/global/`
  (`config.rs:389`).
- **Auto-loaded into the prompt at session start**, budgeted to **200 lines / 25
  600 bytes** (`MEMORY_INDEX_MAX_LINES` / `MEMORY_INDEX_MAX_BYTES`,
  `lib.rs:876-877`) — matches Claude Code's 200-line / 25 KB budget.
- **CRUD via the tool** — `view` (also lists the scope when `path` is omitted),
  `write`, `append`, `delete` (`memory.rs:58`).
- **Reads Claude Code `MEMORY.md` and OKF `index.md` unchanged.**

## What's missing — the lifecycle layer

Storage and load-at-startup are solid; every gap is in **write / update /
maintain**.

### G1 — Auto-memory (P0)

The agent writes memory only when told. The tool description says "save facts
worth keeping, prune entries that become wrong," but nothing _triggers_ a write
on a correction, a stated preference, or a repeated fact — so memory is almost
never used unprompted. This is the biggest UX gap.

_Design sketch:_ a post-turn pass (or a lightweight lifecycle hook) that detects
a "remember this" instruction, a user correction of the agent, or a fact seen ≥N
times, and appends a dated line to the index — gated so it doesn't accrete noise
(dedupe against existing lines; cap writes per turn).

### G2 — In-place editing: section markers → `memory edit` (P0)

Two facets of one feature. Today `write` overwrites the whole file, `append`
only tacks to the end, and `delete` removes the whole file
(`memory.rs:107/120/142`) — there is no way to change a single fact without
`read` → edit-in-prompt → `write` back (lossy and race-prone).

- **Section markers** (`<!-- BEGIN <id> -->` / `<!-- END <id> -->`) are the
  mechanism: a stable anchor a tool can find-and-replace.
- **`memory edit`** is the action built on them: replace one block/line in
  place.

Build the markers first; `edit` depends on them.

### G3 — Date-stamped entries (P1)

No enforced `[YYYY-MM-DD]` prefix. Claude Code stamps entries in `/flush`; hrdr
has no schema, so dedup and rotation later have nothing to sort or age by. A
precondition for G4.

### G4 — Rotation / archival (P1)

At the cap, `read_memory_index` truncates and appends
`"… (truncated — read the full index at <path>)"` (`lib.rs`). Content past the
cap isn't _lost_ — the agent can still `read` the full file — but it is silently
out of the prompt. Claude Code rotates aged entries into archive/topic files so
the in-budget index stays high-signal; hrdr has no rotation, so a growing index
degrades to blind truncation. Depends on G3 (needs dates to age by).

### G5 — On-demand topic loading + navigation index (P2)

Topic files already exist, but only the index is auto-loaded — nothing routes to
the relevant topic file for the current task, and there is no project → path →
status map, so the agent falls back to `read` / `grep`. These two overlap: a
navigation index is what makes on-demand routing possible.

### G6 — `memory search` (P2)

No in-tool search; the agent must use `grep`, which doesn't know the memory
roots. A `search` action scoped to project + global would be a small,
independent add.

### G7 — Drift detection (P2)

No periodic audit comparing the memory system's own docs against reality (Claude
Code runs cron audits). Nice-to-have once G3–G5 exist.

## Priority & sequencing

| Tier   | Item                               | Depends on          | Why                                       |
| ------ | ---------------------------------- | ------------------- | ----------------------------------------- |
| **P0** | G1 auto-memory                     | —                   | agent barely uses memory unprompted       |
| **P0** | G2 section markers → `memory edit` | markers before edit | makes updates safe, not destructive       |
| **P1** | G3 date stamps                     | —                   | precondition for dedup / rotation         |
| **P1** | G4 rotation                        | G3                  | keeps the 200-line cap high-signal        |
| **P2** | G5 topic routing + nav index       | —                   | on-demand loading of existing topic files |
| **P2** | G6 `memory search`                 | —                   | small, independent                        |
| **P2** | G7 drift detection                 | G3–G5               | audit once the lifecycle exists           |

**Start with G2's section markers** — small, self-contained, and it unblocks
safe editing — and **G1 auto-memory**, the highest user-facing impact.
