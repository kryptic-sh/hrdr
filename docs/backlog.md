# hrdr backlog

**One file, and only what is still open.** A changelog says what shipped; this
says what did not, and why — work deferred, findings not fixed, decisions that
need the owner's call, things considered and declined, and coverage gaps stated
as gaps. Anything raised in a session and not finished belongs here before the
session ends. **When an entry ships it is deleted, not annotated** — `git log`
is the history, and so is every design doc that has already landed.

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

## Performance review — second pass 2026-08-04

A fresh `:perf` run over the whole tree (working tree clean at the time). Items
1 and 3 re-found the first review's still-open #1 and #5 and add specifics.
Every finding was re-verified at its cited lines before recording; one candidate
from the run was dropped (item 4).

**Status: item 1 open (needs a decision); items 2-4 dropped — not fixable as
proposed, or disproved at review time.** Each item carries its own tag below.

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

**Not settled without profiling:** whether the per-round save (item 1) actually
dominates wall time (the fsyncs are the suspect, not the serialize), the
per-frame transcript layout loop (`ui.rs:880-916`) at very long transcripts, and
item 2's real lock-contention timing.

## Tidy review 2026-08-04

Quality pass over the whole tree (clean at the time); every candidate re-read at
its cited lines, behavior-preserving only, ranked by confidence.

**Status: item 9 open (needs direction — external API); items 1-8 shipped.**

9. **Low: unused re-export `apply_cache_breakpoints`.** **[needs direction —
   removing a `pub use` from a published crate is an external-API decision]**
   `hrdr-llm/src/lib.rs:38` re-exports it; the only production caller uses
   `crate::types:: apply_cache_breakpoints` (`client.rs:1068`) and no workspace
   crate imports the re-export. Action: drop it — but hrdr-llm is a published
   crate, so removing a `pub use` is a public-API break for external consumers;
   safe only if nothing outside the repo pins it (pre-1.0 → minor bump if so).

**Dropped as not-tidy:** `agent_dirs`/`command_dirs` and
`read_dir_profiles`/`discover_commands` shape differences (change for different
reasons); the `sandbox.rs:545` home_dir copy (wrong dependency direction); the
`split_whitespace().join(" ")` idiom; `sandbox.rs:1255`'s `#[allow(dead_code)]`
is a Windows-only backend in real use.

## Correctness review 2026-08-04

`:review` (low depth) over the whole tree, split across two passes (hrd-agent +
hrd-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor + apps/hrdr). Everything
the passes suspected was disproved, shipped, or is hardening; the lists are
under Cleared and Hardening notes.

**Status: both findings shipped.**

## Deferred 2026-08-10 — strict YAML memory frontmatter

The slice shipped: `memory` frontmatter is parsed and emitted with
`serde_yaml_ng` (`parse_frontmatter` / the `Frontmatter` serialize struct in
`hrdr-tools/src/memory.rs`), a block the parser rejects is an error rather than
a silent empty memory, `load_memories` returns a `Store` carrying `skipped`, and
`flatten_line` collapses newlines at each one-line render site. The owner's live
store was migrated with the same emitter (two files needed quoting) and every
file re-verified against the strict path. Decisions worth not re-litigating:

- **The no-frontmatter path stays.** A file with no `---` block at all (legacy
  Claude Code / OKF notes) is a different supported input, not malformed YAML,
  and still reads as `type: reference` with the description inferred from its
  first non-empty line. Strictness applies only to a block that claims to be
  YAML and is not.
- **`recall` says nothing about skipped files**, by decision — it is injected
  into every turn and is not a place to spend tokens on maintenance chatter. The
  skip is reported in the `MEMORY.md` index, the scope listing and `search`, all
  of which a user reads deliberately.
- **`write` may proceed where `view`/`edit` refuse.** Unparseable content cannot
  round-trip, so `backup_if_drifted` always copies it aside first; nothing is
  lost by replacing a file that has been backed up.
- **Frontmatter is handed to the parser including its opening `---`**, which
  YAML accepts as a document-start marker. That is what makes the reported
  `line L column C` count from the top of the file rather than from inside the
  block — do not "tidy" it by stripping the fence.
- **`MAX_SLUG_LEN` is 200**, chosen against the 255-byte component cap with
  headroom for the longer `<slug>.<unix_ts>-<n>.bak` names the tool derives.
- **Not covered:** exact `recall_score` ordering among several equally-matching
  memories. The search-ranking test discriminates the shared `relevance_score`
  path; a separate ordering test was judged redundant.

## Performance review 2026-08-06

`:perf` (whole codebase — the working tree was clean) over two passes
(hrdr-agent + hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor +
hrdr-test-support + apps/hrdr). The one item left open by decision is below,
re-traced at its cited lines.

**Context — `budget`'s O(H) token estimate per round when the server reports no
usage.** `crates/hrdr-agent/src/budget.rs:122-127`
(`estimate_tokens_in_messages(&self.messages)` per round in `account_usage` when
`acc.usage` is None). Cheap per pass; hoistable into a running total, but needs
care across compaction/resume. Not worth it unless the no-usage case
(self-hosted servers) is common.

**Recorded items confirmed still present (not re-recorded):** the per-frame
full-transcript layout walk (`ui.rs:2664` `transcript_chunks` + `:912-918`
`cum` + `:974-1006` hit map — O(entries) per frame, cached bodies make per-entry
work cheap) and the per-frame cache/save pipeline (autosave turn-end, off-thread
behind a coalescer).

