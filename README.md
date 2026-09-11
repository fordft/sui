# sui

Cache-first multi-agent coding harness. Strong model decomposes and audits;
cheap workers implement in isolated git worktrees; deterministic gates decide
what ships. Ships with a terminal UI — configure providers and run tasks from
one screen.

## Install

Pick **one** — Homebrew (recommended, macOS + Linux) or the shell installer.
Don't stack both: they install the same binaries to different prefixes, and
whichever comes first on PATH wins.

### Homebrew (macOS and Linux)

```bash
brew install fordft/tap/sui-ai
sui tui
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

Installs to `~/.sui/bin` (user-owned, no sudo). If `~/.sui/bin` isn't on your
PATH yet, launch with the absolute path:

```bash
~/.sui/bin/sui tui
```

**Prefer to inspect first?**

```bash
curl -LsSf -o sui-installer.sh \
  https://github.com/fordft/sui/releases/latest/download/sui-installer.sh
less sui-installer.sh
sh sui-installer.sh
```

**Name-collision note** — the Mysten blockchain toolchain also ships a `sui`
command. The shell installer always targets `~/.sui/bin` and never touches
another installation; `install.sh` additionally warns when a different `sui`
already owns the name on PATH.

**Rollback** — pin a release:

```bash
SUI_TAG=v0.3.0 sh -c "$(curl -LsSf \
  https://github.com/fordft/sui/releases/download/v0.3.0/install.sh)"
```

## Launch

```bash
sui --version
sui tui
```

First run opens the setup screen. Add a provider — **DeepSeek**,
**OpenRouter**, or a **custom OpenAI-compatible** endpoint — pick models per
role (orchestrator / workers / auditor), then type a task. Credentials are
entered in-app (masked), stored per your choice: environment variable, session
only, or OS keyring where available. No API key is needed to install.

Non-TUI paths still work: `sui` (REPL), `sui "task"` (one-shot),
`sui-mission`, `sui-certify`.

## Update / uninstall

**Update**: rerun the installer. Profiles, role/model selections, OS-keyring
credentials, journals, and run reports under `~/.config/sui` and
`~/.local/share/sui` are preserved — the installer only replaces
`~/.sui/bin/*`.

**Uninstall**: remove `~/.sui/bin` (that's everything the installer wrote).
Your config, credentials, and journals stay; delete `~/.config/sui` and
`~/.local/share/sui` only if you want them gone.

## Platforms & runtime requirements

Prebuilt binaries: macOS (Apple Silicon, Intel), Linux x86-64 and ARM64 —
GNU builds link against **glibc 2.35** (Ubuntu 22.04 baseline); older distros
and Alpine/musl are not covered. Native Windows is not packaged yet (WSL2
works).

Runtime: **git** and **bash** for workspace/mission operations; the target
project's own toolchain for its builds/tests. Headless Linux is supported —
without a desktop credential service, keys fall back to session/env storage.

## Known limitations

- Changes tab shows status + `diff --stat`, not full diffs.
- Per-worker transcript panes are not implemented; journals under
  `~/.local/share/sui/runs` hold the full record.
- Live provider/model behavior is `UNVERIFIED` until `sui-certify` runs
  against real credentials — installation alone proves nothing about a
  provider's tool-calling or cache behavior.
- Comparison clones isolate *repositories*, not the filesystem — a
  misbehaving agent could inspect siblings.
