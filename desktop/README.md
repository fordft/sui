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
- **Project navigation:** search saved projects by name, path, or host with
  Find a project (⌘/Ctrl+K). Filtering the list keeps the current project open.
- **Starting and following work:** starter cards fill an editable draft without
  sending it. The composer shows the connected model, even in a narrow window.
  Jump to latest returns to the end of a long conversation when you scroll up.
- **Model picker:** click the model badge above the composer to search profiles
  configured on the workspace host (local or SSH). The current profile is marked.
  Selecting another profile starts a new chat and preserves the draft; the previous
  chat remains in recent chats. Switching is disabled while a turn runs. Use
  Configure a model to add another profile through the connection screen.
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
- **Shortcuts** (⌘ on macOS, Ctrl elsewhere): B sidebar, J preview, K project
  search, N new chat, `,` appearance.

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
The same workflow runs the real Electron interface test on Linux, including
project search, starter drafts, transcript navigation, annotations, and the
minimum window size; Desktop publishing requires that test to pass too.
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
installation location; `SUI_TAG=v0.11.7 sh install-desktop.sh` pins a release.
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

1. Choose **This Mac / This computer → Choose a folder** to use the system's
   folder chooser. For SSH, select a host alias imported from `~/.ssh/config`
   (including `Include` files), or choose **Enter another host** for a new
   `user@host`. **Reload SSH config** picks up changes without restarting SUI.
   **Connect & choose folder** opens a folder browser on that host, starting
   at your home directory. Open subfolders, go Up or Home, filter names, or
   show hidden folders, then choose **Use this folder → Open workspace**.
   Browsing does not start the agent and does not require SUI on the host.
   Recent folders are offered separately for each host; a manual path remains
   available. SSH uses your normal user, port, keys, agent and jump-host
   configuration. New host keys are saved on first connection; changed keys
   are rejected. Password/passphrase prompts are noninteractive; unlock your
   key in ssh-agent or macOS Keychain before connecting. If needed, choose
   **Install or update SUI on SSH host** in the dialog. It runs the repository's
   checksum-checking `install.sh` over SSH on that host.
2. Choose a saved model profile from the host's dropdown, or choose **Add a
   provider**. For **ChatGPT**, click **Continue with ChatGPT**, approve ChatGPT
   plan usage in your system browser, choose an available model, then click
   **Use this model**. No API key is needed. **OpenAI API** uses an API key and
   its preset Responses endpoint; **Ollama** uses a local endpoint without a key.
   Other compatible Chat Completions and Responses endpoints can be entered too.
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
the release must include `desktop-bridge`, `desktop-setup`, and `desktop-provider`
for this app.
The app first uses `~/.sui/bin/sui`, then `sui` from the remote PATH. API keys
entered in the setup dialog travel through local stdio or
SSH stdin and are stored by SUI in the selected host's owner-only global config.
The app stores project locations and profile names locally, never API keys.

ChatGPT setup follows OpenAI's [Sign in with ChatGPT guide](https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt).
Plan access depends on account eligibility and the permissions actually granted.
The model list uses the account's current catalog: display names are labels,
and only the exact model slug is sent to the public Responses API. This is a
separate `kind = "chatgpt"` transport; existing `codex-oauth` profiles and API-key
profiles keep their existing behavior. ChatGPT plan cost remains unknown.

Credentials stay in protected, atomic, owner-only files under
`$SUI_HOME/chatgpt/` (default `~/.config/sui/chatgpt/`). Each named account profile
keeps its registration separate; use a different profile name to add an account
or workspace. Returning sign-in reuses its issued client ID and verified account.
Signing out clears its tokens and attempts remote revocation, while retaining the
registration. Unconfirmed revocation is shown explicitly. **Manage usage** opens
ChatGPT Settings → Usage and remains available beside the connected model.

For SSH, sign-in completes locally, then the main process transfers the protected
credential record over SSH stdio. Tokens never reach the renderer or argv. The
remote host retains its own installation ID and owns subsequent refreshes;
the local transfer store retains registration metadata without access or refresh
tokens. Both machines need a SUI build supporting `desktop-provider`. This follows
OpenAI's [self-hosted VM flow](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms).
Cancelling sign-in or closing the connection dialog stops the pending login.
Host discovery reads only aliases, without evaluating config commands or
reading key files. OpenSSH interprets the connection settings when you connect.
Wildcard and negated `Host` patterns are settings, not selectable host names.
Discovery is bounded to 64 config files, 256 KiB per file, 2 MiB total and 500
aliases; a warning reports any limit reached. The remote folder browser shows
at most 500 folders per directory and reports truncation; a manual path can
reach folders outside that list. Closing the browser, changing host or closing
the dialog cancels its pending SSH request. An SSH listing times out after
20 seconds and never changes the currently connected project.

## Verification

```bash
npm test
# Linux GUI smoke test, when Xvfb is installed:
xvfb-run -a node_modules/.bin/electron --no-sandbox test/electron-smoke.js
# Workspace flow through a temporary loopback SSH server (openssh-server):
xvfb-run -a node_modules/.bin/electron --no-sandbox test/workspace-smoke.js
```

`npm test` covers the bridge command construction, the markdown renderer, the
companion's math and preference validation, and the page shell (every asset
exists and is packaged, markup stays inside the CSP, every element the scripts
use exists, every preload call is exposed). Host discovery, safe directory
names, listing bounds, SSH failures, timeouts and cancellation are covered too.

The GUI smoke test opens the actual Electron window, connects to a local SUI
bridge, opens a test web page and checks element annotation, then that the
native preview view steps aside for dialogs and leaves with its panel. The
workspace smoke test uses real OpenSSH, temporary keys and an isolated config
to import a host, browse and filter remote folders, then open a workspace in
the native bridge. CI runs both. External SSH hosts, macOS's native chooser UI
and live model requests require separate validation on those systems.
