---
name: deps_go
description: update Go module dependencies to latest stable
---

Update a Go module's dependencies to their latest stable versions
(`go.mod` + `go.sum`).

- **Detect**: `go.mod` at the module root (the directory containing the
  `module` directive — for a workspace, each `go.mod`; a `go.work` file ties
  them). The module's own version is a git tag, not a manifest field — that's a
  release concern, not a dependency update.

- **Current lockfile**: Go has no separate lockfile — `go.mod` + `go.sum` are
  both manifest and lock. `go get -u ./...` upgrades all direct AND indirect
  dependencies to the newest stable within the Go version's support;
  `go get -u <pkg>` one package. `go list -u -m all` lists what's behind
  (with the newest available).

- **Latest stable**: `go get <pkg>@latest` raises one dependency across majors;
  `go get <pkg>@v1.2.3` pins exactly. After any change, `go mod tidy` prunes
  the now-unused entries and re-syncs `go.sum`, and `go mod verify` checks the
  downloaded modules against `go.sum`.

- **Go-version floor**: a dependency's newest release may require a newer Go
  than the module declares (`go` directive in `go.mod`). Raising `go` in
  `go.mod` is part of the update when the bumps need it — say so, it's a
  toolchain decision.

- **Verify**: `go build ./...`, `go test ./...`, `go vet ./...`, and the
  project's CI commands. Check changed APIs in the module cache
  (`go env GOMODCACHE`, then the unpacked `...@<version>/` copy).

- **Gotchas**: `go get -u` also bumps indirect deps and can be noisy — review
  `git diff go.mod` for what it actually changed. Never hand-edit `go.sum` —
  regenerate with `go mod tidy`. `replace` directives in `go.mod` pin a module
  to a fork/local path that no update moves — bump those explicitly. Yanked
  versions are refused; `@latest` always resolves the newest non-prerelease.
