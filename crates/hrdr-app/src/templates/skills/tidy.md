---
name: tidy
description: report cleanups — DRY up reuse, cut dead code and over-abstraction
---

Report cleanups for the change — a quality pass, NOT a bug hunt. Investigate and
report only; change nothing. Scope: the pending diff by default, or the target
named in arguments if given: $ARGUMENTS

1. Collect the scope:
   - If arguments name a file, module, or area, use that.
   - Otherwise take the pending changes (staged, unstaged, and untracked); on a
     feature branch also diff against the merge-base with the default branch.
   - If there are no arguments and `git status` is clean (nothing pending) — or
     you are not in a git repo — take the entire codebase.
2. Read the code together with what it touches — the helpers it calls, the
   callers it has, and the siblings it sits beside — so a proposed cleanup
   reuses what already exists rather than reinventing it.
3. Look only for quality cleanups, not correctness — that's `:review`, not this:
   - duplicated logic that should call an existing helper instead of repeating
     it,
   - dead code the change orphaned — now-unreachable branches, unused
     functions/fields/imports it left behind,
   - over-abstraction (YAGNI) — a trait, generic, or layer with a single caller
     that a direct call would replace,
   - a level of indirection the code could drop — a wrapper, alias, or
     intermediate that adds no meaning,
   - needless allocations or clones a borrow or reference would avoid.

   Only a cleanup that keeps behavior identical qualifies — a rewrite of the
   same result. If a change would alter what the code does, it is not a tidy;
   leave it out.

4. Verify each candidate before reporting it: re-read the code and its callers
   and confirm the cleanup is safe and behavior-preserving. Drop anything you
   can't back — a speculative "could be cleaner" is noise.
5. Write the report, each entry naming the cleanup, its `file:line`, and the
   concrete action (call helper X, delete unused fn Y, drop wrapper Z), then
   route it by where you're working:
   - **Inside a git repo with a `docs/` directory** → write the full report to
     `docs/tidy-review.md`.
   - **Inside a git repo with no `docs/` directory** → write it to
     `tidy-review.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   plus the path you wrote; when you do NOT, give the user the full report in
   your reply.

6. Report only — don't change any code unless asked to apply the cleanups.
