# hrdr backlog

**One file.** Merged 2026-07-27 from `deferred-improvements.md`, `compare.md`
(the four-harness comparison) and `security-audit.md`, which are deleted — read
`git log` for what they said before this.

**Watch tool 2026-08-06** (the plan document `docs/watch-tool-plan.md` was
archived into this file same day it shipped): the new `watch` tool re-runs a
shell check in the background until it exits 0, returns an id immediately, and
wakes the model with the result when the condition flips — the missing primitive
for "watch the tag's CI run" in the release procedure, which used to force a
blocking `gh run watch` or a sleep-poll loop. Binding decisions carried forward:
a general check-command watcher, not a CI-specific one (the check runs under the
same guardrails and sandbox as `shell`); the result wakes the agent's turn like
a finished background sub-agent (the TUI's `maybe_deliver_background` and
`drain_background` are generic over `BackgroundTask`, so no delivery change);
model-invocable, with the end-turn contract in the tool description; and the
watcher reports "the run finished", never "the release is published" — the
confirmation (enumerate the jobs, check artifacts) stays a model step on wake.
Task and watch ids share one counter (`BackgroundTask::next_id`). The prompt's
"there is no polling tool" rule became "call `watch`, then end your turn", the
Releasing step calls `watch` on the tag run, and the two tests pinning `watch`'s
absence were rewritten. The one outstanding item is under Test coverage gaps.

**Threading pass 2026-08-04** (slices 1/2/3/5 shipped in
`1145ccd`/`b21bec8`/`5130f26`/`6549c7b`; the plan document
`docs/threading-plan.md` was archived into this file same day): blocking tool fs
now runs on `spawn_blocking`, each tool call in a batch runs as its own task
(with the panic-resume and cancel-abort mechanisms), session saves write off the
UI thread behind a latest-wins coalescer, and sub-agent construction runs on the
blocking pool. Binding decisions carried forward: the TUI stays the root task on
the main thread, blocking tool work belongs on `spawn_blocking`, and the
whole-turn agent mutex stays (the UI works around it with `try_lock`). **Slice 4
(attach reads off the UI thread) is deferred**: the `@file` reads are bounded
(~100 KB per file) and one-shot at submit, while the change would convert the
whole input path (`on_key` → `submit_input` → `spawn_turn`/`send_to_subagent`)
to async — a disproportionate ripple for a stall that is imperceptible next to
the per-round save Slice 3 removed. If the submit-time stall ever shows up, the
reads in `crates/hrdr-app/src/util.rs:130-213`
(`read_attach_file`/`read_attach_dir`/ `discover_skills`) are the site.

