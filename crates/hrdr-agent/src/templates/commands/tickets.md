---
name: tickets
description: create and update tickets for tasks in the context or backlog
---

Turn the open tasks in this session — the backlog (`docs/backlog.md`) and
anything raised in the conversation that is still unfinished — into tickets on
the project's issue tracker. Never create a duplicate: an existing ticket that
covers the task gets the new information as a **comment**, not a second ticket.

1. **Pick the tracker.** The repo's remote decides — `git remote -v`, or the
   README/CONTRIBUTING when there is no remote: `github.com` → GitHub's `gh`;
   `gitlab.com` or a self-hosted GitLab remote → GitLab's `glab`; a JIRA project
   → `acli`. Confirm the CLI is installed AND authenticated (`gh auth status`,
   `glab auth status`, acli's equivalent) — an unauthenticated session cannot
   write, so stop and name the missing login rather than pretending tickets
   exist. No remote, or more than one tracker plausible? Ask the user which one
   to use.

2. **Learn the tool, don't recall it.** Run `:cli <tool>` (gh, glab, acli) and
   follow it: confirm the create/search/comment commands exist on THIS
   installation before running them, and read the project's open issue list
   (`gh issue list`, `glab issue list`, acli's search) so the dedup step has the
   real state, not a guess. For `acli` in particular, do not write its
   subcommands from memory — its interface varies by installation; read its
   help.

3. **List the candidates.** Read `docs/backlog.md` and collect every entry that
   is an actual open task — skip the dated records, the considered-and-declined
   notes and the standing constraints (history and rules, not work). Add
   anything from this conversation that is unfinished and not already tracked.
   If there is nothing, say so and stop; do not invent tasks.

4. **Check each candidate against existing tickets.** Search by the task's
   distinctive words — title keywords, the file or symbol it names — with
   `gh search issues "<words>" --repo <owner>/<repo>`,
   `glab issue list --search "<words>"`, or acli's search, and skim the open
   list for a match. A ticket is a match when it covers the same task, not
   merely similar wording.
   - **Match found** → do NOT create. If the new material adds anything the
     ticket does not already say (a status change, a repro, a decision, a link),
     add it as a comment on that ticket (`gh issue comment <n> --body "…"`,
     `glab issue note <n> -m "…"`, acli's comment). If the existing ticket
     already says it all, add nothing and note the match.
   - **No match** → create a ticket. Title: the task in one line. Body: what the
     backlog or conversation says — the failure or ask, the mechanism,
     acceptance criteria — plus a `docs/backlog.md` pointer when the task came
     from the backlog.

5. **Report what happened, ticket by ticket**: created (with the URL or id read
   back from the CLI's output — a success exit code is not proof; the created
   ticket must be visible in the output), commented on (which ticket, what was
   added), or matched-and-skipped (which ticket). Name anything you could not
   create or comment on and why.
