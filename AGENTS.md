# Working on Sui

Sui is a native Rust coding harness: agent loop + mission mode
(orchestrator → workers → auditor). The
objective is verified product quality — good engineering that is easy
to perform, easy to verify, and difficult to falsely declare complete.

## Sources of truth

- `DESIGN.md` — architecture, cache-domain model, mission lifecycle
- `README.md` — user-facing behavior (keep it accurate)
- `sui.example.toml` — every config surface; mirror changes here
- `tests/` — mock-PTY TUI tests, native mission/codex harness tests
- `skills/` — embedded engineering lenses (Agent Skills format:
  frontmatter name/description/cues/roles + body + references/).
  src/skills.rs embeds them at build time; selection is deterministic
  cue matching — no model call, no runtime file discovery.

## Build and verify

```bash
cargo fmt --check && cargo test
cargo clippy --all-targets -- -D warnings   # new code must be clean
```

Integration binaries: `sui`, `sui-mission`, `sui-certify`.

## Pushing to main

- **Every push to `main` must include a fresh version bump.** This applies
  to all changes, including TUI, documentation, and follow-up fixes. Update
  the Sui package version in `Cargo.toml` and its entry in `Cargo.lock` together.
- Fetch `origin/main` before pushing and confirm the version being pushed is
  newer than the version on `origin/main`. A bump from an earlier push does
  not cover later pushes; one bump may cover all commits in the same push.
- Run the required checks on the final versioned state, include the version
  bump in the same push as the changes, and report the new version afterward.

## Invariants — do not break these

- **Contract proof is runtime-owned.** `end_turn` is never deliverable
  proof; plans/verdicts/decisions go through `submit_result` and are
  re-validated. Reported tool activity is evidence, not re-executed.
- **Original repos are never touched by missions** — worktrees only.
- **Permissions are independent per surface.** YOLO/session auto-approve
  never enables web egress or destructive actions.
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
- New provider transports thread through `ProfileCfg`/`Transport`,
  never as special cases in the agent loop.
- TUI motion (`src/tui/{gfx,hero,fx,slime,runner}.rs`) is presentation only:
  `draw` stays a pure function of `(App, clock)` (pin it with
  `slime::freeze_clock` in tests), essential text never depends on
  animation state, and backgrounds stay constant along a row — the PTY
  tests grep the byte stream, so per-cell gradients split words. The lane
  slime (`runner.rs`) lives in rows the layout reserves (`Regions.lane`) —
  never overlay a slime on transcript rows.
