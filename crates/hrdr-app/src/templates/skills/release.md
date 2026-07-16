---
name: release
description: cut a release — bump version, update changelog, commit, tag, push
args: [patch, minor, major]
---

Cut a release.

1. Preflight: the working tree must be clean and on the branch releases are cut
   from. If there are uncommitted changes or no version field anywhere, stop and
   ask.
2. Pick the version: if $ARGUMENTS names a level, use it. Otherwise derive it
   from what actually changed since the last tag
   (`git describe --tags --abbrev=0`, then `git log <tag>..HEAD`) — a breaking
   change is MAJOR, a backwards-compatible feature is MINOR, a fix or an
   internal change is PATCH. Below 1.0 (`0.y.z`), a breaking change bumps MINOR
   and everything else bumps PATCH. Say which level you chose and why.
3. Bump the version (semver) in the project manifest — `Cargo.toml`,
   `package.json`, `pyproject.toml`, `composer.json`, or a `VERSION` file,
   whichever the project uses. Regenerate the lockfile if one is tracked
   (`cargo generate-lockfile`, `npm install --package-lock-only`, etc., matching
   the project's package manager). Go has no manifest — the tag _is_ the
   version. Otherwise: gemspec, `pom.xml`/`build.gradle`, `.csproj` `<Version>`,
   `mix.exs`, `pubspec.yaml` — wherever this project keeps it; look before you
   assume.
4. If a changelog exists (`CHANGELOG.md` or equivalent): move the entries under
   `## [Unreleased]` to a new `## [X.Y.Z] - <today>` heading, keeping an empty
   `Unreleased` above it, and add the compare link if the file keeps them. If
   `Unreleased` is empty, draft entries from `git log <last-tag>..HEAD` using
   Keep-a-Changelog sections — name the actual APIs and behaviors that changed,
   don't rephrase commit subjects. Don't create a changelog if the project has
   none.
5. Before tagging, the tree must be green: run the project's own tests and lints
   and fix anything red.
6. Commit only the manifest, lockfile, and changelog with the message
   `chore: bump version`. Never skip hooks.
7. Tag the commit `vX.Y.Z` and push the commit and the tag. Never move or reuse
   an existing tag — if `vX.Y.Z` already exists, cut the next version instead.
8. If CI releases on version tags, the push _is_ the release — watch the tag's
   CI run to completion instead of deploying manually, and report whether it
   published.
