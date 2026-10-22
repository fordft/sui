// Real Electron + loopback OpenSSH. No external host, user config changes or
// live model requests. Requires openssh-server and an existing /run/sshd.
const { app, BrowserWindow } = require('electron');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const http = require('node:http');
const { spawn, execFileSync } = require('node:child_process');
const { shellQuote } = require('../bridge');
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-ssh-smoke-'));
const fixtureHome = path.join(temporary, 'desktop-home');
const workspace = path.join(temporary, 'projects', 'งาน my app');
const slowWorkspace = path.join(temporary, 'sui-browse-slow');
const bridgeFailure = path.join(temporary, 'bridge-unavailable');
fs.mkdirSync(path.join(fixtureHome, '.ssh'), { recursive: true });
fs.mkdirSync(workspace, { recursive: true });
fs.mkdirSync(slowWorkspace);
fs.mkdirSync(path.join(temporary, 'projects', '.hidden'));
app.setPath('userData', fixtureHome);
app.setPath('home', fixtureHome);
process.env.SUI_HOME = path.join(temporary, 'model-config');
fs.mkdirSync(process.env.SUI_HOME);
fs.writeFileSync(path.join(process.env.SUI_HOME, 'config.toml'), '[profiles.test]\nkind = "ollama"\nmodel = "workspace-test-model"\n', { mode: 0o600 });
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
let sshd, modelServer, failed = false, failNextRequest = false;
const modelRequests = [];

async function freePort() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}

