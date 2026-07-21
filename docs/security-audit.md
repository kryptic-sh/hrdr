# Security and Correctness Audit

- **First pass:** 2026-07-18 (deep review, 24 findings)
- **Second pass:** 2026-07-18, re-verified 2026-07-21 (19 → 5 findings)
- **Latest update:** 2026-07-22 (against HEAD `a76d022`, version `0.7.0`)
- **Depth:** high
- **Method:** static, read-only source review with caller and failure-scenario
  tracing; every confirmed finding fixed under delegated review with a
  regression test, then merged and validated in CI.

## Status at a glance

Two independent audit passes ran against this codebase. Every **confirmed**
finding is resolved except two that are deliberately deferred (each needs an
external confirmation or a decision, not more engineering):

| Item                                     | Status                                     |
| ---------------------------------------- | ------------------------------------------ |
| Pass 1 — 24 confirmed findings           | **24 fixed**                               |
| Pass 2 — 5 real findings (of 19 drafted) | **4 fixed, 1 deferred** (OpenRouter state) |
| Session lost-update (pass 1 #18)         | **Fixed** — open-lock + fork               |

**Deferred (not engineering-blocked):**

- **OpenRouter OAuth `state`** (pass 2, finding 3) — strict `state` validation
  would break login if OpenRouter does not echo `state` back, and the code
  comment says it does not. opencode (checked as a reference) uses a bearer API
  key for OpenRouter with no OAuth flow, so it could not confirm the provider's
  behavior. Revisit once OpenRouter's `state`-echo is confirmed from their docs
  or a live callback capture; `generate_state()` is already in the tree.
- **openai/chatgpt provider merge** — tracked as GitHub issue **#21** (not a
  security finding; a UX/architecture change deferred by request). Endpoint,
  catalog, and kind must switch by credential type; design captured in the
  issue.

---

## Pass 1 — deep audit (24 findings, all confirmed, all fixed)

A high-depth pass that surfaced 24 issues, each independently verified against
the code with a concrete failure scenario before being accepted, then fixed with
a regression test. The three High findings are model-controllable with no auth
(prompt-injected content reaches them); the Medium/Low integer-narrowing and
unbounded-read findings need extreme inputs and are hardening.

| #   | Sev  | Finding                                                   | Resolved in                                |
| --- | ---- | --------------------------------------------------------- | ------------------------------------------ |
| 1   | High | LSP navigation bypasses `guard_secret_read`               | `a932043`                                  |
| 2   | High | LSP rename writes outside the workspace                   | `d242a94`, `ecca804` (+ `aa4642d`)         |
| 3   | High | Regex replacement OOM (literal + capture expansion)       | `0872c0a`, `1f8db24`, `95743c1`            |
| 4   | Med  | Attachment read unbounded before the cap                  | `49f4757`                                  |
| 5   | Med  | `AGENTS.md` / agent profiles: no per-file or total cap    | `ae277a8` (per-file), `22072e6`, `bfac7dd` |
| 6   | Med  | Skill discovery: unbounded bulk reads                     | `ae277a8`, `22072e6`, `bfac7dd`            |
| 7   | Med  | Multi-file LSP rename is non-transactional                | `ec3dd80`, `6a621f3` (cancellation)        |
| 8   | Med  | Malformed LSP positions wrap / silently dropped           | `ec3dd80`, `f952ade` (`+1` overflow)       |
| 9   | Med  | Provider token counters truncate `u64`→`u32`              | `d592989`                                  |
| 10  | Med  | Cancelled MCP requests leak pending entries               | `691cb3f`                                  |
| 11  | Med  | MCP stdio teardown leaves descendant processes alive      | `1a61925` (`d592989` partial)              |
| 12  | Med  | Post-edit reread failure reports false success            | `d592989`                                  |
| 13  | Med  | Config persistence destroys malformed / concurrent config | `691cb3f`, `076cfd4`                       |
| 14  | Low  | `cwd_slug` collisions across distinct projects            | `cdc2518` (hash suffix)                    |
| 15  | Low  | Predictable temp names allow symlink pre-creation         | `07c6815` (`create_new`)                   |
| 16  | Low  | Wire-log symlink TOCTOU _(= pass 2, finding 4)_           | `cc5586e`, `fb0ca0c` (`O_NOFOLLOW`)        |
| 17  | Low  | Input-history load is unbounded                           | `cdc2518`                                  |
| 18  | Low  | Session saves lose cross-process updates                  | `29e6539`, `a76d022` (lock + fork)         |
| 19  | Low  | Cancellation leaks `.hrdr-tmp` files                      | `cdc2518` (RAII `TempFile` guard)          |
| 20  | Low  | ChatGPT catalog / cache reads unbounded                   | `cdc2518` (cache), `ec65315` (stream)      |
| 21  | Low  | `/todo-ttl` `u64`→`i64` truncation corrupts config        | `cdc2518` (clamp)                          |
| 22  | Low  | Editor row count narrows before clamping                  | `cdc2518`                                  |
| 23  | Low  | Session-ID failure → thousands of synchronous opens       | `cdc2518`                                  |
| 24  | Low  | Legacy MCP SSE accounting undercounts partial events      | `cdc2518` (`buffered_bytes` carry)         |

