---
name: test
description: write tests for a change and iterate to green
---

Write tests for the current change and iterate until they pass.

1. Identify what was changed: read `git diff` and `git log -1 --stat` to
   understand the scope of the most recent work.
2. Find the project's test framework and conventions — its test directory
   structure, its assertion library, any test utilities or fixtures already
   in use. Match them.
3. Write tests that exercise:
   - The happy path (expected input → expected output).
   - Edge cases the change might introduce (empty, zero, null, unicode,
     concurrent access, error paths).
   - Regression coverage for the bug or gap this change addresses.
4. Run the tests. If they fail:
   - Read the failure output carefully — don't guess.
   - Fix the code or the test, whichever is actually wrong.
   - Re-run. Repeat until green.
5. Never weaken an assertion, widen a tolerance, skip or ignore a case, or
   catch-and-swallow an error — to make a test pass. A test you defeated
   still fails, silently, in production.
6. Report: which tests you wrote, what they cover, and the final test run
   result.
