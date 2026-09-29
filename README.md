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
