---
name: release
description: cut a release — bump version, update changelog, commit, tag, push
args: [patch, minor, major]
---

Cut a release.

The procedure is the **Releasing** section of your instructions — version
choice, manifest and lockfile, changelog, commit, tag, push, and watching the
tag's run to confirm it published. That section is always in front of a write
agent, and this command deliberately does not restate it: the two copies of it
that used to exist drifted, and what went missing from the always-on one was the
last step — so a release cut by phrase rather than by `:release` stopped at
"pushed".

What `:release` adds on top of it:

1. **Preflight.** The working tree must be clean and on the branch releases are
   cut from. Uncommitted changes, or no version field anywhere, means stop and
   ask rather than deciding for the user.
2. **The level is $ARGUMENTS** when one is given — `patch`, `minor` or `major` —
   and it wins over what the commit range implies. With no argument, derive it
   as the Releasing section says, and say which you chose and why.
