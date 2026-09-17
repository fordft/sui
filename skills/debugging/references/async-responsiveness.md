# Async responsiveness (TUI/daemon liveness)

Applies when: input feels ignored, UI repaints late, an action needs an
extra keypress, or the app freezes while background work is pending.

## Investigation path

Trace: input event → UI state → permission reply → agent execution → repaint.
At each stage ask what could block, starve, or drop the signal.

Candidate causes (hypotheses, not a diagnosis):

- Blocking call (`recv`, `read`, `join`, `sleep`) inside the executor's
  async context — stalls every task on that thread.
- Frame scheduling tied to the wrong event source — repaint waits for a
  keypress instead of a state change.
- Two readers competing for the same input stream — events split or lost.
- Completion notification missing — waiter never wakes.
- Channel full / unbounded queue — backpressure or silent drops.

## Verify

- Send one approval character without a newline — action proceeds with no
  second keypress.
- An independent timer or spinner keeps animating while the prompt waits.
- Cancel while permission is pending — the wait ends and resources clean up.
- Resize during the operation — layout recovers.

## Do not

- Do not "fix" it by requiring Enter — that hides the starvation.
- Do not add a delay/retry loop and call it responsiveness.
