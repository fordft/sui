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

Already ran `codex login`? Sui reuses the session — no API key:

```toml
[profiles.codex]
kind = "codex-oauth"
model = "gpt-5.5"
```

Or sign in directly: `sui auth codex` (browser flow; `--manual` pastes
the callback URL for headless/SSH). This calls the Codex backend's
Responses API — subscription access, not API billing, so cost fields
report unknown rather than zero. Tokens refresh transparently and never
touch journals or prompts.

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
task. After that the chat is an **activity transcript**: each submitted
run folds to one line (`3 reqs · 7 tools · 12s`), expandable to bounded
step previews. Failed steps keep their excerpts; pending permissions are
never hidden.

**Mouse** (works over SSH — your local terminal sends the events):
wheel scrolls, click expands groups/steps/tabs, `[y]/[a]/[n]` permission
buttons click, drag selects and auto-copies via OSC52, `Shift+drag` is
native terminal selection. Toggle in Settings → mouse.

**Keys**: `Ctrl+S` stop task · `Ctrl+Q` quit (both work inside permission
prompts) · `Ctrl+R` cycles reasoning display · `↑↓`+Enter navigates
history · `v` opens the step details view · `?` help.

Mutating tool calls ask first: `y` approves once, `a` approves for the
session (`AUTO` badge in the header), `n`/`Esc` denies.

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
