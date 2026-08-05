---
name: deps_pnpm
description: update pnpm (Node.js) dependencies to latest stable
---

Update a Node.js project's dependencies under pnpm (`pnpm-lock.yaml`).

- **Detect**: `package.json` + `pnpm-lock.yaml` (+ `pnpm-workspace.yaml` for a
  monorepo). If the lockfile is `package-lock.json` / `yarn.lock` / `bun.lock`,
  use `:deps_npm` / `:deps_yarn` / `:deps_bun` instead.

- **Current lockfile**: `pnpm update` (alias `pnpm up`) resolves everything to
  the newest the ranges allow, across all workspaces by default. `pnpm outdated`
  lists what's behind.

- **Latest stable**: `pnpm update --latest` raises every dependency to the
  latest stable (crossing majors, never downgrading to a prerelease). A single
  package: `pnpm update <pkg>@latest` or `pnpm add <pkg>@latest`; a scope:
  `pnpm update "@babel/*"`; exclude with `pnpm update "\!webpack"`.
  `--interactive` lets you pick per package. `--no-save` updates the lockfile
  without touching the ranges.

- **Monorepos**: `pnpm --recursive update`, `pnpm update --workspace` (link
  workspace packages to each other's latest), and per-workspace filters
  (`--filter <package>`).

- **Lockfile**: `pnpm install --lockfile-only` regenerates `pnpm-lock.yaml`
  without touching `node_modules`. Commit the lockfile with the manifest; CI
  runs `pnpm install --frozen-lockfile` and fails otherwise.

- **Verify**: `pnpm install --frozen-lockfile`, then the project's scripts
  (`pnpm test` / `pnpm run lint` / `pnpm run build`). Check
  `node_modules/<pkg>/` for changed APIs.

- **Gotchas**: pnpm blocks dependency lifecycle scripts by default (recent
  versions) — after a big bump, `pnpm approve-builds` may be needed before
  installs succeed. `pnpm dedupe` collapses duplicate versions across the
  graph. Since pnpm 11, `pnpm update` can also bump GitHub Actions pins
  (`--include-github-actions`).
