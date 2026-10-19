const { app, BrowserWindow, WebContentsView, dialog, ipcMain, nativeTheme, session, shell } = require('electron');
const { randomUUID } = require('node:crypto');
const path = require('node:path');
const net = require('node:net');
const fs = require('node:fs');
const { spawn } = require('node:child_process');
const { spawnBridge, spawnSetup, validateHost } = require('./bridge');
const updates = require('./update');
const { sshHosts, remoteFolders } = require('./workspaces');
const { createProviders } = require('./providers');
const providers = createProviders({ spawn: item => spawnSetup(app, item, 'desktop-provider'), openExternal: url => shell.openExternal(url) });

let window;
let bridge;
let browser;
let tunnel;
let target;
let localPort;
let remotePort;
let offeredUpdate;
let folderBrowse;
let activeRun = null;

function cancelFolderBrowse() { folderBrowse?.abort(); folderBrowse = undefined; }

function emit(value) {
  if (window && !window.isDestroyed()) window.webContents.send('sui:event', value);
}

function readProjects() {
  try {
    const items = JSON.parse(fs.readFileSync(path.join(app.getPath('userData'), 'projects.json'), 'utf8'));
    return Array.isArray(items) ? items.slice(0, 30) : [];
  } catch { return []; }
}

function rememberProject(item) {
  const projects = readProjects().filter(p => !(p.kind === item.kind && p.host === item.host && p.workspace === item.workspace));
  projects.unshift({ kind: item.kind, host: item.kind === 'ssh' ? item.host : '', workspace: item.workspace, profile: item.profile });
  const file = path.join(app.getPath('userData'), 'projects.json');
  const tmp = file + '.tmp';
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(tmp, JSON.stringify(projects.slice(0, 30)), { mode: 0o600 });
  fs.renameSync(tmp, file);
}

function saveConnection(item, sessionId) {
  const saved = { version: 1, target: { kind: item.kind, host: item.kind === 'ssh' ? item.host : '', workspace: item.workspace, profile: item.profile, ...(item.model ? { model: item.model } : {}), ...(item.reasoning_effort ? { reasoning_effort: item.reasoning_effort } : {}) }, session: sessionId || null };
  const directory = app.getPath('userData');
  fs.mkdirSync(directory, { recursive: true });
  const temporary = path.join(directory, `connection-${randomUUID()}.tmp`);
  try {
    fs.writeFileSync(temporary, JSON.stringify(saved), { mode: 0o600, flag: 'wx' });
    fs.renameSync(temporary, path.join(directory, 'connection.json'));
  } finally { fs.rmSync(temporary, { force: true }); }
}

function readConnection() {
  try {
    const file = path.join(app.getPath('userData'), 'connection.json');
    if (fs.statSync(file).size > 32768) return null;
    const saved = JSON.parse(fs.readFileSync(file, 'utf8'));
    return saved.version === 1 && saved.target?.workspace && saved.target?.profile ? saved : null;
  } catch { return null; }
}
let sessionId, sessionHasHistory = false;
const closingBridges = new Set();

function stopTunnel() {
  if (tunnel) { tunnel.kill(); tunnel = undefined; }
  localPort = undefined;
  remotePort = undefined;
}

function stopBridge() {
  activeRun = null;
  stopTunnel();
  const child = bridge;
  bridge = undefined;
  target = undefined;
  if (!child) return Promise.allSettled([...closingBridges]);
  const stopped = new Promise(resolve => {
    const timer = setTimeout(() => { child.kill(); }, 5000);
    child.once('close', () => { clearTimeout(timer); resolve(); });
    // EOF asks Rust to cancel and journal the active turn before exiting.
    child.stdin.on('error', () => {});
    child.stdin.end();
  });
  closingBridges.add(stopped);
  stopped.finally(() => closingBridges.delete(stopped));
  return stopped;
}

function writeBridge(value) {
  if (!bridge || !bridge.stdin.writable) throw new Error('Connect to a project first.');
  const line = JSON.stringify(value);
  if (Buffer.byteLength(line) > 12 * 1024 * 1024) throw new Error('Message exceeds the 12 MiB bridge limit.');
  bridge.stdin.write(line + '\n');
  if (value.type === 'send' || value.type === 'run_mission') { activeRun = value.run; saveConnection(target, sessionId); }
}

