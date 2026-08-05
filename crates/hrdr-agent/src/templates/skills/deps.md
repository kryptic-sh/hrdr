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

2. **Learn the tool, don't recall it.** Run `:cli <manager>` and follow it:
   confirm the tool is installed, read its help, verify the commands before
   running them. The version on this machine is the only version that matters.
   When two managers are present (a monorepo with workspaces, a Python project
   with both `uv` and a `requirements.txt`), handle each in turn.

3. **Update the lockfile first, within the current constraints.** The manager's
   plain update command (`cargo update`, `npm update`, `pnpm update`,
   `bun update`, `go get -u`, `composer update`, `bundle update`, …) resolves
   everything to the newest version the existing ranges allow. Commit nothing
   yet — this is the zero-risk half. Beware the managers that do NOT roll
   forward on their own: uv's lockfile only changes when asked
   (`uv lock --upgrade`).

4. **Then decide on constraint bumps.** "Latest stable" may need the manifest
   ranges themselves raised. Prefer the manager's own "latest" flag when it has
   one (`pnpm update --latest`, `bun update --latest`) — it raises every range
   at once — and the manager's own add/update-with-a-version command for a
   single package (`cargo add dep@latest`, `npm install dep@latest`,
   `poetry add dep@latest`, `go get dep@latest`, …). Never guess a version: the
   manager asks the registry.

5. **Regenerate the lockfile with the manager's command and commit it in the
   SAME commit as the manifest.** A frozen-lockfile CI gate fails on any
   manifest change whose lockfile wasn't regenerated — and an uncommitted
   lockfile fix is not a fix. Never hand-edit a lockfile. (Zig is the exception
   that proves the rule: `build.zig.zon` is manifest and lock in one — the
   `.hash` pins ARE the versions, fetched with `zig build --fetch`.)

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
