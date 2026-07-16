---
name: commit
description: commit the working changes with a Conventional Commit message
---

Commit the current work.

1. Run `git status` and `git diff` (staged and unstaged) to see everything that
   changed. If arguments were given, scope the commit to them: $ARGUMENTS
2. Group the changes. If there are unrelated clusters, split them into separate
   commits — one logical change per commit — and commit each in turn.
3. Stage files explicitly by path. Never use `git add -A`, `git add .`, or
   `git commit -a`; they pick up files you haven't reviewed.
4. Write the message in Conventional Commits form: `type(scope): subject`.
   Types: feat, fix, docs, style, refactor, test, chore, perf, ci, build.
   Subject imperative and ≤72 chars. Add a body only when the _why_ isn't
   obvious from the diff.
5. Never skip hooks (`--no-verify`). If a hook fails, fix what it flagged and
   commit again.
6. After committing, show the result with `git log -1 --stat` and stop — don't
   push unless asked.
