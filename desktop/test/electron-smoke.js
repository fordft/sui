// Run with: xvfb-run -a electron --no-sandbox test/electron-smoke.js
const { app, BrowserWindow, dialog } = require('electron');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const tempHome = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-desktop-smoke-'));
app.setPath('userData', tempHome);
process.env.SUI_HOME = tempHome;
fs.writeFileSync(path.join(tempHome, 'config.toml'), '[profiles.test]\nkind = "ollama"\nmodel = "test-model"\n', { mode: 0o600 });
const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-desktop-workspace-'));
let pickerCalls = 0;
dialog.showOpenDialog = async (_window, options) => {
  assert.deepEqual(options.properties, ['openDirectory']);
  pickerCalls++;
  if (pickerCalls === 1) return { canceled: true, filePaths: [] };
  return { canceled: false, filePaths: [workspace] };
};
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
    const ui = code => win.webContents.executeJavaScript(code);
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
    await win.webContents.executeJavaScript(`{document.getElementById('browser-url').value=${JSON.stringify(url)};document.getElementById('go').click()}`);
    await pause(200);
    const guest = win.contentView.children.find(view => view.webContents);
    assert.ok(guest, 'browser guest attached');
    assert.ok(guest.getBounds().width > 100, 'browser is visible in pane');
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
    console.log('DESKTOP_SMOKE_OK');
  } finally { server.close(); fs.rmSync(tempHome, { recursive: true, force: true }); fs.rmSync(workspace, { recursive: true, force: true }); app.quit(); }
}

app.whenReady().then(run).catch(error => { console.error(error); app.exit(1); });
