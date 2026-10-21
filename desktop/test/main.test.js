// Exercise the real main process and IPC handlers with process/window fixtures.
// No Electron runtime, network account or user configuration is needed.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const vm = require('node:vm');
const net = require('node:net');
const { EventEmitter } = require('node:events');
const { PassThrough } = require('node:stream');
const { createRequire } = require('node:module');

function childProcess() {
  const child = new EventEmitter();
  child.stdin = new PassThrough(); child.stdout = new PassThrough(); child.stderr = new PassThrough();
  let closed = false;
  child.close = code => { if (!closed) { closed = true; child.emit('close', code); } };
  child.kill = () => { child.killed = true; setImmediate(() => child.close(1)); };
  child.stdin.on('finish', () => setImmediate(() => child.close(0)));
  return child;
}

function fixture(t, options = {}) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-main-test-'));
  const handlers = new Map(), events = [], children = [], loads = [];
  const filename = path.join(__dirname, '../main.js');
  const requireMain = createRequire(filename);
  const electron = {
    app: { whenReady: () => new Promise(() => {}), on() {}, getPath: () => home },
    ipcMain: { handle: (name, fn) => handlers.set(name, fn) },
    shell: {}, nativeTheme: { shouldUseDarkColors: true }
  };
  const window = {
    isDestroyed: () => false, webContents: { send: (_channel, value) => events.push(value) },
    contentView: { children: [], addChildView(view) { this.children.push(view); } }
  };
  const browser = { webContents: { loadURL: async url => { loads.push(url); await options.loadURL?.(url); } } };
  const context = vm.createContext({
    Buffer, URL, AbortController, setTimeout, clearTimeout, process, __dirname: path.dirname(filename),
    fixtureWindow: window, fixtureBrowser: browser,
    require(name) {
      if (name === 'electron') return electron;
      if (name === './bridge') return { ...requireMain(name), spawnBridge() { const child = childProcess(); children.push(child); return child; } };
      if (name === 'node:child_process') return { spawn: (...args) => options.spawn(...args) };
      if (name === './providers') return { createProviders: () => ({ request: async (_target, request) => ({ action: request.action }), cancel() {} }) };
      return requireMain(name);
    }
  });
  vm.runInContext(fs.readFileSync(filename, 'utf8'), context, { filename });
  const api = vm.runInContext(`
    window = fixtureWindow; browser = fixtureBrowser; registerIpc();
    ({ startBridge, stopBridge, writeBridge, validUrl, waitForTunnel, freePort, openRemotePreview, stopTunnel,
      setTarget(value) { target = value; },
      state() { return { connected: !!bridge, activeRun, tunnel: !!tunnel, localPort, remotePort }; }
    })`, context);
  const ipc = (name, ...args) => handlers.get(`sui:${name}`)(null, ...args);
  async function connect() {
    const count = children.length;
    const pending = api.startBridge({ kind: 'local', workspace: home, profile: 'test' });
    while (children.length === count) await new Promise(setImmediate);
    const child = children.at(-1);
    child.stdout.write(JSON.stringify({ type: 'hello', workspace: home, model: 'test', run_id: 'session', history: [] }) + '\n');
    await pending;
    return child;
  }
  t.after(async () => { await api.stopBridge(); fs.rmSync(home, { recursive: true, force: true }); });
  return { api, ipc, events, children, loads, connect, connection: () => JSON.parse(fs.readFileSync(path.join(home, 'connection.json'))) };
}

