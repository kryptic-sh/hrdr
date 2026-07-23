# Deferred improvements / backlog

Smaller items that were identified but not yet done or tracked elsewhere. Larger
efforts have their own docs (`agent-logic-migration.md`, `task-revive.md`,
`security-audit.md`, `dry-audit.md`, `memory-tool-analysis.md`). Sandbox mode is
issue #13; the Codex catalog pin is issue #2.

## Tooling / agent capability

- **Model pre-flight validation.** Verify a configured model actually exists on
  its provider before starting a turn, so a typo'd/unavailable model fails fast
  with a clear message instead of mid-turn.
- **Batched `edits[]` on the `edit` tool.** Let one `edit` call carry an array
  of `{old_string, new_string}` edits against a file, applied in order — fewer
  round trips than one call per hunk, and atomic per file.
- **LSP diagnostics dedup.** The same diagnostic can surface more than once
  (overlapping ranges / re-published sets); dedupe before showing the model.
- **Sub-agent isolation guard.** A defensive check that a write sub-agent's tool
  operations stay within its worktree — belt-and-suspenders on top of the cwd
  being set to the worktree (escaping is by design for full-FS access, but an
  accidental parent-tree write is worth catching/telemetering).

## Consistency / robustness

- **Guardrail rules live in two places.** The shell guardrail rule set is
  encoded both in `crates/hrdr-tools/src/guardrails.rs` (mechanical enforcement)
  and in `crates/hrdr-agent/src/templates/system.j2` (prompt guidance that tells
  the model not to attempt them). Adding a rule means editing both, or they
  drift. Not worth auto-deriving (the prompt phrasing is deliberately more
  nuanced than the terse guardrail messages) — but a checklist/test that the two
  sets agree would catch drift.

## Test coverage gaps

- **TUI history up/down fix** (`6ff0172`, `suppress_completions`) shipped
  without a regression test — a test that Up/Down after a slash-command history
  entry navigates history rather than the completion popup.

## Known behaviour to revisit

- **Input-path unification UX.** After the "every user message is a queued
  `Steer`" refactor, a submitted message renders when its `Steered` event is
  pumped (a beat after submit) rather than synchronously, matching sub-agent
  behaviour. Intended and imperceptible with a fast pump; if it ever reads as
  laggy, the opener could be pumped synchronously.
