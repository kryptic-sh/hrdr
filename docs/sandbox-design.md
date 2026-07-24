# OS sandbox — filesystem confinement for agents

Status: **design** (not yet implemented). Motivated by a delegated non-Claude
sub-agent that `cd`'d out of its worktree into the parent repo and committed to
`main` (see `docs/agent-logic-migration.md` lineage / the worktree-escape
investigation). Guidance alone only reaches models inclined to obey; hrdr runs
arbitrary models, so it needs an **enforced** boundary — what Codex has by
default and Claude Code has opt-in.

## Goal

Confine an agent's filesystem access to its **working directory** plus a
**dedicated per-session scratch dir**, enforced by the OS (not just the prompt),
**on by default**. A write sub-agent's cwd is its worktree, so the same
mechanism makes the parent repo unwritable — the escape becomes impossible, not
merely discouraged.

## `SandboxMode`

```rust
/// How much of the filesystem an agent may touch. Enforced by the OS for shell
/// children and by a software path-guard for the in-process file tools.
pub enum SandboxMode {
    /// No confinement — full read/write everywhere. The pre-sandbox behavior.
    None,
    /// Read broadly (builds need /usr, toolchains, ~/.cargo, …); write ONLY
    /// within the writable roots (cwd + session scratch + tool-output dir).
    Write,
    /// Read ONLY within the readable roots (cwd + session scratch); no writes
    /// anywhere. For read-only / research agents.
    Read,
}
```

Default: **`Write`** (a coding agent must write in its cwd; it must read the
system to build). `None` is the explicit opt-out.

### Root sets per mode

| Mode  | Readable                 | Writable                                  |
| ----- | ------------------------ | ----------------------------------------- |
| None  | everything               | everything                                |
| Write | everything¹              | `{cwd, session_scratch, tool_output_dir}` |
| Read  | `{cwd, session_scratch}` | — (none)                                  |

¹ **Broad reads in `Write` are a deliberate tradeoff** (matches Codex
`workspace-write`): builds/toolchains read all over the FS, and enumerating
every ecosystem's read roots is fragile. The cost is that a shell command can
_read_ `~/.ssh`, `~/.aws/credentials`, etc. Mitigations: the file tools keep
`guard_secret_read` (already blocks reading known secret files in-process), and
a later refinement can Landlock-allow a curated read set (system dirs +
toolchain caches) instead of `/` to also close shell secret-reads. Flagged, not
solved, in v1.

## The session scratch dir

`/tmp/hrdr.<random>/`, created once at session start (mode `0700`), removed at
session end. It is a writable root in `Write`/`Read`-relevant modes so the agent
has a scratch area outside the project tree. Distinct from the existing
`tool_output_dir` (where `shell`/`grep`/`git` spill overflow) — **both** must be
writable roots in `Write` mode or overflow-spill breaks under the sandbox.

Sub-agents share the session scratch (they are one session); each write
sub-agent's _cwd_ root is its own worktree, so their writable sets are
`{own worktree, shared scratch, tool_output_dir}` — mutually isolated on the
project tree, shared only on throwaway scratch.

## Two enforcement layers (this is the crux)

hrdr is a single process doing both the agent's tool I/O **and** the app's own
I/O (sessions in `~/.local/share`, config, memory). We cannot Landlock the whole
process — it would break the app. So enforcement is split by where the I/O
happens:

### 1. OS sandbox — for `shell` children (the untrusted-command vector)

Applied to each spawned command, not to hrdr itself, so the app is unaffected.

