---
name: review
description: review the pending diff for correctness bugs
args: [low, high]
---

Review the pending changes for bugs. Depth: $ARGUMENTS (default `low` — report
only high-confidence findings; `high` — broader coverage, may include uncertain
findings clearly marked as such).

1. Determine the scope:
   - If the working tree has **pending changes** (staged, unstaged, or
     untracked), review only those. On a feature branch, also diff against the
     merge-base with the default branch.
   - If `git status` is **clean** (nothing pending), or you are not in a git
     repo, review the **entire codebase**.
2. Hunt for correctness problems only: logic errors, broken edge cases (empty,
   zero, unicode, concurrent), error paths that swallow or corrupt state,
   resource leaks, API misuse, behavior changes callers don't expect. Skip
   style, naming, and formatting — that's not this review.
3. Verify every candidate finding before reporting it: re-read the surrounding
   code and the callers, and construct the concrete input or state that triggers
   the failure. Drop anything you can't back with a specific failure scenario —
   plausible-but-unverified findings are noise.
4. Write the findings ranked most-severe first, each with `file:line`, a
   one-sentence statement of the defect, and the failure scenario. If nothing
   survives verification, say so plainly.
5. Route the findings by where you're working:
   - **Inside a git repo with a `docs/` directory** → write the full report to
     `docs/code-review.md`.
   - **Inside a git repo with no `docs/` directory** → write it to
     `code-review.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   (counts and the top issues) plus the path you wrote — not the full list. When
   you do NOT write to disk, give the user the full findings in your reply.

6. Report only — don't change any code unless asked to fix the findings.
