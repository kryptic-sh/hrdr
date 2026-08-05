---
name: deps_yarn
description: update yarn (Node.js) dependencies to latest stable
---

Update a Node.js project's dependencies under yarn (`yarn.lock`). Two very
different CLIs share the name — detect which one the project uses first.

- **Detect**: `package.json` + `yarn.lock`. Classic (Yarn 1) has no
  `.yarnrc.yml` and the `yarn --version` is `1.x`; Berry (Yarn 2/3/4) has a
  `.yarnrc.yml` and `.yarn/` directory. The commands differ — match the
  version.

- **Classic (Yarn 1)**:
  - `yarn outdated` — what's behind, with current/wanted/latest.
  - `yarn upgrade` — within ranges; `yarn upgrade --latest` — everything to
    latest (crossing majors).
  - `yarn upgrade <pkg>` / `yarn upgrade <pkg>@latest` — one package.
  - `yarn upgrade-interactive` — pick per package.
  - Lockfile: `yarn install` regenerates `yarn.lock`; CI runs `yarn install
    --frozen-lockfile`.

- **Berry (Yarn 2+)**:
  - `yarn up <pkg>` — to latest, ignoring the manifest range, across ALL
    workspaces; `yarn up <pkg>@1.2.3` — to a specific version; globs like
    `yarn up "@babel/*"`. `-i` asks per package; `-E`/`-T`/`-C` set the semver
    modifier written to the manifest.
  - `yarn up -R` re-resolves every matching range in the lockfile without
    touching manifests — run both for a full bump.
  - `yarn dedupe` collapses duplicate versions; `yarn explain peer-requirements`
    diagnoses peer conflicts after a bump.
  - Lockfile: `yarn install --mode=update-lockfile` regenerates `yarn.lock`
    cheaply; CI runs `yarn install --immutable` (the frozen gate).

- **Verify**: install with the frozen flag, then the project's scripts. Check
  `.yarn/cache/` or `node_modules/<pkg>/` for changed APIs.

- **Gotchas**: `yarn upgrade --latest` (classic) only touches the current
  workspace; Berry's `yarn up` is project-wide. A `resolutions:` override in
  `package.json` / `.yarnrc.yml` pins a transitive version no update will move —
  raise it explicitly if a dependency you need is stuck behind it.
