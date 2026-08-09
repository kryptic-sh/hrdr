---
name: plan
description: explore read-only and produce an implementation plan
---

Produce an implementation plan for the task given as arguments — explore only,
change nothing: $ARGUMENTS

1. Explore the relevant code read-only — no edits. Find the entry points, the
   seams the change must touch, and the existing conventions to follow. Read
   before you assume; if the task is ambiguous, ask rather than guess.
2. Map the change onto the code: which files and functions the work flows
   through, and where new code would slot in against the current structure.
3. Draft a step-by-step plan. Each step: one concrete action, the files it
   touches (with paths), and how to verify that step before moving on.
4. Call out risks and unknowns — assumptions that could be wrong, edge cases,
   places the existing conventions are unclear or the blast radius is large.
5. Write the plan — the ordered steps, the critical files with paths, the
   risks/unknowns, and the per-step verification — then route it by where you're
   working:
   - **Inside a git repo with a `docs/` directory** → write the full plan to
     `docs/<task>-plan.md` (a descriptive name for the task).
   - **Inside a git repo with no `docs/` directory** → write it to
     `<task>-plan.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the plan to disk, tell the user only a high-level summary plus
   the path you wrote; when you do NOT, give the user the full plan in your
   reply.

6. Stop after the plan. Do not implement anything until asked.