Notes:

- **#3 regex OOM** landed in two steps: `0872c0a` bounded the literal projection
  and `1f8db24` added an incremental bound for regex mode, but the first cut
  checked size _after_ `caps.expand()` — so a single giant match still
  over-allocated. `95743c1` moved the check _before_ the expand (per-match
  projection), closing the single-match case.
- **#2 / #8 LSP** each needed a follow-up: `ecca804` fixed a macOS-only
  regression from `d242a94` (canonicalize _both_ sides of the confinement check,
  not just the target), and `f952ade` closed a `u32::MAX + 1` overflow left by
  the initial position validation in `ec3dd80`.
- **#11 MCP** first got a partial `process_group(0)` in `d592989` that never
  signalled the group; `1a61925` adopted the real `ProcessGroup` guard so
  teardown kills the whole tree (unix `kill(-pgid)`, Windows job-object).
- Several early fixes were duplicated across a messy merge (`207636b`,
  `7fe2e25`, `56268bd`, `83ecab5`, `8de498d`, `aeb356e`); the linear commits
  above are the ones on `main`.

### Session lost-update (#18) — how it was closed

`crates/hrdr-app/src/session.rs`, `sessions.rs`, and the TUI session flow.

Two hrdr instances resuming the same session and autosaving were
last-writer-wins — the later atomic rename silently discarded the other's turns.
Now each active session holds a lifetime **open-lock** (`.{id}.open.lock`,
distinct from the brief `.{id}.lock` id-reservation), carrying `PID TIMESTAMP`:

- **Explicit `/resume`** of a session open in another _live_ instance is refused
  (hard error), and offers **`[f] open a copy`** — `Session::fork` reads the
  source's on-disk snapshot _unlocked_ (source untouched), copies it under a
  fresh id named `"<orig> (fork)"`, and holds the fork's own lock.
- **Auto-resume at startup** on a busy candidate silently starts fresh — no
  jarring startup error.
- **Auto lock cleanup:** a lockfile whose owning PID is gone (crash / `kill -9`)
  is reclaimed on the next open, reusing the existing `is_stale_lock` /
  `owner_process_alive` reclaim (Linux `/proc`, macOS `kill -0`).
- **Residual:** liveness is PID-based, so on a cross-host network filesystem a
  lock from another machine can be misjudged — the same limitation the
  pre-existing id-reservation lock already carried; the open-lock adds no
  cross-host coordination.

### Related structural change

`26125f1` unified the two credential stores (`auth.toml` raw keys + `oauth.json`
OAuth tokens) into a single `~/.config/hrdr/auth.json` (flat tagged map,
migrate-and-delete on first run). Not an audit finding, but it landed alongside
this work and moved the `auth.json` file onto the read-tool secret deny-list.

---

## Pass 2 — targeted audit (19 drafted → 5 real)

A second, independently drafted pass listed 19 findings. Re-verification against
the tree dropped 14:

- **Stale** — already fixed by pass 1 / the tree: LSP secret-read bypass, LSP
  rename confinement, regex-replacement OOM, unbounded attachment reads, and
  session-file permissions (`0600` via `write_atomic`).
