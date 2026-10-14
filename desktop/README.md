# SUI Desktop

SUI Desktop is an app shell for SUI's native Rust agent on macOS and Linux. The
app owns the project list, chat surface, SSH connection and browser preview. The
Rust process on the selected machine owns model requests, tools, permission
decisions, journals and session recovery. The desktop does not reimplement the
agent loop.

## Interface

- **Layout:** a collapsible sidebar (projects and recent chats), the
  conversation, and an optional browser preview panel. Below 1000 px wide,
  opening one of the sidebar or preview closes the other.
- **Themes:** system, light, or dark, from Appearance & companion. Fonts are
  bundled (Inter, SIL OFL); Thai and other scripts use your system fonts.
- **Window chrome:** macOS keeps its traffic lights in the title row, Windows
  draws its caption buttons over it, and Linux keeps the native window frame.
- **Conversation:** assistant text renders markdown (code blocks have Copy);
  tool activity collapses into one work log per burst; approvals appear above
  the composer with Deny / Allow once / Allow session. Enter sends (not while an
  input method is composing), Shift+Enter inserts a newline, and you can keep
  drafting while SUI works.
- **Companion:** a slime that lives in a strip above the composer, so it never
  covers messages on its own. It runs back and forth while SUI works, bounces
  and asks `?` when it needs an approval (the buttons remain the only way to
  approve), cheers or frets when a run ends, naps after 45 seconds, follows your
  cursor, looks down while you type, and can be clicked or picked up. Choose its
  color, accessory, size, and name — or turn it off — in Appearance &
  companion. Motion can be Full, Calm (it stays put), or Off; it defaults to Calm
  when the system asks for reduced motion.
- **Shortcuts** (⌘ on macOS, Ctrl elsewhere): B sidebar, J preview, N new chat,
  `,` appearance.

Appearance choices are stored on this device only and never sent to SUI.

## Development on macOS and Linux

Install Rust, Node.js 22.12+ and npm. From this directory:

```bash
npm ci
npm run dev
```

`npm run dev` builds `target/debug/sui`, installs Electron's runtime if needed,
and opens the app. To create an ad-hoc-signed Mac DMG and ZIP on a Mac:

```bash
npm run dist:mac
```

Artifacts appear in `desktop/dist/`. The GitHub `Desktop` workflow builds both
Apple Silicon and Intel DMG/ZIP files, verifies both bundles' signatures and
architectures, runs the bundled Rust executable, and checks that the packaged
app stays running on Mac runners. It also exercises installation and replacement
with local release artifacts before attaching the packages to a tagged GitHub
Release after the core release is ready.
The app's sidebar checks GitHub Releases for a newer Mac build and can download,
verify, and open its DMG. Drag the app to Applications to finish updating.

**Mac installation without an Apple Developer account (0.11.6 and newer):**
the app and its embedded binaries are ad-hoc signed, with the JIT and library
validation entitlements Electron needs on Apple Silicon. Ad-hoc signatures
verify package integrity; they do not provide Apple Developer ID trust or
notarization. Gatekeeper can still block a downloaded DMG app.

The explicit [Desktop installer](../install-desktop.sh) verifies the release's
SHA-256 checksum, bundle identity, architectures, and signatures before copying
the app to `~/Applications/SUI Desktop.app`. It removes download quarantine from
that copy only, runs the bundled SUI version check, and then replaces an existing
SUI Desktop installation. It leaves the existing app in place if verification
fails and restores it if replacement fails. It needs no sudo or Apple account.

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/fordft/sui/releases/latest/download/install-desktop.sh | sh
open "$HOME/Applications/SUI Desktop.app"
```

To inspect the installer first, download it to a file and run
`sh install-desktop.sh`. An optional absolute directory argument changes the
installation location; `SUI_TAG=v0.11.6 sh install-desktop.sh` pins a release.
Use the same location for later updates. If you installed manually in
`/Applications`, pass `/Applications` when it is writable to your user, or
remove the older copy after installing to `~/Applications`.

The in-app update button still opens a DMG for manual installation. For a
blocked direct download, use System Settings → Privacy & Security → Open Anyway
if macOS offers it, or rerun the explicit installer. Normal Finder first launch
without an exception, and automatic install/restart, still require an Apple
Developer Program account, Developer ID signing, and notarization.

On Linux, `npm run dist:linux` builds an AppImage (x86-64 or ARM64, matching the
machine). It is not built in CI. Native Windows is not supported because SUI does
not ship a Windows binary (use SSH to a Linux host or WSL2). The window chrome
has a Windows path (caption-button overlay) that has not been run on Windows.

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

`npm test` covers the bridge command construction, the markdown renderer, the
companion's math and preference validation, and the page shell (every asset
exists and is packaged, markup stays inside the CSP, every element the scripts
use exists, every preload call is exposed).

The GUI smoke test opens the actual Electron window, connects to a local SUI
bridge, opens a test web page and checks element annotation, then that the
native preview view steps aside for dialogs and leaves with its panel. Live SSH
and model requests need a configured host and credentials; these are separate
from the local smoke test.
