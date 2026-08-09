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
4. WRITE THE TEST BEFORE THE FIX, and run it against the unpatched code. It must
   fail, and the failure it prints is what makes everything after it verifiable
   — paste that output into your summary. A test written after the fix passes on
   arrival, so it never demonstrated anything; you cannot tell it from one that
   would pass against the bug too.
   - If you cannot make it fail, stop. You have not found the bug — you have
     found something you believe about it. Go back to step 3.
   - If the error came from a report or review that already states the input,
     the expected result and the actual one, that is the test: transcribe it.
     There is no judgement call left to make and nothing to skip.
   - Where the failure genuinely resists a unit test — a race, an OS-resource
     limit, a real network — say so explicitly and name the observable you
     checked instead, with the command and its output. That is a narrow
     exception, not the default; reach for it when you have tried and can say
     what stopped you, never to skip ahead to the edit.
5. Fix the root cause with the minimal change. Don't refactor, don't
   restructure, don't touch unrelated code. If the fix reveals a second bug,
   note it but stay focused on this one.
6. Run the test again — it must now pass — then run the existing tests for the
   changed code, so the fix is shown to work and shown not to have cost
   anything.
7. Report: what the root cause was, what you changed and why, the failing output
   from step 4 and the passing result from step 6. If the fix is partial or has
   known limitations, say so.
