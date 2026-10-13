// Run with: xvfb-run -a electron --no-sandbox test/electron-smoke.js
const { app, BrowserWindow } = require('electron');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
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
    if (process.env.SUI_DESKTOP_SCREENSHOT) {
      const shot = await win.capturePage();
      fs.writeFileSync(path.resolve(process.env.SUI_DESKTOP_SCREENSHOT), shot.toPNG());
    }
    console.log('DESKTOP_SMOKE_OK');
  } finally { server.close(); fs.rmSync(tempHome, { recursive: true, force: true }); fs.rmSync(workspace, { recursive: true, force: true }); app.quit(); }
}

app.whenReady().then(run).catch(error => { console.error(error); app.exit(1); });