**Unsettled without profiling:** the per-frame transcript walk's actual share of
frame time, and the streaming-body re-render per frame for in-flight tool calls
(bounded by the block's size, but compounds with the walk for long streams).

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
this section. Items 4 and 5 are both blocked on a decision rather than on
effort; the one audit gap still open is under _Audit items needing a decision_.

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

- **THREE releases are not on the AUR: v0.11.0, v0.11.1 and v0.12.0.** Every
  other channel published each of them — the GitHub release with all its assets,
  the crates on crates.io, Homebrew, Scoop, Alpine — while
  `Publish AUR (hrdr-bin)` failed on all three (runs `30767495527`,
  `30827340210`, `31142520670`), each with
  `The AUR is down due to maintenance. We will be back soon.` and
  `fatal: Could not read from remote repository.` Nothing is wrong in this tree;
  it is one continuous outage on Arch's side, open since 2026-08-03 — the
  v0.12.0 log carries the identical banner, so it is the same outage rather than
  a new failure mode. `hrdr-bin` still reads `0.10.0-1`.

  **Only one check answers "is it back".** The package page and the RPC endpoint
  stay **up** throughout, so neither tells you anything, and SSH auth succeeding
  says nothing either (the login banner works while the git backend refuses).
  The check is `git ls-remote ssh://aur@aur.archlinux.org/hrdr-bin.git` — the
  endpoint the publish job actually pushes through.

  **The fix is three reruns, not new tags:**

  ```
  gh run rerun 30767495527 --failed   # v0.11.0
  gh run rerun 30827340210 --failed   # v0.11.1
  gh run rerun 31142520670 --failed   # v0.12.0
  ```

  Each stages the PKGBUILD and exits 0 on an empty diff, so re-running is safe;
  oldest-first keeps the AUR history sensible. Until they land, all three tag
  runs stay red on `tag-release status`, which is that gate working as intended.
  A rerun while the outage is up just fails again the same way (tried
  2026-08-07). Probed 2026-08-06, -07, -09, -10 and -11: still down every time.
  **Delete this entry once the RPC reports `0.12.0-1`.**

- **Tracked elsewhere, not here:** the Codex catalog compatibility pin
  (`CODEX_CATALOG_COMPAT_VERSION`) is GitHub issue #2, still open there.

## Deferred 2026-08-05

- **Todo panel cut off 1 row at the bottom when following — probed on the resume
  path 2026-08-07, still not reproduced, and the gap is now covered.** The
  untried variable is driven for real: two e2e probes resume a saved session
  through `resume_locked_path` (the jsonl rebuild + follow-state re-pin) and
  render at every height 11–30, with and without a finished sub-agent, asserting
  the panels' last body rows stay above the transcript area's last row. Both
  pass; injecting the reported geometry (the transcript area ending a row early)
  makes them fail in exactly the reported shape, so they are discriminating
  rather than vacuous. If the report resurfaces, the candidates remain
  `draw_chunks`' `scroll`/`inner_scroll` off-by-one at the bottom, or the
  live-panel chunk heights vs the transcript area.

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

## Deferred 2026-08-04 — threading slice 4

**Attach reads still run on the UI thread.** The `@file` reads are bounded (~100
KB per file) and one-shot at submit, while the change would convert the whole
input path (`on_key` → `submit_input` → `spawn_turn`/`send_to_subagent`) to
async — a disproportionate ripple for a stall that is imperceptible next to the
per-round save that slice 3 removed. If the submit-time stall ever shows up, the
reads in `crates/hrdr-app/src/util.rs`
(`read_attach_file`/`read_attach_dir`/`discover_commands`) are the site.

## Which built-ins should become skills — needs a decision, 2026-08-09

With both mechanisms now shipped, four built-in commands are better as Agent
Skill bundles, and the rest are not. Assessed against: skills take no arguments,
skills fire from a description match rather than user intent, and a bundle only
earns its directory when it has `references/` or `scripts/` to hold.

- **Move to skills: `cli`, `deps`, `ci`, `tickets`.** Each is portable procedure
  with no session state, and each branches by ecosystem — `deps` over
  cargo/npm/pnpm/yarn/bun/uv/poetry/go/composer, `ci` over the CI providers,
  `tickets` over `gh`/`glab`/`acli`. Today the model reads every branch to use
  one; as bundles they become a thin `SKILL.md` router plus one `references/`
  file per ecosystem, and `tickets`' search-then-comment dedup wants to be a
  script rather than prose. `cli` needs a rewrite first to drop its
  `args: [tool]` (take the tool from the task context or trailing text); the
  other three declare no args already. Bonus: installed into `~/.agents/skills/`
  they serve Codex and opencode too.
- **Stay commands, three distinct reasons.** Arguments that genuinely change the
  procedure: `review`/`audit` (`low`/`high`), `release`
  (`patch`/`minor`/`major`), `fix`, `plan`, `commit`. Outward-facing
  irreversible tail, so the model must never fire it: `release` (tag + push).
  Scoped to session state rather than to a matching description: `todo`,
  `commit`, `test`, `work`, `sweep`.
- **`perf` and `tidy` are the close call.** Both declare no args and would work
  as skills, but `sweep` orchestrates review + audit + tidy + perf as one set,
  and splitting those four across two mechanisms costs more than it buys. Move
  all four together or none — revisit if `review`/`audit` ever lose their args.

**The open question is how a built-in ships as a bundle.** `build.rs` embeds
`templates/commands/*.md` as single files (`COMMAND_FILES`); a bundle is a
directory. Codex's answer is worth copying: it embeds its system skills and
extracts them to `$CODEX_HOME/skills/.system/` at startup, guarded by a
fingerprint and a marker file so it rewrites only when the binary's copy changed
(`~/Projects/harness/codex/codex-rs/skills/src/lib.rs`, functions
`system_cache_root_dir` / `embedded_system_skills_fingerprint`). Extracting to
`~/.config/hrdr/skills/.builtin/` would let built-in bundles load through
ordinary discovery with no special case. Trade-off to settle: a first-run disk
write, and built-ins become user-editable (codex accepts both).

## Deferred 2026-08-09 — the skills → commands rename

The rename itself shipped whole (module, tool, `/commands`, dirs, recursive
namespaced names). What it left open:

- **A symlink _inside_ a command directory is not walked** — the directory
  itself being a symlink is fine. `discover_commands` builds its
  `ignore::WalkBuilder` with the default `follow_links(false)`, which is what
  makes a symlink cycle a non-issue, but walkdir follows a symlinked _root_
  regardless (`follow_root_links` defaults on, and `WalkBuilder` leaves it
  alone), so `~/.config/hrdr/commands -> ~/dotfiles/commands` — the normal
  dotfiles layout — does contribute its files. What contributes nothing is a
  link dropped below the root, e.g. `~/.config/hrdr/commands/shared -> …`.
  Pinned by `a_symlinked_dir_is_walked_but_a_symlink_inside_one_is_not`
  (`commands.rs`), which asserts both halves; the same shape holds for
  `discover_skills`. Turning nested links on would be safe as far as hangs go
  (`ignore` detects cycles and yields an error rather than looping), so if
  anyone asks for it the change is one call; it is off as the conservative
  default, not because it cannot work.
- **Nothing warns a user whose commands are still in `.hrdr/skills/` or
  `~/.config/hrdr/skills/`.** Those two paths now belong to the `SKILL.md`
  bundles (see the skills entry below), so a stray `review.md` left there is not
  a bundle, is not a command, and goes quiet. Per the pre-1.0 no-migration rule
  there is no shim and no bespoke error; the CHANGELOG's Breaking entry is the
  only notice.

## Deferred 2026-08-09 — Agent Skills (`SKILL.md` bundles)

The slice shipped whole: `hrdr-agent/src/skills.rs` (discovery, validation, the
`skill` tool), `prompt::skills_section`, the always-readable sandbox grant
(`SandboxPolicy::allow_read`), and the shared `:name` namespace in the frontends
(`hrdr_app::PromptEntry`). What it left open:

- **`license`, `compatibility` and `metadata` are parsed and preserved, and
  rendered nowhere.** They are on `Skill` because the format defines them and a
  bundle round-trip must not drop them, but no surface shows them — not the
  picker row, not the `skill` tool's output (which would spend tokens on a
  licence string the model cannot use). Decide a surface before adding one; the
  parse and its test are already there.
- **A symlink _inside_ a skill root is not walked**, same as `discover_commands`
  and for the same reason (`ignore::WalkBuilder`'s default `follow_links(false)`
  is what makes a cycle a non-issue). A symlinked _root_ is walked, so
  `~/.agents/skills -> ~/dotfiles/skills` does contribute its bundles — see the
  commands entry above for why, and
  `a_symlinked_root_is_walked_but_a_symlink_inside_one_is_not` (`skills.rs`) for
  the assertion of both halves. Turning nested links on is one call if anyone
  asks.
- **No per-skill permission model.** opencode gates skills per agent with
  `permission.skill` patterns (`internal-*: deny`); hrdr's only lever is a
  profile's `tools:` allow-list dropping `skill` wholesale, which takes the
  listing with it. Not built because nothing has asked for per-name gating yet.
