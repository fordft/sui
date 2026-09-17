# Subprocess lifecycle (spawned children, pipelines, daemons)

Applies when: a spawned process hangs, outlives its parent, loses output,
zombies, or dies silently.

## Questions

- Does cancellation reach the actual process — or only the handle?
  (process group vs. single pid; grandchildren; shells in between)
- Can the child block forever writing to a full stdout/stderr pipe
  nobody drains?
- Is the exit status actually collected, or is failure inferred from
  output absence?
- What state does the child leave behind on kill — temp files, locks,
  ports, half-written artifacts?
- Does the environment the child needs survive a scrubbed env?

## Verify

- Start → produce output → stop: process exits, no orphan in `ps`.
- Kill mid-write: parent reports failure honestly, no partial artifact
  treated as complete.
- Child crash: surfaced as an error with its captured stderr, not a hang.
- Timeout path: bounded wait, then terminate, then confirm reaping.

## Do not

- Do not treat "the command returned" as "the work finished correctly".
- Do not leak stderr — a child that can't write stderr can deadlock.