async function startBridge(item, resume) {
  await stopBridge();
  const child = spawnBridge(app, item, resume);
  target = item;
  bridge = child;
  let stderr = '';
  let buffer = '';
  let settled = false;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => fail(new Error('SUI did not start within 60 seconds.')), 60000);
    function fail(error) {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      if (bridge === child) stopBridge();
      reject(error);
    }
    child.on('error', fail);
    child.stderr.on('data', chunk => { stderr = (stderr + chunk.toString()).slice(-4000); });
    child.stdout.on('data', chunk => {
      if (bridge !== child) return;
      buffer += chunk.toString();
      if (buffer.length > 1024 * 1024) { fail(new Error('SUI sent an oversized response.')); return; }
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        let value;
        try { value = JSON.parse(line); } catch { fail(new Error('SUI sent invalid JSON.')); return; }
        if (!settled && value.type === 'hello') {
          settled = true;
          clearTimeout(timeout);
          target = { ...item, workspace: value.workspace, model: value.model, reasoning_effort: value.reasoning_effort };
          sessionId = value.run_id;
          sessionHasHistory = !!value.history?.length;
          rememberProject(target);
          saveConnection(target, sessionHasHistory ? sessionId : null);
          resolve(value);
        } else if (settled) {
          if (value.type === 'run_done') { activeRun = null; sessionHasHistory = true; }
          if (value.type === 'send_error' && !sessionHasHistory) saveConnection(target, null);
          emit(value);
        }
      }
    });
    child.on('close', code => {
      if (!settled) fail(new Error(stderr.includes('--desktop-protocol') ? 'Update SUI on this host to use the current model picker.' : stderr.trim() || `SUI exited with code ${code}.`));
      else if (bridge === child) { activeRun = null; bridge = undefined; emit({ type: 'disconnected', message: stderr.trim() || `SUI exited with code ${code}.` }); }
    });
  });
}

function configureProfile(item, profile) {
  const child = spawnSetup(app, item);
  return new Promise((resolve, reject) => {
    let stdout = '', stderr = '', settled = false;
    const timer = setTimeout(() => done(new Error('Profile setup timed out.')), 20000);
    function done(error, result) {
      if (settled) return;
      settled = true; clearTimeout(timer);
      if (error) { child.kill(); reject(error); } else resolve(result);
    }
    child.on('error', done);
    child.stdout.on('data', chunk => { stdout = (stdout + chunk).slice(-4000); });
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
    child.on('close', code => {
      if (code !== 0) return done(new Error(stderr.trim() || `SUI profile setup failed (${code}).`));
      try {
        const result = JSON.parse(stdout.trim());
        if (result.type !== 'profile_saved') throw new Error('Profile was not saved.');
        done(null, result);
      } catch (error) { done(error); }
    });
    child.stdin.end(JSON.stringify(profile) + '\n');
  });
}

function installRemoteSui(item) {
  if (item.kind !== 'ssh') throw new Error('Remote installation requires an SSH host.');
  const host = validateHost(item.host);
  const script = fs.readFileSync(app.isPackaged ? path.join(process.resourcesPath, 'install.sh') : path.resolve(__dirname, '../install.sh'));
  const command = 'sh -s && { "$HOME/.sui/bin/sui" --desktop-protocol 4 desktop-bridge --help >/dev/null 2>&1 && "$HOME/.sui/bin/sui" --desktop-protocol 4 desktop-provider --help >/dev/null 2>&1 || { echo "Installed SUI needs the current Desktop protocol; update SUI on this host." >&2; exit 1; }; }';
  const child = spawn('ssh', ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12', '--', host, command], { stdio: ['pipe', 'pipe', 'pipe'] });
  return new Promise((resolve, reject) => {
    let output = '', settled = false;
    const timer = setTimeout(() => finish(new Error('Remote installation timed out after 3 minutes.')), 180000);
    function finish(error) {
      if (settled) return;
      settled = true; clearTimeout(timer);
      if (error) { child.kill(); reject(error); } else resolve(output.trim());
    }
    child.on('error', finish);
    child.stdout.on('data', chunk => { output = (output + chunk).slice(-8000); });
    child.stderr.on('data', chunk => { output = (output + chunk).slice(-8000); });
    child.on('close', code => finish(code === 0 ? null : new Error(output.trim() || `Installation failed (${code}).`)));
    child.stdin.end(script);
  });
}

function validUrl(input) {
  const url = new URL(input);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) throw new Error('Use an HTTP(S) URL without embedded credentials.');
  return url.toString();
}

