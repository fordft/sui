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
[static contract + 12 tool schemas]  lowest mutation
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
| `bash(command, timeout_ms, output)` | workspace cwd, hard timeout, head+tail bound; auto compact recognized successful Cargo output or raw capture |
| `web_search(query)` | bounded results + source IDs; needs `[web]` key |
| `web_fetch(url)` | public http(s) only, SSRF-checked, markdown text |
| `skill(name)` | loads an engineering lens from the prompt index |
| `browser(action, ...)` | managed headless Playwright; loopback-only by default; typed actions |
| `terminal(action, ...)` | real workspace PTY + xterm screen; bounded input/output and lifecycle |
| `view_image(path)` | workspace-confined, bounded image observation for image-capable profiles |
| `inventory(action, query, path, limit)` | read-only current files or Tree-sitter definition locations; bounded, ignore-aware, per-worktree |
| `code_intel(action, path, line, column, limit)` | native Rust definitions/references/diagnostics over managed stdio LSP; local execution gate, workspace-filtered output |
| `code_context(action, query, path, line, limit, max_bytes)` | lexical ranked source candidates or syntax-aware contextual read; current numbered excerpts, hashes, explicit omissions, per-worktree parse cache |
| `read_tool_output(id, offset, max_bytes)` | read-only exact pages of an agent-owned ephemeral original command capture; no command rerun |

Output envelope is deterministic: `status / exit_code / stdout / stderr /
truncated`. Empty stdout → `<empty>`. No conversational prose in envelopes.

Native output compaction is a one-time view transformation after approved
Bash execution and before journaling/history append. Literal Cargo
test/build/check/clippy invocations qualify only on completed exit zero and
untruncated capture. Validated libtest pass records and recognized compilation
progress may be collapsed; diagnostics, totals, ignored records and unknown
lines remain exact. Command execution, live preview, typed outcomes and mission
acceptance gates continue to use the original process behavior. Unsupported
syntax/output formats and every unsuccessful or incomplete capture fall back
to the original envelope. A compact envelope must be strictly smaller including
its own metadata; storage failure also falls back to raw.
Pipe readers report actual EOF; errors and drain deadlines mark the capture
truncated. Reader tasks abort on future drop and are joined on drain timeout,
so unfinished streams cannot masquerade as complete recovery observations.

Each ToolContext owns a lazy memory-only original-output store, bounded to
eight entries / 512 KiB text / 128 KiB per entry. Random context nonce and
checked monotonic IDs prevent handle reuse across agents or restarts. Original
bounded envelopes can be paged with UTF-8 byte offsets without executing any
command. Eviction and restart make handles unavailable; the compact journal
view remains replayable. Whole read responses obey the caller's byte cap.
Byte measurements identify local output reduction only: provider usage and
cache telemetry retain their existing measured/unknown semantics. Stable
concise-prose guidance never rewrites source, user input or existing history.

Inventory walks current files on demand and parses definitions with embedded
Rust, JS/JSX, TS/TSX, Python and Go grammars. It requires no shell/server,
external index, local parse cache, model inference or repository writes.
Definition extraction covers common syntax, including trait signatures,
aliases and direct function/class bindings; full names are bounded by the
row output budget. Worktree roots confine each worker's observations.
Ignore loading uses bounded regular non-symlink files, with `.ignore` over
`.gitignore` over in-workspace `.git/info/exclude`; unsafe rules prune the
affected subtree and report partial coverage. Global rules and Git metadata
outside the workspace are not loaded, including worktree `.git` pointers.
Root `target`/`build`/`dist`/`coverage` exclusions can be bypassed by an
explicit scope, subject to workspace ignore rules and safety checks;
identically named source subdirectories remain visible.
Credential, dependency and metadata exclusions remain mandatory.

Enumeration is charged before filtering/sorting; all source/ignore bytes
read, including rejected input, count toward the input budget and are
reported as `bytes_read`. Oversized source files are rejected by metadata.
The three-second time budget and cancellation are cooperative; cancel
awaits worker completion, with no hard timeout for stalled remote filesystems.
Entry/node/output budgets bound processing and output. Coverage counters
report unsupported/skipped/broken files and incomplete scans separately.
Results enter only append-only tool history; the epoch prefix is never
rebuilt after edits.
This is syntactic navigation, not semantic references or a resolved call graph.
The tool schema is appended after the original ten and frozen for a session;
adding it intentionally invalidates older resume signatures. Inventory fixes
preserve the tool/system signature and its existing resume behavior.

