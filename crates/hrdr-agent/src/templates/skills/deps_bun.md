---
name: deps_bun
description: update bun (Node.js/JavaScript) dependencies to latest stable
---

Update a JavaScript/TypeScript project's dependencies under bun
(`bun.lock` / `bun.lockb`).

- **Detect**: `package.json` + `bun.lock` (text) or `bun.lockb` (binary).
  Bun also installs from `package-lock.json`, `yarn.lock` and `pnpm-lock.yaml`
  — but if the project's lockfile is one of those, use that manager's run book
  (`:deps_npm` / `:deps_yarn` / `:deps_pnpm`) to keep the file it commits to.

- **Current lockfile**: `bun update` resolves everything to the newest version
  the ranges allow; `bun update <pkg>` one package. `bun outdated` lists what's
  behind.

- **Latest stable**: `bun update --latest` raises every dependency to its
  latest stable, crossing majors; `bun update <pkg>@latest` one package;
  `bun add <pkg>@latest` adds/updates with an explicit range.
  `bun update --interactive` opens a picker (Space to select, `l` toggles a
  package between its range-respecting target and the true latest).

- **Monorepos / workspaces**: `bun update --interactive --recursive` (also
  `-r`) updates across all workspaces, with a Workspace column in the picker.
  Bun's `catalogs` (`bun pm` / `bunfig.toml`) pin shared versions across
  workspaces — update the catalog, not each consumer.

- **Lockfile**: `bun install` regenerates the lockfile (bun re-resolves on
  install). Commit it with the manifest; CI runs `bun install --frozen-lockfile`
  and fails otherwise.

- **Verify**: `bun install --frozen-lockfile`, then the project's scripts
  (`bun test`, `bun run lint`, `bun run build`). Check `node_modules/<pkg>/`
  for changed APIs.

- **Gotchas**: `bun audit` reports advisories (and `bun audit fix` where
  supported). Bun's own runtime version is a separate upgrade (`bun upgrade`),
  not a dependency. With `bun.lockb` (binary), treat it as generated — never
  hand-edit; the diff is noise, the content is what matters.
