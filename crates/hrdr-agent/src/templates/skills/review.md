---
name: review
description: review the pending diff for correctness bugs
args: [low, high]
---

Review the pending changes for bugs. Depth: $ARGUMENTS (default `low` — report
only high-confidence findings; `high` — broader coverage, may include uncertain
findings clearly marked as such).

1. Determine the scope:
   - If the working tree has **pending changes** (staged, unstaged, or
     untracked), review only those. On a feature branch, also diff against the
     merge-base with the default branch.
   - If `git status` is **clean** (nothing pending), or you are not in a git
     repo, review the **entire codebase**.
2. Hunt for correctness problems only: logic errors, broken edge cases (empty,
   zero, unicode, concurrent), error paths that swallow or corrupt state,
   resource leaks, API misuse, behavior changes callers don't expect. Skip
   style, naming, and formatting — that's not this review.
3. Verify every candidate finding by EXECUTING it in your head against the real
   code, not by describing it. A failure scenario that was never traced is a
   story, and it will read exactly like a real one:
   - Take your concrete triggering input and follow it line by line from where
     it enters to the line you claim breaks. Name what every guard, parse, split
     and branch in between does to it. Most false findings die here, at a step
     the narrative skipped.
   - Where the trigger is a string, actually apply the string operations. If the
     code does `strip_prefix` then `split('-').next()` then `parse()`, work out
     what your input yields at each step — the answer is often "the parse fails
     and the dangerous line is never reached".
   - Trace to where the function RETURNS, not to the first step that agrees with
     you. Stopping at the interesting line is how a rejected input gets reported
     as an accepted one: a cookie whose username splits wrong looks like a
     wrong-user login until you read two lines further and find the expiry
     `parse()` failing and the whole thing returning `None`. Follow it out.
   - Re-read every line you are about to cite, and quote from that read. Do not
     cite a symbol you found in one file as if it were in another; if you are
     comparing two pieces of code, open both and give each its own `file:line`.
   - If a guard, type, or caller makes the failure unreachable today, it is not
     a finding. It may be a hardening note (step 5) — say so honestly instead of
     promoting it.
   - Prefer cutting to hedging. A dropped true finding costs one bug; a
     confident false one costs the user's trust in the whole report.
4. If you split the review across `task` sub-agents, what comes back is a list
   of CANDIDATES, not findings. You are publishing them under your own name, so
   the verification in step 3 is yours to do — a sub-agent's confidence is not
   evidence, and reading its report is not checking its work.
   - Re-open every `file:line` a sub-agent cites and re-trace its scenario
     yourself before it enters the report. Two reads across ten findings is not
     verification; if that is all you have time for, publish only what you
     actually checked and say the rest is unverified.
   - Give each sub-agent the verification contract from step 3 in its brief —
     the tracing, the citation rule, the reachability gate — not just "report
     bugs with file:line". A brief that only says what to look FOR gets back
     what the model finds plausible.
   - Cover the whole scope you declared. Sub-agent briefs are how coverage is
     decided: whatever no brief names goes unreviewed, so either brief it or
     list it as a gap in Coverage.
5. Write the findings ranked most-severe first, each with `file:line`, a
   one-sentence statement of the defect, and the traced failure scenario. If
   nothing survives verification, say so plainly — a short honest report is the
   good outcome, and padding it with what you already disproved is not.
   - WRITE THE FAILURE SCENARIO AS A TEST CASE, not as a paragraph. You already
     traced it in step 3, so you have the three things a test needs; give them
     under a `Repro:` heading, on their own lines, in the caller's own terms:
     the exact input or state, the expected result, and the actual one.

     ```
     Repro: days_from_civil(1970, 1, 1)
     Expect: 0
     Actual: 122
     ```

     Not "an incorrect epoch is computed for dates near the year boundary".
     Whoever fixes this next turns that block into a failing test in one step
     and knows when the fix lands. A paragraph has to be re-derived first, and
     in practice is not: it gets read, believed, patched around, and never
     observed to go green.

   - Where a finding genuinely has no expressible input — a race, an unbounded
     resource, a TOCTOU window — say so in the `Repro:` line and give the
     observable instead ("map length after 10k distinct IPs: expect 0 retained,
     actual 10k"). A thing you cannot state an observable for is a Hardening
     note, not a finding.

6. Add three short sections after the findings:
   - **Cleared** — the things you suspected and disproved, one line each with
     the reason they are safe. This is worth as much as the findings: it is the
     expensive half of the work, it stops the next reviewer re-treading it, and
     it shows which stones you turned over.
   - **Hardening** (only if you have any) — things that are correct today but
     fragile: an invariant held by convention rather than by a type, a guard
     that exists in one place and not its sibling. Explicitly not defects, so
     the user can triage them separately.
   - **Coverage** — state the scope you were given (step 1) and, against it,
     exactly what went unreviewed. Report a gap as a GAP: "not reviewed" is the
     honest line and more useful than a reason. Do not invent a constraint to
     excuse it — you have no clock, no budget and no deadline, so "limited by
     available time" is never true, and neither is narrowing a full-codebase
     review to a diff range nobody asked for. "Reviewed everything" is almost
     never true either; saying where you stopped is what lets the user judge the
     report.
7. Route the findings by where you're working:
   - **Inside a git repo with a `docs/backlog.md`** → append the full report to
     `docs/backlog.md` under a dated `## <area> review YYYY-MM-DD` heading
     (backlog.md is the single work-item file; a review's open findings belong
     in it, its closed ones in the Record section — do not create a sibling
     file).
   - **Inside a git repo without `docs/backlog.md`** → append it to `backlog.md`
     at the repo root (creating the file if needed).
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   (counts and the top issues) plus the path you wrote — not the full list. When
   you do NOT write to disk, give the user the full findings in your reply.

8. Report only — don't change any code unless asked to fix the findings.
