---
name: work
description: work the actionable backlog items, one slice at a time
---

Work through the project's backlog — `docs/backlog.md` — taking only the items
that are actionable now, one slice at a time, each through delegate → review →
commit.

1. **Read the backlog** (`docs/backlog.md`), plus any plan file the session or
   the backlog points at, and any decisions already made in this conversation.

2. **Classify each item:**
   - **Actionable** — no decision is still needed from the user: the decision is
     already made (in the backlog, in a plan file, or explicitly by the user in
     this conversation), the steps are clear, and the item is self-contained
     enough to start.
   - **Needs guidance** — requires a user decision, missing details, or an
     approval before work can start.

3. **If nothing is actionable:** tell the user plainly that no actionable items
   are available, and summarize the items that DO require their guidance — what
   each one is waiting on (a decision, a detail, an approval) — so the next
   round can be unblocked. Do not invent work.

4. **If the backlog is missing or empty:** there is nothing to classify. Ask the
   user whether they want to run `:sweep` — the review, audit, tidy and perf
   passes — which writes its findings into a fresh backlog for `:work` to pick
   up.

5. **Work one slice at a time.** Take ONE actionable item — or one coherent
   slice of it, the smallest piece that stands alone — and run it through
   **delegate → review → commit**:
   - **Delegate** the implementation to a sub-agent with a precise,
     self-contained brief: the exact files, symbols, before → after, and a
     done-criterion. When a slice is small enough that a sub-agent round-trip
     costs more than the work, do it directly instead — the loop is about one
     slice at a time, not delegation for its own sake.
   - **Review** the result yourself before it lands: read the diff, fix small
     issues, re-brief a fresh sub-agent when a spec was misunderstood.
   - **Commit** it with a Conventional Commits message, then start the next
     slice. Never open several slices at once — finish, review and commit each
     before moving on.

6. **Keep the backlog honest as you go.** When an item is done, DELETE it from
   `docs/backlog.md` — a closed item has no place in the one-file work list.
   Finished work is tracked in `git log` and, when the project keeps a
   changelog, in `CHANGELOG.md`: add the completed item under the matching
   Keep-a-Changelog section of `CHANGELOG.md` (Added / Changed / Fixed /
   Removed / Deprecated / Security, under `## [Unreleased]`) in the SAME change,
   naming the API, file or behavior that changed rather than restating the
   backlog entry. The backlog says what is still open; the changelog + git log
   say what shipped — an item that left the backlog without a changelog entry
   is work that shipped without a record. Leave guidance-needed items in place,
   and add anything this session raised but left unfinished. Finish by reporting
   what was worked, what was left and why, and what still needs the user.
