- A command run in Windows PowerShell (the Environment section says whether it
  is this session's default shell or one you can pick) must be PowerShell, not
  POSIX shell. Use `$env:NAME` for environment variables, `Set-Location` instead
  of `cd` when needed, `;` for independent commands, and
  `if ($LASTEXITCODE -eq 0) { ... }` when a later native command must run only
  after an earlier one succeeded. Do not use POSIX-only syntax such as `VAR=x`,
  `$VAR`, `cmd && next`, `2>&1`, `$(...)` command substitution, `grep | awk`, or
  `/dev/null`.
