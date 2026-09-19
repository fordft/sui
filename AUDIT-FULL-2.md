# Full-Codebase Audit #2 — sui

Scope: the entire tree, first line to last — `src/**`, `tests/**`, CI,
docs, install script. Method: 8 parallel read-only audit agents, each
assigned a subsystem slice AND instructed to regression-review the
phase-1 diffs (`42647e6`) in their area. Findings were confirmed by
hand before fixing; speculative or style-only findings were rejected.

Remediation commits: `e27e27d`, `d146180`, `67c1bf3`, `11e6470` (the
bulk batch), plus the phase-1 baseline `42647e6` they re-reviewed.

## 1. What the audit found — and what was done

### Severity-1 (correctness/security — all fixed)

| Finding | Fix |
|---|---|
| `TaskContract.id` flowed into worktree paths, `remove_dir_all`, branch names — `../../x` could delete arbitrary dirs | `validate_shape` now requires `[A-Za-z0-9_-]{1,64}`; traversal separators rejected |
| Project `sui.toml` could set `[agent] auto_approve` — a cloned repo granting itself unattended bash/write | `auto_approve` honored only from flag/global config; project value warns + ignored |
| `prompt_cache_key` resolvable from project config — repo picks another tenant's cache domain | project `prompt_cache_key` ignored + warned; flag/env/global only |
| ACP `run_control` deleted `acp-artifacts/<id>` but session reuse never recreated it → guaranteed mission failure on 2nd control turn | directory recreated unconditionally before each control run |
| `spawn_bounded` hung forever when a descendant held stdout/stderr open (PgGuard can't fire while draining) | direct-child `Done` now kills the process group immediately; drains bounded |
| Mid-batch cancel/finish left later `tool_calls` without paired results → malformed next request, replay loses rows | `skip_tail` dispositions every sibling: ToolDone + paired history + journal record, on all three exit paths (interrupt, submission, stop flag) |
| SSE `from_utf8_lossy` per chunk split multibyte chars into U+FFFD | byte-buffered line scanning in both providers; `\n` can't appear inside a UTF-8 char so lines decode whole |
| Stream ending without `finish_reason` looked like clean completion; `usage: null` counted as telemetry; `usage.complete` meant "object arrived" | trailing line processed; null filtered; `complete` now means the response actually finished; missing `finish_reason` journals a warn |
| Escalation `revised_task` deserialized but never shape-validated — a different id silently orphaned the repair path | substituted into the plan, full `validate_shape` (id rules, disjointness, acyclicity) |
| TUI missions shared `tui-<pid>` — a second mission force-deleted the first's accepted branch | session id now includes the run id — unique per mission |
| Terminal escape injection: permission modal + transcript headers rendered raw model/ACP text | `clean()` applied at every render surface; `ToolDone` carries the sanitized `summary` so consumers never reconstruct raw text |
| `percent_decode` panicked/mangled on `%` + multibyte (OAuth callback parsing) | byte-safe decode; malformed sequences preserved without panic |
| Web layer: IPv6-mapped SSRF bypass, trailing-dot hostname bypass, in-flight cache entry poisoned permanently on cancel, cache cap unenforced, API keys leaked into error chains, `dead` latch never reset between runs | all fixed — mapped IPs resolved before checks, trailing dots normalized, `FlightGuard` cleans up on drop, cap enforced, errors redacted at every egress, `begin_run` resets counters/cache/sources/dead |
| Stale `Hit::Perm` zone could close a *different* modal (`modal.take()` before the id check consumed Help) | zones bind to the ask's id; click only takes the modal when it's that Permission ask |
| Journal write failures went to `eprintln!` — corrupts the TUI alt-screen, invisible to the user | `Journal::set_notice` routes failures into `UiEvent::Error`; latched to one shot |
| Export redactor missed modern key shapes (`sk-proj-`, `sk-ant-`, `sk_live_`, `rk_live_`); `known_secrets` missed provider/web keys; `git diff` args injectable via crafted journal revisions | regex tail allows separators; all credential sources collected; revisions must be rev-safe (no leading `-`, no `..`); journal line reads bounded at 16 MB |
| Mission `run()` panic → report stuck at "running", no RunDone, no journal terminal state | `catch_unwind` on the body select → `Flow::Failed("panic: …")` flows through the normal evidence path |
| Orchestrator/escalation ran with `cfg.repo` as workspace — could write into the user's checkout | dedicated scratch worktree `worktrees/control` at mission base; reads real tree, writes confined + discarded; branch deleted on cleanup |
| `?`-propagation after planning skipped the `S::Failed` journal transition (worktree head, integ add, base_for, reset_hard) | converted to `fail!` — every post-planning failure journals `Failed` + a result record |
| Auditor could commit into the integration worktree; `accepted_sha` would certify unaudited changes | tip pinned before each audit pass, verified after — movement fails the mission |
| `plan.base_commit` validated to resolve but never compared to the actual base — drift silently shifted the merge window | must equal real HEAD or the plan is rejected |
| Merged-tree acceptance re-runs weren't journaled — the auditor's evidence existed nowhere | `gate_summary` takes the journal; every re-run logs a `gate` record |
| Native control turns had no deadline (ACP had `task_timeout`) | symmetric `tokio::time::timeout` |
| ACP teardown waited on stderr EOF — a surviving grandchild held it open → hang | stderr wait bounded by `KILL_GRACE` |

### Severity-2 (robustness — fixed)

- `base_for`, `plan::validate`, `external_acceptance` checkout,
  `fresh_env` all used blocking `std::process::Command` inside async
  contexts → converted to `tokio::process` / `worktree::git_rev`.
- Parallel 2-task waves raced on `.git/worktrees` metadata + ref locks
  → `WRITE_LOCK` serializes the ms-scale git mutations; agent runs stay
  parallel.
- `resolve_ctx` cached the uncanonicalized fallback root forever on a
  transient failure → only successful canonicalizations cache.
- `write_file("")` resolved to the workspace root itself → empty paths
  and directory targets rejected.
- `read_file(limit: 0)` reported `showing: 1-0` → normalized empty
  result.
- Bash timeout error reported the *requested* duration, not the clamped
  one; `kill_tree` lacked a pid>0/i32 guard → both fixed.
- `restore_history` didn't restore `guided` — certify restart appended
  the guidance block twice → fingerprint drift. Restored history now
  implies `guided = true` (byte-identical replay).
- `summarize` on malformed args rendered `bash: ` with an empty field →
  shows `<malformed args: …>` bounded.
- `error_class` missed `codex http` / `response incomplete` prefixes →
  classified correctly.
- Web-key keyring writes discarded results (`let _ = …`) → status line
  reports persist/session-only/failure; profile named `web` no longer
  collides with the Exa key slot (namespaced + legacy-read fallback).
- Permission asks parked only under *another* Permission modal — under
  a picker/provider form they popped stale zones or stranded → parked
  under ANY modal, promoted when any modal closes.
- Bracketed paste stored raw control bytes into the editor → sanitized.
- `acp:` role pick was a no-op; `ModelForRole` wrote in-memory only →
  both resolve/persist correctly.
- Ctrl+C on a `Release` hit could stop *and* quit on one press →
  consumed once.
- Mission-mock global serialization `std::Mutex` held across `.await`
  (11 sites) → `tokio::sync::Mutex`.
- Orphan test used `pgrep -x sleep` then asserted `!contains("sleep")` —
  vacuous (pgrep prints PIDs) → now `pgrep -f "^sleep 30$"` with a reap
  grace loop.
- `estimate_tokens` missed the per-tool-call JSON envelope (~40 B each)
  → added; still a heuristic, not a tokenizer.
- Config writes and journal/run-dir creation didn't force private
  perms → 0600/0700 on unix.
- `UiSettings.acceptance` lacked `#[serde(default)]` — an old `[ui]`
  blob would drop ALL settings → defaulted.
- CI ran tests+build but not the mandated `cargo fmt --check` / clippy
  → both added with `-D warnings`; rustfmt+clippy components requested.
- DESIGN.md stale claims (integration checks per merge, escalation ~3,
  concurrency 3, advisor metrics, 4 tools) → synced to implementation
  (once post-merge, 1/mission, max 2, no advisor, 7 tools).
- `sui.example.toml` lacked the persisted `[ui]` surface → documented.
- Dead-guard refactor in `body()` repair match — misleading identical
  arms collapsed.
- `plan.rs` owned-path convention parsed by hand at 3 sites with two
  different strippers → one `parse_owned` parser; agreement pinned by
  test.

### Phase-1 regression review — verdict

Every phase-1 change was re-audited for semantic drift. Two subtle
ones were caught and fixed in this pass:

1. `clean()` moved before `split('\n')` in one call site — an ANSI OSC
   sequence containing `\n` would have split differently. Restored
   clean-whole-then-split; perf unchanged.
2. The capped-excerpt check ran before the filter — trailing status
   lines falsely set `capped`. Reordered.

Everything else (Compiled wire bytes, SSE index-scan, broadcast dedup,
CapBuf, scroll-by single projection) verified semantically identical.

## 2. Validation

- `cargo test --locked --all-targets`: **all suites green**
  (57 lib, 53 tui_mock, 13 acp_mock, 12 mission_mock, 8, 5 export, 9,
  2, + unit tests — every suite passes).
- `cargo clippy --all-targets --locked -- -D warnings`: **0 warnings**
  (baseline was 40 — now enforced in CI).
- `cargo fmt --check`: clean.
- `cargo check --all-targets`: clean.
- Two mid-pass test failures caught real bugs in this batch (stale Perm
  zone consuming Help; RunDone draining pending asks contrary to the
  pinned invariant) — both fixed, not papered over.

## 3. Comparison with Audit #1 + consolidation decisions

Audit #1 optimized *throughput* (per-turn CPU −17%, frame −35%, mission
check dedup, dedup-race fix). Audit #2 found it held up — the wire
bytes are byte-identical, the cache-stable prefix is preserved, and no
phase-1 optimization needed reverting. What audit-2 added is the class
audit-1 deliberately didn't chase: **failure-path and trust-boundary
correctness** — the things that turn a fast harness into a wrong one.