function createBrowser() {
  const partition = `sui-preview-${randomUUID()}`;
  const ses = session.fromPartition(partition);
  ses.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  browser = new WebContentsView({ webPreferences: { partition, sandbox: true, contextIsolation: true, nodeIntegration: false, webSecurity: true } });
  browser.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  browser.webContents.on('will-navigate', (event, url) => {
    try { validUrl(url); } catch { event.preventDefault(); }
  });
  browser.webContents.on('did-navigate', (_event, url) => emit({ type: 'browser_url', url }));
  browser.webContents.on('did-navigate-in-page', (_event, url) => emit({ type: 'browser_url', url }));
  browser.webContents.on('did-fail-load', (_event, code, description) => {
    if (code !== -3) emit({ type: 'browser_error', message: description });
  });
}

function showBrowser() {
  if (!window.contentView.children.includes(browser)) window.contentView.addChildView(browser);
}

function browserBounds(rect) {
  if (!browser || !window || !rect || typeof rect !== 'object') return;
  const size = window.getContentBounds();
  const x = Math.max(0, Math.min(size.width, Math.trunc(Number(rect.x) || 0)));
  const y = Math.max(0, Math.min(size.height, Math.trunc(Number(rect.y) || 0)));
  const width = Math.max(0, Math.min(size.width - x, Math.trunc(Number(rect.width) || 0)));
  const height = Math.max(0, Math.min(size.height - y, Math.trunc(Number(rect.height) || 0)));
  browser.setBounds({ x, y, width, height });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}

async function openRemotePreview(port) {
  if (!target || target.kind !== 'ssh') throw new Error('Open an SSH project first.');
  const number = Number(port);
  if (!Number.isInteger(number) || number < 1 || number > 65535) throw new Error('Enter a remote port from 1 to 65535.');
  stopTunnel();
  localPort = await freePort();
  remotePort = number;
  const host = validateHost(target.host);
  const child = spawn('ssh', ['-N', '-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ExitOnForwardFailure=yes', '-o', 'ConnectTimeout=12', '-L', `127.0.0.1:${localPort}:127.0.0.1:${number}`, '--', host], { stdio: 'ignore' });
  tunnel = child;
  child.on('error', error => emit({ type: 'browser_error', message: error.message }));
  child.on('close', code => { if (tunnel === child) { stopTunnel(); emit({ type: 'browser_error', message: `SSH preview tunnel closed (${code}).` }); } });
  await new Promise(resolve => setTimeout(resolve, 350));
  if (tunnel !== child) throw new Error('SSH preview tunnel could not start.');
  const url = `http://127.0.0.1:${localPort}/`;
  showBrowser();
  await browser.webContents.loadURL(url);
  return { url, remotePort: number };
}

const ANNOTATE_SCRIPT = `new Promise(resolve => {
  const old = document.getElementById('__sui_annotation_overlay__'); if (old) old.remove();
  const overlay = document.createElement('div'); overlay.id = '__sui_annotation_overlay__';
  Object.assign(overlay.style, {position:'fixed',zIndex:'2147483647',pointerEvents:'none',border:'2px solid #4c8cff',background:'rgba(76,140,255,.13)',display:'none',boxSizing:'border-box'});
  document.documentElement.appendChild(overlay);
  let start = null, moved = false;
  const position = r => { Object.assign(overlay.style,{display:'block',left:r.x+'px',top:r.y+'px',width:r.width+'px',height:r.height+'px'}); };
  const selector = el => { const parts=[]; for(let n=el;n&&n.nodeType===1&&parts.length<7;n=n.parentElement){let p=n.tagName.toLowerCase(); if(n.id){p+='#'+CSS.escape(n.id);parts.unshift(p);break}let i=1;for(let s=n.previousElementSibling;s;s=s.previousElementSibling)if(s.tagName===n.tagName)i++;p+=':nth-of-type('+i+')';parts.unshift(p)}return parts.join(' > ') };
  const cleanup = () => {overlay.remove();document.removeEventListener('mousemove',move,true);document.removeEventListener('mousedown',down,true);document.removeEventListener('mouseup',up,true);document.removeEventListener('keydown',key,true)};
  const move = e => {if(start){moved=true;const x=Math.min(start.x,e.clientX),y=Math.min(start.y,e.clientY);position({x,y,width:Math.abs(e.clientX-start.x),height:Math.abs(e.clientY-start.y)})}else{const el=document.elementFromPoint(e.clientX,e.clientY);if(el){const r=el.getBoundingClientRect();position({x:r.x,y:r.y,width:r.width,height:r.height})}}};
  const down = e => {e.preventDefault();e.stopPropagation();start={x:e.clientX,y:e.clientY};moved=false};
  const up = e => {if(!start)return;e.preventDefault();e.stopPropagation();const x=Math.min(start.x,e.clientX),y=Math.min(start.y,e.clientY),w=Math.abs(e.clientX-start.x),h=Math.abs(e.clientY-start.y);const area=moved&&w>8&&h>8;const el=document.elementFromPoint(area?x+w/2:e.clientX,area?y+h/2:e.clientY);const r=el?.getBoundingClientRect();const result={kind:area?'area':'element',url:location.href,selector:el?selector(el):'',text:(el?.innerText||'').slice(0,180),rect:area?{x,y,width:w,height:h}:r?{x:r.x,y:r.y,width:r.width,height:r.height}:null,viewport:{width:innerWidth,height:innerHeight}};cleanup();resolve(result)};
  const key = e => {if(e.key==='Escape'){e.preventDefault();cleanup();resolve(null)}};
  document.addEventListener('mousemove',move,true);document.addEventListener('mousedown',down,true);document.addEventListener('mouseup',up,true);document.addEventListener('keydown',key,true);
})`;

