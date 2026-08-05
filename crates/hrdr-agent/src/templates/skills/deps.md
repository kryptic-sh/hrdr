---
name: deps
description: update project dependencies to latest stable versions
---

Update the project's dependencies to their latest stable versions (newest
non-prerelease), fixing any code the bumps break. Work from the project's own
tooling — never hand-edit a manifest or a lockfile, and never write a version
number from memory: every figure comes from the package manager, which asks the
registry what exists right now.

1. **Identify the package manager.** The manifest and lockfile name it, in this
   order: `Cargo.toml` + `Cargo.lock` (cargo), `package.json` + one of
   `package-lock.json` / `pnpm-lock.yaml` / `yarn.lock` / `bun.lock` (npm, pnpm,
   yarn, bun — the lockfile decides which), `pyproject.toml` + `uv.lock` or
   `poetry.lock` (uv / poetry), `requirements*.txt` (+ `pip-tools`) (pip),
   `go.mod` + `go.sum` (go), `build.zig.zon` (zig's built-in manager),
   `composer.json` + `composer.lock` (composer), `Gemfile` + `Gemfile.lock`
   (bundler). Read the README/CONTRIBUTING too — the project's own command may
   wrap the package manager.

2. **Match the manager to its run book.** The `:deps_*` skills hold the exact
   commands, detection, lockfile handling and gotchas for each: `:deps_cargo`,
   `:deps_npm`, `:deps_pnpm`, `:deps_yarn`, `:deps_bun`, `:deps_uv`,
   `:deps_poetry`, `:deps_pip`, `:deps_go`, `:deps_zig`, `:deps_composer`,
   `:deps_bundler`. Load the matching one and follow it; where two managers are
   present (a monorepo with workspaces, a Python project with both `uv` and a
   `requirements.txt`), run each in turn.

3. **Update the lockfile first, within the current constraints.** The manager's
   plain update command (`cargo update`, `npm update`, `pnpm update`,
   `bun update`, `uv lock`, `go get -u`, `composer update`, `bundle update`, …)
   resolves everything to the newest version the existing ranges allow. Commit
   nothing yet — this is the zero-risk half.

4. **Then decide on constraint bumps.** "Latest stable" may need the manifest
   ranges themselves raised (`cargo add dep@latest`, `npm install dep@latest`,
   `pnpm add dep@latest`, `yarn up dep`, `bun add dep@latest`,
   `uv add --upgrade dep`, `go get dep@latest`, `composer require dep:^X`,
   `bundle update dep`, `poetry add dep@latest`). Prefer the manager's own
   "latest" flag when it has one (`pnpm update --latest`, `bun update --latest`)
   — it raises every range at once. Never guess a version: the manager asks the
   registry.

5. **Regenerate the lockfile with the manager's command and commit it in the
   SAME commit as the manifest.** A frozen-lockfile CI gate fails on any
   manifest change whose lockfile wasn't regenerated — and an uncommitted
   lockfile fix is not a fix. Never hand-edit a lockfile.

6. **Fix the code the bumps break.** Compile, then chase the errors: renamed or
   moved APIs, changed signatures, feature flags, MSRV bumps. Use the installed
   copy of the new version as the truth (`~/.cargo/registry/src/...`,
   `node_modules/...`, `site-packages/...`, `go env GOMODCACHE`, `vendor/`).
   Read the changelog of each bumped crate/package for what moved. Add or update
   tests where behavior changed.

7. **Run the project's whole gate** — its CI config lists every check; run each
   one's command locally, frozen-lockfile flags included. Report plainly what
   you could NOT compile or run locally (platform-gated code, e.g.
   `#[cfg(windows)]` or per-OS paths, is not built by a local run at all — say
   so rather than claiming a green tree).

8. **Report the diff honestly**: which packages moved and to what, which
   manifests changed, which were already current, and what you could not verify
   locally. If a bump was skipped (a yanked version, a prerelease-only newer
   version, a broken release), name it and why.
