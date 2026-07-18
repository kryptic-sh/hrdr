---
name: audit
description: audit the codebase for security bugs and correctness
args: [low, high]
---

Audit the codebase for security vulnerabilities, bugs, and correctness issues.
Depth: $ARGUMENTS (default `low` — report only high-confidence findings;
`high` — broader coverage, may include uncertain findings clearly marked as
such).

1. Map the attack surface: entry points (HTTP handlers, CLI args, file parsers,
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
   - Error handling: swallowed errors that hide failure, panic-on-untrusted-input,
     information leakage in error messages, unsafe unwrap/expect in library code.
   - Concurrency: data races, deadlocks, incorrect `Send`/`Sync` impls, async
     cancellation unsafety, lock order inversions.
3. Verify every candidate finding before reporting it: re-read the surrounding
   code and the callers, and construct the concrete input or state that triggers
   the failure. Drop anything you can't back with a specific failure scenario.
4. Report findings as a list, ranked most-severe first. Each entry: severity
   (critical/high/medium/low), `file:line`, a one-sentence statement of the
   vulnerability or defect, and the concrete failure/exploit scenario.
5. After the findings, give a one-paragraph summary: total findings by severity,
   overall risk assessment, and the top 1-3 things to fix first.
6. Report only — don't change any code unless asked to fix the findings.