function annotationUrl(url) {
  if (target?.kind !== 'ssh' || !localPort || !remotePort) return url;
  try {
    const parsed = new URL(url);
    if (parsed.hostname === '127.0.0.1' && Number(parsed.port) === localPort) {
      parsed.port = String(remotePort);
      return parsed.toString();
    }
  } catch { /* browser owns URL validation */ }
  return url;
}

function registerIpc() {
  ipcMain.handle('sui:update-check', async () => {
    if (process.platform !== 'darwin') return { supported: false };
    offeredUpdate = await updates.latest(app.getVersion(), process.arch);
    return { supported: true, available: offeredUpdate.available, version: offeredUpdate.version };
  });
  ipcMain.handle('sui:update-download', async () => {
    if (process.platform !== 'darwin' || !offeredUpdate?.available) throw new Error('Check for an update first.');
    const fresh = await updates.latest(app.getVersion(), process.arch);
    if (!fresh.available || fresh.version !== offeredUpdate.version) throw new Error('Release changed. Check again.');
    const file = await updates.download(fresh, app.getPath('temp'));
    const error = await shell.openPath(file);
    if (error) throw new Error(error);
    return true;
  });
  ipcMain.handle('sui:projects', () => readProjects());
  ipcMain.handle('sui:restore', () => readConnection());
  ipcMain.handle('sui:ssh-hosts', () => sshHosts(app.getPath('home')));
  ipcMain.handle('sui:pick-workspace', async () => {
    const selected = await dialog.showOpenDialog(window, { title: 'Choose your workspace', buttonLabel: 'Use this folder', properties: ['openDirectory'] });
    return selected.canceled ? null : selected.filePaths[0];
  });
  ipcMain.handle('sui:remote-folders', async (_event, host, directory, hidden) => {
    cancelFolderBrowse();
    const controller = new AbortController();
    folderBrowse = controller;
    try { return await remoteFolders(host, directory, hidden === true, { signal: controller.signal }); }
    finally { if (folderBrowse === controller) folderBrowse = undefined; }
  });
  ipcMain.handle('sui:cancel-folder-browse', () => { cancelFolderBrowse(); return true; });
  ipcMain.handle('sui:connect', (_event, item, resume) => startBridge(item, resume));
  ipcMain.handle('sui:configure', (_event, item, profile) => {
    if (activeRun !== null) throw new Error('Wait for the current turn to finish before changing providers.');
    return configureProfile(item, profile);
  });
  ipcMain.handle('sui:provider', (_event, item, request) => {
    if (activeRun !== null && ['login', 'select', 'logout'].includes(request?.action)) throw new Error('Wait for the current turn to finish before changing providers.');
    return providers.request(item, request);
  });
  ipcMain.handle('sui:cancel-provider', () => { providers.cancel(); return true; });
  ipcMain.handle('sui:manage-usage', () => shell.openExternal('https://chatgpt.com/settings/usage'));
  ipcMain.handle('sui:install-remote', (_event, item) => installRemoteSui(item));
  ipcMain.handle('sui:disconnect', async () => { await stopBridge(); return true; });
  ipcMain.handle('sui:command', (_event, value) => { writeBridge(value); return true; });
  ipcMain.handle('sui:open-browser', async (_event, url) => { showBrowser(); const address = validUrl(url); await browser.webContents.loadURL(address); return address; });
  ipcMain.handle('sui:preview-remote', (_event, port) => openRemotePreview(port));
  ipcMain.handle('sui:browser-bounds', (_event, bounds) => { browserBounds(bounds); return true; });
  ipcMain.handle('sui:browser-show', () => { showBrowser(); return true; });
  ipcMain.handle('sui:browser-hide', () => { if (window.contentView.children.includes(browser)) window.contentView.removeChildView(browser); return true; });
  ipcMain.handle('sui:chrome', (_event, theme) => {
    if (!CHROME[theme] || !window || window.isDestroyed()) return false;
    window.setBackgroundColor(CHROME[theme].color);
    if (process.platform === 'win32') window.setTitleBarOverlay({ ...CHROME[theme], height: TITLEBAR_HEIGHT });
    return true;
  });
  ipcMain.handle('sui:browser-back', () => { if (browser.webContents.navigationHistory.canGoBack()) browser.webContents.navigationHistory.goBack(); });
  ipcMain.handle('sui:browser-forward', () => { if (browser.webContents.navigationHistory.canGoForward()) browser.webContents.navigationHistory.goForward(); });
  ipcMain.handle('sui:browser-reload', () => browser.webContents.reload());
  ipcMain.handle('sui:annotate', async () => {
    if (!browser || browser.webContents.isDestroyed() || !browser.webContents.getURL()) throw new Error('Open a page first.');
    const selection = await browser.webContents.executeJavaScript(ANNOTATE_SCRIPT);
    if (!selection) return null;
    selection.url = annotationUrl(selection.url);
    return selection;
  });
}

