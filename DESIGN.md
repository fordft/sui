# sui — cache-first multi-agent coding harness

Core principle:

> **Expensive model controls decisions; cheap model controls token volume; deterministic software controls state.**

Anything already sent to the model is immutable until an intentional epoch
boundary. Anything expected to change is pushed as far right as possible.
Anything large and reusable gets a cache boundary. Anything large and
non-reusable is reduced before it enters context.

## Modes

```
SMALL / SERIAL (fast path)          MISSION MODE
worker loop only                    Astra-class orchestrator
  ↓                                    │ decompose → MissionPlan
deterministic validation               │ (disjoint owned_paths)
done                                   ▼
                              ┌────────┼────────┐
                              W1       W2       W3     cheap workers
                              worktree worktree worktree
                              └────────┼────────┘
                                   merge in task-id order,
                                   integration tests after each
                                       │
                              Astra-class auditor (once)
                                       │
                                 PASS ─┴─ FAIL → targeted worker repair
```

Fast path is the default. Mission mode only when the task contains genuinely
independent workstreams (delegation is not automatically cheaper).

## Cache topology — two domains, not one

KV state is model-specific. There is no cross-model shared cache.

```
DOMAIN control (frontier model)     DOMAIN worker (cheap model)
  control contract + tools            worker contract + tools
  frozen repo epoch                   frozen repo epoch
  mission contract                    mission invariants
  ── explicit breakpoint ──           ── explicit breakpoint ──
  orchestrator | auditor tails        worker task spec + append-only history
```

- `prompt_cache_key` namespaced to the worker GROUP, not per-worker.
- No sticky-session/GPU headers — provider handles routing.
- No cache keep-alive pings; TTL policy lives in the provider adapter.
- Model names are CONFIG, never constants. Feature-detect cache options.

## Per-agent prompt layout

```
[static contract + 4 tool schemas]   lowest mutation
[frozen repo epoch / map]
[mission contract | task spec]
── cache breakpoint ──
[append-only history]
[volatile tail]                      highest mutation
```

Never mutate model-visible history within an epoch. Compaction = one
deliberate epoch transition (frozen checkpoint → new empty history), never
rolling truncation.

## Tools (frozen interface)

| tool | contract |
|---|---|
| `read_file(path, offset, limit)` | line-numbered, bounded (≤100 default) |
| `write_file(path, content)` | atomic tmp+rename, workspace-confined |
| `edit_file(path, old_str, new_str)` | exact match; 0→fail, >1→fail ambiguous |
| `bash(command, timeout_ms)` | workspace cwd, hard timeout, head+tail bound |

Output envelope is deterministic: `status / exit_code / stdout / stderr /
truncated`. Empty stdout → `<empty>`. No conversational prose in envelopes.

## Conflict control (mission mode)

- Worktrees = filesystem isolation ONLY. They do not prevent semantic
  collisions.
- Ownership is runtime-enforced: `git diff --name-only` vs `owned_paths`
  before every accepted commit. Violation → reject, or worker emits
  `INTERFACE_CHANGE_REQUEST` to orchestrator.
- Foundation-first: orchestrator defines interfaces → worker implements →
  freeze M0 → parallel workers branch from M0.
- Merge in task-id order; integration tests after each merge for attribution.
- Orchestration state lives OUTSIDE the repo:
  `~/.local/share/sui/runs/<run-id>/` (mission.json, workers/, events.jsonl).

## Budgets (concrete stop conditions)

- worker: max turns, max output tokens, max wall-clock — stop, don't spiral
- escalation: bounded packet, hard cap per task (~3); advisor_call_rate >30%
  means worker model too weak for that workload class
- concurrency: start at 3, adapt on cache_fraction / 429s / merge rework

## Metrics

Per mission: total/control/worker cost, input/cached/write/output tokens,
cache_fraction (token-weighted), TTFT, wall/parallel/integration/audit time,
worker_attempts, advisor_calls, test/audit failures, merge_conflicts, accepted.

Optimize $/accepted-task, not $/token.

SLOs: ≥95% stable-prefix hit rate post-warm; ≥90% token-weighted cached-input
fraction; 0 accepted out-of-scope writes; 100% integration tests before audit.

Miss taxonomy: MODEL_CHANGED, EPOCH_CHANGED, TOOL_SCHEMA_CHANGED,
PREFIX_CHANGED, REASONING_CONFIG_CHANGED, TTL_EXPIRED, PROVIDER_ROUTING,
BELOW_MIN_PREFIX, UNKNOWN.

## Safety posture (v0.1, honest limits)

- **Path guard ≠ sandbox.** `resolve()` blocks `..`, absolute, and symlink
  escapes, but a TOCTOU window remains (openat2/RESOLVE_BENEATH later).
  **`bash` is NOT contained at all** — it runs with your user privileges in
  the workspace cwd. Do not run `-y` against repos you care about.
- **Tool-call batch gate:** a batch executes only when
  `finish_reason == "tool_calls"` AND every call is a known tool with valid
  JSON args. Any failure rejects the whole batch — no partial side effects.
- **Endpoint trust gate:** credentials only flow to trusted endpoints
  (CLI/env/--config/global config/any named profile). A project `sui.toml`
  that redirects elsewhere is rejected outright in non-interactive mode and
  requires explicit `y` interactively. `-y` never bypasses. Authenticated
  requests never follow redirects.