- **Considered and declined: warning on a duplicate name across roots.**
  opencode logs one; hrdr does not, because discovery runs inside the TUI and
  stderr there corrupts the display (the same reason `discover_commands` is
  silent at its cap). The visibility went to the `/commands` picker instead —
  `DiscoveredSkills::shadowed` keeps the losing bundle and the row says which
  kind of shadowing it is. Revisit only if a non-TUI frontend appears.
- **hrdr ships no built-in skills**, only built-in commands. Deliberate: a
  built-in belongs in `src/templates/commands/` where it is one file and no
  directory bundle.
- **A jailed agent holds no `skill` tool today, so the always-readable grant
  only helps it `read` a `SKILL.md` by path.** `ToolRegistry::cap_to_jail_set`
  caps jail to `JAIL_TOOLS` (`read`/`grep`/`find`/`ls`/`tree`), which excludes
  `command` and now `skill` alike, and the prompt listing is gated on the tool —
  so the "listing it cannot open" the grant was asked for cannot arise while
  that cap stands. The grant is still the right shape (it is what makes the tool
  addable to the jail set a one-line change, and what lets a jailed agent open a
  bundle a user names), but the decision to leave `skill` out of `JAIL_TOOLS` is
  worth revisiting deliberately rather than by default.
- **`prepare_outgoing_tracked` re-walks both command and skill roots on a `:`
  submit** (`crates/hrdr-app/src/util.rs`) — the walk sits inside the
  `input.trim_start().starts_with(':')` guard, so an ordinary message costs
  nothing; only a message that opens with `:` pays for it, and it pays on top of
  the cached copies the TUI already holds for the popup (`App::commands` /
  `App::skills`). That is the pre-existing command behaviour extended, not a
  regression, and it has not been measured.

  **Open: whether the send path should use the frontend's cached sets instead of
  re-walking.** The trade is immediacy against I/O. Today a command file added
  mid-session expands the moment you type its name — the caches are only
  refreshed by `/reload`, `/cwd` and `apply_cwd`, so reading them instead would
  mean a newly written `.hrdr/commands/foo.md` is `:foo` verbatim until one of
  those runs. Against that, the walk is per-`:`-submit filesystem work that the
  popup has usually just done. It also touches the seam this path now carries:
  the caches are already discovered with the session's `ProjectInstructions`, so
  switching to them would keep the trust gate honoured either way. Not changed;
  needs a call on which behaviour is wanted.

Coverage gaps, stated plainly:

- **Not tested: discovery from the user-scope roots.** Every discovery test uses
  project scopes under a `tempfile::tempdir`, because `$HOME` is one sandboxed
  directory shared by all tests in a process and a real bundle planted there
  would leak into other tests' discovery. The jail-read test
  (`a_jailed_agent_may_read_the_user_skill_roots`) creates only an empty
  `~/.claude/skills` directory for that reason, and
  `codex_home_overrides_the_default_codex_root` tests `skill_dirs` (the pure
  path function) rather than a planted bundle. What is therefore unexercised is
  the walk of a user root, not the roots' composition.

## Deferred 2026-08-11 — media attachments

Images and PDFs ship end to end: `hrdr_llm::media` renders them for all three
dialects, `@file` and `Ctrl+]` construct them, blobs persist beside the session,
and `estimate_tokens_in_messages` prices them. What the six slices left open:

- **What each path may attach is still spelled differently, on purpose.** The
  user writes `@shot.png` and gets `@dir/`, todo refs and `:command` expansion
  with it; the model passes an explicit `attachments: [path]` array. Reusing `@`
  for the model was rejected: the expander lives a crate above `hrdr-agent`,
  swallows every failure (right for a human who can see their own typo, wrong
  for a model that would otherwise delegate a task with no picture), also
  inlines text files, and `@` is ordinary punctuation in prose a model writes.
  Both feed one builder, so the difference is input rather than a second
  implementation — pinned by `the_two_paths_build_the_same_message`.
- **A real Windows liveness probe is still the better fix for `store_lock`.**
  The staleness age is now per-`StoreKind` (`SmallFileRewrite` keeps 60s,
  `BlobStore` gets a span sized from a save's own worst case), which is what
  keeps a save's lock from being stolen mid-write on a platform where
  `process_alive` cannot tell. But the age only matters at all _because_
  `process_alive` has no dependency-free probe on Windows and reports every pid
  dead; everywhere else a live owner is never reaped whatever its age. An
  `OpenProcess`/`GetExitCodeProcess` probe would make both ages a formality —
  and needs a Windows API dependency, which is the owner's call. Not taken here
  for that reason alone.
- **Half the cross-process lock coverage is unix-only, and one thing is untested
  everywhere.** `store_lock`'s tests now re-execute the test binary
  (`LockHolder`, driving the `#[ignore]`d `child_process_holds_the_lock`), so
  `O_EXCL` exclusion between real processes and the guard's cross-process
  release are checked on every platform.
  `a_dead_holders_lock_is_reaped_by_the_next_process` is `#[cfg(unix)]`: reaping
  turns on `process_alive`, which on Windows answers `false` for every pid, so
  there it would pass while proving nothing. What no test covers on any platform
  is a lock genuinely aged past its window in real time — every staleness test
  backdates the timestamp, so the clock arithmetic is exercised but a minute of
  real waiting is not.
- **Token estimates are charged at Anthropic's high-resolution tier for every
  dialect** (`CHARGED_MAX_EDGE_PX` in `media.rs`). OpenAI uses 32×32 patches
  with its own budgets, so one formula is an approximation either way; the
  expensive tier was chosen because under-counting is what lets a request
  overflow its window. Revisit only with a real over-compaction complaint, and
  note the per-attachment cost is cached at construction, so making it
  model-aware means caching dimensions instead and pricing at estimate time.
- **A PDF's page count is a byte scan for `/Type /Page`.** A compressed page
  tree (most real PDFs) falls back to file size at 50 KB/page. Both err high.
- **Idle and delivered are one state for a sub-agent, which is why the pane
  refuses on idleness.** Established while making the pane refuse to steer a
  finished sub-agent: `delegation.rs` registers every sub-agent `running: true`
  and only `spawn_background`'s guard clears it, so `!running` is reachable only
  by the run having ended — and that same block fills the `BackgroundTask` row's
  `result` in the breath before it. `turn_state::drain_background` flips the
  entry's `delivered` later (when the parent's next request folds the row in),
  so there IS a window where an entry is idle and not yet delivered, but the
  report the parent will receive is already fixed in that window: nothing a
  later turn produces can reach it either way. So `send_prompt`'s refusal —
  defined on idleness — implements the owner's rule, which names delivery,
  without widening it. If a sub-agent ever gains a path to idleness that is not
  "finished", that equality breaks and this is the note that says so.
- **`Ctrl+D` on a box holding only a pasted image still quits**, dropping the
  bytes: it reads `editor.content().trim()` directly rather than
  `App::composer_is_empty`, which Ctrl+C and Ctrl+S now share. Left alone
  deliberately — Ctrl+D is the shell-style EOF quit, and quitting discards the
  composer whatever is on it — but it is the third answer to "is the composer
  empty" and the one that disagrees.
- **Considered and declined: auto-attaching a pasted file path.** Dragging a
  file onto a terminal and typing about a path are the same keystrokes, so
  `Event::Paste` leaves text as text. The two deliberate spellings are `@path`
  and `Ctrl+]`, which reads the clipboard's file flavour.
