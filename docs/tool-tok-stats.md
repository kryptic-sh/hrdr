# Tool calls and the tok/s stat in the generating marker

## What happens

The generating marker (spinner + "generating · ctx … · X tok/s …") is driven by
`draw_loader` in `crates/hrdr-tui/src/ui.rs:1294`. It reads from
`TurnStats::tok_per_sec()`, defined in `crates/hrdr-agent/src/turn.rs:134`.

`tok_per_sec()` divides `out_tokens` by `infer_elapsed()`. **Both variables
exclude tool-call time:**

- `out_tokens` only increments on `AgentEvent::Text` and `AgentEvent::Reasoning`
  — streamed model deltas. Tool calls add zero.
- `infer_elapsed()` banks model-working time and pauses the clock whenever
  `tools_running > 0` (`ToolStart` pauses, last `ToolEnd` resumes). Time spent
  waiting on a tool is never added to the denominator.

This is intentional: line 133 says *"Streamed tokens per second of model working
time — not of wall clock, so a long tool call doesn't read as a slow model."*

## What the user sees

### During a tool call

The loader **disappears entirely**. `draw_loader` is only rendered when
`inferring()` is true (`ui.rs:113`). During tool execution, `inferring()` is
false, so `loader_height = 0` — no generating marker on screen at all.

### After tools return

When the last tool of a round finishes, `inferring()` becomes true again. The
loader reappears. `tok_per_sec()` is recomputed as:

```
(earlier tokens + new tokens) / (earlier infer time + new infer time)
```

This is a weighted average. If the model generates at a consistent speed across
both stretches, the number barely changes — so it looks like "tool calls don't
affect tok/s." This is correct: the clock was paused, so tool latency is excluded
from the throughput figure.

## Is this a problem?

**By design.** The intent is to surface model speed, not end-to-end wall-clock
throughput. The clock-freeze comment in `turn.rs:65` says *"the clock stops
rather than inflating the turn with time it spent waiting."*

If the user wants the loader to remain visible during tool calls (e.g. showing
"running tool…" instead of disappearing), or to track wall-clock throughput
separately, that would be a new feature — not a bug in the current behavior.
