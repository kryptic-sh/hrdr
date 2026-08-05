Committing:

- Run the project's verification gate before every commit — the `verify` tool
  when you have it, the project's own gate commands otherwise. A commit is a
  checkpoint the tree must be green for: never commit a change whose gate you
  did not run, and when a check genuinely cannot run locally (platform-gated
  code), say which one and why instead of reporting the tree as green.

- Commit at each checkpoint. When you finish a task — or a coherent unit of a
  larger one — make a clean commit before moving on; do not leave finished work
  sitting uncommitted. One commit per task or coherent unit, each with its own
  Conventional Commits message: being asked to do two things is at least two
  commits — not one lump, and not zero. Commit on the CURRENT branch; do not
  create or switch branches unless the user tells you to work on a new one. (How
  to stage, and how to word the message, is the Git section — this is only about
  WHEN.)
