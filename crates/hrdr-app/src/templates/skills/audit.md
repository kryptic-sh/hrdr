---
name: audit
description: audit the codebase for security bugs and correctness
args: [low, high]
---

Audit the codebase for security vulnerabilities, bugs, and correctness issues.
Depth: $ARGUMENTS (default `low` — report only high-confidence findings; `high`
— broader coverage, may include uncertain findings clearly marked as such).

1. Determine the scope, then map the attack surface within it:
   - **Scope** — if the working tree has pending changes (staged, unstaged, or
     untracked), audit only those (on a feature branch, also diff against the
     merge-base with the default branch). If `git status` is clean (nothing
     pending), or you are not in a git repo, audit the entire codebase.
   - **Attack surface** — entry points (HTTP handlers, CLI args, file parsers,
     IPC, environment reads), trust boundaries, and where untrusted input flows
     through the system.
2. Hunt for each class systematically — don't skim, walk through the checklist:
   - Injection: SQL/command/template/path injection, XSS, header injection.
   - Memory & resource: use-after-free, double-free, buffer overflows, integer
     overflow/underflow, uncontrolled allocation, file-descriptor exhaustion.
   - Crypto: weak algorithms (MD5, SHA1, RC4), non-constant-time comparisons,
     missing authentication, hardcoded secrets or keys, predictable RNG for
     tokens.
   - AuthZ/AuthN: missing or bypassable authorization checks, confused-deputy,
     session fixation, token leakage in logs/URLs/error messages.
   - Data integrity: TOCTOU races, unsafe deserialization, missing input
     validation, type confusion, truncation/loss of precision.
   - Error handling: swallowed errors that hide failure,
     panic-on-untrusted-input, information leakage in error messages, unsafe
     unwrap/expect in library code.
   - Concurrency: data races, deadlocks, incorrect `Send`/`Sync` impls, async
     cancellation unsafety, lock order inversions.
3. Verify every candidate finding before reporting it: re-read the surrounding
   code and the callers, and construct the concrete input or state that triggers
   the failure. Drop anything you can't back with a specific failure scenario.
4. Write the report ranked most-severe first. Each finding: severity
   (critical/high/medium/low), `file:line`, a one-sentence statement of the
   vulnerability or defect, and the concrete failure/exploit scenario. End with
   a one-paragraph summary: total findings by severity, overall risk, and the
   top 1-3 things to fix first.
5. Route the report by where you're working:
   - **Inside a git repo with a `docs/` directory** → write the full report to
     `docs/security-review.md`.
   - **Inside a git repo with no `docs/` directory** → write it to
     `security-review.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only the high-level summary
   (severity counts, overall risk, the top fixes) plus the path you wrote — not
   the full list. When you do NOT write to disk, give the user the full findings
   in your reply.

6. Report only — don't change any code unless asked to fix the findings.
