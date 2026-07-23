# DRY Audit

Codebase duplication analysis for hrdr. Analysis date: 2026-07-23. Each section
names a concern, where it lives, and whether it's _dry_ (one source of truth),
_damp_ (intentional duplication with a good reason), or _wet_ (accidental
duplication to fix). Line references are point-in-time and may drift.

---

## 1. Secret/credential file detection — DRY ✅

**One canonical list in `crates/hrdr-tools/src/lib.rs:559` —
`secret_file_reason`.**

Every content-reading path routes through it:

- `read` tool → `guard_secret_read` → `secret_file_reason`
- `grep` tool → `grep_line_is_secret` → `secret_file_reason`
- `validate_attach_path` (used by `@file` mentions + `/add`) →
  `secret_file_reason`
- `redact_secret_diffs` (git diff output) → its own redaction, but calls the
  same pattern-matching logic

Test coverage for each path exists in its own crate. Adding a new secret-file
pattern requires editing exactly one match block.

## 2. Path helpers — DRY ✅

**`resolve_under`, `floor_char_boundary`, `canonicalize_nearest` — defined once
in `crates/hrdr-tools/src/lib.rs`, re-exported where needed.**

`hrdr-app/src/util.rs:170-174` just re-exports (`pub use hrdr_tools::…`). No
crate reimplements these.

## 3. Slash-command dispatch — DAMP ✅

**`hrdr-tui/src/app/commands.rs:11` has `handle_slash` that mirrors
`hrdr-app/src/commands/dispatch.rs:12`'s `dispatch`.**

This is intentional: the TUI handler intercepts TUI-only commands (`edit`,
`reload`, `goto`, `find`/`search`, `next`/`prev`) then falls through to the
shared dispatcher for everything else. The shared `CommandHost` trait lets both
frontends drive the same command logic. The comment at line 18-21 explains the
split explicitly.

## 4. `AgentConfig` / test construction — WET ⚠️

**~60+ call sites construct `AgentConfig { … }` inline, mostly in tests.**

- `crates/hrdr-agent/src/lib.rs` — ~60 inline constructions in test modules
- `crates/hrdr-tui/src/app/e2e.rs` — ~7 inline constructions
- `crates/hrdr-agent/src/validate.rs` — has `cfg()` and `cfg_with()` helpers
- `crates/hrdr-agent/src/resolve.rs` — has its own `cfg()` and `cfg_with()`
  helpers
- `crates/hrdr-app/src/commands/model.rs` — 3 inline constructions
- `crates/hrdr-app/src/commands/dispatch.rs` — 1 inline
- `crates/hrdr-app/src/login.rs` — 2 inline

`cfg()`/`cfg_with()` are duplicated across `validate.rs` and `resolve.rs` (both
in `hrdr-agent`). `lib.rs` tests have no equivalent at all.

### 4a. `fn r(s: &str) -> ModelRef` — 4 identical copies → DRY ✅

