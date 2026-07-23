---
name: perf
description: report performance problems — hot paths, allocations, complexity
---

Report performance problems in the code — a performance pass, not a correctness
review (that's `:review`) or a quality pass (that's `:tidy`). Investigate and
report only; change nothing. Scope: the pending diff by default, or the target
named in arguments if given: $ARGUMENTS

1. Collect the scope:
   - If arguments name a file, module, or area, use that.
   - Otherwise take the pending changes (staged, unstaged, and untracked); on a
     feature branch also diff against the merge-base with the default branch.
   - If there are no arguments and `git status` is clean (nothing pending) — or
     you are not in a git repo — take the entire codebase.
2. Read the code together with what it touches — the callers, the data sizes it
   runs on, and how often the path runs — so you judge cost where it matters,
   not in the abstract. A slow line on a cold path that runs once is not a
   finding.
3. Hunt for performance problems, worst-impact first:
   - Algorithmic complexity: O(n²)+ where O(n log n)/O(n) is reachable, nested
     loops over large collections, a linear scan a map/set lookup would replace.
   - Allocations on hot paths: needless `clone`/`to_string`/`to_vec`, allocating
     inside a loop, `collect()` just to iterate, growing a `Vec`/`String` with
     no capacity hint, boxing a borrow would avoid.
   - Redundant work: recomputation that could be hoisted out of a loop or
     cached, re-parsing/re-serializing the same data, work repeated per item
     that could be done once.
   - I/O & syscalls: per-item I/O that could be batched, syscalls in a loop,
     unbuffered reads/writes, a blocking call on a hot or async path.
   - Concurrency: a lock held across `.await` or I/O, lock contention or
     over-synchronization, missed parallelism on independent work.
   - Data structures: the wrong container for the access pattern (a `Vec` linear
     search where a `HashMap` belongs), an index rebuilt on every call.
4. Verify every candidate before reporting it: confirm the path is actually hot
   or the input actually large, and reason about (or measure) the real cost.
   Drop micro-optimizations that don't move a hot path — a speculative "might be
   faster" is noise. Note where a fix trades memory for speed or vice versa.
5. Write the report, ranked by impact (biggest win first). Each entry:
   `file:line`, a one-sentence statement of the cost, why the path matters (hot
   / large N / per-request), and the concrete fix. Then route it by where you're
   working:
   - **Inside a git repo with a `docs/` directory** → write the full report to
     `docs/performance-review.md`.
   - **Inside a git repo with no `docs/` directory** → write it to
     `performance-review.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   (the top wins) plus the path you wrote; when you do NOT, give the user the
   full findings in your reply.

6. Report only — don't change any code unless asked to apply the fixes.
