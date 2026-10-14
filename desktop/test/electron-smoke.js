// Run with: xvfb-run -a electron --no-sandbox test/electron-smoke.js
const { app, BrowserWindow } = require('electron');
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
    const ready = await win.webContents.executeJavaScript('document.getElementById("connect-dialog").open && !!window.sui');
    assert.equal(ready, true, 'connection form is visible and preload bridge works');
    await win.webContents.executeJavaScript(`{
      document.querySelector('input[name="kind"][value="local"]').click();
      document.getElementById('workspace').value = ${JSON.stringify(workspace)};
      document.getElementById('profile').value = 'test';
      document.getElementById('connect-button').click();
    }`);
    for (let n = 0; n < 100; n++) {
      if (await win.webContents.executeJavaScript('document.getElementById("connection-pill").textContent') === 'Local') break;
      await pause(50);
    }
    assert.equal(await win.webContents.executeJavaScript('document.getElementById("connection-pill").textContent'), 'Local', 'local SUI bridge connected');
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
    const ui = code => win.webContents.executeJavaScript(code);
    assert.equal(attached(), true);
    await ui('SuiUI.openDialog(document.getElementById("appearance-dialog"))');
    await pause(150);
    assert.equal(attached(), false, 'preview hides while a dialog is open');
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
    console.log('DESKTOP_SMOKE_OK');
  } finally { server.close(); fs.rmSync(tempHome, { recursive: true, force: true }); fs.rmSync(workspace, { recursive: true, force: true }); app.quit(); }
}

app.whenReady().then(run).catch(error => { console.error(error); app.exit(1); });