- **Linux (primary):** [Landlock](https://landlock.io) via a `pre_exec` closure
  in the child (post-fork, pre-exec), added in `proc::configure` /
  `run_streamed_command`'s spawn. The child (`bash`) and every descendant
  (`cargo`, `git`, …) inherit the ruleset — read/write rights granted only on
  the mode's roots. `cd /parent && git commit` then **fails at the OS**: the
  parent isn't a writable root. Requires kernel ≥ 5.13 (Landlock ABI 1); newer
  ABIs add finer rights. The `landlock` crate gives a safe builder.
- **macOS:** wrap the command with `sandbox-exec` and a generated `.sb` profile
  (seatbelt) granting the same roots. (Seatbelt is deprecated-but-present; the
  same approach Codex uses.)
- **Windows:** no first-class equivalent; `SandboxMode` is advisory there
  (software layer only) with a one-time notice. Not a regression — there is no
  sandbox today.
- **Fallback (old kernel / unsupported):** skip the OS layer, keep the software
  layer, and surface once that shell commands are **not** OS-confined so the
  user knows the guarantee is degraded. Never silently pretend to sandbox.

### 2. Software path-guard — for the in-process file tools

`read`/`write`/`edit`/`move`/`copy`/`delete`/`ls`/`grep`/`find`/`tree` do their
I/O in the hrdr process, so the OS sandbox above does not touch them. They get a
mode-aware check at path resolution (the natural home is
`ToolContext::resolve` + the existing `guard_secret_*` seam):

- resolve + canonicalize the path (reuse the existing symlink-safe canonicalize,
  so a `..`/symlink escape is caught — the removed cwd-confinement code is the
  starting point),
- **write op** (`write`/`edit`/`move` dest/`delete`/`copy` dest): reject if the
  canonical path is not under a writable root,
- **read op** in `Read` mode: reject if not under a readable root,
- corrective error naming the roots, so the model self-corrects (Codex's
  positive-declaration lesson: say what IS allowed, not just what isn't).

This layer is also the only enforcement on Windows and in the Landlock-fallback
case, so it must be correct on its own, not merely a nicety.

## Composition with worktree isolation

This is the payoff. A write sub-agent's `cfg.cwd` is its worktree
(`<repo>/.hrdr/worktrees/wt-…`). Under `Write` mode:

- **OS layer:** the shell child can only write under the worktree + scratch, so
  `cd <repo> && git commit` cannot write the parent's index/objects — blocked.
- **Software layer:** `write`/`edit`/`touch`-via-tool against a parent path is
  rejected with a message.

The worktree-escape that started this whole thread becomes structurally
impossible for any model, Claude or not — which is the point.

## Telling the model (Codex's lesson)

Declare the boundary in the system prompt, interpolated like Codex's
`permissions_instructions`: the active `SandboxMode` and the concrete writable
roots ("you may write only within `<cwd>` and `<scratch>`; writing elsewhere is
refused"). A positive allow-list anchored to real paths beats the negative
"don't cd to the parent" clause — the model checks "is this under my root?"
rather than enumerating escapes. Keep the worktree clause too; belt and
suspenders.

## Configuration

- `AgentConfig.sandbox: SandboxMode` (default `Write`).
- Config file: `sandbox = "write" | "read" | "none"`.
- Flag: `--sandbox <mode>` and a `--no-sandbox` alias for `none`.
- Env: `HRDR_SANDBOX=write|read|none`.
- Per-agent: a read-only sub-agent is forced to (at most) `Read` regardless of
  the session default — it has no write tools anyway, so `Read` is the natural
  fit; a write sub-agent inherits the session mode (min of session mode and
  `Write`).

## Out of scope for v1 (follow-ups)

- **Network sandboxing.** Codex also confines network; hrdr's `web`/`fetch`
  tools are in-process (guarded by `web.rs` SSRF checks today). A network mode
  on `SandboxMode` (or a separate axis) is a later addition.
- **Curated read allow-list** for `Write` (close shell secret-reads) — see
  footnote 1.
- **`danger_full_access` parity** — `None` already covers it.

## Implementation slices

1. `SandboxMode` enum + `AgentConfig.sandbox` + config/flag/env plumbing +
   session scratch dir creation/teardown. No enforcement yet (default `None` so
   nothing changes) — just the wiring, tested.
2. Software path-guard in `ToolContext` (writable/readable roots + resolve-time
   check) for the file tools. Flip default to `Write`. Tests: write outside cwd
   refused; read outside cwd refused in `Read`; scratch + tool_output writable.
3. Linux Landlock layer in the shell spawn (`pre_exec`), gated on kernel
   support, with graceful fallback + the degraded-guarantee notice. Tests behind
   a Linux+Landlock cfg.
4. Prompt declaration of mode + writable roots (interpolated).
5. macOS `sandbox-exec` layer. (Windows stays software-only.)

Slice 1–2 give the software boundary (works everywhere, closes the file-tool
vector immediately); slice 3 adds the OS hard-floor for shell on Linux, which is
where the escape actually happened.

## No-migration note (pre-1.0)

New config key with a default; existing sessions/configs unaffected. Turning the
default to `Write` is a behavior change (writes outside cwd now refused) — call
it out in CHANGELOG under Changed/Breaking, and `--sandbox none` restores the
old full-access behavior.
