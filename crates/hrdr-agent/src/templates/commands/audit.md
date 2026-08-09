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
3. Verify every candidate finding by EXECUTING it in your head against the real
   code, not by describing it. A security finding is a claim that some input
   REACHES some line — trace it, or you have not made the claim:
   - Follow your concrete attacker-controlled input line by line from the entry
     point to the line you claim is dangerous. Name what every validation,
     parse, split and branch in between does to it. Most false findings die
     here, at a step the narrative skipped.
   - Where the input is a string, actually apply the string operations. If the
     code does `strip_prefix` then `split('-').next()` then `parse()`, work out
     what your crafted value yields at each step — the answer is often "the
     parse fails and the dangerous line is never reached".
   - Re-read every line you are about to cite, and quote from that read. Do not
     cite a symbol you found in one file as if it were in another; when you
     contrast a weak check against a stronger sibling, open both and give each
     its own `file:line`.
   - If a guard, type, or caller makes the path unreachable today, it is not a
     vulnerability. Say so in the hardening notes instead of promoting it.
   - Trace to where the function RETURNS, not to the first step that agrees with
     you. Stopping at the interesting line is how a rejected input gets reported
     as accepted — the parse two lines down often throws the whole thing out.
   - Severity follows the traced impact, not the scariness of the function name.
     A call that cannot be reached is not critical; a `kill`, `exec` or `unsafe`
     that a guard already covers is not a finding at all.
4. If you split the audit across `task` sub-agents, what comes back is a list of
   CANDIDATES, not findings. You are publishing them under your own name, so the
   verification in step 3 is yours to do — a sub-agent's confidence is not
   evidence, and reading its report is not checking its work. Re-open every
   `file:line` it cites and re-trace before the finding enters the report; give
   each brief the verification contract from step 3, not just "report
   vulnerabilities with file:line"; and cover the whole scope you declared —
   whatever no brief names goes unaudited, so either brief it or list it as a
   gap in Coverage.
5. Write the report ranked most-severe first. Each finding: severity
   (critical/high/medium/low), `file:line`, a one-sentence statement of the
   vulnerability or defect, and the traced failure/exploit scenario. Then add:
   - **Cleared** — what you suspected and disproved, one line each with why it
     is safe. The expensive half of an audit, and it stops the next auditor
     re-treading it.
   - **Hardening** (if any) — correct today but fragile; explicitly not
     vulnerabilities.
   - **Coverage** — which entry points and classes you actually walked, and
     which you did not. Report a gap as a GAP: "not audited" is the honest line.
     Do not invent a constraint to excuse it — you have no clock, no budget and
     no deadline, so "limited by available time" is never true, and neither is
     narrowing a full-codebase audit to a diff range nobody asked for. "Audited
     everything" is almost never true either; saying where you stopped is what
     lets the user judge the report.

   End with a one-paragraph summary: total findings by severity, overall risk,
   and the top 1-3 things to fix first.

6. Route the report by where you're working:
   - **Inside a git repo with a `docs/backlog.md`** → append the full report to
     `docs/backlog.md` under a dated `## <area> audit YYYY-MM-DD` heading
     (backlog.md is the single work-item file; open findings belong in it, and
     do not create a sibling file).
   - **Inside a git repo without `docs/backlog.md`** → append it to `backlog.md`
     at the repo root (creating the file if needed).
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only the high-level summary
   (severity counts, overall risk, the top fixes) plus the path you wrote — not
   the full list. When you do NOT write to disk, give the user the full findings
   in your reply.

7. Report only — don't change any code unless asked to fix the findings.
