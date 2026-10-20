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

## Questions for the owner — index, 2026-10-15

Everything below that waits on an owner decision rather than on work. Each line
names the entry holding the options; the entry is the source of truth, this is
only the list. Delete a line when its question is answered.

- **A manual CI run publishes from any branch.** `publish-github-release` in
  `.github/workflows/ci.yml` runs on every `workflow_dispatch`, whatever the
  ref, and `publish-crates`, `aur-bin`, `brew-tap`, `scoop-bucket` and `alpine`
  all follow it — so dispatching on a branch creates a GitHub release named
  after the branch, publishes the workspace's `Cargo.toml` version to crates.io
  if it is not there yet, and pushes packaging with the branch name as the
  version. Options: (a) gate publishing on
  `startsWith(github.ref, 'refs/tags/v')` for both events, which keeps
  re-running a release by dispatching on its tag, and then also spell `/` as `-`
  in the archive names (`TAG=${GITHUB_REF_NAME//\//-}` in the bash steps,
  `-replace '/', '-'` in the pwsh one — both checked locally 2026-10-16) so a
  branch dispatch builds cleanly; (b) leave it, if dispatching on a branch to
  publish is intended. Found 2026-10-16 while fixing the naming; the naming fix
  was held back, because the failing `Package` step is today the only thing that
  stops a `wip/…` dispatch from publishing. _Session coordination, queue
  item 9._
- **A failed OAuth token save goes unnoticed**, and the uncalled public
  `valid_access_token`. Pick how a refresh whose rotated token could not be
  saved is surfaced (a status line through the frontend, a returned warning, or
  failing the request), and whether to drop the unused re-export — an API break
  for an outside user of `hrdr-agent`, like `apply_cache_breakpoints`. _Test
  coverage → "Config-mutation path tests" paragraph, Session coordination
  section._
- **CRLF through `replace` and `write`** — preserve a file's dominant line
  ending or write what was given. _Windows support review 2026-09-15._
- **`cwd_slug` hash and migration.** _Windows support review 2026-09-15._
- **Owner-only DACL on Windows credential/session files.** _Consistency /
  robustness → "hrdr sets no Windows ACL"._
- **`/trust revoke`** and whether it downgrades the running session. _Tooling /
  agent capability._
- **Windows `write`-mode OS confinement** (Low-integrity relabelling of writable
  roots). _Sandbox follow-ups._
- **`termwiz` for the pty test harness.** _Queue item 1 → "The pty tests read
  the screen by stripping escapes"._
- **Per-pane compaction cancel; main-scoped system lines.** _Left open by the
  compaction fix._
- **Queue items 4, 5, 7, 10** — LLM-managed session names, Windows terminal
  titles, TODO-to-TODO dependencies, OpenAI usage-dashboard parity. _Session
  coordination and queued work._
- **Replacing `serde_yaml_ng`.** _Needs a decision 2026-10-14._
- **Which built-ins become skills.** _Needs a decision 2026-08-09._
- **Project-writable instruction surfaces** — a write sub-agent can edit
  `AGENTS.md` mid-session, and project commands/skills shadow built-ins by name;
  both left open on purpose, the second the stronger candidate to close.
  _Peer-comparison findings → Permissions, isolation, and state._
- **Which peer-comparison features to build** — none is a fix; each is a product
  choice: session search, prompt-size introspection, shell parsing instead of
  regex guardrails, runtime tool descriptions, per-model behaviour as data,
  deferred tool loading, an ask-the-user tool, one permission evaluator,
  per-call escalation, a "something changed" channel, session env vars for shell
  commands (default off), and a truncation-cap experiment. _Peer-comparison
  findings still open._
- **The guardrail-to-model escalations** — `AGENTS.md` content scanning (wants
  evidence of a real attempt first), teeth for the test nudge, and an evidence
  gate that judges relevance. _Top of the list._
- **Provider divergences** — reasoning replay keyed on the field rather than the
  model, which statuses retry, more `retry_after` phrasings (wants real provider
  messages), and unknown Anthropic stop reasons. Each is pinned by a test.
  _Provider divergences left open by decision._
- **Drop the `apply_cache_breakpoints` re-export from `hrdr-llm`** — a
  public-API break for a published crate. _Tidy review 2026-08-04 #9._
- **Manual smokes that need the owner's environment** — `watch` against a real
  GitHub run, and the DeepSeek provider with a real `DEEPSEEK_API_KEY`. _Test
  coverage gaps._
- **`CommandHost` text-fallback defaults; `gate_rank`/`kind_rank` merge;
  verbose-gating the session banner; `jail` write floor and read-only git.**
  Smaller calls recorded where each lives.
- **Vim mode behaviour calls** — Ctrl+C in Insert, the start mode and the mode
  after a submit, which clipboard a yank or delete reaches, arrows in a
  multiline draft, and where the mode indicator goes. _Vim mode — proper
  support._
- **Added 2026-10-20 — open items that need a call, each tagged where it
  lives:** MCP tool-description wrapping (_Hardening notes, 2026-08-05 security
  audit_); `open_system_handler` `raw_arg` rewrite (_Security audit
  2026-08-28_); write mode's whole temp dir (_Peer-comparison findings →
  Permissions, isolation, and state_); a shell pre-flight (_Sandbox
  follow-ups_); handle-relative no-follow opens (_Consistency / robustness_); a
  profile with no search tool (_Known behaviour to revisit_); an image dimension
  check, and a real-time lock-staleness test (_Deferred 2026-08-11 — media_);
  the hrdr clipboard worker, text-over-PNG, and an hjkl release carrying the
  clipboard fixes (_Windows support review_); `compaction_tail_start` and the
  trailing backslash (_Correctness review 2026-08-14_); `verify` dry run and
  resumed `read_only` (_Security audit 2026-08-30_); the cron late fire
  (_Correctness review 2026-09-04_); the unix completion-identity red/green
  (_Windows support review_); per-surface shell choice (queue item 6) and
  broadening the self-update handoff (queue item 8).
- **Added 2026-10-20, second pass — each tagged where it lives:** an endpoint
  override for testing the OAuth exchanges (_Session coordination →
  "Config-mutation path tests"_); measuring a head-only summarization request,
  and a nightly live-cache CI job (_Compaction rewrite_); skill metadata's
  surface, `skill` in the jail set, and cached sets on the `:` send path
  (_Deferred 2026-08-09 — Agent Skills_); a `write`-mode read allow-list
  (_Sandbox follow-ups_); what memory drift detection should flag (_Tooling /
  agent capability_); the `enqueue_save_as` snapshot (_Performance review
  2026-08-14_); the per-round base64 re-encode (_Performance review
  2026-08-28_); deleting the two unmerged `wip/windows-paste-transport*`
  branches (_queue item 9_); a second OS account and a second machine for the
  foreign-user and remote-client IPC rejection tests (_Session coordination →
  "Resume/verification gaps"_); running the `xvfb-run`/headless-Sway guidance in
  CI (_Session coordination → "GUI-testing prompt guidance"_); a live image
  smoke per dialect (_Deferred 2026-08-11 — media attachments_).
- **Declined by judgment 2026-10-20, yours to overrule — each tagged where it
  lives:** a real `fork` test and unlock-failure injection for the IPC lock
  (_Session coordination → "CI run `37922581404`" paragraph_); a second wrap-up
  compaction (_Hardening notes, 2026-08-04 correctness pass_); a clippy ban on
  `dirs::home_dir` (_Windows support review → Coverage gaps_).
- **Added 2026-10-20, third pass:** declined 08-30 perf #16, the running token
  counter (_Performance review 2026-09-04_); macOS end-to-end tests for the
  strict Seatbelt profile and network (_Hardening notes_); CI as the only runner
  of the unix-only guard tests (_Windows support review → Coverage gaps_).
- **Should a blocking hook that cannot run fail closed?** A `pre_tool` or
  `user_prompt` hook that times out, cannot spawn or crashes only adds a note,
  so the call or message it guards goes ahead. _Known behaviour to revisit._

---

## Vim mode — proper support, requested 2026-10-18

The owner asked for vim mode to work "properly/nicely". A read-only
investigation (2026-10-18) drove `hrdr_editor::VimEngine` and a scratch
`hjkl_vim::vim_editor` probe against the resolved hjkl 0.42.2, read the TUI's
key routing, and found vim mode **crashing on the first key**: `VimEngine::new`
built through `Editor::new`, which since hjkl 0.33.6 installs no discipline.
Fixed 2026-10-18 (`vim_editor`, pinned by `vim_tests::keys_drive_the_vim_fsm`).
The hjkl engine itself is capable — `dw`, counts, `ciw`/`ci"`, `.`, undo/redo,
visual delete, named registers, `/` search, macros, marks and `R` all worked in
the probe; `:` is a no-op by design (ex commands are the host's). What remains
is how hrdr hosts it. Every `hjkl-*` crate resolves to 0.43.0, the newest on
crates.io (adopted 2026-10-20; its `hjkl-vim` API breaks touched nothing hrdr
uses — the workspace built and passed unchanged). Evidence is marked: **[ran]**
observed, **[read]** from the code, **[inferred]** not confirmed.

1. **Vim e2e coverage.** [ran] `app::e2e::ctrl_d_scrolls_and_never_quits` never
   feeds a key to the engine — which is how the crash went unnoticed. The
   `app::e2e::vim_*` tests now type through `App::on_key`; each item above lands
   with one. S per item.

**[Deferred — questions for the owner; see the index at the top]**

README's vim section (under Keybindings) describes the behaviour as it is,
2026-10-18; each answer below changes it, so update it in the same change.

2. **Mode indicator and showcmd.** [read] `mode_label()` is never drawn, the
   labels say SELECT where vim says VISUAL, and nothing shows pending keys,
   "recording @a", or Replace mode (it reports as Insert). **Question:** draw it
   in the input pane's top row, or in the status bar? Verify: a render shows
   `-- INSERT --` after `i`. S–M.
3. **Ctrl+C in Insert.** [read] Ctrl+C clears the whole input box in every mode;
   vim users press it to leave Insert, and lose the draft. **Question:** keep
   "clear box" everywhere, or make Ctrl+C act as Esc in vim Insert? S.
4. **Start mode, and the mode after a submit.** [read + ran] The engine starts
   in Normal and `set_content("")` after a submit leaves it there, so every
   message starts with `i`. **Question:** start (and return after submit) in
   Insert or Normal; and should Ctrl+Enter or Alt+Enter submit from Insert? S.
5. **Which clipboard a yank or delete reaches.** [read] Every `x`/`d`/`c`/`y`
   goes to the OS clipboard (`record_yank_to_host` in hjkl-engine's
   `editor.rs`), as if `clipboard=unnamedplus` were always set, so deleting a
   word overwrites what the user copied elsewhere. **Question:** keep that, or
   vim's default (only `"+`/`"*` touch the OS clipboard — needs a host flag or
   an hjkl option)? S–M.
6. **Arrows in a multiline draft.** [read] Up/Down always walk history, in every
   mode — deliberate (cleared under Correctness 2026-08-05 half B).
   **Question:** keep that, or let the arrows move within a multiline draft in
   Insert and walk history only at its first/last line? S.

---

## Session coordination and queued work — 2026-10-08

Work in this order; these entries preserve the queue across sessions. Commit and
push each verified slice to `main` before proceeding to the next slice.

1. **Cross-client agent messaging.** Implement, fully test, review, and commit
   before starting the remaining backlog work. All running hrdr sessions under
   the same OS user can discover and message each other without consent or
   opt-in groups. Enable by default, with a configuration flag to disable it.
   Discovery exposes session ID, session name, and session working directory.
   Incoming messages clearly identify the sending agent, wake an idle agent, and
   queue for a busy agent like user messages. Preserve sender provenance rather
   than presenting peer text as a human instruction. Resume from
   `hrdr-agent::messaging::Messaging::{start, descriptor, refresh, discover, send, dequeue, wait, close}`
   and `hrdr-tools::local_ipc`; do not rebuild the existing transport,
   registration, or peer-provenance layers. Done: the default-enabled
   `session_messaging` flag (`$HRDR_SESSION_MESSAGING`), the `$HRDR_IPC_DIR`
   discovery-parent override (the test sandbox sets it), and the TUI lifecycle
   in `hrdr-tui`'s `app::messaging` (start once the session has an id, refresh
   name/cwd, rotate on session replacement, wake idle agents, one peer message
   at a time on a busy agent's steering queue, report undelivered messages on
   rotation and exit, peer inbox blocks update handoff), and the `sessions` /
   `message_session` tools (`hrdr-tui`'s `app::messaging_tools`; targets are
   resolved by session id plus cwd against a fresh discovery, so no endpoint
   reaches the model, and the sender identity is this session's registration).
   Discovery now probes at most `PROBE_CONCURRENCY` candidates at once and skips
   a candidate whose probe fails instead of failing the whole scan. A brand-new
   session is not discoverable until its first message mints an id; only the
   main agent gets the tools — sub-agents cannot message peers, so no child
   identity is issued yet. Headless `hrdr run` (`apps/hrdr`'s
   `headless_messaging`) registers as `run-<pid>` during its prompt's turn,
   closes admission when that turn ends, and runs the accepted messages as
   further turns; `tests/headless_messaging.rs` covers it across real processes.
   One full-suite run saw the peer's `Messaging::start` fail with `WouldBlock`:
   the directory mutation lock is held across an fsync in `MutationGuard`, and
   the retry budget was the 2s wire deadline. Directory operations now retry for
   `DIRECTORY_DEADLINE`; one later full run passed, which does not prove the
   flake is gone. That fix left both ends of a message exchange on the 2s wire
   deadline while the receiver's sender check could wait out the full directory
   budget, so on Windows CI a send failed twice with `BrokenPipe`
   (`headless_messaging`, then the pty session-switch test, never locally): the
   receiver's worker timed out and dropped the pipe. Message exchanges now run
   to `MESSAGE_DEADLINE` on both ends, and a test holds the directory lock past
   the wire deadline. **Not the whole story:** the pty send failed with
   `BrokenPipe` once more on Windows CI afterwards (leak-guard job, run for
   `dc022eca`). The next suspect, hardened but unproven: `serve` ended the whole
   server on any accept error outside `unavailable`, and a Windows accept
   authenticates each client (`OpenProcess` on a client already exiting can fail
   access-denied), so one vanishing discovery probe could stop a session
   serving. Per-client authentication failures now surface as
   `ConnectionAborted` (`local_ipc::transport::rejected_peer`), which `serve`
   skips. No test can provoke that failure deterministically; the pty test now
   reports, on a failed send, whether the target was still published, so a
   recurrence names the mechanism. A new symptom on 2026-10-15 (Windows
   leak-guard job, run 38643593063 for `fc1acfca`, the first runs with CI's
   pinned ConPTY host): the send reported `SENT_TO_PEER`, but the peer's
   `dequeue` saw nothing within `EXIT` — "the peer never received the message".
   The same suite passed in that run's `test windows-latest` job; not reproduced
   locally. The next run (38648655413, `665ea349`, same job) failed the same
   test one step earlier: the `sessions` tool result did not list the peer at
   all — discovery, not delivery. Both recurrences are the leak-guard job under
   the pinned host, where the whole workspace suite runs in one process tree.
   The listing took 214 ms, about `PROBE_RETRY_DELAY` plus nothing, so the
   peer's probes failed fast or it was no longer a candidate — consistent with
   its serve loop having stopped: `serve` breaks on any accept error outside
   `unavailable`, its listener then releases the lease, and the next listing
   reaps the record. The error used to be discarded; it is now kept
   (`Messaging::stopped_by`) and the pty test's failure messages print it.
   **That hypothesis is falsified:** the next recurrence (`dad2f48f`, Windows
   leak-guard job, 2026-10-15) printed `peer open: true, stopped by: None` — the
   peer was serving throughout. The listing again took 215 ms, one
   `PROBE_RETRY_DELAY`, and held the two other sessions, so this peer's probe
   failed fast twice in a row while its server ran. The cause is in the probe
   exchange, and `Messaging::discover` drops every probe error
   (`Err(_) => None`), so nothing said which. Now `Messaging::discover_detailed`
   keeps each skipped candidate with the status it answered or the error it
   failed with, and the `sessions` tool lists them under `unreachable`, so the
   next recurrence's request body names the reason. An unavailable-classed
   connect error came back as `answered Unavailable` and was not retried (only
   `Err` was), dropping a peer on one transient connect failure; since
   2026-10-15 both get the single retry
   (`discovery_retries_a_session_that_was_unavailable_once`). Whether that was
   the flake's cause is not yet shown. Next recurrence (`a78abc90`, 2026-10-15)
   moved to the release smoke job and past discovery: the peer was listed, then
   never received the message (`peer open: true, stopped by: None`). The mock
   model says `SENT_TO_PEER` whatever `message_session` returned, so the test
   now prints that tool's answer on this failure too. That answer arrived on the
   next recurrence (`bbf1ddd4`, run 38952539010, Windows leak-guard job,
   2026-10-16, `peer open: true, stopped by: None`): `message_session` said "no
   running session has session_id … in "/pty-peer"" after 428 ms. The tool
   re-runs discovery (`peers` in `messaging_tools.rs`) rather than reusing the
   `sessions` listing, so this is the discovery flake again, not delivery: the
   live peer dropped out of a second listing moments after the first one found
   it. The same commit's tag run passed every job, this one included. Again on
   `ec0bef24` (run 39225324122, Windows release smoke, 2026-10-19, 237 ms,
   `peer open: true, stopped by: None`), while its tag run passed. Since then
   `message_session` resolves through `discover_detailed`
   (`messaging_tools::resolve_target`): a peer the second listing found
   registered but not answering is refused with its probe's answer ("registered
   but did not answer (…)"), and only a peer missing from the registry
   altogether still reads "no running session" — so the next recurrence says
   which of the two dropped it. It did, on the next recurrence (`35094198`, run
   39341063820, Windows build + smoke, 2026-10-20, 212 ms,
   `peer open: true, stopped by: None`): "registered but did not answer (The
   pipe is being closed. (os error 232))" — `ERROR_NO_DATA`, which a pipe write
   gets once the other end has closed, so the peer's server dropped the probe's
   connection early, twice running (the retry included). One mechanism was tried
   and falsified the same day: a client that opens the pipe and closes it before
   the server's `ConnectNamedPipe` does not deafen the listener (the accept
   succeeds and the next client is served). The serve loop discarded both the
   accept errors it treats as transient and every worker's error, so the peer
   could not say why; it now keeps the latest (`Messaging::last_dropped`,
   `a_dropped_connection_leaves_its_reason`), and the pty test prints it beside
   `stopped by`. It recurred with that in place (`c13626e3`, run 39416820368,
   Windows build + smoke, 2026-10-20, 324 ms): the same 232, and
   `last dropped: None` — the peer's server recorded no failed accept, no worker
   error and no worker timeout, so whatever closed the pipe did not go through a
   server path that errors. Reading the server rules out a cancelled accept: the
   pending instance lives on the `Listener` across cancellation, and nothing
   after `connect` completes awaits. Since then each step of `exchange` labels
   its error ("connecting", "reading the greeting", "sending the request",
   "reading the reply"; `a_failed_probe_names_the_step_it_failed_at`), so the
   next recurrence says on which side and when the pipe closed. Not reproduced
   locally (12 runs of the two messaging tests together, 6 of the full `tui_pty`
   suite, Windows, pinned host). No test covers `stopped_by` being set: nothing
   can make a pipe accept fail on demand. Same day, one parallel `local_ipc` run
   failed `native_cross_process_rejects_malformed_truncated_and_stalled_frames`
   after stalling for 43s; four reruns passed in ~5s. Again 2026-10-18, in a
   full Windows workspace run beside other sessions' builds: that test and
   `native_cross_process_sequential_clients` failed on a `TimedOut` connect, and
   in the same binary `lsp::tests::pre_warm_spawns_the_server_before_any_edit`
   and `json_rpc_error_responses_are_forwarded_not_discarded` found "the python3
   language server is not available"; all four passed alone and in the next full
   run. Real-process coverage: `tests/tui_pty.rs`
   (`a_live_tui_messages_a_peer_and_is_woken_by_its_reply`: tools, peer wake,
   shutdown leaves discovery) and `tests/headless_messaging.rs` (busy delivery,
   disabling), on the CI matrix; verified locally on Windows only so far.
   `a_switched_tui_session_republishes_and_its_exit_reports_undelivered_messages`
   covers `/new` rotation and the exit report across processes. Still
   unexercised across processes: access-control denial for a foreign OS user.
   Mutation check: removing the TUI's arrival wake did not fail the pty test —
   something else woke the loop there — so the wake is guarded only by the
   in-process test that waits for `PeerReady`.

   Verify delivery, discovery, disabling, busy/idle behavior, and sender
   attribution before declaring this complete. Support Windows, macOS, and Linux
   with equivalent discovery and delivery behavior. Run native cross-process
   messaging and access-control tests on each supported platform through the CI
   matrix. Local verification is native Windows only (no WSL, by the owner's
   call 2026-10-14); Linux and macOS runtime verification comes from the CI
   matrix every push to `main` runs. A Windows-only pass or cross-compilation
   alone does not establish macOS/Linux runtime correctness. Record any missing
   platform verification explicitly rather than treating it as passed.

