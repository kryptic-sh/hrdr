---
name: tidy
description: tidy the change — DRY up reuse, cut dead code and over-abstraction
---

Tidy the change — a quality pass, NOT a bug hunt. Scope: the pending diff by
default, or the target named in arguments if given: $ARGUMENTS

1. Collect the scope. With no arguments, take the pending changes (staged,
   unstaged, and untracked); on a feature branch also diff against the
   merge-base with the default branch. If arguments name a file, module, or
   area, use that instead.
2. Read the changed code together with what it touches — the helpers it calls,
   the callers it has, and the siblings it sits beside — so a cleanup reuses
   what already exists rather than reinventing it.
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
4. Apply the fixes. Keep behavior identical — every change is a rewrite of the
   same result, never a change to what the code does. If a cleanup would alter
   behavior, leave it and note it rather than making it.
5. Run the project's format, lint, and tests; confirm they pass and behavior is
   unchanged.
6. Report what you tidied and why, and anything you deliberately left alone.
