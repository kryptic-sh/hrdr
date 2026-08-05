---
name: sweep
description: run the review, audit, tidy and perf passes in one sweep
---

Run the four quality passes as one sweep. Load each pass's skill with the
`skill` tool (`name: "review"`, `"audit"`, `"tidy"`, `"perf"`) and follow its
procedure in full — never restate any of them here, so a change to a pass takes
effect without touching this skill. Each pass routes its own report to the
backlog; let it. When arguments are given, forward them to each pass as its
target scope. $ARGUMENTS

Order:

1. `:review` — correctness bugs in the pending diff.
2. `:audit` — security and correctness across the codebase.
3. `:tidy` — cleanups: DRY, dead code, over-abstraction.
4. `:perf` — hot paths, allocations, complexity.

When all four are done, give the user one merged summary: the findings per pass,
where each pass wrote its report, and any item two or more passes flagged — the
cross-cutting ones first.