2. **Remaining actionable backlog.** After messaging is finished and fully
   tested, reassess the entries against current code and implement actionable
   slices, with tests, review, verification, and separate coherent commits. Keep
   items requiring user decisions explicit rather than guessing answers.
3. **Backlog pruning — first pass done 2026-10-14.** Two read-only audits
   checked the entries after this queue against the code; only entries confirmed
   resolved (re-read at the cited symbol) were deleted, and shipped review
   findings were removed from their sections. Gaps: an audit judged by reading,
   so an entry it could not decide stayed; the queue above was not audited this
   way.
4. **LLM-managed session names.** **[Deferred — question for the owner; see the
   index at the top]** Generate the default session name through the LLM on its
   first turn. Let the agent rename the session when its direction, goal, or
   progress changes enough that the existing name no longer fits. Implement and
   test initial naming and subsequent meaningful renaming after the preceding
   queued work. **Deferred 2026-10-14 — questions for the owner before
   implementation:** (a) Should the agent ever rename a session the user named
   explicitly (`/new <name>`, `/rename`)? Recommended: never — a user name is
   pinned until the user clears or changes it — with the alternative being that
   the agent may propose a rename the user confirms. (b) How is the name
   produced? Options: a separate small model request after the first turn (one
   extra request per session, works with any model), or a `rename_session` tool
   the model calls itself (no extra request, but depends on the model choosing
   to call it). (c) Which model pays for (b)'s first option — the session's
   model or the configured `subagent_model`? Today a session is named from the
   first line of its first message (`session_name_from`).
5. **Terminal titles — Windows report unresolved.** **[Deferred — question for
   the owner; see the index at the top]** The status/cwd/name title shipped
   (`hrdr-tui`'s `app::title`), and `tests/tui_pty.rs` shows on Windows that the
   OSC title reaches a ConPTY client. The reported Windows symptom (the tab
   staying `PowerShell`) was not reproduced, so its cause is unknown: a terminal
   setting such as Windows Terminal's `suppressApplicationTitle`, or a host that
   ignores OSC titles, would produce it with hrdr working. **Question for the
   owner:** which terminal and profile showed `PowerShell`? The macOS and Linux
   pty runs are on CI only.
6. **Selectable shells — macOS/Linux and hook/verify coverage.** Shipped: Git
   Bash first on Windows (`Shell::resolved`, `git_bash`), every installed shell
   offered through the `shell` tool's `shell` argument, and dialect sections for
   each offered shell. Verified locally on Windows only; macOS/Linux rely on CI.
   Hooks, `verify`, `watch` and the TUI's `!command` still run the default shell
   only — no per-call choice there. **[Deferred — question for the owner; see
   the index at the top]** **Question:** give those a shell choice too (a config
   key per surface, or one shared default), or keep them on the default shell?
7. **TODO-to-TODO dependencies — question for the owner.** **[Deferred —
   question for the owner; see the index at the top]** Watch-aware TODO nudges
   shipped: an item's `waits_on` names a running watch (see the changelog). The
   request's repeated "TODO item" wording could also mean one TODO item waiting
   on another; that was not built. Should it be, and should a blocked item then
   be exempt from the nudge the same way?
8. **Self-update, restart, and turn continuation — requested 2026-10-09.**
   Highest priority after finishing the current path-rendering slice, per the
   user's clarification; take this before the other Windows backlog items. Add a
   model tool for working on the hrdr codebase that installs hrdr through Cargo,
   restarts the running application into the newly installed version, and
   automatically resumes the same session and continues the interrupted turn.
   Support the agent working on hrdr itself: install the exact version/source
   revision it has tested, rather than silently fetching an unrelated newer
   revision. Preserve session history, goals, TODOs, working directory, and the
   continuation point durably before handing control to the replacement process.
   Avoid replaying completed side-effecting tool calls or recursively repeating
   the update on resume. Define handling for active child tasks, watches, queued
   messages, and session locks; do not claim they survive without a tested
   handoff/recovery mechanism. Handle Windows executable-in-use replacement and
   terminal handoff as well as Unix restart behavior. A failed build/install
   must leave the current instance usable; a failed restart must leave a
   recoverable session and actionable diagnostics. Approved on 2026-10-09: build
   the current clean working copy on `main`, requiring its commit to match live
   `origin/main` and successful GitHub CI for that exact hash in the origin
   repository (forks supported). Install into the default user Cargo location.
   No permanent launcher: if Windows refuses safe executable promotion, abort
   and keep the current instance usable. After committed handoff, recover from
   durable state rather than letting both processes continue. Resolve the
   detailed recovery protocol before implementation. Test actual process
   replacement, version identity, automatic turn continuation, exact goal/TODO
   preservation, and failure recovery on the supported platform matrix.
   Completed first slice: `hrdr --build-info` exposes compile-time version, full
   commit (or `unknown`), target, and clean/dirty/unknown source state before
   startup config, trust, model, or session initialization. It is diagnostic
   only, not installation authorization. `hrdr update-check --json` now
   validates local cleanliness, `main`, live origin-main equality, direct-fork
   lineage, and the exact origin-repository CI run/job matrix before
   configuration/session startup. It emits only sanitized fixed-status
   diagnostics and still does not install or restart. Durable continuation is
   blocked on new, reviewed primitives: session checkpoint validation while
   holding the ordinary session lock; process-instance (not PID-only) ownership;
   anti-symlink journal storage; and a cross-platform crash-durable record
   replacement API whose parent-directory durability failures are surfaced. Do
   not add an update/restart surface until those invariants have focused
   recovery tests. A best-effort foundation now exists: an append-only
   checksummed, flushed journal binds a backed session-lock holder to a
   parseable hashed saved checkpoint and recovers only the newest validated
   frame. Sudden Windows power loss may lose or ambiguously preserve the newest
   handoff; no update/restart surface may claim otherwise. An internal
   coordinator now commits only an idle saved session and rejects runtime
   queues, child tasks, watches, drafts, active turns, and save errors. Hidden
   passive recovery now claims only dead-owner committed checkpoints, restoring
   no queues and starting no runtime work. Interactive Windows `update` now
   stages only after its tool turn ends, then uses one attested recovery
   continuation; it still refuses queues, tasks, watches, drafts, and active
   work. Windows CI now exercises isolated default-Cargo installation,
   retained-old fallback recovery, and request reuse. Next persist queues before
   broadening the handoff contract. CI eligibility is approved: require the
   latest `push` CI run and latest attempt on `main` for the exact
   origin-repository hash, with verification and packaging jobs successful; only
   tag-only publishing jobs may be skipped. Manual and older runs do not
   qualify. **Windows recovery cwd, fixed 2026-10-14:** CI runs `38207254188`
   and `38211455090` failed every self-update E2E recovery because the
   coordinator canonicalized the recovery cwd, so a session saved under an 8.3
   short name (`RUNNER~1`) was looked up under a different session directory.
   The E2E fixture now launches through a junction so the alias case runs on
   every machine. Without the fix it fails locally with the CI error; with it,
   all five E2E tests pass locally. CI run `38478769569` (`b319222d`) passed the
   `Windows Cargo self-update E2E` step after the install waits got their own
   budget. The 2026-10-12 `auth.json.lock` timeout in
   `hrdr-agent::auth_store::tests::concurrent_writers_different_providers_all_preserved`
   was the lock's whole-wait budget running out behind fifteen progressing
   writers; fixed in `StoreLock::acquire`. **Unix self-update, 2026-10-14:**
   `apps/hrdr`'s `update_exec` installs in place and `exec`s the installed
   binary, which claims the checkpoint as its own successor
   (`Session::recover_handoff_as_successor`); process leases exist on Linux and
   macOS. `tests/unix_update_e2e.rs` passed in WSL (Linux) and fails without
   `--exec-successor`; macOS relies on CI's `Unix Cargo self-update E2E` step.
   Until then the Windows E2E's recovery-cwd check passed vacuously:
   `/cwd apps/hrdr` was refused as untrusted, and PowerShell echoed the marker
   in its own error. With both fixed it caught `/cwd` storing the verbatim
   `\\?\` spelling, under which recovery could not find the session.
   **Remaining:** Queue persistence is not needed for today's contract: the
   handoff waits for quiescence, and a queued steer relaunches a turn at `Done`,
   so queued messages are delivered before the handoff rather than lost. It
   becomes necessary once the contract is broadened to hand off with work
   outstanding. **[Deferred — question for the owner; see the index at the
   top]** **Question:** broaden the handoff to carry queued messages, child
   tasks and watches (needs queue persistence and a recovery test per kind), or
   keep the quiescent-only contract?
9. **Windows multiline paste — shipped 2026-10-15 (`58b2c631`); gaps left.** Not
   verified by hand: pasting and typing in a real Windows Terminal and in a
   classic console window (the pty tests run under ConPTY only). CI's Windows
   jobs pin Microsoft's ConPTY host (`scripts/pin-conpty.ps1`), so crossterm's
   Windows reader — still used under the inbox `conhost.exe` — no longer has pty
   coverage there. The release `Package` steps name archives after the ref, so a
   `workflow_dispatch` run on a branch whose name holds `/` fails them
   (`hrdr-wip/…tar.gz`: no such directory); main and tags are unaffected. That
   failure is currently what stops such a run from publishing — see "A manual CI
   run publishes from any branch" under the owner questions; do not fix the
   naming alone. `wip/windows-paste-vt-input`, the merged work, was deleted
   2026-10-16. Two branches remain on origin, neither merged:
   `wip/windows-paste-transport` (the original probes, also local) and
   `wip/windows-paste-transport-rebased` (the same probe commit rebased — its
   tip is not in `main`, despite having been called merged here). Both only on
   the owner's word. **[Deferred — question for the owner; see the index at the
   top]** **Questions:** the publishing gate (see the index); may the two
   `wip/windows-paste-transport*` branches be deleted; and the hand check in
   Windows Terminal and a classic console needs your machine's terminals.
10. **OpenAI OAuth usage-dashboard parity with Codex — requested 2026-10-09.**
    **[Deferred — question for the owner; see the index at the top]** Deferred
    until the earlier queued work is finished. Compare hrdr's
    OAuth-authenticated OpenAI requests with the official `openai/codex`
    implementation: usage accounting, request categorization, and credit usage
    as reflected in the GPT usage dashboard. Identify which request metadata,
    endpoint behavior, and response usage/rate-limit fields Codex actually uses;
    do not assume dashboard attribution is entirely client-controlled or invent
    unsupported accounting fields. Trace hrdr's corresponding paths, implement
    evidenced parity, and test request construction and usage interpretation.
    Verify dashboard attribution with an authorized real account separately;
    record any server-side visibility limits. Never log tokens or send private
    request contents as diagnostics. Reference checkout:
    `~/Projects/mxaddict/codex` from `https://github.com/openai/codex.git`,
    cloned at `a06545b311fe01e51ce855c7aa5d8da21e9e7aaf`; pin the reference
    revision used for the comparison. **Comparison done 2026-10-14, against
    `openai/codex` at `a06545b311fe01e51ce855c7aa5d8da21e9e7aaf`
    (`~/Projects/mxaddict/codex`); nothing implemented — it needs the owner's
    answers below.** hrdr's `hrdr-llm::codex` request sends `originator: hrdr`,
    the bearer token, the provider's `ChatGPT-Account-Id`, an optional
    `x-openai-internal-codex-responses-lite`, and `prompt_cache_key` in the
    body. Codex additionally sends: `originator: codex_cli_rs` (its
    `DEFAULT_ORIGINATOR`, `login/src/auth/default_client.rs`) and a matching
    `User-Agent`; `session-id` / `thread-id` and `x-client-request-id`
    (`codex-api/src/requests/headers.rs`, `endpoint/responses.rs`);
    `x-openai-subagent` naming the request kind for review, compaction and
    memory consolidation requests; the sticky-routing `x-codex-turn-state`
    token, read from a response and replayed for the rest of the turn;
    `x-codex-installation-id`, `x-codex-window-id`, `x-codex-turn-metadata`,
    `x-codex-beta-features`; and a `client_metadata` body object (installation,
    session, thread and turn ids, `request_kind`, …;
    `core/src/responses_metadata.rs`). **Questions for the owner:** (a)
    Dashboard attribution most plausibly keys on `originator`, but sending
    `codex_cli_rs` from hrdr would present hrdr to OpenAI as Codex — a client
    impersonation. Recommended: keep `hrdr`. Is that acceptable even if hrdr
    usage then shows under a different category, or not at all? (b) Which
    discrepancy prompted this — usage missing, mis-categorized, or counted
    differently? Answering needs an authorized account and a before/after look
    at the dashboard, which this session could not do. (c) Independently of
    attribution, `x-codex-turn-state` sticky routing and `x-openai-subagent` on
    compaction requests may have functional effects (cache locality, request
    classification). Adopt them as Codex-private headers, or leave them?

**GUI-testing prompt guidance — not executed here (2026-10-14):** the `Tests`
rule in `templates/write.md` names `xvfb-run -a` and a Sway compositor started
with `WLR_BACKENDS=headless`. Neither was run: this machine's WSL has neither
installed, and installing them needs the owner's sudo. The Windows and macOS
statements say only that no general headless display exists there. **[Deferred —
question for the owner; see the index at the top]** Local testing is native
Windows only, so WSL is out. **Question:** verify the two commands in a CI step
on the Linux runner (an extra job installing Sway), or leave the guidance
unexecuted?

**Resume/verification gaps — 2026-10-09:** Work after the current messaging
runtime slice was deferred at the user's wrap-up request. The queue above stays
open. Linux and macOS execution relies on CI (local testing is native Windows
only). Native foreign-account access-denial and remote-machine pipe rejection
were not exercised; retain those as security coverage gaps. **[Deferred —
question for the owner; see the index at the top]** Both need something this
machine does not have: a second local OS account (creating one needs
administrator rights) and a second machine on the network for the pipe's
remote-client rejection. **Question:** provision a second account (here or as a
CI step that creates one and runs the client as it), or accept the reading of
the ACL and the reject-remote-clients flag as the evidence? Transport CI run
`37799426844` passed after rerunning its unprovisioned macOS Clippy job; that
result does not cover later registration/runtime changes.

CI run `37922581404` (`82f51778`) passed Windows/Linux/macOS nextest, ordinary
tests under leak guards, Clippy, build/smoke, dependency checks, and
release-target builds after the streaming fixture and lock-lifetime corrections.
The duplicate-handle regression demonstrates delayed close-only release, not
proof of the earlier macOS failure's cause. Foreign-PID guard destruction — the
state a forked child is in when it drops an inherited `OwnedLock` — is pinned
since 2026-10-20 by `a_foreign_process_dropping_the_guard_keeps_the_lock` (red
with the PID check removed; run on Windows, unix by CI). A real `fork` is not
run: forking a multithreaded test binary is unsound beyond a bare `_exit`, and
the test reaches the same state without it. Unlock failure injection is
declined: its only effect is one stderr line, so a test would need a child
process to capture stderr plus an injection hook in `OwnedLock::drop`, all to
pin a message nothing reads. **[Deferred — question for the owner; see the index
at the top]** Both were declined by judgment, not on your word. **Question:**
accept both, or want either built (a unix-only CI test that forks and has the
child drop the guard; a child-process test capturing the unlock-failure line)?

During the export/editor label slice, an additional local leak-guard run
reported `a_real_tool_round_keeps_the_scrolled_up_viewport` failing with
`a turn event: Elapsed(())` and `A TEST LEAKED TEMPFILE DIRS INTO /tmp.`. The
timeout and surviving directories have not been independently attributed. The
session-write label slice also reported default-nextest shell/watch timeout
failures and a leak-guard timeout in
`dropped_backpressured_write_releases_native_handle`; reduced-concurrency runs
passed without test changes. These reports do not establish default-concurrency
reliability. New write-label tests do not directly exercise no-ID TUI saves,
fork write failures, or child snapshot wiring; those callers were inspected. The
session-read slice's all-features ordinary workspace run reported
`backpressure_write_timeout_releases_native_handle` failing with
`pending native write retained its pipe handle: Elapsed(())`; its verification
gate and separate enabled leak-guard run passed. This remains an uncorrected
intermittent native-pipe test failure, not evidence of a session-read
regression.

Config-mutation path tests cover storage and frontend failures; OAuth
completion's captured preference is source-inspected, not exercised by a
dedicated async regression test. Credential-save browser capture is likewise
source-inspected rather than covered by a network/save integration test. Both
are blocked, not skipped (checked 2026-10-20): the preference is only visible in
a save that follows a successful code exchange, and `openrouter_exchange` and
`openai_exchange` post to fixed provider URLs, so reaching the save needs the
real provider or a new endpoint override in production code. The callback half
is driven end to end by `a_refused_browser_login_reports_why`. **[Deferred —
question for the owner; see the index at the top]** **Question:** add an
endpoint override for the token exchanges (behind a cargo feature only tests
enable, since an override that ships could redirect a login's code to another
host), or accept these two as source-inspected? Credential replacement-failure
coverage uses a Windows readonly fixture
(`credential_save_write_failure_labels_follow_style`). A unix twin was declined
2026-10-18: a read-only file does not stop a unix rename, a read-only directory
fails the lock before the write, and a directory at the path fails the read —
nothing makes only `write_atomic` fail — and the label is one `with_context` in
`save_entry_at` shared by every platform, which the Windows test pins. A failed
blob-store lock is ignored by design, not oversight (`lock_blob_store`'s doc: a
save proceeds unguarded rather than lose an attachment or the session, with
`BLOB_GRACE_SECS` as the fallback, and the collector skips its pass). The OAuth
refresh persistence error is also discarded, and that one is **[Deferred —
question for the owner; see the index at the top]**: `coordinated_oauth_access`
passes `coordinated_access_core` a persist callback that drops
`save_oauth_entry_at`'s error, so a failed save of a rotated refresh token goes
unnoticed until the next start cannot refresh. `hrdr-agent` has no logging
channel, and failing the request would throw away an access token that is valid
for this session, so surfacing it means choosing a channel.
`valid_access_token`, the uncoordinated variant that drops the same error, has
no caller in the tree — it survives only as a public re-export of a published
crate (found 2026-10-16).

Intermittent failures seen in runs during the messaging work and not
root-caused: session-save timing. Watch-delivery timing was the tests' own
waits: on 2026-10-20 a full `hrdr-tools` run failed
`watch_delivers_after_the_condition_flips` (15s wait, 3 Git Bash rounds take
~11s idle) in three consecutive runs, then
`watch_one_second_timeout_gets_a_polling_opportunity` (10s), while the machine
was shared with other sessions. Both now wait `ruling_deadline` — the ceiling
plus one `CHECK_TIMEOUT` plus one interval, by which the watch must have ruled —
and two full runs passed. The other `wait_done` callers keep fixed 10s/15s waits
and have not failed. (The third,
`a_coalesced_transcript_folds_exactly_like_the_event_stream`'s `took_ms`
mismatch, was the two folds stamping a reasoning block's duration from the wall
clock; the test compares that a duration was recorded, not its milliseconds,
since 2026-10-16.)

**The pty tests read the screen by stripping escapes — needs a decision.**
**[Deferred — question for the owner; see the index at the top]**
`apps/hrdr/tests/common` `visible` removes escape sequences instead of emulating
a terminal, so text the renderer writes around cells it skips as unchanged
(`P<skip>ER<skip>REPLY` for `PEER_REPLY_SEEN` over an earlier `SENT_TO_PEER`)
never appears whole, and a substring assertion fails while the screen is right.
Frame pacing (2026-10-14) made one test hit it; that test now waits on the
server's request instead. **Question for the owner:** take `termwiz` (already in
the tree via `portable-pty`) as a dev-dependency of `apps/hrdr` and render each
test's stream into a real screen? Every screen assertion would then mean what it
says; the alternative is checking non-screen evidence test by test.

**Shell timeout tests, 2026-10-14.** The missing-prefix failures in
`tools::shell::tests::a_timeout_fails_the_call_but_a_non_zero_exit_does_not` and
`tools::tests::bash_timeout_kills_process_and_keeps_partial_output` had two
candidate causes. The post-kill drain gap was real and is fixed: a line the
command had not finished was dropped with the read loop, which the new
unfinished-line case shows deterministically (it fails without the drain). The
1s deadline could also expire before Git Bash started under load; both tests now
use `test_env::PRINT_THEN_SLEEP_TIMEOUT_SECS`. Which cause produced the
2026-10-14 CI failures is unknown — neither was reproduced on demand. On
2026-10-20 CI run `39402872279` failed the first test's PowerShell branch at the
5s deadline with no output at all, after the drain fix — so startup outlasting
the deadline is real; the constant is 10s since. Since 2026-10-18 the test runs
every shell in `Shell::available()`, so on Windows the PowerShell branches (the
unfinished line included) run beside Git Bash; they pass locally. The same day,
one full `hrdr-tools` run in WSL under load (a release build running beside it)
failed `proc::tests::git_runner_deadline_covers_readers_after_the_leader_exits`
on its `TimedOut` kind assertion; five isolated reruns passed. A full Windows
`hrdr-tools` run on 2026-10-14 likewise failed
`proc::tests::git_runner_kills_an_overflowing_program_without_waiting_for_its_timeout`
(its 2s budget for a `.cmd` printing 10,000 lines); three isolated reruns
passed. Both are `CommandRunner` wall-clock tests failing only under suite load.
Since 2026-10-16 both run against `RUN_BUDGET` (20s), far below their children's
`CHILD_WAIT_SECS`, with a compile-time assertion keeping the two apart; each was
shown red with its runner property broken. The deadline test's `TimedOut`
failure is not explained: anything else means both pipes closed inside 100ms,
i.e. the backgrounded holder never ran (a failed fork under load is the guess,
not verified). Its assertion now prints the run's status and output, so the next
failure says which.

