# SUI Desktop

SUI Desktop is a Mac app shell for SUI's native Rust agent. The app owns the
project list, chat surface, SSH connection and browser preview. The Rust process
on the selected machine owns model requests, tools, permission decisions,
journals and session recovery. The desktop does not reimplement the agent loop.

## Development on macOS

Install Rust, Node.js 22.12+ and npm. From this directory:

```bash
npm ci
npm run dev
```

`npm run dev` builds `target/debug/sui`, installs Electron's runtime if needed,
and opens the app. To create an unsigned Mac DMG and ZIP on a Mac:

```bash
npm run dist:mac
```

Artifacts appear in `desktop/dist/`. The GitHub `Desktop` workflow builds both
Apple Silicon and Intel artifacts. macOS may require one-time approval to open
an unsigned local build. Release signing and notarization are not configured.

## Use

1. Add a local project or an SSH host (`user@host`) and an existing workspace
   directory. SSH uses your normal keys and host configuration. New host keys
   are saved on first connection; changed keys are rejected. If needed, choose
   **Install or update SUI on SSH host** in the dialog. It runs the repository's
   checksum-checking `install.sh` over SSH on that host.
2. Select a SUI model profile on that machine. The dialog can save an Ollama,
   OpenAI Responses, or OpenAI-compatible API profile without a terminal.
3. Send a task. The app streams agent output and shows each tool approval with
   Once, Session and Deny choices. Recent chats can be resumed when the native
   replay checks allow it.
4. Open an HTTP(S) URL in the browser panel. For a web app listening on a
   remote machine's loopback address, enter its port under **Remote preview**;
   the app creates an SSH port forward.
5. Choose **Annotate**, click an element or drag an area, then write a comment
   beside the chat and send it. SUI receives the page URL, DOM selector, nearby
   text, viewport and selected bounds with the comment. Page text is labeled
   untrusted. The app does not currently attach browser image bytes to the
   model; visual inspection can be requested through SUI's existing browser
   tool on the project host.

The remote workspace must exist. The remote installation button requires HTTPS
access to GitHub Releases on that host; it installs the latest released SUI, so
the release must include `desktop-bridge` and `desktop-setup` for this app.
The app first uses `~/.sui/bin/sui`, then `sui` from the remote PATH. API keys
entered in the setup dialog travel through local stdio or
SSH stdin and are stored by SUI in the selected host's owner-only global config.
The app stores project locations and profile names locally, never API keys.

## Verification

```bash
npm test
# Linux GUI smoke test, when Xvfb is installed:
xvfb-run -a node_modules/.bin/electron --no-sandbox test/electron-smoke.js
```

The GUI smoke test opens the actual Electron window, connects to a local SUI
bridge, opens a test web page and checks element annotation. Live SSH and model
requests need a configured host and credentials; these are separate from the
local smoke test.
