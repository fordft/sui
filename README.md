# sui

Cache-first multi-agent coding harness. A strong model decomposes and
audits; cheap workers implement in isolated git worktrees; deterministic
gates decide what ships. Ships with a mouse-enabled terminal UI —
configure providers, run tasks, and watch every step from one screen.

```bash
sui                 # the TUI — everything happens here
sui --mission       # TUI in mission mode (orchestrator → workers → auditor)
sui --yolo          # auto-approve everything (same flag as the `a` key)
sui "fix the bug"   # headless one-shot — no UI
```

## Install

Pick **one** — Homebrew (recommended, macOS + Linux) or the shell
installer. Don't stack both: they install the same binaries to different
prefixes, and whichever comes first on PATH wins.

### Homebrew (macOS and Linux)

```bash
brew install fordft/tap/sui-ai
sui
```

The `sui-ai` formula installs prebuilt binaries (`sui`, `sui-mission`,
`sui-certify`) — no Rust or compilation needed. Update with
`brew upgrade sui-ai`; remove with `brew uninstall sui-ai` (your config,
credentials, and journals are never touched).

### Shell installer (no Homebrew)

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/fordft/sui/releases/latest/download/sui-installer.sh | sh
```

Checksum-verified alternative (recommended — also checks for a foreign
`sui` on your PATH):

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/fordft/sui/releases/latest/download/install.sh | sh
```

Installs to `~/.sui/bin` (user-owned, no sudo). If `~/.sui/bin` isn't on
your PATH yet, launch with the absolute path: `~/.sui/bin/sui`.

**Prefer to inspect first?**

```bash
curl -LsSf -o sui-installer.sh \
  https://github.com/fordft/sui/releases/latest/download/sui-installer.sh
less sui-installer.sh
sh sui-installer.sh
```

**Name-collision note** — the Mysten blockchain toolchain also ships a
`sui` command. The shell installer always targets `~/.sui/bin` and never
touches another installation; `install.sh` additionally warns when a
different `sui` already owns the name on PATH.

**Rollback** — pin a release:

```bash
SUI_TAG=v0.4.2 sh -c "$(curl -LsSf \
  https://github.com/fordft/sui/releases/download/v0.4.2/install.sh)"
```

## Model planes

Sui speaks to models three ways. Pick per role — solo, orchestrator,
worker, auditor can each use a different one.

### OpenAI-compatible API profiles

DeepSeek, OpenRouter, a local proxy — anything that serves
`/v1/chat/completions`. Configure in the TUI setup screen or
`~/.config/sui/config.toml`:

```toml
[profiles.cheap]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
key_env = "DEEPSEEK_API_KEY"
pricing = { input = 0.28, cached = 0.028, output = 0.42 }   # $/MTok
```

Credentials entered in-app are masked and stored per your choice: OS
keyring, plaintext config, session-only, or env var. On headless machines
with no keyring, the config-file store is the default.

### ChatGPT sign-in (codex-oauth)

Already ran `codex login`? That's all you need — Sui detects
`~/.codex/auth.json` and auto-registers a `codex` profile (model follows
your Codex CLI config). Pick it in Settings → any role, or pass
`--control-profile codex` / `--worker-profile codex` to sui-mission. No
API key, no TOML.

To customize, an explicit profile wins over the auto-registered one —
or use Settings → "+ add provider" → "ChatGPT (Codex OAuth)":

```toml
[profiles.codex]
kind = "codex-oauth"
model = "gpt-5.5"
```

No Codex CLI login? Sign in directly: `sui auth` (browser flow;
`--manual` pastes the callback URL for headless/SSH). This calls the
Codex backend's Responses API — subscription access, not API billing,
so cost fields report unknown rather than zero. Tokens refresh
transparently and never touch journals or prompts.

### External coding agents (ACP)

Run a whole agent — `devin acp`, the Codex ACP adapter — in a mission
role over the Agent Client Protocol. Configured in user-owned config
only, with an explicit trust bit:

