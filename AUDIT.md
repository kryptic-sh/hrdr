# Codebase Audit

Date: 2026-07-18  
Version reviewed: 0.6.2  
Scope: Rust workspace, application entry point, tests, and CI configuration

## Executive summary

No critical or high-severity correctness or security defects were confirmed. The
codebase has strong defenses around SSRF, credential storage, secret-file reads,
subprocess cleanup, atomic writes, and delegated Git worktrees.

One defense-in-depth finding remains: the opt-in wire-log path follows symbolic
links before validating the target.

## Findings

### Low: Wire-log opening follows symbolic links

**Location:** `crates/hrdr-llm/src/client.rs:77-94`

`open_wire_log` opens the caller-selected `HRDR_LOG_REQUESTS` path before
checking the opened handle's metadata. `OpenOptions::open` follows symbolic
links, and `file.metadata()` describes the link target. A symbolic link to a
regular file therefore passes the regular-file check. On Unix, the subsequent
`set_permissions(0600)` also operates on that target.

**Trigger:** Another local actor, or a mistaken setup, places a symbolic link at
the configured wire-log path before hrdr opens it.

**Impact:** hrdr can append request and response data to an unintended regular
file and change that file's permissions. Exploitation requires control over the
configured path or its containing directory, so this is defense-in-depth rather
than a privilege-boundary bypass. Rotation also performs path-based rename and
reopen operations, so validating only once before opening would not fully close
path-replacement races.

**Recommendation:** On supported Unix targets, open with no-follow semantics and
validate the opened descriptor as a regular file. Keep all rotation operations
anchored to a trusted directory or reapply the same no-follow open discipline
after rotation. At minimum, reject a pre-existing symlink with
`symlink_metadata`, while documenting that this alone does not eliminate
time-of-check/time-of-use races.

## Coverage observations

Compaction and delegation contain complex, stateful behavior worth continued
integration coverage. Contrary to an initial broad review, these areas are not
untested: `crates/hrdr-agent/src/lib.rs` includes worktree cleanup and task-diff
tests, and compaction helpers are exercised through the crate test module. A
focused cancellation/failure test for `CompactingGuard` and a full
background-task lifecycle test would still improve regression protection, but
their absence is not a confirmed defect.

## Checks run

| Check                                       | Result                                             |
| ------------------------------------------- | -------------------------------------------------- |
| `cargo test --workspace`                    | Passed                                             |
| `cargo clippy --all-targets -- -D warnings` | Passed                                             |
| `cargo deny --all-features check`           | Passed; duplicate dependency-version warnings only |
| `cargo fmt --all --check`                   | Passed                                             |
| Working-tree status before audit document   | Clean                                              |

## Reviewed safeguards

The audit specifically sampled and traced:

- SSRF filtering, redirect handling, DNS rebinding resistance, and response-size
  caps.
- Credential-file permissions and atomic replacement.
- Secret-file read and Git-diff redaction guards.
- Shell command guardrails and subprocess-tree termination.
- Session and delegated-worktree cleanup behavior.
- Untrusted external-content framing.

No additional actionable defect was confirmed in those areas.
