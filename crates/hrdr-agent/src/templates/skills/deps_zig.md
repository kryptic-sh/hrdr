---
name: deps_zig
description:
  update Zig dependencies (zig build / build.zig.zon) to latest stable
---

Update a Zig project's dependencies under Zig's built-in package manager
(`build.zig.zon` + `zig build`). Zig has no separate package-manager binary and
no `zig update` command — the build system is the package manager.

- **Detect**: `build.zig.zon` (the manifest) next to `build.zig`. The zon is
  manifest AND lockfile in one: each entry under `.dependencies` pins a package
  with `.url` + `.hash` (remote) or `.path` (local). There is no separate
  lockfile to regenerate — the hashes in the zon ARE the pins, and `zig build`
  fetches exactly what the hashes name.

- **Fetch the declared set**: `zig build --fetch` downloads every dependency and
  exits (no build). `--fetch=needed` (default) fetches lazy dependencies as
  needed; `--fetch=all` fetches lazy ones up front. CI runs this before building
  — a dependency whose `.hash` doesn't match its `.url` fails here, which is
  Zig's frozen-lockfile gate.

- **Latest stable**: raise a dependency by pointing it at the new release and
  letting zig recompute the pin:
  - `zig fetch --save <new-url>` — add or update a dependency entry in
    `build.zig.zon`, downloading the tarball and writing the correct `.hash`
    (Zig 0.14+).
  - `zig fetch <url>` — same download, but only prints the hash; paste it into
    the `.hash` field by hand.
  - Edit the `.url` by hand and re-run `zig build --fetch` — zig reports the
    expected `.hash` on mismatch, which you then paste in.
  - A local dependency (`.path`) needs no fetch; update it like any vendored
    code.

- **Verify**: `zig build --summary all`, then the project's steps —
  `zig build test`, `zig build run`, or whatever `zig build --help` lists as
  steps. Check changed APIs in the fetched source (the global package cache
  under `~/.cache/zig` or the local `.zig-cache`, keyed by the `.hash`).

- **Gotchas**: the zon's shape has moved between Zig releases (`.lazy`,
  dependency field names, the top-level `.dependencies` table) — match the
  project's Zig version (`zig version`, or `.min_zig_version` in the zon) rather
  than assuming the latest syntax. `.hash` is a content hash — never
  hand-compute or guess it; only `zig fetch` output satisfies it. A package that
  requires a newer Zig than the project's pinned version is not a "newer
  release" you can take — say so. The community `mason`/`gyro` package managers
  are archived; the built-in one is the standard.