- **Credentials:** bash children get an explicit env allowlist (no API keys,
  no unrelated secrets). Project `sui.toml` cannot inject `api_key` or
  define `[profiles]` — profiles live in user-owned config only.
- **Bounded execution:** max_turns, per-request deadline, per-command
  timeout (process-group kill), context budget = est_tokens + reserve.
- **Cancellation:** Ctrl-C aborts the in-flight request or, mid-tool, kills
  the process group via the same kill+reap path as timeout. Verified: no
  orphans.
- **Telemetry honesty:** usage fields are optional — absent means
  "not reported", never zero. Per-layer prefix hashes are determinism
  diagnostics, not proof of provider cache hits.

## Certification

`sui-certify --profile <name> [--profile <name2>]` drives the production
loop (real provider stream, real tools, real journal) against a generated
fixture workspace: tool continuation, identical replay, changed tail,
append-only growth, journal restart/replay, and deliberate prefix
invalidation. ≤20 requests/profile, one retry on transport errors, results
table + checks + cost estimate written to the run dir. Profiles without
credentials run anyway and report **UNVERIFIED**.

## Mission mode (v0.2, explicitly selected)

`sui-mission --control-profile strong --worker-profile cheap --task "…"`.
Models produce typed artifacts; the **Rust state machine** owns scheduling
and every transition: `Planning → Dispatching → Integrating → Auditing →
Accepted | Failed | Cancelled`, with `Repairing` as a bounded side-state.

- **Orchestrator** (strong profile): decomposes into a validated
  `MissionPlan` — tasks with `id`, `base_commit`, `owned_paths`
  (disjoint by construction), `depends_on`, `acceptance` commands,
  `max_turns`. Delivered via `submit_result` (intercepted tool call);
  invalid payloads get one resubmission, then Planning fails.
- **Workers** (cheap profile, homogeneous): one worktree each under
  `~/.local/share/sui/runs/<mission>/worktrees/`, first user message is a
  bounded task contract — no orchestrator history is copied. Pool = 1 by
  default, max 2, concurrency only on a 2-task independent wave.
- **Validation**: ownership is checked twice — plan-time overlap rejection
  and post-work `git diff` vs `owned_paths`. Acceptance commands run via
  the same bounded process path as `bash` (filtered env, pgid kill).
- **Integration**: passing task branches merge in-order into
  `sui-mission-<id>` (a worktree, never the original checkout).
  `integration_checks` gate the combined candidate. Conflicts escalate.
- **Repair/escalation**: 1 repair round per task (fresh worker session,
  failure capsule only). Escalation = a fresh control session deciding
  retry-with-revised-contract or abort; budget 1 per mission.
- **Audit** (strong profile, separate session): sees objective, contracts,
  integrated diff, runtime-computed gate results, and repair history —
  submits PASS/FAIL + required_fixes. One audit-repair round.
- **Cache families**: control plane (orchestrator, auditor, escalation)
  shares `CONTROL_SYSTEM` + identical tools — one stable strong-model
  prefix; role text lives in the volatile tail. Workers share
  `WORKER_SYSTEM` — one cheap-model prefix; task contracts arrive after it.
- **Failure hygiene**: original checkout is never touched; on failure the
  worktrees stay for inspection, on acceptance they are removed.
  Cancellation drops in-flight work; the process-group guard kills
  children either way (verified: no orphans).

`--compare` is a pilot harness, not a verdict. It runs three strategies —
strong-only, cheap-only, and the mission — each in a **separate disposable
`git clone`** of the same committed base. That is *repository* separation
(no shared refs/history), **not** a filesystem sandbox — agents could still
read sibling env dirs via `bash`, so transcripts need inspection for
cross-strategy access. Strategy order rotates across `--trials N`.
Acceptance is a **fixed external suite** (`--acceptance <cmd>`, frozen
before any strategy runs, applied identically to every candidate);
`--trusted-path <p>` restores named paths (e.g. test dirs) from the
candidate's base commit first, so a candidate can't weaken the tests it's
judged by — trusted tests absent from base should live outside the repo.
Mission-internal checks and audit are supplementary, never the yardstick. Report rows: outcome, external accept, candidate sha,
per-family requests + provider-reported cache tokens, est. cost, elapsed,
repairs, escalations. Failed runs count toward cost. Telemetry
completeness and host RSS are recorded. Cache claims are provider-reported
usage only — `CONTROL_SYSTEM` equality does not by itself prove prefix
reuse (tool schemas are cache-relevant; the report measures, not infers).
Fresh repair sessions trade prior-history reuse for bounded context — kept
deliberately; repair cost/success is measured before any second strategy.

Deferred: TUI, auto mode-selection, pools >2, indexing, compaction.

## Build order

- **v0** fast path: worker loop + 4 tools + journal + context compiler +
  OpenAI-compatible provider + permission gate — done
- **v0.1** certification runner, endpoint trust gate, execution invariants —
  done
- **v0.2** bounded mission mode — done (mock-verified; live runs UNVERIFIED
  until profiles carry credentials)
- **v1** explicit cache-breakpoint adapters, larger pools after serial
  delegation evidence, miss taxonomy

## Non-goals

No per-worker premium auditor. No dynamically rewritten shared prefix. No
orchestration files inside the repo. No model-per-specialty explosion. No
generic sticky-GPU abstraction. No keep-alive pings. No full worker histories
to the auditor. No frontier model typing boilerplate. No embeddings in v1.