- **Considered and declined: a temp file for pasted bytes.** They live on the
  composer behind the `Arc` the attachment already holds; dropping the vector is
  the cleanup, where a file would need a lifetime policy for every way a message
  can end and would litter after the first missed one.
- **Not covered:** no test drives a real provider with a real image. Every
  dialect assertion is on the JSON hrdr builds, so the shapes are pinned against
  the docs, not against a live endpoint that accepted them.

### Left by the vendor-doc audit, 2026-08-11

Every field of `anthropic_block` / `responses_item` / `openai_part` was checked
against the current vendor docs and matches them; the per-image ceiling was
raised to Anthropic's documented 10 MB (it had been 5 MB, which is the
Bedrock/Google-Cloud number, and hrdr never speaks the Messages API to either).
What that audit did not close:

- **No dimension check anywhere.** The vision docs cap an image at 8000x8000 px,
  and apply a stricter per-image dimension limit once a request carries more
  than 20 images ("resize each image so that neither dimension exceeds 2000
  px"). `image_dimensions` already reads width and height for the token
  estimate, so the check is cheap to add — it was left out because a refusal
  hrdr invents is worse than a provider error hrdr reports, and nobody has hit
  it.
- **The image-count cap is the conservative branch.** 100 is the documented
  limit for 200k-context models; other models take 600. The gate is handed a
  model name, not a window, so it applies the smaller one.
- **OpenRouter's parser can read a PDF that the model itself cannot, and the
  gate refuses that request first.** `file-parser` turns a PDF into text for any
  model, so a text-only model on OpenRouter could take one — but
  `check_attachments` refuses on the models.dev modality list before the body is
  built, so the plugin only ever rides on requests for models the catalog does
  not know. Widening the gate for one provider means teaching it which provider
  it is talking to, which it deliberately is not; leave it unless someone asks.
- **Doc is silent: whether OpenAI's chat-completions `file.file_data` wants a
  bare base64 string or a `data:` URL.** The API reference says only "The base64
  encoded file data"; the PDF guide's examples are all Responses-API. hrdr sends
  the `data:` URL, which is what OpenRouter documents explicitly for the same
  field, and what the Responses examples use.
- **DeepSeek and OpenCode Zen document nothing about attachments.** DeepSeek's
  chat-completion reference types a user message's `content` as a string with no
  content-part array at all, so its models are text-only and the modality gate
  is what refuses them; Zen's page describes a gateway to other vendors' models
  and never mentions image or file input. Neither claim comes from a vendor
  sentence about attachments, because neither vendor writes one.

## Deferred 2026-08-10 — the guardrail audit

The slice shipped: git global options no longer defeat the git rules, whole-tree
`checkout`/`restore` is caught in every spelling, `-p` counts as interactive,
the pipe rule covers non-shell interpreters, delete-by-expansion and
`find … -delete`/`xargs rm` are refused, an uncompilable `[[guardrails]]` entry
is reported, and the prompt-to-rail drift test runs both directions
(`PROMPT_PROHIBITIONS` in `prompt.rs`, every row either a rail-enforced command
or a mandatory prompt-only reason). What it left open:

- **`git checkout -p` is not caught**, only `rebase`/`add`/`commit` carry the
  interactive rule. It is patch mode and equally TTY-dependent; the prompt does
  not name it either, so neither half covers it. One alternation to widen if it
  ever shows up.
- **`:(top)`, git's long-form spelling of the `:/` repo-root pathspec, is not in
  the whole-tree set.** Rare enough that widening the alternation was judged not
  worth the false-positive surface.
- **`git rm $FILE` and `npm rm $PKG` trip the delete-by-expansion rule.** The
  rule requires `rm` in program position (so `docker run --rm $IMAGE` passes),
  but a leading subcommand word still matches. Refusing them is consistent — it
  is the same delete built from a variable — and it is a slightly wider blast
  radius than "the `rm` program". Revisit if it fires on real work.
- **Considered and declined: extending the whole-tree `rm` list.**
  `rm -rf ~/Projects/scratch`, `rm -rf ../build` and `rm -rf /home/<user>/x`
  stay allowed. Deleting a named directory is ordinary cleanup, and refusing
  every path under home or above the cwd stops far more real work than it saves.
  The rules catch the shape where the model cannot SEE its target instead. The
  decision is written on the rule in `default_guardrails()` so it is not
  re-"fixed" later.
- **Considered and declined: catching `rm -rf "$DIR"/*`.** A variable as a path
  prefix is legitimate (`rm -rf "$TMPDIR"/*` is ordinary cleanup), and no rail
  separates the two. Recorded as a `PromptOnly` row with that reason.
- **The sub-agent restore ban is kept deliberately**, now with its reason in
  `subagent_write.md`: a sub-agent shares the parent's tree and cannot tell the
  parent's uncommitted work from its own, so the look-at-the-diff-first
  procedure `write_main.md` gives the main agent is one it cannot actually run.

## Top of the list

What is left here needs a decision, not work.

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

- **Fuzzy `old_string` matching that preserves unchanged lines — the cheapest
  useful subset shipped 2026-08-07 (`fb407e3`).** `edit` now retries the match
  with per-line normalizations (trailing whitespace trimmed; smart quotes,
  dashes and figure/NBSP spaces mapped 1:1 — no NFKC, no new dependency) and
  applies a unique match, reporting it in the result so the model sees what
  changed. Deliberately NOT taken: internal space-run collapsing (indentation
  and space-count differences still get the near-match message — a collapse
  would break the 1:1 byte-offset recovery), and pi's duplicate-line alignment
  machinery (unneeded: the span splice replaces only the matched region). The
  item was deleted, not annotated — `git log` has the detail.
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
- **Project commands and skills shadow built-ins by name.** `command_dirs` and
  `skill_dirs` both start with the working tree's own roots (`.hrdr/commands`,
  `.claude/commands`, `.opencode/command(s)`; `.hrdr/skills`, `.agents/skills`,
  `.claude/skills`, `.codex/skills`), all under the writable cwd, and a project
  entry is discovered _before_ the built-ins and wins the name — so a project
  `.hrdr/commands/commit.md` silently replaces the vetted `:commit`. Re-runs on
  every `set_cwd`/`clear` and in every new `Agent::new`. Same shape as
  `AGENTS.md` but with a **weaker second use** — a project command is a
  convenience where `AGENTS.md` is a core feature — which makes it the stronger
  candidate of the two if either is closed. This is the **trusted** case only;
  the untrusted one is the bullet below.