test('bridge protocol failures reject startup or disconnect a live run exactly once', async t => {
  for (const payload of ['not-json\n', 'null\n', '[]\n', '{}\n', 'x'.repeat(1024 * 1024 + 1)]) {
    for (const connected of [false, true]) {
      const f = fixture(t);
      let child;
      let rejected;
      if (connected) { child = await f.connect(); f.api.writeBridge({ type: 'send', run: 7, text: 'hello' }); }
      else {
        const pending = f.api.startBridge({ kind: 'local', workspace: '/tmp', profile: 'test' });
        rejected = assert.rejects(pending, /SUI sent/);
        await new Promise(setImmediate);
        child = f.children.at(-1);
      }
      // Split the oversized frame to exercise accumulation across reads.
      const bytes = Buffer.from(payload);
      child.stdout.write(bytes.subarray(0, 600000)); child.stdout.write(bytes.subarray(600000));
      child.stdout.write('{"type":"run_done","run":7,"outcome":"done"}\n');
      if (rejected) await rejected;
      await new Promise(setImmediate);
      assert.equal(f.api.state().connected, false);
      assert.equal(f.api.state().activeRun, null);
      assert.equal(f.events.filter(e => e.type === 'disconnected').length, connected ? 1 : 0);
      assert.equal(f.events.filter(e => e.type === 'run_done').length, 0);
    }
  }
});

test('bridge accepts many bounded frames in a large read and preserves split UTF-8', async t => {
  const f = fixture(t), child = await f.connect();
  f.api.writeBridge({ type: 'send', run: 1, text: 'hello' });
  const unicode = Buffer.from(JSON.stringify({ type: 'delta', run: 1, text: 'สวัสดี' }) + '\n');
  const split = unicode.indexOf(Buffer.from('ส')) + 1;
  child.stdout.write(unicode.subarray(0, split)); child.stdout.write(unicode.subarray(split));
  assert.equal(f.events[0].text, 'สวัสดี');
  const frames = Array.from({ length: 1100 }, () => JSON.stringify({ type: 'delta', run: 1, text: 'x'.repeat(1024) }) + '\n').join('');
  assert.ok(Buffer.byteLength(frames) > 1024 * 1024);
  child.stdout.write(frames + '{"type":"run_done","run":1,"outcome":"done"}\n');
  assert.equal(f.api.state().connected, true);
  assert.equal(f.api.state().activeRun, null);
  assert.equal(f.events.length, 1102);
});

test('an old bridge error cannot disconnect its replacement', async t => {
  const f = fixture(t), old = await f.connect();
  const current = await f.connect();
  old.emit('error', new Error('late old process error'));
  f.api.writeBridge({ type: 'send', run: 1, text: 'hello' });
  current.stdout.write('{"type":"run_done","run":1}\n');
  assert.equal(f.api.state().connected, true);
  assert.equal(f.events.filter(e => e.type === 'disconnected').length, 0);
});

test('rejected submissions release provider access without stale events clearing another run', async t => {
  const f = fixture(t), child = await f.connect();
  f.api.writeBridge({ type: 'run_mission', run: 7, text: 'objective' });
  assert.throws(() => f.ipc('provider', {}, { action: 'select' }), /Wait for/);
  child.stdout.write('{"type":"send_error","run":7,"message":"not ready"}\n');
  assert.equal((await f.ipc('provider', {}, { action: 'select' })).action, 'select');
  assert.equal(f.connection().session, null, 'a rejected first submission does not create a saved chat');
  f.api.writeBridge({ type: 'send', run: 8, text: 'hello' });
  child.stdout.write('{"type":"send_error","run":7}\n{"type":"run_done","run":7}\n');
  assert.equal(f.api.state().activeRun, 8);
  child.stdout.write('{"type":"run_done","run":8}\n');
  f.api.writeBridge({ type: 'send', run: 9, text: 'next' });
  child.stdout.write('{"type":"send_error","run":9}\n');
  assert.equal(f.api.state().activeRun, null);
  assert.equal(f.connection().session, 'session', 'rejection preserves an existing chat');
});

