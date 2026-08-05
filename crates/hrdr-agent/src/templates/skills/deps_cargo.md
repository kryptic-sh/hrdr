---
name: deps_cargo
description: update Rust dependencies (cargo) to latest stable
---

Update a Rust project's dependencies to their latest stable versions.

- **Detect**: `Cargo.toml` (workspace or single crate) + `Cargo.lock` (when
  tracked — library crates that gitignore the lock skip the regen). Workspace
  members and their own `Cargo.toml`s may declare deps outside the root
  `[workspace.dependencies]` table — check every member.

- **Current lockfile**: `cargo update` resolves everything to the newest the
  ranges allow; `cargo update -p <crate>` one crate, `cargo update -p
  <crate>@<version>` a specific pin. If it reports "Locking 0 packages", the
  lockfile is already current — the ranges themselves may still lag.

- **Latest stable**: "latest" is the newest non-prerelease (`max_stable_version`
  on crates.io). Raise a range with `cargo add <crate>@<version>` (edits the
  manifest, asks the registry — never hand-write a version). `cargo add` on an
  already-present dep keeps the existing range unless you pass `@version`.
  Workspace deps: run it with `--package <member> --target <cfg>` as needed and
  move the entry up into `[workspace.dependencies]` if that's where it lives.
  `cargo upgrade` (cargo-edit, if installed) raises every range at once; never
  install it just for this — `cargo add` per dep is fine.

- **Transitives**: `cargo update` covers them. A lockfile carrying several
  major versions of one crate is normal when different crates pin different
  majors — `cargo tree -i <crate>@<version>` tells you who pulls each.

- **Verify**: `cargo build` (workspace), `cargo test`, `cargo clippy --all-targets
  -- -D warnings`, `cargo fmt --all`. Run the project's CI gate — typically
  with `--locked`, which fails if the lockfile wasn't regenerated, so commit
  the regenerated `Cargo.lock` in the same commit as any manifest change.

- **Platform-gated code** (`#[cfg(windows)]` etc.) is NOT compiled by a local
  run — a green local pass says nothing about it. Check moved items against the
  unpacked new source (`~/.cargo/registry/src/*/<crate>-<version>/`) and say
  plainly what you could not compile.

- **Gotchas**: a yanked release is refused by `cargo add`/`update`; an
  `-rc`/`-beta` version sorts oddly in plain version sort — the stable one is
  the max without a prerelease suffix. `cargo update --dry-run` shows what would
  move without changing anything.