- **Every frontend discovery must take the agent's own
  `Agent::project_instructions`, never `ProjectInstructions::Load`.** The trust
  gate's answer lives on the agent, derived once in `Agent::new` from the
  effective sandbox mode; a frontend that passes `Load` to `discover_commands` /
  `discover_skills` is a second answer to the same question, and it is the one
  the user types into. That is what the `:` completion popup, the `/commands`
  picker and `prepare_outgoing_tracked` all used to do, so a declined
  directory's commands and skills were offered and expanded into the sent
  message anyway. The seam now is: `Agent::project_instructions()`,
  `App::project_instructions` (read once at construction — the send path cannot
  take the agent's lock, since a running turn holds it, which is exactly when a
  steer is typed), and `CommandHost::project_instructions()`, which is
  deliberately required rather than defaulted so a new host cannot forget to
  answer.

  Coverage gap:
  `a_jailed_session_neither_offers_nor_expands_the_projects_own_names`
  (`hrdr-tui/src/app/e2e.rs`) drives `/reload`, the `/commands` picker and a
  `:name` submit under a jailed harness, and
  `a_skipped_project_expands_neither_its_commands_nor_its_skills`
  (`hrdr-app/src/util.rs`) covers the send-path seam directly. The other two
  frontend rediscovery sites are **not** exercised by a jailed test and are
  correct by inspection only: `TuiHost::cwd_changed` (`/cwd`, which reaches it
  only for a directory the user _has_ trusted) and `App::apply_cwd`, reached
  from `apply_session` when a resumed session names a different cwd — that path
  asks the trust store nothing, so it is the one that moves a jailed session's
  cwd without a trust answer, and the one worth a test if this is revisited.

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