test('preview normalizes bare addresses while rejecting non-web URLs and credentials', async t => {
  const f = fixture(t);
  for (const [input, expected] of [
    [' www.google.com ', 'https://www.google.com/'], ['example.com:8443/path', 'https://example.com:8443/path'],
    ['localhost:5173', 'http://localhost:5173/'], ['127.0.0.1:3000/a', 'http://127.0.0.1:3000/a'],
    ['[::1]:3000', 'http://[::1]:3000/'], ['//example.com/path', 'https://example.com/path'],
    ['https://localhost:5173/path', 'https://localhost:5173/path']
  ]) assert.equal(await f.ipc('open-browser', input), expected);
  const loaded = f.loads.length;
  for (const input of ['', null, 'javascript:alert(1)', 'data:text/html,hi', 'file:///etc/passwd', 'ftp://example.com', 'https://user:password@example.com', 'user:password@example.com']) {
    await assert.rejects(f.ipc('open-browser', input));
  }
  assert.equal(f.loads.length, loaded);
});

test('SSH preview waits for a slow tunnel before loading the page', async t => {
  let ready = false, child;
  const server = net.createServer(socket => socket.end());
  t.after(() => new Promise(resolve => server.close(resolve)));
  const f = fixture(t, {
    spawn(command, args, options) {
      assert.equal(command, 'ssh'); assert.equal(options.stdio[2], 'pipe');
      assert.ok(args.includes('ExitOnForwardFailure=yes'));
      child = childProcess();
      const port = Number(args[args.indexOf('-L') + 1].split(':')[1]);
      setTimeout(() => server.listen(port, '127.0.0.1', () => { ready = true; }), 550);
      return child;
    },
    loadURL() { assert.equal(ready, true, 'loadURL must follow tunnel readiness'); }
  });
  f.api.setTarget({ kind: 'ssh', host: 'fixture' });
  const result = await f.api.openRemotePreview(5173);
  assert.equal(result.remotePort, 5173);
  assert.equal(f.loads.length, 1);
  child.stderr.write('fixture disconnect'); child.close(255);
  assert.match(f.events.at(-1).message, /fixture disconnect/);
  assert.equal(f.api.state().tunnel, false);
});

test('SSH startup reports authentication failure and never loads a page', async t => {
  let child;
  const f = fixture(t, { spawn() {
    child = childProcess();
    setImmediate(() => { child.stderr.write('Permission denied (publickey)'); child.close(255); });
    return child;
  } });
  f.api.setTarget({ kind: 'ssh', host: 'fixture' });
  await assert.rejects(f.api.openRemotePreview(5173), /Permission denied \(publickey\)/);
  assert.equal(f.loads.length, 0);
  assert.equal(f.api.state().tunnel, false);
  assert.equal(child.killed, true);
});

test('tunnel readiness times out and cancellation removes pending probes', async t => {
  const f = fixture(t), child = childProcess(), controller = new AbortController();
  const port = await f.api.freePort();
  await assert.rejects(f.api.waitForTunnel(child, port, controller.signal, () => '', 100), /did not become ready/);
  assert.equal(child.listenerCount('close'), 0);
  const pending = f.api.waitForTunnel(child, port, controller.signal, () => '');
  controller.abort();
  await assert.rejects(pending, /cancelled/);
  assert.equal(child.listenerCount('error'), 0);
});

test('replacing a starting SSH preview cancels the old attempt without closing the new one', async t => {
  const children = [];
  const f = fixture(t, { spawn() { const child = childProcess(); children.push(child); return child; } });
  f.api.setTarget({ kind: 'ssh', host: 'fixture' });
  const first = assert.rejects(f.api.openRemotePreview(3000), /cancelled/);
  while (!children.length) await new Promise(setImmediate);
  const second = assert.rejects(f.api.openRemotePreview(5173), /second fixture failure/);
  while (children.length < 2) await new Promise(setImmediate);
  await first;
  assert.equal(children[0].killed, true);
  assert.notEqual(children[1].killed, true);
  children[1].stderr.write('second fixture failure'); children[1].close(255);
  await second;
  assert.equal(f.loads.length, 0);
});
