# hrdr `memory` tool — toward Claude-Code-style, LLM-managed memory

How to evolve hrdr's `memory` tool so the model can **fully manage** its own
memory — decide what to keep, write it structured, recall the right piece,
update and prune it — the way Claude Code does. Analysis date: 2026-07-23.

**Sources.** hrdr: `crates/hrdr-tools/src/memory.rs` (the tool);
`crates/hrdr-agent/src/lib.rs:876-960` (`read_memory_index`/`gather_memory` +
the budget constants); `crates/hrdr-agent/src/config.rs:389` (storage roots).
Claude Code: <https://code.claude.com/docs/en/memory> and
<https://ianlpaterson.com/blog/claude-code-memory-architecture/>.

## What hrdr already has

- **Plain-Markdown storage**, two scopes — `project` (this cwd) and `global`
  (all projects) — under `<XDG data>/hrdr/memory/projects/<cwd-slug>/` and
  `…/global/` (`config.rs:389`), supplied via
  `ToolContext::memory_project`/`memory_global`.
- **An index auto-loaded at session start**, budgeted to **200 lines / 25 600
  bytes** (`lib.rs:876-877`) — same budget Claude Code uses.
- **CRUD tool** — `view`/list, `write`, `append`, `delete` (`memory.rs:58`) —
  and it reads a Claude Code `MEMORY.md` / OKF `index.md` unchanged.

Storage and load-at-startup are solid. What's missing is the **model** of memory
and the **lifecycle** that lets the LLM own it.

## The Claude Code model (what "fully LLM-managed" means)

Four properties make Claude Code's memory manageable by the model rather than by
a human curating a file:

1. **One memory = one small file.** Each fact is its own file (kebab-case slug),
   not a line inside one growing document. That is what makes update, delete,
   and dedup precise — you replace or remove a file, never surgically edit a
   shared blob.
2. **Structured frontmatter** on every file:
   - `name` — the slug (stable id, and the `[[link]]` target).
   - `description` — one line; this is what **recall matches against**.
   - `type` — `user` (who the user is), `feedback` (a correction/preference,
     with **Why** + **How to apply**), `project` (ongoing work/constraints not
     in the repo), `reference` (a pointer to an external resource). Bodies use
     absolute dates and link related memories with `[[name]]`.
3. **`MEMORY.md` is a generated index of one-line pointers, not content** —
   `- [Title](file.md) — hook`. It is the map, loaded each session; the memories
   themselves live in the files.
4. **Recall is relevance-based, not a dump.** The pointer index loads every
   session; the memories whose `description` fits the current task surface **in
   full** (Claude Code injects them in `<system-reminder>` blocks). The model
   never has to grep, and the 200-line budget is never the ceiling on total
   knowledge — only on the always-loaded map.

The lifecycle is the model's: it writes/updates/prunes files and the pointer
line directly, guided by rules baked into the tool's instructions — **check for
an existing file before creating (update, don't duplicate), delete memories that
turn out wrong, don't store what the repo/git/AGENTS.md already records or what
only matters to this one conversation, convert relative dates to absolute.**

## Target design for hrdr

The keystone is switching from "one big index the model hand-maintains" to
"one-file-per-memory + a pointer index the tool maintains + relevance recall."
Everything else follows.

### 1. Structured memory files (the schema)

Adopt the frontmatter above (`name` / `description` / `type`, body with
`[[links]]`). hrdr already stores topic files; give them this schema. Keep
reading schema-less Claude Code/OKF files as before (no-migration-friendly): a
file without frontmatter is treated as `type: reference`, `description` = its
first heading/line.

### 2. Tool maintains the pointer index

When `write` / `edit` / `delete` touches a memory file, the tool updates
`MEMORY.md`'s pointer line for it (add / rewrite `description` / remove). The
model never edits two places or lets the index drift from the files. This is the
single biggest reliability win — it removes the "did I update the index?" burden
entirely.

### 3. Operations for precise management

- `write { scope, name, type, description, body }` — create or replace one
  memory file (and its pointer).
- `edit { scope, name, … }` — update a field or the body of one memory in place
  (replaces the current whole-file overwrite / append-only pair). Subsumes the
  earlier "section markers" idea — with one-file-per-memory there is no shared
  blob to anchor into.
- `delete { scope, name }` — remove the file and its pointer.
- `search { scope, query }` — rank memories by `description` + body match,
  return pointers (the model then `view`s the ones it wants). First-class recall
  on demand, not `grep`.
- `view` / `list` — as today.

### 4. Relevance recall

At session start, load the pointer index (as now) **plus** the full text of
memories whose `description` matches the working context, injected the way hrdr
already injects `<system-reminder>` context. On a cwd/topic change, refresh the
recalled set. This is what makes the store scale past 200 lines without rotation
gymnastics: the map stays small, the relevant memories arrive in full, and the
rest stay on disk until searched.

### 5. Auto-memory (the lifecycle triggers)

Bake explicit write-triggers into the tool description so the model saves
unprompted at the natural moments — an explicit "remember this", a user
**correction** of the model, a stated durable **preference**, a non-obvious
**project decision** — classified by `type`, deduped against existing files
(update in place), and pruned when a later fact contradicts them. Gate it (cap
writes per turn; never store conversation-only trivia or repo-derivable facts)
so it stays high-signal.

## How the earlier gaps fold in

| Gap (old ID)                 | In the target design                                                                                                                      |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| G1 auto-memory               | §5 lifecycle triggers — still the top behavioral win.                                                                                     |
| G2 in-place edit / markers   | §3 `edit` on one-file-per-memory — markers no longer needed.                                                                              |
| G3 date stamps               | Part of the schema (§1): absolute dates in the body.                                                                                      |
| G4 rotation / archival       | Largely **obviated** by §4 recall (map = pointers, not content); what's left is pruning contradicted/stale memories (§5), not truncation. |
| G5 topic routing + nav index | **Subsumed** by the pointer index (§2) + relevance recall (§4).                                                                           |
| G6 `memory search`           | §3 `search` — a first-class operation.                                                                                                    |
| G7 drift detection           | A periodic prune/verify pass over the files vs the index (cheap once §2 keeps them in sync).                                              |

## Priority & sequencing

1. **Keystone — §1 schema + §2 tool-maintained pointer index.** Everything
   depends on one-file-per-memory with a self-syncing index; do this first.
2. **§4 relevance recall.** Turns the store from "an index the model reads" into
   "memories the model is handed" — removes the truncation/routing problems (G4,
   G5) outright.
3. **§5 auto-memory triggers.** The highest-visibility behavior change: the
   model starts using memory unprompted, correctly typed and deduped.
4. **§3 `edit` / `search` / prune.** Precise management + on-demand recall +
   drift-free maintenance (G2, G6, G7).