**Fixed** (`56b76ab`): one `pub(crate) fn r` in `model_ref.rs` (#[cfg(test)]),
removed 4 copies from `models.rs`, `resolve.rs`, `validate.rs`, `lib.rs`.

### 4b. `fn spec(s: &str) -> ModelSpec` — 4 copies → DRY ✅

**Fixed** (`56b76ab`): one `pub(crate) fn spec` in `model_ref.rs` (#[cfg(test)]),
removed 3 copies from `lib.rs` and 1 from `agents_dir.rs`.

### 4c. `ProviderConfig` — no `Default` impl → DRY ✅

**Fixed** (`56b76ab`): `ProviderConfig` now derives `Default`.

### 4d. `SubagentProfile` — no `Default` impl → DRY ✅

**Fixed** (`56b76ab`): `SubagentProfile` now derives `Default`.

**Remaining**: `cfg()`/`cfg_with()` duplication across `validate.rs` and
`resolve.rs`; `lib.rs` test modules have no equivalent helper. `AgentConfig`
inline constructions (~60 sites) not yet consolidated.

## 5. Session management layering — DRY ✅

**`hrdr-agent/src/session.rs` — persistence (read/write/lock).**
**`hrdr-app/src/sessions.rs` — UI-thread-safe wrappers.**

Clean split:

- `hrdr_agent::session::Session` / `SessionState` — the data model + file I/O
- `hrdr_app::save_agent_session` — locks the agent mutex, syncs state, saves
- `hrdr_app::latest_session_for_cwd` / `open_latest_session_for_cwd` — startup
  auto-resume logic

No overlap; the `hrdr-app` layer calls the `hrdr-agent` layer, never
reimplements it.

## 6. Skill/sub-agent discovery — DAMP ✅

**`crates/hrdr-app/src/skills.rs:64` — `skill_dirs` walks from cwd up to `/` +
XDG dirs.** **`crates/hrdr-agent/src/prompt.rs:201` — `gather_agent_docs` walks
from cwd up to `/` for AGENTS.md files.**

Same walk pattern (cwd → root + XDG fallback), but different payloads (skills vs
agent docs). A shared "walk project dirs" iterator could DRY the directory
traversal, but the logic is simple (~15 lines each) and the divergence in what
they collect makes a shared abstraction borderline over-engineering at current
scale.

## 7. `CommandHost` trait impls — DAMP ✅

**Three `impl CommandHost` blocks:**

- `crates/hrdr-tui/src/app/commands.rs` — real TUI host
- `crates/hrdr-app/src/commands/dispatch.rs` — `TestHost` (used in dispatch
  tests)
- `crates/hrdr-app/src/login.rs` — `TestLoginHost`

The trait itself is the DRY mechanism; each impl is a different kind of host.
`TestHost` and `TestLoginHost` share some trivial method bodies (no-ops for
`autosave`, `set_session_label`, etc.) but the login host has login-specific
state the dispatch test host doesn't need. A shared `TestHost` base could
eliminate the duplicate no-op bodies, but the overhead is tiny.

## 8. TUI selector draw functions — WET ⚠️

**Six nearly identical modal-drawing functions in `crates/hrdr-tui/src/ui.rs`,
each ~100-150 lines:**

| Function                | Line | Selector Type           | Width Clamp |
| ----------------------- | ---- | ----------------------- | ----------- |
| `draw_model_selector`   | 229  | `ModelSelector`         | 92          |
| `draw_skill_selector`   | 334  | `SkillSelector`         | 92          |
| `draw_login_modal`      | 424  | `LoginProviderSelector` | 76          |
| `draw_effort_selector`  | 600  | `EffortSelector`        | 64          |
| `draw_theme_selector`   | 686  | `ThemeSelector`         | 92          |
| `draw_session_selector` | 771  | `SessionSelector`       | 110         |

All six share identical scaffolding (~40 lines each, 240 lines total):

1. Calculate centered `Rect` from area with width/height clamping
2. `f.render_widget(Clear, rect)`
3. Create `Block` with identical style/padding (`BLOCK_PAD_X`, `theme.user_bg`)
4. Early return on `inner.height < 3 || inner.width < 6`
5. Search line (`"Search  "` + filter + cursor `▌`)
6. Hint line (`"N items · ↑↓ select · Enter … · Esc cancel"`)
7. List height math (`inner.height.saturating_sub(3)`)
8. Scroll offset calculation
9. Row iteration with selected/unselected `Line` rendering

The only per-selector variations are the **row type**, **width clamp**, **hint
text**, and **row rendering**. The selector state machine (`Selector<T>` in
`crates/hrdr-tui/src/app/selector.rs`) is already perfectly DRY; the draw
functions are the duplication.

**Actionable**: Extract a generic `draw_selector_modal<T>` that takes closures
for row rendering and a config struct for width/hint/label. Estimated savings:
~500 lines removed, one place to change selector chrome.

## 9. File-attach flows — DRY ✅

**Both `@file` mentions (`util.rs:109 expand_mentions`) and `/add`
(`dispatch.rs:290`) use the same function: `hrdr_tools::read_attach_file`.**

Both also share `crate::MAX_ATTACH_BYTES` from `util.rs:107`. The only
difference is `/add` rejects overlarge files while `@file` truncates — a
deliberate UX choice documented at `dispatch.rs:306-309`.

## 10. Post-edit `FileChange` notes formatting — WET ⚠️

**`tools/write.rs:94-97`** and **`tools/edit.rs:216-219`** — identical 4-line
block:

```rust
let mut warn = fc.notes.join("\n");
if !warn.is_empty() {
    warn.insert(0, '\n');
}
```

Both tools call `apply_file_change`, receive `FileChange { notes, .. }`, and
format the notes identically. A `fn formatted_notes(&self) -> String` on
`FileChange` (or a free helper in `mutation.rs`) would replace both copies.

## 11. `create_dir_all` + `with_context` — WET ⚠️

**`tools/write.rs:86-89`**, **`tools/fileops.rs:99-102`** (MoveTool),
**`tools/fileops.rs:410-413`** (CopyTool) — three identical instances:

```rust
if let Some(parent) = to.parent() {
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
}
```

Extract as `async fn ensure_parent_dir(path: &Path) -> Result<()>` in
`tools/mod.rs`.

## 12. Secret-file write/edit guards — DAMP ✅

**`tools/write.rs:45-51`** and **`tools/edit.rs:105-111`** share identical
`secret_file_reason(canonicalize_nearest(path))` → `bail!(…)` structure, but
each message is tailored ("refusing to write…" vs "refusing to edit…") and
meaningful to the model. `fileops.rs:384-388` has a different message again
("copying it would place its contents…"). The read-side `guard_secret_read` is
already a shared helper; the write-side variation is legitimately DAMP.

## 13. `tokio::fs::try_exists(…).await.unwrap_or(false)` — minor WET

8 occurrences: `tools/write.rs:52`,
`tools/fileops.rs:20,92,184,197,199,391,403`. A trivial
`async fn path_exists(path: &Path) -> bool` helper would clean these up. Low
priority.

## 14. `"(no matches)"` string literal — minor WET

5 occurrences: `tools/find.rs:87`, `tools/grep.rs:244,255,409,511`. Should be a
`const NO_MATCHES: &str = "(no matches)";` in `tools/mod.rs`.

## 15. `ignore::WalkBuilder` patterns — DAMP ✅

4 files build `ignore::WalkBuilder`: `find.rs`, `grep.rs`, `tree.rs`,
`replace.rs`. `grep.rs` already extracted its own `ignore_walker` helper;
`find.rs` has an inline copy differing only in `max_depth` and `parents`.
`tree.rs` and `replace.rs` use intentionally different configurations.

## 16. `strip_prefix(&ctx.cwd).unwrap_or(&path).display()` — minor WET

`tools/replace.rs:153,166`, `tools/lsp_nav.rs:442` — three uses of the same
relative-path display pattern. A
`fn rel_display<'a>(path: &'a Path, cwd: &Path) -> Display` helper would be
cleaner.

---

## Summary

| #   | Concern                            | Verdict   |
| --- | ---------------------------------- | --------- |
| 1   | Secret-file detection              | DRY ✅    |
| 2   | Path helpers                       | DRY ✅    |
| 3   | Slash-command dispatch             | DAMP ✅   |
| 4   | AgentConfig test construction      | WET ⚠️    |
| 4a  | `fn r()` ModelRef parser (×4)      | WET ⚠️    |
| 4b  | `fn spec()` ModelSpec parser (×3)  | WET ⚠️    |
| 4c  | `ProviderConfig` no Default (×25)  | WET ⚠️    |
| 4d  | `SubagentProfile` no Default (×11) | WET ⚠️    |
| 5   | Session layering                   | DRY ✅    |
| 6   | Project-dir walk                   | DAMP ✅   |
| 7   | CommandHost impls                  | DAMP ✅   |
| 8   | TUI selector draw functions        | WET ⚠️    |
| 9   | File-attach flows                  | DRY ✅    |
| 10  | Post-edit notes formatting         | WET ⚠️    |
| 11  | `create_dir_all` + context         | WET ⚠️    |
| 12  | Secret-file write/edit guards      | DAMP ✅   |
| 13  | `try_exists` scattered (×8)        | minor WET |
| 14  | `"(no matches)"` literal (×5)      | minor WET |
| 15  | `ignore::WalkBuilder`              | DAMP ✅   |
| 16  | `strip_prefix` display (×3)        | minor WET |

**Actionable items (ranked by impact):**

1. Extract `draw_selector_modal<T>` for 6 TUI selector draw functions (#8) —
   ~500 lines consolidated, one place to change selector chrome.
2. Add `Default` for `ProviderConfig` and constructor for `SubagentProfile`
   (#4c/#4d) — eliminate ~36 full-field boilerplate sites.
3. Extract shared test helpers in `hrdr-agent`: `fn r()`, `fn spec()`,
   `fn cfg()` (#4a/#4b) — eliminate 9 duplicated definitions.
4. Fix post-edit notes formatting (#10) — `fn formatted_notes()` on
   `FileChange`.
5. Extract `ensure_parent_dir()` (#11) — 3 call sites → 1.
6. Low-hanging fruit: `NO_MATCHES` const (#14), `path_exists` helper (#13),
   `rel_display` helper (#16).

---

Verdict: **The codebase is well-factored.** Most duplication is intentional
(DAMP) with documented rationale. The WET spots are small, mechanical, and
straightforward to fix — no architectural rot.