Code context reuses inventory's traversal and guarded source readers,
including fresh ignore policy and mandatory exclusions. It runs no model,
process or network service. A credential-content heuristic withholds whole
files before hashing, parsing, caching or output; accepted excerpts remain
exact. Known token formats and line-local credential assignments are
recognized; incomplete credential literals are withheld. False positives
and missed unusual secrets remain possible.
Search ranks literal term matches in paths,
syntax names and source; these are candidate relevance signals, not resolved
semantic edges or proof of task-context completeness. Read starts from a
file and line, selecting its enclosing definition and bounded structural
context. Unsupported text falls back to line windows. Exact source excerpts
carry 1-based positions and observed-content hashes, with omissions and
coverage flags. The whole output is capped by the caller's byte budget.

Syntax facts can be reused in a bounded in-memory cache owned by one
ToolContext, keyed by canonical workspace, path, language and fresh source
content identity. Enumeration and source/ignore reads remain current on
every call. Cache hits return exact excerpts again: prior observations may
have left the active history during compaction. Local parse counters are
not provider prompt-cache hits. Source hashes identify individual reads,
not an atomic workspace snapshot. The tool schema is appended after
code_intel and frozen per session; results append to history and never
rewrite the repository epoch or stable prefix. Its addition intentionally
changes the Resume signature.

Rust code intelligence uses a Sui-owned stdio client and an installed
`rust-analyzer`/`rust-src` toolchain, without an editor, display, external
agent harness, runtime download or new configuration surface. Each native
agent's canonical workspace/worktree owns a lazy process; no global semantic
cache crosses worktrees. Arguments and output locations use 1-based lines
and Unicode scalar columns, converted to/from LSP UTF-16 internally.
Definition/reference targets are workspace-filtered, and omitted or invalid
locations make the result incomplete. Native analyzer diagnostics are
observations, never compiler/test acceptance proof.

Results expose `project_mode: cargo` for a regular root `Cargo.toml`, or
`project_mode: detached` otherwise. Detached initialization builds one
standalone file's crate graph. Changing that source stops the current
backend and initializes a new graph under the same cancellation and
initialization deadline. Detached results always report incomplete analysis
because coverage across files is unknown. Nested manifests are not selected
automatically; the queried project must be part of the root Cargo workspace
or Sui must run from its Cargo root for project semantics.

Launching the language server passes through the normal local execution
gate. Its fixed initialization and configuration replies disable build
scripts, proc macros and check-on-save; Cargo metadata is locked and offline.
Sui sends no save/execute/format commands, uses private server configuration,
and checks configuration before every launch/use. A descriptor-anchored,
presence-only scan reads no configuration contents and ignores `.gitignore`
and `.ignore`. It counts at most 10,000 entries before filtering, descends
at most 128 levels, and uses a cooperative three-second budget. Inventory's
mandatory metadata, dependency and credential exclusions plus root
`target`/`build`/`dist`/`coverage` directories are skipped; similarly named
nested source directories are inspected. Any visible `rust-analyzer.toml`,
including a symlink or nonregular file, produces `UnsafeConfiguration`.
Other visible symlinks, filesystem errors, exceeded limits or unsupported
safe inspection produce `ConfigCoverageUnknown`; the cached backend closes.
This guard covers the inspected tree between calls, not continuous external
filesystem changes. Rust/Cargo tooling runs with user privileges: guarded
output and disabled build features are not an OS sandbox or a guarantee
that project compiler configuration cannot execute.

Initialization/readiness is limited to 60 seconds, individual queries to
30 seconds, and explicit kill/reap cleanup to three seconds. A valid
`ContentModified` response (`-32801`), or `ServerCancelled` (`-32802`) with
`data.retriggerRequest: true`, retries the same LSP query at most three
additional times with 25-millisecond delays. All attempts share the original
query deadline and cancellation; they add no model/provider request or
document update. Cancellation, timeout or protocol failure discards the
process; a later call initializes afresh. Agent interruption also invalidates
an idle backend during provider waits, cancelled tools and stops between
tools, while successful turns retain the session. Owned-client drop also
kills the process group. Mutating file/shell/terminal tools invalidate the
backend before execution, including mutations
from commands that subsequently fail. Tool source/location reads are capped
at 512 KiB per file and 32 MiB per call; rows at 24 KiB and at most 200.
These limits do not bound rust-analyzer's internal workspace indexing.
Cancellation is registered before initial filesystem reads. Initial source
and result-processing workers share cooperative stop/deadline checks and
are joined on explicit cancellation; pre-LSP cancellation closes an idle
cached backend without interrupting another owner of the service.
Result processing has a cooperative three-second deadline and examines at
most 4,096 records; all remaining records count as omitted. Cancellation
signals the blocking worker, joins it and invalidates the backend; dropping
the caller also signals its worker. Filesystem calls must return before
cooperative cleanup finishes. Source line offsets and Unicode boundaries
are indexed once per call rather than rescanned for each result. Processed
records require valid diagnostic messages/full reports or Location/Link
shapes and ranges; malformed semantic payloads error and reset the backend.

