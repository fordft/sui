// Run with: xvfb-run -a electron --no-sandbox test/electron-smoke.js
const { app, BrowserWindow, dialog, ipcMain } = require('electron');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const tempHome = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-desktop-smoke-'));
app.setPath('userData', tempHome);
process.env.SUI_HOME = tempHome;
fs.writeFileSync(path.join(tempHome, 'config.toml'), '[profiles.test]\nkind = "ollama"\nmodel = "test-model"\n[profiles.reasoning]\nkind = "openai-responses"\nmodel = "gpt-6.1-sol"\n[profiles.alternative]\nkind = "ollama"\nmodel = "other-model"\n', { mode: 0o600 });
const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-desktop-workspace-'));
let pickerCalls = 0;
dialog.showOpenDialog = async (_window, options) => {
  assert.deepEqual(options.properties, ['openDirectory']);
  pickerCalls++;
  if (pickerCalls === 1) return { canceled: true, filePaths: [] };
  return { canceled: false, filePaths: [workspace] };
};
// UI-only ChatGPT fixture: native OAuth/JWT/transport and private IPC are
// exercised separately; this never opens a real account or spends plan usage.
let fixtureChatgpt = false, fixtureLogins = 0, fixtureRefreshes = 0;
let fixtureApproval = false, fixtureImages = false, fixtureMission = false, fixtureMissionReject = false, fixtureMissionChecks = 0;
const missionCommands = [];
const approvalCommands = [], imageCommands = [];
const accountModels = [
  { id: 'account-model-slug', name: 'Account model display name', input_modalities: ['text', 'image'], default_reasoning: 'low', reasoning_levels: [{ effort: 'low', description: 'Fast' }, { effort: 'high', description: 'Deep' }] },
  { id: 'second-account-model', name: 'Another account model', input_modalities: ['text', 'image'], default_reasoning: 'medium', reasoning_levels: [{ effort: 'medium', description: 'Balanced' }, { effort: 'xhigh', description: 'Deep' }, { effort: 'ultra', description: 'Codex delegation' }] }
];
const originalHandle = ipcMain.handle.bind(ipcMain);
ipcMain.handle = (channel, handler) => originalHandle(channel, async (event, ...args) => {
  if (channel === 'sui:command' && fixtureMission && args[0].type === 'mission_info') {
    fixtureMissionChecks++;
    setImmediate(() => BrowserWindow.getAllWindows()[0].webContents.send('sui:event', {type:'mission_info',mission:{ready:true,profiles:[{name:'fixture-role',model:'fixture'}]}}));
    return true;
  }
  if (channel === 'sui:command' && fixtureMission && ['send', 'run_mission'].includes(args[0].type)) {
    const command = args[0]; missionCommands.push(command);
    if (fixtureMissionReject && command.type === 'send') {
      setImmediate(() => BrowserWindow.getAllWindows()[0].webContents.send('sui:event', {type:'send_error',run:command.run,message:'fixture rejected'}));
      return true;
    }
    const view = BrowserWindow.getAllWindows()[0].webContents;
    setImmediate(() => {
      if (command.type === 'run_mission') {
        view.send('sui:event', { type: 'mission_started', run: command.run });
        view.send('sui:event', { type: 'mission_state', state: 'Working' });
        view.send('sui:event', { type: 'delta', run: command.run, agent: 'w-a', text: 'First worker' });
        view.send('sui:event', { type: 'delta', run: command.run, agent: 'w-b', text: 'Second worker' });
        view.send('sui:event', { type: 'mission_result', run: command.run, outcome: 'accepted', branch: 'sui-mission-fixture', accepted_sha: 'fixture-sha', run_dir: '/fixture/run' });
      }
      view.send('sui:event', { type: 'run_done', run: command.run, outcome: command.type === 'send' ? 'done' : 'accepted' });
    });
    return true;
  }
  if (channel === 'sui:command' && fixtureApproval && args[0].type === 'approve') {
    approvalCommands.push(args[0]);
    const view = BrowserWindow.getAllWindows()[0].webContents;
    setImmediate(() => {
      view.send('sui:event', { type: 'settings', approval: 'yolo' });
      view.send('sui:event', { type: 'permission_resolved', id: args[0].id });
      view.send('sui:event', { type: 'run_done', run: 777, outcome: 'stopped' });
    });
    return true;
  }
  if (channel === 'sui:command' && fixtureImages && args[0].type === 'send') {
    imageCommands.push(args[0]);
    setImmediate(() => BrowserWindow.getAllWindows()[0].webContents.send('sui:event', { type: 'send_error', run: args[0].run, message: 'fixture upload rejected' }));
    return true;
  }
  if (channel === 'sui:provider' && fixtureChatgpt) {
    const [target, request] = args;
    assert.equal(target.kind, 'local');
    if (request.action === 'login') {
      fixtureLogins++;
      fs.appendFileSync(path.join(tempHome, 'config.toml'), '\n[profiles.chatgpt]\nkind = "chatgpt"\nmodel = ""\n');
      fs.mkdirSync(path.join(tempHome, 'chatgpt'), { recursive: true });
      fs.writeFileSync(path.join(tempHome, 'chatgpt/chatgpt.json'), JSON.stringify({ client_id: 'oaiapp_fixture', subject: 'fixture-subject', email: 'fixture@example.com', access_token: 'private-smoke-access', refresh_token: 'private-smoke-refresh', scopes: ['chatgpt.tokens.use.direct'], expires_at: Math.floor(Date.now() / 1000) + 3600 }), { mode: 0o600 });
      fs.writeFileSync(path.join(tempHome, 'chatgpt/chatgpt.models.json'), JSON.stringify({ schema: 1, account: createHash('sha256').update('oaiapp_fixture:fixture-subject').digest('hex'), fetched_at: Math.floor(Date.now() / 1000), items: accountModels }), { mode: 0o600 });
      return { type: 'account', account: { email: 'fixture@example.com', connected: true, sharing: true } };
    }
    if (request.action === 'models') { if (request.refresh) fixtureRefreshes++; return { type: 'models', items: accountModels }; }
    if (request.action === 'logout') {
      fs.writeFileSync(path.join(tempHome, 'chatgpt/chatgpt.json'), JSON.stringify({ client_id: 'oaiapp_fixture', subject: 'fixture-subject', email: 'fixture@example.com' }), { mode: 0o600 });
      return { type: 'account', account: { email: 'fixture@example.com', connected: false, sharing: false }, revocation_confirmed: true };
    }
    if (request.action === 'select') throw new Error('Account model switching must not configure a pinned profile');
  }
  return handler(event, ...args);
});
require('../main');

