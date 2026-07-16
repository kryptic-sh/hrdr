---
name: review
description: review the pending diff for correctness bugs
args: [low, high]
---

Review the pending changes for bugs. Depth: $ARGUMENTS (default `low` — report
only high-confidence findings; `high` — broader coverage, may include uncertain
findings clearly marked as such).

1. Collect the full pending diff: staged, unstaged, and untracked files. On a
   feature branch, also diff against the merge-base with the default branch.
2. Hunt for correctness problems only: logic errors, broken edge cases (empty,
   zero, unicode, concurrent), error paths that swallow or corrupt state,
   resource leaks, API misuse, behavior changes callers don't expect. Skip
   style, naming, and formatting — that's not this review.
3. Verify every candidate finding before reporting it: re-read the surrounding
   code and the callers, and construct the concrete input or state that triggers
   the failure. Drop anything you can't back with a specific failure scenario —
   plausible-but-unverified findings are noise.
4. Report findings ranked most-severe first, each with `file:line`, a
   one-sentence statement of the defect, and the failure scenario. If nothing
   survives verification, say so plainly.
5. Report only — don't change any code unless asked to fix the findings.
