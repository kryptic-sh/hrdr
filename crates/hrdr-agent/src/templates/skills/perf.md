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
4. Verify every candidate before reporting it. A performance finding is a claim
   about how OFTEN a line runs and how BIG its input is — establish both, don't
   assume them:
   - Trace the callers up to something whose frequency you can name (per
     request, per file, per token, once at startup). If you cannot reach such a
     caller, you do not know the path is hot, and you should say so or drop it.
   - Re-read every line you are about to cite, and quote from that read — a grep
     match tells you a pattern occurred, not what the surrounding loop bounds
     are. When you contrast two call sites, open both and give each its own
     `file:line`.
   - Drop micro-optimizations that don't move a hot path — a speculative "might
     be faster" is noise. Note where a fix trades memory for speed or vice
     versa.
5. Write the report, ranked by impact (biggest win first). Each entry:
   `file:line`, a one-sentence statement of the cost, why the path matters (hot
   / large N / per-request — with the caller that establishes it), and the
   concrete fix. Then add a short **Coverage** section: which paths you actually
   traced and which you did not, and anything whose cost you could not settle
   without profiling. Then route it by where you're working:
   - **Inside a git repo with a `docs/backlog.md`** → append the full report to
     `docs/backlog.md` under a dated `## <area> review YYYY-MM-DD` heading
     (backlog.md is the single work-item file; open findings belong in it, and
     do not create a sibling file).
   - **Inside a git repo without `docs/backlog.md`** → append it to `backlog.md`
     at the repo root (creating the file if needed).
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   (the top wins) plus the path you wrote; when you do NOT, give the user the
   full findings in your reply.

6. Report only — don't change any code unless asked to apply the fixes.