async function run() {
  modelServer = http.createServer((request, response) => {
    if (request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'text/html' });
      response.end('<html><body>SSH preview ready</body></html>');
      return;
    }
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      modelRequests.push(JSON.parse(body));
      if (failNextRequest) {
        failNextRequest = false;
        response.writeHead(401, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: { message: 'Fixture credentials expired' } }));
        return;
      }
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: ' + JSON.stringify({ choices: [{ index: 0, delta: { role: 'assistant', content: 'Screenshot received.' }, finish_reason: 'stop' }] }) + '\n\ndata: [DONE]\n\n');
    });
  });
  await new Promise(resolve => modelServer.listen(0, '127.0.0.1', resolve));
  fs.writeFileSync(path.join(process.env.SUI_HOME, 'config.toml'), `[profiles.test]\nkind = "ollama"\nmodel = "workspace-test-model"\nimage_input = true\nbase_url = "http://127.0.0.1:${modelServer.address().port}/v1"\n`, { mode: 0o600 });
  const remoteHome = path.join(temporary, 'remote-home');
  fs.mkdirSync(remoteHome);
  const port = await freePort();
  for (const key of ['host-key', 'client-key']) execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-f', path.join(temporary, key)]);
  const wrapper = path.join(temporary, 'remote-command');
  const bin = path.join(temporary, 'bin');
  fs.mkdirSync(bin);
  const binary = process.env.SUI_DESKTOP_CLI || path.resolve(__dirname, '../../target/debug/sui');
  fs.symlinkSync(binary, path.join(bin, 'sui'));
  // Route the standard SUI install location to the built fixture binary too,
  // so an older ~/.sui/bin/sui on the test runner cannot take precedence.
  const rewriteBinary = 's|"$HOME/.sui/bin/sui"|"' + binary.replace(/[\\&|]/g, '\\$&') + '"|g';
  fs.writeFileSync(wrapper, `#!/bin/sh\nPATH=${shellQuote(bin)}:"$PATH"\nSUI_HOME=${shellQuote(process.env.SUI_HOME)}\nHOME=${shellQuote(remoteHome)}\nexport PATH SUI_HOME HOME\ncase "$SSH_ORIGINAL_COMMAND" in *sui-browse-slow*) sleep 1 ;; esac\ncase "$SSH_ORIGINAL_COMMAND" in *desktop-bridge*) if [ -f ${shellQuote(bridgeFailure)} ]; then echo 'Fixture SSH connection closed' >&2; exit 1; fi ;; esac\nsui_fixture_command=$(printf '%s' "$SSH_ORIGINAL_COMMAND" | sed ${shellQuote(rewriteBinary)})\nexec sh -c "$sui_fixture_command"\n`, { mode: 0o700 });
  const serverConfig = path.join(temporary, 'sshd-config');
  fs.writeFileSync(serverConfig, `Port ${port}\nListenAddress 127.0.0.1\nHostKey ${temporary}/host-key\nPidFile ${temporary}/sshd.pid\nAuthorizedKeysFile ${temporary}/client-key.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nForceCommand ${wrapper}\n`);
  sshd = spawn('/usr/sbin/sshd', ['-D', '-e', '-f', serverConfig], { stdio: ['ignore', 'ignore', 'pipe'] });
  let serverErrors = '';
  sshd.stderr.on('data', chunk => { serverErrors = (serverErrors + chunk).slice(-4000); });
  sshd.on('error', error => { serverErrors = error.message; });
  const sshConfig = path.join(fixtureHome, '.ssh', 'config');
  fs.writeFileSync(sshConfig, `Include ${fixtureHome}/.ssh/hosts.conf\n`);
  fs.writeFileSync(path.join(fixtureHome, '.ssh', 'hosts.conf'), `Host sui-smoke\n HostName 127.0.0.1\n Port ${port}\n User ${os.userInfo().username}\n IdentityFile ${temporary}/client-key\n IdentitiesOnly yes\n UserKnownHostsFile ${temporary}/known-hosts\n`);
  // Keep the real user's SSH config untouched. The shim only selects this
  // fixture's config; /usr/bin/ssh still owns auth, host verification and I/O.
  // Preview authentication deliberately takes longer than the old 350ms wait.
  fs.writeFileSync(path.join(bin, 'ssh'), `#!/bin/sh\ncase " $* " in *" -N "*) sleep 0.6 ;; esac\nexec /usr/bin/ssh -F ${shellQuote(sshConfig)} "$@"\n`, { mode: 0o700 });
  process.env.PATH = bin + path.delimiter + process.env.PATH;
  await pause(200);
  assert.equal(sshd.exitCode, null, `fixture SSH server started: ${serverErrors}`);
  fs.writeFileSync(path.join(fixtureHome, 'projects.json'), JSON.stringify([{ kind: 'ssh', host: 'sui-smoke', workspace: path.dirname(workspace), profile: 'test' }]));
  require('../main');
  for (let n = 0; n < 100 && !BrowserWindow.getAllWindows().length; n++) await pause(30);
  const win = BrowserWindow.getAllWindows()[0];
  assert.ok(win);
  await new Promise(resolve => win.webContents.once('did-finish-load', resolve));
  const ui = code => win.webContents.executeJavaScript(code);
  const waitFor = async (expression, message) => {
    for (let n = 0; n < 200; n++) { if (await ui(expression)) return; await pause(30); }
    throw new Error(message + ': ' + await ui('JSON.stringify({connection:document.getElementById("connect-error").textContent,provider:document.getElementById("provider-error").textContent,profiles:document.getElementById("profiles-status").textContent,providerOpen:document.getElementById("provider-dialog").open,kind:document.getElementById("model-kind").value,profile:document.getElementById("setup-profile").value,model:document.getElementById("model-id").value,providerBusy,activeRun:state.activeRun})'));
  };
  await waitFor('state.hello !== null && !connectionBusy', 'saved SSH project reconnects automatically');
  const startupRun = await ui('state.hello.run_id');
  await ui('showDialog()');
  await waitFor('!!document.querySelector(\'#host-choice option[value="sui-smoke"]\')', 'SSH alias imported from Include');
  await ui('document.getElementById("host-choice").value="sui-smoke"; document.getElementById("host-choice").dispatchEvent(new Event("change"))');
  assert.equal(await ui('document.getElementById("host").value'), 'sui-smoke');
  assert.equal(await ui('document.getElementById("connect-button").disabled'), true, 'choose a workspace before opening');
  await ui('document.getElementById("browse-workspace").click()');
  await waitFor('!document.getElementById("choose-folder").disabled', 'real SSH home listing');
  assert.equal(await ui('state.hello.run_id'), startupRun, 'folder browsing does not replace the connected agent');
  assert.ok(fs.existsSync(path.join(temporary, 'known-hosts')), 'SSH verified and stored the loopback host key');
  await ui('document.getElementById("close-folder-browser").click(); document.getElementById("recent-workspaces").selectedIndex=1; document.getElementById("recent-workspaces").dispatchEvent(new Event("change")); document.getElementById("browse-workspace").click()');
  await waitFor('!document.getElementById("choose-folder").disabled', 'recent folder listing');
  assert.equal(await ui('document.getElementById("folder-path").textContent'), path.dirname(workspace));
  assert.equal(await ui('document.querySelectorAll(".folder-row").length'), 1, 'hidden folders are omitted by default');
  await ui('document.getElementById("folder-show-hidden").click()');
  await waitFor('!document.getElementById("choose-folder").disabled && document.querySelectorAll(".folder-row").length===2', 'hidden folders can be shown');
  await ui('document.getElementById("folder-filter").value="งาน"; document.getElementById("folder-filter").dispatchEvent(new Event("input"))');
  assert.equal(await ui('document.querySelectorAll(".folder-row").length'), 1);
  if (process.env.SUI_DESKTOP_SCREENSHOT) { await pause(200); fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-folders.png'), (await win.capturePage()).toPNG()); }
  await ui('document.querySelector(".folder-row").click()');
  await waitFor('!document.getElementById("choose-folder").disabled', 'folder opened');
  assert.equal(await ui('document.getElementById("folder-path").textContent'), workspace);
  await ui('document.getElementById("folder-up").click()');
  await waitFor('!document.getElementById("choose-folder").disabled', 'parent folder opened');
  assert.equal(await ui('document.getElementById("folder-path").textContent'), path.dirname(workspace));
  await ui('document.querySelectorAll(".folder-row")[1].click()');
  await waitFor('!document.getElementById("choose-folder").disabled', 'project folder reopened');
  await ui('document.getElementById("choose-folder").click(); document.getElementById("profile").value="test"; document.getElementById("profile").dispatchEvent(new Event("input"))');
  assert.equal(await ui('document.getElementById("workspace").value'), workspace);
  assert.equal(await ui('document.getElementById("connect-button").disabled'), false);
  await ui('document.getElementById("connect-button").click()');
  await waitFor('state.hello !== null', 'workspace connected over real SSH');
  assert.equal(await ui('document.getElementById("composer-model").textContent'), 'workspace-test-model');
  assert.equal(await ui('state.hello.workspace'), workspace);
  assert.equal(await ui('document.getElementById("connect-dialog").open'), false);
  // Returning to the dialog preserves the recent path; changing the host must
  // invalidate it and cancel a pending listing instead of applying stale data.
  await ui('showDialog()');
  await waitFor('!document.getElementById("refresh-hosts").disabled', 'host list refreshed');
  await ui(`void browseFolders(${JSON.stringify(slowWorkspace)})`);
  await pause(100);
  await ui('document.getElementById("host-choice").value=":manual"; document.getElementById("host-choice").dispatchEvent(new Event("change"))');
  assert.equal(await ui('document.getElementById("workspace").value'), '');
  assert.equal(await ui('document.getElementById("connect-button").disabled'), true);
  await pause(1100);
  assert.equal(await ui('folderListing'), null, 'a cancelled host request cannot repopulate the folder browser');
  assert.equal(await ui('document.getElementById("folder-browser").classList.contains("hidden")'), true);
  assert.equal(await ui('state.hello.model'), 'workspace-test-model', 'browsing and cancellation preserve the connected project');
  await ui('refreshHosts()');
  assert.equal(await ui('document.getElementById("host-label").classList.contains("hidden")'), false, 'reloading preserves the choice to enter a host manually');
  win.setSize(720, 560);
  await pause(400);
  const layout = await ui(`({ width: innerWidth, scroll: document.documentElement.scrollWidth, outside: [...document.querySelectorAll('body *')].filter(el => { const r=el.getBoundingClientRect(); return r.width && r.right > innerWidth+1; }).slice(0, 12).map(el => ({ id: el.id, class: el.className, right: el.getBoundingClientRect().right })) })`);
  assert.ok(layout.scroll <= layout.width, JSON.stringify(layout));
  assert.equal(await ui('document.getElementById("connect-button").getBoundingClientRect().bottom <= innerHeight'), true, 'open workspace remains visible in a small window');
  // Send an actual image over SSH, then reload the renderer and resume the
  // persisted native conversation without entering any connection details.
  await ui('document.getElementById("close-dialog").click()');
  const pixels = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jEn8AAAAASUVORK5CYII=';
  await ui(`{ const data = new DataTransfer(); data.items.add(new File([Uint8Array.from(atob(${JSON.stringify(pixels)}), c => c.charCodeAt(0))], 'mac-screen.png', { type: 'image/png' })); document.getElementById('composer').dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: data })); }`);
  await waitFor('state.images.length === 1 && !state.imagesLoading', 'dropped image draft');
  await ui('document.getElementById("composer-execution").value="mission"; document.getElementById("composer-execution").dispatchEvent(new Event("change"))');
  await waitFor('state.conversationMode === "mission" && state.missionInfo !== null', 'Mission mode available over SSH');
  assert.equal(await ui('document.getElementById("mission-options").classList.contains("hidden")'), false);
  assert.equal(await ui('document.getElementById("run-mission").disabled'), true, 'execution blocked without Git while coordinator chat remains available');
  const originalRun = await ui('state.hello.run_id');
  await ui('document.getElementById("message").value = "SSH screenshot"; document.getElementById("message").dispatchEvent(new Event("input")); document.getElementById("send").click()');
  await waitFor('state.activeRun === null && state.messages > 0', 'image turn over SSH');
  assert.equal(modelRequests.length, 1);
  assert.equal(await ui('document.getElementById("run-mission").disabled'), true, 'chat succeeds but execution stays unavailable until a commit exists');
  assert.ok(JSON.stringify(modelRequests[0].messages).includes('Runtime conversation mode'), 'Enter sends coordinator discussion, not worker execution');
  assert.ok(JSON.stringify(modelRequests[0].messages).includes(pixels), 'remote runtime sends the exact dropped pixels to the model');
  const bookmark = JSON.parse(fs.readFileSync(path.join(fixtureHome, 'connection.json'), 'utf8'));
  assert.equal(bookmark.session, originalRun);
  assert.equal(bookmark.target.host, 'sui-smoke');
  assert.ok(!JSON.stringify(bookmark).includes(pixels), 'connection preferences never store images');
  const loaded = new Promise(resolve => win.webContents.once('did-finish-load', resolve));
  win.webContents.reload(); await loaded;
  await waitFor('state.hello !== null && !connectionBusy && state.hello.history.some(m => m.content.includes("SSH screenshot"))', 'restart restores the same native conversation');
  assert.equal(await ui('document.getElementById("connect-dialog").open'), false, 'restart requires no connection setup');
  assert.equal(await ui('state.conversationMode'), 'mission', 'Mission mode survives native SSH recovery');
  assert.notEqual(await ui('state.hello.run_id'), originalRun, 'native recovery forks the original journal');
  assert.equal(modelRequests.length, 1, 'opening a saved chat does not run the model or replay tools');
  await ui('document.getElementById("message").value = "Use the earlier image"; document.getElementById("message").dispatchEvent(new Event("input")); document.getElementById("send").click()');
  await waitFor('state.activeRun === null && state.messages > 2', 'continued SSH conversation');
  assert.equal(modelRequests.length, 2);
  assert.ok(JSON.stringify(modelRequests[1].messages).includes(pixels), 'image history survives a restart over SSH');
  // The real SSH process takes >350ms to start; load only after forwarding works.
  await ui(`document.getElementById('remote-port').value = '${modelServer.address().port}'; previewRemote()`);
  const preview = win.contentView.children.find(view => view.webContents);
  assert.ok(preview, 'remote preview attached');
  assert.equal(await preview.webContents.executeJavaScript('document.body.textContent'), 'SSH preview ready');
  assert.match(preview.webContents.getURL(), /^http:\/\/127\.0\.0\.1:\d+\/$/);
  assert.notEqual(new URL(preview.webContents.getURL()).port, String(modelServer.address().port), 'preview uses the forwarded local port');
  // A commit made outside Desktop must enable execution without toggling modes.
  for (const args of [['init','-q'],['config','user.email','test@example.invalid'],['config','user.name','Fixture'],['commit','--allow-empty','-qm','base']]) execFileSync('git', ['-C', workspace, ...args]);
  await ui('document.getElementById("refresh-mission").click()');
  await waitFor('state.missionInfo?.ready && !state.missionInfoPending && !document.getElementById("run-mission").disabled', 'Git readiness refreshes after an external commit over SSH');
  assert.equal(modelRequests.length, 2, 'refresh checks Git without invoking a model');
  fs.appendFileSync(path.join(process.env.SUI_HOME, 'config.toml'), `\n[profiles.alternative]\nkind = "ollama"\nmodel = "another-workspace-model"\nimage_input = true\nbase_url = "http://127.0.0.1:${modelServer.address().port}/v1"\n`);
  await ui('document.getElementById("message").value="Keep this draft"; document.getElementById("choose-model").click()');
  await waitFor('document.querySelectorAll(".model-option").length === 2', 'model picker includes alternate profile');
  await ui('[...document.querySelectorAll(".model-option")].find(button => button.textContent.includes("another-workspace-model")).click()');
  await waitFor('state.hello?.model === "another-workspace-model" && !connectionBusy', 'model change over SSH');
  assert.equal(modelRequests.length, 2, 'switching models makes no extra model request');
  assert.ok(await ui('state.hello.continuation.entries.some(e => e.content.includes("SSH screenshot"))'), 'switching preserves conversation text');
  assert.equal(await ui('document.getElementById("message").value'), 'Keep this draft');
  assert.equal(await ui('state.conversationMode'), 'mission');
  await ui('document.getElementById("send").click()');
  await waitFor('state.activeRun === null', 'continued turn after model change');
  assert.equal(modelRequests.length, 3);
  assert.equal(modelRequests[2].model, 'another-workspace-model');
  assert.ok(JSON.stringify(modelRequests[2].messages).includes('SSH screenshot'));
  assert.ok(!JSON.stringify(modelRequests[2].messages).includes(pixels), 'model handoff carries portable text rather than old image/wire state');
  const switchedReload = new Promise(resolve => win.webContents.once('did-finish-load', resolve));
  win.webContents.reload(); await switchedReload;
  await waitFor('state.hello?.model === "another-workspace-model" && !connectionBusy', 'reopen after model change');
  assert.equal(modelRequests.length, 3, 'reopening never reruns a previous turn');
  assert.ok(await ui('state.hello.history.some(e => e.content.includes("SSH screenshot"))'));
  // A terminal authentication error must keep both the native conversation and
  // an unsent newer draft through the actual provider configuration flow.
  failNextRequest = true;
  await ui(`{ const data = new DataTransfer(); data.items.add(new File([Uint8Array.from(atob(${JSON.stringify(pixels)}), c => c.charCodeAt(0))], 'retry.png', { type: 'image/png' })); document.getElementById('composer').dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: data })); }`);
  await waitFor('state.images.length === 1 && !state.imagesLoading', 'failed-turn image draft');
  await ui('document.getElementById("message").value="Retry after provider repair"; document.getElementById("message").dispatchEvent(new Event("input")); document.getElementById("send").click()');
  await waitFor('state.activeRun === null && !!document.querySelector(".recovery-card")', 'terminal provider failure');
  assert.equal(modelRequests.length, 4);
  await ui('document.getElementById("message").value="Keep my newer draft"; [...document.querySelectorAll(".recovery-card button")].find(button => button.textContent === "Configure provider").click()');
  await waitFor('document.getElementById("provider-dialog").open && !document.getElementById("save-model").disabled', 'repair the existing provider');
  await ui('document.getElementById("save-model").click()');
  await waitFor('!document.getElementById("provider-dialog").open && !document.getElementById("connect-button").disabled', 'provider saved');
  assert.equal(await ui('document.getElementById("connect-button").textContent'), 'Resume chat');
  await ui('document.getElementById("connect-button").click()');
  await waitFor('state.hello !== null && !connectionBusy', 'provider repair reconnects');
  assert.ok(await ui('state.hello.history.some(e => e.content.includes("Retry after provider repair"))'), 'provider repair must reopen the failed native conversation');
  assert.equal(modelRequests.length, 4, 'repair never resubmits a failed request');
  assert.equal(await ui('document.getElementById("message").value'), 'Keep my newer draft');
  await ui('[...document.querySelectorAll(".recovery-card button")].find(button => button.textContent === "Restore request to composer").click()');
  assert.equal(await ui('document.getElementById("message").value'), 'Retry after provider repair\nKeep my newer draft');
  assert.equal(await ui('state.images[0].data'), pixels, 'failed images remain available after repairing the provider');
  assert.equal(modelRequests.length, 4);
  await ui('document.getElementById("send").click()');
  await waitFor('state.activeRun === null', 'user sends the restored request');
  assert.equal(modelRequests.length, 5);
  assert.ok(JSON.stringify(modelRequests[4].messages).includes('SSH screenshot'));
  assert.ok(JSON.stringify(modelRequests[4].messages).includes(pixels));
  assert.equal(JSON.stringify(modelRequests[4].messages).split(pixels).length - 1, 2, 'native resume retains the failed image history as well as the explicitly resent attachment');
  assert.equal(await ui('state.lastSubmission'), null, 'successful requests are no longer offered for retry');
  // A failed reconnect must not consume the saved pointer or discard a newer
  // draft. Retry from the same connection form after the host becomes available.
  const beforeOutage = JSON.parse(fs.readFileSync(path.join(fixtureHome, 'connection.json'), 'utf8'));
  await ui(`{ const data = new DataTransfer(); data.items.add(new File([Uint8Array.from(atob(${JSON.stringify(pixels)}), c => c.charCodeAt(0))], 'unsent.png', { type: 'image/png' })); document.getElementById('composer').dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: data })); }`);
  await waitFor('state.images.length === 1 && !state.imagesLoading', 'new unsent image');
  await ui('document.getElementById("message").value="Keep this during an outage"; configureCurrentProvider()');
  await ui('closeProviderSetup()');
  fs.writeFileSync(bridgeFailure, 'unavailable');
  await ui('document.getElementById("connect-button").click()');
  await waitFor('!connectionBusy && !state.hello && document.getElementById("connect-error").textContent.includes("Fixture SSH connection closed")', 'failed SSH reconnect');
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(fixtureHome, 'connection.json'), 'utf8')), beforeOutage, 'failed startup preserves the saved chat pointer');
  assert.equal(await ui('document.getElementById("message").value'), 'Keep this during an outage');
  assert.equal(await ui('state.images[0].data'), pixels);
  assert.equal(await ui('document.getElementById("connect-button").textContent'), 'Resume chat');
  await ui(`setWorkspace(${JSON.stringify(slowWorkspace)})`);
  assert.equal(await ui('document.getElementById("connect-button").textContent'), 'Open workspace', 'recovery context never crosses workspace boundaries');
  await ui(`setWorkspace(${JSON.stringify(workspace)})`);
  fs.rmSync(bridgeFailure);
  await ui('document.getElementById("connect-button").click()');
  await waitFor('state.hello !== null && !connectionBusy', 'retry resumes after the outage');
  assert.ok(await ui('state.hello.history.some(e => e.content.includes("Retry after provider repair"))'));
  assert.equal(await ui('document.getElementById("message").value'), 'Keep this during an outage');
  assert.equal(await ui('state.images[0].data'), pixels);
  assert.equal(await ui('state.lastSubmission'), null);
  assert.equal(modelRequests.length, 5, 'reconnect does not send a draft or replay a successful request');
  // If repairing the profile changes its model, native resume must still
  // refuse incompatible wire state. The user can explicitly continue with text.
  await ui('configureCurrentProvider()');
  await ui('document.getElementById("model-id").value="repaired-workspace-model"; document.getElementById("model-id").dispatchEvent(new Event("input")); document.getElementById("save-model").click()');
  await waitFor('!document.getElementById("provider-dialog").open && !document.getElementById("connect-button").disabled', 'changed model saved');
  await ui('document.getElementById("connect-button").click()');
  await waitFor('!connectionBusy && !state.hello && document.getElementById("connect-error").textContent.includes("recorded profile, model, tools or project guidance changed")', 'incompatible native history refused');
  assert.equal(modelRequests.length, 5);
  assert.equal(await ui('document.activeElement.textContent'), 'Continue with conversation text', 'repair action is focused even in a short window');
  assert.equal(await ui('document.activeElement.getBoundingClientRect().bottom <= document.querySelector("#connect-dialog .dlg-foot").getBoundingClientRect().top'), true, 'repair action is visible above the connection footer');
  if (process.env.SUI_DESKTOP_SCREENSHOT) { await pause(250); fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-repair-continuation.png'), (await win.capturePage()).toPNG()); }
  await ui('[...document.querySelectorAll("#connect-error button")].find(button => button.textContent === "Continue with conversation text").click()');
  await waitFor('state.hello?.model === "repaired-workspace-model" && !connectionBusy', 'explicit portable continuation after profile repair');
  assert.ok(await ui('state.hello.continuation.entries.some(e => e.content.includes("SSH screenshot"))'));
  assert.equal(await ui('document.getElementById("message").value'), 'Keep this during an outage');
  assert.equal(await ui('state.images[0].data'), pixels);
  assert.equal(modelRequests.length, 5);
  await ui('configureCurrentProvider();');
  await ui('closeProviderSetup(); document.getElementById("saved-profile").value="test"; document.getElementById("saved-profile").dispatchEvent(new Event("change"))');
  assert.equal(await ui('document.getElementById("connect-button").textContent'), 'Continue chat', 'choosing another repair profile retains portable context');
  await ui('document.getElementById("connect-button").click()');
  await waitFor('state.hello?.model === "workspace-test-model" && !connectionBusy', 'repair can continue with another saved profile');
  assert.ok(await ui('state.hello.continuation.entries.some(e => e.content.includes("SSH screenshot"))'));
  assert.equal(modelRequests.length, 5);
  await ui('document.getElementById("new-chat").click()');
  await waitFor('state.hello !== null && !connectionBusy && !state.hello.continuation', 'intentional new chat stays fresh');
  assert.equal(await ui('state.hello.history.length'), 0);
  assert.equal(await ui('document.getElementById("message").value'), '', 'New chat has its own empty draft');
  const reopen = async () => {
    const loaded = new Promise(resolve => win.webContents.once('did-finish-load', resolve));
    win.webContents.reload(); await loaded;
    await waitFor('state.hello !== null && !connectionBusy', 'native connection restored after reload');
  };
  const typeDraft = text => ui(`document.getElementById('message').value=${JSON.stringify(text)}; document.getElementById('message').dispatchEvent(new Event('input'))`);
  const attach = async () => {
    await ui(`{ const data = new DataTransfer(); data.items.add(new File([Uint8Array.from(atob(${JSON.stringify(pixels)}), c => c.charCodeAt(0))], 'durable.png', { type: 'image/png' })); document.getElementById('composer').dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: data })); }`);
    await waitFor('state.images.length === 1 && !state.imagesLoading', 'durable image draft');
  };
  // Before the first turn the native journal is not resumable. Local drafts
  // still survive a restart, including a last keystroke before debounce fires.
  await attach();
  await typeDraft('Unsent project plan');
  await reopen();
  assert.equal(await ui('document.getElementById("message").value'), 'Unsent project plan');
  assert.equal(await ui('state.images[0].data'), pixels);
  assert.equal(await ui('document.getElementById("connect-dialog").open'), false);
  assert.equal(modelRequests.length, 5, 'restoring an unsent draft makes no model request');
  await waitFor('!!document.querySelector(".session-row[aria-current=true]")', 'unsent chat appears in recent chats');
  await ui('document.querySelector(".session-row[aria-current=true]").parentElement.querySelector(".chat-edit").click(); document.getElementById("chat-title").value="Pinned project plan"; document.getElementById("chat-pinned").checked=true; document.getElementById("chat-edit-form").requestSubmit()');
  await waitFor('!document.getElementById("chat-edit-dialog").open', 'chat label saved');
  await typeDraft('Last keystroke before closing');
  await reopen();
  assert.equal(await ui('document.getElementById("message").value'), 'Last keystroke before closing');
  await waitFor('document.querySelector(".session-row")?.textContent.includes("★ Pinned project plan")', 'pin and name restored first');
  assert.equal(await ui('[...document.querySelectorAll(".session-row")].filter(r => r.textContent.includes("Pinned project plan")).length'), 1, 'unsent identity migration does not duplicate labels');
  // A fresh chat, an existing draft and a different workspace never share
  // composer state. Opening rows uses their saved profile and conversation.
  await ui('document.getElementById("new-chat").click()');
  await waitFor('state.hello !== null && !connectionBusy && document.getElementById("message").value === ""', 'fresh second draft');
  assert.equal(await ui('state.images.length'), 0);
  await typeDraft('Second chat draft');
  await ui('document.getElementById("chat-filter").value="Pinned project"; document.getElementById("chat-filter").dispatchEvent(new Event("input"))');
  assert.equal(await ui('document.querySelectorAll(".session-row").length'), 1, 'chat search filters titles');
  await ui('document.querySelector(".session-row").click()');
  await waitFor('!connectionBusy && document.getElementById("message").value === "Last keystroke before closing"', 'first draft restores from its row');
  assert.equal(await ui('state.images[0].data'), pixels);
  const originalTarget = await ui('state.target');
  await ui(`connect({ ...state.target, workspace: ${JSON.stringify(slowWorkspace)} })`);
  await waitFor(`!connectionBusy && state.target.workspace === ${JSON.stringify(slowWorkspace)}`, 'other workspace connects');
  assert.equal(await ui('document.getElementById("message").value'), '');
  assert.equal(await ui('state.images.length'), 0);
  assert.equal(await ui('document.querySelectorAll(".session-row").length'), 0, 'local labels stay in their workspace');
  await ui(`connect(${JSON.stringify(originalTarget)})`);
  await waitFor(`!connectionBusy && state.target.workspace === ${JSON.stringify(workspace)}`, 'original workspace connects');
  await waitFor('!![...document.querySelectorAll(".session-row")].find(r => r.textContent.includes("Pinned project plan"))', 'original labels return');
  await ui('[...document.querySelectorAll(".session-row")].find(r => r.textContent.includes("Pinned project plan")).click()');
  await waitFor('!connectionBusy && document.getElementById("message").value === "Last keystroke before closing"', 'original workspace draft preserved');
  await ui('document.getElementById("clear-draft").click()');
  await reopen();
  assert.equal(await ui('document.getElementById("message").value'), '', 'cleared draft does not return after restart');
  assert.equal(await ui('state.images.length'), 0);
  assert.equal(modelRequests.length, 5);
  await typeDraft('Named conversation hello');
  await ui('document.getElementById("send").click()');
  await waitFor('state.activeRun === null && state.sessionHasHistory', 'named conversation gets native history');
  const stableIdentity = await ui('state.hello.conversation_id');
  await typeDraft('Follow up on this named conversation');
  await reopen();
  assert.equal(await ui('state.hello.conversation_id'), stableIdentity);
  assert.equal(await ui('document.getElementById("message").value'), 'Follow up on this named conversation');
  await waitFor('[...document.querySelectorAll(".session-row")].filter(r => r.textContent.includes("Pinned project plan")).length === 1', 'native journal forks share one pinned name');
  assert.equal(modelRequests.length, 6, 'restart never submits the follow-up draft');
  const beforeSaveFailure = await ui('state.hello.run_id');
  const chatDirectory = path.join(fixtureHome, 'chats'), backup = path.join(fixtureHome, 'chats-backup');
  fs.renameSync(chatDirectory, backup); fs.writeFileSync(chatDirectory, 'unavailable storage');
  try {
    await typeDraft('Keep this even when saving fails');
    await ui('document.getElementById("new-chat").click()');
    await waitFor('!connectionBusy && document.getElementById("transcript").textContent.includes("Save the draft before switching chats")', 'failed save blocks navigation');
    assert.equal(await ui('state.hello.run_id'), beforeSaveFailure);
    assert.equal(await ui('document.getElementById("message").value'), 'Keep this even when saving fails');
  } finally { fs.rmSync(chatDirectory); fs.renameSync(backup, chatDirectory); }
  await ui('SuiChats.flush()');
  // Keyboard send must obey the same connection transition guard as buttons,
  // including the asynchronous draft save before the old bridge is replaced.
  await ui(`{
    window.savedDraftFlush = SuiChats.flush;
    let first = true;
    SuiChats.flush = async () => {
      if (first) { first = false; await new Promise(resolve => { window.finishDraftSave = resolve; }); }
      return window.savedDraftFlush();
    };
    connect(state.target);
    void 0;
  }`);
  assert.equal(await ui('connectionBusy && document.getElementById("send").disabled'), true);
  assert.equal(await ui('document.getElementById("composer").inert && document.getElementById("annotation-card").inert'), true, 'draft controls cannot change while switching');
  await ui('document.getElementById("message").dispatchEvent(new KeyboardEvent("keydown", {key:"Enter",bubbles:true}))');
  assert.equal(await ui('state.activeRun'), null, 'Enter cannot start a turn during draft save or connection changes');
  assert.equal(await ui('document.getElementById("message").value'), 'Keep this even when saving fails');
  await ui('SuiChats.flush=window.savedDraftFlush; window.finishDraftSave()');
  await waitFor('!connectionBusy && state.hello !== null', 'connection completes after the draft save');
  assert.equal(await ui('document.getElementById("composer").inert'), false, 'draft editing becomes available after connection');
  assert.equal(await ui('document.activeElement.id'), 'message', 'typing focus returns after the transition guard is released');
  assert.equal(modelRequests.length, 6, 'connection transitions never submit a queued Enter');
  console.log('WORKSPACE_SSH_SMOKE_OK');
}

app.whenReady().then(run).then(() => app.quit()).catch(error => { console.error(error); failed = true; app.quit(); });
app.on('will-quit', () => { modelServer?.close(); sshd?.kill(); fs.rmSync(temporary, { recursive: true, force: true }); if (failed) app.exit(1); });
