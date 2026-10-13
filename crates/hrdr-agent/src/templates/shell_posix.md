- POSIX `sh` is NOT bash (the Environment section says whether it is this
  session's default shell or one you can pick) — a command run in it must be
  portable POSIX, with no bashisms: `[ … ]` not `[[ … ]]`, `=` not `==` in
  `test`, `.` not `source`, no arrays, no `set -o pipefail`, no process
  substitution (`<(…)`), no `${var,,}`/`${var^^}`. If you genuinely need bash,
  run the command in bash when it is offered, or invoke it explicitly
  (`bash -c '…'`) only once you've confirmed it exists.
