---
name: fix
description: root-cause and fix a pasted error
---

Diagnose and fix the error whose details were provided as arguments: $ARGUMENTS

1. Parse the error: extract the file path, line number, error message, and any
   stack trace or context. If the error output is incomplete, ask for the full
   output — don't guess.
2. Read the failing file and trace backward from the error site:
   - What function or block contains the error?
   - What inputs reach it — where do they come from?
   - What assumptions does the code make that the failing input violates?
3. Identify the root cause — not the symptom. A `NullPointerException` is not
   the cause; the cause is what allowed a null to reach that point. State it in
   one sentence before touching any code.
4. Fix the root cause with the minimal change. Don't refactor, don't
   restructure, don't touch unrelated code. If the fix reveals a second bug,
   note it but stay focused on this one.
5. Verify the fix: reproduce the original failure (if possible) and confirm it
   no longer occurs. Run any existing tests for the changed code.
6. Report: what the root cause was, what you changed and why, and the
   verification result. If the fix is partial or has known limitations, say so.