**Docs consolidation 2026-08-04.** Every other work-item file in `docs/` is
folded into this one and deleted — `git log` has what they said: the 2026-08-04
code review (`docs/code-review.md`, 6 findings, all fixed), the performance
review (see [Performance review — 2026-08-04](#performance-review-2026-08-04)),
the threading plan, the compaction rewrite plan (binding decisions kept in
[Compaction rewrite](#compaction-rewrite)), the DeepSeek provider plan (shipped,
one manual-smoke item below), and the sandbox redesign decision record (its open
items live in [Sandbox follow-ups](#sandbox-follow-ups)).

**Every claim below was re-verified against the tree at `8c76cdb`** before it
was carried over. What did not survive verification is either corrected in place
or listed under
[Corrections made during the merge](#corrections-made-during-the-merge). Items
that had shipped are in [Record](#record-closed-efforts), not here.

**Pruned 2026-07-27**: a fifteen-commit pass (`0fae706`..`36a7f2b`) cleared
everything that was actionable without a decision, and those entries are
**deleted, not annotated** — `git log` is the history. What the pass taught, and
the two decisions it surfaced, are under
[Cleared in the 2026-07-27 pass](#cleared-in-the-2026-07-27-pass).

**Pruned again 2026-07-30**, by the sandbox redesign (`5c9f675`..`c114a6a`). It
closed most of [Sandbox follow-ups](#sandbox-follow-ups) and the first two
top-of-list items, mostly by deleting the mechanism rather than finishing it —
see
[Cleared in the 2026-07-30 sandbox redesign](#cleared-in-the-2026-07-30-sandbox-redesign).
The same pass folded `docs/context.md` (a dated open-items file from 2026-07-29)
into this one and deleted it, so this is again the only backlog. Entries whose
subject no longer exists are annotated where the reasoning still teaches
something and deleted where it does not.

**Pruned again 2026-08-01**, by the full-codebase review pass
(`4e66a1c`..`2e3be29`, released v0.10.0). It closed all sixteen of its own
findings, so `docs/code-review.md` is deleted per the convention below; what it
left open is under [Review coverage still owed](#review-coverage-still-owed) and
[Known behaviour to revisit](#known-behaviour-to-revisit), and what it taught is
under
[Cleared in the 2026-08-01 review pass](#cleared-in-the-2026-08-01-review-pass).

**Pruned again 2026-08-02**, by the backend pass (`7e80605`..`9c3d012`). macOS
Seatbelt turned out to have been running on CI all along and is closed; Windows
gained an OS backend for `read` mode and is half closed, with the remaining half
recorded under [Sandbox follow-ups](#sandbox-follow-ups) as a decision rather
than an implementation. See
[Cleared in the 2026-08-02 backend pass](#cleared-in-the-2026-08-02-backend-pass).

**Pruned again 2026-08-02**, by the **removal of the web and desktop UIs**.
`hrdr-web`, `hrdr-ui` and `hrdr-protocol` are deleted from the tree and
`hrdr serve` is gone; hrdr is terminal-only. Everything downstream of that went
with them — the Web UI follow-ups, the "one live session, many windows" goal and
its `EventLog` multi-reader blocker, the whole frontend-parity plan (seam
analysis, protocol additions, slice order, wasm portability measurements) and
every web-only review finding. This is a **deletion, not a deferral**: nothing
here is waiting for the web to come back. See [Record](#record-closed-efforts)
for the one-paragraph epitaph and [Standing constraints](#standing-constraints)
for the rule it leaves behind.

**Pruned again 2026-08-03**, by the **directory trust gate**
(`0f4b440`..`ccbe08e`). hrdr now asks, once per working directory, whether that
directory's files may steer the session, and the answer decides whether
`AGENTS.md` and project skills are read at all. That reframes the
instruction-surface entries rather than closing them: what a trusted directory's
files may then do is still open, and is rewritten in place under
[Permissions, isolation, and state](#permissions-isolation-and-state). What it
opened is one gap of its own, under
[Tooling / agent capability](#tooling--agent-capability), and one rule, under
[Standing constraints](#standing-constraints).

**Pruned again 2026-08-06, of the fixes and shipped items** the dated review
sections had accumulated: every `[fixed — …]` finding is deleted, the six
fully-fixed sections (the 2026-08-05 correctness/audit/tidy/perf passes and the
2026-08-06 correctness/audit passes) are collapsed to closed records keeping
only their Cleared/Hardening/Coverage learning, the partially-fixed tidy and
perf passes keep only their open items, and the shipped/moot coverage gaps and
sandbox follow-ups are gone. Deleted, not annotated — `git log` and
[Record: closed efforts](#record-closed-efforts) are the history. What remains
open is under the sections above; the hardened-but-fragile notes stay where they
are.

Conventions:

- **Symbol names, not line numbers.** Line numbers rot — the old docs cited
  `hrdr-tools/src/lib.rs:965` for `Tool::description`, now at `:1155`, and half
  of `compare.md` cited a `system.j2` that no longer exists. Peer-harness
  citations keep their paths (clones are still at
  `~/Projects/harness/{codex,hermes-agent,opencode,pi}`).
- **Docs for finished work get deleted.** What survives a completed effort is
  only what still binds future work: those live in
  [Standing constraints](#standing-constraints).
- **Peer claims were not re-run.** The comparison was verified twice when it was
  written (2026-07-26, one sub-agent per harness, each given the same
  preliminary claims to confirm or refute). This merge re-verified the **hrdr
  side** of every finding — the half that decides whether an item is still open.

---

## Performance review 2026-08-04

From `docs/performance-review.md`, archived into this file. Findings ranked by
impact; the threading slices (above) closed #1's save half and #1d, and moved
the tool-blocking fs off tokio workers. What remains open:

1. **Per-round full-history clone ×3 in the event pipeline.** The turn loop
   clones the whole message list per round (`turn_loop.rs:759`), the registry
   log re-clones it (`registry.rs:415`; the reducer ignores the payload, the
   headless runner reads only its `len`), and the frontend re-clones it in
   `persist_mid_turn`. Slice 3 moved the save off the UI thread; the three
   clones remain — log a lightweight marker, hand the `History` payload by
   value, and drop `Session::save`'s double state clone
   (`session.rs:341`/`:778`).
2. **Request body deep-copied into a `serde_json::Value` per request.**
   `client.rs:1062-1120` (`to_value` + grafts) then reqwest serializes the tree;
   under no graft (default cache mode, no prompt-cache key, not DeepSeek) the
   Value is pure waste — serialize `ChatRequest` straight to bytes.
3. **`list_sessions()` rescanned per frame and per keystroke** while a `/resume`
   popup is live (`hrdr-app/completion.rs:201` ← `ui.rs:148`, `app.rs:994`;
   read_dir + stat + sort every call). Memoize by editor content / sessions-dir
   mtime.
4. **`rank_file_matches` over the 20k-file index per frame and per keystroke**
   (`hrdr-tui/app/completion.rs:160-166`, `WALK_MAX_FILES = 20_000`), with a
   `to_ascii_lowercase` per path per call. Precompute a lowercase path table.
5. **Full-history token re-estimate every round** on endpoints reporting no
   usage (`budget.rs:122-123`). Keep a running `messages_tokens` counter.
6. **Per-line `canonicalize_nearest` in the shell secret filter.**
   `grep_line_is_secret` (`lib.rs:1233`) realpath-chains the same path token
   once per match line; memoize the verdict per token for one command's run.
   (The grep-walk per-file canonicalize is closed: Slice 1 moved the whole walk
   to `spawn_blocking`.)
7. **Compaction tail-window selection re-sums overlapping suffixes**
   (`compaction.rs:451-460`) plus a per-stage history clone in the ladder
   sizing. One newest→oldest accumulating pass.
8. **`/resume` picker rebuilds every row per frame** (`ui.rs:600-663`):
   `relative_time` + `display_dir` + three width passes, unchanged between
   frames. Cache rendered rows and widths.
9. **Picker refilter allocates per candidate per keystroke**
   (`selector.rs:43-46`, `models.rs:786-791` `format!`+`to_lowercase`).
   Precompute a lowercase haystack per choice. **[partially addressed —
   `1b84108` hoisted the query normalization into a shared `fuzzy_match_q` core;
   the per-choice haystack build remains, deferred to a picker-layer pass — it
   collides with tidy 2026-08-06 #6 on the same pickers]**

## Performance review — second pass 2026-08-04

A fresh `:perf` run over the whole tree (working tree clean at the time). The
archived first review above is the canonical record where both cover the same
ground — items 1 and 3 re-found that review's still-open #1 and #5 and add
specifics. Every finding was re-verified at its cited lines before recording;
one candidate from the run was dropped (item 4).

**Status: item 1 open (needs a decision); items 2-4 dropped — not fixable as
proposed, or disproved at review time.** The fixed items are in Record: closed
efforts. Each item carries its own tag below.

1. **Per-round full-history save with two fsyncs — still the dominant cost of a
   long session.** **[needs direction — pick the crash-durability tradeoff]**
   `turn_loop.rs:785` clones the whole `messages` on every tool round and emits
   `AgentEvent::History`; the frontend clones again (`hrdr-tui/src/app.rs:2748`)
   into `persist_mid_turn`, which clones the whole `SessionState`
   (`app/session.rs:302`), `Session::new(snapshot.persisted())` (`:325`) clones
   once more (`session.rs:341`), `Session::save` clones again and re-serializes
   everything (`session.rs:778`/`:785`), and `write_atomic` does two fsyncs
   (`auth.rs:116`, dir fsync `:135-138`) — per round, on the save task. O(N) per
   round over a history that grows every round → O(N²) bytes written per
   session. The transcript already got the append-only jsonl fix
   (`session.rs:115-123` names the O(n²) it removed); `messages` has the same
   problem remaining. Sub-agents pay the same synchronously on their turn task
   (`delegation.rs:374-379` → `RunSnapshot::save` `:149-166`). Fix: write the
   round's appended messages to the append-only jsonl and keep full `.json`
   serialization for turn end; move (don't clone) the messages into the save
   path. Tradeoff: mid-turn crash durability drops from "at most one round lost"
   to "at most one turn lost" unless the jsonl also records messages.
2. **~3-4 heap allocations per streamed token.** **[dropped — infeasible: the
   event must survive to the frontend's `on_event` (`registry.rs:880-883`), so
   `record` cannot give ownership to `from_event`; a borrowed-`Record` variant
   is a larger refactor for one small clone per chunk]** Per chunk:
   `Accumulator::push` clones the delta (`hrdr-llm/src/types.rs:820`), the
   reasoning path clones (`turn_loop.rs:223`), `record` clones the whole event
   into the log (`registry.rs:427`), `Record::from_event` clones every string
   field again (`transcript_log.rs:111-143`), and the pane drain clones once
   more (`registry.rs:103`). Fix: `Record::from_event` takes the event by value
   (the closure at `registry.rs:880-883` owns it and drops it immediately),
   moving the strings — cuts ~2 allocations per token.
3. **No-usage fallback re-estimates the whole history every round.** **[dropped
   — risk > value: a running token counter must track ~11 message-mutation sites
   including in-place edits (`turn_loop.rs:408`, `compaction.rs:531`); a missed
   one silently corrupts the budget cap for a ~µs-per-round win]**
   `budget.rs:122-127`: when the endpoint reports no usage,
   `estimate_tokens_in_messages(&self.messages)` is O(N) per round. Fix: keep a
   running prompt-token estimate, add only messages appended since the last
   call. Same as the archived review's #5, still open.
4. **Dropped: compaction "overlapping suffixes".** The run claimed
   `compaction.rs:451-452` re-sums a growing slice per candidate turn — the
   archived first review's #7 made the same claim. It does not reproduce: the
   loop walks candidates newest→oldest with `tail_start` set to each `start`, so
   every `estimate_tokens_in_messages(&msgs[start..tail_start])` covers one
   disjoint turn; total work is O(tail), which the budget check needs.
   `mega_turn_tail_start` (`:505-506`) estimates single messages — fine. The
   archived #7's "per-stage history clone in the ladder sizing" was not examined
   here.

**Coverage** — traced: turn_loop, transcript, session, registry, transcript_log,
usage, budget, compaction, delegation, prompt, pane, hrdr-tools memory/sandbox/
lsp, hrdr-llm types/sse, hrdr-tui app/app-session/ui, hrdr-app util/completion/
format, apps/hrdr main, hrdr-editor. Good news confirmed along the way: the
jsonl coalescing is sound, the prompt is built once per agent, tool `defs()` is
once per turn, and the TUI has per-entry render caches. Corrections to the
hints: `lsp.rs` runs per edit, not per keystroke; `prompt.rs` is not on a
per-token path. Not settled without profiling: whether the per-round save
(item 1) actually dominates wall time (the fsyncs are the suspect, not the
serialize), the per-frame transcript layout loop (`ui.rs:880-916`) at very long
transcripts, and item 2's real lock-contention timing.

## Tidy review 2026-08-04

Quality pass over the whole tree (clean at the time); every candidate re-read at
its cited lines, behavior-preserving only, ranked by confidence.

**Status: item 9 open (needs direction — external API); items 1-8 fixed (Record:
closed efforts). Each item carries its own tag below.**

9. **Low: unused re-export `apply_cache_breakpoints`.** **[needs direction —
   removing a `pub use` from a published crate is an external-API decision]**
   `hrdr-llm/src/lib.rs:38` re-exports it; the only production caller uses
   `crate::types:: apply_cache_breakpoints` (`client.rs:1068`) and no workspace
   crate imports the re-export. Action: drop it — but hrdr-llm is a published
   crate, so removing a `pub use` is a public-API break for external consumers;
   safe only if nothing outside the repo pins it (pre-1.0 → minor bump if so).

**Coverage** — examined closely: hrdr-editor, hrdr-test-support, hrdr-llm export
surface, hrdr-app (sessions/history/format/transcript/effort/login/pane/
commands), hrdr-tools (lib, ls/read/shell/tree/mutation/mcp/hooks/lsp head),
hrdr-tui (lib/tui/theme/trust_prompt/app, app/session, app/selector, app/
completion, ui spinner area), hrdr-agent (paths/agents_dir/skills/auth/
auth_store/config/session/usage/transcript/turn_loop head/delegation head/
transcript_log), apps/hrdr main. Skimmed: sandbox/lsp/memory/verification/web,
ui.rs body, app/e2e.rs. Dropped as not-tidy: `agent_dirs`/`skill_dirs` and
`read_dir_profiles`/`discover_skills` shape differences (change for different
reasons); the `sandbox.rs:545` home_dir copy (wrong dependency direction); the
`split_whitespace().join(" ")` idiom; `sandbox.rs:1255`'s `#[allow(dead_code)]`
is a Windows-only backend in real use.

## Correctness review 2026-08-04

`:review` (low depth) over the whole tree, split across two passes (hrd-agent +
hrd-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + apps/hrdr). Both
findings below were re-traced at the cited lines; everything else the passes
suspected was disproved (Cleared) or is hardening.

**Status: finding 1 open (needs direction); finding 2 fixed — `f901485` (Record:
closed efforts). Each finding carries its own tag below.**

1. **`memory` descriptions containing a newline are silently truncated, and the
   truncation becomes permanent on the first edit.** (low) **[needs direction —
   pick the fix: quote+escape in `emit_memory` (format change, existing files
   must keep parsing) vs reject newlines in descriptions]**
   `hrdr-tools/src/memory.rs`: `emit_memory` (`:416-429`) writes
   `description: {value}` unquoted, one line; `parse_memory` (`:365-397`) reads
   per line via `split_once(':')`, so a value whose second line has no colon is
   dropped; `parse_scalar` (`:350-360`) additionally strips literal edge quotes
   that `emit` never adds. The index (`rebuild_index` at `:464` via
   `load_memories`) and `search`/`recall` then see only the first line, and
   `edit` (`:256-272`) re-emits the parsed (truncated) value when no new
   description is given.

   ```
   Repro: memory write {name:"x", description:"Build it\nThen deploy"} then memory edit {name:"x", body:"…"}
   Expect: description "Build it\nThen deploy" survives write → edit → index.
   Actual: index/search/recall show "Build it"; the edit rewrites the file with
           "Build it", deleting "Then deploy" permanently.
   ```

   Fix: quote and escape the description in `emit_memory` (and parse
   accordingly), or reject control newlines in descriptions at write time.

**Cleared** (suspected, traced, safe): SSE decoder overlong-line/UTF-8/EOF
handling; jsonl torn-line rollback and offsets; token/cost arithmetic clamps;
cache-breakpoint offsets (char-boundary guarded); retry budgets; the OAuth
single-flight coordinator; registry turn generations vs cancelled runs; TUI
completion `items.len() - 1` underflow (guarded by non-empty lists); save
pipeline lost-wakeup (`Notify` permit semantics); `truncate`/`middle_bounds`/
`collect_lines` boundary math; `truncate_inline`; history dedup/cursor math;
guardrail regex escapes (`--force-with-lease`, `git checkout .`); shell-arg
recursion bounds; sandbox canonicalization/write-escape/linked-worktree grants/
Landlock/Seatbelt; memory slug/traversal and index skips; completion-offset char
boundaries; login/OAuth state checks; `mega_turn_tail_start` reachability (it is
reachable — the sub-agent opener is a real user turn).

**Hardening** (correct today, fragile): PID-reuse vs stale session/store locks
(`session.rs:456`, `store_lock.rs:172`) — a recycled PID keeps a lock "alive"
forever, giving a spurious permanent busy error; `openai_refresh` requires
`refresh_token` in the response (`oauth.rs:361`) — a spec-minimal server forces
a re-login; the wrap-up round shares `overflow_compacted`
(`turn_loop.rs:524, 842`) — a second overflow on the forced wrap-up errors the
turn; `cost_partial` is a process-lifetime latch (`budget.rs:44`) that
`reset_session_cost` doesn't clear; `memory` `safe_stem` allows Windows-reserved
device names (`con`, `aux`, `nul`) — a confusing Windows-only write failure;
`parse_scalar` quote-stripping loses legitimately edge-quoted values (cosmetic
until finding 1's edit truncates).

**Coverage** — walked in full: hrdr-llm sse/retry/capped_read/catalog/types/
client; hrdr-agent session/transcript_log/transcript/compaction/budget/usage/
oauth/auth_store/registry/turn_loop/turn_state/turn/store_lock/model_ref/
resolve/auth/delegation/paths; hrdr-tools sandbox/memory/guardrails/shell/todo/
write/secret_diff/test_nudge/hooks/mcp-client/truncate core; hrdr-tui app.rs,
app/session, app/commands, app/completion, tui.rs; hrdr-app lib/util/history/
completion/effort/sessions/login/format/commands-dispatch; hrdr-editor. GAPs —
skimmed, not line-walked: `config.rs` (rest), `prompt.rs` (rest), `lib.rs`
(13.6k lines), `agents_dir.rs`, `skills.rs`, `hooks.rs`, `trust.rs`, `pane.rs`
(rest), `hrdr-tui/ui.rs` (4253 lines), `hrdr-app` status/highlight/subagents/
themes/palette/config and the rest of commands/, `hrdr-tools` lsp/web/gate/
ansi/proc/verification/mcp-transport and tools/{edit,read,replace,grep,tree,
verify,mutation,find,ls}, `apps/hrdr/src/main.rs`, hrdr-llm anthropic/codex/fs,
`app/e2e.rs`. Defects confined to those files are unreviewed.

## Correctness review 2026-08-05

`:review` (low depth) over the whole tree, split across two passes (hrdr-agent +
hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + apps/hrdr). Both
findings below were re-traced at the cited lines; everything else the passes
suspected was disproved (Cleared) or is hardening.

**Closed: both findings fixed — `674ca0f` (quota needle), `f8ed179`
(process-group drop-kill); the regression tests from the Repro blocks are in
place (Record: closed efforts). What the pass left to remember follows.**

**Cleared** (suspected, traced, safe — half B): history draft-stash vs vim's
trailing newline (symmetric per engine); Enter-path reservation dropped before
the first write (`reserve_session_id` early-return); save-coalescer lost wakeup
(`Notify` permit semantics); `/temp` edges (`is_finite` + `0.0..=2.0` covers 2,
0, nan, inf, 1e40; `default`/`reset` clears); `/export` hardening (second token
and existing path refused before write); `replace` capture-expansion OOM
(`refs × match_len` over-estimates and refuses before `expand`); `grep`
multiline line math; `read` coverage/CRLF/budget; `tree` depth consistency; SSRF
guards (connect-time resolver closes the rebinding TOCTOU; IPv4-mapped v6,
100.64/10, link-local, unique-local covered); LSP framing (`take(remaining+1)`,
Content-Length cap, colliding-id skip); mouse drag band clamps; arrow history
walk (deliberate behavior change, symmetric); `/copy msg` huge-range scan
(breaks at first `None`); `todo` evidence gate and id minting; `edit` CRLF
recovery; proc.rs pid guard (`pid > 1`); MCP pending bookkeeping (`PendingGuard`
removes id on failure/timeout).

**Cleared** (half A): fork jsonl copy (`std::fs::copy` preserves 0600;
`Session::save` never truncates the sibling jsonl; `load_path` folds it); retry
taxonomy (typed errors short-circuit on kind, so the phrase scan can't override
a correctly-classified `Transient`; `is_context_overflow`'s
`UsageLimit => false` arm is reached before any body scan); `compact()` indexing
(`before <= 2` early-return and `tail_start >= 2` keep
`messages[1..]`/`messages[tail_start..]` in bounds); `thinking_budget` ceiling
math (clamped into `[1024, max_tokens-1024]`); `model_version` segment rejection
(snapshot dates read 4.0, not 4.20250514); `parse_imf_fixdate` same-era past
dates (`saturating_sub` → `None`); wire-log test hooks (visibility-only, no
process-global state); prompt assembly / `prefix_len_before` (char-boundary
split guarded); config persistence (read-modify-write under `StoreLock`, unique
sibling temp + rename); `discover_skills`/`read_dir_profiles` caps (off-by-one
checked); `pane.rs` sync cursor (no replays; main pane never pruned).

**Hardening** (correct today, fragile): history persist spawns one OS thread per
`record` (`history.rs:148-163`; chain joins previous handle so writes never
reorder, but a burst while the disk is slow piles up one waiting thread per
outstanding write — a bounded worker channel would be equivalent); `fetch`'s
`is_blocked_host` is advisory — correctness rests on the connect-time
`SsrfGuardResolver`, one refactor from being the only guard (`web.rs:244-250`);
`parse_imf_fixdate` pre-1970 wrap (`client.rs:391`: `days as u64` wraps, one
spurious 60 s wait — practically unreachable); `trust.rs` newline in a directory
name (`trust.rs:100`: `writeln!` of a path containing `\n` splits the store, so
that directory can never be trusted and re-asks every launch — requires the user
to have answered yes on such a path, and is a permanently-untrustable directory,
not a trust escalation).

**Coverage** — walked in full (half B): hrdr-tools `ansi`, `proc`,
`verification`, `gate` (head/matching), `web`, `lsp`, `mcp/transport`,
tools/{find, ls, read, edit, write, replace, grep, tree, verify, mutation,
todo}, hrdr-app `history`/`config`/`status`/`completion`/`subagents`/`util`,
hrdr-tui `app.rs` (input/mouse/history/session paths), `app/session`,
`app/selector`, `app/e2e`, hrdr-editor `lib`/`plain`, `apps/hrdr/src/main.rs`,
the five fresh commits (be0f340, 6793464, 4bcbc36, b0316df, 52b452b). Walked in
full (half A): hrdr-llm `fs`/`retry`, hrdr-agent
`agents_dir`/`skills`/`hooks`/`trust`/`pane`/ `paths`/`turn_state`, the four
fresh commits (f901485, 12fb89c, 2a78ec2, 7eca4b7); walked key paths, skimmed
the rest: hrdr-agent `config`/`prompt`/`lib` (13.6k; remainder walked by the
2026-08-04 pass per the backlog), hrdr-llm `anthropic`/`codex`/`client` (error
classification, Retry-After parsing, stream loops). GAPs — not reviewed here
(walked by the 2026-08-04 pass per the backlog): hrdr-agent
`oauth`/`auth`/`auth_store`/`resolve`/`model_ref`/
`registry`/`transcript`/`transcript_log`/`turn_loop`/`turn`/`budget`/`usage`/
`store_lock`; hrdr-tools `lib.rs`/`sandbox`/`memory`/`guardrails`/
`mcp/{mod,client,tool,types,util}`; hrdr-app
`lib`/`highlight`/`themes`/`palette`/
`format`/`pane`/`sessions`/`transcript`/`effort`/`login`/
`commands/{model,helpers}`; hrdr-tui `theme`/`trust_prompt`/`tui`/`lib`/
`app/completion` and the `ui.rs` body; hrdr-editor `host`; `hrdr-test-support`.
Skimmed: hrdr-tui `ui.rs` scroll/selection/status-bar math, `app/commands.rs`
(be0f340 diff only), hrdr-tools `hooks.rs` (head), `secret_diff`/`test_nudge`.

## Security audit 2026-08-05

`:audit` (low depth) over the whole tree, split across two passes (hrdr-agent +
hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + apps/hrdr). One new
finding, verified at the cited lines; both 2026-08-05 correctness-review
findings were independently confirmed by the audit (the cross-cutting items).
Everything else suspected was disproved (Cleared) or is hardening.

**Closed: finding 1 fixed — `bc31e37` (owner-only config write); the two
confirmed review findings fixed — `674ca0f` (quota needle), `f8ed179`
(process-group drop-kill). What the pass left to remember follows.**

**Confirmed, already recorded** (independent audit confirmation of the
correctness review — cross-cutting): the `ProcessGroup::drop` group-kill on the
normal completion path (audit half B traced `proc.rs:197-209` +
`shell.rs:504`/`:711` to the same mechanism, also inherited by `verify.rs:157`
and `app.rs:1477`); and the bare `"quota"` needle terminalizing rate limits
(audit half A traced `retry.rs:195` + `:240-242` + `client.rs:335-337`
independently).

**Cleared** (suspected, traced, safe — half A): `5bc2e5d` memory-drift backup
(`std::fs::copy` preserves the source's bits; `.bak` lands in the same memory
root and can never be ingested — `load_memories` loads only `*.md`; the stem is
`safe_stem`-sanitized, no traversal); `f901485` fork jsonl copy (copy preserves
0600; `outcome.id` is a slugified `unique_session_id`, so the copy target can't
leave the session dir); `7eca4b7` repo-plan hunting (a `base.md` fragment
instructing the model; no code reads any file — follow-up reads go through the
sandboxed tools); `2a78ec2` `#[doc(hidden)] pub` test hooks (visibility-only;
`serve_response` binds 127.0.0.1 ephemeral; `set_backend_for_test` mutates one
instance); `12fb89c` taxonomy beyond the quota needle (typed errors
short-circuit on kind; mid-stream downgrades fire only after a `Transient`
classification; saturation on every usage counter); config.rs remainder
(`deny_unknown_fields`, per-field bounds, absolute-only writable roots,
alias-collision refusal, StoreLock read-modify-write); agents_dir/skills
(bounded discovery, fail-closed frontmatter, extension+stem path use only);
trust.rs (0600 store, exact canonical match, no ancestor trust, idempotent
check-then-append); anthropic/codex stream parsing (SSE capped 32 MiB, unknown
indices ignored not defaulted, unknown stop_reason passes through with a
warning, `Retry-After` clamped); chatgpt_models (10 MiB body cap, redirect
policy none, `AuthFailed` never serves stale, cache stores only sanitized rows);
prompt.rs (AGENTS.md gated on metadata size before read, no ancestor walk, jail
passes `ProjectInstructions::Skip`, bounded memory index); sweep_sessions
(auto-named only, open-lock held for the whole action, sibling jsonl +
subagents/ removed with the `.json`, unparseable files left for `/doctor`);
cwd_slug/sanitize_name (alphanumeric + hash suffix, no path escape).

**Cleared** (half B): `/export` path traversal (the argument comes from the TUI
input box a human types — no model or headless path reaches dispatch; existing
file refused, so no overwrite through a symlink to an existing target); `/temp`
hardening (`is_finite` + `0.0..=2.0`, `default`/`reset` clears); mouse
select-to-copy (anchor/head clamped, band read from the painted buffer only);
arrow-history walk and Enter-path lag (`Reservation`'s `Drop` releases the id
lock on every path); hjkl 0.41 (mechanical `Buffer`→`View` migration, disabled
default render fields); SSRF (`SsrfGuardResolver` closes the DNS-rebinding
TOCTOU; alternate IP encodings normalized by `getaddrinfo` before
`is_blocked_ip`; redirect targets covered; bodies capped); MCP (10 MiB body
caps, per-message SSE cap with `buffered_bytes()` reset, colliding
server-initiated ids rejected, stdio writes bounded by the 64-slot channel,
`PendingGuard` removes ids on failure/timeout); LSP (16 MiB frame / 16 KiB
header / 64 headers caps, errors degrade to "no diagnostics" never a failed
edit, `uri_to_path` percent-decodes lossy-never-panics); shell tool (`bash -c`
arg is one argv element by design, output bounded per-line and in-memory, secret
filter + diff redactor on every line, `!command` unsandboxed but still
filtered); read/edit/replace OOM and swap-TOCTOU guards; main.rs (trust gate
runs before `Agent::new`, jail forces `read_only` — the second flag is what
makes the jail hold).

**Hardening** (correct today, fragile — explicitly not vulnerabilities): the
verification-gate prompt section is repo-authored content the trust gate does
not cover — `Gate::detect` runs unconditionally (`lib.rs:1818`, `:2211`) and
`gate_section` (`prompt.rs:574-612`) renders the parsed CI commands as
authoritative fact ("run them … before you report work finished"), even from an
untrusted directory; inert today because `JAIL_TOOLS`
(`hrdr-tools/src/ lib.rs:1549` = read/grep/find/ls/tree) has no shell or
`verify` and the runner line is skipped when `verify` isn't registered
(`prompt.rs:582-589`), but it is the one instruction surface the trust gate does
not protect, and it becomes a live injection vector if jail ever gains
`verify`/`shell`; `backup_if_drifted` (`hrdr-tools/src/memory.rs`) — `unix_ts`
is `as_secs`, so two drift-detections in the same second produce the same `.bak`
and the second copy silently overwrites the first (never clobbers a memory — the
later copy is the more recent drift); `atomic_write` write-path TOCTOU
(`tools/mutation.rs:149-154` — admitted in the comment; requires a hostile
process racing the agent's own edits); MCP tool descriptions ride into the tools
cache block unwrapped (`mcp/client.rs:366-370`, `Box::leak`) — a compromised
operator-installed server can steer the model through its descriptions, where
results are wrapped as untrusted; `/export` writes to any absolute path the user
names (`conversation.rs:29` — equivalent to the user's own shell redirection,
but the transcript contains model output); `AgentDocs` doc comment
(`prompt.rs:817-827`) still describes walking cwd→root, stale since the trust
gate (doc drift only).

**Coverage** — walked in full (half A): `fs`, `trust`, `agents_dir`, `skills`,
`hooks`, `pane`, `config` (all 2750 lines), `prompt` (agent-docs, sections,
memory/environment/skills builders), `anthropic` (request build, thinking
dialects, stream loop, `map_event`, usage), `codex` (stream loop, `map_event`,
reasoning capture), `chatgpt_models`, `provider_catalog`, `models` cache paths,
`auth` (`write_atomic`), plus the fresh commits f901485/12fb89c/2a78ec2/
7eca4b7/5bc2e5d. Walked in full (half B): hrdr-tools ansi/gate/guardrails/
hooks/proc/verification/web/lsp, mcp/{transport,client,mod,util,tool},
tools/{shell,read,edit,replace,grep,find,ls,tree,verify,mutation,
secret_diff,test_nudge,write}, hrdr-app config/history/status/util/completion/
conversation + dispatch (all arms) + the save/mint pipeline, hrdr-tui app.rs
(mouse/history/`!command`/save paths), app/session, app/completion, hrdr-editor
host/plain/lib, hrdr-test-support, `apps/hrdr/src/main.rs`, the five fresh
commits (be0f340, 6793464, 4bcbc36, b0316df, 52b452b). Skimmed, not line-walked:
`hrdr-tools/src/lib.rs` remainder, `sandbox.rs`, `memory.rs` (beyond the
backup-permission question), `hrdr-tui/ui.rs` body, `app/e2e.rs`, hrdr-app
login/transcript/pane/skills/highlight/themes/palette/format/effort/sessions/
subagents, commands/{host,model,helpers,compaction}, `trust_prompt`/`tui`. GAPs:
hrdr-agent `session`/`transcript_log`/`transcript`/`registry`/`turn_loop`/
`turn`/`turn_state`/`delegation`/`oauth`/`auth_store`/`budget`/`usage`/
`compaction`/`store_lock`/`resolve`/`model_ref`/`paths`/`validate`, hrdr-llm
`client`/`types`/`sse`/`retry`/`capped_read`/`catalog`/`lib`, and `lib.rs`
(13.6k) beyond its I/O and prompt-assembly surfaces — not re-walked, per the
backlog's record that the 2026-08-04/05 passes covered them.

**Summary** — 1 new finding (low), 0 critical/high/medium; both
correctness-review findings independently confirmed. Overall risk is low: every
untrusted-input path walked (SSE parsing, agent/skill discovery, session paths,
cache reads, provider payloads, shell/secret boundary, sandbox write paths,
SSRF, MCP/LSP framing) is bounded, saturating, or fail-closed. Fix first: (1)
the config-write mode widening (`owner_only_options()`, one function), (2) the
open bare-`"quota"` needle (retry.rs:195), (3) the `ProcessGroup::drop`
group-kill — the review's finding 1, which the audit independently confirmed.

## Tidy review 2026-08-05

`:tidy` (low depth) over the whole tree. Every candidate re-read at its cited
lines together with its callers; only behavior-preserving extractions listed.
Nothing blocking — six DRY candidates, all since fixed.

**Closed: all six fixed — `bc31e37` (findings 3-4 and the hrdr-agent half of 1)
and `37f8623` (findings 1-2, 5-6).**

**Coverage** — examined closely: hrdr-editor (all three files),
hrdr-test-support (lib + helpers), hrdr-app (`lib`, `format`, `status`,
`completion`, `effort`, `sessions`, `skills`, `themes`, `palette`, `highlight`,
`history`, `subagents`, `pane`, `transcript`, `util`, `login` head), hrdr-tui
(`lib`, `theme`, `trust_prompt`, `app/session`, `app/completion`,
`app/selector`, `app/util`, `ui.rs` function inventory + six picker renderers),
hrdr-tools (`lib.rs` head/mid, `tools/mod.rs`, `grep` walkers, `mcp/mod.rs`),
hrdr-llm (`lib.rs` re-export surface, `fs.rs`, `catalog.rs` head), hrdr-agent
(`lib.rs` head — hooks, messages/todos idioms, model filter;
`skills`/`agents_dir` discovery, `transcript_log` owner-only dirs). Skimmed:
hrdr-tui `app.rs` body, rest of `ui.rs`, hrdr-agent `lib.rs` remainder,
`apps/hrdr/src/main.rs` (grep only), hrdr-app `commands/*` and `config.rs`. GAPs
— not looked at (walked by the 2026-08-04/05 passes per the backlog): hrdr-agent
`session`/`turn_loop`/`turn`/
`turn_state`/`registry`/`pane`/`transcript`/`transcript_log`/`compaction`/
`delegation`/`budget`/`usage`/`config`/`prompt`/`paths`/`resolve`/`model_ref`/
`oauth`/`auth` bodies; hrdr-llm `client`/`anthropic`/`codex`/`retry`/`sse`/
`types` bodies; hrdr-tools `sandbox`/`memory`/`lsp`/`web`/`verification`/`gate`
and remaining tools; hrdr-tui `app/e2e.rs`. Deliberately dropped: the
`sandbox.rs:545` `home_dir` copy (wrong dependency direction — already
recorded), the `now_ms` one-line delegations in `oauth`/`chatgpt_models`/`login`
(residual of the already-fixed item 1), the `ui.rs` picker-renderer shape (each
differs in fields/dimensions; extraction speculative).

## Performance review — third pass 2026-08-05

`:perf` (low depth) over the whole tree, new ground only. Every finding
re-verified at its cited lines with the caller traced to a named frequency;
everything already recorded in the two 2026-08-04 performance reviews was
checked and left alone (the per-round save pipeline, the `to_value` request
body, the completion rescans, the token re-estimate, the secret-filter
canonicalize, the compaction ladder clones, the fstat-per-record, the per-token
event clones — all still present, all previously recorded).

**Closed: all three fixed — `674ca0f` (items 1-2), `bc31e37` (item 3).**

**Coverage** — traced: the streaming decode path in full (sse.rs → client.rs /
anthropic.rs / codex.rs stream loops and `map_event`s → `drain_stream` →
`registry::record` → `transcript_log`), the turn-loop per-round surface (request
build, `defs()`, `maybe_self_compact`, budget, `account_usage`, History event,
tool batch dispatch, OAuth refresh), compaction (`first_viable_compact_stage`
ladder — recorded, re-checked), the TUI frame loop (`draw` → `transcript_chunks`
→ per-entry render caches, status bar, input, panels, scrollbar), pane sync
(incremental replay), shell tool output ingest, sandbox `check_read`/
`check_write`, catalog load/parse paths, `history.rs`, `format.rs`, `status.rs`,
hrdr-app completion file-index walk (off-thread, cached), `wrap_untrusted`,
`collect_lines`, `canonicalize_nearest`. Not re-walked (covered by the two
recorded passes): hrdr-agent `lib.rs` body, hrdr-tools `lib.rs` remainder,
`sandbox.rs` internals, mcp, web, verification, hooks, lsp, hrdr-editor,
hrdr-app login/transcript/sessions, `config.rs`, `prompt.rs`. GAPs — cost not
settled without profiling: the per-frame full-transcript layout walk
(`ui.rs:884-1001`: `transcript_chunks` + the `cum` loop builds a `Vec<Chunk>`
over all entries every frame, cached bodies or not) — still the recorded "not
settled" item; and the relative weight of items 1-2 (decode-path CPU) vs the
recorded per-round save pipeline (disk) is unmeasured.

## Correctness review 2026-08-06

`:review` (low depth) over the whole tree — the working tree was clean, so the
entire codebase was in scope, split across two passes (hrdr-agent + hrdr-llm;
hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + hrdr-test-support + apps/hrdr).
Both findings were re-traced at the cited lines; the Cleared and Hardening lists
are the passes' own. The changed code since the last sweep (`fcabdaa`) is the
TUI grouping/expansion work — both findings are in it.

**Cleared** — hrdr-agent + hrdr-llm (chunk A): the budget-reset loop
(`turn_loop.rs:538-550` — `while step < max_steps` guards, so `max_steps - step`
cannot underflow; the steer-reset round-counting matches the in-tree test);
`drain_steering`'s `bool` (the no-hook/recall delivery path cannot return
`Err`); compaction `context_after` (computed after `self.messages` is replaced,
over the same `[system, summary, tail]` + `tools.defs()` the next turn sends;
`saturating_add`; `0` only on the no-op path the TUI ignores); the `config.rs`
UI-key removal (the two key lists are pinned together by
`the_agent_accepts_every_ui_key`); the `Tool.expanded` removal (always
`#[serde(skip)]`, never serialized, no construction sites left); the
`edit | replace → ToolBody::Diff` splice (both tools return a `unified_diff`;
the renderer only colors lines); the skills registry/`build.rs` generation
(`files.sort()` deterministic, per-file `rerun-if-changed`,
CRLF/BOM/invalid-YAML handled, tested); the unchanged hrdr-llm decode paths
(sse/retry/capped_read/fs skimmed — no new issues).

hrdr-tui + the rest (chunk B): the frozen-spinner-in-summary suspicion (animated
bodies bypass `BLOCK_CACHE` via `lazy_height`, rebuilt per visible frame);
summary-vs-head-call cache-key collision (separate thread-local caches, and the
5th `BodyKey` element separates preview from full); `tool_group_head` walking
past the head (reverse `take_while(group_absorbs)` + `.last()` lands on the
first tool); a groupable tool after a group rendering standalone (impossible —
`tool_group_end` absorbs it); `content_rect` band math vs painted content;
`split_add_remove` byte-indexing (all indices from `find('+')` + ASCII runs, all
char boundaries); `browsing()` underflow (`pos ∈ 0..total` always);
`classify_diff_line` vs `---` headers; the uncapped edit/write/replace result
diffs (deliberate, tests updated); compaction-gauge no-op; `MAX_DIAG_LINES` 8→10
arithmetic; row-hit misplacement on wrapped rows (every hit in a call block
carries the same `ToggleToolCall(idx)` — a wrap-misaligned rect makes a row
dead, never toggles the wrong call).

**Hardening** (correct today, fragile):

- `turn_loop.rs:547-549` — `max_steps` is no longer a hard per-turn cap: every
  delivered steer resets the round budget, so sustained steering (or a held key)
  keeps the turn alive indefinitely; the cap only binds once steers stop.
  Deliberate and test-pinned, but the meaning changed.
- `turn_loop.rs:800-813` — the checkpoint note's "the turn ends at Y" is
  computed from the current budget, so after a reset the model can be told the
  turn ends at `max_steps` when further steers will extend it (advisory text
  only).
- `compaction.rs:908` — `context_after` is a ~4-bytes/token local estimate, not
  a provider reading; the post-compaction gauge drifts from the provider's real
  `prompt_tokens` (the estimator deliberately under-counts).
- `build.rs:29` — `to_string_lossy` on skill filenames: a non-UTF-8 filename in
  `templates/skills/` emits a lossy `include_str!` path and fails the build with
  a confusing error (shipped set is ASCII).
- `tool_groups`/`tool_open` keyed by the provider-supplied tool-call id
  (`turn_loop.rs:1078`): backends that number ids per request (llama.cpp-style
  `call_0`, `call_1`) reuse ids across turns, so a group/call expanded in turn 1
  makes turn 2's same-named group render expanded without a click. Unreachable
  today (in-repo mocks use unique ids); worth a session-unique suffix if ids are
  ever untrusted.
- `pack_loader_segments` counts `chars`, not display cells — safe only because
  every loader/summary segment is ASCII or width-1 braille; a wide (CJK) segment
  would overrun the width.
- A `Reasoning` entry that never settles (interrupted turn, restored session)
  keeps `took_ms: None` and shows a spinner forever; the resume path never
  re-stamps it.

**Coverage** — chunk A line-walked: `turn_loop.rs` 1-40, 280-1012 (full `run`,
drain, budget); `turn_state.rs` (full, 138 lines); `compaction.rs` 520-919;
`config.rs` 740-869; `skills.rs` 1-669; `transcript.rs` 300-459; `prompt.rs`
500-619; the full diffs of `lib.rs`/`session.rs`/`pane.rs` (all test/comment
changes); `build.rs` and `templates/*`. Also full: hrdr-llm `sse.rs`,
`retry.rs`, `capped_read.rs`, `fs.rs`, `types.rs` 690-889 (Accumulator),
`client.rs` 304-403, `lib.rs`. GAPs — not reviewed at all (unchanged since
`fcabdaa`, covered by that sweep): hrdr-agent `agents_dir.rs`, `auth.rs`,
`auth_store.rs`, `budget.rs`, `chatgpt_models.rs`, `delegation.rs`, `hooks.rs`,
`model_ref.rs`, `models.rs`, `oauth.rs`, `paths.rs`, `provider_catalog.rs`,
`registry.rs`, `resolve.rs`, `store_lock.rs`, `transcript_log.rs`, `trust.rs`,
`turn.rs`, `usage.rs`, `validate.rs`; hrdr-llm `anthropic.rs`, `catalog.rs`,
`codex.rs`, `client.rs` (beyond the classified-error paths).

Chunk B line-walked: every changed hunk of `ui.rs` (plus live reads of
`draw_chunks`, `flush`, the cache/lazy-height machinery, `transcript_chunks`,
`render_block`, `content_rect`, the tool-block/summary builders) and `app.rs`
(click/toggle paths, `prune_scrollback`, `push_entry`, history recall, Ctrl+S,
`TurnMsg::Compacted`); `app/commands.rs` hunks; `app/e2e.rs` skimmed with the
viewport/gap-click/call-preview tests read in full; the full diffs of
`theme.rs`, the hrdr-app files, `hrdr-tools/src/lsp.rs` +
`tools/{edit,replace,write}.rs`, `apps/hrdr/src/main.rs`, and the two test
files. GAPs — not opened: `crates/hrdr-editor`, `crates/hrdr-test-support`, and
the unchanged regions of `ui.rs`/`app.rs`/`e2e.rs` outside the hunks (all
unchanged since `fcabdaa`, covered by that sweep).

**Closed: both findings fixed — `695f07c` (finding 1), `de51c8b` (finding 2);
the regression tests from the Repro blocks are in place (Record: closed
efforts).**

## Security audit 2026-08-06

`:audit` (low depth) over the whole tree — the working tree was clean, so the
entire codebase was in scope, split across two passes (hrdr-agent + hrdr-llm;
hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + hrdr-test-support + apps/hrdr).
All findings were re-traced at the cited lines; the Cleared and Hardening lists
are the passes' own.

**Demoted from a pass finding to hardening, with the trace that disproved the
repro**: the claim "`SandboxMode::Read` does not confine the `memory` tool's
writes" (`memory.rs:253, 292, 305` use bare `fs::write`/`remove_file`; in `Read`
mode `writable_roots` is empty by construction, `sandbox.rs:198`). Traced
unreachable in the default configuration: `effective_sandbox` floors a
write-capable session's `read` request to `Write` (`config.rs:1797`, test-pinned
at `:2737-2745`), so `SandboxMode::Read` is only ever entered with
`read_only = true` — and the read-only tool scoping withholds `memory` (a write
tool; `Tool::read_only` defaults false, `lib.rs:1289`, and `MemoryTool` never
overrides it). Residual, recorded as hardening below.

**Cleared** — hrdr-agent + hrdr-llm (chunk A): OAuth CSRF/state (constant-time
compare; the `!=` probe only decides whether to keep listening); PKCE (RFC 7636
vector-pinned); credentials on disk (0600/0700, `create_new` + atomic rename,
locked RMW); token leakage in errors/logs (sanitized bodies, no auth headers in
the wire log, 8 KiB error cap); cross-provider key leak (`resolve_api_key`'s
parent fallback gated on identical `base_url`); path traversal (session ids
sanitized, cache names slugged, sub-agent `cwd` canonicalized + containment-
checked); unbounded retries (`RetryBudget` caps at 10 attempts/≈6¼ min,
`Retry-After` clamped); SSE/JSON overflow (32 MiB per-event caps, truncated-
event rejection, char-boundary-guarded slices); auth-header confusion
(provider-configured auth headers stripped from `extra_headers`); JWT account id
without signature check (feeds only a routing header); prompt-injection framing
(AGENTS.md labeled + TOFU-gated per directory, skills source-labeled).

hrdr-tools + the rest (chunk B): sandbox escape via `canonicalize_nearest`
(lexical `..` normalization, 40-hop symlink budget, regression-tested); symlink
race in temp writes (`create_new` + rename; the in-place fallback is
deliberate); SSRF in `fetch` (connect-time resolver filters internal/loopback/
link-local/CGNAT, no TOCTOU; `::` unblocked but fails at the OS level); MCP
transport (bounded reads, id-space separation, host-match SSRF guard, group-
killed children); shell injection in hooks (`Shell::quote` substitution,
metacharacter tests); command injection in `shell` (the command string IS the
intended payload; guardrails documented as a non-boundary); path traversal in
the file tools (canonicalize-before-root-check); memory-tool path escape
(`safe_stem` + component `Normal` check); secret-file exfiltration (structural
deny-list post-canonicalization in read/write/edit/grep/attach/shell-line
filter); terminal escape injection (ratatui cell buffers, ANSI-stripped for the
model); `@file` expansion (secret deny + handle-identity TOCTOU check + 100 KiB
cap); trust-gate bypass (headless auto-jail; ask is interactive-only); Windows
re-exec token (hrdr-emitted only, fatal if not lowered); config/session/history
files (0600 atomic writes, 10 MiB history cap, lenient TOML parse); uncontrolled
allocation (all reads byte-capped, `replace_all` projected before allocating).

**Hardening** (correct today, fragile):

- **The `memory` tool writes outside the sandbox roots in every mode**
  (`memory.rs:253, 292, 305`; the out-of-roots write is by design,
  `lib.rs:1542-1543`, and Read mode is unreachable for a memory-holding agent
  per the trace above). The residual is a profile whose explicit `tools:`
  allow-list names `memory` on a read-only agent — that agent would hold the
  tool and its writes would bypass the "write NOWHERE" boundary. If the promise
  must hold absolutely, route memory mutations through `resolve_write` or drop
  the tool from read-only sessions.
- The OpenRouter callback server reflects the attacker's own (escaped) state
  string (`oauth.rs:259`); the comment at `:255-257` should be preserved — a
  future "echo the expected value" change would leak the CSRF token.
- `trust.rs:95-100` opens the trusted-dirs store with `create(true)`: a
  pre-existing world-readable file keeps its mode and a symlink is followed. Low
  sensitivity (directory paths), user-owned dir.
- Write-path TOCTOU is documented, not closed: `resolve_write` canonicalizes,
  then `atomic_write` re-opens the path (`mutation.rs:150-153`); a symlink swap
  between the two could redirect a write. Same-user, no privilege boundary.
- `is_blocked_ip` has no IPv6 `::` arm — harmless today (connect fails); one
  line to make symmetric with the v4 `0.0.0.0` block.
- `grep_line_is_secret`'s `-`-delimited token parse (`lib.rs:1261-1276`) can
  mis-attribute a line to a wrong path prefix — a crafted filename can false-
  negative the courtesy filter.
- Landlock grants full `AccessFs::from_all` (incl. `REFER`) on writable roots —
  safe only because the `/` rule lacks REFER on the source side of a cross-root
  rename; the `NotEnforced` → refuse-spawn check is the backstop.
- Seatbelt profiles are author-written and unvalidated on real macOS
  (`sandbox.rs:1358-1364`); only a macOS run can validate them.
- `turn_loop.rs:40-43` — the frontend `AgentEvent` queue downstream is an
  acknowledged unbounded hop.

**Coverage** — chunk A walked in full: `oauth.rs`, `auth.rs`, `auth_store.rs`,
`store_lock.rs`, `trust.rs`, `skills.rs`, `capped_read.rs`, `sse.rs`,
`retry.rs`, `fs.rs`, `hooks.rs`, `paths.rs`, `usage.rs`, `budget.rs` (start),
`provider_catalog.rs` (start), plus targeted sections of `client.rs`,
`types.rs`, `anthropic.rs`, `config.rs`, `session.rs`, `compaction.rs`,
`delegation.rs`, `prompt.rs`, `transcript.rs`, `turn_loop.rs`. GAPs (grepped,
not read end-to-end): hrdr-agent `lib.rs` (13.8 k lines), `models.rs`,
`model_ref.rs`, `resolve.rs`, `registry.rs`, `validate.rs`, `pane.rs`,
`chatgpt_models.rs`, the non-test bodies of `anthropic.rs`/`codex.rs`, and
`transcript_log.rs`/`turn.rs`/`turn_state.rs` beyond spot checks. No shell
execution exists in scope (the only `Command::new` uses are `kill -0` liveness
probes).

Chunk B read in full: `sandbox.rs`, `web.rs`, `memory.rs`, `hooks.rs`,
`guardrails.rs` (first 300 lines), `proc.rs`, `write.rs`, `edit.rs`,
`mutation.rs`, `shell.rs` (spawn/stream core; ~1000 lines of tests skimmed),
`ansi.rs`, `find.rs`, `ls.rs`, `secret_diff.rs` (head), `mcp/transport.rs`,
`mcp/client.rs` (core), `sessions.rs`, `config.rs`, `history.rs`, `skills.rs`,
`trust_prompt.rs`, `tui.rs`, `editor/{lib,host}.rs`, `test-support/lib.rs`,
`main.rs` (headless/trust/config path). Read partially: `lib.rs`, `grep.rs`,
`verification.rs`/`verify.rs`, `lsp.rs`, `dispatch.rs`, `app.rs`/`ui.rs`. GAPs —
not opened: `hrdr-tui/src/ui.rs` (pure rendering), the rest of
`hrdr-tui/src/app/*`, `hrdr-tools/src/gate.rs`, `test_nudge.rs`, the bodies of
`tools/{tree,replace,read,todo,mod}.rs` (verified for resolve/secret-guard
routing via grep), `hrdr-tools/src/mcp/{mod,tool,types, util}.rs`, the bulk of
`hrdr-app/src/{login,status,transcript,completion, format,themes,palette,pane,highlight,effort,commands/*}`.
Test modules not audited.

**Closed: 2 findings, both low, both fixed — `4638e76` (finding 1), `a328c03`
(finding 2). 0 medium/high. The codebase is unusually well-hardened — every
untrusted-input read is byte-capped, credentials are 0600/0700 with atomic
locked writes, retries are bounded, path traversal is blocked at every file-name
boundary, OAuth state/PKCE/token handling is correct, and error paths avoid
echoing secrets. What the two fixes shipped: (1) a total-byte budget on
`Accumulator::push` so a flooding endpoint cannot grow memory for 300 s; (2)
ownership-checked lock release in `StoreLock` (Windows reap race + cheap
insurance against future refactors).**

## Tidy review 2026-08-06

`:tidy` (whole codebase — the working tree was clean) over two passes
(hrdr-agent + hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor +
hrdr-test-support + apps/hrdr). Every entry below is behavior-preserving. The
five dead-item claims and the SSE-literal claim were re-verified by
workspace-wide grep; the rest are the passes' own, verified by them against
callers.

**Findings (ranked):**

5. **Three hand-rolled temp-sibling + rename atomic writers.** `write_atomic`
   (`crates/hrdr-agent/src/auth.rs:95-141`, create_new + fsync),
   `write_config_doc` (`config.rs:2190-2214`, create_new, no fsync — a strict
   subset), `catalog::write_cache` (`crates/hrdr-llm/src/catalog.rs:458-485`).
   Action (caveat): move `write_atomic`'s logic into `hrdr-llm::fs` as the one
   shared helper; delegating adds fsync to config/catalog writes (durability
   win, no content change) — keep fsync on the credential path. Minimal
   alternative: document `write_config_doc` as `write_atomic` minus fsync.

6. **Picker-navigation blocks duplicated.**
   `crates/hrdr-tui/src/app/commands.rs` — `model_selector_key:512`,
   `session_selector_key:601`, `theme_selector_key:657`,
   `skill_selector_key:696`, `effort_selector_key:1112` — each carries the same
   Esc/Ctrl+C-close + up/down/backspace/push_char block (~20 near-identical
   blocks); `app.rs:1638-1688` repeats the wheel pattern five times. Action: a
   `Selector<T>` nav helper returning the key so each handler keeps its
   divergent bits (Enter, theme preview, model Ctrl+D, skill Enter-inserts).

7. **Duplicated hook-spawn/outcome block.** `crates/hrdr-tools/src/hooks.rs` —
   `run_file_hooks` (`:112-158`) and `run_event_hooks` (`:284-357`) share
   spawn→timeout→kill-on-timeout→ disarm-on-success plus the verbatim-identical
   failure/couldn't-run/timed-out note strings. The event path has one extra arm
   (exit-2 block note, `:322-332`) and pushes stdout to context; the file path
   discards it. Action: a shared `HookRunResult`/note builder for the four
   common arms.

8. **Test helpers duplicated across sibling test binaries.** `visible()` is
   verbatim in `apps/hrdr/tests/tui_pty.rs:80` and `trust_pty.rs:50`;
   `pty_available()`/`skip_for_want_of_a_pty()` in three files
   (`tui_pty.rs:48,67`, `trust_pty.rs:28,41`, `headless_tty.rs:35,48`) with a
   fourth variant in `sandbox_windows.rs:30-41`. The shared-helper home already
   exists (`tests/common/mod.rs`, which carries `drain_pty`/`pty_text` and the
   `#![allow(dead_code)]` for exactly this). Action: move all three into
   `common/mod.rs`.

9. **`spawn_line`/`spawn_diff`/`spawn_popup` near-duplicates.**
   `crates/hrdr-app/src/commands/host.rs` — `spawn_line` and `spawn_diff`
   (identical poster/tokio::spawn/await/post bodies differing only in the
   `starts_with("diff ")` classification), plus `spawn_popup`, a third copy
   added by the notice redesign (`2bff248`). Action: a private
   `post_async(poster, fut, classify)` all three default methods call.

**Deliberate mirrors checked and left alone** — `render_unfinished_todos` vs
`render_todos` (outputs genuinely differ; acknowledged in a comment);
`unix_millis` vs `unix_now` (different units); hand-rolled Levenshtein in
`models.rs` (justified as not worth a dependency); `usage_key` via `ModelRef`'s
Display (would canonicalize providers and change store keys — NOT
behavior-identical); `cached_body` vs `cached_block` (different maps/types, a
generic is more machinery than it removes); shell overflow-file naming vs
`save_overflow` (shell must keep the handle open — a streaming design the helper
doesn't support); `PlainEngine::paste` override (deliberately more efficient
than the default); `ProcessGroup`/`GroupKill` two-wrapper design;
`mcp::parse_sse_for_id` with its explained `#[allow]`.

**Coverage** — chunk A line-walked: hrdr-llm `lib.rs`, `fs.rs`,
`capped_read.rs`, `retry.rs` (1-490), `sse.rs` (1-270), `catalog.rs` (1-499),
`types.rs` (580-909), `client.rs` (20-404, 600-659, 1164-1587); hrdr-agent
`hooks.rs`, `paths.rs`, `turn_state.rs`, `trust.rs`, `auth.rs`, `budget.rs`,
`usage.rs`, `store_lock.rs`, `provider_catalog.rs`, `auth_store.rs`, `turn.rs`
(1-120), `turn_loop.rs` (160-409), `compaction.rs` (562-621), `model_ref.rs`
(1-130, 215-269), `resolve.rs` (1-120), `validate.rs` (1-120), `pane.rs`
(130-309), `registry.rs` (120-159), `chatgpt_models.rs` (100-379), `skills.rs`
(160-358), `agents_dir.rs` (150-489), `transcript.rs` (495-534),
`transcript_log.rs` (1-340), `config.rs` (2140-2229), `delegation.rs` (550-609),
`prompt.rs` (340-414), `models.rs` (280-319, 735-813), `lib.rs` (1-279). GAPs —
not line- walked: hrdr-llm `anthropic.rs`/`codex.rs` bodies, hrdr-agent `lib.rs`
(13.8 k) rest, `prompt.rs`/`session.rs`/`delegation.rs`/`config.rs`/`oauth.rs`
rest; all test files (6) not opened.

Chunk B read in full: hrdr-editor (all), hrdr-test-support (all four test
files), hrdr-app (`lib.rs`, `commands/*`,
`completion,config,effort,format, highlight,history,login,palette,pane,sessions,skills,status,subagents,themes, transcript,util`),
hrdr-tui (`app.rs`, `ui.rs`,
`app/{commands,completion, selector,session,util}`), hrdr-tools (`lib.rs`,
`ansi.rs`, `gate.rs`, `guardrails.rs`, `hooks.rs`, `proc.rs`, `test_nudge.rs`,
`tools/*`), apps/hrdr (`main.rs`, `tests/common/mod.rs`,
`tests/{smoke,headless,headless_tty, sandbox_windows,trust_pty}`). GAPs —
skimmed, not line-walked: hrdr-tui `app/e2e.rs` (8.9 k; all 202 helpers
grep-verified as used), `tests/tui_pty.rs` (beyond 200), hrdr-tools
`sandbox.rs`/`lsp.rs`/`memory.rs`/`verification.rs`/ `web.rs`/`mcp/client.rs`
(symbol + targeted reads only).

**Status: items 5-9 open, each naming its concrete action; the rest are fixed
(Record: closed efforts).**

## Performance review 2026-08-06

`:perf` (whole codebase — the working tree was clean) over two passes
(hrdr-agent + hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor +
hrdr-test-support + apps/hrdr). All findings below were re-traced at the cited
lines. Already-shipped wins from the 2026-08-05 pass were verified in place and
are not re-reported.

**Findings (ranked by impact):**

6. **`PaneSet::sync` rebuilds a full snapshot of every registry entry once per
   frame (cross-cutting — flagged by the TUI-side pass, code in hrdr-agent).**
   `crates/hrdr-agent/src/pane.rs:302-341` clones label/model/provider/
   base_url/effort per entry and locks + clones each steering queue, per call;
   invoked per frame from `hrdr-tui/src/app.rs:2022-2029` (`sync_panes`, which
   also pins panes — "Called each frame") plus per event
   (`app.rs:2054, 2081, 2359, 2619, 2760, 2902`). Fine at 1-2 agents; per-frame
   string cloning + N mutex acquisitions with many sub-agents. Fix: diff the
   registry snapshot rather than rebuilding it.

7. **Minor — every historical tool call's arguments re-parsed from JSON on every
   Anthropic request.** `crates/hrdr-llm/src/anthropic.rs:530-534`
   (`serde_json::from_str(&call.function.arguments)` per assistant message with
   tool_calls, per request build). Same unchanged strings each round; the
   OpenAI/Codex paths never re-parse. Fix: cache the parsed `Value` on
   `ChatMessage` after first parse (memory: a second copy of args on the wire
   struct).

8. **Minor — input pane laid out twice per frame.**
   `crates/hrdr-tui/src/ui.rs:74-77` (`desired_rows` → `compute_wrapped_layout`)
   and `crates/hrdr-editor/src/plain.rs:215-230` (`render` →
   `compute_wrapped_layout` again) on identical content, same frame, every
   spinner tick and keypress. Visible for a multi-KB paste. Fix: compute once in
   `draw` and pass it, or cache keyed by `(len, width)`.

9. **Context — `budget`'s O(H) token estimate per round when the server reports
   no usage.** `crates/hrdr-agent/src/budget.rs:122-127`
   (`estimate_tokens_in_messages(&self.messages)` per round in `account_usage`
   when `acc.usage` is None). Cheap per pass; hoistable into a running total,
   but needs care across compaction/resume. Not worth it unless the no-usage
   case (self-hosted servers) is common.

**Recorded items confirmed still present (not re-recorded):** the per-frame
full-transcript layout walk (`ui.rs:2664` `transcript_chunks` + `:912-918`
`cum` + `:974-1006` hit map — O(entries) per frame, cached bodies make per-entry
work cheap) and the per-frame cache/save pipeline (autosave turn-end, off-thread
behind a coalescer). Also noted: `/resume ` argument completion re-lists
sessions per keystroke (`hrdr-app/src/completion.rs:197` — small N, cacheable).

**Coverage** — chunk A traced end-to-end: turn loop (full), registry + Events
(full), client.rs SSE/request path, anthropic.rs + codex.rs build_body/stream
loops, budget.rs, compaction.rs, prompt.rs assembly + lib.rs callers, oauth.rs
coordinated access + auth_store cache, transcript_log.rs, session.rs save/load
caching, usage.rs, turn_state.rs, transcript.rs apply_event/hash. NOT read in
full (judged per-call or startup): hrdr-agent `lib.rs` (13.8 k),
`delegation.rs`,
`models.rs`/`model_ref.rs`/`resolve.rs`/`config.rs`/`chatgpt_models.rs`/
`provider_catalog.rs`/`skills.rs`/`agents_dir.rs`/`validate.rs`/`trust.rs`/
`auth.rs`/`store_lock.rs`/`hooks.rs`/`paths.rs`. Not profiled: the exact
per-parse cost of the 3.5 MB catalog and the OpenAI request-build serialize-vs-
clone split (both O(H), inherent to the wire format).

Chunk B traced fully: ui.rs frame path, app.rs event loop, hrdr-editor plain +
lib, hrdr-app completion/history/status/pane/sessions/format/
transcript/skills/subagents, hrdr-tools read/edit/write/replace/grep/tree/shell
ingest/secret_diff/sandbox/memory/tool dispatch/truncation, apps/hrdr headless
path. Not fully traced (per-tool-call / startup): hrdr-tools lsp/web/guardrails/
verification/proc/mcp internals, tools/{todo,verify,mutation-tail}, hrdr-app
commands/\*, config, login, effort, palette, themes, hrdr-tui app/commands.rs,
e2e.rs (tests), trust_prompt.rs. Unsettled without profiling: the per-frame
transcript walk's actual share of frame time, and the streaming-body re-render
per frame for in-flight tool calls (bounded by the block's size, but compounds
with the walk for long streams).

**Status: findings 6-9 open; the rest are fixed — `2f38e1b`, `631b432`,
`eafc82c`, `9d5f5ed`, `695840d`, `1e24635` (Record: closed efforts).**

## Dependency upgrades held back, 2026-08-03

A `cargo update` sweep took every compatible release and the major bumps that
were safe (`base64` 0.23, `sha2` 0.11, `similar` 3, `toml` 1, `toml_edit` 0.25,
`which` 8). Four majors were deliberately NOT taken. Each was tried, and the
reason below is what the attempt showed — not a guess about what it might do.

- **`hjkl` 0.33 → 0.40 is an architecture migration, not a bump.** 23 compile
  errors in `hrdr-editor` alone, all cascading from one root cause:
  `hjkl_buffer::Buffer` no longer implements `hjkl_engine::View`, and that trait
  is now sealed (`View: Cursor + Query + BufferEdit + Search + sealed::Sealed`),
  so nothing outside the engine can implement it. hjkl-buffer 0.40 has its own
  separate `View` type (`buffer.rs`), with the viewport moved onto the engine
  `Host` adapter — so `Editor<Buffer, HrdrHost>` is the wrong shape now, not a
  renamed one. `BufferView` also gained `background`, `cursor_column` and
  `search_ranges`. Doing this properly means understanding the new
  engine/host/view split and re-verifying the whole TUI input and rendering
  surface; it is its own task, and `hrdr-tui` was never reached because
  `hrdr-editor` failed first.
- **`reqwest` 0.12 → 0.13 changes where TLS roots come from.** It compiles once
  `rustls-tls` is renamed to `rustls`, and that is the trap: 0.12's `rustls-tls`
  resolves to `webpki-roots` (CA roots compiled INTO the binary), while 0.13's
  `rustls` pulls `rustls-platform-verifier` → `rustls-native-certs` (the system
  trust store). Verified both ways with `cargo tree`. hrdr ships static musl
  tarballs and an Alpine `.apk`; in a scratch or distroless container there is
  no system cert store, so every provider request would fail TLS while the build
  stayed green. Take this only with a decided answer on root certs, and test it
  in a container with no `/etc/ssl/certs`.
- **`ctor` 0.6 → 1.0 needs a new dependency.** 1.0 removed `#[ctor::dtor]` — it
  lives in a separate `dtor` crate now — and requires `#[ctor(unsafe)]` at every
  `#[ctor]` site. `hrdr-test-support`'s `remove_sandbox` dtor is what keeps the
  suite from leaving a sandbox dir per test binary in `/tmp`, so it cannot just
  be dropped. Adding `dtor` is a manifest decision for the owner, which is why
  this stopped here rather than proceeding.
- **`windows-sys` 0.52 → 0.61 cannot be verified locally.** The crate is
  `cfg(windows)`-only, so a local build compiles none of it and CI is the only
  verdict, one full round trip per attempt. At least one break is already
  visible by reading: `HANDLE` became a pointer in 0.59, so `sandbox.rs`'s
  `let mut token: HANDLE = 0;` and any `!= 0` comparison have to become
  `null_mut()`/`is_null()`. That code lowers the process integrity level, so it
  is the wrong place to write blind and confirm later. Worth doing as its own
  change, with the CI round trips budgeted.

**What the sweep did catch, worth remembering:** `toml` 1.0 changed `Value`'s
`FromStr` to parse a single VALUE rather than a document, so
`text.parse::<toml::Value>()` now fails on any real config with
`unexpected content, expected nothing`. Both call sites swallowed it with
`.ok()?` — one of them `provider_alias_collision_error`, a startup refusal that
would have gone on refusing nothing at all. Parse a `toml::Table` for a
document. Three existing tests went red and are the only reason this was not
shipped silently.

## Compaction rewrite

The plan (`docs/compaction-rewrite-plan.md`) is archived into this section — its
open items are below, and the binding decisions it left are listed at the end of
this section. Item 0, the origin-blind turn-boundary defect, and work items 1–3
shipped 2026-08-04; items 4 and 5 are below, both blocked on a decision rather
than on effort.

An audit of the shipped work against the plan, same day, raised ten gaps. Nine
are closed: the removed `max_tokens` override (see the standing constraint
below), the untested never-execute guard, the previous summary dropped by the
shrink stages, the missing sub-agent tail assertion, the unscoped cache figure
in the notice, the unenforced one-code-path claim, the unreadable cache fraction
(closed by the session-accounting work — `CompactionReport` now carries the
shrink stage and the attempt count, and compaction's own calls reach the session
counters), and the two below that were considered and declined. One is left,
under _Audit items needing a decision_.

### Item 4 — trigger on the body, not the total (BLOCKED, needs a decision)

The plan says hrdr measures total prompt tokens against the window while the
prefix (system prompt, `tools[]`, memory) is exactly what compaction cannot
reclaim, and cites codex's
`AutoCompactTokenLimitScope::{Total, BodyAfterPrefix}` as the shape to copy.

**Not built, because the arithmetic does not transfer.** hrdr's trigger is
DERIVED (`compaction_trigger` = `window − min(reserved, window/4)`), not
configured. Restating it against the body — fire when
`total − prefix ≥ window − reserved − prefix` — is the identical inequality, so
a literal port changes nothing. Codex's knob only means something because its
limit is a configured absolute number that the user sets against one scope or
the other.

The real defect the item is pointing at is narrower and worth fixing on its own
terms: **on a small window a compaction can reclaim nothing and still fire every
round.** Worked example — a 32k window with the reserve clamped to `window/4`
gives a 24k trigger; a 15k prefix leaves a 9k body, and `preserve_recent_tokens`
alone can exceed that, so `compact()` replaces the history with a summary plus a
tail that is no smaller, the trigger is still met on the next round, and each
round buys another summarization call. `compact()` only no-ops on the STRUCTURE
of the history (`before == after`), never on how many tokens a compaction would
actually free.

Options, needing the owner's call:

- **Gate on the reclaim.** Before summarizing, estimate
  `body − (tail + a summary allowance)` and no-op when it is below some floor.
  Cheap, local to `compact()`, and does not touch the trigger. Risk: an agent
  that genuinely cannot fit its next request now no-ops instead of trying, so
  overflow recovery has to be the thing that reports it clearly.
- **Subtract the prefix from the trigger's meaning** and expose the scope as
  config, closest to codex. Costs a config key, and the key is hard to explain
  precisely because the limit it scopes is derived.
- **Leave it.** The churn case needs a small window AND a large prefix; hrdr's
  prefix is large, so this is really a "small local model" defect.

Not measured on a real session — the example above is arithmetic from the
constants, not an observed run.

### Item 5 — compact with the outgoing model on a model switch (BLOCKED on shape)

Verified as described in the plan: `set_model_ref` → `adopt_resolved`
invalidates the cached window and the next `maybe_self_compact` fires against
the new, smaller trigger. So a downshift IS handled — but the summarizing is
then done by the INCOMING model, on a history that may not fit its window, with
a cold cache.

**The blocker is architectural, not conceptual.** `Agent::set_model_ref` and
`adopt_resolved` are synchronous, and compacting is an `async` model call. Doing
this properly means either making the switch path async (it is called from the
`/model` command flow in `hrdr-app` and `hrdr-tui`, so the change is not local)
or adding an explicit "compact before switching" step ahead of the switch in
those callers, which leaves the agent's own API able to perform an unsafe
switch. Neither is a detail to pick while implementing.

The plan's second half is separable and worth doing regardless: codex's
`should_retry_with_current_model` taxonomy (`InvalidRequest`,
`UnexpectedStatus`, `ContextWindowExceeded`, `UsageLimitReached`,
`ServerOverloaded`, `InternalServerError`, `RetryLimit`) separates
"model-specific, a different model might work" from "transient, retry the same
request". hrdr's `is_transient` only has the second class, so several
permanently-failing cases are retried. **Shipped `12fb89c`** (2026-08-05): the
one permanent class hrdr actually retried was the spent-quota 429 — typed
`Transient` on status alone and retried through the whole ~6-minute backoff. The
new `ChatErrorKind::UsageLimit` (codex's `UsageLimitReached`) is terminal,
decided by the body where it is available (`error_from_response` plus the
Anthropic/Codex/OpenAI mid-stream error objects); `is_transient` and
`is_context_overflow` treat it as terminal, and the four-class taxonomy is
documented on `ChatErrorKind` against codex's model. The model-switch half (a
different model might work) still has no machinery here — all four classes
surface rather than switch.

### Standing constraint: compaction overrides NO request parameter

**From Anthropic's prompt-caching docs, read 2026-08-04.** The summarization
call must be byte-identical to an ordinary turn in its parameters as well as its
prompt, or the prefix cache that compacting in place exists to hit is gone.

- `output_config.effort` and the `thinking` config sit in the same invalidation
  class as `tool_choice`: each _"always invalidates message blocks"_, and on
  models that render the config ahead of them, `tools` and `system` too. An
  effort or thinking override on the compaction path would undo the whole
  in-place-compaction win on **every** model.
- `max_tokens` is not on that table itself, but it is not neutral either.
  `thinking_budget` in `hrdr-llm/src/anthropic.rs` derives
  `thinking.budget_tokens` from it, so on the MANUAL thinking dialect — per
  `classify`, Claude 3.x and Claude 4 below 4.6, with an effort configured —
  capping the output rewrites the thinking block and costs the cache. (4.6 and
  later send an adaptive block with no budget, as does every model `classify`
  does not recognize; with no effort set the shape is `Off` and no thinking
  block is sent.)

The compaction-only output cap that used to exist was **removed** for that
reason: the summarizer now runs with the session's own `max_tokens`. Two things
replaced what it guarded — `first_viable_compact_stage` sizes its headroom off
the session's real allowance (falling back to `COMPACT_ASSUMED_OUTPUT_TOKENS`
only when the endpoint reports none), and a summary that runs to the limit is
still refused rather than accepted truncated. What is gone is the spend ceiling:
a pathological summarizer can now produce up to the session's full output
allowance. Revisit only if that is observed in practice, and not by
reintroducing a compaction-only cap.

Guarded by `the_compaction_request_keeps_the_live_prefix_byte_for_byte`, which
asserts the two requests' `max_tokens` match, and by the comment in
`Agent::plain_completion`.

### Audit items needing a decision

**The summarization request sends the whole history, not just the head.** So the
verbatim tail is summarized AND kept, and the model that resumes sees the same
events at two levels of detail — mitigated, not removed, by
`continuation_framing` telling it the verbatim tail is authoritative. The reason
recorded in `Agent::compact` is that truncating at `tail_start` would end the
request where no earlier request ended and so reach no cached prefix. **That
reasoning is worth re-checking before acting on it either way**: a head-only
request is still a byte-prefix of the request that was just cached, so whether
it hits depends on where the provider's cache breakpoints sit, not on where the
request ends. Nobody has measured it. Deciding needs the breakpoint behaviour
confirmed against Anthropic's docs and then a real two-request comparison —
which is the same setup the cache-fraction notice was built to make visible.

### Left open by the session-accounting work

**There is still no check that the prompt cache actually hits.** The counters
`AgentUsage::cache_hit_rate` feeds are reported, tested and readable, but every
test drives them from mocked usage figures — nothing proves a real provider
serves a real hit. `CompactionReport`'s own doc admits why: it needs a live key
and two sequential requests. The right shape is an `#[ignore]`d integration test
that runs a turn, compacts, and asserts `cached_tokens > 0` on the summarization
call, wired into a nightly CI job with a secret. That is an infrastructure
decision, not a code change. Until it exists, the session rate in `/cost` is the
only thing standing between a silent cache regression and a billing period.

**Cache counters start at zero on a resume.** `AgentUsage`'s three new fields
are `#[serde(default)]`, so a resumed session's rate describes only the calls
made since the resume — while `tokens_in`/`tokens_out`/`cost_usd` carry forward.
Deliberate rather than overlooked: reconstructing them would need per-call cache
history, which nothing stores. Worth revisiting only if the split reads as a bug
in practice.

### Audit items considered and declined

**The tool-call retry does not go through `RetryBudget`.** The plan's letter
says it should; `compact` counts its own `tool_call_attempts` instead.
Deliberate — `RetryBudget::retry` only retries transient network and server
failures and refuses everything else, so a model answering with a tool call
would fall straight through it and abort the compaction on the first offence.
The separate counter is bounded by `COMPACT_TOOL_CALL_ATTEMPTS` and is now
covered by `compaction_gives_up_on_a_model_that_only_ever_calls_tools`.

**"Logged AND persisted" has no log to write to.** `hrdr-agent` takes no
`tracing` or `log` dependency at all, and adding one for a single line is not
worth a dependency the project has otherwise done without. The transcript is the
log: every compaction emits `AgentEvent::Notice(report.notice())`, and the
trigger is persisted with the summary itself as
`MessageOrigin::Summary(reason)`, so a resumed session still knows why its
history was replaced.

### Noticed while working, not fixed

`session_name_from` (`hrdr-agent/src/session.rs`) takes the first `Role::User`
message, which after a compaction is the summary — so a session first NAMED
after a compaction gets "This conversation was compacted…" as its name. Now
trivially fixable with the `MessageOrigin` predicate that compaction uses
(`compaction::is_user_turn`), but out of scope for the plan and rare in
practice: names are set on the first save, which normally precedes any
compaction.

### Binding decisions from the archived plan

These constrain any future compaction work; the plan file is deleted, so they
live here:

- Compact **in place**, request shape unchanged (no separate summarization
  endpoint, no `tool_choice` — rejected 2026-08-04).
- The reinjected summary is a **distinguished message** (a `MessageOrigin` of
  its own), never a user turn; prose framing is guaranteed by instruction plus a
  hard guard.
- Keep hrdr's **whole-turn tail** and the **local shrink ladder** (the staged
  fallback when the full history won't fit the window).
- The `MessageOrigin` serialized names changed — a **clean break** on session
  files, decided 2026-08-04.
- Compaction **overrides no request parameter** (see the standing constraint
  above).

## Owed right now

- **v0.11.0 is not on the AUR.** Every other channel published on 2026-08-03 —
  the GitHub release with all its assets, the seven crates on crates.io,
  Homebrew, Scoop, Alpine — and `Publish AUR (hrdr-bin)` failed on run
  `30767495527` with `The AUR is down due to maintenance`, an outage on Arch's
  side with nothing wrong in this tree. `hrdr-bin` still reads `0.10.0-1`.
  Probed for an hour afterwards and it was still down: note that the package
  page and the RPC endpoint stay **up** through this, so neither one tells you
  whether it has cleared — the check that answers is
  `git ls-remote ssh://aur@aur.archlinux.org/hrdr-bin.git`, and SSH auth
  succeeding says nothing either (the login banner works while the git backend
  refuses). **The fix is `gh run rerun 30767495527 --failed`, not a new tag**;
  the job stages the PKGBUILD and exits 0 when the diff is empty, so re-running
  it is safe. The tag run stays red until it lands, which is
  `tag-release status` working as intended. Probed again 2026-08-06: the RPC
  still reports `0.10.0-1`. **Delete this entry once the RPC reports
  `0.11.0-1`.**

## Deferred 2026-08-05

- **Todo panel cut off 1 row at the bottom when following.** Reported
  2026-08-05: with the transcript fully scrolled down (following, `offset 0`),
  the todo list's last row sits one line below the visible area — the
  scrollbar's `↓` lands on the `▸ N finished` toggle row while panel rows
  continue beneath it, as if the transcript area ends a row early. **Not
  reproduced** in e2e probes: following at `offset 0` at every terminal height
  (11–30 rows), with and without a finished sub-agent panel present, the panel's
  bottom pad renders on the transcript area's last row. The repro gap: the
  report came from a _resumed_ session (the `resumed … (359 messages)` notice
  was on screen), so the untried variable is the resume path — resume rebuilds
  the transcript and the follow state, which the probes did not drive. Revisit
  with a resume-driven repro before touching the layout; candidates if it
  reproduces: `draw_chunks`' `scroll`/`inner_scroll` off-by-one at the bottom,
  or the live-panel chunk heights vs the transcript area.

- **Hide `Notice` transcript entries unless verbose — the noise half is closed
  by the notice redesign (`2bff248`).** The variant-mixing blocker is gone:
  `App::system` and the `TurnMsg::System` handler now toast, and a slash
  command's data output opens an Esc-dismissible popup — so the
  `EntryKind::Notice` chrome this item named ("session saved as …", config
  reloads, slash-command output) no longer enters the transcript at all. What
  remains is the session-opening banner (the `App::new` welcome + `/reload`'s
  header repush, `hrdr-tui/src/app.rs`'s `App::new` and `app/commands.rs`'s
  `reload_cmd`), which still renders always. Decide whether THAT should be
  verbose-gated; the proposed `Command`-variant split and the
  `EntryKind::Notice(_) if !expand_tools` guard are moot.

## Top of the list

The five that were here are all shipped — see
[Cleared in the 2026-07-27 pass](#cleared-in-the-2026-07-27-pass). Items 1 and 2
closed on 2026-07-30 with the sandbox redesign, both by **deletion**: the
file-tool metadata guard is gone (it refused the honest path while `shell`
walked round it), so there is no list to extend; and there is no `git` tool, so
nothing runs outside `sandboxed_shell_command`. What is left needs a decision,
not work.

1. **Content scanning for `AGENTS.md`, if ever wanted.** Largely superseded
   2026-08-02 by the directory trust gate (`hrdr_agent::trust`): the question
   "may this checkout's files steer my agent" is now answered by the user before
   anything is read, an unanswered directory opens jailed and loads neither
   `AGENTS.md` nor project skills, and the read no longer walks ancestors. What
   was already there stays — a skipped file surfaces as a notice naming path and
   size, and the block header states the provenance and the ceiling. What is
   still unbuilt is hermes' `_scan_context_content` — blocking a file with a
   `[BLOCKED: …]` placeholder on an injection heuristic — which would now only
   bite inside a directory the user explicitly vouched for. Deliberately
   deferred, and more so than before: a regex scanner over project docs
   false-positives on exactly the repos a coding agent gets pointed at (security
   tooling, shell-hardening guides, this file), hermes needed three scopes to
   make it tolerable, and a false positive silently drops conventions the user
   asked for. **Wants evidence of a real attempt first.**
2. **The test nudge has no teeth.** Fired 3/3 in one session, obeyed 1/3. With
   `verify` in place it has somewhere to escalate to instead of staying
   advisory.
3. **The evidence gate checks presence, not relevance.** A `verification` field
   containing "git log shows 3 commits" satisfies a claim it does not support.
   Weakest of the set — whether evidence _answers_ its claim is a semantic
   judgement a string check cannot make. One observation behind it; worth
   leaving until there is a second.
4. **`git restore <path>` / `git checkout <path>` is unguarded, and the
   don't-discard-others'-work rule is sub-agent-only.** The guardrails block the
   whole-tree forms (`git checkout .`, `git restore .`) but not the single-path
   form — the one that discards someone else's uncommitted work file by file.
   And `templates/subagent_write.md` forbids `git checkout`/`restore`/`stash`
   outright while the main agent's copy — `write_main.md` since the 2026-08-01
   split, not `write.md` — only tells it to look first, though the main agent
   has more authority and the same need. Both surfaced by a real incident: a
   concurrent hrdr session was editing a file, an unexpected `M` appeared in
   `git status`, and it was restored away on the assumption that the only other
   writer was a sub-agent. Recovered because the other session had committed it.

---

## Peer-comparison findings still open

Grouped by theme. Cross-harness agreement is noted because two harnesses
reaching the same conclusion independently is the strongest signal in the
comparison.

### Where hrdr is the outlier

| Thing                                | codex      | hermes            | opencode   | pi  | hrdr                    |
| ------------------------------------ | ---------- | ----------------- | ---------- | --- | ----------------------- |
| Per-model prompt/behaviour variation | ✅ catalog | ✅ substring list | ✅ 9 files | ✗   | **✗** (verified: none)  |
| Runtime-composed tool descriptions   | ✅         | ✅                | ✅         | ✅  | **✗** (`&'static str`)  |
| Shell commands parsed, not regexed   | ✅         | —                 | ✅         | ✗   | **✗** (15 regexes)      |
| Ask-the-user affordance              | ✅         | ✅                | ✅         | ✗   | **✗** (verified absent) |
| Repeated-call / loop detection       | —          | —                 | ✅         | ✗   | ✅ shipped 2026-07-27   |
| Deferred tool loading                | ✅         | ✅                | —          | ✗   | **✗** (~31 defs sent)   |
| Model-invocable skills               | ✅         | ✅                | ✅         | ✅  | ✅ shipped 2026-07-27   |

Rows 1 and 2 are three- and four-of-four against us — the ones where being the
outlier is most likely to be a mistake rather than a stance.

- **Parse shell commands instead of regex-matching them.** Codex:
  `shell-command/src/parse_command.rs` (82 KB) plus a Starlark `execpolicy` DSL.
  Opencode: tree-sitter (bash + PowerShell) walking command nodes, deriving
  out-of-project path arguments and an **arity-truncated** "always" prefix
  (`permission/arity.ts`) so `git checkout main -b foo` generalises to
  `git checkout *`. hrdr matches 15 hand-written regexes against the raw command
  line, with the cost admitted in-tree (_"the regex crate has no lookaround —
  `--force` must not also match `--force-with-lease`"_, plus `[^&|;]*` on every
  rule to avoid crossing a separator: a hand-rolled tokeniser spelled in regex).
  _Cost:_ a week. _Counter-argument that keeps this off the top of the list:_
  hrdr's guardrails are deliberately a **small deny list on an autonomous
  agent**, not an approval system — a parse buys precision, not a new
  capability. **Worth it only if hrdr adopts a path-scoped restriction** (`.git`
  protection's shell half, or out-of-project asks).
- **Runtime-composed tool descriptions.** Verified: `Tool::description()`
  returns `&'static str`. Three live consequences: `task`'s description cannot
  list the actually-configured models; the guardrail-message duplication (below)
  could be eliminated by construction; and the sandbox's writable roots cannot
  appear in the descriptions of the tools they constrain, which is why the
  positive declaration had to go into `SECTION_SANDBOX` instead. All four peers
  build descriptions at runtime. _Caveat:_ `&'static str` buys testability and
  cache stability — interpolating runtime values changes the schema between
  turns and invalidates the tools cache block. If the only real case is "list
  configured models in `task`", `parameters()` is already a runtime
  `serde_json::Value`: cheaper and cache-equivalent.
- **Per-model behaviour as data, not code.** Codex ships a remote catalog with
  per-model `base_instructions`, `instructions_template`, personality variables
  and `ModelMessages{approvals, auto_review, permissions}`; hermes ships an
  editable substring list plus per-family blocks carrying a dated provenance
  trail (_"Observed on DeepSeek v4-flash… returned fabricated listings"_,
  _"adapted from OpenCode's gemini.txt"_, _"Ported from cline/cline#11514"_);
  opencode selects one of nine prompt files by model-id substring. **hrdr is the
  only one of four with zero per-model variation** (verified: no model-string
  branching in `prompt.rs`). Both vendors that post-train also send **less**
  prompt to their own models — codex's guidance shrinks from gpt-5.2 to gpt-5.6,
  hermes pointedly omits Claude from its enforcement list. If hrdr ever ships
  per-model prompts: **wire them with a test that every file in the directory is
  reachable** — codex has 5 dead prompt files of 6, opencode 1 of 9 plus two
  more.
- **Deferred tool loading behind a search bridge.** Codex:
  `ToolExposure::{Direct, Deferred, DirectModelOnly, Hidden}` + BM25 search over
  withheld metadata, MCP and V1 sub-agents default to `Deferred`. Hermes:
  `tool_search`/`tool_describe`/`tool_call` bridges, **core tools never
  deferred** (_"Always-load means always-load. No exceptions."_), and the gate
  is a **no-op unless deferrable tools would exceed ~10% of the context
  window**. Both exempt core tools. hrdr sends every def every request (~31 for
  a fully-featured main agent: 17 from `ToolRegistry::with_defaults`, the rest
  from `Agent::new`). _Decisive caveat:_ that is ~4-6k tokens, usually cached.
  **Do it if MCP tool counts get large, not now.**
- **Ask-the-user affordance.** Verified absent (`grep -rn "AskUser|ask_user"` →
  0). Three of four peers have one; hrdr's autonomy posture (headless runs,
  NDJSON, cost caps) is the reason it does not, which makes this a stance — but
  an unrecorded one until now.

### Editing and tool ergonomics

- **Fuzzy `old_string` matching that preserves unchanged lines.** hrdr already
  _detects_ the class and writes a good message (_"a near-match differing only
  in whitespace/indentation exists"_) but still **fails the call**; it has a
  CRLF retry (`is_crlf_dominant`) and no trailing-whitespace or quote retry,
  while `read` clips at `MAX_LINE`, so the model's view can differ from disk in
  exactly these ways. pi retries in a normalized space (NFKC, per-line
  `trimEnd`, smart quotes → ASCII, dash/space variants → plain) and — the clever
  part — `applyReplacementsPreservingUnchangedLines` widens each replacement to
  the lines it touches, rewrites only those, and copies every other line back
  byte-for-byte, with a duplicate-line alignment guard and a line-count
  assertion. _Caveat:_ fuzzy matching in an edit tool normalizes Unicode as a
  side effect of an unrelated change. **Cheapest useful subset:** trailing
  whitespace + quotes/dashes/spaces, no NFKC, no new dependency — and **report**
  when a fuzzy match was used (pi tracks `usedFuzzyMatch` and doesn't surface
  it).
- **Per-model argument tolerance.** pi repairs tool arguments per model —
  `prepareEditArguments` re-parses `edits` when it arrives as a JSON string,
  commented _"Some models (Opus 4.6, GLM-5.1) send edits as a JSON string
  instead of an array"_. hrdr's tolerance is `serde(alias)` on **path fields
  only** (`read.rs`, `edit.rs`, `fileops.rs`); the payload fields that cost most
  when they fail — `old_string`/`new_string`/`content`/`pattern` — have none.
- **Expose session/model metadata to shell commands.** Verified:
  `Shell::command` configures program + args only, nothing else. pi injects
  `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`,
  `PI_REASONING_LEVEL` (deleting inherited copies first) and tells the model
  they exist. _Caveat:_ widens what leaks into every subprocess and its logs —
  pi's `exposeSessionEnvironment` toggle is the right shape, defaulting **off**.
- **Truncation caps are 10×/40× tighter than opencode's.** Verified
  `DEFAULT_MAX_OUTPUT = 5_120` / `DEFAULT_MAX_OUTPUT_LINES = 50` against
  opencode's 50 KB / 2000 lines. Both spill to a re-readable file so nothing is
  lost, but 50 lines means a `cargo test` failure or a 60-line diff costs a
  second round trip opencode wouldn't pay. **Not measured in hrdr's traces —
  worth one experiment**, not a change.

### Permissions, isolation, and state

**Instruction surfaces a repo can write.** From the 2026-07-29 sub-agent
attack-surface audit (traced against codex `81da9de`). The entry point is now
gated: hrdr asks before it trusts a working directory (`hrdr_agent::trust`), an
untrusted one opens jailed and loads neither file, and `AGENTS.md` is read from
the working directory only, so nothing is inherited from an ancestor the user
never answered for. What the gate does **not** answer is what a trusted
directory's files may then do:

- **Project `AGENTS.md` is writable by a write sub-agent** and read back as
  project conventions on the parent's next prompt rebuild (`/clear`, `set_cwd`,
  a new agent). The trust answer is given once, at open, so a sub-agent editing
  the file mid-session changes what the parent reads next without being asked
  again. Left open on purpose: `AGENTS.md` is also how a project legitimately
  carries instructions, and narrowing it costs that. A `// NOTE:` sits on the
  push site in `build_system_prompt_sections`. Closing it would mean the
  `memory`-tool treatment (main agent only), which that tool could afford
  because it had no second use.
- **Project skills shadow built-ins by name.** `skill_dirs` includes
  `cwd/.hrdr/skills`, `cwd/.claude/commands`, `cwd/.opencode/command`, all under
  the writable cwd; project files are discovered _before_ built-ins and win,
  with `model_invocable` defaulting true, so `.hrdr/skills/commit.md` silently
  replaces the vetted `:commit`. Re-runs on every `set_cwd`/`clear` and in every
  new `Agent::new`. Same shape as `AGENTS.md` but with a **weaker second use** —
  a project skill is a convenience where `AGENTS.md` is a core feature — which
  makes it the stronger candidate of the two if either is closed.
- **A jailed session cannot `/cwd` into another untrusted directory.** The hole
  is closed — `/cwd` refuses a directory nobody has answered for, so a trusted
  session can no longer walk into a fresh checkout and read its `AGENTS.md` with
  the tool set the first directory earned. The refusal is unconditional, which
  is stricter than it needs to be for a session that is _already_ jailed: it
  loads no project instructions wherever it stands, so moving between untrusted
  repos could safely be allowed for an audit session. Left strict on purpose —
  one rule is easier to state than one with an exception — but if auditing
  across repos becomes real work, this is the exception to add.
- **There is no way to answer the trust question mid-session.** The menu needs
  the terminal, which the TUI owns once it starts, so the only answer available
  to `/cwd` is "no": a user who wants to move into a new directory has to start
  hrdr there. Closing it means a TUI modal on the `CommandHost` seam
  (`begin_*`), which is a bigger change than the gate itself and wants the
  `CommandHost` default-bodies question settled first.

**Two smaller confinement gaps, verified and unchanged:**

- **`std::env::temp_dir()` is granted whole** in `write` mode, not just
  `session_scratch_dir()`. Broader than the stated need, and pre-existing.
- **`shell` runs unconfined where no OS backend exists** — Windows always, Linux
  without Landlock. The file tools stay guarded and `NO_OS_SANDBOX_NOTICE`
  fires, so it is admitted rather than silent.

- **One permission evaluator instead of four unrelated mechanisms.** Opencode
  has a single primitive — an ordered `{permission, pattern, action}` list
  evaluated `findLast`-wins with globbing on both fields — and gets plan mode,
  sub-agent restriction, read-only agents, out-of-project confinement, `.env`
  gating, loop detection and headless mode out of it as **data**. hrdr now has
  **four** mechanisms that don't compose: `guardrails` (shell only, terminal
  `bail!`), `read_only` (registry name filter), per-tool secret-file `bail!`s
  (deliberately not shared), and — since 2026-07-27 — the sandbox path guard.
  Adding a fifth restriction means writing a fifth mechanism. _Caveat:_
  opencode's three actions are `allow|ask|deny`; hrdr's autonomy posture
  collapses that to `allow|deny`, and a two-valued evaluator over globs is worth
  much less. **Honest MVP:** keep the mechanisms, express _what they check_ as
  one rule list. A refactor, not a feature — wait until hrdr wants a second
  path-scoped restriction (`.git` protection is exactly that trigger).
- **Out-of-project access as an observable event, not a removed capability.**
  Opencode's `external-directory.ts` raises an `external_directory` permission
  keyed on the containing **directory glob**, with an allow-list pre-seeded from
  the overflow dir, temp, skill dirs and reference dirs, called from `read`,
  `edit`, `write`, `glob`, `grep`, `lsp`, `shell` and `apply_patch`. hrdr
  removed cwd confinement outright (`f0d903a`) and the sandbox has since closed
  the **write** half — reads stay broad by decision, so this is now specifically
  about _read_ visibility. _Caveat:_ hrdr's write sub-agents legitimately read
  the parent repo (shared `Cargo.lock`, `~/.cargo`, `/usr/lib`), so the
  allow-list may need to be large enough that the signal isn't worth it.
- **Per-call permission escalation.** Still open, and note that hrdr **built and
  then deleted** an approval gate on 2026-07-30 — the mechanism existed for
  widening the _sandbox_, whose motivating failure (bwrap's user namespace
  breaking ssh) was removed rather than routed around. This item is the
  different, narrower question of overriding a **guardrail**, and it survives
  that deletion. Anyone rebuilding a gate should read why the last one went: it
  only ever helped when a human was present to answer, and a human who is
  present can run the command themselves (`!command`). Codex's shell schemas
  carry `sandbox_permissions`, `justification`, `prefix_rule` and
  `additional_permissions`, with outcomes `ApprovedForSession` and
  `ApprovedExecpolicyAmendment` (the latter appending a durable rule to
  `$CODEX_HOME/rules/default.rules`). hrdr's guardrails are terminal `bail!`
  with no approval path, so when a guardrail is wrong — `git add -A` in a repo
  the user genuinely wants fully staged — the agent can only give up or work
  around the regex. _Caveat:_ interactive approval cuts against hrdr's posture.
  Smaller defensible version: keep guardrails terminal, allow an explicit
  `override_guardrail: "<rule>"` argument that logs loudly and is refused unless
  config opted that rule into overridable.
- **A channel to tell the model that something changed.** hrdr has no way to say
  "a tool appeared, the cwd moved, memory was written" — the only path is a
  prompt rewrite, and `refresh_system` fires on MCP connect, `clear()` and
  `set_cwd()` only. Codex decomposes mutable state into ten typed sections and
  emits a **developer-role fragment containing only the delta** per sampling
  step, byte-budgeted, in stable XML markers, advanced by RFC 7386 merge
  patches. Hermes goes the opposite way (freeze, rebuild at compaction) and
  **given hrdr's small volatile set, hermes' posture is the cheaper correct
  answer** — the memory half of it already shipped. If the honest list is
  "memory changed, AGENTS.md changed", one appended `# Context update` developer
  message gets most of the value. Do the cheap version first.
- **Memory: usage tracking and an external-drift guard.** Two halves; the drift
  half shipped, usage tracking is open. Codex tracks whether memories are _used_
  (citation blocks, plus parsing the model's shell commands for reads of memory
  paths) and feeds `usage_count`/`last_usage` into pruning — hrdr's
  `read`/`grep` calls under the memory dir are directly observable with no
  parsing needed. _Caveat:_ usage count is a bad proxy for memories whose value
  is preventing a mistake — hrdr's own `no-migration-pre-1.0` earns its keep by
  being _injected_, never read, so counting reads would prune the most valuable
  first. Separately, hermes' `_detect_external_drift` refuses a full-file
  rewrite when on-disk content wouldn't round-trip through the tool's own parser
  (manual edit, sibling session), backing up to `.bak.<ts>` instead of
  clobbering — **shipped `5bc2e5d`**: `backup_if_drifted` in `memory.rs` copies
  a drifted file to `<slug>.<ts>.bak` before the write/edit rewrite, refuses the
  rewrite when the backup cannot be written, and the result line names the
  backup. Both fold into the memory-drift item below.
- **Per-provider tool-JSON-schema rewriting — file, don't build.** Opencode
  rewrites every tool schema per model before the wire, with three quirks and a
  reason each: OpenAI/Azure sanitisation; Moonshot/Kimi strip every sibling key
  of a `$ref` (_"Moonshot expands `$ref` before validation and rejects sibling
  keywords"_) and collapse tuple-style `items`; Gemini converts integer enums to
  string enums. hrdr ships one schema shape to every provider (verified: no
  `sanitize`/`$ref`/`additionalProperties` handling in `hrdr-llm`) and targets a
  **wider** provider spread. _Caveat, strongest in this section:_ **there is no
  evidence hrdr is broken on any provider.** This is a known-good design for
  when a provider rejects a schema, not work.

### Observability

- **Session search.** Verified: sessions live at
  `sessions/<cwd-slug>/<name-slug>.json`, zstd-compressed once idle, with
  `list_sessions()` but no index — so cross-project recall means walking every
  slug directory and decompressing every archive. "What did we decide about the
  delegation retry backoff three weeks ago?" is unanswerable. Hermes: FTS5,
  three modes inferred from args, **zero LLM calls**. Two specifics worth
  copying if built: **exclude sub-agent sessions from results** (hermes hides
  `("subagent","tool")` sources; hrdr's on-disk sub-agent runs are the exact
  analog and would flood every query), and **demote rather than exclude**
  automated sources, because repetitive vocabulary dominates bare BM25 and
  starves interactive sessions. _Honest smaller version:_ grep the current
  project's slug directory, decompressing lazily. No FTS engine, most of the
  value.
- **Prompt introspection — leverage on every size claim in this file.** Verified
  absent (`grep -rn "prompt-size|context_breakdown"` → 0). Both the codex and
  hermes passes closed with the same admission: neither binary was instrumented,
  so **every size comparison in the old comparison doc was structural, not
  measured**, and hrdr's prompt had to be reconstructed in Python to be counted.
  Hermes ships both halves: a live per-category budget (system prompt, tool
  definitions, rules, skills index, **MCP separately from builtin schemas**,
  sub-agent definitions, memory, conversation) preferring the provider's
  measured `last_prompt_tokens` over its own estimate; and `hermes prompt-size`,
  which builds a real offline agent with dummy credentials so the numbers match
  the wire. hrdr has the estimators and a context gauge but no category
  attribution and no way to dump the assembled prompt. _Caveat:_ char/4 invites
  false precision — report bytes and labelled estimates, resist a
  percentage-of-window pie chart.

---

## Sandbox follow-ups

**Most of this list closed on 2026-07-30 with the sandbox redesign** (nine
slices, `5c9f675`..`c114a6a`; `docs/sandbox-redesign.md` is the decision record
— what the deletions taught is under
[Cleared in the 2026-07-30 sandbox redesign](#cleared-in-the-2026-07-30-sandbox-redesign)).
What remains open:

- **Curated read allow-list for `write` mode** — still the honest gap, but the
  shape changed. A `shell` command in `write` mode can still
  `cat ~/.ssh/id_rsa`. What did land is narrower and real: `shell` output now
  drops lines naming a credential file AND redacts the hunk body of a diff
  touching one (`DiffRedactor`), so the _accidental_ leak — a broad `rg`, a
  `git diff` of `.env` — no longer reaches the transcript. Deliberate
  exfiltration is untouched and admits it.
- **No shell-command pre-flight** — still open, and cheaper than it was: the
  EROFS note now names the remedy (`sandbox_writable_roots`,
  `--sandbox-writable-root`, `!command`), which was most of what a pre-flight
  would have said.
- **Windows `write` mode has no OS confinement** (`read` mode is confined by
  Mandatory Integrity Control since 2026-08-02; `write` still takes
  `NO_OS_SANDBOX_NOTICE`), and closing it costs something the other two backends
  do not. A Low-integrity child can only write to objects _labelled_ Low, so
  each writable root would have to be relabelled
  (`icacls <root> /setintegritylevel Low`) and reverted after. Two consequences,
  both real: the label **persists** if hrdr dies between spawn and revert, and
  while it is set **any other Low-integrity process can write there** — a
  sandboxed browser renderer, say. Landlock and Seatbelt leave no trace at all.
  Needs a decision before it touches a user's repository, not just an
  implementation.

Opened by the redesign:

- **The sub-agent `<stem>.json` snapshot is written and never read.** It existed
  for `task_revive`, which is gone. Its only reader now is its own test
  (`background_subagent_persists_its_own_session_state`). Deleting it removes a
  per-turn-boundary write; keeping it keeps a crash-durable record of what a
  sub-agent actually said. Decide, rather than leaving a file nobody loads — and
  note that the panes read the sibling `.jsonl`, not this.
- **`--sandbox jail` cannot apply to a write-capable session**, so it floors at
  `write` and emits a notice pointing at the `prisoner` agent. That is honest
  but it is not what the word means. A jailed _main_ agent is coherent (five
  tools, no shell — an audit session), and the write floor exists for sub-agents
  that must write. Worth revisiting as "session jail means jail, and a
  write-capable sub-agent under it is the thing that floors".
- **`jail` loses `git log` on the audited repo** — real provenance value, and
  the accepted cost of having no subprocess. Argues for a narrow read-only git
  capability later, not a general shell.
- **Package caches are writable and shared across projects.** Content-addressed
  and integrity-checked, which is what makes it acceptable, but poisoning
  `~/.cargo/registry` affects builds the user later runs by hand. Revisit if a
  per-project cache overlay ever gets cheap.

**Where codex's sandbox has moved past hrdr's** (context, not separate items):
its policy is a precedence-ordered entry list of
`{path, access, missing_path_behavior}` with `Read|Write|Deny`,
most-specific-wins and **deny beats write beats read**, paths as globs or
symbolic tokens
(`Root, Minimal, ProjectRoots{subpath}, Tmpdir, SlashTmp, Unknown` — `Unknown`
retained so newer config degrades to warn-and-ignore); named permission profiles
with `extends` inheritance versus hrdr's flat four-value `SandboxMode`; and a
mode hrdr has no slot for, `PermissionProfile::External { network }` —
"filesystem isolation is enforced by an external caller", which hrdr's
`SandboxMode::None` conflates with "unsandboxed".

---

## Tooling / agent capability

- **Trust can be granted and never revoked.** `hrdr_agent::trust` has
  `is_trusted` and `trust`; there is no `untrust`, no listing, and no command
  that reaches either. Undoing an answer means finding
  `$XDG_CACHE_HOME/hrdr/trusted-dirs` and editing it by hand — which is fine for
  the owner and is not a thing a user can be expected to discover. The asymmetry
  is deliberate as far as it goes (only the yes is stored, so declining needs no
  undo), but a directory whose provenance turns out to be wrong is exactly the
  case the gate exists for. Smallest honest version: a `/trust` command that
  prints the store and takes `revoke [path]`, defaulting to the current
  directory. Needs a decision about whether revoking mid-session also has to
  downgrade the running session, which is the same terminal-ownership problem as
  answering the question mid-session.
- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap because
  `rebuild_index` regenerates the pointer index on every mutation (verified), so
  files and index cannot drift **structurally**; what is missing is semantic
  staleness plus the usage-tracking half recorded above. The external-drift half
  shipped 2026-08-05 (`5bc2e5d`) — see the memory item in
  [Permissions, isolation, and state](#permissions-isolation-and-state).
- **Profile-faithful revive** — the residual the above left. Only _capability_
  is persisted, not the profile: a revived run does not get its original persona
  (`agent_prompt`) or explicit `tools:` allow-list back, because neither was
  ever persisted. Restoring them means persisting the profile name and
  re-resolving it at revive time, which is a larger change than the capability
  fix and a question about what "revive" means — the same run, or the same
  _agent_. **Needs a call.**
- **Skills follow-ups** (feature shipped 2026-07-27 — `hrdr-agent/src/skills.rs`
  plus `prompt::skills_section`). Left out on purpose: no `skill` usage signal
  (nothing records whether the model ever loads one, so there is no evidence for
  or against the listing's wording); no categories, unlike hermes'
  category→skills grouping, which only pays off past a few dozen skills; and a
  body still arrives as one tool result, so a procedure over
  `SKILL_OUTPUT_MAX_BYTES` (24 KiB) spills to a file the model must read.

---

## Consistency / robustness

- **Guardrail rules live in two places** — and now a test says so when they
  drift. The rule set is still encoded both in `hrdr-tools/src/guardrails.rs`
  (mechanical enforcement) and in the prompt fragments (guidance telling the
  model not to attempt them), which is deliberate: the prompt phrasing is more
  nuanced than the terse guardrail message, so it is written, not derived. What
  shipped 2026-07-27 (`37edba4`) is the drift check —
  `every_guardrail_is_explained_in_the_prompt` pairs each rule with the token(s)
  the rendered write+shell prompt must contain, positionally, so a 16th
  guardrail fails the test until whoever added it writes the guidance too (or
  records that the rule needs none). Two notes from building it: the guidance is
  spread across `base.md` (pipe-to-shell) and `shell.md` (interactive git), not
  only `write.md`; and the two pipe rules share one identical message string, so
  the table is keyed positionally rather than by message. Eliminating the
  duplication itself still wants runtime-composed descriptions.
- **The pipe-to-shell guardrail's recovery text assumes POSIX.** Verified
  nuance: there are **two** pipe rules — a POSIX `curl|wget … | sh` one and a
  case-insensitive PowerShell-shaped `iwr|iex` one — so coverage is not
  POSIX-only. What is POSIX-only is the recovery example
  (`curl -fsSL <url> -o <tmp>/script.sh`), built in `guardrails.rs` outside the
  `Shell` seam because `default_guardrails()` has no shell in scope. Correct
  today (the shell is always bash/sh); it is the one place a new dialect would
  have to be threaded through by hand.
- **hrdr sets no Windows ACL on any file it writes.** The residual of the
  Windows-drift audit pass, which **ran** and landed three fixes (`8e5bc9d`):
  the credential `sync_all` gated on unix though portable, `atomic_write`'s
  symlink guard likewise, and owner-only file creation re-decided at four sites.
  All ~130 `cfg` gates were classified; ~25 are `#[cfg(unix)]` on _tests_
  (needing bash, python3 or symlinks) and are not findings, and `proc.rs`, the
  pid-liveness probes and `prompt.rs`'s package-manager names are deliberate and
  documented. The guarantee left on Windows is the containing per-user
  directory's inherited default, stated once on `hrdr_llm::owner_only_options`.
  Per-user ACLs need a new dependency in `hrdr-llm` and are a deliberate
  non-goal until someone runs hrdr on Windows in anger.
- **`O_NOFOLLOW` covers only the final path component.** A symlinked _parent_
  directory is still traversed on the wire-log open, and there is no Windows
  equivalent at all, so callers relying on it keep their own preflight check.
  Recorded on `owner_only_options_no_follow`; closing it properly means
  resolving the whole path under a directory handle.
- **`CommandHost`'s text-fallback defaults are now test-only.** Found in the
  2026-08-02 web-removal sweep, not fixed — the parent's call, and the
  Standing-constraints host seam is deliberately kept. `begin_login`,
  `begin_model_selector`, `begin_model_selector_for`, `begin_session_selector`,
  `begin_skill_selector`, `begin_effort_selector` and `begin_theme_selector` all
  carry default bodies that list the choices as text "for a frontend without
  modal support" — that frontend was `hrdr-web`. `TuiHost`
  (`hrdr-tui/src/app/commands.rs`) overrides every one of them, so the only
  callers left are the test hosts, `TestHost` (`commands/dispatch.rs`) and
  `RouteTestHost` (`login.rs`). Same shape for `supports_command`: the trait
  default is `true`, `TuiHost` returns `true` with the comment "the TUI
  implements the full registry", and `help_body_for`'s `show` closure therefore
  never filters anything in production — nor in tests, which never pass a
  closure that returns `false`. Deleting the defaults would force the test hosts
  to spell out the no-ops; keeping them keeps a documented seam. Worth a
  decision either way rather than drifting.

---

## Test coverage gaps

- **`watch` against a real GitHub run: manual smoke never ran.** The watch-tool
  plan's last item (watch an actual tag-run CI run through `gh`) needs a live
  GitHub run, so it was not runnable in the automated suite; every automated
  slice shipped (the poller lifecycle, guardrails, cancel, delivery, schema
  bounds, prompt rewiring). Run it before trusting the release flow's watch step
  on a real run.

- **DeepSeek built-in provider: manual smoke slice never ran.** The provider
  plan's slice 7 (single-turn + agentic tool-call turn against the real API)
  needs a live `DEEPSEEK_API_KEY` and was not runnable in the automated suite;
  every other slice shipped. Run it before trusting a real DeepSeek session.

- **No end-to-end test consumes a real `Retry-After`.** hrdr-agent's `MockResp`
  has the variants `Sse`, `HttpError` and `HttpErrorBody`, and none can set a
  response header, so the agent's retry loop honouring a server-named delay is
  unexercised. `retry_after_from_headers` is covered directly instead. Closing
  it means another `MockResp` variant carrying headers.
- **The warm models.dev catalog path is uncovered, deliberately.**
  `catalog::load_cached` reads process-global state (`HRDR_MODELS_PATH` / the
  XDG cache dir), so warming it in a test leaks into every other test in the
  binary. The cold path and the `ANTHROPIC_MAX_TOKENS` 8192 fallback are pinned;
  the warm resolution rules are covered separately in `catalog`'s own tests.
  Recorded so nobody "closes" it with a test that only appears to work.
- **`no_color_turns_it_off_on_a_terminal` cannot fail on hrdr's own check.** Two
  layers deliver the behaviour it asserts: `colour_stderr` in `apps/hrdr`, and
  crossterm, which suppresses colour under `NO_COLOR` by itself
  (`style/types/colored.rs`). Forcing hrdr's decision permanently on leaves the
  test green — verified by doing it. `term_dumb_turns_it_off_on_a_terminal` is
  the one that goes red, so the check is guarded; what is unguarded is the
  `NO_COLOR` arm of it specifically. Both tests are kept and the limitation is
  written on the test itself, because what the user cares about is that the
  variable works, not which layer honoured it. Closing it means asserting on
  `colour_stderr` directly, which is a private fn in a binary crate.
- **`the_question_is_painted_in_the_theme_from_config` (trust_pty) fails under a
  `NO_COLOR` environment.** The binary suppresses colour (crossterm does it
  itself), so the assertion that the trust prompt carries the theme's RGB escape
  goes red when the test process inherits `NO_COLOR` — found 2026-08-05, the
  inverse sibling of the entry above. CI is unaffected (no `NO_COLOR` there); a
  local run with `NO_COLOR=1` exports must unset it for the suite. Closing it
  means asserting on something other than the raw escape stream, or the test
  clearing `NO_COLOR` for the child it spawns.
- **`serve_once` takes `&'static str`**, so every mock SSE body must be a
  literal (the stop-reason tests `Box::leak` theirs). Fine today; it makes a
  table-driven stream test awkward. `impl Into<String>` is a one-line change
  when someone needs it.
- **The suite leaks `tempfile` dirs into `/tmp`, and nothing guards it.**
  Measured 2026-08-02 on the owner's machine: 3,643 `/tmp/.tmp*` directories
  holding 771 MB, the oldest dated 2026-07-15 and 795 of them created that day
  alone by ordinary `cargo test` runs. Each holds real session state —
  `hrdr/history`, `hrdr/sessions/<cwd-slug>/session.json` — so something wrote
  through `XDG_DATA_HOME` while it pointed at a `tempfile::tempdir()` that had
  already been dropped, or was still writing as `remove_dir_all` walked it. Not
  the `hrdr-test-support` ctor sandbox: that one is named
  `hrdr-test-sandbox-<pid>`, its `#[ctor::dtor]` fires, and zero of those were
  on disk after the same runs. The suspects are the helpers that swap
  `XDG_DATA_HOME` for the duration of a closure — `with_test_env` in
  `session.rs` and in `hrdr-app/src/sessions.rs`, and the e2e harness's own root
  in `hrdr-tui/src/app/e2e.rs` — all of which drop the `TempDir` while
  background savers may still hold the old path. `leak_guard.rs` cannot see
  this: it asserts on a sentinel `$HOME`, and these land in `temp_dir()`
  instead. Closing it means both a fix and a guard that counts `/tmp` entries
  across a suite run, or the leak silently returns.

---

## Small corrections owed in hrdr-llm

Raised while closing the provider-divergence audit, out of those slices' scope,
each re-verified against the tree.

- **The `UNNAMED_MODEL` docstring tells half the story.** It states that putting
  the sentinel on the wire "cannot succeed anywhere it is actually read", then
  says the _OpenAI-shaped_ builder omits the field. Correctly scoped as far as
  it goes, but it never says the two native builders emit it verbatim — and this
  docstring is where a reader would go to check the invariant. Add the caveat,
  or fold it into the early-error decision below.
- **A `thinking_delta` for an index with no open block is silently dropped.**
  `map_event`'s `thinking_slot.get_mut` no-ops, which looks right; the point is
  that the neighbouring `input_json_delta` path carries an explicit note about
  why it must not default to slot 0, and the thinking path's equivalent choice
  is unexplained. A comment, not a fix.
- **`parse_imf_fixdate` ignores the weekday.** It splits on `", "` and discards
  the prefix, so `Xyz, 06 Nov 2999 …` parses fine. Laxer than RFC 7231 and
  harmless — the weekday is redundant with the date — but worth knowing before
  someone "fixes" a test that relies on it.

---

## Provider divergences left open by decision

Each is pinned by a test and commented in the source; what is missing is a
decision, not work. Filed here rather than under the closed audit below, because
a reader scanning for open items would not look inside a section labelled
closed.

- **408/522/524 are retryable only because `classify_status` says so.**
  `is_transient`'s text fallback has needles for the other six transient
  statuses and none for these, so those three arms are the only thing making a
  Cloudflare origin timeout retryable rather than fatal. A test derives the
  unprotected set from real behaviour, so both deleting an arm and quietly
  adding a needle fail loudly. Giving them needles is a behaviour change nobody
  has asked for — decide, don't drift.
- **Every mid-stream error path hardcodes `retry_after: None`** (`client.rs`,
  `anthropic.rs`, `codex.rs`), and `retry_after_hint` reads a typed error's
  field directly — so a rate limit delivered _mid-stream_ never has its
  requested delay honoured on any backend. Only the HTTP-status path
  (`error_from_response`) does. Asserted and commented, not fixed.
- **`UNNAMED_MODEL` reaches the wire literally on both native backends**,
  because `wire_model` runs after their early returns. Pinned as a known
  limitation. Erroring early is worth doing, but at provider-selection time in
  hrdr-agent — not in `chat_stream`, where it would fire once per turn and where
  a wrong error kind would make the retry loop spin on a permanent config error.
- **An unrecognized Anthropic `stop_reason` still reports
  `truncated() == false`.** Deliberate: it now raises a notice naming the value
  (`1871631`), and guessing a direction is wrong either way — folding to `stop`
  hides a truncation, folding to `length` calls a refusal truncated. Revisit
  only if a real reason turns up that wants a fourth answer.

---

## Provider-divergence audit — closed 2026-08-02

Ten findings closed across six commits (`c5a6019`, `49feedd`, `5d87db5`,
`2363e6b`, `d602b7d`, `1871631`). `hrdr-llm` went from 215 to 244 tests. What is
kept is the part that shapes future work; the open residue is in the three
sections above.

- **Host-keyed backend detection is why the native paths were untestable.**
  `detect_backend` reads the host, so every mock on `127.0.0.1` is
  `Backend::OpenAi`, and `Client::chat_stream` never dispatched to
  `anthropic`/`codex` — leaving both backends' post-loop stream assembly
  unreachable. A `#[cfg(test)] Client::set_backend_for_test` fixes it for one
  client instance with no statics to race through. Reach for it rather than
  re-deriving the problem.
- **The client-warning slot is process-global**, and a test asserting on it will
  catch foreign writes — `apply_extra_headers` warns once per process when it
  drops an auth header, which is exactly what made the first run of the
  no-warning test fail. The stop-reason tests serialize on a mutex and burn that
  latch first. Anything new asserting on `take_client_warning` needs the same
  care.
- **Not a coverage gap, do not re-derive:** `Delta` deserializes only
  `reasoning_content`, so providers streaming `delta.reasoning` (several
  OpenAI-compatible gateways) have their reasoning silently dropped. Missing
  feature, not a missing test.
- **Never audited at all:** `sse.rs`, `capped_read.rs`, `fs.rs`, most of
  `catalog.rs`; hrdr-tui / hrdr-tools / hrdr-app entirely.
- **`duration_constant_names.rs` walks `crates/` on disk, not
  `[workspace] members`.** Its `rust_sources` recurses `crates` and `apps`,
  skipping only `target`, so a directory left behind by a removed crate is
  scanned. The leftover that prompted this (`crates/hrdr-ui/`, build output the
  crate's deletion could not take with it) has since been removed from disk, so
  nothing is mis-scanned today — but the rule's scope is still "whatever is on
  disk", unlike `every_test_binary_is_sandboxed.rs`, which parses `members` and
  documents why. Verified by reading both, not by a failing run.

---

## Left open by the compaction fix (`/compact` pane targeting)

The data-loss half is fixed: the TUI now summarizes the conversation on screen.
Two residuals, both from the same root — the turn **handle** slot is
session-wide while the compaction is now per-pane:

- **The TUI can no longer Esc-cancel a sub-pane compaction.** `in_flight`,
  `is_busy` and `cancel_turn` are all keyed to `MAIN_KEY`, so with the clock now
  on the sub key they see nothing in flight. Before the fix Esc did cancel it —
  while also stopping main's clock, which was the defect. Needs a decision on
  per-pane cancellation, not a patch.
- **The TUI's compaction result line lands in main's transcript** (`push_entry`
  is main-scoped), so "compacted: N → M" appears in the main conversation even
  when a sub-agent was summarized. TUI system lines are main-scoped generally;
  changing that is a wider call.

---

## dispatch.rs review — findings 2026-08-02

`hrdr-app/src/commands/dispatch.rs` (946 lines) was read in full for the first
time, with its six sibling modules, the `CommandHost` trait and its call site.
Ten tests exist, covering `/add` and `/copy msg`; the other ~30 commands have
none. Findings below are traced from source — nothing was reproduced at runtime.

**`/cwd` writes one agent and reads another.** `dispatch.rs` resolves the
argument against `host.cwd()` and writes `host.agent()`, and those are different
agents (`cwd()` is main-derived, `agent()` is the active pane). On a sub-agent
pane, a relative `/cwd` resolves against main's cwd, moves only the sub-agent,
then repoints the global chrome as if main had moved. A bare `/cwd` afterwards
contradicts the status bar. `/status` has the same split — main's cwd beside the
active agent's message count. Needs a call: derive the base from `host.agent()`,
or add `active_cwd()` so both halves name one agent.

**Canonical names are case-sensitive, aliases are not.** `resolve_alias`
lowercases only for its match arms and returns the original on fall-through, so
`/CD`, `/RESET`, `/Clear` work while `/CWD`, `/Status`, `/Model`, `/Help` answer
"unknown command". Two spellings of one command behave oppositely. Decide which
way, then pin it — neither side has a test, so any fix could regress silently.

Smaller, each verified: `/copy msg 1-99999999999999` labels the range it
_requested_ rather than what it copied (the existing bounded-scan test uses
exactly that input; still open, cosmetic). The rest of this paragraph's items —
`/export`'s format/overwrite/token handling, `/effort` applying a level,
`/login`/`/skills` arguing, `/doctor`'s pre-spawn probes — were fixed in
`be0f340` (Record: closed efforts).

**Checked and fine, so nobody re-derives it:** the `bool` contract is sound —
every in-arm return is `true`, only the unknown arm and the non-`/` guard return
`false`, so nothing swallows input or leaks a command. No byte-offset `&str`
slicing, no indexing, no `unwrap`/`expect` outside tests. `/etc/hosts` correctly
falls through to the model. `parse_msg_range` rejects the degenerate ranges. The
`/add` size cap that looks duplicated is not — `read_attach_file` gates on
`metadata().len()`, which is `0` for procfs, so the post-read check is the real
backstop. No arm reads state, awaits, then writes back stale. `dispatch` is
synchronous and holds no lock across an await.

**Coverage worth adding first**, in order of how bad a silent regression is: a
table asserting `dispatch` returns `true` for every name in `SLASH_COMMANDS` and
`false` only for unknown input (catches an arm being deleted, or a
`return false` slipping into a handler); the busy guards firing at all — they
are the "check that cannot fail" shape and nothing observes them today.

**Not covered by this pass:** no runtime exercise. The concurrency claims are
reasoned from lock scopes, not from a racing repro. TUI modal/picker key routing
was judged only at its `begin_*` entry points. `login.rs`, `skills.rs`,
`completion.rs` and `sessions.rs` were read only where dispatch calls into them.
`open_system_handler`'s Windows and macOS arms are `#[cfg]`- gated and were not
compiled.

---

## Review coverage still owed

The 2026-08-01 pass (see
[Cleared in the 2026-08-01 review pass](#cleared-in-the-2026-08-01-review-pass))
closed every finding it raised, but it did not read everything. What it never
opened, so nobody records it as reviewed:

- **`hrdr-tui/src/ui.rs` block rendering** and **`app/commands.rs`**. Only the
  mouse/selection path (`e1b3023`) and the scroll/highlight math were read.
- **Twelve `hrdr-tools` files**: `find`, `ls`, `secret_diff`, `mutation`,
  `todo`, `tree`, `verify`, `memory`, `verification`, `ansi`, `test_nudge`,
  `lsp`. The 2026-07-31 pass listed them as a gap and the 2026-08-01 pass
  covered `gate`, `hooks`, `web`, `replace` and `mcp/client` instead.

## Known behaviour to revisit

Not bugs; things whose surprise is worth having written down.

- **A panicking tool now waits for its batch siblings before the turn ends.**
  Since the tool-batch dispatch change (2026-08-04, `run_tool_batch` spawns each
  call as its own task and resumes the panic after `join_all`), a panicking
  tool's `panicked` outcome is delivered only once every other call in the same
  batch has finished — an inline panic used to abort the siblings immediately.
  All tested observables (immediate panics, ordering, streaming, timeouts,
  cancel) are unchanged; the only surprise is the delay when a panicking call
  runs beside a long sibling (e.g. a shell at its five-minute timeout). A
  `FuturesUnordered` join that aborts siblings on the first panic would restore
  the old immediacy; deferred as a rework of the riskiest slice for an edge
  case.

- **A profile can allow-list itself into having no search tool.** `read_only`
  keeps a shell on purpose, and `jail` keeps `grep`/`find`/`ls`/`tree`; but an
  `allowed_tools` list naming neither leaves an agent that can only `read` by
  exact path. Nothing validates that at load, and no prompt section can name a
  search tool it does not hold — `base.md` says "whichever search tool you hold"
  precisely because there may be none. Worth deciding whether such a profile
  should be refused at load rather than shipped blind.
- **`write.md` is the largest always-on prompt** after the git/release split,
  and most of its rules carry a shouted ALL-CAPS header. Emphasis that dense
  stops selecting. Neither is a defect; both are what is left of "the always-on
  prompt is too long".

- **A mid-stream retry can double the text the user already saw.**
  `drain_stream` forwards `AgentEvent::Text` to the frontend as it arrives, so a
  stream that fails after emitting some output and is then retried — by
  `RetryBudget` on a transient error, or now by `recover_context_overflow` on a
  drain-time `context_length_exceeded` (PR #24) — re-streams the model's reply
  from the start, and the frontend has no signal to discard the first partial.
  Not new and not observed in practice: Codex reports overflow as a
  `response.failed` event before any content. Noted because the overflow path
  widened the set of errors that reach the retry, and because the fix (a
  "discard what I streamed for this round" event) is a protocol change, not a
  local one.
- **Building a sandbox policy touches the parent repo's `.git`.**
  `git_metadata_roots` `create_dir_all`s `refs/heads/hrdr` and
  `logs/refs/heads/hrdr` so they exist to be canonicalized and bind-mounted — so
  `Agent::new` with a linked-worktree cwd creates those two dirs in the
  **parent** `.git` at construction time. Harmless (git ignores empty ref dirs),
  but constructing an agent is not read-only with respect to the repo.
- **A commit from a linked worktree can print a `packed-refs.lock` EROFS line.**
  Ref maintenance triggered by a commit inside a confined worktree may fail to
  create `<parent>/.git/packed-refs.lock` while the commit itself lands and
  exits 0. Observed during bring-up, asserted by no test — treat it as possible,
  not guaranteed, and **do not widen the roots to silence it**.
- **Input-path unification UX.** Since "every user message is a queued `Steer`",
  a submitted message renders when its `Steered` event is pumped (a beat after
  submit) rather than synchronously, matching sub-agent behaviour. Intended and
  imperceptible with a fast pump; if it ever reads as laggy, pump the opener
  synchronously.
- **tok/s excludes tool time.** The generating marker divides streamed tokens by
  _model working time_: `infer_elapsed()` pauses while `tools_running > 0`, and
  the loader is hidden entirely during a tool call. By design — it reports model
  speed, not wall-clock throughput. Showing "running tool…" instead of hiding
  the loader, or tracking wall-clock throughput separately, is a feature, not a
  fix.

---

## Considered and declined

Recorded so the next audit does not re-litigate them — if you disagree, argue
with the reason, don't re-file the finding.

- **Batched `edits[]` on the `edit` tool — declined 2026-07-26.** The design was
  worked through (flat `edits: [{path, old_string, new_string}]`, anchors
  resolved against as-read content, two-phase all-or-nothing) and rejected on
  cost/benefit: single edits are what models handle best; with prompt caching
  the marginal cost of a second edit call is its own args plus the trimmed
  result, so the batch's real saving is round-trip latency — not worth the
  validation/overlap/error-reporting machinery, which is its own bug surface.
  The failure-retry cost that motivated it was fixed at the root instead
  (formatter-aware staleness + apply-anyway, `da714e1`). Two constraints on any
  revival: tool args must be object-rooted, so a bare array can never be the
  schema; and pi's version is worth reading first (matching against the
  **original** content, overlaps rejected naming both indices, applied in
  reverse) because hrdr would feel it more — every mutating call is a
  serialization barrier, and hrdr serializes **all** mutation globally where pi
  queues per realpath.
- **A CLI-shaped tool surface — rejected 2026-07-27.** Asked whether every tool
  should be an args-array like `git`. No: model CLI fluency is with _existing_
  CLIs (available through `shell`), an invented CLI grammar is less familiar
  than JSON function-calling, args-arrays give zero field-level schema guidance,
  and the whole cross-cutting layer (read-guard, staleness culprit, secret
  guard, LSP-on-edit, spool nudges) keys on knowing which field is the path.
  `git`'s shape is right **because it wraps a known CLI behind an allowlist**,
  not because args-arrays are good.
- **Merging the three LSP tools into one `lsp` tool with an operation enum.**
  Opencode collapsed nine operations into one tool and had to make
  `line`/`character` **required** even for `workspaceSymbol`, which ignores
  them. The enum only pays past ~6 operations. **Add ops as new tools if
  wanted.**
- **Moving hrdr's tool descriptions into `.txt` files.** Opencode does, but it
  composes at runtime on top of them — `include_str!` would be a lateral move.
  The change that matters is the return type (`&'static str` → `String`), filed
  above.
- **Slash-command dispatch is mirrored** between `hrdr-tui/src/app/commands.rs`
  and `hrdr-app/src/commands/dispatch.rs`. Intentional: the TUI handler
  intercepts TUI-only commands (`edit`, `reload`, `goto`, `find`, `next`/`prev`)
  then falls through to the shared dispatcher. `CommandHost` is the DRY
  mechanism; the split is explained at the call site.
- **Two project-dir walks** — `skills.rs::skill_dirs` and
  `prompt.rs::gather_agent_docs` both walk cwd → `/` plus XDG dirs. **Now both
  in `hrdr-agent`** (they were split across crates when this was first judged),
  so a shared iterator is cheaper than it was; still ~15 lines each with
  diverging payloads (skill dirs vs `AGENTS.md`), so still judged borderline
  over-engineering. Re-examine only if a third walk appears.
- **Three `CommandHost` impls** — `TuiHost` and the test hosts `TestHost` and
  `RouteTestHost`. The trait is the shared mechanism; the test hosts share some
  trivial no-op bodies, but the login host carries login-specific state. A
  shared test base would remove a few no-ops for very little gain.
- **Secret-file write/edit guards are tailored, not shared.** `write.rs`,
  `edit.rs` and `fileops.rs` each `bail!` with their own message ("refusing to
  write…", "refusing to edit…", "copying it would place its contents…"). The
  structure repeats; the wording is deliberately specific and meaningful to the
  model. The read side (`guard_secret_read`) is already shared.
- **`tree.rs` and `replace.rs` build their own walkers.** Genuinely different
  configuration — variable `max_depth` and no ignore toggles in `tree.rs`;
  `hidden(false)` with no `.gitignore` handling in `replace.rs` — so they stayed
  out of the shared `ignore_walker` that `find` and `grep` use.
- **The three grep backends keep separate bodies.** Divergent flag sets
  (ripgrep's `--hidden`/`--glob`, POSIX grep's documented `--exclude-dir` trap,
  the built-in `ignore::Walk`). `GrepBackend` already dispatches by exhaustive
  match; shared methods would wrap nothing.
- **Two "is this the ChatGPT/Codex endpoint" checks, on purpose.**
  `hrdr_llm::detect_backend` uses a permissive host+substring test to pick a
  wire protocol (a mirror or gateway still needs the Responses-API body shape);
  `config::is_codex_oauth` uses strict equality against one constant to gate
  OAuth credential injection. Unifying them would weaken a security boundary
  documented at its call site.
- **`AgentEvent` is matched in two places** — `transcript.rs`'s shared
  `apply_event` fold and `subagent_transcript.rs`'s `Record` projection — but
  they build different artifacts (live TUI transcript vs serializable record).
  Not a fork.
- **`lsp.rs` and `mcp/client.rs` spawn without `proc::spawn_group`.** They hold
  `Option<ProcessGroup>` in long-lived fields, rely on the guard's `Drop` with
  documented field ordering, and never kill explicitly — the `GroupKill` handle
  would be dead weight.
- **Evals.** pi's `packages/evals` is `private: true`, holds **one** case
  (assert the model answers "Paris"), is scored by plain `vitest` exact-match
  with no judge and no dataset, **is not run in CI**, and its harness disables
  everything pi does (`noTools: "all"`, `noExtensions`, `noSkills`). Against
  hrdr's ~1,450 in-repo tests that is an aspiration, not an advantage. **If hrdr
  builds evals, build them because we want them** — not to close a gap.
- **Cron / scheduled runs.** Hermes' `cron/scheduler.py` is 194 KB around an
  in-process 60-second poll thread inside a long-lived gateway daemon under
  launchd/systemd. It presupposes a resident daemon hrdr does not have and does
  not want; scheduled work for a coding agent lives in CI, and the intra-session
  case is "say what you are waiting on and end the turn" (there is no polling
  tool: `watch` was deleted 2026-07-30). Note the second-order cost hermes paid:
  cron sessions poisoned session-search ranking badly enough to need a demotion
  tier. The one detail worth stealing if hrdr ever ships anything scheduled is
  the _posture_ — cron runs get `skip_memory=True` unconditionally because
  _"cron system prompts would corrupt user representations"_, and approvals fail
  **closed** there.
- **Auto-downloading `rg`/`fd`.** See the grep-backend item above: rejected on
  distribution grounds (single static binary) and because the degradation ladder
  is a feature on locked-down machines — **not** because auto-download is
  inherently unacceptable (hermes does it with SHA-256 + cosign).
- **Code mode / V8 tool execution.** Codex ships it and makes it mandatory on
  its newest models (`gpt-5.6-{sol,terra,luna}` declare
  `"tool_mode": "code_mode_only"`, hiding every nested tool behind `exec` in a
  fresh V8 isolate); opencode's `CodeModeTool` is **not** in its builtin
  registry and exposes MCP/CodeMode tools only, explicitly not top-level tools —
  the opposite idea under the same name. **Not the same feature twice; ignore
  both.**

Seams already done right, worth copying rather than reinventing: `Shell`
(`tools/shell.rs`), `EditorEngine` (`hrdr-editor`, trait + impls with zero
call-site branching), `Transport` (`mcp/types.rs`), `GrepBackend`, `ModelRef`,
`ChatErrorKind`, `proc::ProcessGroup`.

---

## Leads worth not regressing

From the comparison, and only the ones a future change could plausibly trade
away. Not work; guardrails on work.

**Checked and correct as-is — do not "fix" these:**

- `git_metadata_roots` (`sandbox.rs`) serves hrdr **itself** being launched
  inside a user's linked worktree, where the agent must still commit. Its
  sibling `enclosing_git_dir` does the same for an agent scoped _below_ a repo
  root (a `task` with a narrow `cwd`). Both are pinned by tests; neither is dead
  code left over from the removed sub-agent worktrees.
- The `git worktree remove --force` and `git branch -D` guardrails protect a
  _user's_ own worktrees and branches generically — they were never about
  sub-agent worktrees.
- The `git rebase HEAD` guardrail is a generic `-C <dir>` footgun rule, not
  task-specific.
- **`memory.rs` writing outside the sandbox roots is correct.** That is where
  memory lives, and routing it through `check_write` would break the feature.
  The audit framed this as a bypass to plug; it is not one. What was separable
  was _authority_, and that is handled (`313eb0e`: `memory` is main-agent-only).
- Session and transcript persistence carry no removed fields and neither uses
  `deny_unknown_fields`, so an old file still loads. Relevant every time a
  record type loses a variant — as `Record::EscalationDecided` just did.

- **Sub-agent filesystem isolation — all four peers lack it.** codex has two
  generations of sub-agent tooling and no worktrees; hermes' children share the
  parent's cwd while its own tool description claims _"separate working
  directory"_ (its `delegate_tool.py` has zero `worktree|mkdtemp|os.chdir|cwd=`
  matches) and default `max_concurrent_children` is 3; opencode's child session
  runs in the same directory (its `worktree/index.ts` exists but is not
  referenced from `task.ts`); pi's exists only as a 1015-line example spawning
  `pi --json` subprocesses with no isolation. **hrdr matched codex and opencode
  here deliberately in 2026-07-29: sub-agents share the working directory, and
  the isolation this paragraph praised was removed. The read-only `.git` that
  briefly replaced it went too (2026-07-30) — it refused the file tools a write
  `shell` walked round. What a sub-agent's scope is now: its `cwd`, which `task`
  can narrow, enforced by the kernel.**
- **Read-before-write that refuses.** hrdr blocks every non-`Fresh` `ReadState`.
  Hermes detects staleness and its own docstring says _"Does not block — the
  write still proceeds"_; pi's `write` overwrites unconditionally. A lead over
  two peers independently — do not soften it into a warning.
- **Semantic `rename`.** No LSP at all in codex or hermes; opencode's `lsp` tool
  is experimental and has **no rename op**. The single capability no other
  harness in this comparison has.
- **Guardrails with no off switch.** hermes has `HERMES_YOLO_MODE`, a default
  `"smart"` mode where an auxiliary LLM auto-approves, and a headless path that
  auto-approves **without running the scanners** (plus a CVE for a contextvar
  race onto that path). hrdr's are compiled in, read no env var, have no LLM in
  the loop, and apply to sub-agents. **hrdr's autonomy posture is coherent
  precisely because there is no headless carve-out.**
- **USD cost budgeting, and session retention/compression.** Absent in all four
  peers, both of them.
- **ROI-gated mid-history pruning with recoverable pointers.** Codex has only
  truncate-at-capture and full auto-compact with no rung between; opencode's
  prune is off by default and replaces content with
  `"[Old tool result content cleared]"` and **no re-read path**. hrdr spills to
  `tool_output_dir()` and substitutes a pointer with recovery instructions.
- **Concurrency that cannot corrupt the tree.** `concurrent()` defaults to
  `read_only()`, so mutating tools are a strict barrier. Codex's `shell_command`
  and `exec_command` both declare parallel-safe with no path-level locking.
- **Skill shadowing beats skill syncing.** Built-ins embedded, project/user
  files shadow by name, first-source-wins, tested. Hermes copies bundled skills
  to `~/.hermes/skills/` and needs an MD5 origin-hash manifest, a v1→v2
  migration and a `.no-bundled-skills` opt-out to work out whether the user
  customised a copy. hrdr has no such state to get wrong.
- **Post-edit LSP diagnostics folded into the edit result.** `apply_file_change`
  returns `lsp.diagnostics_note` with the success message — the model learns it
  broke the build in the same tool result. Opencode does the same on mutation
  (parity on the mechanism that matters); codex and hermes have no LSP.
- **Bounded output everywhere, not just for the shell.** 11 file/search tools
  plus `git`, all through `truncate_saved`. Codex truncates only shell output,
  so `cat` of a 5 MB file costs a round trip to discover.
- **What every peer got wrong and hrdr should not copy:** dead prompt files that
  look live (codex 5-of-6, opencode 1-of-9 plus two more); read-before-write
  that warns instead of blocking (hermes, pi); sub-agent self-reports treated as
  facts (hermes pops `files_written` before the model sees it and answers with a
  prompt saying summaries _"are SELF-REPORTS, not verified facts"_ — hrdr's
  answer is that a sub-agent's edits are already in the tree, so `git diff` is
  the mechanical check); and skill loading that fails **open** (opencode logs
  and skips a YAML error, silently drops a file that fails its shape check).

---

## Standing constraints

Decisions from completed work that still govern new work. These are not backlog
items — they are rules.

- **Every markdown file goes through prettier — the prompt templates included.**
  Prettier is the owner's markdown standard and has no exceptions in this repo.
  When a reflow turns a test red, the TEST is what changes; the formatter is not
  negotiable. Stated because the opposite rule was briefly written here on
  2026-08-02 after `prettier --write` on `templates/write.md` reflowed the file
  and failed 9 prompt tests — carving the directory out of the standard was the
  wrong fix and the owner rejected it.

  Those tests failed because they pinned where a sentence WRAPPED
  (`p.contains("the\n  comment outlives …")`), which is layout, not content.
  `prompt::says` / `prompt::unwrapped` fix that properly: they collapse a soft
  wrap (a single newline plus its indent) to one space on both sides of the
  comparison, so an assertion tracks words rather than columns. Blank lines
  survive, so tests that genuinely assert structure still can. Assert prompt
  text through `says`, never `contains`. Verified by reformatting every template
  at `--print-width 60` and re-running: 631 passed, unchanged — and a real
  wording change still fails, checked by editing one.

  Watch for two things prettier normalizes beyond wrapping: `*emphasis*` becomes
  `_emphasis_`, and byte-offset tests (the shared-prefix checks in `prompt.rs`)
  must compare `unwrapped` text or a wrap inside the searched phrase breaks the
  `find`.

- **No drifting numbers in comments or docs.** Decided 2026-08-02 by the owner,
  after the web removal left three separate stale counts behind ("Four
  `CommandHost` impls", "nine-crate workspace" twice, "publishes ~9 crates"). A
  count of code elements — impls, crates, call sites, tests, fields, variants —
  must not appear in prose: nothing updates it when the code changes, and a
  wrong number reads as verified. Either say it without the number ("a
  multi-crate workspace", "the test hosts") or, if the reader genuinely needs
  the value, put the value in a `const` and have the prose name the `const`
  instead of repeating its digits. **A number that has to be written out in a
  comment is a sign the value belongs in a variable and is not there yet.**
  Numbers that describe something outside the tree (a wire protocol's limit, an
  upstream API's cap, a dated incident report) are fine — they cannot drift with
  our code.

- **hrdr is terminal-only.** Decided 2026-08-02 by the owner: the web server,
  the browser SPA and the desktop/mobile shell are removed, not deferred. The
  TUI (plus `hrdr run` for headless) is the whole frontend surface. Do not
  reintroduce a second frontend, a wire protocol or a `CommandHost` impl for one
  without the owner asking for it — `hrdr-app`'s host seam exists to keep the
  TUI honest, not to hold a place for a client that no longer exists.

- **A pty test drains through `common::drain_pty`, never a bare read loop.** Two
  Windows-only traps sit behind that helper, and both are silent on Linux and
  macOS. A ConPTY asks the terminal where the cursor is (`ESC[6n`) and **blocks
  until something answers**, so a harness that only reads captures exactly those
  four bytes and then times out; and a ConPTY master returns `WouldBlock` before
  the child has written anything, so `while let Ok(n) = read(…)` treats a
  not-yet as end-of-stream. `tui_pty.rs` carried both fixes and a comment saying
  they had cost a red run — writing a second harness from scratch cost another
  one (`c0e22e9`), which is why this is a rule rather than a comment. Likewise
  **do not assert that output contains no escape byte**: a ConPTY writes its own
  mode sets, window title and cursor commands regardless of what the child does,
  so that assertion tests the terminal. Assert on the specific sequence under
  test — `ESC[38;5;` for a foreground colour.

- **hrdr-agent owns ALL agent logic; hrdr-app is only agent↔TUI glue.** Every
  agent, main or sub, runs the same codepath — no special-casing, no parity
  forks. Do not ask "how should sub-agents behave"; they behave exactly like the
  main agent because it is the same code.
- **Only the `AgentEvent` fold persists in a transcript.** User=`Steered`,
  Assistant=`Text`, `Reasoning` text, tool args+results, agent `Notice`→System.
  Frontend-pushed _chrome_ is not persisted — slash-command System output,
  `/diff` output, per-turn Stats, Header, `Reasoning.took_ms` — it is
  display-only, not context, and not needed to resume.
- **No migration or back-compat fallback before 1.0.** Clean breaks; delete
  old-format fallback code when you find it.
- **`hrdr-llm` has three streaming paths** (`client.rs`, `anthropic.rs`,
  `codex.rs`), and an invariant added to one does not reach the other two. This
  produced security finding O4 (duplicate auth header) and a wire log that
  silently covered only one backend. Anything cross-cutting added to
  `client.rs`'s request path must be checked against the other two.
- **The sandbox is a boundary, not a hint** (the code is
  `crates/hrdr-tools/src/sandbox.rs`).
  - _Never confine the hrdr process itself._ No Landlock `restrict_self`, no
    prctl, outside a child `pre_exec`. hrdr does its own session/config/memory
    I/O in-process; confining it breaks the app.
  - _Never silently pretend to sandbox._ Any path that runs a command with less
    confinement than the mode asks for must set its notice first (each notice at
    most once per process). `read` degrading to write-confinement under Landlock
    is decided and allowed — being quiet about it is not.
  - _The writable set is all of it:_ cwd + `env::temp_dir()` + session scratch +
    tool-output + the four linked-worktree git metadata roots (worktree gitdir,
    `objects`, `refs/heads/hrdr`, `logs/refs/heads/hrdr`) + configured extras.
    Drop temp and compilers die; drop scratch/tool-output and overflow spill
    breaks; drop the git roots and every write sub-agent's commit fails.
    Equally: never widen to the whole parent `.git` — that re-opens the escape
    the sandbox exists to close.
  - _bwrap argv order is semantics_ (later mounts shadow earlier ones):
    `--ro-bind / /` before the rw `--bind`s; `--tmpfs /tmp` before the
    cwd/scratch/tool-output binds (the scratch dir lives under `/tmp`); and
    `/bin`, `/sbin`, `/lib`, `/lib64` emitted as `--symlink <read_link(p)> <p>`
    on usr-merged distros, never `--ro-bind` and never a guessed
    `usr/<basename>`.
  - _`ToolContext::new` stays unconfined._ Only `Agent::new` installs a real
    policy; hundreds of tool tests build a bare context against tempdirs.
  - _Guard model-supplied paths only._ Memory storage, overflow-spill writes and
    hook/LSP/MCP subprocesses are app infrastructure and bypass
    `resolve_read`/`resolve_write` by design. The one deliberate widening is
    `rename`, which also guards the server-returned workspace-edit targets — the
    guard's contract is _where writes land_, not _who typed the path_.
  - _`SECTION_SANDBOX` stays after `SECTION_ENVIRONMENT`_ (its roots name the
    per-agent worktree, so it belongs in the volatile tail), and the
    prompt-cache split anchor stays `SECTION_ENVIRONMENT`.
  - _Broad reads in `write` mode and full env passthrough in bwrap_ (no
    `--clearenv`) are decided v1 tradeoffs, not oversights. Narrowing either is
    follow-up work, not a bug fix.
- **A skill the model can load is still the user's procedure**
  (`hrdr-agent/src/skills.rs`, `prompt::skills_section`).
  - _The listing is a menu, never the content._ Name + one-line description
    only; bodies come from the `skill` tool when one applies. Under the byte
    budget descriptions are dropped tail-first and **names always survive** — a
    name the model cannot see is a skill it can never load.
  - _No source paths in the listing._ They name the per-agent worktree, so they
    would differ between sibling sub-agents and push per-agent bytes into the
    shared cache prefix. The tool's own result names the source, where it costs
    nothing shared.
  - _A skill body is instruction, and it is project-authored._ It reaches the
    model as tool output — which the base prompt otherwise calls data, never a
    command — so the result frames it explicitly as the user's/project's
    instructions and names the source. Same trust class as `AGENTS.md`, and the
    same open exposure (an untrusted clone's `.hrdr/skills`).
  - _`model_invocable: false` is a boundary._ Such a skill is unlisted **and**
    refused by the tool, with an error telling the model to ask the user to run
    `:name`. Only a literal `false` opts out (a typo fails open, visibly, rather
    than silently hiding a skill). Built-in `:release` used to carry it because
    its last step pushes a tag — **reversed 2026-08-05 by the owner**: every
    built-in, `:release` included, is now model-invocable, and the model is
    expected to follow the skill's own preflight (clean tree, right branch, ask
    before deciding) rather than being barred from loading it.
  - _The prompt section is gated on the tool._ A profile whose `tools:`
    allow-list drops `skill` gets no listing: naming a tool an agent lacks is
    the defect the pi comparison found, not a pattern to repeat.
- **A new tool picks its interface shape by rule, not by taste** (taxonomy from
  the 2026-07-27 survey of all 31 tools). The shape is load-bearing: the
  cross-cutting layer (read-guard, staleness culprit naming, secret guard,
  LSP-on-edit, spool nudges) keys on JSON-schema'd fields, so a tool the harness
  cannot introspect is a tool it cannot protect.
  - _Default_ — one noun-tool, flat args object: one capability, one required
    primary arg, the rest optional flags (`read`, `edit`, `grep`).
  - _`action` enum_ — several **mutating** verbs over one resource sharing one
    field vocabulary (`memory`: view/write/edit/delete/search over
    name/description/body/scope).
  - _`mode` enum_ — **read-only** views of one dataset (`models`:
    current/providers/models).
  - _Separate prefix-family tools_ — verbs with distinct schemas or distinct
    read-only gating (`task_*`: spawn takes description/prompt/model, diff takes
    commit, cleanup takes force). One mega-schema would leave most fields
    meaningless per action — that is the real hallucination trap.
  - _CLI args-array passthrough_ — reserved for wrapping an **existing,
    well-known** CLI behind an allowlist (`git`). Never the shape for a bespoke
    tool. A model that wants raw CLI already has `shell`.
  - _Time is seconds, always_ — `timeout_secs`, `interval_secs`; never `_ms` in
    a model-facing schema (`shell.timeout_ms` renamed 2026-07-27, old name
    poisoned). Removed or renamed params must be **poisoned** with an
    instructive error: `tool_args` ignores unknown fields workspace-wide, so a
    silent drop is the default failure mode.
  - _Shared vocabulary across tools_ — one concept keeps one field name and one
    default polarity everywhere: `pattern` + `literal: true` opt-out is the
    matching shape for both `grep` and `replace` (aligned 2026-07-27; their
    previously inverted regex defaults were a silent trap).

---

## Corrections made during the merge

What the three source docs got wrong, found by re-verifying every claim. Kept
because a backlog that quietly fixes its own errors teaches nothing.

**These are a dated record (2026-07-27), not current facts.** Several describe
code that has since been deleted: there is no `git` tool (#1, #2), the tool
count in #6 predates the 2026-07-30 cut of ten tools, and the guardrail count in
#3 has moved. Read them as "what was corrected then", and check the code for
now.

1. **`git -C /elsewhere log` does not succeed.** The sandbox follow-up asserted
   it did, in `read` mode. Leading flags in the subcommand slot (`-C`, `-c`,
   `--git-dir`) are refused with a dedicated test, and `FORBIDDEN_ANY` blocks
   seven more. The true residual — the git tool spawns an **unconfined**
   subprocess because only `shell` and `watch` are wrapped — is now what the
   item says.
2. **The `git` tool has 9 read-only subcommands, not 14.** `ALLOWED` is
   `status, diff, log, show, blame, branch, describe, remote, shortlog`.
   `compare.md`'s opencode section said 14.
3. **Guardrails: 15 rules, and one of them is PowerShell-shaped.** `compare.md`
   said "14 hand-written regexes"; the real count is 13 destructive-command
   patterns plus 2 pipe-to-shell rules (POSIX and `iwr|iex`). So
   `deferred-improvements.md`'s "the pipe-to-shell guardrail assumes POSIX" was
   true only of its **recovery text**, not its coverage.
4. **The login test host is named `RouteTestHost`, not `TestLoginHost`.** (The
   `CommandHost` impl count this item also corrected is moot now that the web UI
   is gone: `TuiHost` plus the two test hosts, `RouteTestHost` and `TestHost`.)
5. **The two project-dir walks are now in one crate.** `skill_dirs` moved to
   `hrdr-agent` with the skills work, so the "different crates" half of the
   reason to leave them alone is gone; the verdict survives on size and
   diverging payloads.
6. **Tool count: ~31, not ~30** — 17 from `ToolRegistry::with_defaults`, the
   rest registered by `Agent::new`. The taxonomy's "all 31 tools" was right.
7. **`security-audit.md`'s closing line was stale.** It said the Windows-drift
   pass "was never run"; it ran and landed three fixes (`8e5bc9d`).
8. **Every `system.j2` citation in `compare.md` was dead.** The template engine
   was removed in `5f6e386` — the prompt is `include_str!` markdown fragments
   assembled as an ordered section list. Those line numbers pointed at a file
   that no longer exists, which is why this file cites symbols instead.
9. **Line numbers rot generally.** `Tool::description` moved from `lib.rs:965`
   to `:1155`; `gather_agent_docs` from `:210` to `prompt.rs:567`. Same policy.
10. **The audit's summary table did not add up.** Severity rows summed to 19
    (2 + 4 + 13) against a stated total of 16 findings. Which number is wrong is
    not recoverable from the doc, and every finding is closed either way — so
    the record above states the total and the discrepancy rather than picking
    one.
11. **The sandbox's path guard covers 16 call sites across 9 tool files** (`ls`,
    `fileops`, `grep`, `edit`, `lsp_nav`, `read`, `replace`, `write`, `tree`),
    not the "14 sites" the shipping notes recorded — `replace` and `rename`'s
    server-returned targets were added after that count was written.

---

## Cleared in the 2026-07-27 pass

Fifteen commits, `0fae706`..`36a7f2b`, cleared every item that was actionable
without a decision: the five that were top of this list, plus the LSP dedup,
revive capability, the notice channel, and the grep-backend, guardrail-drift and
TUI-history test gaps. Read `git log` for what each did — the entries are gone
from the sections above, per this file's own convention.

What survives is the part that would otherwise be relearned:

- **`AgentEvent::Notice` never reaches the model.** The `doom_loop` entry
  prescribed injecting one; it would have fixed nothing. The channel that
  reaches the model is a note appended to the round's last tool result — the
  shape the round-budget warning, the repeat nudge and the truncation warning
  all now share.
- **`read_only()` means "does not mutate the working tree", not "touches no
  state".** `todo` was classified as mutating and pruned from read-only agents
  while the prompt told every agent to plan with it. Anything holding state only
  in the agent's own `ToolContext` belongs in the read-only set — and if its
  calls are order-sensitive, it opts out of `concurrent()` separately, which
  defaults to `read_only()`.
- **An item's proposed fix is a hypothesis.** Three of these described their own
  fix wrongly (the two above, plus `git -C /elsewhere log`, which was already
  refused). Verify the claim before implementing the remedy.
- **A guard's blast radius is the thing to check first.** The metadata guard
  initially covered `.hrdr`/`.claude`/`.opencode` too, which would have refused
  every write a sub-agent makes in `<repo>/.hrdr/worktrees/wt-N` had the check
  been whole-path rather than root-relative, and did block "add a project skill"
  even when it was correct. Narrowed to `.git`; the rest is a decision at the
  top of this file.

## Cleared in the 2026-07-30 sandbox redesign

Nine slices, `5c9f675`..`c114a6a`, ≈9,000 lines net removed.
`docs/sandbox-redesign.md` is the decision record and stays; the code is the
truth. `docs/context.md` was folded into this file and deleted, per this file's
own convention.

Closed **by deletion**, which is the shape of most of it: the `.git` lock, all
of escalation, the network axis, bwrap, `DenialKind`, ten tools, and `grep`'s
two subprocess backends. Also shipped: `jail` mode with a fixed five-tool set,
the `prisoner` agent, per-profile `sandbox:`, per-session `tool_output_dir`,
package-manager cache roots, `--sandbox-writable-root`, unified
untrusted-content wrapping, and `task`'s `cwd`.

What survives that would otherwise be relearned:

- **Deleting a mechanism can be the fix.** The escalation ladder existed for one
  failure — bwrap's user namespace making ssh refuse `/etc/ssh/ssh_config` — and
  removing bwrap removed the failure. Two large features (a consent gate with an
  audit trail, a widening ladder) were answering a problem the redesign deleted.
  Ask what a mechanism is _for_ before improving it.
- **A guard the front door bypasses stops only the honest path.** The `.git`
  file-tool lock refused `write`/`edit` a path `shell` reached in one step,
  while refusing legitimate `.git/info/exclude` edits and user-requested hooks.
  Same reasoning killed the network denial: it bought one hop of latency, not
  containment, because the sub-agent reports to a parent that has a network.
- **"Available and ignored" is the only usage figure worth acting on.** That a
  tool the model was handed gets called measures availability. `references` 2
  calls in 9,350, `definition` 0, `rename` 0, `watch` 4 — that measures need.
- **Removing a tool means auditing what it was the only home for.** `grep`
  filtered credential files out of its own output; deleting it from every
  non-jail mode would have left `shell` — the actual search path — with no
  secret handling at all. The filter moved to `shell` and grew a diff-aware
  half.
- **Confinement that a mode's tool set makes unreachable is not confinement.**
  `web_fetch`/`web_search`/MCP run in the hrdr parent, outside the sandbox, so a
  "confined" agent holding them had a working network egress. Jail's boundary is
  its tool set as much as its roots.
- **A floor that silently inverts a request needs a notice.** `--sandbox jail`
  on a write-capable session floors at `write`; without saying so, somebody who
  typed the word meaning "contain me" gets full project write and never learns.
- **`cargo` here runs through a wrapper that indents `error:` lines**, so
  `grep -E "^error"` reports a false pass. Read its summary line. This cost a
  follow-up commit.

## Cleared in the 2026-08-01 review pass

`docs/code-review.md` (2026-07-31, refreshed 2026-08-01) is **deleted** per this
file's own convention — every finding in it shipped. Nine commits,
`4e66a1c`..`2e3be29`, released as **v0.10.0**; `git log` is the history.

Sixteen findings closed. The ones still in the tree: `SseDecoder::finish`
returning `Ok` with a truncated event; a Codex error whose code sat at one level
and its message at another; a hook timeout reporting seconds as `ms`; a crashed
turn leaving its tool call spinning forever; the SSRF blocklist's missing
`100.64/10`; `attr_value` matching an attribute suffix; a DDG snippet reading
past its block; a CI-file cap that hid every single-file config; a leaked
`String` per env var; and a stale `allow(dead_code)`. The rest were in the
since-removed web server and its wire protocol — `git log` is their history.

What survives that would otherwise be relearned:

- **A skill the model cannot see is not a copy that can be relied on.**
  `write.md`'s Releasing section and the `:release` skill were the same
  procedure twice, and only the skill said to watch the tag's CI run.
  `model_invocable: false` keeps a skill out of the listing entirely, so
  plain-English "cut a release" only ever reached the copy missing that step.
  Duplication drifts; the reachable copy is the one that must be complete.
- **A red tag run SKIPS its publish jobs rather than failing them.** The push
  succeeds, the tag exists, nothing is published — how v0.4.3 and v0.5.0 were
  tagged with nothing behind them, and it happened again on `de1b12b` in this
  same pass. Enumerate the run's jobs and confirm the artifact landed; "tagged
  and pushed" is not "released".
- **Gate the prompt on what the tool set IS, not on what built it.**
  `ToolRegistry::with_defaults` registers `grep`/`find`/`ls`/`tree` and
  `Agent::new` strips them for every non-jail mode, so `has("grep")` alone
  marked a full write agent as jailed. The jail is the whole shape: those tools,
  no write tool, and no shell.
- **A test that models an impossible agent proves nothing about the real one.**
  The read-only prompt test built `retain_only(read_only_names())` — shell-less
  — while `config.read_only` deliberately keeps a shell. It had never covered
  the agent it was named for. Prefer building a live `Agent` over
  hand-assembling a registry.
- **Compressing prose has a ceiling the tests set.** 224 pinned literal spans
  across the corpus, ~110 in `write.md` alone: careful rewording of four
  always-on files yielded ~900 bytes, while moving git/release guidance behind
  `!delegated` yielded 9.4 KB. Structure beats wording by an order of magnitude
  when the wording is already frozen.
- **Guidance with no trigger phrase has to stay resident.** Deleting and
  Dependencies did not move to main-only with Git and Releasing: a sub-agent
  deletes files and reads dependency APIs like anyone else, and nothing is said
  before `rm -rf` that a gate could match on.

## Cleared in the 2026-08-02 backend pass

Six commits, `7e80605`..`9c3d012`. Two of the three OS backends changed status,
neither by writing much code.

**macOS Seatbelt was never untested — the tests just could not say so.** Its
end-to-end test opened with two silent `return`s (no `/usr/bin/sandbox-exec`, no
shell), so a run that exercised nothing was indistinguishable from one that
passed, and this file recorded it as never having run while CI ran it on every
macOS job. Both conditions now assert on a runner and skip only locally, and
`ci_runs_a_real_os_backend` fails if a runner detects the `None` fallback.

**Windows `read` mode is now confined**, by Mandatory Integrity Control: a
Low-integrity process cannot write to any object labelled Medium or higher,
which is everything the user owns, while reads are untouched. Applied the way
Landlock is — by the child to itself — because `CreateProcessAsUserW` cannot be
reached through a `tokio::process::Command`, so hrdr re-execs itself as
`hrdr __sandbox-exec -- <shell> -c <cmd>` and lowers its own token first.

What survives that would otherwise be relearned:

- **A skip that cannot fail is not a skip.** The Seatbelt tests were the shape
  `write.md` already warns about, one level up: not a check that could not fail,
  but a check that could decline to run and report the same green. Any test
  gated on a prerequisite needs to assert where that prerequisite is guaranteed.
- **`current_exe()` is the test binary inside a unit test.** The first Windows
  end-to-end test lived in `hrdr-tools`, whose test binary is what the backend
  then re-executed — it handed `__sandbox-exec -- cmd …` to libtest as filter
  arguments and wedged the Windows job for 37 minutes. Anything exercising the
  real wrapper belongs in `apps/hrdr/tests/`, where `CARGO_BIN_EXE_hrdr` names
  it.
- **Blind FFI fails on names, not logic.** Three CI round trips, every one a
  constant or trait import that had moved between `windows-sys` releases
  (`SE_GROUP_INTEGRITY`, `anyhow::Context`) — never the token or SID logic,
  which was written once and never changed. Spell a fixed ABI value out locally
  instead of importing it and the class disappears.
- **A red run skips its publish jobs rather than failing them.** Seen again on
  `de1b12b`: rustfmt went red, and the six publish jobs reported `skipped`. The
  release was not cut, which is the system working — but only because the tag
  had not been pushed yet.

## Record: closed efforts

No worklist here — read `git log`. Kept only so nobody re-opens a closed
question.

**2026-08-06 backlog slices — eight perf/tidy items** (`944014c`, `2a112f1`,
`f6c64b3`, `25d690e`, `1b84108`, `1e24635`, `c351731`, plus the docs commit
recording them). Worked one slice at a time (direct — each was small enough that
a delegation round-trip cost more than the work), each gated before commit. Tidy
2026-08-06: `truncate_on_boundary` folded into `floor_char_boundary`; `is_fresh`
shared from `hrdr-llm::catalog` (the provider_catalog copy deleted); the YAML
scalar stringifier shared via `skills::scalar_to_string`; `entry_content_hash`'s
dead `expand_all` param dropped (its pinning test deleted — the property became
structural); and the item-12 minors: `now_ms` forwarding wrappers inlined,
`is_usage_limit` narrowed to `pub(crate)`, `filter_model_choices` hoists its
query normalization behind a shared `fuzzy_match_q` core, one
`sibling_with_suffix` in `hrdr-llm::fs` replaces the wire-log rotation / config
backup / store-lock name builders, one `flatten_slug` core for `cwd_slug` and
`child_transcript_id`, and one `workspace_root()` in hrdr-test-support for the
three workspace-walking test binaries. Perf 2026-08-06: the shell ingest line is
streamed by moving the owned newline-terminated buffer into the sink instead of
`format!`ing a copy per line. Perf 2026-08-04: `TranscriptLog` tracks its
appended length and drops the per-record `metadata()`; the rollback boundary is
the open length plus every successful line. Perf 2026-08-04 #9 (picker refilter)
was **partially addressed, not closed** — the query-normalization half shipped
in `1b84108`, the per-choice haystack precompute was deferred (cross-cutter over
the picker layer, collides with tidy 2026-08-06 #6); see its tag in the section.
The AUR publish for v0.11.0 is still blocked on the AUR's own outage (probed
2026-08-06: the git backend still answers "down due to maintenance", RPC still
`0.10.0-1`) — the "Owed right now" entry stays.

**2026-08-06 perf review finding 5** (`695840d`). The `@file` completion index
is now `(path, lowercase_path)` pairs: `spawn_file_index` (hrdr-app util.rs)
computes the lowercase form once per tree walk on the blocking task, and
`rank_file_matches` ranks against the precomputed half — one lowercase per
keystroke instead of one per indexed path. The TUI's `file_index` field and
`TurnMsg::FileIndex` carry the pairs. Mixed-case ranking pinned by
`rank_file_matches_uses_the_precomputed_lowercase_form`.

**2026-08-06 perf review finding 4** (`9d5f5ed`). `AgentEvent::History` now
carries `Arc<Vec<ChatMessage>>` and `Agent.messages` is an `Arc` (copy-on-write
via `Arc::make_mut` at every mutation: turn loop, compaction, `clear`,
`push_user_message`, `push_user_note`, the system-message rewrites,
`set_messages`). The turn-loop emitter, the registry's event log and `since()`'s
clones are refcount bumps; the TUI's `persist_mid_turn` and the delegation
snapshot keep the one deep copy; the OpenAI request body's `to_vec()` is the
wire copy, out of scope. Regression test
`history_event_shares_the_agents_message_arc` drives a real mock-server round
and asserts the emitted payload shares the agent's allocation
(`Arc::strong_count >= 2` at the sink) — red on a deep-copying emitter.

**2026-08-06 perf review finding 3** (`eafc82c`). `ReadTool` no longer reads and
parses the whole file per call. The file is scanned once in chunks
(`WindowScanner` in `crates/hrdr-tools/src/tools/read.rs`), capturing exactly
the `[start, start+limit)` window and counting newlines for the total the
coverage record needs; only the window is UTF-8-validated (a binary tail outside
the window no longer fails a paged read of the text before it — it errors when a
page reaches it; `full` reads still validate the whole file). Rendered output,
totals and coverage semantics unchanged, pinned by
`window_scan_matches_str_lines_semantics` (equivalence to `str::lines()` over
three chunkings — 1-byte chunks, 3-byte, whole — which caught a mid-chunk
double-capture bug in the first draft) and the existing paging/coverage suite;
the window-only-UTF-8 contract is pinned by
`windowed_read_no_longer_requires_the_whole_file_to_be_text`, red on the old
code first.

**2026-08-06 perf review finding 2** (`631b432`). The per-line shell secret
filter (`grep_line_is_secret`, which canonicalized `cwd.join(token)` on every
line of a command's stdout/stderr) is now `SecretLineMemo` in hrdr-tools: a
per-run map of joined-path → verdict created once per `shell` run beside the
`DiffRedactor`, so each distinct path token is canonicalized once and repeated
`rg -n`/`grep -n` match lines collapse to a map lookup. The verdict is a per-run
snapshot (pinned by `a_secret_line_memo_is_a_per_run_snapshot`, shown red on the
unmemoized filter first).

**2026-08-06 perf review finding 1** (`2f38e1b`). `load_cached` memoizes the
parsed models.dev catalog per path, keyed by the file's mtime (the pattern
already shipped in hrdr-agent's `auth_store`), and returns `Arc<Value>` so a hit
is a refcount bump rather than a deep clone. The Anthropic branch's per-round
`max_output_cached` → `load_cached` lookup (every request, because `max_tokens`
defaults to `None`) went from a 3.5 MB read + JSON parse per round to O(1); a
changed file invalidates by mtime, and misses are cached too. The two hrdr-agent
callers pass `catalog.as_deref()`. Regression test:
`cached_read_serves_the_memoized_parse_until_the_mtime_moves`, shown red on the
unmemoized read first.

**2026-08-06 backlog slices — the four dated 2026-08-06 findings** (`4638e76`,
`a328c03`, `de51c8b`, `695f07c`, `205844e`). Security audit findings 1-2: the
`Accumulator` caps the accumulated reply at 64 MiB (content, reasoning and
tool-call fragments) and errors the stream past it; `StoreLock::drop` removes
the lock file only when it still carries the guard's own PID (a reaped and
re-claimed lock is never deleted out from under its new owner). Correctness
findings 1-2: a click on a call inside an expanded tool group pins the group
summary's own top row, not the click row (no more view jump while scrolled up);
`thinking_open` is renumbered across `prune_scrollback` and cleared when the
transcript is wholesale rebuilt (`/clear`, resume). Tidy review items 1-2: five
grep-verified-dead `pub` items deleted (`PromptDelivery::into_handle`,
`ModelRef::into_parts`, `Panes::active_sub`, `transcript_to_plain_text` and its
orphaned `clip`, `wire_protocol` + test + re-export), and the six hand-written
SSE-overflow messages collapsed onto `SseOverflow::to_string()`. Each fix ships
a regression test that was shown red on the old behavior first.

**2026-08-06 notice redesign — `::Notice` leaves the transcript** (`a371434`,
`2bff248`, `c18a63c`, `72f29e2`). A slash command's status line (a setting
change, `/verbose`, a login notice, an async `/models` result) now toasts on the
clipboard-feedback stack, and a data command's output (`/help`, `/status`,
`/cost`, `/tools`, `/prompt`, `/guardrails`, `/doctor`) opens an Esc-dismissible
popup — nothing a command prints enters the transcript, so nothing it prints can
split a streaming thinking block. The agent's own `Notice` events (errors,
budget warnings) are still recorded as `Entry::system`, held in `record()` until
the thought closes so the event log and jsonl fold to one complete block. The
TUI's mid-thought chrome deferral was removed as dead; the loader and live
panels now sit exactly one blank row off the surface above. Binding: the
transcript belongs to the conversation; frontend chrome is toast or popup, never
an entry. (`/diff` keeps its colored transcript block as a deliberate feature.)

**2026-08-05 Enter-path lag** (`6793464`). The Enter path blocked the UI thread
on the disk twice: the first Enter of a session ran the full session save
(serialize + `write_atomic`'s two fsyncs) synchronously in `reserve_session_id`,
and every unique Enter rewrote the input-history file (two more fsyncs) via
`HistoryBrowser::record`. Both are off the event loop now — `save_session` was
split into a sync `mint_session` (id + open-lock + reservation, still on the UI
thread because the id names the sub-agent transcript dir before the turn runs)
plus the deferred write through the existing save task, and the history persist
runs on a detached thread chained behind the previous write. Also fixed while
there: `reserve_session_id` mints with `state.cwd` synced, so a brand-new
session's first save lands under the real cwd slug instead of the empty-cwd one
the turn-end autosave orphaned. NOT addressed — still open: backlog perf #1
(per-round full-history save with two fsyncs, the documented dominant cost of a
long session; needs the crash-durability decision) and the designed
auto-compaction before the first request of a near-full context, which delays
the _reply_ (the message itself appears before it).

**2026-08-05 CI fallout after the dep update** (`5e3676e`, `a88e17b`). The
reqwest 0.13 upgrade pulled webpki-root-certs (same CDLA-Permissive-2.0 as its
0.12-era webpki-roots), so the deny.toml per-crate exception was repointed. The
Windows test runner then exposed a real race in the memory mtime cache (perf
review #4's cache, `6b3bf37`): a rewrite landing within the same mtime tick —
coarse filesystem granularity, some Windows setups — looked unchanged to the
mtime-only key, so `rebuild_index` rebuilt the pointer index from the stale
pre-mutation entry and a memory edit kept its old description on disk.
`rebuild_index` now drops the root's cache before re-reading; a regression test
pins a file's mtime back to the cached value and asserts a same-length
description change still reaches the index. **The direct-path half was closed
2026-08-05 (`7cfaac5`, `c722120`)** — `load_memories`' own mtime-only cache
served the same stale entry after a same-tick edit, and the Windows runner hit
it on every CI run (a coarse _write_ clock stamps rapid writes with one mtime
while still storing an explicitly-set time verbatim, so the first probe — a
`set_modified` round-trip — said "fine" and changed nothing). Each memory root
is now probed once by the failure mode itself — three rapid writes, fine only if
every adjacent pair's mtime differs — and a coarse root bypasses the cache
entirely, re-reading every call.

**2026-08-04 review-batch slices** (`0dce43d`, `1f4a46b`, `2b991c2`, `7d6c3a4`,
`176de8e`, `820bc5d`, `96517bf`, `0144f93`, `33a4cf8`, `f531de6`, `6b3bf37`,
`f901485`). The actionable items from the three 2026-08-04 reviews were worked
one slice at a time (delegate → review → commit → push): all eight tidy dedups
(`now_ms`, `create_dir_owner_only`, `home_dir`, terminal-restore, `todos_owned`,
spinner frame, `bounded` move, `md_theme_with`), the registry lock-scope fix
(perf #2), the running content hash (perf #3), the memory per-file-mtime cache
(perf #4), and the `Session::fork` transcript copy (review #2). Two perf items
were dropped after brief-time verification: #5 — the event must survive to the
frontend's `on_event`, so `record` cannot give ownership to `from_event`; a
borrowed-`Record` variant is a larger refactor for one small clone per chunk —
and #6 — a running token counter must track ~11 message-mutation sites including
in-place edits; a missed one silently corrupts the budget cap for a
~µs-per-round win. Still open pending a decision: perf #1 (crash-durability
tradeoff), tidy #9 (external-API re-export), review #1 (memory-description
format).

**Security & correctness audit** (2026-07-22, re-reviewed 2026-07-23, last
finding closed 2026-07-26; full-codebase, high depth). Attack surface was mapped
by entry point — HTTP handlers (`fetch`, `search`, MCP HTTP/SSE), CLI args, file
parsers, IPC (MCP stdio/HTTP, LSP), environment reads — and each vulnerability
class was checked against every source file: injection, memory/resource, crypto,
AuthZ/AuthN, data integrity, error handling, concurrency. **16 findings, all
fixed, 0 open** — the resolved detail is in `git log`. (Its summary table listed
2 High / 4 Medium / 13 Low against a total of 16; see correction 10.) O3, the
last to close, was the `read` TOCTOU identity check running only on unix, now
enforced on both platforms through one helper (`guard_not_swapped`, `1794c5a`).
Overall risk was assessed **Low**, and what the security-critical paths get
right is worth keeping that way: the `fetch`/SSRF guard uses a TOCTOU-free DNS
resolver; `SseDecoder` is memory-bounded; the credential store uses atomic
write + `0600` + cross-process locking; PKCE uses a CSPRNG verifier with SHA-256
S256; the untrusted-content envelope uses a verified-absent nonce; the secret
denylist covers `read`, `grep`, `git`, `replace`, `fileops`, `lsp_nav`,
`write`/`edit`; `canonicalize_nearest` prevents `..` escapes. No MD5/SHA1, no
hardcoded secrets, no panics on untrusted SSE input, no unbounded allocation in
hot paths. Two platform residuals were **not** findings and are tracked above:
no Windows ACL, and `O_NOFOLLOW` covering only the final component.

**Prompt architecture** (`c5e5ced`, `5f6e386`, `6274c80`, `b1a698f`, plus
`5adc9ff`, `e02cb5f`). The hermes pass's top finding — a cache breakpoint at
hrdr's own stable/volatile boundary — plus the frozen-memory defect it found
while verifying it. `system.j2` and minijinja are gone; the prompt is ten
`include_str!` markdown fragments assembled as an ordered named-section list
(`base → global_agents_md → global_memory → project_agents_md → project_memory → capability group → skills → persona → environment → sandbox`),
memory re-gathers at the compaction boundary (the one moment the prefix cache is
dead anyway), persona sits above the environment tail, and
`SystemPrompt::prefix_len_before(SECTION_ENVIRONMENT)` — a fold over section
lengths, not a substring search — is carried to the client as
`system_cache_split`. All four Anthropic breakpoints are now spent: tools,
stable prefix, system tail, rolling last message. A resumed/revived session
rebuilds the prompt so the split matches the installed text, and the
OpenAI-shape path emits the system message as two marked parts at the same
boundary.

**Model-invocable skills** (`3ffc406`, 2026-07-27). Closed the
pi/hermes/opencode finding and the defect behind it. Discovery, parsing and
expansion moved to `hrdr-agent/src/skills.rs`; `prompt::skills_section` renders
a name + one-line-description menu as `SECTION_SKILLS` (956 bytes for the nine
listed built-ins, in the cached prefix, no bodies and no source paths); a
read-only `skill` tool returns the expanded body through the same `expand_body`
a `:` invocation uses. Took pi's opt-out shape as `model_invocable: false`; did
not take hermes' "err on the side of loading" framing or its operator-side
disable list.

**OS sandbox** (issue #13, `df01afb`..`bf0ac01`, 2026-07-27). Nine slices from a
twice-verified spec, now deleted — the design lives in
`crates/hrdr-tools/src/sandbox.rs`'s doc comments and tests.
`SandboxMode {none,write,read}`, default `write`; software path-guard on 16 call
sites across nine file-tool modules; bwrap primary on Linux, Landlock fallback,
Seatbelt on macOS, software-only on Windows; degradation notices byte-pinned.
What survives as rules is under Standing constraints; what was left out is under
Sandbox follow-ups.

**Web UI** (shipped 2026-07-26, **removed 2026-08-02**). `hrdr serve` — axum
HTTP+WS, an optional embedded Dioxus SPA, three auth modes, TLS-gated remote
access — plus the `hrdr-ui` client and the `hrdr-protocol` wire crate. Deleted
outright by owner decision, along with every follow-up and parity entry it had
accumulated here; hrdr is terminal-only (see Standing constraints). `git log`
and CHANGELOG.md are its history.

**Directory trust gate** (`0f4b440`..`ccbe08e`, 2026-08-03). Replaced the
never-built plan to _scan_ `AGENTS.md` for injection with asking the user
instead: `hrdr_agent::trust` stores one canonical path per line under the XDG
cache dir, `apps/hrdr`'s `trust_gate` runs before any command, and an unanswered
directory opens in `SandboxMode::Jail` with `read_only` forced — without that
second flag `effective_sandbox` floors jail to `write`, which was found by
running the binary rather than by reading it. The question is drawn by
`hrdr-tui`'s `trust_prompt` with ratatui on the alternate screen, sharing the
header's `splash_lines` and the session's `Theme`. Three decisions worth not
re-litigating: **exact paths, never ancestors** (owner's reason: trusting
`~/Projects` must not trust a repo just cloned into it); **only the yes is
stored**, so declining is asked again rather than sticking; and the **default
selection is cancel**, so a reflex Enter opens nothing. A caution-envelope
around `AGENTS.md` was built first (`2ae5e77`) and reverted (`de9bd83`) in
favour of this — do not rebuild it. The same effort made `gather_agent_docs`
read the working directory only and moved the headless chrome's colour onto
crossterm; what it left open is under
[Tooling / agent capability](#tooling--agent-capability),
[Permissions, isolation, and state](#permissions-isolation-and-state) and
[Test coverage gaps](#test-coverage-gaps).

**Also closed and deleted along the way:** the transcript unification
(hrdr-agent owns the `Entry` model, `apply_event` builder and renderer; the
frontend renders only), the agent-logic migration (main and sub-agents on one
codepath), session retention/compression, the memory tool's design, the DRY and
seam audits (their survivors are under Considered and declined), and the
tool-robustness audit (13 items: 11 shipped, 2 dropped in re-triage).

**Code review 2026-08-04** (full codebase, low depth). Six findings, all fixed
and pushed the same day: the dangling-symlink write sandbox escape (fixed in
`canonicalize_nearest`), the compressed-session open-lock id divergence, the
`max_tokens`/`max_completion_tokens` routing on the sentinel path, the shell
spool dropping the cap-crossing line, the phantom `!command` tool block, and the
`@agent` mention splice targeting the wrong offset. The report itself was folded
into this file on the same day's docs consolidation and deleted;
`docs/code-review.md` is gone — `git log` has the detail.

**Docs consolidation 2026-08-04.** `docs/` is down to this file alone. The
performance review (findings carried into
[Performance review 2026-08-04](#performance-review-2026-08-04)), the threading
plan (carried into the threading-pass note at the top), the compaction rewrite
plan (carried into [Compaction rewrite](#compaction-rewrite)), the DeepSeek
provider plan (shipped; the one open slice is under Test coverage gaps), and the
sandbox redesign decision record (its open items were already under
[Sandbox follow-ups](#sandbox-follow-ups)) were archived into this file and
deleted. Read `git log` for what they said before this.

**Tracked elsewhere:** the Codex catalog compatibility pin is GitHub issue #2.
Issue #13 (sandbox) is shipped and should be closed.

**Sweep fixes 2026-08-05** — the four dated reports above, closed the same day:

- `674ca0f` — bare `"quota"` dropped from `USAGE_LIMIT_PHRASES` (per-minute
  request quotas are retryable again; only insufficient_quota/billing/credit
  balance/spend limit are terminal), `log_wire` takes the fields lazily (no
  per-chunk `json!` allocation when the wire log is off), and the OpenAI SSE
  loop single-parses the common chunk via a `contains("\"error\"")` pre-check.
- `f8ed179` — `GroupKill::disarm()`: the success paths disarm the process-group
  guard so a backgrounded child a command left running survives; timeout/cancel/
  error keep the armed guard. The guard is forgotten rather than set to `None`
  (dropping it would fire the kill); one leaked Windows job handle per disarmed
  command, documented in `disarm`.
- `bc31e37` — `write_config_doc` creates its temp via `owner_only_options` +
  `create_new` (a `chmod 600` config.toml survives a settings write); a
  process-local path+mtime cache serves `load_oauth_entry_at` (per-round ChatGPT
  OAuth read becomes a stat; a deleted store still reads as no credentials — the
  serve-on-delete deviation was caught in review and corrected); the
  turn-end/session hook payload+call deduped into `run_hooks`;
  `fuzzy_match(query, parts)` added with `provider://model` kept as one part.
- `37f8623` — the five app picker filters converted to `fuzzy_match` (both
  `is_subsequence` copies deleted), the autosave-error notice ×3 into
  `note_save_error`, the punctuation trim ×3 into `trim_token`,
  `command_arg_offset` shared by `arg_completions` and `file_arg_token`, and
  `paste_as_keys` shared by the editor's paste paths.
- Known residual, recorded on `disarm`: the Windows job-handle leak per disarmed
  command (clearing `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before close would
  remove it; needs a Windows CI round trip to verify, so left documented rather
  than written blind).