`file_in_project` reports `true`, `false` or `unknown` from a per-file
semantic graph observation. The installed RA `experimental/openCargoToml`
query confirms membership only through a valid regular Cargo manifest inside
the workspace, with ancestors opened without following links. Detached or
unlinked files report false; unsupported membership queries remain unknown.
Membership and the semantic query share the same 30-second deadline and
cancellation. Complete analysis requires Cargo mode, confirmed file
membership, healthy/quiescent server state and no omitted results.
Unlinked files and unknown membership remain partial even when the server
is healthy. Unknown health and empty results cannot establish verified
absence; diagnostics do not replace compiler/test proof.

`code_intel` is appended after inventory and frozen in the native schema
list even when its backend is unavailable. Observations enter appended tool
history, never the stable repository prefix. Its addition changes tool/system
signatures; existing inspection/export remain available, while Resume rejects
older signatures through the existing compatibility check.

UI sessions initialize lazily per native agent. Trusted global `[browser]`
config owns UI consent, package bootstrap, and remote-browser policy;
YOLO/session grants never enable that surface. The embedded JSON-lines
driver takes typed operations, not agent-supplied JavaScript. Pinned npm
dependencies (scripts disabled) live outside repos, including mission
worktrees. Chromium always runs headless. HTTP redirect destinations and
WebSocket endpoints share the browser policy; service workers are blocked.
PTY programs inherit an explicit environment allowlist and have their own
process group. Cancellation/timeouts reset the driver and PTY to prevent
stale responses; agent drop also kills the process groups.

Text user messages retain their old JSON-string shape. Image observations
use standard chat-completions content parts, converted to `input_image`
by the Codex Responses adapter. All sibling tool responses are appended
before image observations. Existing history is never rewritten. Image
bytes are memory-only by default and excluded from journals/exports;
optional screenshot paths are workspace-confined. A trusted profile's
`image_input` capability gates attachment; unsupported profiles receive
an honest unverified-visual result. Image context reservations are
conservative estimates, never fabricated provider usage or cost. Journals
do not reconstruct image history: image-bearing runs cannot claim an
identical replay prefix from text-only journal data.

## Conflict control (mission mode)

- Worktrees = filesystem isolation ONLY. They do not prevent semantic
  collisions.
- Ownership is runtime-enforced: `git diff --name-only` vs `owned_paths`
  before every accepted commit. Violation → reject, or worker emits
  `INTERFACE_CHANGE_REQUEST` to orchestrator.
- Foundation-first: orchestrator defines interfaces → worker implements →
  freeze M0 → parallel workers branch from M0.
- Merge in task-id order; integration checks run once on the combined
  candidate (identical commands on identical state — duplicates audited
  out in the perf pass), with merged-tree acceptance re-runs journaled.
- Orchestration state lives OUTSIDE the repo:
  `~/.local/share/sui/runs/<run-id>/` (mission.json, workers/, events.jsonl).

## Budgets (concrete stop conditions)

