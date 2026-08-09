---
name: commit
description: commit the working changes with a Conventional Commit message
---

Commit the current work.

1. Run `git status` and `git diff` (staged and unstaged) to see everything that
   changed. If arguments were given, scope the commit to them: $ARGUMENTS
2. Group the changes. If there are unrelated clusters, split them into separate
   commits — one logical change per commit — and commit each in turn.
3. Stage and word each commit as the Git section of your instructions says —
   explicit paths, Conventional Commits subject, body when the _why_ isn't
   obvious. Not restated here: it is already in front of you, and the copy that
   drifted from it is how this command came to specify a different subject
   length than the rules it was meant to follow.
4. Never skip hooks (`--no-verify`). If a hook fails, fix what it flagged and
   commit again.
5. After committing, show the result with `git log -1 --stat` and stop — don't
   push unless asked.
