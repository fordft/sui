---
name: debugging
description: Reproduce-first diagnosis for bugs, failures, hangs, flaky behavior, and regressions. Use whenever the task is "X doesn't work" — even when the user names no cause.
cues: [bug, fix, broken, crash, fails, failing, error, hang, freeze, flaky, regression, doesnt-work, not-working, stuck, timeout]
roles: [lead, worker]
---

# Debugging

## Reproduce before diagnosing

Reproduce the symptom through the real interface (test, PTY, command, UI)
before choosing a cause. A stack trace or a passing unit test that never
exercised the path is not reproduction.

## Rank hypotheses by evidence

- Form 2–3 candidate causes from the symptom, ranked by likelihood.
- Pick the cheapest test that discriminates between them.
- Trace the actual path — don't fix the function that *looks* wrong.
- Keep a list of eliminated hypotheses; do not re-test them.

## Verify the fix, not the symptom's absence

- Write the failing test first when the harness supports it.
- Confirm the fix addresses the root cause, not the observable surface.
- Check the fix didn't silence a legitimate error path.

## Do not

- Do not retry the same operation hoping for a different result — after
  bounded attempts, change approach or report a blocker.
- Do not weaken the reproduction or the test to make the bug pass.
- Do not assume moving work to a thread/process fixes lifecycle issues.

## Completion

Report: reproduced cause, the correction, regression evidence. Mark any
behavior you could not exercise as unverified.