**Most of this list closed on 2026-07-30 with the sandbox redesign**; what the
deletions taught is under [Standing constraints](#standing-constraints). What
remains open:

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
- **Command follow-ups** (the feature shipped 2026-07-27 as "skills" and was
  renamed 2026-08-09 — `hrdr-agent/src/commands.rs` plus
  `prompt::commands_section`). Left out on purpose: no `command` usage signal
  (nothing records whether the model ever loads one, so there is no evidence for
  or against the listing's wording); no categories, unlike hermes'
  category→skills grouping, which only pays off past a few dozen entries; and a
  body still arrives as one tool result, so a procedure over
  `COMMAND_OUTPUT_MAX_BYTES` spills to a file the model must read.

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
  `begin_command_selector`, `begin_effort_selector` and `begin_theme_selector`
  all carry default bodies that list the choices as text "for a frontend without
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
- **A disarmed process group leaks one Windows job handle.** Recorded on
  `GroupKill::disarm`: clearing `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before
  closing the handle would remove it, but verifying that needs a Windows CI
  round trip, so it is documented rather than written blind.

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
- **`serve_once` takes `&'static str`**, so every mock SSE body must be a
  literal (the stop-reason tests `Box::leak` theirs). Fine today; it makes a
  table-driven stream test awkward. `impl Into<String>` is a one-line change
  when someone needs it.
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

---

## Small corrections owed in hrdr-llm

Raised while closing the provider-divergence audit, out of those slices' scope,
re-verified against the tree.

- **`parse_imf_fixdate` ignores the weekday.** It splits on `", "` and discards
  the prefix, so `Xyz, 06 Nov 2999 …` parses fine. Laxer than RFC 7231 and
  harmless — the weekday is redundant with the date — but worth knowing before
  someone "fixes" a test that relies on it.

---

## Provider divergences left open by decision

Each is pinned by a test and commented in the source; what is missing is a
decision, not work — except the last, which is a missing feature.

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
- **`Delta` deserializes only `reasoning_content`**, so providers streaming
  `delta.reasoning` (several OpenAI-compatible gateways) have their reasoning
  silently dropped. A missing feature, not a missing test — do not re-derive it
  as a coverage gap.

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

`hrdr-app/src/commands/dispatch.rs` was read in full for the first time, with
its sibling modules, the `CommandHost` trait and its call site. The finding
below is traced from source — nothing was reproduced at runtime.

**`/cwd` writes one agent and reads another.** `dispatch.rs` resolves the
argument against `host.cwd()` and writes `host.agent()`, and those are different
agents (`cwd()` is main-derived, `agent()` is the active pane). On a sub-agent
pane, a relative `/cwd` resolves against main's cwd, moves only the sub-agent,
then repoints the global chrome as if main had moved. A bare `/cwd` afterwards
contradicts the status bar. `/status` has the same split — main's cwd beside the
active agent's message count. Needs a call: derive the base from `host.agent()`,
or add `active_cwd()` so both halves name one agent.

**Checked and fine, so nobody re-derives it:** the `bool` contract is sound —
every in-arm return is `true`, only the unknown arm and the non-`/` guard return
`false`, so nothing swallows input or leaks a command. No byte-offset `&str`
slicing, no indexing, no `unwrap`/`expect` outside tests. `/etc/hosts` correctly
falls through to the model. `parse_msg_range` rejects the degenerate ranges. The
`/add` size cap that looks duplicated is not — `read_attach_file` gates on
`metadata().len()`, which is `0` for procfs, so the post-read check is the real
backstop. No arm reads state, awaits, then writes back stale. `dispatch` is
synchronous and holds no lock across an await.

**Not covered by this pass:** no runtime exercise. The concurrency claims are
reasoned from lock scopes, not from a racing repro. TUI modal/picker key routing
was judged only at its `begin_*` entry points. `login.rs`,
`commands/prompt_commands.rs`, `completion.rs` and `sessions.rs` were read only
where dispatch calls into them. `open_system_handler`'s Windows and macOS arms
are `#[cfg]`- gated and were not compiled.

---

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

- **Auto-compaction delays the reply, not the message.** When a session opens a
  turn against a near-full context, the designed auto-compaction runs before the
  first request, so the submitted message appears immediately and the model's
  answer waits on the summarization call. Intended; noted because it reads as a
  hang.

---

## Hardening notes

Correct today, fragile — explicitly not vulnerabilities, and each a live
property of code that still exists. Read before "simplifying" any of them.

**From the 2026-08-04 correctness pass:** PID-reuse vs stale session/store locks
(`session.rs:456`, `store_lock.rs:172`) — a recycled PID keeps a lock "alive"
forever, giving a spurious permanent busy error; `openai_refresh` requires
`refresh_token` in the response (`oauth.rs:361`) — a spec-minimal server forces
a re-login; the wrap-up round shares `overflow_compacted`
(`turn_loop.rs:524, 842`) — a second overflow on the forced wrap-up errors the
turn; `cost_partial` is a process-lifetime latch (`budget.rs:44`) that
`reset_session_cost` doesn't clear; `parse_scalar` quote-stripping loses
legitimately edge-quoted values (cosmetic until finding 1's edit truncates).

**From the 2026-08-05 correctness pass:** history persist spawns one OS thread
per `record` (`history.rs:148-163`; chain joins previous handle so writes never
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

**From the 2026-08-05 security audit:** the verification-gate prompt section is
repo-authored content the trust gate does not cover — `Gate::detect` runs
unconditionally (`lib.rs:1818`, `:2211`) and `gate_section`
(`prompt.rs:574-612`) renders the parsed CI commands as authoritative fact ("run
them … before you report work finished"), even from an untrusted directory;
inert today because `JAIL_TOOLS` (`hrdr-tools/src/ lib.rs:1549` =
read/grep/find/ls/tree) has no shell or `verify` and the runner line is skipped
when `verify` isn't registered (`prompt.rs:582-589`), but it is the one
instruction surface the trust gate does not protect, and it becomes a live
injection vector if jail ever gains `verify`/`shell`; `atomic_write` write-path
TOCTOU (`tools/mutation.rs:149-154` — admitted in the comment; requires a
hostile process racing the agent's own edits); MCP tool descriptions ride into
the tools cache block unwrapped (`mcp/client.rs:366-370`, `Box::leak`) — a
compromised operator-installed server can steer the model through its
descriptions, where results are wrapped as untrusted; `/export` writes to any
absolute path the user names (`conversation.rs:29` — equivalent to the user's
own shell redirection, but the transcript contains model output).

**From the 2026-08-06 correctness pass:**

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
- `build.rs:29` — `to_string_lossy` on command filenames: a non-UTF-8 filename
  in `templates/commands/` emits a lossy `include_str!` path and fails the build
  with a confusing error (shipped set is ASCII).
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

**From the 2026-08-06 security audit.** One claim was demoted from a finding to
hardening, with the trace that disproved the repro: the claim
"`SandboxMode::Read` does not confine the `memory` tool's writes"
(`memory.rs:253, 292, 305` use bare `fs::write`/`remove_file`; in `Read` mode
`writable_roots` is empty by construction, `sandbox.rs:198`). Traced unreachable
in the default configuration: `effective_sandbox` floors a write-capable
session's `read` request to `Write` (`config.rs:1797`, test-pinned at
`:2737-2745`), so `SandboxMode::Read` is only ever entered with
`read_only = true` — and the read-only tool scoping withholds `memory` (a write
tool; `Tool::read_only` defaults false, `lib.rs:1289`, and `MemoryTool` never
overrides it). Residual, recorded as hardening below.

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

---

## Cleared: suspected, traced, safe

Suspected by a dated pass, traced, and found sound. Recorded so nobody
re-derives them.

- **Correctness 2026-08-04.** SSE decoder overlong-line/UTF-8/EOF handling;
  jsonl torn-line rollback and offsets; token/cost arithmetic clamps;
  cache-breakpoint offsets (char-boundary guarded); retry budgets; the OAuth
  single-flight coordinator; registry turn generations vs cancelled runs; TUI
  completion `items.len() - 1` underflow (guarded by non-empty lists); save
  pipeline lost-wakeup (`Notify` permit semantics); `truncate`/`middle_bounds`/
  `collect_lines` boundary math; `truncate_inline`; history dedup/cursor math;
  guardrail regex escapes (`--force-with-lease`, `git checkout .`); shell-arg
  recursion bounds; sandbox canonicalization/write-escape/linked-worktree
  grants/ Landlock/Seatbelt; memory slug/traversal and index skips;
  completion-offset char boundaries; login/OAuth state checks;
  `mega_turn_tail_start` reachability (it is reachable — the sub-agent opener is
  a real user turn).
- **Correctness 2026-08-05 (half A).** fork jsonl copy (`std::fs::copy`
  preserves 0600; `Session::save` never truncates the sibling jsonl; `load_path`
  folds it); retry taxonomy (typed errors short-circuit on kind, so the phrase
  scan can't override a correctly-classified `Transient`;
  `is_context_overflow`'s `UsageLimit => false` arm is reached before any body
  scan); `compact()` indexing (`before <= 2` early-return and `tail_start >= 2`
  keep `messages[1..]`/`messages[tail_start..]` in bounds); `thinking_budget`
  ceiling math (clamped into `[1024, max_tokens-1024]`); `model_version` segment
  rejection (snapshot dates read 4.0, not 4.20250514); `parse_imf_fixdate`
  same-era past dates (`saturating_sub` → `None`); wire-log test hooks
  (visibility-only, no process-global state); prompt assembly /
  `prefix_len_before` (char-boundary split guarded); config persistence
  (read-modify-write under `StoreLock`, unique sibling temp + rename);
  `discover_commands`/`read_dir_profiles` caps (off-by-one checked); `pane.rs`
  sync cursor (no replays; main pane never pruned).
- **Correctness 2026-08-05 (half B).** history draft-stash vs vim's trailing
  newline (symmetric per engine); Enter-path reservation dropped before the
  first write (`reserve_session_id` early-return); save-coalescer lost wakeup
  (`Notify` permit semantics); `/temp` edges (`is_finite` + `0.0..=2.0` covers
  2, 0, nan, inf, 1e40; `default`/`reset` clears); `/export` hardening (second
  token and existing path refused before write); `replace` capture-expansion OOM
  (`refs × match_len` over-estimates and refuses before `expand`); `grep`
  multiline line math; `read` coverage/CRLF/budget; `tree` depth consistency;
  SSRF guards (connect-time resolver closes the rebinding TOCTOU; IPv4-mapped
  v6, 100.64/10, link-local, unique-local covered); LSP framing
  (`take(remaining+1)`, Content-Length cap, colliding-id skip); mouse drag band
  clamps; arrow history walk (deliberate behavior change, symmetric);
  `/copy msg` huge-range scan (breaks at first `None`); `todo` evidence gate and
  id minting; `edit` CRLF recovery; proc.rs pid guard (`pid > 1`); MCP pending
  bookkeeping (`PendingGuard` removes id on failure/timeout).
- **Security audit 2026-08-05 (half A).** `5bc2e5d` memory-drift backup
  (`std::fs::copy` preserves the source's bits; `.bak` lands in the same memory
  root and can never be ingested — `load_memories` loads only `*.md`; the stem
  is `safe_stem`-sanitized, no traversal); `f901485` fork jsonl copy (copy
  preserves 0600; `outcome.id` is a slugified `unique_session_id`, so the copy
  target can't leave the session dir); `7eca4b7` repo-plan hunting (a `base.md`
  fragment instructing the model; no code reads any file — follow-up reads go
  through the sandboxed tools); `2a78ec2` `#[doc(hidden)] pub` test hooks
  (visibility-only; `serve_response` binds 127.0.0.1 ephemeral;
  `set_backend_for_test` mutates one instance); `12fb89c` taxonomy beyond the
  quota needle (typed errors short-circuit on kind; mid-stream downgrades fire
  only after a `Transient` classification; saturation on every usage counter);
  config.rs remainder (`deny_unknown_fields`, per-field bounds, absolute-only
  writable roots, alias-collision refusal, StoreLock read-modify-write);
  agents_dir/commands (bounded discovery, fail-closed frontmatter,
  extension+stem path use only); trust.rs (0600 store, exact canonical match, no
  ancestor trust, idempotent check-then-append); anthropic/codex stream parsing
  (SSE capped 32 MiB, unknown indices ignored not defaulted, unknown stop_reason
  passes through with a warning, `Retry-After` clamped); chatgpt_models (10 MiB
  body cap, redirect policy none, `AuthFailed` never serves stale, cache stores
  only sanitized rows); prompt.rs (AGENTS.md gated on metadata size before read,
  no ancestor walk, jail passes `ProjectInstructions::Skip`, bounded memory
  index); sweep_sessions (auto-named only, open-lock held for the whole action,
  sibling jsonl + subagents/ removed with the `.json`, unparseable files left
  for `/doctor`); cwd_slug/sanitize_name (alphanumeric + hash suffix, no path
  escape).
- **Security audit 2026-08-05 (half B).** `/export` path traversal (the argument
  comes from the TUI input box a human types — no model or headless path reaches
  dispatch; existing file refused, so no overwrite through a symlink to an
  existing target); `/temp` hardening (`is_finite` + `0.0..=2.0`,
  `default`/`reset` clears); mouse select-to-copy (anchor/head clamped, band
  read from the painted buffer only); arrow-history walk and Enter-path lag
  (`Reservation`'s `Drop` releases the id lock on every path); hjkl 0.41
  (mechanical `Buffer`→`View` migration, disabled default render fields); SSRF
  (`SsrfGuardResolver` closes the DNS-rebinding TOCTOU; alternate IP encodings
  normalized by `getaddrinfo` before `is_blocked_ip`; redirect targets covered;
  bodies capped); MCP (10 MiB body caps, per-message SSE cap with
  `buffered_bytes()` reset, colliding server-initiated ids rejected, stdio
  writes bounded by the 64-slot channel, `PendingGuard` removes ids on
  failure/timeout); LSP (16 MiB frame / 16 KiB header / 64 headers caps, errors
  degrade to "no diagnostics" never a failed edit, `uri_to_path` percent-decodes
  lossy-never-panics); shell tool (`bash -c` arg is one argv element by design,
  output bounded per-line and in-memory, secret filter + diff redactor on every
  line, `!command` unsandboxed but still filtered); read/edit/replace OOM and
  swap-TOCTOU guards; main.rs (trust gate runs before `Agent::new`, jail forces
  `read_only` — the second flag is what makes the jail hold).
- **Correctness 2026-08-06 (hrdr-agent + hrdr-llm).** the budget-reset loop
  (`turn_loop.rs:538-550` — `while step < max_steps` guards, so
  `max_steps - step` cannot underflow; the steer-reset round-counting matches
  the in-tree test); `drain_steering`'s `bool` (the no-hook/recall delivery path
  cannot return `Err`); compaction `context_after` (computed after
  `self.messages` is replaced, over the same `[system, summary, tail]` +
  `tools.defs()` the next turn sends; `saturating_add`; `0` only on the no-op
  path the TUI ignores); the `config.rs` UI-key removal (the two key lists are
  pinned together by `the_agent_accepts_every_ui_key`); the `Tool.expanded`
  removal (always `#[serde(skip)]`, never serialized, no construction sites
  left); the `edit | replace → ToolBody::Diff` splice (both tools return a
  `unified_diff`; the renderer only colors lines); the command
  registry/`build.rs` generation (`files.sort()` deterministic, per-file
  `rerun-if-changed`, CRLF/BOM/invalid-YAML handled, tested); the unchanged
  hrdr-llm decode paths (sse/retry/capped_read/fs skimmed — no new issues).
- **Correctness 2026-08-06 (hrdr-tui and the rest).** the
  frozen-spinner-in-summary suspicion (animated bodies bypass `BLOCK_CACHE` via
  `lazy_height`, rebuilt per visible frame); summary-vs-head-call cache-key
  collision (separate thread-local caches, and the 5th `BodyKey` element
  separates preview from full); `tool_group_head` walking past the head (reverse
  `take_while(group_absorbs)` + `.last()` lands on the first tool); a groupable
  tool after a group rendering standalone (impossible — `tool_group_end` absorbs
  it); `content_rect` band math vs painted content; `split_add_remove`
  byte-indexing (all indices from `find('+')` + ASCII runs, all char
  boundaries); `browsing()` underflow (`pos ∈ 0..total` always);
  `classify_diff_line` vs `---` headers; the uncapped edit/write/replace result
  diffs (deliberate, tests updated); compaction-gauge no-op; `MAX_DIAG_LINES`
  8→10 arithmetic; row-hit misplacement on wrapped rows (every hit in a call
  block carries the same `ToggleToolCall(idx)` — a wrap-misaligned rect makes a
  row dead, never toggles the wrong call).
- **Security audit 2026-08-06 (hrdr-agent + hrdr-llm).** OAuth CSRF/state
  (constant-time compare; the `!=` probe only decides whether to keep
  listening); PKCE (RFC 7636 vector-pinned); credentials on disk (0600/0700,
  `create_new` + atomic rename, locked RMW); token leakage in errors/logs
  (sanitized bodies, no auth headers in the wire log, 8 KiB error cap);
  cross-provider key leak (`resolve_api_key`'s parent fallback gated on
  identical `base_url`); path traversal (session ids sanitized, cache names
  slugged, sub-agent `cwd` canonicalized + containment- checked); unbounded
  retries (`RetryBudget` caps at 10 attempts/≈6¼ min, `Retry-After` clamped);
  SSE/JSON overflow (32 MiB per-event caps, truncated- event rejection,
  char-boundary-guarded slices); auth-header confusion (provider-configured auth
  headers stripped from `extra_headers`); JWT account id without signature check
  (feeds only a routing header); prompt-injection framing (AGENTS.md labeled +
  TOFU-gated per directory, commands source-labeled).
- **Security audit 2026-08-06 (hrdr-tools and the rest).** sandbox escape via
  `canonicalize_nearest` (lexical `..` normalization, 40-hop symlink budget,
  regression-tested); symlink race in temp writes (`create_new` + rename; the
  in-place fallback is deliberate); SSRF in `fetch` (connect-time resolver
  filters internal/loopback/ link-local/CGNAT, no TOCTOU; `::` unblocked but
  fails at the OS level); MCP transport (bounded reads, id-space separation,
  host-match SSRF guard, group- killed children); shell injection in hooks
  (`Shell::quote` substitution, metacharacter tests); command injection in
  `shell` (the command string IS the intended payload; guardrails documented as
  a non-boundary); path traversal in the file tools
  (canonicalize-before-root-check); memory-tool path escape (`safe_stem` +
  component `Normal` check); secret-file exfiltration (structural deny-list
  post-canonicalization in read/write/edit/grep/attach/shell-line filter);
  terminal escape injection (ratatui cell buffers, ANSI-stripped for the model);
  `@file` expansion (secret deny + handle-identity TOCTOU check + 100 KiB cap);
  trust-gate bypass (headless auto-jail; ask is interactive-only); Windows
  re-exec token (hrdr-emitted only, fatal if not lowered);
  config/session/history files (0600 atomic writes, 10 MiB history cap, lenient
  TOML parse); uncontrolled allocation (all reads byte-capped, `replace_all`
  projected before allocating).

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
- **Two project-dir walks** — `commands.rs::command_dirs` and
  `prompt.rs::gather_agent_docs` both walk cwd → `/` plus XDG dirs. **Now both
  in `hrdr-agent`** (they were split across crates when this was first judged),
  so a shared iterator is cheaper than it was; still ~15 lines each with
  diverging payloads (command dirs vs `AGENTS.md`), so still judged borderline
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
  case is the `watch` tool: start a check, end the turn, get woken when it
  flips. Note the second-order cost hermes paid: cron sessions poisoned
  session-search ranking badly enough to need a demotion tier. The one detail
  worth stealing if hrdr ever ships anything scheduled is the _posture_ — cron
  runs get `skip_memory=True` unconditionally because _"cron system prompts
  would corrupt user representations"_, and approvals fail **closed** there.
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

**Deliberate mirrors checked and left alone** (tidy pass 2026-08-06) —
`render_unfinished_todos` vs `render_todos` (outputs genuinely differ;
acknowledged in a comment); `unix_millis` vs `unix_now` (different units);
hand-rolled Levenshtein in `models.rs` (justified as not worth a dependency);
`usage_key` via `ModelRef`'s Display (would canonicalize providers and change
store keys — NOT behavior-identical); `cached_body` vs `cached_block` (different
maps/types, a generic is more machinery than it removes); shell overflow-file
naming vs `save_overflow` (shell must keep the handle open — a streaming design
the helper doesn't support); `PlainEngine::paste` override (deliberately more
efficient than the default); `ProcessGroup`/`GroupKill` two-wrapper design;
`mcp::parse_sse_for_id` with its explained `#[allow]`.

**Dropped by the 2026-08-05 tidy pass:** the `sandbox.rs:545` `home_dir` copy
(wrong dependency direction — already recorded), the `now_ms` one-line
delegations in `oauth`/`chatgpt_models`/`login` (residual of the already-fixed
item 1), the `ui.rs` picker-renderer shape (each differs in fields/dimensions;
extraction speculative).

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
- **A command the model can load is still the user's procedure**
  (`hrdr-agent/src/commands.rs`, `prompt::commands_section`).
  - _The listing is a menu, never the content._ Name + one-line description
    only; bodies come from the `command` tool when one applies. Under the byte
    budget descriptions are dropped tail-first and **names always survive** — a
    name the model cannot see is a command it can never load.
  - _No source paths in the listing._ They name the per-agent worktree, so they
    would differ between sibling sub-agents and push per-agent bytes into the
    shared cache prefix. The tool's own result names the source, where it costs
    nothing shared.
  - _A command body is instruction, and it is project-authored._ It reaches the
    model as tool output — which the base prompt otherwise calls data, never a
    command — so the result frames it explicitly as the user's/project's
    instructions and names the source. Same trust class as `AGENTS.md`, and the
    same open exposure (an untrusted clone's `.hrdr/commands`).
  - _`model_invocable: false` is a boundary._ Such a command is unlisted **and**
    refused by the tool, with an error telling the model to ask the user to run
    `:name`. Only a literal `false` opts out (a typo fails open, visibly, rather
    than silently hiding a command). Built-in `:release` used to carry it
    because its last step pushes a tag — **reversed 2026-08-05 by the owner**:
    every built-in, `:release` included, is now model-invocable, and the model
    is expected to follow the command's own preflight (clean tree, right branch,
    ask before deciding) rather than being barred from loading it.
  - _The prompt section is gated on the tool._ A profile whose `tools:`
    allow-list drops `command` gets no listing: naming a tool an agent lacks is
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

Promoted here when the effort that taught them was deleted:

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
- **Confinement that a mode's tool set makes unreachable is not confinement.**
  `web_fetch`/`web_search`/MCP run in the hrdr parent, outside the sandbox, so a
  "confined" agent holding them had a working network egress. Jail's boundary is
  its tool set as much as its roots.
- **Removing a tool means auditing what it was the only home for.** `grep`
  filtered credential files out of its own output; deleting it from every
  non-jail mode would have left `shell` — the actual search path — with no
  secret handling at all.
- **"Available and ignored" is the only usage figure worth acting on.** That a
  tool the model was handed gets called measures availability, not need.
- **`AgentEvent::Notice` never reaches the model.** The channel that reaches the
  model is a note appended to the round's last tool result — the shape the
  round-budget warning, the repeat nudge and the truncation warning all share.
- **The transcript belongs to the conversation; frontend chrome is toast or
  popup, never an entry.** A slash command's status line toasts and its data
  output opens a dismissible popup, so nothing a command prints can split a
  streaming thinking block. (`/diff` keeps its colored transcript block as a
  deliberate exception.)
- **`read_only()` means "does not mutate the working tree", not "touches no
  state".** Anything holding state only in the agent's own `ToolContext` belongs
  in the read-only set — and if its calls are order-sensitive, it opts out of
  `concurrent()` separately, which defaults to `read_only()`.
- **Gate the prompt on what the tool set IS, not on what built it.**
  `ToolRegistry::with_defaults` registers `grep`/`find`/`ls`/`tree` and
  `Agent::new` strips them for every non-jail mode, so `has("grep")` alone
  marked a full write agent as jailed. The jail is the whole shape: those tools,
  no write tool, and no shell.
- **Duplication drifts; the reachable copy is the one that must be complete.**
  `write.md`'s Releasing section and the `:release` command were the same
  procedure twice, and only the command said to watch the tag's CI run — while
  `model_invocable: false` kept it out of the listing entirely, so plain-English
  "cut a release" only ever reached the copy missing that step.
- **Guidance with no trigger phrase has to stay resident.** Deleting and
  Dependencies did not move to main-only with Git and Releasing: a sub-agent
  deletes files and reads dependency APIs like anyone else, and nothing is said
  before `rm -rf` that a gate could match on.
- **Structure beats wording when the wording is already frozen.** Hundreds of
  pinned literal spans across the prompt corpus cap what rewording can save;
  moving git/release guidance behind `!delegated` saved an order of magnitude
  more than carefully rewording four always-on files did.
- **The prompt's four Anthropic cache breakpoints are all spent** — tools,
  stable prefix, system tail, rolling last message. A fifth means giving one up.
  `SystemPrompt::prefix_len_before(SECTION_ENVIRONMENT)` (a fold over section
  lengths, not a substring search) is what the client carries as
  `system_cache_split`; a resumed session rebuilds the prompt so the split
  matches the installed text.
- **The trust gate's three decisions, not to be re-litigated:** **exact paths,
  never ancestors** (trusting `~/Projects` must not trust a repo just cloned
  into it); **only the yes is stored**, so declining is asked again rather than
  sticking; and the **default selection is cancel**, so a reflex Enter opens
  nothing. A caution-envelope around `AGENTS.md` was built first and reverted in
  favour of this — do not rebuild it.
- **Threading: the TUI stays the root task on the main thread**, blocking tool
  work belongs on `spawn_blocking`, and the whole-turn agent mutex stays (the UI
  works around it with `try_lock`).
- **`watch` is a general check-command watcher, not a CI-specific one** — the
  check runs under the same guardrails and sandbox as `shell`, and its result
  wakes the agent's turn like a finished background sub-agent. It reports "the
  run finished", never "the release is published": the confirmation (enumerate
  the jobs, check the artifact landed) stays a model step on wake.
- **A red tag run SKIPS its publish jobs rather than failing them.** The push
  succeeds, the tag exists, nothing is published — how two tags were cut with
  nothing behind them. Enumerate the run's jobs and confirm the artifact landed;
  "tagged and pushed" is not "released".
- **`cargo` here runs through a wrapper that indents `error:` lines**, so
  `grep -E "^error"` reports a false pass. Read its summary line.
- **An item's proposed fix is a hypothesis.** Backlog entries have described
  their own fix wrongly more than once. Verify the claim before implementing the
  remedy.
- **A test that models an impossible agent proves nothing about the real one.**
  The read-only prompt test built a shell-less registry while `config.read_only`
  deliberately keeps a shell, so it had never covered the agent it was named
  for. Prefer building a live `Agent` over hand-assembling a registry.
- **A skip that cannot fail is not a skip.** The Seatbelt end-to-end test opened
  with two silent `return`s, so a run that exercised nothing was
  indistinguishable from one that passed. Any test gated on a prerequisite must
  assert where that prerequisite is guaranteed.
- **`current_exe()` is the test binary inside a unit test.** A backend that
  re-execs hrdr therefore re-executes libtest and wedges the run; anything
  exercising the real wrapper belongs in `apps/hrdr/tests/`, where
  `CARGO_BIN_EXE_hrdr` names it.
- **Blind FFI fails on names, not logic.** Every Windows CI round trip cost was
  a constant or trait import that had moved between `windows-sys` releases,
  never the token or SID logic. Spell a fixed ABI value out locally instead of
  importing it and the class disappears.
