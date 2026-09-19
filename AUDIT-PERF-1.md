# Performance Audit #1 — sui

Scope: agent loop, context assembly, provider transport, tools, mission
orchestration, TUI render loop, web cache, startup. Method: measure →
fix → re-measure; a permanent harness (`tests/perf.rs`) now exists so
future changes can be diffed, not guessed.

Environment: Linux x86_64, release build (`cargo test --release`),
single-shot medians noted where noisy.

## 1. Baseline measurements

Per-turn context pipeline (history of N turns ≈ 4.8KB/turn of mixed
user/assistant/tool traffic):

| turns | ctx size | compile | estimate | fingerprint | body-serialize | total/turn |
|------:|---------:|--------:|---------:|------------:|---------------:|-----------:|
|    10 |   48 KB | 0.018 ms | 0.037 ms | 0.069 ms | 0.053 ms | **0.18 ms** |
|    40 |  192 KB | 0.107 ms | 0.093 ms | 0.311 ms | 0.275 ms | **0.79 ms** |
|   100 |  481 KB | 0.073 ms | 0.259 ms | 0.771 ms | 0.582 ms | **1.69 ms** |

Other baselines:
- `transcript::rows()` on a loaded 712-row transcript: **2.5 ms/frame**
  (≈76 ms CPU/s during a 30 fps stream; ×2 per scroll tick).
- `resolve()` (workspace canonicalize + containment): ~9 µs/call,
  called once per fs tool call.
- `sui --help` startup: instant, 6.3 MB RSS — not a bottleneck.
- `sui export` on a 590 KB journal: ~10 ms — acceptable.
- Mission integration checks: executed **2× per mission** (once gating,
  once formatting the auditor prompt), **4×** with one audit-repair
  round. These are `cargo test`-class subprocesses — the single largest
  wall-clock waste found.

## 2. Cache verdict (the question that mattered)

**The request prefix is already provider-cacheable.** `tests/perf.rs`
`prefix_is_cache_stable` asserts request N's serialized bytes are a
strict prefix of request N+1's — passes. Context layering is correct:
static system → epoch → guidance → append-only history; no dynamic
values (timestamps, run ids, cwd) enter the prefix.

`prompt_cache_key` is plumbed end-to-end (profile → Provider → request
body) but **opt-in per profile** — set it in config or OpenAI-class
providers may still cache heuristically. Actual cache hits can only be
confirmed from provider telemetry (`usage.prompt_tokens_details.
cached_tokens` / DeepSeek `prompt_cache_hit_tokens`), which the journal
records per request — check `cache_read_tokens` in any real run.

**Nothing in this batch changes wire format** — `Compiled` serializes
byte-identical to the old `Vec<Message>` (asserted in the A/B test), so
prefix stability is preserved, not just intended.

## 3. Optimizations applied (commit `42647e6`)

### Agent loop / context

- **`compile()` → borrowed `Compiled` view** (head + history slice, lazy
  Serialize). Eliminates a deep clone of the entire history per turn —
  O(ctx) memcpy + per-String allocation every request. compile now ~0 ms.
- **`estimate_tokens()`** sums field lengths directly instead of
  `serde_json::to_string` per message. 0.26 → 0.001 ms at 100 turns.
- **`request_fingerprint()`** streams serialization into SHA-256
  (`to_writer` over a hash sink). Same digest, no 480 KB intermediate
  String. 0.77 → 0.41 ms at 100 turns.
- **`summarize()`** consumes the `Value` the batch validator already
  parsed — kills a duplicate JSON parse per tool call.

### Mission

- **Integration checks run once**, results reused for the auditor's
  `gate_summary`. Was: run at integration + re-run verbatim for the
  audit prompt (+2× more on repair). On a mission with `cargo test` as
  the check this halves the gate cost; on repair it quarters it.
  Gate journal records are still written once per execution; the
  auditor sees byte-identical content.
- **`agent_limits`** (global TOML + project sui.toml parse) computed
  once in `MissionRt`, passed to every `mk_agent` — was re-read per
  worker/control spawn (4–10× per mission).
- Usage aggregation **prefilters** journal lines on a substring before
  JSON parsing (parsed check stays authoritative).

### Tools

- **`ToolContext.canon_root`** (OnceLock) — `resolve()` realpath()s the
  workspace root once per context, not once per fs call. Per-call saving
  is ~µs locally; meaningful on NFS/overlay workspaces.
- **`read_file`** streams with `BufReader` + reused line buffer and
  `write!` directly into the output — no whole-file `Vec<&str>`, no
  per-line `format!` alloc. (Reads the whole file only to count `total`.)
- **`kill_tree`** is `libc::kill(-pgid, SIGKILL)` — was a `/bin/kill`
  fork+exec per call. `PgGuard` disarms after the timeout/cancel kill —
  no double SIGKILL; group cleanup on normal exit retained (background
  jobs a command leaves behind are still reaped).

### Providers / SSE

- **provider.rs + codex.rs SSE loops**: index-scan complete lines, one
  `buf.drain(..pos)` per chunk — was `find`+`drain` per line (O(lines ×
  buffer) memmove) plus a per-line `String` alloc.