| Phase-1 choice | Audit-2 verdict |
|---|---|
| `Compiled` borrowed view | Kept — byte-identical serialization proven; the `send` regression was noise (A/B equal after warmup) |
| SSE index-scan + single drain | Kept — extended to byte-level scanning in audit-2 for UTF-8 safety; the index approach made that trivial to add |
| Mission check dedup | Kept — extended: merged-tree re-runs now journaled, so the dedup didn't cost evidence |
| `broadcast` in-flight dedup | Kept — `FlightGuard` added on top for cancel cleanup; primitive choice was right |
| scroll_by single projection | Kept — full projection memo deferred again (see below) |
| `prompt_cache_key` plumbing | Kept, restricted — project config can no longer set it (trust-boundary fix on top) |

### Deferred (deliberate, with reasons)

| Item | Why deferred |
|---|---|
| Transcript `rows()` memoization | 26 mutation sites, no funnel — a missed site renders stale rows. ~5% CPU while streaming doesn't justify a correctness hazard. Revisit if a mutation-seq can be made structural. |
| Codex `response_items` full replay persistence | Needs a journaled-item schema + restart reconstruction — a design change, not a fix. |
| Worktree isolation is advisory | `bash` runs with user privileges — a worker can `git` the main repo. Documented in DESIGN's honest-limits; real containment needs OS-level sandboxing. |
| Escalation sees base snapshot, not the failed task's worktree | Letting it into the task worktree risks its stray writes getting committed by the repair round's `commit_all`. The capsule carries the evidence; reading base is correct context. |
| Export timeline clones each event's `data` | ~10 ms on the biggest real journal — wide churn, invisible gain. |

## 4. Honest assessment

- The audit found real breakage, not style nits: two paths to arbitrary
  filesystem deletion, a silent privilege-escalation vector in project
  config, four distinct hangs, replay/cancellation protocol violations,
  and a credential-leak class. All confirmed findings are fixed and
  gated.
- Performance posture after both phases: per-turn CPU ~1.4 ms at 100
  turns (−17%), frame 1.6 ms (−35%), one full integration-check pass
  eliminated per mission, web dedup stall removed, keyring reads
  deadline-bounded.
- The largest remaining lever is unchanged from audit-1: **provider
  cache hits** (`prompt_cache_key` + `cache_read_tokens` telemetry).
  Local CPU is already orders of magnitude under network latency.
- Trust model is now coherent end-to-end: project files can suggest
  tasks/limits but cannot grant approvals, choose cache domains, set
  endpoints with credentials, or supply credentials.
