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
5. Report the plan: the ordered steps, the critical files with paths, the
   risks/unknowns, and the per-step verification.
6. Stop after the plan. Do not implement anything until asked.