## GitHub Actions setup-zig cache verification

`step-security/setup-zig` v2.2.2 was adopted for its Node.js 24 runtime after a
confined source audit. Fresh Zig archives are verified with Zig's minisign key,
but archives restored from GitHub Actions cache are extracted and executed
without re-verification. The current workflow limits that residual risk to a
public repository on GitHub-hosted runners with a literal Zig version and pins
the action to its audited commit. Revisit this dependency when `mlugg/setup-zig`
ships a Node.js 24 release or the replacement verifies cache hits.

## Frame cost measured 2026-08-13 — one fix shipped, one declined

Prompted by "lag as the context gets bigger". Measured with a throwaway probe
driving the real `Harness` in `--release`, against a synthetic transcript and
against a real 4800-entry session (14.7MB jsonl, folded through
`Session::load_path`). The probe was deleted; the numbers below are from those
runs.

**Shipped:** `viewport_rows` in `hrdr-tui/src/ui.rs` — the frame copies one
screenful of rows instead of every row of the blocks the viewport overlaps. A
20,000-row tool result on screen went 38.8ms → 0.23ms a frame. See the changelog
entry.

1. **An unchanged frame still materializes every assembled block — declined
   2026-10-15, measured.** Since this was first measured, transcript assembly is
   cached per pane and rebuilt only from the first dirty entry
   (`cached_assembly`, `suffix_restart`), and a running tool's static rows are
   cached with only its spinner swapped (`ChunkRows::AnimatedChrome`). What is
   left is `materialize_transcript` calling `flush` on every assembled block
   each frame. Re-measured in `--release` on Windows with a throwaway probe
   (deleted) over alternating user/assistant entries at width 100: 500 entries
   30 µs, 2000 entries 124 µs, 4800 entries 329 µs per unchanged frame — about
   0.07 µs per entry, a third of the 2026-08-13 figure. Caching the materialized
   prefix would touch the scroll and hit-testing maths keyed off `cum` for a
   fraction of a millisecond; revisit only with a much larger transcript or a
   felt-lag report.

**Measured and cleared — do not re-investigate without new evidence.** Each of
these was a plausible theory that the numbers killed:

- **Render cost per frame.** Real 4800-entry session: 0.94ms warm, and 1.0ms
  mean while streaming a reply token by token. Keystroke handling 178ns. Not the
  lag.
- **`Arc::make_mut` on the agent history deep-copying the context per push.**
  `Agent::history_strong_count` was added temporarily to observe it: the
  refcount is 1 before and after a turn, so `make_mut` mutates in place, and a
  deep clone of a 3.9MB history is 181µs anyway.
- **Session autosave.** Per turn, off the UI thread; 1.5ms at a 3.9MB history.
- **Memory.** RSS 56MB with the real 4800-entry session loaded and rendered —
  the per-entry render caches are not a leak at realistic sizes.
- **Whole-turn overhead**, mock endpoint, so hrdr's own CPU only: 1.8ms at 321KB
  of history, 14.9ms at 3.9MB.

**Coverage gap:** everything above is hrdr's own CPU against a local mock
endpoint. Time-to-first-token, streaming throughput over ssh, and terminal write
cost were NOT measured, and a big context makes the provider's own prefill
slower regardless of anything hrdr does. If the lag is still felt after this
fix, that is where to look next — start by asking which lag it is (typing,
scrolling, or waiting for the reply).

## Performance review — second pass 2026-08-04

A fresh `:perf` run over the whole tree (working tree clean at the time). Items
1 and 3 re-found the first review's still-open #1 and #5 and add specifics.
Every finding was re-verified at its cited lines before recording; one candidate
from the run was dropped (item 4).

**Status: item 1 (the per-round full-history save) shipped 2026-10-15 as the
round journal; items 2-4 dropped — not fixable as proposed, or disproved at
review time.** Each item carries its own tag below.

2. **~3-4 heap allocations per streamed token.** **[dropped — infeasible: the
   event must survive to the frontend's `on_event`
   (`AgentRegistry::start_turn`), so `record` cannot give ownership to
   `from_event`; a borrowed-`Record` variant is a larger refactor for one small
   clone per chunk]** Per chunk: `Accumulator::push` clones the delta
   (`hrdr-llm/src/types.rs:820`), the reasoning path clones
   (`turn_loop.rs:223`), `record` clones the whole event into the log
   (`registry.rs:427`), `Record::from_event` clones every string field again
   (`transcript_log.rs:111-143`), and the pane drain clones once more
   (`registry.rs:103`). Fix: `Record::from_event` takes the event by value (the
   `on_event` closure in `AgentRegistry::start_turn` owns it and drops it
   immediately), moving the strings — cuts ~2 allocations per token.
3. **No-usage fallback re-estimates the whole history every round.** **[dropped
   — risk > value: a running token counter must track ~11 message-mutation sites
   including in-place edits (`turn_loop.rs:408`, `compaction.rs:531`); a missed
   one silently corrupts the budget cap for a ~µs-per-round win]**
   `Agent::account_usage` (`budget.rs`): when the endpoint reports no usage,
   `estimate_tokens_in_messages(&self.messages)` is O(N) per round. Fix: keep a
   running prompt-token estimate, add only messages appended since the last
   call. Same as the archived review's #5, still open.
4. **Dropped: compaction "overlapping suffixes".** The run claimed
   `compaction_tail_start` re-sums a growing slice per candidate turn — the
   archived first review's #7 made the same claim. It does not reproduce: the
   loop walks candidates newest→oldest with `tail_start` set to each `start`, so
   every `estimate_tokens_in_messages(&msgs[start..tail_start])` covers one
   disjoint turn; total work is O(tail), which the budget check needs.
   `mega_turn_tail_start` estimates single messages — fine. The archived #7's
   "per-stage history clone in the ladder sizing" does not reproduce either
   (checked 2026-10-18): `first_viable_compact_stage` builds the elided history
   once, memoized in its `elided` argument, and `estimate_tail_window` sizes
   each tail stage over a slice of it.

**Not settled without profiling:** item 2's real lock-contention timing. (The
per-frame transcript layout loop at very long transcripts was measured
2026-10-15 — see "Frame cost measured 2026-08-13".)

## Tidy review 2026-08-04

Quality pass over the whole tree (clean at the time); every candidate re-read at
its cited lines, behavior-preserving only, ranked by confidence.

**Status: item 9 open (needs direction — external API); items 1-8 shipped.**

9. **Low: unused re-export `apply_cache_breakpoints`.** **[Deferred — question
   for the owner; see the index at the top]** **[needs direction — removing a
   `pub use` from a published crate is an external-API decision]**
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
  memories. The search-ranking test discriminates the shared `Lowered::score`
  path; a separate ordering test was judged redundant.

## Performance review 2026-08-06

`:perf` (whole codebase — the working tree was clean) over two passes
(hrdr-agent + hrdr-llm; hrdr-tools + hrdr-tui + hrdr-app + hrdr-editor +
hrdr-test-support + apps/hrdr). The one item left open by decision is below,
re-traced at its cited lines.

**Context — `budget`'s O(H) token estimate per round when the server reports no
usage.** `Agent::account_usage` in `crates/hrdr-agent/src/budget.rs`
(`estimate_tokens_in_messages(&self.messages, …)` per round when `acc.usage` is
None). Cheap per pass; hoistable into a running total, but needs care across
compaction/resume. Not worth it unless the no-usage case (self-hosted servers)
is common.

**Recorded items confirmed still present (not re-recorded):** the per-frame
full-transcript layout walk (`ui.rs:2664` `transcript_chunks` + `:912-918`
`cum` + `:974-1006` hit map — O(entries) per frame, cached bodies make per-entry
work cheap) and the per-frame cache/save pipeline (autosave turn-end, off-thread
behind a coalescer).

**Settled since (2026-10-15):** the per-frame transcript walk was measured — 329
µs for an unchanged frame at 4800 entries, recorded under
[Frame cost measured 2026-08-13](#frame-cost-measured-2026-08-13--one-fix-shipped-one-declined)
— and an in-flight tool call no longer re-renders its body per frame: its static
rows are cached and only the spinner changes (`ChunkRows::AnimatedChrome`).

## Dependency upgrades held back, 2026-10-14

The 2026-08-03 entries are resolved: `hjkl` is on 0.41 and `reqwest` on 0.13.
Kept from that sweep, because it is what caught a silent failure: `toml` 1.0
changed `Value`'s `FromStr` to parse a single VALUE rather than a document, so
`text.parse::<toml::Value>()` fails on any real config with
`unexpected content, expected nothing`; parse a `toml::Table` for a document.

On 2026-10-14 `cargo update` took every compatible release and
`cargo upgrade --incompatible` moved the `hjkl-*` crates to 0.42 and `zstd` to
0.14, both without source changes. `cargo update --verbose` then reports only
`generic-array` 0.14.7 and `psm` 0.1.24 behind latest; neither appears in
`cargo tree --target all`, so they are lockfile entries held by upstream pins
with nothing to do here.

## Needs a decision 2026-10-14 — replacing `serde_yaml_ng`

**[Deferred — every item in this section is a question for the owner; see the
index at the top]**

The scheduled `cargo-deny` run began failing on RUSTSEC-2024-0449:
`unsafe-libyaml` is unmaintained. It is an "unmaintained" advisory, not a
vulnerability, and it reaches us only through `serde_yaml_ng` 0.10.0, which is
still its newest release. `deny.toml` now ignores it with a reason, the same
treatment as the two syntect-borne advisories. **Question for the owner:**
should the YAML crate be replaced? It is used for memory/skill frontmatter
(`parse_frontmatter`) and for CI workflow parsing in `gate.rs`, which reads
untrusted YAML. A pure-Rust parser such as `noyalib` or `serde-saphyr` would
remove the C-transpiled `unsafe` code, but it is a new dependency. Any swap must
keep `gate.rs`'s recursion and byte caps, and the strict-frontmatter behaviour
described above. Until there is a decision, the ignore stays.

## Compaction rewrite

The plan (`docs/compaction-rewrite-plan.md`) is archived into this section. The
binding decisions it left are listed below; the one audit gap still open is
under _Audit items needing a decision_.

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
**[Deferred — question for the owner; see the index at the top]** **Question:**
spend a live-key measurement on it (two requests against Anthropic, comparing
cached tokens for a whole-history and a head-only summarization), and switch to
head-only if it still hits?

### Left open by the session-accounting work

**There is still no check that the prompt cache actually hits.** The counters
`AgentUsage::cache_hit_rate` feeds are reported, tested and readable, but every
test drives them from mocked usage figures — nothing proves a real provider
serves a real hit. `CompactionReport`'s own doc admits why: it needs a live key
and two sequential requests. The right shape is an `#[ignore]`d integration test
that runs a turn, compacts, and asserts `cached_tokens > 0` on the summarization
call, wired into a nightly CI job with a secret. That is an infrastructure
decision, not a code change. **[Deferred — question for the owner; see the index
at the top]** **Question:** add a provider key as a CI secret and a nightly job
running the ignored test? Until it exists, the session rate in `/cost` is the
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
  **[Deferred — question for the owner; see the index at the top]** by the
  notice redesign (`2bff248`).** The variant-mixing blocker is gone:
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

**[Deferred — every item in this section is a question for the owner; see the
index at the top]**

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
  parse and its test are already there. **[Deferred — question for the owner;
  see the index at the top]** **Question:** show them in the `/commands` picker
  row, or keep them stored only?
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
  worth revisiting deliberately rather than by default. **[Deferred — question
  for the owner; see the index at the top]** **Question:** add `skill` to
  `JAIL_TOOLS`, so a jailed agent can load a bundle it is listed, or keep jail
  to the five read tools?
- **`prepare_outgoing_tracked` re-walks both command and skill roots on a `:`
  submit** (`crates/hrdr-app/src/util.rs`) — the walk sits inside the
  `input.trim_start().starts_with(':')` guard, so an ordinary message costs
  nothing; only a message that opens with `:` pays for it, and it pays on top of
  the cached copies the TUI already holds for the popup (`App::commands` /
  `App::skills`). That is the pre-existing command behaviour extended, not a
  regression, and it has not been measured.

  **Open: whether the send path should use the frontend's cached sets instead of
  re-walking.** **[Deferred — question for the owner; see the index at the
  top]** The trade is immediacy against I/O. Today a command file added
  mid-session expands the moment you type its name — the caches are only
  refreshed by `/reload`, `/cwd` and `apply_cwd`, so reading them instead would
  mean a newly written `.hrdr/commands/foo.md` is `:foo` verbatim until one of
  those runs. Against that, the walk is per-`:`-submit filesystem work that the
  popup has usually just done. It also touches the seam this path now carries:
  the caches are already discovered with the session's `ProjectInstructions`, so
  switching to them would keep the trust gate honoured either way. Not changed;
  needs a call on which behaviour is wanted.

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
- **No lock test waits out a staleness window in real time.** **[Deferred —
  question for the owner; see the index at the top]** The cross-process
  `store_lock` tests (`LockHolder`, driving the `#[ignore]`d
  `child_process_holds_the_lock`) run on every platform, liveness probe
  included, but every staleness test backdates the timestamp: the clock
  arithmetic is exercised, a minute of real waiting is not. **Question:** add an
  `#[ignore]`d test that waits out a real window (a minute per run, run by
  hand), or accept the backdated coverage?
- **An image is now priced for the endpoint it is bound for** (`TokenTarget` in
  `media.rs`, threaded through `estimate_tokens_in_messages`), which closes the
  "charged at Anthropic's tier for every dialect" entry that stood here. What is
  still an approximation, deliberately:
  - **Anthropic is always charged the high-resolution tier**
    (`HIGH_RES_MAX_EDGE_PX`/`HIGH_RES_MAX_IMAGE_TOKENS`). Which tier a model is
    on is a property of its generation ("Claude 4.7 and later"), and the only
    way to decide it from a model id is a hard-coded name list that goes stale;
    stale here means charging 1568 for an image that costs 4784. Reconsider only
    if models.dev ever publishes the tier.
  - **OpenAI is charged an uncapped 32×32 patch count** (`openai_patch_tokens`),
    which is what `"detail": "auto"` costs on the GPT-5.6 family. The two
    documented under-estimates it cannot see from a width and a height: the
    `-mini`/`-nano` multipliers (×1.62–×2.46), and the tile method used by
    GPT-4o/GPT-4.1/o-series, which scales a _small_ image's short edge up to 768
    px and so charges more than its patches (a 200×200 image is 49 patches here,
    ~765 tokens there). Both would need a per-model table hrdr does not have.
  - **Round-half-to-even is transcribed but not pinned by a vendor number.**
    `div_round_ties_even` follows Anthropic's published reference
    implementation, and `halves_round_to_the_even_neighbour` asserts the rule
    itself — but no image in the published table lands on a tie that changes its
    patch count, so nothing proves the two interact as the vendor's code does.
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
  the docs, not against a live endpoint that accepted them. **[Deferred —
  question for the owner; see the index at the top]** **Question:** run a manual
  smoke with your keys (one image to each dialect: OpenAI, Anthropic, Codex), or
  accept the docs-pinned shapes?

### Left by the vendor-doc audit, 2026-08-11

Every field of `anthropic_block` / `responses_item` / `openai_part` was checked
against the current vendor docs and matches them; the per-image ceiling was
raised to Anthropic's documented 10 MB (it had been 5 MB, which is the
Bedrock/Google-Cloud number, and hrdr never speaks the Messages API to either).
What that audit did not close:

- **No dimension check anywhere.** **[Deferred — question for the owner; see the
  index at the top]** The vision docs cap an image at 8000x8000 px, and apply a
  stricter per-image dimension limit once a request carries more than 20 images
  ("resize each image so that neither dimension exceeds 2000 px").
  `image_dimensions` already reads width and height for the token estimate, so
  the check is cheap to add — it was left out because a refusal hrdr invents is
  worse than a provider error hrdr reports, and nobody has hit it. **Question:**
  add a client-side refusal (or a downscale) before sending, or keep reporting
  the provider's error?
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

- **`git stash -p` is still not caught**, and deliberately so — the rest of that
  gap closed by widening the interactive rule's alternation to `checkout`,
  `restore` and `reset`. `stash` cannot join them as the rule stands: the flag
  arm matches anything between the subcommand and the `-p`, so including it also
  refuses `git stash show -p`, which only prints a diff, with a message claiming
  it needs a TTY. The regex crate has no lookaround to say "not `show`", and a
  hung shell hits its own timeout while a false refusal never resolves.
  `patch_mode_is_interactive_too` asserts `git stash show -p` runs, so whoever
  widens it lands there. Closing it properly wants the parsed-command work in
  the peer-comparison section, not a longer regex.
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

**[Deferred — every item in this section is a question for the owner; see the
index at the top]**

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

**[Deferred — every item in this section is a question for the owner; see the
index at the top]**

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
  The JSON-string case pi repairs is now handled for hrdr's one array argument
  that a string cannot also mean: `todo`'s `todos` (2026-10-15). `task`'s
  `attachments` is left refusing a string with the exact form to send, since a
  bare string there could just as well be one path.
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

- **Project `AGENTS.md` is writable by a write sub-agent** **[Deferred —
  question for the owner; see the index at the top]** and read back as project
  conventions on the parent's next prompt rebuild (`/clear`, `set_cwd`, a new
  agent). The trust answer is given once, at open, so a sub-agent editing the
  file mid-session changes what the parent reads next without being asked again.
  Left open on purpose: `AGENTS.md` is also how a project legitimately carries
  instructions, and narrowing it costs that. A `// NOTE:` sits on the push site
  in `build_system_prompt_sections`. Closing it would mean the `memory`-tool
  treatment (main agent only), which that tool could afford because it had no
  second use.
- **Project commands and skills shadow built-ins by name.** **[Deferred —
  question for the owner; see the index at the top]** `command_dirs` and
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

  Coverage: `a_jailed_session_neither_offers_nor_expands_the_projects_own_names`
  (`hrdr-tui/src/app/e2e.rs`) drives `/reload`, the `/commands` picker and a
  `:name` submit under a jailed harness, and
  `a_skipped_project_expands_neither_its_commands_nor_its_skills`
  (`hrdr-app/src/util.rs`) covers the send-path seam directly.
  `a_jailed_session_moved_by_a_resume_reads_no_project_names` (2026-10-18)
  covers `App::apply_cwd`, reached from `apply_session` when a resumed session
  names a different cwd — the path that moves a jailed session without asking
  the trust store, and `a_jailed_session_moved_by_cwd_reads_no_project_names`
  (2026-10-20) covers `TuiHost::cwd_changed` (`/cwd` into a directory the user
  _has_ trusted); both go through `App::rediscover`.

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

- **`std::env::temp_dir()` is granted whole** **[Deferred — question for the
  owner; see the index at the top]** in `write` mode, not just
  `session_scratch_dir()`. Broader than the stated need, and pre-existing.
  **Question:** narrow it to the session scratch dir (compilers and tools that
  write `TMPDIR` would then fail under the sandbox unless pointed there), or
  keep the whole temp dir writable?
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

- **Curated read allow-list for `write` mode** **[Deferred — question for the
  owner; see the index at the top]** — still the honest gap, but the shape
  changed. **Question:** build an allow-list (which paths outside the workspace
  may a `write`-mode shell read), or leave reads open with the output redaction
  below as the only guard? A `shell` command in `write` mode can still
  `cat ~/.ssh/id_rsa`. What did land is narrower and real: `shell` output now
  drops lines naming a credential file AND redacts the hunk body of a diff
  touching one (`DiffRedactor`), so the _accidental_ leak — a broad `rg`, a
  `git diff` of `.env` — no longer reaches the transcript. Deliberate
  exfiltration is untouched and admits it.
- **No shell-command pre-flight** **[Deferred — question for the owner; see the
  index at the top]** — still open, and cheaper than it was: the EROFS note now
  names the remedy (`sandbox_writable_roots`, `--sandbox-writable-root`,
  `!command`), which was most of what a pre-flight would have said.
  **Question:** is a pre-flight still wanted, and if so what should it check
  before a command runs?