- worker: max turns, max output tokens, max wall-clock — stop, don't spiral
- escalation: bounded packet, hard cap — 1 per mission (a worker that
  can't finish after repair gets one control-model shot, then fail)
- concurrency: pool 1 by default, max 2 — a wave only parallelizes on
  exactly 2 independent tasks

## Metrics

Per mission: total/control/worker cost, input/cached/write/output tokens,
cache_fraction (token-weighted), TTFT, wall/parallel/integration/audit time,
worker_attempts, test/audit failures, merge_conflicts, accepted.

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

Native Solo/headless writers record a versioned resume header containing only
identity, workspace/profile/model and hashes binding the wire adapter/options,
system, schemas and project guidance. Credentials are excluded. Native turns
have runtime-owned start/end boundaries; process-lifetime Unix locks prevent
recovery of an active writer. Recovery validates closed turns and paired tool
calls, verifies bounded opaque sidecars, and forks the evidence into a new
private run. No historical tool is dispatched. Identity, request sequence and
context epoch are restored; permissions are resolved from the current launch.
Older journals without a wire signature, image-bearing histories and incomplete
turns remain inspectable/exportable but cannot claim exact session recovery.

TUI Usage counts every attempted native request, including absent usage and
transport failure. Its cache ratio sums only valid, complete, provider-reported
input/cache pairs and reports their request coverage. First observed requests
per session/epoch are separated from subsequent requests. Local prefix/tool/
model/guidance changes are diagnostics, never proof of TTL or routing misses.

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

### Native mission profiles

Every mission role receives a native Profile directly. The shared Agent
loop owns model requests, tools, permissions, context and telemetry;
control roles submit structured results through runtime interception.
Workers run only inside their task worktrees. Native cancellation retains
bounded child process cleanup and terminal RunDone events.

Removed executable-agent tables and acp: UI roles are rejected before
agent launch with migration guidance. User configuration and historical
journals are preserved. Export keeps the read-only legacy event decoder;
there is no executable-agent driver or artifact bridge.

JSON insertion order is an explicit native dependency feature. It preserves
the existing provider/tool fingerprints, wire serialization and native
session replay after removing the SDK that previously enabled it transitively.

### ChatGPT OAuth models (codex-oauth)

A profile with `kind = "codex-oauth"` rides the user's ChatGPT sign-in —
reusing `~/.codex/auth.json` (or Sui's own store written by
`sui auth`, which keeps an independent refresh chain) — and calls
`chatgpt.com/backend-api/codex/responses`, the Responses-API backend the
Codex CLI itself uses.

When a session is discoverable, `config::profiles` auto-registers `codex`
(model defaulting to the Codex CLI's own configured model) — `codex login`
alone makes the profile selectable everywhere; explicit TOML wins.

- **Transport**: Responses API over SSE, `store:false` — assistant items
  (incl. encrypted reasoning) replay verbatim across turns via
  `Message::Assistant.response_items`, never serialized into
  chat-completions bodies. Tools flatten to `function` specs; calls and
  results ride as `function_call` / `function_call_output` items.
- **Tokens**: access token is a JWT (~8-10d); refresh rotates and writes
  back atomically so the shared file with the Codex CLI stays valid; a
  `refresh_token_reused` races the CLI → re-read and retry once. The
  token is sent only to `chatgpt.com`/`auth.openai.com`, never to
  journals or prompts.
- **Honesty**: usage counts are real tokens but subscription-billed —
  cost fields stay unknown, not zero. `sui auth` does PKCE on
  localhost:1455 or `--manual` paste for headless/SSH.

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

Deferred: auto mode-selection, pools >2, persistent repository-wide indexing. Native proactive compaction
uses an append-only summary request and an explicit checkpoint epoch transition.

## TUI

`sui tui` — Ratatui/Crossterm shell over the frozen core. The UI renders
typed events and emits user commands; it never re-implements the agent
loop and never mutates request payloads (tab switches, scrolling, and
status numbers stay out of model context — append-only history and frozen
prefixes are unchanged).

- **First run**: no profiles → Setup opens a provider editor instead of
  demanding hand-edited TOML. Three provider kinds: DeepSeek
  (`api.deepseek.com`), OpenRouter (`openrouter.ai/api/v1`), custom
  OpenAI-compatible. Model entry searches `GET {base}/models` (OpenRouter
  catalog shows ctx/price/tool-claims when published) with a manual-entry
  fallback. The exact request endpoint is previewed; no `/v1` guessing.
- **Roles**: Solo profile; Mission orchestrator/workers/auditor (auditor
  defaults to the orchestrator profile). Any provider may fill any role;
  no profile is hardcoded to a vendor. Worker concurrency 1–2 (cap kept).
- **Keys**: masked entry; env-var or session-only storage by default,
  optional OS keyring when available (falls back cleanly headless).
  Keys never hit journals, chat, or TOML.
- **Screens**: Chat (streaming, multiline, paste, scroll, unicode-safe) /
  Tasks (plan + per-task status) / Changes (files, audit, accepted SHA) /
  Usage (per-agent/model requests + provider-reported cache tokens; each
  token field tracks measurement coverage independently, with `—` for
  unknown and an explicit partial marker; unknown costs never render 0) /
  Settings (providers, roles, workspace,
  worker count, acceptance commands). Sidebar lists agents + run state;
  is hidden by default in Solo and shown in Mission at ≥110 columns
  (30-column allocation with a one-column gutter); Ctrl+B toggles it.
- **Presentation**: slime (black-navy and deep-blue, azure accent) semantic palette by default; `[ui].theme =
  "dark"` selects the previous neutral palette; `[ui].theme =
  "terminal"` uses the terminal's foreground for essential text and ANSI
  borders; selection also uses attributes that survive `NO_COLOR`. Missing
  or unknown theme names resolve to slime. Transcript heading/fence/quote
  styling preserves literal text and row ownership; capped previews expose
  a truncation marker and retain captured details. Dialogs subdue the
  background, and palette/permission heights follow their content.
  Preferences use the existing
  `UiSettings` persistence path and never affect model requests.
- **Motion and pixel art**: `gfx` is a small software rasterizer — an RGBA
  canvas at two pixels per terminal row, folded into cells through `▀` half
  blocks (each half composites over what is already in the cell; `blank_only`
  refuses cells holding text). `hero` draws the slime (signed-distance body,
  gel shading, face, mood props), the glossy wordmark, and ambient bubbles;
  `fx` holds `Anim`, the motion policy, and screen effects (gel backdrop,
  flowing border, shimmer, confetti, dialog fade); `slime` holds moods,
  copy, and the clock. Invariants: (1) a frame is a pure function of
  `(App, clock)` — `draw` only reads `App.anim`, whose timestamps are
  advanced by `fx::observe` from the event loop, and `slime::freeze_clock`
  pins the clock for tests and screenshots; (2) essential text is never
  gated on animation state (the splash fades words in, it never withholds
  them); (3) pixel art requires RGB colours, a truecolor terminal, and no
  `NO_COLOR` — otherwise the text mascot is drawn, and `motion = "off"`
  renders static frames; (4) effects that overlay the transcript draw only
  into blank cells; (5) bytes on the wire are budgeted: backdrop rows share
  one background (words stay contiguous in the byte stream), pixel colours
  snap to coarse steps against their backdrop, and resting motion advances
  in a few discrete poses — an idle screen repaints a handful of cells per
  frame rather than the whole hero. Full motion draws at 20 fps ambient and
  30 fps while something reacts; `calm` draws at ~8 fps.
- **Layout**: `tui::layout::regions` owns header, content, sidebar,
  composer, metadata, and footer geometry. Drawing and resize/input
  anchoring share it. Chat has an unboxed transcript; the composer grows
  from one to six text rows and scrolls internally. `Buf::view` shares
  grapheme/display-width calculations between input painting and caret
  placement; editing moves and deletes whole graphemes.
- **Navigation**: Ctrl+P or the header opens a searchable command palette,
  backed by a bounded command registry and the existing App/Effect paths.
  Ctrl+T cycles the five views; Esc returns to Chat. Opening or dismissing
  the palette retains draft text and transcript focus. Disabled palette
  actions explain why they are unavailable. Permission requests queue
  while a dialog is open, so ordinary typing cannot become approval;
  the palette displays a pending-permission notice.
  Tasks, Changes, and Usage maintain independent bounded offsets over
  wrapped rows. Activity navigation uses the same visibility/grouping
  rules as transcript rendering and reveals its selected target.
- **Control**: permission prompts are in-TUI modals (y/a/n) wired through
  the same Gate; Stop and Ctrl-C fire a shared `Notify` + flag consumed by
  the existing cancellation path (process-group cleanup intact). Terminal
  is restored on quit, error, and panic.
- **Test connection** runs one small live request reporting
  streaming/tools/usage as Verified/Unverified/Unsupported — catalog
  metadata is never treated as end-to-end verification.
- Rendering is event-driven with a ~30fps cap; the chat buffer is bounded
  (journal remains the durable record). Live provider status stays
  `UNVERIFIED` until certified with real credentials.

## Build order

- **v0** fast path: worker loop + 10 tools + journal + context compiler +
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

### Implemented cache/replay boundaries

`ProfileCfg.kind` selects Chat Completions, standard `openai-responses`, or
`codex-oauth`; native profiles carry the resolved transport through every
launcher. Standard and OAuth Responses share a decoder that preserves opaque
reasoning and known cache-read/write zeros. Session identity is frozen on each
provider instance; configured cache-group keys take precedence. Cache options
are not inferred from model names or advertised without backend verification.

Native compaction starts proactively at 80% of the context budget, including
tool schema estimates and completion reserve. It appends a bounded summary
instruction using identical tools and provider settings. A completed text-only
summary must free enough context before a `context_checkpoint` is persisted;
only then is the active projection replaced by the checkpoint and latest text
task. Old journal events remain intact. Failed/interrupted summaries never
execute tools or alter history. The summary is untrusted context, not contract
proof. An already-oversized request cannot be safely compacted through that
same full-context request and still hits the hard guard.

Opaque replay items stay in bounded 0600 content-addressed sidecars in the
0700 run directory. Journals reference their hash/name; exports omit contents.
Replay verifies path, hash and size, and refuses missing or altered state.
Cost estimates partition total input into ordinary, cache-read and cache-write
buckets; estimated or incomplete telemetry/pricing cannot imply zero cost.
