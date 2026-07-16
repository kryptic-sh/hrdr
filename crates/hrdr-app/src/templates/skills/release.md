---
name: release
description: cut a release — bump version, update changelog, commit, tag, push
args: [patch, minor, major]
---

Cut a release. Bump level: $ARGUMENTS (default `patch`).

1. Preflight: the working tree must be clean and on the branch releases are cut
   from. If there are uncommitted changes or no version field anywhere, stop and
   ask.
2. Bump the version (semver) in the project manifest — `Cargo.toml`,
   `package.json`, `pyproject.toml`, `composer.json`, or a `VERSION` file,
   whichever the project uses. Regenerate the lockfile if one is tracked
   (`cargo generate-lockfile`, `npm install --package-lock-only`, etc., matching
   the project's package manager).
3. If a changelog exists (`CHANGELOG.md` or equivalent): move the entries under
   `## [Unreleased]` to a new `## [X.Y.Z] - <today>` heading, keeping an empty
   `Unreleased` above it. If `Unreleased` is empty, draft entries from
   `git log <last-tag>..HEAD` using Keep-a-Changelog sections — name the actual
   APIs and behaviors that changed, don't rephrase commit subjects. Don't create
   a changelog if the project has none.
4. Commit only the manifest, lockfile, and changelog with the message
   `chore: bump version`. Never skip hooks.
5. Tag the commit `vX.Y.Z` and push the commit and the tag.
6. If CI releases on version tags, the push _is_ the release — watch the tag's
   CI run to completion instead of deploying manually, and report whether it
   published.