```toml
[agents.devin]
command = "devin"
args = ["acp"]
approved = true        # required — Sui never installs or runs unapproved agents
```

External agents own their internal model/tool loop; Sui still owns the
contract, worktrees, ownership checks, acceptance gates, and audit.
Their tool calls are evidence, never re-executed; their `end_turn` is
never proof. Children spawn with an empty environment — provider keys
never leak into them.

## The TUI

First run opens setup — add a profile, pick models per role, type a
task. The main screen puts the conversation first, with a growing multiline
composer and compact header. Completed activity folds to one line
(`3 reqs · 7 tools · 12s`); final answers and failed-step excerpts stay
visible. Headings, fenced code, and quotes get distinct styles; their
original text remains available for selection and details. Expand activity
for bounded previews or open its details. A running timer and explicit
approval state show what is happening; a finished run displays its runtime
outcome without implying verification.

![Sui's home screen in the default slime theme](docs/images/sui-tui-slime.png)

*Rendered from the TUI's own frame buffer at 150×46 with the animation clock
pinned — in a terminal the slime hops, blinks, and follows your typing.*

![Sui's conversation view in the dark theme](docs/images/sui-tui-dark.png)

*Terminal capture using a local mock provider.* More views:
[empty screen](docs/images/sui-tui-empty.png),
[command palette](docs/images/sui-tui-commands.png),
[permission prompt](docs/images/sui-tui-permission.png),
[terminal-native colors on a light background](docs/images/sui-tui-terminal.png).

**Commands and views:** `Ctrl+P` or the clickable header opens a searchable
palette. Type to filter, use `↑↓` and Enter to choose, or Esc to return to
your draft. The palette sizes to its results and keeps the selected row
visible. Chat, Tasks, Changes, Usage, and Settings remain available;
`Ctrl+T` cycles views and Esc returns to Chat. Existing `/solo`, `/mission`,
`/export`, and `/help` commands still work.

Tasks, Changes, and Usage scroll independently with the wheel, arrow keys,
Page Up/Down, and Home/End. Their position indicators show when more content
is available. Usage keeps each agent/model pair separate: `—` means a
measurement is missing, `0` means a reported zero, and `+ (partial)` marks
totals with missing measurements.

Usage also shows a **token-weighted cache percentage**, with measured-request
coverage. Only complete, non-estimated input/cache pairs with cached ≤ input
enter the percentage; failed or missing-usage requests remain in its coverage
denominator. First requests in each observed session/epoch and subsequent
requests have separate rates. The latest epoch, request purpose and first-delta
latency are shown when recorded. Observed model/prefix/tool/guidance/cache-key
changes are local diagnostics; provider TTL/routing causes remain unknown.

**Appearance:** Settings → theme switches between `slime` (default: black and
deep blue with an azure gel slime), `dark`, and `terminal` (your terminal's
foreground, background, and ANSI colors). The preference persists as
`[ui].theme`. No special font is required; `NO_COLOR` is respected. Solo
starts with the sidebar hidden; Mission shows agents and run status at widths
of 110 columns or more. `Ctrl+B` toggles it; Tasks remains accessible on
narrow screens.

**The slime and motion:** the home screen is a small animated world. A
pixel-art slime blinks, hops, follows your typing with its eyes, naps when
you leave it alone, and squishes when you click it; a short splash plays at
launch (any key skips it). While a task runs, the composer border flows,
status text shimmers, and a companion slime keeps you company under short
transcripts; a successful run ends with a burst of bubbles. Empty Tasks,
Changes, and Usage views get a napping slime. Everything here is
presentation: it never reaches model requests, journals, or exports.

- Pixel art needs a truecolor terminal (`COLORTERM=truecolor`). With
  `NO_COLOR`, the `terminal` theme, or a terminal that announces less, Sui
  draws a text slime instead.
- Settings → motion (or *Cycle motion* in the palette) chooses `full`,
  `calm` (~8 fps, no splash, bubbles, or confetti — the default over SSH),
  or `off` (frames are static). It persists as `[ui].motion`; `SUI_MOTION`
  overrides it for one launch.

**Input and keys:** Enter sends · `Ctrl+N` inserts a newline · `↑` recalls
history from an empty composer · Tab focuses activity · `v` opens selected
step details · `Ctrl+R` cycles reasoning · `Ctrl+O` switches Solo/Mission ·
`Ctrl+S` stops · `Ctrl+Q` quits · F1 opens help. The composer grows to six
text rows, then scrolls to keep the cursor visible. Editing respects
Unicode grapheme boundaries. Editing a recalled task makes it your draft;
history navigation no longer replaces that edited text. A draft entered
during a run is kept until you send it after the run finishes. Mode changes
are available after the current run stops.
Ctrl+B, Ctrl+O, Ctrl+R, and F1 also work while activity has keyboard focus.
Changing reasoning visibility keeps the selected activity when it remains visible.

**Mouse** (also over SSH): wheel scrolls, click expands activity or opens
commands, drag selects and auto-copies via OSC52. `Shift+drag` uses native
terminal selection. Toggle capture in Settings → mouse.

Mutating tool calls ask first: `y` approves once, `a` approves for the
session (`AUTO` in the header), `n`/Esc denies. Enter never approves.
Permission previews scroll with `↑↓`, PageUp/PageDown, or the wheel;
decision buttons remain fixed and clickable. `Ctrl+S` and `Ctrl+Q` work
inside prompts. An ask arriving while you type in a dialog is queued;
closing the dialog surfaces it without treating typed text as approval.

## Web research

Native agents get two first-class tools — `web_search` and `web_fetch` —
backed by [Exa's hosted MCP service](https://exa.ai/mcp). Off by default;
enable in **Settings → web research** (`off`/`ask`/`auto`) or
`[web] access = "ask"` in global config. Anonymous access works but is
rate-limited; an optional Exa API key (Settings → exa api key, or
`SUI_EXA_API_KEY`) raises usage.

Search results come back as source IDs (`[S1]`, title, URL, snippet) —
snippets are labeled snippets, not fetched pages. `web_fetch(url)` reads
one source as bounded Markdown; cached hits are labeled with their age.
The policy is independent of tool auto-approve: **YOLO never turns web
access on**, and queries/requested URLs leave the machine. Unsafe
targets — private/link-local/metadata IPs, credential-bearing or
secret-shaped URLs, non-http(s) schemes — are refused before anything is
sent. Per-run request caps apply across all workers in a mission.

## Built-in headless UI testing

Ask Sui to test a web or terminal interface directly. Native agents have
`browser`, `terminal`, and `view_image` tools; Sui opens and owns the
sessions. No monitor, desktop, Xvfb, separate Playwright CLI, or ttyd
server is required, including on Ubuntu over SSH.

- `browser`: open a URL, inspect an accessibility snapshot, click/fill by
  role and exact name or CSS selector, press keys, resize, screenshot, close.
- `terminal`: start a real program with arguments in the workspace,
  type/press keys, resize, inspect the interpreted terminal screen,
  screenshot, close. ANSI cursor movement and alternate screens are rendered
  with xterm, rather than treated as a raw output log.
- `view_image`: send a workspace PNG/JPEG/WebP (up to 4 MiB) as image input.

On first use, independent UI consent covers downloading **pinned**
Playwright/xterm packages and Chromium into the user cache. Later sessions
reuse the cache. The server needs **Node.js 20+, npm, and Chromium system
libraries**; Sui does not install privileged OS packages. No project
dependencies are modified. Session processes are killed on cancellation,
timeout, close, or agent drop. Each native agent owns its own sessions.

Browser requests and WebSockets are restricted to HTTP(S)/WS(S) on
`localhost`, `127.0.0.1`, and `[::1]`, including redirect destinations.
Service workers and downloads are disabled. To authorize unattended UI
sessions, set `[browser] approved = true` in **global user config**;
`allow_remote = true` separately permits remote traffic, and
`auto_install = false` prevents automatic dependency downloads. Project
config, YOLO, and web-research settings cannot enable these permissions.
This is an automation guard, not an OS sandbox; terminal programs run
with user privileges and a scrubbed environment, like the bash tool.

Image-capable API profiles must declare `image_input = true` in trusted
`[provider]` or `[profiles.<name>]` config. Codex OAuth defaults to image
input. Text-only profiles still use snapshots and can save screenshots,
but receive an explicit **visual review unverified** result. Images stay
in memory unless the tool requests a workspace-relative `path`; image
bytes are never placed in journals or exports. Browser screenshots mask
password fields and elements marked `data-sui-private`; snapshots exclude
these fields too, and fill refuses private/password targets. Use test data.
Screenshots are viewport captures. Check persisted application state as
well as appearance before declaring a flow verified.

The real browser/PTY integration test is opt-in because it can download
dependencies:

```bash
cargo test --test ui_headless -- --include-ignored
```

## Engineering lenses

Native agents carry a small always-on **scan** (outcome · correctness ·
interaction · failure/recovery · security · performance · compatibility ·
maintainability · verification — including concerns the user didn't
name) plus a compact index of engineering guides. At task start, Sui
selects up to three relevant lenses from task cues + project facts
(detected manifests/deps) and injects them into the run — labeled
*runtime guidance, not user requirements*. The `skill` tool loads any
other lens on demand. Guides inform judgment; they never add
requirements, permissions, or scope. Bundled lenses: product-exploration,
debugging (+ async/subprocess references), code-review (+ rust/python/
typescript references), tui-quality, first-run-experience,
release-verification.

## Missions

`sui --mission` or `sui-mission` headless:

```bash
sui-mission --control-profile strong --worker-profile cheap \
  --auditor-profile strong --task "..." 
# external agents in any role:
sui-mission --control-profile codex --worker-agent devin --task "..."
```

The runtime — not the model — owns scheduling, worktrees, contract
validation, ownership checks, acceptance commands, integration, and
bounded repair/escalation. Your original checkout is never modified;
accepted work lands on a `sui-mission-*` branch.

## Resume a native session

Open **Ctrl+P → Recent sessions / Resume**, or type `/resume`, then choose a
recorded session for the current workspace. The searchable list shows up to 30
recent native sessions. Your draft stays in the composer. The conversation,
recorded tool results and Usage return without running any old tool calls.
Saved checks are historical evidence and need rechecking against current code.

Headless/SSH entry points use the same recovery path:

```bash
sui --resume latest "continue the task"
sui --resume <run-id>                  # opens the TUI on a terminal
```

Recovery creates a fresh private run with a copy of the journal and verified
opaque replay sidecars; the source journal stays unchanged. It retains the
native session identity, request sequence and context epoch. Current permission
settings apply; approvals recorded in the old journal never grant permission.
Cache reuse still depends on the provider, its retention and routing.

The recorded profile/model, wire adapter, tool schemas, system contract and
project guidance must still match. An active session, unfinished turn or tool
call, damaged journal, missing/tampered sidecar, or memory-only image observation
is refused explicitly. Recovery is bounded to 32 MiB of journal and 64 MiB of
replay state. Missions use their own lifecycle and cannot be resumed this way.
Sessions recorded before resume headers were introduced remain exportable;
they cannot promise the same request header and are marked unavailable.

## Export a run report

Every run journals to `~/.local/share/sui/runs/<run-id>/` — requests,
messages, tool calls with exit codes, mission gates, audit verdicts:

```bash
sui export --latest              # newest run for the current workspace
sui export --run <run-id>        # a specific run
sui export --run <id> --format json
sui export --run <id> --include-diff   # bounded git diff of accepted work
```

In the TUI: **Settings → export run report**, or `/export` in the input —
works during a run (labeled partial). Reports land at
`~/.local/share/sui/exports/<run-id>/report.md` (mode `0600`), built from
journals with credentials redacted best-effort; missing data shows
"Not recorded"/"Unknown", never invented. **Review before sharing** —
reports contain project code. No API calls needed.

## Config files

- **Global** `~/.config/sui/config.toml` — providers, profiles, trusted
  agents, UI settings. The only place credentials live.
- **Project** `sui.toml` — workspace prefs only. API keys and `[agents]`
  here are *ignored*; a project file that redirects `base_url` to an
  untrusted endpoint is gated interactively and hard-fails headless.
- **State** `~/.local/share/sui/` — journals, run reports, history.

See `sui.example.toml` for the full annotated schema.

If a provider rejects a model name containing picker details such as
`ctx=272000`, `tools`, or `$2.00/$8.00per-M`, check the profile's `model`
in the global config. Earlier builds could save the entire display label.
Replace it with the exact model ID from that provider's catalog, then
restart Sui. Context size, tool support, and pricing are display metadata;
they are never part of the selected model ID. A corrected ID still needs
to be available to your provider account.

## Update / uninstall

**Update**: `brew upgrade sui-ai` or rerun the installer — config,
credentials, and journals are preserved; only `~/.sui/bin/*` is replaced.

**Uninstall**: remove `~/.sui/bin`. Delete `~/.config/sui` and
`~/.local/share/sui` only if you want the state gone.

## Platforms & runtime requirements

Prebuilt binaries: macOS (Apple Silicon, Intel), Linux x86-64 and ARM64 —
GNU builds link against **glibc 2.35** (Ubuntu 22.04 baseline); older
distros and Alpine/musl are not covered. Native Windows is not packaged
(WSL2 works).

Runtime: **git** and **bash** for workspace/mission operations; the
target project's own toolchain for its builds/tests. Headless Linux is
supported — without a desktop credential service, keys fall back to
session/env storage, and `sui auth codex --manual` handles login.

## Cache behavior and long sessions

Profiles default to Chat Completions. Set `kind = "openai-responses"` in a
trusted global profile to use a compatible `/v1/responses` endpoint directly.
This keeps Responses usage fields (including reported zero cache reads and
writes) and encrypted reasoning for tool continuation. It also avoids token
count changes introduced by some gateways' Chat conversion paths. Sui never
subtracts a guessed gateway overhead. `sui-certify` reports a token-weighted
cache rate with measured-request coverage; missing fields remain unknown.
Pricing uses separate uncached, cache-read, and cache-write input buckets.
An incomplete count or required price makes the estimate unknown.

With `[agent] context_compaction = true` (default), the native loop requests a
summary near 80% of its context budget. The request appends a summary instruction
while preserving model, tool schemas, settings and the existing conversation.
A completed bounded summary becomes a journaled context checkpoint in a new
epoch, retaining the latest text task verbatim. Summary generation counts toward
request limits and usage. Tool calls from a summarization request are never
executed. Failed or insufficient summaries preserve the old history; input
already above the hard budget still stops. A summary provides context, not
runtime verification or permission. Set `context_compaction = false` to retain
the original stop-at-budget behavior.

Opaque reasoning for journal replay lives in bounded, owner-only, hash-verified
`replay-*.json` sidecars next to the journal. Sidecar contents are excluded from
exports. Keep them with the journal for exact Responses replay; a missing or
modified sidecar fails replay explicitly. Image-bearing history still cannot
claim exact replay from text-only journals. Gateway/backend caching controls
require separate capability verification; an API-compatible endpoint need not
support explicit breakpoints or public API diagnostics.

## Known limitations

- Changes tab shows status + `diff --stat`, not full diffs.
- Per-worker transcript panes are not implemented; journals under
  `~/.local/share/sui/runs` hold the full record.
- Provider/model behavior is `UNVERIFIED` until `sui-certify` runs
  against real credentials — installation proves nothing about a
  provider's tool-calling or cache behavior.
- Comparison clones isolate *repositories*, not the filesystem — a
  misbehaving agent could inspect siblings.
- codex-oauth rides your ChatGPT subscription — requests count against
  your Codex rate windows, and reuse of `~/.codex/auth.json` assumes the
  file-based store (keyring-stored credentials aren't readable).
