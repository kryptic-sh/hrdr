---
name: cli
description: learn a command-line tool's real usage from the tool itself
args: [tool]
---

Learn how to drive a command-line tool by reading what the tool itself publishes
— the help and docs on THIS machine — instead of recalling or guessing its
interface. The tool you learn is the version actually installed: never outdated,
never mismatched, and the skill works for any tool on the machine, curated run
books included.

`$ARGUMENTS` is the tool to learn (or the task to learn it for — pick the tool
from the context).

1. **Confirm the tool exists first.** `command -v <tool>` (`where <tool>` on
   Windows). If it is not installed, say so — never invent commands for a tool
   that is not there, and do not install it unless asked.

2. **Walk the discovery ladder, cheapest first:**
   - `tldr <tool>` — examples-first cheat sheet, when tldr is installed (a
     subcommand: `tldr <tool> <sub>`).
   - `<tool> --help` (fall back to `-h` or `help` for tools that use those); a
     subcommand's own screen: `<tool> <sub> --help`.
   - `man <tool>` (or `info <tool>`) — the reference, for flags the help screen
     omits; read the sections you need rather than the whole page.
   - The tool's own docs when the help is thin — its `docs`/`help` subcommand,
     its README, or its website.
   - **The repo beats the man page for project-specific usage.** How THIS
     project drives the tool is in its CI config, Makefile, scripts and README —
     read those before assuming the default invocation.

3. **Verify before you trust, and before you use.**
   - Help text is a hypothesis; the executed command is the test. After
     learning, run a harmless read-only invocation to confirm the flags are
     real: `--version`, a `--dry-run` / `--check` / `--list` form, or the
     subcommand's help.
   - Never mutate state with a flag you only half-remember from a man page. Stay
     on the read-only or dry-run form of everything until the command is
     confirmed; the first command that would change something is a checkpoint,
     not a guess.

4. **What to learn:** the auth/configuration step first (most tools fail there:
   `gh auth status`, `aws sso login`, `doctl auth init`), then the read-only
   inspection commands, then the mutating ones. Note what this version actually
   has: a flag the help lists is real; one recalled from a different version may
   not be.

5. **When the tool's own help is unusable** — an interactive-only help, a
   missing man page, a version that disagrees with what the project expects —
   say so and fall back to the project's own docs and CI config rather than
   guessing.