- **codex replay items**: `id`/`status` stripped at capture (once per
  item) instead of mutated out on every request rebuild; the replay
  clone still strips defensively. Dropped a `tool_calls.clone()`.

### TUI

- **`clean()`**: single pre-sized `String` + `push` — was a per-char
  `String`/`&str` alloc through `map().collect()`.
- **`wrapped()` / `tail_rows()`**: wrap only the lines that render;
  tail previews walk source lines from the back. Cleaning stays
  whole-buffer (inputs are bounded) — deliberately kept clean-then-split
  order so an ANSI OSC containing `\n` is removed whole, identical to
  the old eager path.
- **Failure excerpt**: tail-window without collect-all-then-reverse;
  `capped` marker now means "renderable rows remain" exactly.
- **`scroll_by`**: one `rows()` projection per tick (was two over
  identical state — the first capture was discarded by the second).
- **`Buf::insert_str`**: one `Vec::splice` — was per-char `insert`
  (O(paste_len × tail_len) memmove on every paste).
- **Startup keyring reads**: all profile keys + web key read on one
  worker thread under a shared 5 s deadline. The probe was already
  bounded; the per-entry reads were not — a locked/wedged
  secret-service could hang launch N × timeout. Missing entries degrade
  to session-only exactly like NoEntry.

### Web

- **In-flight dedup race fixed**: `Notify` → `broadcast::Sender<()>`.
  Under the old primitive a waiter that observed `InFlight` could have
  the producer's `notify_waiters()` land before its `notified()` was
  registered → stall to the 40 s timeout. Receivers now `subscribe()`
  under the cache lock and `channel(1)` buffers the completion — the
  send can never be missed. Correctness fix that also removes the
  worst-case latency.

## 4. After-numbers

| turns | compile | estimate | fingerprint | serialize | total/turn | Δ vs baseline |
|------:|--------:|---------:|------------:|----------:|-----------:|--------------|
|   100 | ~0 ms | 0.001 ms | 0.41 ms | ~0.98 ms* | **1.40 ms** | **−17%** |

\* `send` measures `json!` wrap + `to_string`; run-to-run noise ±40%.
A dedicated A/B (`compiled_vs_vec_serialize`) shows `Compiled` and
`Vec<Message>` serialize at equal cost after warmup **and produce
byte-identical output** — no wire regression.

- `rows()`: **2.5 → 1.63 ms/frame (−35%)** on the 712-row transcript.
- Per-turn non-network CPU: **1.69 → 1.40 ms (−17%)**; the clone
  elimination also removes ~480 KB of allocations per turn at N=100.
- Mission: eliminated one full pass of `integration_checks` per mission
  (the dominant wall-clock item; exact saving = whatever the project's
  check commands cost, typically seconds-to-minutes).

## 5. Deferred / rejected

| Finding | Decision | Why |
|---|---|---|
| `worktree.rs` sync `Command` inside async mission code | **deferred** | Real (can stall a tokio worker ~ms-scale per git op, occasionally 100 ms+ on merge). Fix = 19 call sites → async or `spawn_blocking`. Scheduled for audit-2 consolidation; needs care around merge-conflict error paths. |
| `export.rs` clones every event `data` into the timeline | deferred | Export is ~10 ms on the biggest real journal; wide mechanical churn for invisible gain. |
| Multiple TOML parses at TUI startup (`profiles`/`agent_names`/`load`/`agent_limits`) | deferred | µs-scale each, once per launch — not user-visible. |
| `capture_anchor`/`fix_anchor` still do full `rows()` per event while scrolled | partial fix | `scroll_by` halved; per-event `fix_anchor` during scrolled streaming still projects. Real fix = per-group mutation seq + spliced projection — a bigger refactor deferred to audit 2. |
| Provider-side single-serialization (hash the actual body bytes) | deferred | Would move fingerprint into `stream_chat`; better fidelity but signature churn for ~0.4 ms/turn. |

## 6. Correctness validation

- `cargo test`: **152 tests, all green** (11 suites incl. mock-provider
  integration, mission, ACP, TUI, PTY).
- `cargo fmt --check`: clean. `cargo clippy --all-targets`: 40 warnings,
  identical to baseline (zero new).
- `prefix_is_cache_stable`: pass — byte-identical leading prefix across
  consecutive requests.
- Wire-format parity proven by assertion in `compiled_vs_vec_serialize`.
- SSE changes verified against edge cases by inspection: partial chunks
  accumulate in `buf`, CRLF trimmed per line, `[DONE]`/terminal handling
  unchanged, unterminated tail preserved across chunks.

## 7. Honest assessment

- Largest user-visible wins: mission check dedup (seconds→minutes per
  mission), TUI frame cost (−35%), web dedup stall removal.
- Per-turn CPU is now ~1.4 ms at 100-turn context — already 3 orders
  below network latency; further shaving has diminishing returns.
  The next real lever is **provider-side cache hits** (enable
  `prompt_cache_key`, confirm via `cache_read_tokens`), not local CPU.
- Known remaining structural cost: TUI `rows()` full re-projection per
  frame/event while scrolled — needs a per-group render cache.
