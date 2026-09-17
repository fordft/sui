# Working on Sui

Sui is a Rust coding harness: native agent loop + optional external
agents (ACP) + mission mode (orchestrator → workers → auditor). The
objective is verified product quality — good engineering that is easy
to perform, easy to verify, and difficult to falsely declare complete.

## Sources of truth

- `DESIGN.md` — architecture, cache-domain model, mission lifecycle
- `README.md` — user-facing behavior (keep it accurate)
- `sui.example.toml` — every config surface; mirror changes here
- `tests/` — mock-PTY TUI tests, ACP/mission/codex harness tests
- `skills/` — embedded engineering lenses (Agent Skills format:
  frontmatter name/description/cues/roles + body + references/).
  src/skills.rs embeds them at build time; selection is deterministic
  cue matching — no model call, no runtime file discovery.

## Build and verify

```bash
cargo fmt --check && cargo test
cargo clippy --all-targets -- -D warnings   # new code must be clean
```

Integration binaries: `sui`, `sui-mission`, `sui-certify`, `sui-acp-bridge`.

## Invariants — do not break these

- **Contract proof is runtime-owned.** `end_turn` is never deliverable
  proof; plans/verdicts/decisions go through `submit_result` and are
  re-validated. Reported tool activity is evidence, not re-executed.
- **Original repos are never touched by missions** — worktrees only.
- **Permissions are independent per surface.** YOLO/session auto-approve
  never enables web egress, ACP agents, or destructive actions.
- **Honest telemetry.** Unknown cost/cache stays unknown — never
  report $0 or fabricated hits. Secrets never enter journals, exports,
  model messages, or spawned-agent environments.
- **Cache discipline.** Stable-to-volatile message order; tool schemas
  frozen per session; new context is appended, never rewritten.
- **Completion is a state, not a turn end.** Finish with the
  `state:`/`verified:`/`unverified:` block (see src/charter.rs).

## Conventions

- Deterministic envelopes: `status:`/`error:`/`denied:` tool text.
- Bounded output everywhere — truncate, never drop silently.
- New provider/agent kinds thread through `ProfileCfg`/`Backend`,
  never as special cases in the agent loop.