const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

async function run() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'content-type': 'text/html' });
    response.end('<html><body><button id="target" style="position:absolute;left:50px;top:50px;width:180px;height:50px">Save changes</button></body></html>');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    for (let n = 0; n < 100 && !BrowserWindow.getAllWindows().length; n++) await pause(30);
    const win = BrowserWindow.getAllWindows()[0];
    assert.ok(win, 'SUI window opened');
    await new Promise(resolve => win.webContents.once('did-finish-load', resolve));
    const ui = code => win.webContents.executeJavaScript(code).catch(error => {
      throw new Error(`Renderer check failed: ${code}`, { cause: error });
    });
    for (let n = 0; n < 100; n++) { if (await ui('document.getElementById("connect-dialog").open')) break; await pause(30); }
    const ready = await win.webContents.executeJavaScript('document.getElementById("connect-dialog").open && !!window.sui');
    assert.equal(ready, true, 'connection form is visible and preload bridge works');
    await win.webContents.executeJavaScript(`{
      document.querySelector('input[name="kind"][value="local"]').click();
      document.getElementById('browse-workspace').click();
    }`);
    for (let n = 0; n < 100 && !pickerCalls; n++) await pause(30);
    await pause(50);
    assert.equal(await ui('document.getElementById("workspace").value'), '', 'cancelling the native chooser leaves the workspace empty');
    assert.equal(await ui('document.getElementById("connect-button").disabled'), true);
    await ui('document.getElementById("browse-workspace").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('document.getElementById("workspace").value') === workspace) break;
      await pause(30);
    }
    assert.equal(pickerCalls, 2, 'local workspace uses the native folder chooser');
    assert.equal(await ui('document.getElementById("workspace").value'), workspace);
    await ui(`{
      document.getElementById('profile').value = 'test';
      document.getElementById('profile').dispatchEvent(new Event('input'));
      document.getElementById('connect-button').click();
    }`);
    for (let n = 0; n < 100; n++) {
      if (await win.webContents.executeJavaScript('document.getElementById("connection-pill").textContent') === 'Local') break;
      await pause(50);
    }
    assert.equal(await win.webContents.executeJavaScript('document.getElementById("connection-pill").textContent'), 'Local', 'local SUI bridge connected');
    assert.equal(await ui('document.getElementById("composer-model").textContent'), 'test-model', 'the connected model is visible beside the draft');
    assert.equal(await ui('document.getElementById("composer-reasoning").disabled'), true, 'unknown models keep provider default');
    const modeRun = await ui('state.hello.run_id');
    await ui('document.getElementById("composer-mode").value = "auto-edits"; document.getElementById("composer-mode").dispatchEvent(new Event("change"))');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello.approval === "auto-edits" && !state.settingsPending')) break;
      await pause(30);
    }
    assert.equal(await ui('state.hello.approval'), 'auto-edits');
    assert.equal(await ui('state.hello.run_id'), modeRun, 'mode applies to the current chat');
    const originalRun = await ui('state.hello.run_id');
    await ui('document.getElementById("message").value = "Reasoning draft"; connect({ ...state.target, profile: "reasoning" })');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.model === "gpt-6.1-sol" && !connectionBusy')) break;
      await pause(30);
    }
    assert.equal(await ui('document.getElementById("composer-reasoning").disabled'), false);
    const reasoningRun = await ui('state.hello.run_id');
    await ui('document.getElementById("composer-reasoning").value = "medium"; document.getElementById("composer-reasoning").dispatchEvent(new Event("change"))');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.reasoning_effort === "medium" && !connectionBusy')) break;
      await pause(30);
    }
    assert.equal(await ui('state.hello.reasoning_effort'), 'medium');
    assert.notEqual(await ui('state.hello.run_id'), reasoningRun);
    assert.equal(await ui('document.getElementById("message").value'), 'Reasoning draft');
    if (process.env.SUI_DESKTOP_SCREENSHOT) fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-controls.png'), (await win.capturePage()).toPNG());
    await ui('connect({ ...state.target, profile: "test", reasoning_effort: undefined })');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.model === "test-model" && !connectionBusy')) break;
      await pause(30);
    }
    await ui('document.getElementById("choose-model").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('document.querySelectorAll(".model-option").length >= 2')) break;
      await pause(30);
    }
    assert.equal(await ui('document.getElementById("model-dialog").open'), true, 'the model badge opens the picker');
    assert.equal(await ui('document.querySelector(".model-option[aria-current=true]").textContent'), 'test-modeltest · Current');
    await ui('document.getElementById("model-filter").value = "missing-model"; document.getElementById("model-filter").dispatchEvent(new Event("input"))');
    assert.equal(await ui('document.getElementById("model-picker-status").textContent'), 'No matching models.');
    await ui('document.getElementById("model-filter").value = "OTHER-MODEL"; document.getElementById("model-filter").dispatchEvent(new Event("input"))');
    assert.equal(await ui('document.querySelectorAll(".model-option").length'), 1, 'search matches model IDs without case sensitivity');
    await ui('document.getElementById("message").value = "Keep this draft"; document.querySelector(".model-option").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.model === "other-model" && !connectionBusy')) break;
      await pause(50);
    }
    assert.equal(await ui('state.hello.model'), 'other-model', 'selection connects to the chosen profile');
    assert.notEqual(await ui('state.hello.run_id'), originalRun, 'switching starts a new session');
    assert.equal(await ui('document.getElementById("composer-model").textContent'), 'other-model');
    assert.equal(await ui('document.getElementById("message").value'), 'Keep this draft', 'switching preserves the draft');
    assert.equal(await ui('document.getElementById("model-dialog").open'), false);
    await ui('document.getElementById("choose-model").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('document.querySelector(".model-option[aria-current=true]") !== null')) break;
      await pause(30);
    }
    const switchedRun = await ui('state.hello.run_id');
    await ui('document.querySelector(".model-option[aria-current=true]").click()');
    assert.equal(await ui('state.hello.run_id'), switchedRun, 'selecting the current model keeps this session');
    await ui('state.activeRun = 99; setBusy(true); document.getElementById("choose-model").click()');
    assert.equal(await ui('document.getElementById("choose-model").disabled'), true, 'switching is disabled during a turn');
    assert.equal(await ui('document.getElementById("model-dialog").open'), false);
    await ui('state.activeRun = null; setBusy(false); document.getElementById("message").value = ""');
    assert.equal(await ui('document.querySelectorAll(".suggestion").length'), 4, 'starter cards are available');
    await ui('document.querySelector(".suggestion").click()');
    assert.equal(await ui('document.getElementById("message").value'), 'Explain how this codebase is organized');
    assert.equal(await ui('state.activeRun'), null, 'a starter card only fills the draft');
    await ui('document.getElementById("message").value = ""; document.getElementById("message").dispatchEvent(new Event("input"))');
    await ui('document.getElementById("project-filter").value = "no-matching-project"; document.getElementById("project-filter").dispatchEvent(new Event("input"))');
    assert.equal(await ui('document.getElementById("project-list").textContent'), 'No matching projects');
    assert.equal(await ui('document.getElementById("connection-pill").textContent'), 'Local', 'filtering keeps the project connected');
    await ui('document.getElementById("clear-project-filter").click()');
    assert.equal(await ui('document.querySelectorAll(".project-row").length'), 1);
    assert.equal(await ui('document.getElementById("project-count").textContent'), '1');
    await ui('window.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: SuiUI.platform !== "darwin", metaKey: SuiUI.platform === "darwin", bubbles: true }))');
    assert.equal(await ui('document.activeElement.id'), 'project-filter', 'project search shortcut focuses the search');
    await ui('document.getElementById("sb-appearance").click(); document.querySelector(\'input[name="motion"][value="off"]\').click(); document.getElementById("appearance-dialog").close(); document.getElementById("project-filter").blur()');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      await pause(200);
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-welcome.png'), (await win.capturePage()).toPNG());
    }
    const url = `http://127.0.0.1:${server.address().port}/`;
    const address = `127.0.0.1:${server.address().port}`;
    await win.webContents.executeJavaScript(`{document.getElementById('browser-url').value=${JSON.stringify(address)};document.getElementById('go').click()}`);
    await pause(200);
    const guest = win.contentView.children.find(view => view.webContents);
    assert.ok(guest, 'browser guest attached');
    assert.ok(guest.getBounds().width > 100, 'browser is visible in pane');
    assert.equal(guest.webContents.getURL(), url, 'a bare loopback address loads over HTTP');
    assert.equal(await ui('document.getElementById("browser-url").value'), url, 'the address bar keeps the normalized URL');
    await win.webContents.executeJavaScript('document.getElementById("annotate").click()');
    await pause(200);
    guest.webContents.sendInputEvent({ type: 'mouseMove', x: 100, y: 75 });
    guest.webContents.sendInputEvent({ type: 'mouseDown', button: 'left', clickCount: 1, x: 100, y: 75 });
    guest.webContents.sendInputEvent({ type: 'mouseUp', button: 'left', clickCount: 1, x: 100, y: 75 });
    for (let n = 0; n < 100; n++) {
      if (await win.webContents.executeJavaScript('!document.getElementById("annotation-card").classList.contains("hidden")')) break;
      await pause(50);
    }
    const annotation = await win.webContents.executeJavaScript('document.getElementById("annotation-target").textContent');
    assert.match(annotation, /#target/);
    assert.match(annotation, /127\.0\.0\.1/);
    const heroGeometry = await win.webContents.executeJavaScript(`({
      petBottom: document.querySelector('.pet').getBoundingClientRect().bottom,
      titleTop: document.getElementById('welcome-title').getBoundingClientRect().top
    })`);
    assert.ok(heroGeometry.petBottom <= heroGeometry.titleTop, 'companion does not cross the welcome title');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      if (process.platform === 'linux') win.setBounds({ x: 0, y: 0, width: 1280, height: 900 });
      await pause(300);
      const filename = path.resolve(process.env.SUI_DESKTOP_SCREENSHOT);
      fs.writeFileSync(filename, (await win.capturePage()).toPNG());
      const browserShot = await guest.webContents.capturePage();
      fs.writeFileSync(filename.replace(/\.png$/, '-browser.png'), browserShot.toPNG());
      // capturePage omits native WebContentsView content. X11 captures the
      // actual composed window, including the browser pane, for visual review.
      if (process.platform === 'linux' && process.env.DISPLAY) {
        const screen = spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', '-f', 'x11grab', '-video_size', '1280x900', '-i', process.env.DISPLAY, '-frames:v', '1', filename.replace(/\.png$/, '-screen.png')], { timeout: 10000 });
        assert.equal(screen.status, 0, `X11 screenshot failed: ${screen.stderr}`);
      }
    }
    // The guest page is a native view above the renderer: it must step aside for
    // dialogs and leave with its panel, then come back.
    const attached = () => win.contentView.children.some(view => view.webContents);
    assert.equal(attached(), true);
    await ui('document.getElementById("sb-appearance").click()');
    await pause(150);
    assert.equal(attached(), false, 'preview hides while a dialog is open');
    assert.equal(await ui('document.getElementById("pet-on").checked'), true, 'appearance reflects the visible companion');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-appearance.png'), (await win.capturePage()).toPNG());
    }
    await ui('document.querySelector(\'input[name="theme"][value="dark"]\').click(); document.getElementById("appearance-dialog").close()');
    await pause(150);
    assert.equal(attached(), true, 'preview returns when the dialog closes');
    assert.equal(await ui('document.documentElement.dataset.theme'), 'dark', 'theme choice applies');
    await ui('document.getElementById("toggle-preview").click()');
    await pause(150);
    assert.equal(attached(), false, 'closing the panel detaches the native view');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-dark.png'), (await win.capturePage()).toPNG());
    }
    await ui('document.getElementById("toggle-preview").click()');
    await pause(300);
    assert.equal(attached(), true, 'reopening the panel brings it back');
    assert.equal(await ui('document.querySelectorAll(".pet").length'), 1, 'the companion is on stage');
    await ui('appendMessage("user", "Please review this page")');
    await pause(100);
    const laneGeometry = await ui(`({
      petTop: document.querySelector('.pet').getBoundingClientRect().top,
      petBottom: document.querySelector('.pet').getBoundingClientRect().bottom,
      laneTop: document.getElementById('pet-lane').getBoundingClientRect().top,
      laneBottom: document.getElementById('pet-lane').getBoundingClientRect().bottom
    })`);
    assert.ok(laneGeometry.petTop >= laneGeometry.laneTop - 1, 'chat companion stays below the transcript');
    assert.ok(laneGeometry.petBottom <= laneGeometry.laneBottom + 1, 'chat companion stays above the composer');
    await ui('appendMessage("assistant", "I’ll inspect the project, make a focused change, and verify the result.\\n\\n```sh\\ncargo test\\n```")');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      await pause(100);
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-chat.png'), (await win.capturePage()).toPNG());
    }
    await ui('for (let i = 0; i < 18; i++) appendMessage(i % 2 ? "assistant" : "user", "Conversation scroll test " + i + "\\n" + "A longer explanation to exercise navigation. ".repeat(12)); document.getElementById("transcript").scrollTop = 0');
    await pause(500);
    assert.equal(await ui('document.getElementById("jump-to-latest").classList.contains("hidden")'), false, 'scrolling up reveals Jump to latest');
    await ui('document.getElementById("jump-to-latest").click()');
    for (let n = 0; n < 40; n++) {
      if (await ui('document.getElementById("jump-to-latest").classList.contains("hidden")')) break;
      await pause(50);
    }
    assert.equal(await ui('document.getElementById("jump-to-latest").classList.contains("hidden")'), true, 'Jump to latest returns to the end');
    win.setSize(720, 560);
    await pause(400);
    assert.equal(await ui('document.documentElement.scrollWidth <= innerWidth'), true, 'the minimum window width has no page overflow');
    assert.equal(await ui('document.getElementById("composer-model").getBoundingClientRect().width > 0'), true, 'the model remains visible in a small window');
    assert.equal(await ui('document.getElementById("send").getBoundingClientRect().right <= innerWidth'), true, 'the send control stays inside the window');
    fixtureMission = true;
    await ui('clearAnnotation(); setMissionInfo({ ready: true, profiles: [{name:"fixture-role", model:"fixture"}] }); document.getElementById("composer-execution").value="mission"; document.getElementById("composer-execution").dispatchEvent(new Event("change"))');
    await ui('document.getElementById("message").value="Discuss first"; document.getElementById("message").dispatchEvent(new Event("input")); document.getElementById("message").dispatchEvent(new KeyboardEvent("keydown", {key:"Enter", bubbles:true, cancelable:true}))');
    for (let n = 0; n < 100 && !missionCommands.length; n++) await pause(30);
    for (let n = 0; n < 100 && await ui('state.activeRun !== null'); n++) await pause(30);
    assert.equal(missionCommands[0].type, 'send'); assert.equal(missionCommands[0].mode, 'mission', 'Mission Enter chats');
    // Refresh metadata for the UI-only committed-repository fixture.
    await ui('setMissionInfo({ ready: true, profiles: [{name:"fixture-role", model:"fixture"}] }); document.getElementById("mission-options").open=true; document.getElementById("mission-worker").value="fixture-role"; document.getElementById("mission-workers").value="2"');
    assert.equal(await ui('document.documentElement.scrollWidth <= innerWidth'), true, 'Mission options fit the minimum width');
    for (let n = 0; n < 100 && await ui('state.missionInfoPending'); n++) await pause(30);
    await ui('document.getElementById("message").value="Goal B"; document.getElementById("message").dispatchEvent(new Event("input")); document.getElementById("run-mission").click()');
    for (let n = 0; n < 100 && missionCommands.length < 2; n++) await pause(30);
    for (let n = 0; n < 100 && await ui('state.activeRun !== null'); n++) await pause(30);
    assert.equal(missionCommands[1].type, 'run_mission'); assert.equal(missionCommands[1].worker_profile, 'fixture-role'); assert.equal(missionCommands[1].max_workers, 2);
    assert.equal(await ui('document.querySelector(".mission-result").textContent.includes("fixture-sha")'), true, 'runtime branch and acceptance are visible');
    assert.equal(await ui('state.missionStreams.size'), 2, 'parallel worker text stays separate');
    assert.equal(missionCommands[1].text, 'Goal B');
    assert.equal(await ui('state.lastMissionTask'), 'Goal B', 'accepted execution updates the repeated objective');
    for (let n = 0; n < 100 && await ui('state.missionInfoPending'); n++) await pause(30);
    await ui('document.getElementById("run-mission").click()');
    for (let n = 0; n < 100 && (missionCommands.length < 3 || await ui('state.activeRun !== null')); n++) await pause(30);
    assert.equal(missionCommands[2].text, 'Goal B', 'repeat sends the exact objective shown on screen');
    assert.equal(await ui('[...document.querySelectorAll(".msg.user")].at(-1).textContent'), 'Run mission: Goal B');
    fixtureMissionReject = true;
    await ui('document.getElementById("message").value="Rejected goal"; document.getElementById("send").click()');
    for (let n = 0; n < 100 && (missionCommands.length < 4 || await ui('state.activeRun !== null')); n++) await pause(30);
    assert.equal(await ui('state.lastMissionTask'), 'Goal B', 'a rejected draft cannot replace the last accepted goal');
    assert.equal(await ui('document.getElementById("message").value'), 'Rejected goal');
    fixtureMissionReject = false;
    await ui('document.getElementById("message").value=""; document.getElementById("message").dispatchEvent(new Event("input"))');
    for (let n = 0; n < 100 && await ui('state.missionInfoPending'); n++) await pause(30);
    const checksBefore = fixtureMissionChecks;
    await ui('window.dispatchEvent(new Event("focus"))');
    for (let n = 0; n < 100 && fixtureMissionChecks === checksBefore; n++) await pause(30);
    assert.ok(fixtureMissionChecks > checksBefore, 'returning to the window refreshes Git readiness');

    await ui('state.activeRun=778; onEvent({type:"permission", run:778, id:551, summary:"worker a"}); onEvent({type:"permission", run:778, id:552, summary:"worker b"}); onEvent({type:"permission_resolved", id:551})');
    assert.equal(await ui('state.permissionId'), 552, 'the next concurrent approval stays actionable');
    await ui('onEvent({type:"run_done", run:778, outcome:"stopped"}); document.getElementById("composer-execution").value="solo"; document.getElementById("composer-execution").dispatchEvent(new Event("change"))');
    fixtureMission = false;
    fixtureApproval = true;
    await ui('state.activeRun = 777; onEvent({ type: "permission", run: 777, id: 555, summary: "bash: " + "long command line\\n".repeat(2000), allow_session: false, allow_yolo: true })');
    await pause(200);
    assert.equal(await ui('document.getElementById("permission-summary").scrollHeight > document.getElementById("permission-summary").clientHeight'), true, 'long approval text scrolls inside its own area');
    assert.equal(await ui('document.getElementById("composer-mode").disabled'), false, 'permission mode can be changed at a pending approval');
    const button = await ui('(() => { const r = document.querySelector("[data-choice=yolo]").getBoundingClientRect(); return { x:r.x+r.width/2, y:r.y+r.height/2, bottom:r.bottom, top:r.top }; })()');
    assert.ok(button.top >= 0 && button.bottom <= 560, 'approval button is visible in the minimum window');
    if (process.env.SUI_DESKTOP_SCREENSHOT) fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-approval.png'), (await win.capturePage()).toPNG());
    win.webContents.sendInputEvent({ type: 'mouseDown', button: 'left', clickCount: 1, x: Math.round(button.x), y: Math.round(button.y) });
    win.webContents.sendInputEvent({ type: 'mouseUp', button: 'left', clickCount: 1, x: Math.round(button.x), y: Math.round(button.y) });
    for (let n = 0; n < 100; n++) { if (approvalCommands.length && await ui('state.activeRun === null')) break; await pause(30); }
    assert.deepEqual(approvalCommands, [{ type: 'approve', id: 555, choice: 'yolo' }], 'one physical click reaches the approval bridge');
    assert.equal(await ui('document.getElementById("permission").classList.contains("hidden")'), true);
    fixtureApproval = false;
    await ui('newChat()');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello !== null && document.getElementById("stage").dataset.stage === "hero"')) break;
      await pause(50);
    }
    await pause(200);
    assert.equal(await ui('document.querySelector(".suggestion:last-child").getBoundingClientRect().bottom <= document.getElementById("composer-context").getBoundingClientRect().top'), true, 'starter cards fit above the composer in a small window');
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-compact.png'), (await win.capturePage()).toPNG());
    }
    await ui('showDialog(state.target); showProviderSetup()');
    for (let n = 0; n < 100; n++) {
      if (await ui('document.querySelectorAll("#saved-profile option").length >= 3')) break;
      await pause(30);
    }
    assert.equal(await ui('document.querySelectorAll("#saved-profile option").length >= 3'), true, 'setup discovers saved profiles without typing names');
    assert.equal(await ui('document.getElementById("model-kind").value'), 'chatgpt');
    assert.equal(await ui('document.getElementById("api-setup").classList.contains("hidden")'), true, 'ChatGPT setup has no API key form');
    assert.equal(await ui('document.getElementById("chatgpt-login").textContent'), 'Continue with ChatGPT');
    if (process.env.SUI_DESKTOP_SETUP_SCREENSHOT) {
      win.setSize(1280, 900);
      await pause(150);
      await ui('document.getElementById("model-setup").scrollIntoView({block:"nearest"})');
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SETUP_SCREENSHOT), (await win.capturePage()).toPNG());
    }
    await ui('document.getElementById("model-kind").value = "openai-responses"; document.getElementById("model-kind").dispatchEvent(new Event("change"))');
    assert.equal(await ui('document.getElementById("model-url-field").classList.contains("hidden")'), true, 'OpenAI API uses a preset endpoint');
    assert.equal(await ui('document.getElementById("save-model").disabled'), true, 'API setup requires model and key');
    await ui('document.getElementById("model-kind").value = "ollama"; document.getElementById("model-kind").dispatchEvent(new Event("change")); document.getElementById("model-id").value = "local-fixture"; document.getElementById("model-id").dispatchEvent(new Event("input"))');
    assert.equal(await ui('document.getElementById("model-key-field").classList.contains("hidden")'), true, 'Ollama hides the API key');
    assert.equal(await ui('document.getElementById("save-model").disabled'), false, 'Ollama can be configured without a key');
    await ui('document.getElementById("save-model").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('!providerBusy && document.getElementById("profile").value === "ollama"')) break;
      await pause(30);
    }
    assert.match(fs.readFileSync(path.join(tempHome, 'config.toml'), 'utf8'), /local-fixture/);
    fixtureChatgpt = true;
    await ui('showProviderSetup(); document.getElementById("model-kind").value = "chatgpt"; document.getElementById("model-kind").dispatchEvent(new Event("change")); document.getElementById("chatgpt-login").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('!providerBusy && document.getElementById("profile").value === "chatgpt" && !document.getElementById("provider-dialog").open')) break;
      await pause(30);
    }
    assert.equal(await ui('document.getElementById("profile").value'), 'chatgpt', 'sign-in finishes without a model selection step');
    assert.equal(await ui('document.getElementById("chatgpt-model")'), null, 'setup does not pin an account to one model');
    await ui('document.getElementById("connect-button").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.model === "account-model-slug" && !connectionBusy')) break;
      await pause(50);
    }
    assert.equal(await ui('document.getElementById("composer-model").textContent'), 'Account model display name');
    assert.equal(await ui('document.getElementById("composer-reasoning").value'), 'low', 'use the account model default');
    await ui('document.getElementById("choose-model").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.profiles?.some(p => p.model === "second-account-model")')) break;
      await pause(30);
    }
    assert.equal(await ui('state.profiles.filter(p => p.profile === "chatgpt").length'), 2, 'all models share one account profile');
    await ui('document.getElementById("refresh-model-catalog").click()');
    for (let n = 0; n < 100; n++) {
      if (fixtureRefreshes && await ui('!document.getElementById("refresh-model-catalog").disabled')) break;
      await pause(30);
    }
    assert.equal(fixtureRefreshes, 1, 'refresh requests a fresh account catalog');
    await ui('document.getElementById("message").value = "Keep account draft"; [...document.querySelectorAll(".model-option")].find(b => b.textContent.includes("Another account model")).click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('state.hello?.model === "second-account-model" && !connectionBusy')) break;
      await pause(30);
    }
    assert.equal(await ui('state.hello.model'), 'second-account-model');
    assert.equal(await ui('document.getElementById("composer-model").textContent'), 'Another account model');
    assert.equal(await ui('document.getElementById("message").value'), 'Keep account draft');
    assert.deepEqual(await ui('[...document.getElementById("composer-reasoning").options].filter(o => !o.disabled).map(o => o.value)'), ['default', 'medium', 'xhigh']);
    assert.equal(await ui('document.querySelector("#composer-reasoning option[value=ultra]").disabled'), true);
    fixtureImages = true;
    const pixels = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jEn8AAAAASUVORK5CYII=';
    await ui(`{ const data = new DataTransfer(); data.items.add(new File([Uint8Array.from(atob(${JSON.stringify(pixels)}), c => c.charCodeAt(0))], 'mac-screen.png', { type: 'image/png' })); document.getElementById('composer').dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: data })); }`);
    for (let n = 0; n < 100; n++) { if (await ui('state.images.length === 1 && !state.imagesLoading')) break; await pause(30); }
    assert.equal(await ui('document.querySelector(".image-attachment span").textContent'), 'mac-screen.png');
    await ui('document.getElementById("send").click()');
    for (let n = 0; n < 100; n++) { if (imageCommands.length && await ui('state.images.length === 1 && state.activeRun === null')) break; await pause(30); }
    assert.equal(imageCommands[0].images[0].data, pixels, 'dropped pixels are sent over the bridge');
    assert.equal(await ui('document.getElementById("message").value'), 'Keep account draft', 'rejected uploads preserve the text draft');
    assert.equal(await ui('state.images.length'), 1, 'rejected uploads preserve image drafts');
    await ui('document.querySelector(".image-attachment button").click()');
    assert.equal(await ui('state.images.length'), 0, 'attachments can be removed before sending');
    fixtureImages = false;
    assert.equal(fixtureLogins, 1, 'switching models reuses the signed-in account');
    assert.match(fs.readFileSync(path.join(tempHome, 'config.toml'), 'utf8'), /\[profiles.chatgpt\]\nkind = "chatgpt"\nmodel = ""/);
    if (process.env.SUI_DESKTOP_SCREENSHOT) fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT).replace(/\.png$/, '-account-models.png'), (await win.capturePage()).toPNG());
    assert.equal(await ui('document.getElementById("composer-usage").classList.contains("hidden")'), false, 'ChatGPT usage control is visible beside the model');
    win.setSize(720, 560);
    await pause(150);
    assert.equal(await ui('document.documentElement.scrollWidth <= innerWidth'), true, 'ChatGPT model and usage controls fit the narrow window');
    await ui('showDialog(state.target)');
    await ui('document.getElementById("saved-profile").value = "chatgpt"; document.getElementById("saved-profile").dispatchEvent(new Event("change"))');
    assert.equal(await ui('document.getElementById("provider-dialog").open'), false, 'choosing a connected account does not require setup');
    await ui('showProviderSetup(); document.getElementById("model-kind").value = "chatgpt"; document.getElementById("setup-profile").value = "chatgpt"; setupAccount = savedProfiles.find(p => p.name === "chatgpt").account; updateProviderForm()');
    await ui('document.getElementById("chatgpt-logout").click()');
    for (let n = 0; n < 100; n++) {
      if (await ui('!providerBusy && state.hello === null')) break;
      await pause(30);
    }
    assert.equal(await ui('state.hello'), null, 'signing out disconnects the active account');
    assert.equal(await ui('document.getElementById("connect-button").disabled'), true, 'a signed-out ChatGPT profile must reconnect before use');
    console.log('DESKTOP_SMOKE_OK');
  } finally { server.close(); fs.rmSync(tempHome, { recursive: true, force: true }); fs.rmSync(workspace, { recursive: true, force: true }); app.quit(); }
}

app.whenReady().then(run).catch(error => { console.error(error); app.exit(1); });
