---
name: deps_npm
description: update npm (Node.js) dependencies to latest stable
---

Update a Node.js project's dependencies (npm lockfile — `package-lock.json`).

- **Detect**: `package.json` + `package-lock.json`. If the lockfile is
  `pnpm-lock.yaml` / `yarn.lock` / `bun.lock`, use `:deps_pnpm` / `:deps_yarn` /
  `:deps_bun` instead — the lockfile decides the manager.

- **Current lockfile**: `npm update` resolves everything to the newest the
  ranges allow. `npm outdated` lists what's behind, with current/wanted/latest
  columns.

- **Latest stable**: raise a range with `npm install <pkg>@latest` (bumps the
  `package.json` range and installs — the one command for "update this dep").
  For everything at once, `npm update --save` is not the same as `--latest`;
  the reliable all-at-once route is per-package `@latest` or a tool like
  `npx npm-check-updates -u` (which rewrites ranges to the latest and is the
  community standard for bulk bumps).

- **Lockfile**: `npm install` regenerates `package-lock.json`. Commit the
  lockfile in the same commit as the manifest; CI runs `npm ci` (frozen) and
  fails otherwise.

- **Verify**: `npm ci` then the project's own scripts (`npm test`, `npm run
  lint`, `npm run build`). Check `node_modules/<pkg>/` (the installed copy) for
  changed APIs.

- **Gotchas**: `npm ci` deletes `node_modules` — never run it as a casual
  local check on a project mid-work. `npm audit`/`npm audit fix` addresses
  advisories; `npm audit fix` only upgrades within ranges, `--force` is needed
  for majors — prefer an explicit `npm install <pkg>@latest` for a bumped
  major. Prerelease dist-tags: `@latest` is the stable tag; a package's newest
  published version may still be a prerelease under a different tag.