- **False / not present** — cited symbols (`ModelRef::parse().unwrap()`,
  `PersistedMessageInner`, `is_path_in_workspace`) do not exist in this repo's
  history; the real deserialize path already uses
  `map_err(serde::de::Error::custom)?`. `write_atomic` symlink "overwrite" does
  not work because `rename(2)` replaces a symlink at the destination rather than
  following it. DNS-rebinding, memory-scope traversal, and MCP/`watch`
  "arbitrary command" claims describe either code that returns only filtered
  addresses, path validation that rejects non-`Normal` components, or the
  intended, documented threat model (guardrails are "a safety net against model
  mistakes, not a security boundary").

The five that survived: two actionable (owner-only file/dir permissions), one
defense-in-depth, and two marginal.

### 1. Medium — Input history file uses umask-dependent permissions ✅ FIXED (`0610404`)

**Location:** `crates/hrdr-app/src/history.rs:116-129`

`persist_history` created the directory with `std::fs::create_dir_all(parent)`
and wrote the file with `std::fs::write(path, body)` — neither set an explicit
mode, so the history file landed at `0666 & ~umask` (typically `0644`). Input
history contains the user's own prompts.

**Fix:** routed the write through the `write_atomic` helper the session path
already uses, which creates its temp with `.mode(0o600)` before renaming.

### 2. Medium — Credential directory created with umask, not 0700 ✅ FIXED (`b182502`)

**Location:** `crates/hrdr-agent/src/auth.rs:186`;
`crates/hrdr-agent/src/oauth.rs:556`

`save_token_at` / `save_oauth_at` called bare `create_dir_all(parent)` with no
follow-up, so `~/.config/hrdr/` landed at `0755` — world-listable. The
credential files themselves were correctly `0600`, but the directory leaked
which providers you have authenticated.

**Fix:** a shared `create_dir_owner_only` helper sets the dir to `0o700` after
`create_dir_all`, mirroring the existing `subagent_transcript.rs:89-95` pattern.

### 3. Medium — OpenRouter OAuth flow has no CSRF state parameter ⏸ DEFERRED

**Location:** `crates/hrdr-agent/src/oauth.rs:280-287`;
`crates/hrdr-app/src/login.rs:344,467`

`openrouter_authorize_url` builds the authorize URL from `callback_url` +
`code_challenge` + `code_challenge_method=S256` only — no `state`. The callback
is awaited with an empty `expected_state`, so `parse_callback`'s check passes
for any callback that omits `state`.

**Why deferred, not skipped:** strict `state` validation would break OpenRouter
login if the provider does not echo `state` back — and the existing code comment
says it does not. opencode, checked as a reference implementation, authenticates
OpenRouter with a plain bearer API key and has **no** PKCE/`state` flow at all,
so it could not confirm the provider's behavior.

**Mitigations already present (why it is defense-in-depth):** the listener binds
`127.0.0.1` only, so a _local_ attacker is required; PKCE binds the exchange
(`openrouter_exchange` sends the `code_verifier`), and a code the attacker
obtained was issued against the attacker's own challenge, so exchanging it with
our verifier fails at OpenRouter. Worst realistic case is a local denial of the
login attempt, not credential substitution.

**Fix when unblocked:** mint and pass `state` as the ChatGPT/OpenAI flow already
does — `generate_state()` (`oauth.rs:101`) exists and is used there.

### 4. Low — Wire-log file opening has a symlink TOCTOU window ✅ FIXED (`fb0ca0c`)

_(Same issue as pass 1, finding 16.)_

**Location:** `crates/hrdr-llm/src/client.rs:77-122`

`open_wire_log` did a `symlink_metadata` preflight then a _separate_
symlink-following open; the non-atomic window (documented in-source) let a local
attacker swap in a symlink between check and open. `rotate_wire_log` shared it.

**Why marginal:** the path is caller-chosen via the opt-in `HRDR_LOG_REQUESTS`,
the append runs as the hrdr user (a symlink to a root file just yields
`EACCES`), and a post-open `is_file()` descriptor check backs it up.

**Fix:** open with `custom_flags(libc::O_NOFOLLOW)` on Unix so a final-component
symlink fails with `ELOOP` — the open _is_ the check. Residual: `O_NOFOLLOW`
only covers the final component; a symlinked _parent directory_ is still
followed.

### 5. Low — HTML escaping incomplete in OAuth error page ✅ FIXED (`f36034b`)

**Location:** `crates/hrdr-agent/src/oauth.rs:262-276`

`html_escape` covered `&`, `<`, `>` but not `"` or `'`.

**Why theoretical:** the only caller interpolates the value as element **text**
content (`<p>{}</p>`), never an attribute, so the missing quote escapes are
unreachable and `<`/`>` escaping already blocks tag injection.

**Fix (defense-in-depth):** added `"` → `&quot;` and `'` → `&#39;` so the helper
is correct for any future attribute-context use.

---

## Verification

Every fix landed with a regression test proven to fail without it, under
delegated implementation + independent review, and each batch was validated in
CI across ubuntu/macOS/windows (rustfmt, clippy `-D warnings`, cargo-deny,
cargo-machete, the full test suite, and the leak-guard). As of the latest update
the suite is **1240 tests** green.

Two review catches worth recording, because both passed the implementing agent's
own gates yet were wrong:

- The first regex-OOM bound (#3) checked size _after_ expansion, so a single
  giant match still over-allocated — caught in review, fixed in `95743c1`.
- The aggregate-cap work (#5/#6) added `eprintln!` truncation warnings to code
  that runs inside the TUI, which would corrupt the display — caught in review,
  made silent in `bfac7dd`.
