# Handoff: Ad-hoc Cross-Provider Delegation

**For a fresh session.** Everything below is a claim — re-verify against source
before it gates an action (this repo's evidence rule). Date: 2026-07-12.

## TL;DR

Implement a `provider` argument on the `task` tool so the main agent can
delegate to any configured + authenticated provider/model at delegation time
(e.g. ChatGPT main agent → `openrouter` / `deepseek/deepseek-chat` sub) without
a predefined `[[subagent]]` profile. **Spec and plan are written, reviewed to
convergence, and committed. No production code is written yet** — the next
session executes the plan.

## Where things are

- **Repo:** `/home/shinobu/Projects/hrdr` (Rust workspace).
- **Branch:** `feat/cross-provider-delegation`, cut from
  `fix-chatgpt-oauth-model-discovery-v2` (PR #7). Currently 6 commits ahead of
  #7 — **all docs**, no code:
  ```
  e1e76d3 align spec auth-gate error text
  bc429f8 fold Fable review findings
  4afa9bd fold plan-review findings
  184c1d1 implementation plan
  3410444 scope auth gate to ad-hoc path
  4ceb5eb spec
  ```
- **Authoritative docs (read these first, in order):**
  1. Spec —
     `docs/superpowers/specs/2026-07-12-cross-provider-delegation-design.md`
  2. Plan — `docs/superpowers/plans/2026-07-12-cross-provider-delegation.md`
- The plan is execution-ready: 3 tasks, TDD, exact code blocks + line anchors,
  all verified against source in two review rounds (2 parallel reviewers + a
  Fable adversarial pass). Every finding was folded.

## Branch topology (why this branch, not another)

- `main` (local + `origin/main`) = clean mirror of `upstream/main`.
- `personal` = the daily-driver integration branch = #7 + #8 (introspection).
- `fix-chatgpt-oauth-model-discovery-v2` = **#7** (ChatGPT OAuth + model
  discovery), open upstream PR, head lives on the fork.
- `feat/agent-model-introspection` = **#8** (introspection), draft upstream PR,
  stacked on #7.
- **This feature depends on #7, NOT on #8.** It needs #7's OAuth +
  `provider_auth_state`; it is independent of #8's introspection. It is a
  sibling of #8, not a child. Do not base it on `personal` or `main`.

## What to do (execution)

1. Confirm you are on `feat/cross-provider-delegation` with a clean tree.
2. Use `superpowers:subagent-driven-development` (fresh subagent per task,
   review between) **or** `superpowers:executing-plans` (inline with
   checkpoints). Follow the plan task-by-task; it is TDD (write failing test →
   run → implement → run → commit).
3. **Rust gate before every code commit** (this repo's CI-gating checks):
   ```
   cargo fmt
   cargo clippy --all-targets --locked -- -D warnings
   cargo test --locked
   ```
   Verify by bare exit codes, not piped output.
4. The three tasks:
   - **Task 1** — extract `repoint_to_provider` helper (endpoint/auth/model +
     the provider-identity fix: set `cfg.provider`), refactor
     `config_for_agent_profile` to use it.
   - **Task 2** — add the `provider` schema property + `apply_task_overrides`
     (auth gate, ad-hoc-only) + wire into `SubagentTool::execute`; update the
     tool's top-level description.
   - **Task 3** — README + CHANGELOG; then the **live e2e** (see below).

## Live end-to-end verification (Task 3, Steps 5-6)

Uses real credentials — this is the proof the feature works, per the `verify`
skill.

- **Credentials present:** `~/.config/hrdr/auth.toml` holds a real `openrouter`
  key (saved via `/login`); `~/.config/hrdr/oauth.json` holds a live ChatGPT
  OAuth session (valid ~10 days from 2026-07-12).
- **⚠ `config.toml` is mid-experiment:** currently `provider = "openrouter"`,
  `model = "gpt-5.6-sol"` — an invalid combo (that model isn't an openrouter
  slug). For the e2e (ChatGPT main → openrouter sub) either set
  `provider = "chatgpt"`, `model = "gpt-5.5"` in `~/.config/hrdr/config.toml`,
  or pass `--provider chatgpt` on the CLI. Restore/leave it as the user prefers
  afterward.
- Find a valid openrouter slug with the models CLI (fixed in #7):
  `./target/debug/hrdr --provider openrouter models`.
- Force `background: false` in the delegation prompt so the one-shot headless
  run blocks until the sub replies (task defaults to background/detached).
- Expected: the sub's reply is an openrouter model id (not a `gpt-5.*`), proving
  the request hit openrouter. Negative check: delegating to a built-in whose key
  is unset is rejected _before_ spawn with `provider '<x>' is not configured …`.

## Gotchas

- **No AI attribution** anywhere (commits, PRs, comments) — house rule.
- **Conventional Commits.** Run `prettier --write` on any changed `.md` (a
  post-edit hook may also reformat — re-Read a file before a second Edit).
- Plan line anchors (`:405-434`, `:640-646`, `:4614`, …) are against the #7-base
  `crates/hrdr-agent/src/lib.rs`. Task 1 shifts later line numbers, but the plan
  edits are keyed on **quoted search blocks**, not raw line numbers, so they
  still apply; re-locate by the quoted text if an anchor drifts.
- The `task` method on `SubagentTool` is `execute` (not `call`).
- The auth gate is **ad-hoc-path only** — do NOT fold it into the shared
  `repoint_to_provider` helper; that would change shipped #7 profile behaviour
  and break `subagent_profile_repoints_to_a_different_provider`.

## After implementation (do NOT push without the user's OK)

- Merge the finished branch into local `personal` for daily use (rebuild
  `personal` per its maintenance recipe if needed).
- PR target is a decision for the user: stack onto #7's branch, or target `main`
  and show the whole stack (as #8 does). Confirm before opening/pushing.
- Push gating: green `cargo fmt` + `clippy` + full `cargo test --locked` by bare
  exit codes first.