// Window chrome follows the platform: macOS keeps its traffic lights inset in
// our title row, Windows draws its caption buttons over it, and Linux keeps
// the native frame (always works, whatever the compositor or window manager).
const CHROME = {
  dark: { color: '#0a0d12', symbolColor: '#aab5c6' },
  light: { color: '#eceff4', symbolColor: '#475266' }
};
const TITLEBAR_HEIGHT = 48;

function windowOptions() {
  const chrome = CHROME[nativeTheme.shouldUseDarkColors ? 'dark' : 'light'];
  const base = {
    width: 1440, height: 900, minWidth: 720, minHeight: 560, show: false,
    backgroundColor: chrome.color, title: 'SUI Desktop',
    webPreferences: { preload: path.join(__dirname, 'preload.js'), sandbox: true, contextIsolation: true, nodeIntegration: false }
  };
  if (process.platform === 'darwin') return { ...base, titleBarStyle: 'hiddenInset', trafficLightPosition: { x: 18, y: 17 } };
  if (process.platform === 'win32') return { ...base, titleBarStyle: 'hidden', titleBarOverlay: { ...chrome, height: TITLEBAR_HEIGHT } };
  return { ...base, autoHideMenuBar: true };
}

function createWindow() {
  window = new BrowserWindow(windowOptions());
  const current = window;
  window.once('ready-to-show', () => window.show());
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.webContents.on('will-navigate', event => event.preventDefault());
  window.loadFile(path.join(__dirname, 'index.html'));
  createBrowser();
  let closing = false;
  window.on('close', event => {
    if (bridge || closingBridges.size) {
      event.preventDefault();
      if (closing) return;
      closing = true;
      providers.cancel(); cancelFolderBrowse();
      stopBridge().finally(() => { if (!current.isDestroyed()) current.destroy(); });
    }
  });
  window.on('closed', () => { providers.cancel(); cancelFolderBrowse(); stopBridge(); if (browser) browser.webContents.close(); browser = undefined; window = undefined; });
}
app.whenReady().then(() => { app.setAppUserModelId('dev.sui.desktop'); registerIpc(); createWindow(); });
let quitting = false;
app.on('before-quit', event => {
  if (quitting) return;
  event.preventDefault();
  providers.cancel(); cancelFolderBrowse();
  stopBridge().finally(() => { quitting = true; app.quit(); });
});
app.on('window-all-closed', () => { if (process.platform !== 'darwin') app.quit(); });
app.on('activate', () => { if (!window && !quitting) createWindow(); });