- **Windows `write` mode has no OS confinement** **[Deferred — question for the
  owner; see the index at the top]** (`read` mode is confined by Mandatory
  Integrity Control since 2026-08-02; `write` still takes
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

- **`--sandbox jail` cannot apply to a write-capable session** **[Deferred —
  question for the owner; see the index at the top]**, so it floors at `write`
  and emits a notice pointing at the `prisoner` agent. That is honest but it is
  not what the word means. A jailed _main_ agent is coherent (five tools, no
  shell — an audit session), and the write floor exists for sub-agents that must
  write. Worth revisiting as "session jail means jail, and a write-capable
  sub-agent under it is the thing that floors".
- **`jail` loses `git log` on the audited repo** **[Deferred — question for the
  owner; see the index at the top]** — real provenance value, and the accepted
  cost of having no subprocess. Argues for a narrow read-only git capability
  later, not a general shell.
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

- **Trust can be granted and never revoked.** **[Deferred — question for the
  owner; see the index at the top]** `hrdr_agent::trust` has `is_trusted` and
  `trust`; there is no `untrust`, no listing, and no command that reaches
  either. Undoing an answer means finding `$XDG_CACHE_HOME/hrdr/trusted-dirs`
  and editing it by hand — which is fine for the owner and is not a thing a user
  can be expected to discover. The asymmetry is deliberate as far as it goes
  (only the yes is stored, so declining needs no undo), but a directory whose
  provenance turns out to be wrong is exactly the case the gate exists for.
  Smallest honest version: a `/trust` command that prints the store and takes
  `revoke [path]`, defaulting to the current directory. Needs a decision about
  whether revoking mid-session also has to downgrade the running session, which
  is the same terminal-ownership problem as answering the question mid-session.
- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap because
  `rebuild_index` regenerates the pointer index on every mutation (verified), so
  files and index cannot drift **structurally**; what is missing is semantic
  staleness plus the usage-tracking half recorded above. The external-drift half
  shipped 2026-08-05 (`5bc2e5d`) — see the memory item in
  [Permissions, isolation, and state](#permissions-isolation-and-state).
  **[Deferred — question for the owner; see the index at the top]**
  **Question:** what should count as stale or contradicted (an age, a file the
  memory names no longer existing, a model-judged conflict), and should a pass
  prune or only flag?
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
- **hrdr sets no Windows ACL on any file it writes.** **[Deferred — question for
  the owner; see the index at the top]** The residual of the Windows-drift audit
  pass, which **ran** and landed three fixes (`8e5bc9d`): the credential
  `sync_all` gated on unix though portable, `atomic_write`'s symlink guard
  likewise, and owner-only file creation re-decided at four sites. All ~130
  `cfg` gates were classified at the time; `proc.rs` and `prompt.rs`'s
  package-manager names are deliberate and documented. (The unix-gated _tests_
  that pass called "not findings" were mostly shell and python tests Windows
  could run all along, and now does — see "Windows support review 2026-09-15".)
  The guarantee left on Windows is the containing per-user directory's inherited
  default, stated once on `hrdr_llm::owner_only_options`. Per-user ACLs need a
  new dependency in `hrdr-llm` and are a deliberate non-goal until someone runs
  hrdr on Windows in anger. **Question for the owner (2026-10-15):** that
  condition now holds — the owner develops and runs hrdr natively on Windows.
  Should credential and session files get an explicit owner-only DACL? The
  dependency would be `windows-sys` (already in the tree through `hrdr-tools`
  and `hrdr-tui`, with `Win32_Security_Authorization` for the Low-integrity
  labels), added to `hrdr-llm`; the cost is the code to build the DACL and its
  tests. Leaving it means a credential file is protected only by the profile
  directory's inherited ACL, which on a standard install is already per-user.
- **The no-follow open covers only the final path component.** **[Deferred —
  question for the owner; see the index at the top]** A symlinked _parent_
  directory is still traversed on the wire-log open, on unix (`O_NOFOLLOW`) and
  Windows (`FILE_FLAG_OPEN_REPARSE_POINT`, since 2026-10-16) alike. Recorded on
  `owner_only_options_no_follow`; closing it properly means resolving the whole
  path under a directory handle (`openat` on unix, `NtCreateFile` relative to a
  directory handle on Windows). **Question:** build that (L: platform code on
  both sides, for the wire log and now the trust store), or accept
  final-component protection for same-user files?
- **`CommandHost`'s text-fallback defaults are now test-only.** **[Deferred —
  question for the owner; see the index at the top]** Found in the 2026-08-02
  web-removal sweep, not fixed — the parent's call, and the Standing-constraints
  host seam is deliberately kept. `begin_login`, `begin_model_selector`,
  `begin_model_selector_for`, `begin_session_selector`,
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

---

## Test coverage gaps

- **`watch` against a real GitHub run: manual smoke never ran.** **[Deferred —
  question for the owner; see the index at the top]** The watch-tool plan's last
  item (watch an actual tag-run CI run through `gh`) needs a live GitHub run, so
  it was not runnable in the automated suite; every automated slice shipped (the
  poller lifecycle, guardrails, cancel, delivery, schema bounds, prompt
  rewiring). Run it before trusting the release flow's watch step on a real run.

- **DeepSeek built-in provider: manual smoke slice never ran.** **[Deferred —
  question for the owner; see the index at the top]** The provider plan's slice
  7 (single-turn + agentic tool-call turn against the real API) needs a live
  `DEEPSEEK_API_KEY` and was not runnable in the automated suite; every other
  slice shipped. Run it before trusting a real DeepSeek session.

- **The warm models.dev catalog path is uncovered, deliberately.**
  `catalog::load_cached` reads process-global state (`HRDR_MODELS_PATH` / the
  XDG cache dir), so warming it in a test leaks into every other test in the
  binary. The cold path and the `ANTHROPIC_MAX_TOKENS` 8192 fallback are pinned;
  the warm resolution rules are covered separately in `catalog`'s own tests.
  Recorded so nobody "closes" it with a test that only appears to work.
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

**[Deferred — every item in this section is a question for the owner; see the
index at the top]**

Each is pinned by a test and commented in the source; what is missing is a
decision, not work — except the last, which is a missing feature.

- **Whether a reasoning replay should follow the FIELD rather than the model
  name.** The `reasoning_content` graft in `Client::body_json` was keyed on
  DeepSeek's own host, which meant every gateway serving DeepSeek — OpenCode
  Zen, OpenRouter, Together — dropped the field and got
  `The reasoning_content in the thinking mode must be passed back to the API`
  back as a 400. The gate now also fires on a wire model id naming deepseek,
  which covers the gateways. What is still keyed on a NAME is the general case:
  any provider streaming `reasoning_content` may want it echoed, and a model id
  is a guess about that. The alternative — replay whenever the assistant message
  carries reasoning the endpoint itself produced — was NOT taken because a
  mid-session model switch would then send one model's reasoning to another
  model's API, and nothing on a `ChatMessage` records which endpoint produced
  it. Recording that provenance is the real prerequisite; decide it before
  widening again.

  **Worth knowing before diagnosing the next report, because two plausible
  explanations were wrong first.** The failure looks intermittent — in the
  session that reported it, the main agent survived five requests after a
  `reasoning_content`-bearing turn while one sub-agent survived four and failed
  the fifth, on the same model, endpoint and codepath. It is not intermittent.
  The requirement is per assistant TURN, not per thought: once a conversation is
  in thinking mode every assistant message must carry the field, and a turn the
  model answered without thinking has none to carry, so a run dies exactly when
  its history first contains such a turn. Two theories were entertained and
  killed by measurement against the live endpoint: that Zen fans out and only
  some upstreams enforce the rule (the error body names one,
  `Error from provider (Console)`), and that Zen streams reasoning under the
  `reasoning` key that `Delta` does not read — it streams `reasoning_content`,
  confirmed on the wire. **Replay the endpoint before theorising**: a two-round
  tool-calling conversation, once with the field omitted and once with it
  present, answers this in one command. Note also that a model reading its own
  transcript concluded the model "fails deterministically at the gateway" and
  burned two further delegations switching models on that belief.

- **408/522/524 are retryable only because `classify_status` says so.**
  `is_transient`'s text fallback has needles for the other six transient
  statuses and none for these, so those three arms are the only thing making a
  Cloudflare origin timeout retryable rather than fatal. A test derives the
  unprotected set from real behaviour, so both deleting an arm and quietly
  adding a needle fail loudly. Giving them needles is a behaviour change nobody
  has asked for — decide, don't drift.
- **`retry_after_hint`'s text scan matches exactly one phrasing.** The
  mid-stream half of the rate-limit fix rests on it — a mid-stream error arrives
  inside a 200, which rarely carries `Retry-After`, so the delay usually exists
  only in the message the gateway wrote — and the scan splits on the literal
  `retry-after:` and reads the digits after it. A provider that writes
  `try again in 12s`, `please retry after 12 seconds` or names an absolute time
  is not matched, and hrdr falls back to its own backoff exactly as before.
  Widening it is a parser change with a false-positive surface (any number in
  any error message that happens to follow a matching phrase), so it wants a
  real provider message to justify each spelling added rather than a guess at
  the set. Nobody has collected those messages.
- **An unrecognized Anthropic `stop_reason` still reports
  `truncated() == false`.** Deliberate: it now raises a notice naming the value
  (`1871631`), and guessing a direction is wrong either way — folding to `stop`
  hides a truncation, folding to `length` calls a refusal truncated. Revisit
  only if a real reason turns up that wants a fourth answer.

---

## Left open by the compaction fix (`/compact` pane targeting)

The data-loss half is fixed: the TUI now summarizes the conversation on screen.
Two residuals, both from the same root — the turn **handle** slot is
session-wide while the compaction is now per-pane:

- **The TUI can no longer Esc-cancel a sub-pane compaction.** **[Deferred —
  question for the owner; see the index at the top]** `in_flight`, `is_busy` and
  `cancel_turn` are all keyed to `MAIN_KEY`, so with the clock now on the sub
  key they see nothing in flight. Before the fix Esc did cancel it — while also
  stopping main's clock, which was the defect. Needs a decision on per-pane
  cancellation, not a patch.
- **The TUI's compaction result line lands in main's transcript** **[Deferred —
  question for the owner; see the index at the top]** (`push_entry` is
  main-scoped), so "compacted: N → M" appears in the main conversation even when
  a sub-agent was summarized. TUI system lines are main-scoped generally;
  changing that is a wider call.

---

## dispatch.rs review — findings 2026-08-02

`hrdr-app/src/commands/dispatch.rs` was read in full for the first time, with
its sibling modules, the `CommandHost` trait and its call site. Its one finding
(`/cwd` and `/status` on a sub-agent pane) shipped 2026-10-15; what is left is
the record of what was checked.

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
`commands/prompt_commands.rs`, `completion.rs` and `sessions.rs` were reviewed
in full 2026-10-20 (one read-only pass, findings re-traced): `login.rs`'s four
findings shipped; the other three had one cosmetic finding, since fixed.
`open_system_handler`'s macOS arm is `#[cfg]`-gated and compiled only on CI; the
Windows arm compiles locally now.

---

## Known behaviour to revisit

Not bugs; things whose surprise is worth having written down.

- **A blocking hook that cannot run lets the action through.** **[Deferred —
  question for the owner; see the index at the top]** `run_event_hooks`
  (`hrdr-tools/src/hooks.rs`) blocks only on a hook that exits 2. A timeout, a
  spawn failure or any other exit becomes a note in `HookOutcome::notes`, and
  the tool call or user message goes ahead — so a guard hook fails open exactly
  when the machine is loaded or its shell is broken. Seen 2026-10-18:
  `peer::tests::peer_delivery_bypasses_human_processing_and_preserves_provenance`
  (an `exit 2` `user_prompt` hook) let the human message through once in a full
  Windows workspace run beside other sessions' builds, then passed three times
  alone. Which of the three outcomes it hit was not captured. Options: keep
  failing open (a broken hook never wedges a session), fail closed for
  `pre_tool`/`user_prompt` only (the two events whose exit 2 means "stop"), or
  make it a per-hook setting. Fail-closed also needs the user to be able to get
  out of a session whose hook is broken.
- **The memory cache can be off entirely on a Windows dev box.**
  `mtime_granularity_is_fine` (`hrdr-tools/src/memory.rs`) probes three rapid
  writes, and a root whose mtimes do not separate them is never cached, so
  `load_memories` re-reads every file on each call. Observed 2026-10-16: in a
  `tempfile` directory on this Windows 11 machine, two loads did not share the
  cached memory until the test forced the verdict
  (`a_cache_hit_shares_the_cached_memory`). Correct by design (a stale hit is
  worse than a re-read); it only means the cache's win is platform-dependent.

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

- **A profile can allow-list itself into having no search tool.** **[Deferred —
  question for the owner; see the index at the top]** `read_only` keeps a shell
  on purpose, and `jail` keeps `grep`/`find`/`ls`/`tree`; but an `allowed_tools`
  list naming neither leaves an agent that can only `read` by exact path.
  Nothing validates that at load, and no prompt section can name a search tool
  it does not hold — `base.md` says "whichever search tool you hold" precisely
  because there may be none. **Question:** refuse such a profile at load, warn
  about it, or allow it?
- **`write.md` is the largest always-on prompt** after the git/release split,
  and most of its rules carry a shouted ALL-CAPS header. Emphasis that dense
  stops selecting. Neither is a defect; both are what is left of "the always-on
  prompt is too long".

- **A mid-stream retry leaves the cut-off reply on screen.** `drain_stream`
  forwards `AgentEvent::Text` to the frontend as it arrives, so a stream that
  fails after emitting some output and is then retried — by `RetryBudget` on a
  transient error, by `recover_context_overflow` on a drain-time
  `context_length_exceeded`, or by `drop_unsupported_param` — re-streams the
  model's reply from the start, and the frontend has no signal to discard the
  first partial. It is not doubled into one block: every retry path emits a
  `Notice` first (the drain retry's "stream interrupted — retrying", the
  overflow's compaction notices, the dropped parameter's queued notice), and
  `transcript::apply_event` starts the restreamed text as a new assistant entry
  after it. So the user sees the partial, the notice, then the whole reply; the
  history the model sees holds only the reply. Pinned for the drain retry by
  `mock_server::a_mid_stream_retry_keeps_the_partial_and_the_reply_apart`
  (mutation-checked 2026-10-19: without the notice the two run together as "Half
  an ansThe whole answer"), and for the overflow by
  `a_mid_stream_overflow_keeps_the_partial_and_the_reply_apart` (2026-10-20:
  without its two notices the same run-together). The dropped parameter's path
  is by reading — a parameter is refused at connect, before any text. Hiding the
  partial outright needs a "discard what I streamed for this round" event, which
  is a protocol change across every `AgentEvent` consumer and the transcript
  log, not a local one — not worth it while the partial is labelled.
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
— closed 2026-10-15 for every lock hrdr now writes: each records its owner's
`ProcessLease`, and `is_stale_lock` reaps a lock whose pid now belongs to a
different process. What is left: a lock written by an older hrdr (no lease), or
one whose owner cannot be probed (`ProcessLiveness::Unknown`), still falls back
to the pid alone; the wrap-up round shares `overflow_compacted` (`turn_loop.rs`)
— a second overflow on the forced wrap-up errors the turn. Giving the wrap-up
its own once-only flag was tried and declined 2026-10-20: the first compaction
already summarized everything before the current turn, which compaction keeps
verbatim, so the second one has nothing to shrink and the turn errors anyway
("too large to compact … nothing left to shrink"). Only a mid-turn steer adds a
turn a second compaction could fold. **[Deferred — question for the owner; see
the index at the top]** Declined by judgment after trying it. **Question:**
accept that, or give the wrap-up its own compaction for the steered case anyway?

**From the 2026-08-05 correctness pass:** `fetch`'s `is_blocked_host` is
advisory — correctness rests on the connect-time `SsrfGuardResolver`, one
refactor from being the only guard (`web.rs`).

**From the 2026-08-05 security audit:** `atomic_write` write-path TOCTOU
(`tools/mutation.rs` — admitted in the comment; requires a hostile process
racing the agent's own edits); **[Deferred — question for the owner; see the
index at the top]** MCP tool descriptions ride into the tools cache block
unwrapped (`handshake_and_list` in `mcp/client.rs`, `Box::leak`) — a compromised
operator-installed server can steer the model through its descriptions, where
results are wrapped as untrusted; `/export` writes to any absolute path the user
names (`export_conversation` — equivalent to the user's own shell redirection,
but the transcript contains model output). **Question (MCP):** wrap MCP tool
descriptions in the untrusted-content envelope — they are written by the server,
which you installed but may not control — at the cost of a longer, noisier tool
block, or keep treating them as operator-trusted config?

**From the 2026-08-06 correctness pass:**

- `Agent::run`'s round loop (`turn_loop.rs`) — `max_steps` is no longer a hard
  per-turn cap: every delivered steer resets the round budget, so sustained
  steering (or a held key) keeps the turn alive indefinitely; the cap only binds
  once steers stop. Deliberate and test-pinned, but the meaning changed.
- `context_after` (`compaction.rs`) is a ~4-bytes/token local estimate, not a
  provider reading; the post-compaction gauge drifts from the provider's real
  `prompt_tokens` (the estimator deliberately under-counts).

**From the 2026-08-06 security audit.** One claim was demoted from a finding to
hardening, with the trace that disproved the repro: the claim
"`SandboxMode::Read` does not confine the `memory` tool's writes"
(`MemoryTool::execute` uses bare `fs::write`/`remove_file`; in `Read` mode
`SandboxPolicy::writable_roots` is empty by construction). Traced unreachable in
the default configuration: `effective_sandbox` floors a write-capable session's
`read` request to `Write` (test-pinned by
`effective_sandbox_matches_the_decision_table`), so `SandboxMode::Read` is only
ever entered with `read_only = true` — and the read-only tool scoping withholds
`memory` (a write tool; `Tool::read_only` defaults false, and `MemoryTool` never
overrides it). Residual, recorded as hardening below.

- **The `memory` tool writes outside the sandbox roots in every mode**, by
  design. The one path to it on a read-only agent — an explicit `tools:`
  allow-list naming it — is closed (2026-10-15): a read-only agent's allow-list
  can only narrow its read-only set (`Agent::new`; test
  `a_read_only_allow_list_cannot_add_a_write_tool`).
- The OpenRouter callback server reflects the attacker's own (escaped) state
  string (`parse_callback`, `oauth.rs`); its comment against echoing
  `expected_state` should be preserved — a future "echo the expected value"
  change would leak the CSRF token.
- `trust::trust` opens the trusted-dirs store with `create(true)`: a
  pre-existing world-readable file keeps its mode. Mitigated (re-checked
  2026-10-15): the store's directory is forced to `0700` on every trust write
  (`auth::create_dir_owner_only`, existing directories included), so no other
  user can reach the file. Low sensitivity (directory paths). A final-component
  symlink is refused since 2026-10-20 on write and on read
  (`a_symlinked_store_is_not_written_through`,
  `a_symlinked_store_is_not_read_through`); a symlinked parent directory is
  still traversed — `owner_only_options_no_follow` covers the final component
  only, and the parent is the user's own 0700 cache directory.
- Write-path TOCTOU is documented, not closed: `resolve_write` canonicalizes,
  then `atomic_write` re-opens the path; a symlink swap between the two could
  redirect a write. Same-user, no privilege boundary. Closing it is the
  handle-relative no-follow opens question under Consistency / robustness
  (deferred to the owner there).
- Landlock grants full `AccessFs::from_all` (incl. `REFER`) on writable roots —
  safe only because the `/` rule lacks REFER on the source side of a cross-root
  rename; the `NotEnforced` → refuse-spawn check is the backstop.
- Seatbelt profiles are author-written (`seatbelt_profile`, `sandbox.rs`). The
  `write` profile is validated by the kernel on macOS CI —
  `shell_write_outside_roots_is_denied_under_seatbelt` runs a real
  `sandbox-exec` and passed in run `39411371973` (checked 2026-10-20). The
  `read` (strict) profile and the network allowance are only compared as text.
  **[Deferred — question for the owner; see the index at the top]**
  **Question:** add macOS end-to-end tests for the strict profile's read refusal
  and a confined network call (CI-only, since no Mac is here), or keep the text
  checks?

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

- **Making `remove_lock_file_if_owned` atomic — declined 2026-10-16.** It reads
  the lock's pid, then deletes by path, so a reap and reclaim between the two
  would delete the new holder's lock. Three reasons not to change it alone.
  There is no portable atomic compare-and-delete for a plain file: renaming the
  lock aside first only moves the window, since restoring a lock taken by
  mistake can clobber or briefly unlock the next holder. The precondition is a
  contender judging a LIVE holder stale (`is_stale_lock`), which needs an
  unprobeable lease and `process_alive` reporting a running pid dead. And the
  reaper's own path (`is_stale_lock`, then delete) carries the same
  read-then-delete window, already accepted as hardening. Closing either for
  real means an OS advisory lock (`flock` / `LockFileEx`) under the lock file —
  a redesign of the store-lock scheme, worth doing only with a reproduced
  failure.

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
- **The hrdr-agent `mock_server` tests stay in-crate** (`src/mock_server.rs`,
  extracted from the lib.rs monolith 2026-08-30). They call private modules
  (`transcript`, `turn_loop`, `compaction`, `oauth`) and
  `#[cfg(test)] pub(crate)` re-exports, so a `tests/` integration module — which
  sees only the public API — is not a pure move; reaching it needs a deliberate
  pub-surface decision. The shared helper `assistant_with_calls` lives there as
  `pub(crate)` and is imported back into `mod tests`.

## Windows support review 2026-09-15

Two read-only review passes over the whole tree for Windows defects, plus a
per-test diff of the CI logs at `b640b41`: Linux ran 2195 tests, Windows 2111,
and 88 ran only on Linux. Shipped the same day: the whole
shell/hook/MCP/LSP/process-tree suite un-gated onto Windows (2174 tests there at
`5a8d8a5`), terminal restore around the editor, PATH resolution of bare program
names (`resolve_program`), the read-mode wrapper's exit code, the `@file` swap
check, a process liveness probe for locks and temp sweeps, `$EDITOR` parsing,
vscode-uri drive URIs, the `!command` live-output cap, the test-support
data-root leak, and the leak guard on all three OSes. Everything below was found
and **not** fixed.

**Open — needs a decision:**

- **CRLF files through `replace` and `write`.** **[Deferred — question for the
  owner; see the index at the top]** `edit` recovers a CRLF file
  (`is_crlf_dominant` in `edit.rs`), `replace` does not: a pattern containing
  `\n` never matches a CRLF file, and a `\n` in the replacement leaves mixed
  endings. `read` presents lines without `\r`, so a `write` of content built
  from a read silently converts a CRLF file to LF and the diff shows every line
  changed — the normal state of a Windows checkout with `core.autocrlf=true`.
  The choice is whether `write`/`replace` preserve an existing file's dominant
  line ending (what `edit` effectively does) or write exactly what they were
  given. Verified from code; not observed in a session.

**Open — found, not fixed:**

- **Multiline paste** — fixed for modern console hosts (`58b2c631`); the
  bracketed-paste pty regressions run ungated on Linux, macOS and Windows CI.
  What is left is under queue item 9 at the top of this file.
- **`cwd_slug` keys session folders on the raw cwd string, through an unstable
  **[Deferred — question for the owner; see the index at the top]** hash — needs
  a decision.** Two problems in `hrdr-agent`'s `paths::cwd_slug`. (1) Windows
  keeps whatever drive and path case the shell was started with (VS Code's
  terminal uses `c:\`), so one project can get two session folders and `/resume`
  cannot see the other's sessions (inferred, not reproduced). (2) The 16-hex
  suffix comes from `std::hash::DefaultHasher`, whose algorithm Rust documents
  as unspecified and free to change between releases: a toolchain bump could
  re-key every session folder, hiding all saved sessions and per-project memory
  at once (verified by reading the code; no release has done it yet). Fixing
  either changes every existing slug, so it needs a migration. **Question for
  the owner:** switch to a specified hash (e.g. SHA-256 via the existing
  `hrdr_llm::sha256_hex`) over a normalized cwd (case-folded drive on Windows),
  and on lookup fall back to the legacy slug, moving the folder when found? Or
  keep today's keys and accept both risks? The fallback costs one extra
  directory probe per session lookup and a rename on first use.
- **Path presentation verification follow-up.** Config-diagnostic regressions
  passed Linux/macOS CI in `37912493951`; the bounded consumer audit found no
  further active gaps, not exhaustive proof. Startup diagnostics recover a typed
  preference from valid TOML or default to `true`; failed reloads retain the
  active style. The reload regression exercises manual dispatch and the mtime
  handler; the watcher-to-loop link is driven by
  `a_config_save_reaches_the_loop_through_the_watcher` (2026-10-20, red without
  the `ConfigChanged` arm or the watcher's ping); a successful reload's notice
  is asserted (`a_successful_config_reload_applies_it_and_says_so`). The failure
  test takes its whole expected toast from `reload_checked`, but also requires
  the literal diagnostic in it (`could not parse config file:` after the styled
  path, or the `sandbox_writable_roots` message), and `reload_message_tests`
  pins the prefix — rewording the diagnostic fails it (mutation-checked
  2026-10-18). Only the TOML parser's own excerpt is unpinned, deliberately:
  that text is the `toml` crate's. Preserve filesystem identities, Unix literal
  backslashes, parser excerpts, protocol URIs, and captured shell output.
- **Completion identity verification gaps:** Unix collision tests in
  `hrdr-app/src/util.rs` and `hrdr-tui/src/app/e2e.rs` passed Linux/macOS CI in
  `37912493951`; local verification was Windows-only, including a failing
  separator-normalization mutation. Unix old-code red/green remains unrun.
  **[Deferred — question for the owner; see the index at the top]** Local runs
  are Windows-only by your call; showing it needs a CI run of the old code (a
  throwaway branch reverting the fix). **Question:** worth one CI run, or accept
  the Windows mutation as the evidence?
  `indexed_completion_accepts_platform_path_identity` asserts indexing and
  acceptance; that `@d/e/` and `@d\e/` each attach their own directory's listing
  is `util::tests::colliding_directory_mentions_list_their_own_directory`
  (2026-10-18, `cfg(unix)`), whose only run is CI's — it was never seen to fail
  on old code. (The backslash-as-separator ranking regression the equal-length
  fixture could not catch is now pinned on every platform by
  `rank_file_matches_splits_basenames_on_slash_only`.)
- **Cursor rendering verification gaps — reported 2026-10-09.** The user saw
  transcript-to-input cursor flicker in both Alacritty and Windows 11 Terminal.
  `cursor_backend::draw_frame` now hides before frame output and reveals only
  after final positioning and flushing; recording-backend tests cover the real
  Ratatui drawing seam, failures, and unwind cleanup. Visual confirmation in
  either terminal remains outstanding. Ordered writes do not make frames atomic
  or guarantee terminal blink timing; restoration after output failure is best
  effort. `Write` forwarding and short writes, autoresize `size()`/clear
  failures, and a failing or panicking restoration on drop are covered since
  2026-10-16 (`cursor_backend/tests.rs`).
- **Parallel config-mutation test failure:** During cursor-adapter verification,
  the ordinary parallel workspace suite failed in
  `config_mutation_path_style_host_forwarding` at
  `host.info_log.last().unwrap()`. The serial suite and nextest passed.
  Investigate shared environment/config state before attributing the failure or
  changing the test; do not treat serial success as proof that parallel
  execution is reliable. Narrowed 2026-10-15, cause still not found: an empty
  `info_log` means `persist_setting` SUCCEEDED (the default host logs only on
  error), so `config.toml` was valid again between the test writing invalid
  bytes and persisting — something rewrote it mid-test. Ruled out by reading:
  every dispatch test that runs a config-writing command holds `CONFIG_LOCK`
  (the two registry tests included); `browser_login_completion_line` writes
  config directly but its only test passes `token_saved: false`; `/doctor` and
  `helpers.rs`'s reload test only read the path; no `spawn_blocking` work writes
  config; leftover async tasks are dropped unpolled with each current-thread
  `#[tokio::test]` runtime; nothing in `hrdr-app` sets env vars, and
  `with_test_env` moves only `XDG_DATA_HOME`. `hrdr-agent` writes `config.toml`
  only through `persist_setting`/`remove_setting`, so agent construction
  (`TestHost::new`) is out too. Also ruled out by reading, 2026-10-16: another
  test binary (the test-support ctor's sandbox root is per pid, and `cargo test`
  runs binaries one at a time); the config root moving mid-test
  (`hjkl_xdg::config_dir` reads `XDG_CONFIG_HOME`, which only the ctor sets —
  `with_test_env` moves `XDG_DATA_HOME`, and no runtime code sets either);
  `persist_setting` resolving a different path (it calls `config_file_path` like
  the test); the login hosts (`RouteTestHost::persist_setting` is a no-op, and
  the direct write in `browser_login_completion_line` needs
  `token_saved: true`). Still unexplained. The test now reports the file's
  contents when the save unexpectedly succeeds, so the next failure shows what
  rewrote it — or, as a `NotFound` read error, that the file was deleted, which
  `read_config_doc` would also let succeed (a missing file reads as an empty
  document). Not reproduced 2026-10-19: 25 back-to-back parallel runs of
  `cargo test -p hrdr-app --lib` on Windows all passed (179 tests each), so it
  likely needs the load of the full workspace suite, like the other flakes
  recorded here. That same day one `cargo test --workspace` run failed 4 of the
  hrdr-app lib tests (175 passed; the binary took 59s against ~5s alone), but
  the output was filtered away before the names were read, so whether this test
  was among them is unknown; the next four workspace runs, logged in full, were
  all green. Next time, run with `--no-fail-fast` into a log file. Four more
  full `--no-fail-fast` workspace runs on 2026-10-20, logged, passed every
  hrdr-app test (one failed only a `watch` test, since fixed).
- **Idle logo verification gaps:** Visual behavior in Windows Terminal/Alacritty
  was not reproduced. Wide/combining glyph clipping remains unmeasured. The
  header-visibility pass's cost was measured 2026-10-20 with a throwaway probe
  (deleted) in `--release` on Windows, 120×40, header on screen: 118.7–120.1 µs
  per frame with the pass, 114.7–118.8 µs with it disabled — inside run-to-run
  noise. Overlay coverage intentionally does not affect animation eligibility;
  only transcript viewport visibility does.

**Coverage gaps (stated as gaps):**

- **Some tests still run only on unix.** Unix by nature: the mode-bit tests
  (`*_owner_only`, `*_0600*`, `ensure_private_dir_sets_0700_on_unix`,
  `atomic_write_preserves_mode_and_replaces_content`), the `O_NOFOLLOW` refusals
  (`open_wire_log_rejects_pre_existing_symlink`,
  `open_wire_log_refuses_final_component_symlink_with_o_nofollow`,
  `owner_only_options_no_follow_refuses_a_symlink`; Windows'
  `FILE_FLAG_OPEN_REPARSE_POINT` half is covered on every platform by
  `owner_only_options_no_follow_never_reaches_a_link_target`) and the Landlock
  backend. **Not unix by nature, still gated:** none — so nothing here is
  actionable; each remaining unix-only test checks a unix mechanism (mode bits,
  `O_NOFOLLOW`, Landlock) that has no Windows counterpart to test. **[Deferred —
  question for the owner; see the index at the top]** Their only runs are CI's,
  since local testing is Windows-only. **Question:** accept CI as their sole
  verification, or allow a local Linux run for changes to those guards? The
  last, `a_jailed_delegation_requires_a_contained_cwd`'s symlink escape, now
  links to the caller's own parent instead of `/` and fails on Windows when
  containment is checked lexically. What remains is unix by nature: the
  `hrdr-llm` `O_NOFOLLOW`/mode-bit tests, `sandbox.rs`'s `/etc` link, and
  `lsp.rs`'s `within_root` case, which re-spells the path by letter case on
  Windows instead. Also ported 2026-10-15:
  `a_dangling_symlink_cannot_escape_the_writable_roots` (the dangling-link
  sandbox escape; fails on Windows when dangling links go unresolved) and the
  symlink half of `a_writable_root_is_writable_all_the_way_down` — which cannot
  fail on Windows: the policy's roots are `\\?\`-prefixed there, so a raw
  unresolved path is refused anyway. Earlier the same day: `tree_symlink_marks`
  (its body was `cfg(unix)`, so it passed on Windows checking nothing),
  `attachment_presentation_directory_suffixes`' link case, and
  `normalize_path_handles_existing_and_nonexistent_symlink_targets` — rewritten
  2026-10-18 to assert the exact path, which found a unix escape: a trailing
  `..` stopped `canonicalize_nearest`'s walk and fell back to the text, so
  `cwd/link/x/../../y` read as `cwd/y` while a write creating `x` lands beside
  the link's target. Fixed there; the unix arm is verified by CI only (no local
  cross toolchain: `cc` needs `x86_64-linux-gnu-gcc`), and on Windows the test
  pins the textual answer the OS itself uses, which no mutation of the unix arm
  can move. The guards they cover do run on Windows.
  `hrdr_test_support::symlink_or_skip` is the portable helper — file or
  directory link by target kind, skipping locally without the privilege and
  failing on CI. Ported 2026-10-15, each shown to fail on Windows when its guard
  is broken: `atomic_write_writes_through_a_symlink`,
  `guard_not_swapped_rejects_a_path_repointed_after_open`,
  `a_secret_line_memo_is_a_per_run_snapshot`, the commands/skills discovery and
  trust symlink tests, `open_wire_log_rejects_symlink_placed_after_rename` (the
  wire log's only symlink guard off unix),
  `confinement_resolves_a_symlinked_workspace_root` (which needed `file_uri` in
  place of a hand-built `file://` string). Expect `\\?\` canonical prefixes and
  file-URI formatting in the rest. `a_bang_command_runs_unsandboxed` stays
  unix-only on purpose (its doc says why).
- **The Windows leak guard sees less than the others.** `dirs::home_dir()`
  ignores `HOME`/`USERPROFILE` there, so a test that writes through a `dirs`
  fallback lands in the runner's real profile, not the sentinel. Its temp-dir
  check does work. Inert today: no workspace crate depends on `dirs` directly
  (only transitively — checked 2026-10-20), and both of hrdr's own `home_dir`
  helpers (`agents_dir`, `sandbox`) read `HOME`/`USERPROFILE`. A `clippy.toml`
  ban was considered and declined: it would name a crate nothing here links, and
  adding `dirs` is already a dependency decision that would surface it.
  **[Deferred — question for the owner; see the index at the top]**
  **Question:** accept, or add the `disallowed-methods` entry now as a tripwire?
- **Windows test runs with a failure leaked their temp dirs — fixed
  2026-10-15.** libtest ends a binary with a failed test through
  `std::process::exit`, which on Windows is `ExitProcess`: no `atexit` handler
  runs, and `dtor` registers through `atexit` there, so `remove_sandbox` and
  `drop_deferred_temp_dirs` were skipped and every failing run left its sandbox
  root plus each `defer_tempdir` dir behind (measured: a passing `hrdr-tui` lib
  run left 0, the same run with one forced failure left 62). That is what the
  earlier "17 `.tmp` dirs, in a run where a test failed" sightings were. Each
  sandbox root now holds an exclusively locked `.alive` file for its process's
  life, and every test binary's ctor removes roots whose lock it can take
  (`sweep_abandoned_sandboxes`; roots without the file go after an hour);
  deferred dirs are created inside the root (`hrdr_test_support::tempdir`).
  Verified on Windows: the failing run now leaves only its root, which the next
  run removes. The `.tmp*` dirs leaked into the system temp dir before the fix —
  17,297 on the owner's machine, all `tempfile`-shaped and none touched in the
  last hour — were deleted on the owner's word 2026-10-16.
- **`hjkl-clipboard` Windows backend — reviewed 2026-10-18 (0.42.2).** Read in
  full, plus one live read-only probe (another process holding the clipboard
  open); no write was run, since it would wipe the owner's clipboard. Sound:
  `CloseClipboard` on every path, `GlobalFree` only when `SetClipboardData`
  fails, FFI types and constants right for x86_64 and aarch64 (by reading).
  Fixed in hrdr the same day: vim `"+p` kept Windows `\r` (`clipboard_text` in
  `hrdr-editor/src/host.rs`). **Upstream** (kryptic-sh/hjkl): fixed there
  2026-10-20 in `99b0d328` — `OpenClipboard` retried, lossy `CF_UNICODETEXT`,
  `dib_to_png` honouring `biCompression`/masks with all-zero alpha read as
  opaque and no `i32::MIN` overflow, `cf_hdrop::parse` tolerating a trailing odd
  byte, and a 256 MiB read cap — on top of `b2e2e257` (allocate before
  `EmptyClipboard`, 2026-10-18). Each was shown red by reverting it; the ignored
  real-clipboard round trip was not run (it would replace the owner's
  clipboard). **hrdr has only the first:** it is on hjkl 0.43.0 (which carries
  `b2e2e257`), and `99b0d328` is unreleased. **[Deferred — question for the
  owner; see the index at the top]** **Question:** cut an hjkl release (a
  publish to crates.io) carrying `99b0d328` and move hrdr to it, or wait for
  hjkl's own next release? Left upstream, by decision or nature: `set_text`
  stops at an interior NUL (`CF_UNICODETEXT` is NUL-terminated, so the format
  cannot carry one); line endings are not converted (hjkl's own backlog holds
  that decision; hrdr strips `\r` in `clipboard_text`); `get_png`/`get_bytes`
  copy the whole `GlobalSize` block, whose slack past the written bytes the PNG
  and CF_HTML parsers ignore; 16-bit and palette DIBs are still refused.

  Open, **in hrdr** [read] **[Deferred — question for the owner; see the index
  at the top]** every clipboard call is a synchronous OS call on the UI thread —
  `App::paste_clipboard`, `/paste`, `HrdrHost::read_clipboard`, and
  `flush_clipboard` after every vim key — and `GetClipboardData` waits on a
  delayed-render source (Excel, Office), so a slow or hung source app stalls the
  TUI; moving them off-thread is the fix. Not done 2026-10-18, for two reasons:
  hjkl's `Host::read_clipboard` is synchronous, so the `"+p` read cannot move at
  all; and a write worker alone opens an ordering trap — `"+y` then `"+p` would
  read the OS clipboard before the worker wrote it and paste the old contents,
  unless the read first waits for the worker to drain. (`Clipboard` is
  `Send + Sync`, and hjkl-clipboard's `MockBackend` records writes.) And
  `clipboard_paste` prefers `Png`, which `WindowsBackend::available` reports
  whenever `CF_DIBV5` is listed — Office puts a bitmap beside its text, so
  Ctrl+] on copied cells likely attaches a picture of them (unverified: no
  Office here). Not verified: delayed-render stall lengths, real
  PrintScreen/Snipping Tool output, aarch64. **Questions:** (a) move clipboard
  calls off the UI thread? Writes could go to a worker, but a vim `"+p` reads
  through hjkl's synchronous `Host::read_clipboard`, so every read would have to
  wait for queued writes to drain — a design change, not a fix. (b) prefer text
  over PNG when the clipboard offers both? That fixes Office cells pasting as a
  picture (unverified — no Office here) but breaks a browser's "copy image",
  which offers text beside the image too.

## Correctness review 2026-08-14

`:review` (low depth) over the whole tree (working tree clean at the time),
split across two sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and
hrdr-llm + hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every
candidate the passes raised was re-traced at its cited lines by the sweep lead;
nothing survived as a defect. **Status: no findings — all items below are
hardening (correct today, fragile). Four shipped 2026-08-14/15: `split_fence`
opening-fence whitespace, the wrap-up History gap, the `read_capped_text`
truncation marker, and the Windows lock pid-ownership guard (its residual, a
Windows liveness probe, shipped 2026-09-15 as `hrdr_tools::process_alive`).**

**Hardening (triaged — both deferred to the owner):**

1. **[Deferred — question for the owner; see the index at the top]**
   **`compaction_tail_start` charges the always-kept newest turn against the
   preserve budget** (`hrdr-agent/src/compaction.rs`). `tokens` accumulates
   newest-first, so once the newest turn alone exceeds `preserve_recent_tokens`
   the walk breaks at the first older turn — no older turns are kept even if
   each is tiny, and the tail can be far smaller than the budget suggests.
   Matches the documented walk; worth revisiting only if a large newest turn is
   seen to starve the tail. **Question:** stop charging the newest turn against
   the budget, or keep the documented walk?
2. **[Deferred — question for the owner; see the index at the top]**
   **Plain-engine trailing backslash traps Enter** (`hrdr-editor/src/plain.rs`):
   any message ending in `\` never submits on plain Enter (it becomes a newline
   and eats the backslash); sending needs a second Enter. Documented escape
   design (Alt/Shift+Enter also newline), so deliberate — noted for a Windows
   path / LaTeX-heavy user. **Question:** keep `\` + Enter as a newline, or send
   a message that ends in `\` on Enter?

**Cleared (suspected, traced, safe — do not re-investigate):** Anthropic
cache-token accounting (`message_start_usage`:
`input_tokens + cache_read + cache_write` is the correct total per the
provider's wire semantics, and the `call_cost` partition + `Accumulator::push`
merge preserve all three fields — the "breakdown, not addition" comment is
confusing but defensible); SSE overflow handling (`sse.rs` line-buffer cap,
`data:` fold cap with char-boundary truncation, discard-after-overflow contract
— all test-pinned); the mid-stream rate-limit 429-only classification asymmetry
(deliberately pinned by the test table in `client.rs`); `edit` over a stale read
(refuses when the anchor only fuzzy-matches — the conservative direction); the
viewport paint-one-screenful math (`first`/`last` via `partition_point` over
`cum`, rows pre-wrapped to `inner_width` — test-pinned); read/write
`covered_through`/`clipped` sig model; `Retry-After` delta-seconds + IMF-fixdate
parsing (clamped, pinned against the RFC's worked example);
`mentions_identifier` (a token cannot match inside
`max_output_tokens`/`max_completion_tokens` — not substrings); `Accumulator`
tool-call index (≥1024 skipped before resize; id-less calls get nonce-qualified
synthetic ids so replays don't collide); `read_capped_text` exact-cap vs
oversized distinction (one extra chunk read decides); proc tree-kill disarm +
unix group-kill `pid > 1` guard; `compaction_tail_start`'s empty/leaderless
return (`compact` guards `before <= 2` upstream, so the >len index is never
reached); `command_arg_offset`'s `+1` (the sigil byte, traced with
`"/verbose   off"`); `extract_agent_mention` byte-offset splicing (token walk
lands on the real mention; `foo@explore` correctly not matched); u32 overflow in
the token estimators (needs >4G estimated tokens ≈ 17 GB of text — a hardening
note at most); `coordinated_oauth_access` single-flight race (waiter registers
under the state lock before `refreshing` is set, so `notify_waiters()` cannot
fire between decision and await; `RefresherGuard` clears on every exit including
cancellation); cross-process session/store lock correctness on Linux/macOS (PID
TIMESTAMP format, mtime fallback, dead-owner probe, re-exec child-process
tests); `compute_wrapped_layout` (wide-glyph, zero-width, whitespace-crossing,
hard-break paths agree with tests); `run()`'s budget-exhausted wrap-up and
mid-stream retry (no text duplication; overflow recovery compacts canonical
history); `last_prompt_tokens` surviving `/clear` (one no-op
`maybe_self_compact`, no wasted model call); `is_quit_command("q")` (deliberate
cross-CLI design, tested); `budget.rs` / `usage.rs` / `turn.rs` arithmetic (no
underflow/overflow path); `session.rs` v1-v2 deserialization, `sanitize_name`,
`session_id_from_path`, `persisted_messages` wire-vs-file serialization
(`#[serde(default)]` covers every re-added field); history-writer coalescing and
the config watcher's ping-under-lock ordering; `parse_imf_fixdate` tolerating
`Feb 31`/`hour 99` (computes a wrong-but-clamped delay — harmless).

**Coverage:** reviewed in full — hrdr-agent (all modules; prompt _text_ skimmed,
`chatgpt_models.rs` skimmed — catalog mapping), hrdr-app, hrdr-editor, hrdr-llm
(`client.rs`, `types.rs`, `retry.rs`, `sse.rs`, `capped_read.rs`, `fs.rs`,
`lib.rs`, `anthropic.rs` 1-1096, `codex.rs` 1-788, `media.rs`, `pdf.rs` 1-1300,
`catalog.rs` 1-590), hrdr-tools (`lib.rs` 1-2800, `tools/shell.rs`, `read.rs`,
`write.rs`, `edit.rs`, `mutation.rs`, `grep.rs` 1-460, `mod.rs` 1-800,
`sandbox.rs` 1-1600, `memory.rs` 1-730, `lsp.rs` 1-1150, `proc.rs`), hrdr-tui
(`ui.rs` draw/viewport/block-render core, `app.rs` EventSender +
prune/clear/apply), hrdr-test-support, `apps/hrdr/src/main.rs`. Skimmed, not
line-by-line: `tools/replace.rs`, `todo.rs`, `tree.rs`, `ls.rs`, `find.rs`,
`verify.rs`, `watch.rs`, `secret_diff.rs`, `gate.rs`, `guardrails.rs`,
`hooks.rs`, `web.rs`, `verification.rs`, `ansi.rs`, `test_nudge.rs`, `mcp/*`,
the remaining TUI (selectors, popups, theme, tui.rs, trust_prompt.rs), the bulk
of test modules, and all integration-test dirs. `hrdr-app/src/login.rs`, skimmed
here, was reviewed in full 2026-10-20 (see the dispatch.rs review).
Security-specific classes were out of scope for this pass (the audit covers
them).

## Security audit 2026-08-14

`:audit` (low depth) over the whole tree (working tree clean at the time), split
across two sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and hrdr-llm +
hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every candidate was
re-traced at its cited lines by the sweep lead. **Status: 1 medium — the medium
is the fix-first item. (All four findings shipped 2026-08-14: retention-sweep
escape, OAuth bind-order, headless terminal escape, and the capture-cap
medium.)**

**Hardening (triaged — one declined, one a standing note):**

- **`gate.rs` parses untrusted CI YAML** (`serde_yaml_ng::from_str` over up to
  16 × 512 KiB workflow files). Investigated 2026-08-14: `serde_yaml_ng`'s
  deserializer has an inherent 128-level recursion limit (`de.rs`), and the
  input is size-capped — the residual surface (alias expansion within those
  bounds) is a one-turn CPU blip on a repo the user chose to open, and the crate
  exposes no knob to tighten. Declined; revisit only with evidence of a real
  attempt.
- **`untrusted_nonce` uses `DefaultHasher`** (`hrdr-tools/src/lib.rs`) — sound
  because the final `contains` check makes a collision a redraw, not a boundary
  failure; keep it that way (never hash the body itself).

**Cleared (suspected, traced, safe — do not re-investigate):** TUI terminal
injection via tool results/model text (ratatui drops control-char graphemes on
every render path — the only unfiltered sinks are the headless ones in finding
4); SSRF in `fetch`/`search` (connect-time `SsrfGuardResolver` filters resolved
addresses with no check→use gap, redirects disabled, bodies capped); SSE /
Accumulator memory caps on the OpenAI path (32 MiB per-event, 64 MiB total,
tool-call index capped at 1024 — finding 1 is the native-backend gap around it);
`pdf.rs` (checked offsets, 16 MiB inflate cap, xref/length verification, no deep
recursion — returns `None` on anything hostile); media image headers (bounded
walks, zero-dimension rejected); sandbox escapes (`resolve_read` /
`resolve_write` canonicalize through `canonicalize_nearest` before
`starts_with`; Landlock inode-anchored and fail-closed; macOS pinned by absolute
path; Windows refuses to run unconfined on failure; jail holds only the 5
read-only tools); shell command construction (`bash -c <model text>` is the
documented purpose; guardrails word-split with quote awareness and nested
`sh -c` re-scan; output line-capped and ANSI-cleaned); `secret_file_reason`
deny-list + shell-output redaction (structural path matching, open-then-
`guard_not_swapped` on reads); API-key handling (key only in headers, stripped
from provider config, 0600 wire log with `O_NOFOLLOW`, keys masked in the login
modal); memory-tool path traversal (`safe_stem` rejects separators / lengths /
device names); LSP/MCP framing (body/header/line caps, wall-clock read bounds,
SSE `endpoint` host-matched against the base, id-space collision fixed);
panics/unwraps on untrusted input (none in library code — the non-test `expect`s
are TLS init, infallible YAML serialization, compile-time regexes, a
provably-set `settle_until`); command injection (no shell string building;
`kill -0` and `touch` are arg-vector); YAML frontmatter caps + fail-closed
parsing (a `read_only`/`tools` restriction is never silently dropped); session
load TOCTOU (one-handle read), atomic writes, `O_EXCL` locks with dead-PID
reaping; OAuth CSPRNG PKCE + state with constant-time comparison, listener bound
to 127.0.0.1 only, HTML-escaped error pages, no secret echoed in errors;
`auth.json` 0600/dir 0700 with atomic locked read-modify-write; kind +
canonical-endpoint double-gated credential resolution; jail mode skips project
AGENTS.md/commands and is re-asserted on `set_cwd`; attachment-store digest +
length verification and 64-hex-only sweep; `Retry-After` parsing (strict,
clamped, cannot stall or panic).

**Coverage:** walked in full — hrdr-agent (agents*dir, commands, skills, trust,
attachment_store, session load/save/locks/sweep/compression, auth_store, auth,
oauth both flows + refresh coordinator, store_lock, transcript_log, registry,
config, resolve, provider_catalog, prompt, turn_loop, delegation, hooks,
history), hrdr-app (config/sessions/login/util), hrdr-editor, hrdr-llm (client,
anthropic, codex, sse, capped_read, retry, types, media, pdf, fs, catalog),
hrdr-tools (lib, sandbox, shell, web, memory, lsp, hooks, proc, guardrails,
gate, mcp/*, the tool implementations), hrdr-tui (render paths, trust*prompt),
`apps/hrdr/src/main.rs`. Skimmed, not line-by-line: `chatgpt_models.rs`
account-catalog fetch, compaction summarization internals, budget.rs, MCP client
wiring, the TUI's `app.rs` input-handler detail and selectors,
`apps/hrdr/tests/*`beyond the headless suite. GAP: `hrdr-agent/src/prompt.rs`
prompt _text_ — read, not audited line-by-line. (`hrdr-app/src/login.rs`, also
listed here, was reviewed in full 2026-10-20.)`apps/hrdr/src/main.rs` beyond the
trust gate was not audited by chunk 1 (covered by chunk 2).

## Performance review 2026-08-14

`:perf` over the whole tree (working tree clean at the time), split across two
sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and hrdr-llm + hrdr-tools +
hrdr-tui + hrdr-test-support + apps/hrdr. Every candidate re-traced at its cited
lines by the sweep lead. **Status: 5 shipped 2026-08-14/15 — the TUI idle
redraw + completion memo, tool-args parse-once, ToolEnd move, grep newline
offsets, and the attachment digest memo; the memory cache serves a hit by `Arc`
(2026-10-16). 1 open (deferred, reason inline).**

1. **One whole-history copy is left per round, and only while a save is still
   writing.** The rest shipped 2026-10-15. A background run no longer snapshots
   per round. A main-session round copies only the messages it added into the
   app's state (`persist_mid_turn` extends it, unless the anchor shows a
   rewrite). A round-journal append needs no state copy at all
   (`enqueue_save_as` → `plan_round`). What is left: when a round arrives while
   the previous write is still in flight, `enqueue_save_as` clones the whole
   `SessionState` into `pending_save`. Deferred: the snapshot is captured at
   enqueue time on purpose, so a `/rename` or `/clear` cannot interleave with
   the write (see `enqueue_save`'s doc comment). Capturing it at promote time
   instead would change that contract, and this copy only happens when writes
   back up. **[Deferred — question for the owner; see the index at the top]**
   **Question:** keep the enqueue-time snapshot, or capture at promote time and
   give `/rename` and `/clear` another way to stay ordered with the write?
   **Already optimized (traced and cleared — do not re-hunt):** session save
   path (created-cache, compact JSON, transcript in a sibling jsonl,
   off-thread); session listing per `/resume` keystroke (mtime-keyed
   `meta_cache`); compaction (single elision build, estimator, ladder
   pre-sizing, `compaction_tail_start` bounded by `DEFAULT_TAIL_TURNS = 2`);
   streaming transcript fold (per-chunk hash fold, coalesced jsonl writes);
   per-turn tool-token estimate hoisted out of the round loop; memory recall
   mtime-cache; memoized cost rates; probed-once context window; editor
   wrap-layout memo per frame; file index lowercased once at build; SSE decode
   (`Cow::Borrowed` for valid UTF-8); catalog `cached_read` (mtime + `Arc`);
   shell ingest (memoized secret check, bounded to tool calls); sandbox
   `is_under_any` (linear over a handful of roots, per tool call).

**Coverage:** traced end-to-end — hrdr-llm (sse, capped_read, client stream,
retry, catalog, media, pdf), hrdr-tools (read, replace, shell, watch, find,
tree, ls, sandbox, lsp, gate, guardrails, hooks, web, mcp, ansi, proc),
hrdr-agent (session save/load, compaction, transcript fold, turn_loop, budget,
delegation snapshot, oauth refresh), hrdr-app (completion, session listing,
status), hrdr-editor, hrdr-tui frame/input path, apps/hrdr (startup only). Not
settled without profiling: the full per-frame transcript render cost in
`hrdr-tui` (the frame loop is out of both chunks' scope; the walk itself is the
already-tracked backlog item, and finding 2 is its frequency amplifier),
hrdr-llm request serialization per round, selector filter functions
(per-keystroke over small lists — tens/hundreds), `status_sections` per-frame
formatting. Dropped as noise: per-chunk `Reasoning`/`Text` event clones
(inherent to event ownership, coalesced downstream); `refresh_oauth_if_needed`'s
per-round `auth.json` read (small file, ChatGPT-identity only); `find_hits`
re-lowering per `/next`/`/prev` (command-frequency, not per-keystroke). GAP:
`hrdr-tui/src/app/e2e.rs` is test-only, skipped.

## Correctness review 2026-08-28

`:review` (low depth) over the whole tree (working tree clean at the time),
split across two read-only sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and
hrdr-llm + hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every
candidate was re-traced at its cited lines by the sweep lead before recording.
**Cleared (suspected, traced, safe — do not re-investigate):** attachment digest
memoization (`00989fd` — `Attachment.bytes` is a private `Arc<[u8]>` with no
mutation path, so the construction-time `sha256` cannot drift; `read_blob`
re-hashes loaded bytes for verification, so the memo cannot weaken the
corruption check); `AttachmentRef::of`/`write_blobs` zip pairing (both derive
from the same `self.state.messages` snapshot with no interleaved mutation);
trust.rs newline guard + test change (`key` preserves a literal `\n` through
canonicalize and fallback arms, so the refusal fires whether or not the
directory exists); PlainEngine cursor math; history-writer coalescing
(capacity-1 `try_send` under the pending lock cannot lose the newest snapshot —
the wakeup sees the stored snapshot); completion `items.len() - 1` underflow
(`compute_completions` only returns `Some` when `items` is non-empty);
`spinner_live()` coverage (every animated source is covered; only the wall-clock
header logo is intentionally frozen); `Agent::agent_names` static profile list
(built once, cannot drift mid-session — the only dynamic part is the
lock-held-empty case, which is finding 2).

**Hardening (correct today, fragile):**

- `remove_lock_file_if_owned` read-then-delete — declined 2026-10-16; see
  Considered and declined.
- The completion cache key omits `suppress_completions` — cleared 2026-10-16,
  not a defect: `active_completions` returns `None` before the cache while
  suppressed, and suppression is not an input to `compute_completions`, so the
  cached value for a given content and `completion_generation` is the same
  either way. Only a completion input changing without a generation bump could
  serve a stale entry, and that holds for every input, not this flag.
- `agent_names` is "best-effort under lock" (`hrdr-app` `helpers.rs`): empty
  while a turn holds the agent, so `@name` completion offers no sub-agents
  mid-turn; the completion memo is invalidated on `TurnMsg::Done`, so the empty
  answer no longer sticks. (Its `agent_cwd` twin fell back to the process cwd;
  the TUI now uses `App::tools_cwd`, which falls back to the app's own `dir_cwd`
  — fixed 2026-10-16.)

**Coverage:** reviewed in depth — the three post-sweep commits and every file
they touch (`session.rs` lock/reserve/save paths, `store_lock.rs`,
`attachment_store.rs`, `media.rs`, `trust.rs`), `hrdr-tui` `completion.rs` /
`commands.rs` `cwd_changed` / `session.rs` save flow / `util.rs`,
`hrdr-app/src/commands/helpers.rs`, `hrdr-test-support`,
`apps/hrdr/src/main.rs`. GAP: not re-reviewed line-by-line — the bulk of
`hrdr-agent` (`lib.rs`, `prompt.rs`, `compaction.rs`, `delegation.rs`,
`turn_loop.rs`, `config.rs`, `commands/dispatch.rs`, `login.rs`), `hrdr-llm`
(`client.rs`/`anthropic.rs`/
`codex.rs`/`sse.rs`/`types.rs`/`catalog.rs`/`pdf.rs`), `hrdr-tools` (`lib.rs`,
`sandbox.rs`, most `tools/*`, `mcp/*`), `hrdr-tui` (`ui.rs`, most of `app.rs`,
`e2e.rs`, selectors, theme), and all integration-test dirs. These were covered
by the 2026-08-14 passes; their cleared items were not re-derived.

## Security audit 2026-08-28

`:audit` (low depth) over the whole tree (working tree clean at the time), split
across two read-only sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and
hrdr-llm + hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every
candidate was re-traced at its cited lines by the sweep lead before recording.
**Cleared (suspected, traced, safe — do not re-investigate):** SSRF in
`fetch`/`search` (connect-time `SsrfGuardResolver`, `is_blocked_ip` covers
loopback/private/link-local/CGNAT/unspecified plus IPv4-mapped v6, redirects
re-checked); `catalog` fetch has no SSRF guard but `HRDR_MODELS_URL` is
operator-set env, not attacker-influenceable; MCP `endpoint` event SSRF (host
matched against the operator-configured base before POST, fail-closed); shell
command injection (the command string is the tool's stated payload; guardrails +
sandbox first); hook injection via `{path}` (`Shell::quote` single-quotes, so an
embedded `'; rm …; '` stays one inert argument); terminal escape injection
(ratatui drops control-char graphemes on every render path; the only unfiltered
sinks were the headless ones, closed 2026-08-14); completion memoization
(correctness-only — the correctness finding #2); attachment digest memo
(`Attachment.bytes` private immutable `Arc<[u8]>`; `read_blob` re-hashes on
load); SSE/JSON overflow caps; `gate.rs` CI YAML recursion (bounded by
`serde_yaml_ng`'s 128-level limit plus byte/file caps); OAuth CSRF/state
(constant-time compare), PKCE (CSPRNG), HTML-escaped error pages, loopback-only
listener, bind-before-browser; token leakage (sanitized errors never echo the
body); credential store (0600 atomic, StoreLock RMW, refuses to clobber
malformed); trust store (canonicalize, exact-match, refuses `\n`/`\r` before
write); session id/path traversal (`sanitize_name` + `id != sanitize_name(&id)`
guard); `attachments_arg` (model `task` attachments) routed through
`resolve_read` + `check_attachments`; `resolve_subagent_cwd` containment; OAuth
single-flight coordinator (cancel-safe `RefresherGuard`).

**Hardening (correct today, fragile — not vulnerabilities):**

- **[Deferred — question for the owner; see the index at the top]**
  **`open_system_handler` Windows `cmd /C start` escaping is partial**
  (`windows_escape_for_cmd_start`, `hrdr-app/src/commands/dispatch.rs`): only
  `&` is caret-escaped, so other cmd metacharacters (`|`, `<`, `>`, `%`, `^`) in
  a path would still be interpreted. Inert today: `open_browser` passes a fixed
  OAuth URL and `open_editor` has no in-tree caller (the TUI overrides the trait
  default). Worth fixing only if a real user-path caller appears — and then it
  is not "caret-escape more characters" (looked at 2026-10-18):
  `std::process::Command` quotes an argument holding a space, inside quotes a
  caret is literal, and no escape stops `%NAME%` expanding in a quoted `cmd /C`
  argument. A real fix builds the command line itself (`raw_arg`) and must keep
  the one live caller working: the percent-encoded OAuth authorize URL, which
  today's `&`-only escaping handles. **Question:** build the `raw_arg` command
  line now, ahead of any user-path caller, or keep waiting for one?

The pre-existing `session.rs` reservation-lock lost-update (since fixed,
`40bdb6f8`: `Reservation::drop` is the only release),
`remove_lock_file_if_owned` TOCTOU, and the `is_blocked_ip` v6-encoding /
write-path-TOCTOU / MCP `Box::leak` / `untrusted_nonce` `DefaultHasher` notes
are already recorded and were re-confirmed, not re-filed.

**Coverage:** walked line-by-line — hrdr-agent `trust.rs`, `auth.rs`,
`auth_store.rs`, `oauth.rs`, `session.rs` lock/reserve/save/load/sweep core,
`attachment_store.rs`, `resolve.rs`, `paths.rs`, `hooks.rs`, `transcript_log.rs`
read side, `delegation.rs` attachments/`cwd`/`resolve_subagent_cwd`,
`skills.rs`/ `commands.rs`/`config.rs` (targeted); hrdr-app `login.rs`,
`dispatch.rs` `open_system_handler`, `host.rs`; hrdr-tools `web.rs`,
`tools/shell.rs`, `gate.rs`, `proc.rs`, `hooks.rs`, `mcp/transport.rs`,
`mcp/client.rs`, `lib.rs` resolve/identity/secret/truncate; hrdr-llm `sse.rs`,
`capped_read.rs`, `catalog.rs`, `fs.rs`, `media.rs`, `lib.rs`; hrdr-tui
`completion.rs` plus the ff2c949 diff; `hrdr-test-support`;
`apps/hrdr/src/main.rs` (trust gate + sandbox-exec wrapper). GAP: not re-audited
line-by-line — `hrdr-editor` (`plain.rs`/`host.rs`/`lib.rs`); the bulk of
`hrdr-agent` (`lib.rs`, `prompt.rs`, `compaction.rs`, `turn_loop.rs`,
`registry.rs`, `transcript.rs`, `transcript_log.rs` write side,
`chatgpt_models.rs`, `provider_catalog.rs`, `models.rs`, `budget.rs`); hrdr-app
`config.rs`, `sessions.rs`, `history.rs`, `util.rs`, `completion.rs`,
`subagents.rs`; hrdr-llm `client.rs`/`anthropic.rs`/
`codex.rs`/`types.rs`/`pdf.rs`; hrdr-tools `sandbox.rs`, `memory.rs`, `lsp.rs`,
`guardrails.rs`, and the remaining tool impls; hrdr-tui `ui.rs`, `app.rs` input
handlers, selectors, theme; `hrdr-tui/src/app/e2e.rs` (test-only). These were
covered by the 2026-08-14 audit; their cleared items were not re-derived.

**Summary:** 1 low, 0 critical/high/medium. Overall risk unchanged and low — the
surfaces that would carry a real bug (SSRF, command/hook injection, file-tool
traversal/TOCTOU, SSE/JSON caps, secret exfiltration, terminal injection) were
re-verified as guarded. Both fixes it named first have shipped: `read_blob` is
gated on `is_blob_name`, and the reservation-lock lost-update (`40bdb6f8`).

## Tidy review 2026-08-28

`:tidy` over the whole tree (working tree clean at the time), split across two
read-only sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and hrdr-llm +
hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every candidate re-read
at its cited lines by the sweep lead; behavior-preserving only. Every finding
shipped; what remains is what was declined.

**Dropped as not-tidy:** `confirm_identity_with`/`Entitlements` re-export
(deliberate DI seam, same shape as `ask_to_trust`); `split_fence`/
`discover_agent_profiles` `pub`→`pub(crate)` (marginal, lib.rs is the intended
surface); `account_digest` thin wrapper (names a domain concept, ~15 call sites
— DRY not indirection); O_EXCL lock-create loop duplication across
`try_reserve`/`acquire_open_lock`/`StoreLock::acquire` (already declined —
different contention/error policies); `digest_hex` (hrdr-agent) vs
`Attachment::sha256` (hrdr-llm) (different crates — now candidate 4);
`active_completions` cloning `Completions` (callers need owned;
`Option<&Completions>` conflicts with the `&mut app` writes in scope);
`editor.content()` allocating per cache hit (perf/API question, out of tidy
scope); `spinner_live` vs `in_flight`/`running` (genuinely different
predicates); platform-gated twin fns (deliberate `#[cfg]` splits, project
idiom); `chrome_line` vs `chrome_fragment` (differ: newline vs flush);
`replace_model_choices` inline re-filter (clearer than `refilter()`'s reset);
retry.rs lowercase copies (one-liners across differing input types);
`sha256: Arc<str>` field (deliberate O(1)-clone choice).

**Coverage:** hrdr-editor read in full; hrdr-app `lib.rs`/`sessions.rs`/
`themes.rs`/`effort.rs`/`palette.rs`/`pane.rs`/`transcript.rs`/`subagents.rs`/
`helpers.rs`; hrdr-agent `auth.rs`/`paths.rs`/`hooks.rs`/`usage.rs`/
`turn_state.rs`/`store_lock.rs`/`attachment_store.rs`/`validate.rs`/`models.rs`;
hrdr-tui `completion.rs`/`selector.rs`/`util.rs`/`session.rs`; hrdr-tools
`verify.rs`; `hrdr-test-support`; `apps/hrdr/src/main.rs`; plus the diffs of the
recent commits. GAP: not line-by-line — the bulk of `hrdr-tools/src/tools/*` and
`mcp/*`, `hrdr-llm` `client.rs`/`anthropic.rs`/`codex.rs`/`sse.rs`/`pdf.rs`/
`fs.rs`, `hrdr-tui` `app.rs`/`ui.rs`/`theme.rs`/`trust_prompt.rs`,
`hrdr-tui/src/app/e2e.rs` (test-only), and the large `hrdr-agent` modules
(`lib.rs` body, `prompt.rs`, `session.rs` beyond the lock sections,
`delegation.rs`, `compaction.rs`, `config.rs`).

## Performance review 2026-08-28

`:perf` over the whole tree (working tree clean at the time), split across two
read-only sub-agents — hrdr-agent + hrdr-app + hrdr-editor; and hrdr-llm +
hrdr-tools + hrdr-tui + hrdr-test-support + apps/hrdr. Every candidate re-traced
at its cited lines by the sweep lead. **Status: findings 1, 3 and 4 shipped
2026-08-28, 5 on 2026-10-15; 2 is disposed below, and the deliberate-tradeoff
revisit remains — see the note after the coverage section.**

**Deliberate tradeoff, not changed (revisit):** attachment base64 is re-encoded
every round (`hrdr-llm/src/media.rs:788-790`, `STANDARD.encode(&self.bytes)` per
attachment per request build). The doc comment at `:639-642` records the choice
— caching the encoding keeps a second resident copy (4/3 the bytes). The
`00989fd` digest memo set a precedent (hash once at construction), but that
string is fixed-size while base64 scales with the payload, so the memory
tradeoff is different. Worth a deliberate re-decision with a real image/PDF
session benchmark; the cheaper middle ground is to serialize requests by
borrowing the cached encoding rather than building a `serde_json::Value` tree
per round (the already-tracked "request serialization per round" gap).
**[Deferred — question for the owner; see the index at the top]** **Question:**
keep re-encoding, cache the encoding (a second resident copy at 4/3 the bytes),
or take the borrow-on-serialize middle ground? Any of them wants a real
image/PDF session benchmark first.

**Coverage:** traced end-to-end — hrdr-tui `ui.rs` (transcript_chunks/flush/
cache, status bar, selectors), `app.rs` on_key, `completion.rs`, `selector.rs`;
hrdr-llm `sse.rs`, `client.rs` request/stream, `anthropic.rs`/`codex.rs` body +
decode, `media.rs`, `catalog.rs`; hrdr-tools `read.rs`/`replace.rs`/`edit.rs`/
`find.rs`/`tree.rs`/`lib.rs`; hrdr-agent `turn_loop`, `budget`, `prompt`,
`transcript`, `delegation`, `session`/`attachment_store`, `trust`, `commands`/
`skills`/`agents_dir`, `pane`; hrdr-app `completion`, `util`, `format`,
`highlight`, `history`, `sessions`, `status`, `transcript`, `helpers`;
hrdr-editor `lib`/`plain`/`host`. Not settled without profiling: the full
per-frame transcript render walk in `hrdr-tui` (already the tracked gap) and the
absolute weight of the base64 re-encode vs its memory tradeoff. GAP: not traced
line-by-line — `hrdr-tools/src/sandbox.rs`, `lsp.rs`, `shell.rs`, `watch.rs`,
`mcp/*`, `hooks.rs`, `guardrails.rs`, `verification.rs`, `proc.rs`, `web.rs`
(mostly per-tool-call frequency); `apps/hrdr/src/main.rs` (startup only);
`hrdr-test-support` (test-only).

**Disposition (2026-08-28):** findings 1, 3 and 4 shipped. Finding 2
(`HighlightCache` deep-copying the block per call) was declined here as "only
saves the inner-`Vec` allocations", but that held for the proposed `Arc` return,
not for lending: since 2026-10-14 `highlight_each` lends the cached lines, so a
streaming frame copies each span string once (into its `Span`) instead of twice.
The per-frame full re-render remains the tracked transcript-render-walk gap.
Finding 5 (SSE line-buffer capacity) was first left unapplied as micro, then
shipped 2026-10-15 since the item still read as open: `flush_line` hands the
buffer back with its capacity and the field logic moved, unchanged, into
`take_line`.

---

## Correctness review 2026-08-30

`:review` over the whole tree (working tree clean at the time), split across
five sub-agents by crate area; every finding re-verified at its cited lines by
the sweep lead before recording (the `replace`→`.git` one empirically, with the
pinned `ignore-0.4.33` walker). Six findings survive; hrdr-agent came out clean
(no defects — five hardening items, top two spot-checked). **Status: items 1-5
shipped 2026-08-30 (`.git`-component skip, reserved-stem refusal, RFC-7231
bounds in `parse_imf_fixdate`, the cancelled-shell id-marker in the `UserShell`
handler, and the `--auto-compact` parse warning — each with a regression test
that failed before the fix). Item 6, five hrdr-agent hardening candidates, is
settled: the `task_cancel`/handle race was already fixed
(`BackgroundPublication`), and on 2026-10-15 the budget wrap-up prompt became a
`MessageOrigin::Nudge`, the registry `events` lock recovers from poison, a batch
drains its live-output forwarders until they close, and `RunGuard` follows a
steered continuation (`begin_next_turn`). Two of those were live defects — live
output cut off after 128 chunks, and a sub-agent cancelled mid-continuation left
`running` forever. Both the guard and the delegation loop's call to it are
pinned: `a_run_cancelled_in_a_steered_continuation_is_released` (mock server)
drives a background run through a steered continuation and cancels it there.**

**Cleared (top of the list):** sandbox escapes (`..` and dangling-symlink
canonicalization — the dangling case is a documented accepted write-TOCTOU, and
reads are open-first guard-second); Landlock failure mode fails the spawn rather
than running half-confined; `atomic_write` symlink/hardlink identity; `edit`
CRLF+fuzzy mapping; MCP id routing and SSE endpoint host-match; SSRF
(`SsrfGuardResolver` at connect time, redirect re-check per hop); OAuth CSRF
hand-rolled constant-time state compare; jail containment (`cap_to_jail_set`
applied last); blob path traversal (gated on `is_blob_name`); session/transcript
bombs (100 MiB caps before parse); prompt-injection via command bodies (bounded
by trust prompt + `model_invocable:false`); all OAuth/PKCE/token-leak
candidates; batching panic/cancel paths backfill `[interrupted]` stubs.

**Coverage:** hrdr-tools full (a few test-module tails skimmed); hrdr-agent
production code walked, with `chatgpt_models.rs`, `provider_catalog.rs`,
`agents_dir.rs`, `commands.rs`, `skills.rs`, `validate.rs`, `trust.rs`,
`auth_store.rs` + test tails fn-mapped only; hrdr-llm full, no gaps; tui/app/
editor full, with
`app/commands/{conversation,model,prompt_commands, compaction,helpers,types}.rs`,
`config.rs`, `login.rs`, `status.rs`, `effort.rs`, `subagents.rs`,
`sessions.rs`, `themes.rs`, `transcript.rs`, tui
`lib.rs`/`theme.rs`/`trust_prompt.rs`, and the e2e suite named as gaps; apps +
test-support full except the Windows-only wrapper (read, not run).

## Security audit 2026-08-30

`:audit` over the whole tree (clean), split across four sub-agents by crate;
every finding re-verified at its cited lines. **1 medium, 5 low; 0 high. Overall
risk low-to-moderate; safe to ship as-is.** **Status: findings 1-6 shipped
2026-08-30 (terminal control-char sanitizer at the render entry points — block
bodies, markdown, editor pane, trust-prompt cwd; MCP parse error truncated to
500 bytes; attachment filenames escaped and backtick-quoted in sub-agent
prompts; the workspace map built from the jailed sub-agent's resolved `cwd`;
`Policy::none()` on both client builders so auth headers never ride a cross-host
307/308; `MAX_PASTE_CHARS` paste cap — each with a regression test that failed
before the fix).**

**Cleared (top of list):** SSRF — connect-time `SsrfGuardResolver`, redirects
re-checked per hop, SearXNG operator-env-only (justified); command injection —
no `Command::new` on untrusted input anywhere, hooks' `{path}` POSIX-quoted;
path traversal in sandbox/memory/diff headers; unbounded allocation / fd
exhaustion — every seam capped (shell lines 8 KiB, fetch/web bodies byte-capped,
MCP 10 MiB/60 s, LSP 16 MiB, `replace` projected-output caps 64 MiB); integer
overflow — saturating arithmetic throughout (the Retry-After case is fixed
2026-08-30 — RFC-7231 bounds + `u64::try_from(days)` in `parse_imf_fixdate`);
TOCTOU on reads open-first guard-second; secret/token leakage —
`secret_file_reason` structural, wire-log records URL+body never headers,
auth-header names filtered from config headers; `ChatError` never interpolates
keys; TLS rustls, no cert bypass; OAuth CSRF/ state/PKCE; blob traversal;
session/transcript 100 MiB caps; untrusted-content envelope nonce forgery —
`wrap_untrusted` verifies the nonce absent from the body.

**Hardening (correct today, fragile):** **[Deferred — question for the owner;
see the index at the top]** `verify` executes gate commands parsed from repo CI
YAML — keep out of `JAIL_ TOOLS` (already is) and consider a `--dry-run`;
session-file `read_only` and identity are still trusted from disk at resume
(same-user files; the `cwd` is re-checked against the trust store since
2026-10-15). **Questions:** add a `verify` dry run that lists the commands
without running them? Re-derive `read_only` on resume rather than trusting the
file?

**Coverage:** hrdr-tools walked in full incl. `mcp/` and secret_diff; the
platform-gated backends (Windows LowIntegrity, macOS Seatbelt) were only read —
never compiled or run on this Linux host (CI round trips needed). hrdr-agent:
delegation, oauth, auth, trust, resolve, attachment_store, transcript_log,
paths, budget, prompt, commands/skills/agents_dir discovery, config env/key
resolution, session deserialization, turn_loop tool-execution; skimmed (not
line-read) models.rs, chatgpt_models.rs, registry.rs, pane.rs, usage.rs,
turn.rs, ~/11k lines of lib.rs. hrdr-llm walked in full (build_body block
assembly read contextually; no remote-fetch-then-attach path exists).
tui/app/editor walked at entry points; hrdr-agent/tools internals read only at
the traced interfaces.

## Tidy review 2026-08-30

`:tidy` over the whole tree (clean), split across three sub-agents; every
candidate verified at its cited lines (top items by the sweep lead; clippy
`--workspace --all-targets --all-features -D warnings` is clean in all crates —
compiler-verified dead code is none anywhere). **Status: eight items shipped
2026-08-30, and the `tool_lines` preview dedup shipped later; none are listed.
Of what remains, one is declined and one is left to the owner.** Findings 1–2 of
wave 1 and 1–4 of wave 3 are safe, behavior-preserving dedups; nothing
behavior-changing proposed.

2. **Test-suite dedups — the rest declined 2026-10-14.** `tui_pty.rs` now reads
   through `common::drain_pty`. Declined: merging
   `chrome_line`/`chrome_fragment` (`apps/hrdr/src/main.rs`) — they differ in
   shape (styled head plus unstyled tail plus newline, versus a streamed
   fragment that flushes), so one function would need a flag per caller; and
   deleting `run_hrdr_inner` (`tests/headless.rs`) — a one-line forwarder whose
   removal only makes two callers spell out `None`.
3. **Flagged, decision left to owner** **[Deferred — question for the owner; see
   the index at the top]** (re-confirmed under Tidy review 2026-09-04) —
   `gate_rank` (`gate.rs`) vs `kind_rank` (`verification.rs`) are the identical
   Format…Test mapping in two private fns, but the maintainer's comment at
   `gate.rs` documents keeping them apart ("three different questions"). Merging
   is behavior-identical today but overturns a stated decision — left alone
   unless the duplication is wanted on principle.

**Dropped (would change behavior):** `is_env_assignment` in guardrails vs
verification accept _different_ syntax; `split_shell_words` deliberately
recovers unterminated quotes where `shell-words` errors; LSP pending-request
correlation vs MCP's — a subsystem refactor, not a tidy.

**Coverage:** hrdr-tools + hrdr-tui read in full (test files scanned at
signature level); hrdr-llm full; hrdr-agent/app sampled (satellite reads +
targeted greps; the 14.7k-line `lib.rs` not exhaustively read); hrdr-editor +
hrdr-test-support + apps full incl. all test binaries.

## Performance review 2026-08-30

`:perf` over the whole tree (clean), split across four sub-agents; findings
re-verified at cited lines (top items by the sweep lead). Two prior backlog
items re-found (see cross-list). **Status 2026-10-15: 1 shipped and removed (the
owner chose to write the sub-agent snapshot once, at run end); 2 shipped and
removed (the round journal, owner-decided); 3 declined; 4 merged; the
running-tool re-render finding shipped (`ChunkRows::AnimatedChrome`) and is
removed.**

3. **Streaming markdown re-rendered in full per frame — declined 2026-10-15,
   measured.** The body key is the `content_hash`, which changes per delta, so a
   streaming reply re-runs `markdown_lines` once per frame (paced by
   `tui::defer_message_frame`). Measured in `--release` on Windows against
   prose + list + fenced-code markdown at width 100: 1.7 KB 63 µs, 6.6 KB 242
   µs, 26.6 KB 0.97 ms, 106 KB 4.3 ms per render — about 37 µs per KB, under the
   frame interval even for a reply far longer than usual. Rendering append-only
   would have to split markdown at block boundaries that are not independent
   (loose lists span blank lines, fences hold them, link definitions resolve
   backwards), so the risk is a reply that renders differently while streaming
   than once finished. Revisit only with a felt-lag report on a long reply.
4. **O(n) transcript walk per frame** — merged into
   `## Frame cost measured 2026-08-13` #1, which holds the 2026-10-15
   measurement.
5. **Declined 2026-10-14 — editor wrap layout recomputed over the whole buffer
   on every keystroke** (`hrdr-editor` `compute_wrapped_layout`). Measured: one
   layout of a buffer at the 256K-character paste cap (`MAX_PASTE_CHARS`) took
   4.0 ms in a release build on the dev machine, and keystrokes arriving
   together are folded into one frame. A few milliseconds per frame in the worst
   case does not justify line-scoped invalidation and the correctness risk it
   brings; revisit if the cap grows.
6. **Declined 2026-10-14 — per-file `canonicalize_nearest` in the grep/replace
   walkers.** The recorded fix (raw-path `secret_file_reason` first) saves
   nothing: the common file is not a secret, so it is still canonicalized. The
   one real saving — canonicalize the root once and join each entry's relative
   path — is only sound if no directory component can be a link, and on Windows
   a junction directory inside the tree can be walked into; a shortcut there
   could let a junction into `~/.ssh` past the deny-list. A few syscalls per
   file are not worth weakening a secret-file check.
7. **LOW — grep still loads each searched file whole** (`grep.rs`). Since
   2026-10-14 a file over `MAX_READ_BYTES` is not loaded: it is named in the
   result ("not searched — … search them with `rg` through `shell`"), so memory
   is bounded and "no matches" never stands alone over an unread file. Left:
   files under the cap are still read whole, and the multiline walk must be (its
   pattern can span lines). Streaming the single-line walk would need a binary
   check up front (`read_to_string` rejects non-UTF-8 today) and a ring buffer
   for `-C`; worth it only if large-but-under-cap files show up in profiles.
8. **Declined 2026-10-14 — paged reads re-scan the file from byte 0 on every
   page** (`read.rs` `WindowScanner`). The pass also counts the file's total
   lines for the page header, so resuming the window still needs a full count
   unless line totals are cached per file with change detection — staleness risk
   traded for a millisecond-scale scan of a file already capped at
   `MAX_READ_BYTES`.
9. **Declined 2026-10-14 — per-chunk event clones on the streamed-delta path**
   (`registry.rs` `record`). Each event is copied into `to_write` and again into
   the event log, because `to_write` also feeds the transcript write that runs
   after the registry lock is released. Every way to drop a copy costs more than
   it saves: writing the transcript from the log entry holds the log's lock
   across file I/O; building the `Record`s under the registry lock lengthens a
   lock every UI frame takes; storing `Arc<AgentEvent>` in the log changes its
   type for every pane that reads it. One small allocation per token does not
   justify any of those.
10. **LOW — no-usage fallback re-estimates the whole history every round.**
    `budget.rs`: `estimate_tokens_in_messages` per round when the server reports
    no usage — O(messages) per round. Prior backlog item (`2026-08-04` #3)
    dropped the running-counter fix as risk > value (~µs per round) — noted, not
    re-proposed.

**Checked, not findings:** prompt build runs once per session and on
`/clear`/compaction — no per-turn rebuild; transcript jsonl is coalesced (512 B
/ 500 ms) with no fsync storm; `Arc::make_mut` free during a turn (no long-lived
second holder); compaction linear and rare; oauth round check is a small
auth-store read per round only for ChatGPT; completion popup memoized per
content+generation; input pane layout shared between sizing and render. The
per-frame `sync_panes` lock traffic and per-frame delegation-string clones are
bounded and disappear if finding 5 lands.

**Deliberate tradeoff noted (revisit with a benchmark):** attachment base64 is
re-encoded every round `hrdr-llm/src/media.rs` — already recorded in the
2026-08-04 perf section above; unchanged.

**Coverage:** hrdr-tools — all tools traced, sandbox process/bind-mount setup
dominated by the spawned command itself (not traced in depth); hrdr-agent —
traced end-to-end from `History` emission and the per-round loop; not settled
without profiling: real fsync/serialize wall-clock vs network round-trip (the
O(n²) _shape_ is certain), and whether hrdr-app's `PaneSet::sync` drains every
agent's events deque per tick (a pane nobody renders would retain every delta);
tui/app/editor — draw path, event loop, ticker, spinner-live, both engines,
highlight cache, todo panel traced; `hjkl_markdown`/syntect internals assumed
linear (not measured); absolute per-frame times need a profile. hrdr-llm —
findings below were appended after the section was written.

### hrdr-llm (appended 2026-08-30)

19. **Declined 2026-10-14 — catalog cross-provider re-scan every request**
    (`hrdr-llm/src/catalog.rs`, `input_modalities_cached` / `max_output_cached`
    with `provider=None`). Each scan is one map lookup per provider (207
    measured), tens of µs, once per request rather than per token, beside a
    network round trip. A memo keyed by catalog mtime, provider and model would
    add invalidation logic for a saving nobody would see. Revisit with a profile
    showing it.
20. **Declined 2026-10-14 — per-event `data.contains("\"error\"")` pre-scan**
    (`hrdr-llm/src/client.rs`). A substring scan before the O(len) parse is far
    cheaper than the parse itself and documented in place as deliberate;
    removing it means giving the shared `ChatChunk` an `error` field for a gain
    nobody has measured. Revisit only with a profile showing it.

**Checked, not problems (hrdr-llm):** full-history re-serialization per request
is inherent (each backend needs the whole history; OpenAI fast path already
skips the `Value` tree when no grafts apply; wire log off by default); per-event
JSON parse is the wire itself; accumulator `String` growth is amortized
geometric; tool-slot `resize_with` amortized and capped; `resolved_model` lock
never held across `.await`; pdf/image parse runs once at attach then cached as
`Cost`; `pdf_page_count`'s `windows().position()` is not O(n²) (cursor only
moves forward); catalog `cached_read` cold read happens once per mtime. Not
settled without profiling: how much of per-token CPU the JSON parse dominates;
true per-request catalog-scan µs; per-attachment base64 wall time. The
deliberate per-request base64 tradeoff is recorded above.

## Correctness review 2026-09-04

`:review` over the whole tree (clean at the time; scope = entire codebase per
the procedure), run single-agent. Focus on the code landed since the 2026-08-30
sweep — the shared per-chunk SSE drain (`hrdr-llm/src/sse.rs`), the `goal` and
`cron` tools, the hrdr-agent lib.rs split (events.rs / agent_impl.rs /
mock_server.rs), the TUI paste cap, the gate/sandbox/whitespace dedup, and the
`x-opencode-session` header work — plus a correctness walk of the
highest-density areas (stream loops, session serialization, tool batch
execution, background task lifecycle, shell/watch delivery).

**Original finding: 1 new (Low, race) — fixed and closed 2026-09-04 (see
CHANGELOG `## [Unreleased]`). A re-confirmed `task_cancel` publication item
remained open at that review and was fixed 2026-09-09. Rest cleared.**

**Cleared (suspected, traced, safe):**

- **SSE drain extraction is behavior-preserving.** Diffed each backend's
  pre-refactor loop against `next_sse_events`: error texts byte-identical (the
  `\`-continued string folds to the same single space), overflow and EOF
  handling match, and `finish()`'s post-flush overflow recheck is present in all
  three callers' semantics.
- **gate/sandbox/whitespace dedup.** `is_whole` is literally
  `classify(...) == Scope::Whole`; `collapse_whitespace` is the same
  `split_whitespace().join(" ")` as all four old copies; the `join_paths`
  generic iterates `&&Path`/`&PathBuf` identically under `AsRef<Path>`.
- **`x-opencode-session`.** Host-gated (`is_opencode`, host == opencode.ai
  exactly, mirroring `is_openrouter`); never duplicates an operator-set header
  (`extra_headers_contains`); re-asserted on `/model` switch, re-minted on
  `clear()`, TUI pushes the durable id at the single funnel (`session.rs`
  `refresh_subagent_dir` — first reserve, resume, first save), and the first
  reserve runs at turn start while the agent lock is free, so the durable id
  normally lands before the first request.
- **Paste cap.** `take(MAX_PASTE_CHARS)` in `on_paste` and the toast's counted
  `pasted` agree; both composer and login field capped.
- **Accumulator usage merge.** `max` + don't-clobber-Some preserves Anthropic's
  two-phase usage; `cache_creation_input_tokens` survives `message_delta`
  (tested both orders).
- **`system_cache_split`.** Both consumers (`mark_system_cache` in
  `types.rs:499`, `split_system_for_cache` in `anthropic.rs:430`) guard
  `is_char_boundary` and fall back to the single-block shape; the offset is a
  byte sum of sections (`prefix_len_before`, `prompt.rs:331`), safe to compute.
- **cron vs `/clear` and resume.** `/clear` never wipes `ctx.crons` (only the
  TUI's todo list is cleared in `clear_all`), so there is no id-reuse-with-a-
  still-armed-scheduler collision and no silently-unarmed recreate; schedulers
  surviving `/clear` is deliberate (crons persist to the session file and are
  re-armed on resume; `cron_armed` + idempotent `arm_crons` make resume
  double-arm impossible).
- **cron scheduling arithmetic.** `next_fire` scans minute-by-minute from
  from+1, `wait` computed from the same `now`; unreachable-schedule refusal at
  create time; `id = max + 1` monotonic.
- **Watch lifecycle.** Entry pushed before the poller spawns, so `task_cancel`
  can always address a fresh watch; poller self-terminates on cancel/absence;
  `finish` never clears `cancelled`, so a cancelled-then-finished watch cannot
  deliver.
- **`expect("array")` in the models tool** (`hrdr-agent/src/lib.rs:392,402,417`)
  is structurally safe — `value` is built with `"warnings": <vec>`.
- **`read_line_capped`** indexing safe (`take >= 1` whenever the buffer is
  non-empty); shell overflow spool seeding + per-line append consistent
  (regression test present); display re-trim to the real budget present.
- **`ToolContext::resolve/write/read` gate + signature bookkeeping** — no new
  gaps found in the read-before-edit or stale-modifier paths.

**Hardening (correct today, fragile):**

- **A cron fire is late after a clock jump or a suspend** **[Deferred — question
  for the owner; see the index at the top]** (`run_scheduler`, `cron.rs`). Not
  DST, as this entry first said (corrected 2026-10-20, by reading): `next_fire`
  resolves the target day's local time with that day's own offset
  (`to_local_datetime`), so `next - Local::now()` is the true interval across a
  transition, and the sleep is monotonic. What does shift a fire is the wall
  clock moving during the sleep (a manual change, an NTP step), or a suspend
  where the monotonic clock does not count suspended time: the one pending fire
  is delivered late, never dropped, and the loop recomputes from `Local::now()`
  after it. Sleeping in bounded chunks and re-checking the wall clock would cap
  the lateness; not done, since nothing here can move the clock in a test to
  show it works. **Question:** ship the chunked sleep without a test that can
  show it, or leave the late fire?

**Coverage:** full codebase scope (working tree clean at review start). Read in
full: `hrdr-llm` sse.rs + the three backends' stream loops and map_event,
types.rs (Accumulator, usage merge, cache breakpoints), retry.rs classifiers;
`hrdr-tools` shell.rs (run_streamed_command, caps, spool), watch.rs, cron.rs,
goal.rs, gate.rs, sandbox.rs join paths, agents_dir.rs; `hrdr-agent`
turn_loop.rs (run loop, nudge/goal backstops, tool batch + cancellation guards),
turn_state.rs, session.rs serialization + persisted_messages, delegation.rs
(background spawn/reap, task_cancel), prompt.rs SystemPrompt, usage.rs;
`hrdr-tui` tui.rs run loop, app.rs paste cap + idle wake + with_agent,
app/session.rs reserve/adopt/autosave, app/commands.rs clear_all, app/e2e.rs
cited tests. NOT line-read this pass (covered by the 2026-08-30 sweep;
skimmed/fn-mapped only): hrdr-llm pdf.rs, media.rs, fs.rs, capped_read.rs,
catalog.rs internals and test tails; hrdr-agent compaction.rs, oauth/, trust.rs,
auth_store.rs internals, chatgpt_models.rs, provider_catalog.rs; hrdr-tools
lsp.rs, mcp/, memory.rs (tails), guardrails.rs, verification.rs internals;
hrdr-app dispatch/engine internals; hrdr-editor; apps/hrdr (Windows-only wrapper
read, not run); the bulk of the e2e/mock_server suites. Those areas rode the
previous sweep's verification and this pass's broader read; new findings there
remain possible. Platform-gated backends (Windows LowIntegrity, macOS Seatbelt)
were read only — never compiled or run on this Linux host.

## Security audit 2026-09-04

`:audit` over the whole tree (working tree clean at the start), run
single-agent. Scope = entire codebase; focus on what landed since the 2026-08-30
sweep — the `goal` and `cron` tools (persistence, scheduler concurrency,
delivery), the shared per-chunk SSE drain, the `x-opencode-session` /
OpenCode-gateway flow, the hrdr-agent split (events.rs / agent_impl.rs), the TUI
paste cap, the gate/sandbox/whitespace dedup — plus the standing high-risk paths
(shell/sandbox/lsp/mcp, credential handling, session deserialization). Every
candidate was re-traced at its cited lines against the current code. **Status: 2
new findings, both fixed and closed 2026-09-04 (stream-error truncation and the
`get_json` probe cap — see CHANGELOG `## [Unreleased]`); the `cron cancel` race
from the 09-04 review was fixed and closed the same day; the rest cleared.** No
code changed by the audit itself.

**Cleared (suspected, traced, safe — do not re-investigate):**

- **cron scheduler concurrency / deadlock.** `deliver` and `cancel` share one
  lock order — `crons` first, then `background_tasks` (nested) — so the
  existence check and the push are atomic with respect to a cancel and there is
  no lock-order inversion. `arm_cron` holds `cron_armed` alone.
- **Double-arm across create/resume (`/clear`-then-recreate).** `cron_armed` is
  inserted before the spawn; `arm_crons` is idempotent; a second arm is a no-op,
  so two schedulers cannot race to deliver one fire.
- **cron schedule arithmetic.** `parse_field` bounds-checks every range
  (`start`/`end` inside `lo..=hi`, `start <= end`, `step != 0`), `step_by`
  cannot overflow, the Feb-29 case is bounded by `NEXT_FIRE_HORIZON_DAYS`, and
  the unreachable-schedule refusal happens at create time. The one recorded
  mistiming (a late fire after a clock jump) has no security content.
- **`cron`/`goal` in jail.** `JAIL_TOOLS` is the fixed 5-tool set
  (`lib.rs:1712`) — read/grep/find/ls/tree — so neither tool reaches a jailed
  agent.
- **`x-opencode-session` / OpenCode gateway.** The id is CSPRNG-minted
  (`new_prompt_cache_key`, `lib.rs:837-848`, 16 random bytes); the header is
  host-gated (`is_opencode`, exact host or `.opencode.ai` suffix,
  `client.rs:657-660`), never duplicates an operator-set header
  (`client.rs:1239-1244`), rides `auth()` on every request including the
  `/models` GETs, and is re-asserted/re-minted on the paths the review traced.
  The 08-30 cross-host redirect fix (`Policy::none`, `client.rs:803` and `:955`,
  and the same client object is what both native backends send on) is present,
  so the new header — and the old `x-api-key`/Bearer — cannot ride a redirect.
- **SSE drain extraction.** Caps and overflow semantics preserved (32 MiB
  line/event, overflow → error never a truncated event, char-boundary-safe
  cuts), `[DONE]` handled before `finish()`, `finish()` re-checks overflow;
  behavior-preserving per the 09-04 review diff.
- **codex/anthropic map_event paths.** No indexing into untrusted arrays
  (`tool_slot.get`, `.copied()`, `Option`-chained); `assign_slot` and
  `capture_reasoning_item` are capped by `MAX_CAPTURED_ENTRIES` /
  `MAX_ACCUMULATED_BYTES`; unknown event types ignored; error objects classified
  by `code`/`type` first, message text only as a fallback.
- **Background-delivery gating.** `drain_background` delivers only
  `done && !delivered && !cancelled`; a cancelled task's result is discarded,
  and the delivered result rides the arrival-reminder framing ("a report to
  read", not an instruction).
- **Agent split commits (78d329e, f2e2f7e, ea34689).** Move-only (four methods
  widened private → `pub(crate)`); nothing security-relevant changed in the
  move.
- **Paste cap and gate/sandbox/whitespace dedup.** `take(MAX_PASTE_CHARS)` (also
  bounds memory from a poisoned clipboard); `join_paths`/`is_whole`/
  `collapse_whitespace` verified byte-equivalent to the originals.

**Coverage:** walked in full this pass — `hrdr-tools` cron.rs, goal.rs,
ToolContext + registration + `JAIL_TOOLS`, lib.rs secret/identity/resolve
surface around the tools; `hrdr-agent` `turn_loop.rs` (run loop, nudge/goal
backstops, connect_stream/connect_and_drain, retry + overflow recovery,
drain_background), turn_state.rs, session.rs (crons/goals serialize/load/adopt,
persisted_messages), agent_impl.rs session-id mint + adopt, delegation.rs cancel
window (re-read for re-confirmation), events.rs; `hrdr-llm` sse.rs, client.rs
(auth/redirect/session header/get_json/SSE loop), anthropic.rs (request build +
stream loop + error paths), codex.rs (map_event, slot/reasoning caps), retry.rs
classification, types.rs; split-commit diffs (78d329e/f2e2f7e) read as diffs.
NOT re-audited line-by-line (no new code since 08-30; covered by the previous
audits, whose cleared items were not re-derived): lsp.rs, `mcp/`, memory.rs,
guardrails.rs, verification.rs, web.rs, shell.rs internals, sandbox.rs policy
build, compaction.rs, `oauth/`, auth_store.rs, attachment_store.rs, trust.rs,
history/transcript write side, `hrdr-tui` ui.rs/app.rs render + input detail +
selectors, `hrdr-editor`, `hrdr-app` dispatch internals, `apps/hrdr/src/main.rs`
beyond the probe, and the bulk of the mock_server/e2e test suites.
Platform-gated backends (Windows LowIntegrity, macOS Seatbelt) were read only —
never compiled or run on this Linux host. GAP: those areas ride prior-sweep
verification; new findings there remain possible.

**Summary:** 2 low, 0 medium/high/critical, all fixed and closed 2026-09-04
(stream-error truncation, `get_json` probe cap — see CHANGELOG
`## [Unreleased]`), plus the `cron cancel` race recorded by the 09-04 review,
also fixed and closed the same day. The re-confirmed `task_cancel` publication
item remained open at this audit and was fixed 2026-09-09. Overall risk
unchanged and low — the surfaces that would carry a real bug
(redirect/auth-header leak, terminal injection, sandbox escape, SSRF, secret
exfiltration, session-file traversal) were re-verified as guarded, and the new
cron/goal/session-id machinery is sound.

## Tidy review 2026-09-04

`:tidy` over the whole tree (clean), one pass. Prior-sweep context: the
2026-08-30 report's items re-checked in the current code first; its shipped
items verified gone (SSE drain single `sse::next_sse_events`; whitespace dedup
single `collapse_whitespace`; `Gate::matched` → `is_whole`; `join_paths` one
generic; cwd view-tail shared; `chrome_line`/`chrome_fragment` and
`run_hrdr_inner` no longer in the tree; `tui_pty` `Session::spawn` now calls
`common::drain_pty`). New-code focus was what landed since: the goal/cron tools,
the agent split (events.rs/agent_impl.rs/mock_server.rs), the SSE-drain
refactor, the gate/sandbox/whitespace dedup, the TUI paste cap, and the 09-04
OpenCode session-id fix. clippy
`--workspace --all-targets --all-features -D warnings` is clean, so
compiler-visible dead code is none; findings are duplication/indirection only.
**Three new items; of the 08-30 items re-confirmed below, only the one held by
stated decision remains** (the `tool_lines` preview duplication was routed
through `preview_head`/`preview_tail` 2026-10-15). (All three new items — the
double paste warning, the goal args wrapper, and the shared id mint — fixed and
closed 2026-09-04.)

**Re-confirmed open from 2026-08-30:**

- **08-30 #5, by decision** **[Deferred — question for the owner; see the index
  at the top]** — `gate_rank` (gate.rs:75-83) vs `kind_rank`
  (verification.rs:346-354): identical Format…Test mapping, still deliberately
  held apart by the comment at gate.rs:70-73 ("three different questions that
  only look alike"). Re-confirmed open; merged only if the duplication is wanted
  on principle.

**Dropped (not proposed):** merging `render_goals` (goal.rs:139-153) with
`render_todos` (todo.rs:253-274) — same `#N mark content` row shape but
different item types, status sets, marks and an evidence echo; a mark-closure
helper for two callers is over-abstraction, and the goal.rs doc explicitly keeps
the renderer its own. The per-backend SSE event loops (client.rs:1536-1611,
anthropic.rs:672-692, codex.rs:317-330) share a skeleton but diverge per dialect
(`[DONE]`/mid-stream error objects are OpenAI-only; `map_event` shapes differ);
the drain is already the shared `sse::next_sse_events`. `BackgroundTask`
construction (watch.rs:199, cron.rs:440) differs in every field; a 10-arg
factory is not a win. The todo/goal turn-end nudge blocks (turn_loop.rs:668-708
vs 710-740) are documented sister backstops with divergent messages and the todo
watch. The session-id threading that mirrors `prompt_cache_key`
(agent_impl.rs:427-428, 698-699, 987-989, 1049-1054) is explicitly documented
parallel state — same class as 08-30 #5; left alone.

**Coverage:** hrdr-tools goal.rs, cron.rs, todo.rs, gate.rs, verification.rs,
lib.rs (`tool_args`, `collapse_whitespace`, `next_id` area), sandbox.rs
(join/whitespace region) in full; hrdr-tui app.rs (paste paths both entry
points, cwd view tail, session-id reserve), ui.rs (tool_lines, cached_body/
cached_block), commands.rs sampled; hrdr-agent events.rs in full, agent_impl.rs
(accessors, session-id, adopt_resolved), turn_loop.rs nudge region, lib.rs
re-exports + goal/cron test modules, mock_server.rs at signature level;
hrdr-editor lib.rs wrap region in full; hrdr-llm sse.rs in full + all three
chat_stream loops; hrdr-app + apps/hrdr tests (env tables in all three PTY
files, common/mod.rs) and main.rs (grep for chrome/run_hrdr_inner). Remainder
(sandbox policy internals, memory/lsp/mcp, ui.rs blocks beyond the cached
regions, the mock_server/e2e suites) carries prior-sweep coverage.

## Performance review 2026-09-04

`:perf` over the whole tree (clean at start). Focus per the sweep brief: the
code landed since 2026-08-30 — the `goal`/`cron` tools (cron's per-cron tokio
scheduler), the shared per-chunk SSE drain, the hrdr-agent split
(events.rs/agent_impl.rs), the TUI paste cap, the gate/sandbox/whitespace dedup
— plus a status pass on every open 08-30 perf finding, re-verified at its
current line (the agent split and SSE refactor moved/handled several).
**Findings: the new LOW `cron` fast-forward item was fixed and closed
2026-09-04. Recorded 08-30 findings below remain open where stated; each is one
line + current file:line instead of a full writeup.** No code changed by the
perf pass itself.

Ranked by impact, biggest first:

3. **Unchanged frames still materialize every assembled block** — merged into
   `## Frame cost measured 2026-08-13` #1.
4. **08-30 #12/#13/#16/#17 (LOW), settled 2026-10-14.** #12 (`replace`'s literal
   path counts matches before replacing) is declined: the count is what bounds
   the output size before the replacement allocates. #13 shipped (`memory.rs`
   `Lowered`). #16 is the no-usage re-estimate, now 08-30 #10, declined as
   risk > value in 2026-08-04 #3. **[Deferred — question for the owner; see the
   index at the top]** **Question:** accept the decline, or build the running
   token counter (about eleven message-mutation sites to keep in step, to save
   microseconds per round)? #17 is closed (checked 2026-10-20): dispatch no
   longer clones the args to render a label; the one clone left, in
   `recorded_args`, builds the record on purpose.

**Checked, not findings (new code since 08-30):**

- SSE-drain refactor is cost-parity with the old loops: per-chunk `drain()`
  Vec + per-event JSON parse are the wire itself; `trim()` and `from_utf8_lossy`
  borrow on valid UTF-8. Verified all three loops (`client.rs:1533-1615`,
  `anthropic.rs:669-694`, `codex.rs:314-332`).
- **The 08-30 "not settled" event-retention question resolves as bounded.**
  `PaneSet::sync` (`pane.rs:313-423`) drains AND compacts every live agent's
  event deque per tick (`events_since` → replay → `compact`,
  `registry.rs:730-749`), and a finished, delivered, unpinned sub-agent's pane +
  deque are reaped by `prune` (`registry.rs:899-901`) — a pane nobody renders
  does not retain deltas. Per-tick sync cost is O(agents + events-since-tick).
- cron scheduler: one long-sleeping tokio task per cron, wait computed once per
  fire; per-fire work is two small `crons.iter().find` scans + one
  `background_tasks` push. `arm_crons` on resume is O(crons), once. Scheduler
  never holds a lock across `sleep`/`await`.
- goal tool, paste cap (`take(MAX_PASTE_CHARS)` once per paste, 256 KiB bound,
  `app.rs:35,1767-1770`), `x-opencode-session` (one header String per request),
  gate/sandbox/whitespace dedup (sub-millisecond, per-invocation-size inputs):
  all bounded, per-invocation only.

**Coverage:** read in full this pass — cron.rs (scheduler + `next_fire` +
deliver), goal.rs, sse.rs + all three backend stream loops, types.rs
`Accumulator::push`, registry.rs (`record`/`since`/`compact`/`prune`) + pane.rs
`sync`, delegation.rs (snapshot save + task_cancel), session.rs save path,
transcript_log.rs write side, events.rs; re-verified every re-confirmed item
above at its cited line against the current tree. NOT re-traced (untouched since
08-30; ride prior coverage): hrdr-editor internals, hrdr-app beyond the
highlight cache, memory.rs/lsp/mcp, guardrails/verification, e2e/mock_server
suites, apps/hrdr. Still needs profiling, as at 08-30: absolute per-frame draw
times, per-token clone/lock cost vs network round-trip, catalog-scan µs,
snapshot/session fsync wall time.
