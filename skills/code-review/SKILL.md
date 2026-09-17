---
name: code-review
description: Counterexample-seeking review of a completed change. Use for audits, self-review before finishing, and "check this" tasks — find a realistic input or condition that breaks it.
cues: [review, audit, check, verify, look-over, correctness, safe, secure]
roles: [lead, auditor, worker]
---

# Code review — seek a counterexample

"Does this look good?" invites rubber-stamping. Instead: find a realistic
input, user action, or environmental condition under which this violates
the acceptance criteria. Reproduce it when possible.

## Order of review

1. What does the change claim to do? Read the diff, not the summary.
2. Which requirement does each hunk satisfy? Unmotivated hunks are findings.
3. Edges: empty/zero/negative, Unicode, huge input, timeout mid-way,
   cancellation, permission denied, missing file, pre-existing data.
4. Contracts: does it preserve public behavior it didn't intend to change?
   Error paths — does a failure produce a clear message or a silent swallow?
5. Concurrency: shared state across awaits, guard lifetimes, ordering.

## Report shape

Per finding: affected behavior · evidence or reproduction · impact ·
proposed correction. Severity: blocker (violates criteria/safety) vs.
minor (maintainability, style with a real cost). Speculative style
opinions are not findings.

## Do not

- Do not review the description — review the candidate.
- Do not list nits as blockers; label everything by real impact.
- Do not accept "tests pass" as proof of the untested behavior.
