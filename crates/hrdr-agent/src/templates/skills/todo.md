---
name: todo
description: report what is left to work on from the session context
---

Review the current session context — everything we've discussed, decided, and
changed in this session — and report what remains to be done.

1. Scan the conversation and tool outputs for unfinished items: tasks we said
   we'd do but haven't, decisions deferred, loose ends, follow-ups mentioned.
2. Also check the working tree: uncommitted changes, TODO comments in code,
   half-finished refactors, scratch files.
3. Report a concise list grouped by:
   - **Immediate** — items we are mid-way through and should finish now.
   - **Next** — items queued after the current one.
   - **Later** — deferred or nice-to-have items.
4. Each entry: one sentence describing the item and its current state. No
   speculation, no filler — only what the context actually says.
5. If nothing is left, say so plainly.
6. Route the list by where you're working:
   - **Inside a git repo with a `docs/backlog.md`** → add to `docs/backlog.md`
     every item that is not already in it (backlog.md is the single work-item
     file — do not create a sibling file, and do not duplicate an item the file
     already records; follow its conventions: symbol names, not line numbers,
     and delete an entry once the item is done).
   - **Inside a git repo without `docs/backlog.md`** → append the list to
     `backlog.md` at the repo root (creating the file if needed).
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.
