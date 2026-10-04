// Embedded, runtime-owned JSON-lines driver. No agent JS/shell evaluation.
const { chromium } = require('playwright');
const fs = require('node:fs');
const readline = require('node:readline');
let browser, context, page, terminalPage;
const remote = process.argv.includes('--allow-remote');
const limit = 16000;
const bounded = text => text.length > limit ? text.slice(0, limit) + '\n<truncated>' : text;
const privateFields = 'input[type="password"], [autocomplete*="password"], [data-sui-private]';
function allowed(raw, socket = false) {
  try {
    const u = new URL(raw);
    return !u.username && !u.password &&
      (socket ? ['ws:', 'wss:'] : ['http:', 'https:']).includes(u.protocol) &&
      (remote || ['localhost', '127.0.0.1', '[::1]'].includes(u.hostname));
  } catch { return false; }
}
const errors = [];
function note(text) {
  if (errors.length === 20) errors.shift();
  errors.push(String(text).length > 512 ? String(text).slice(0, 512) + '<truncated>' : String(text));
}
async function start() {
  if (browser) return;
  browser = await chromium.launch({ headless: true, args: remote ? [] : [
    '--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost',
    '--force-webrtc-ip-handling-policy=disable_non_proxied_udp',
  ] });
  context = await browser.newContext({
    viewport: { width: 1280, height: 800 }, serviceWorkers: 'block', acceptDownloads: false,
  });
  await context.route('**/*', async route => {
    if (!allowed(route.request().url())) {
      note('blocked request by browser network policy');
      return route.abort('blockedbyclient');
    }
    // Handle redirects one hop at a time; every destination is checked.
    try {
      const response = await route.fetch({ maxRedirects: 0, timeout: 15000 });
      await route.fulfill({ response });
    } catch { await route.abort('failed').catch(() => {}); }
  });
  await context.routeWebSocket('**/*', ws => {
    if (allowed(ws.url(), true)) ws.connectToServer();
    else { note('blocked websocket by browser network policy'); ws.close(); }
  });
  context.on('page', p => {
    p.setDefaultTimeout(10000);
    p.setDefaultNavigationTimeout(15000);
    p.on('pageerror', e => note(e.message));
    p.on('console', m => { if (m.type() === 'error') note(m.text()); });
    p.on('dialog', d => d.dismiss().catch(() => {}));
  });
  page = await context.newPage();
}
function target(a) {
  if (a.role) return page.getByRole(a.role, { name: a.name, exact: true });
  return page.locator(a.selector);
}
async function capture(p) {
  const image = await p.screenshot({ type: 'png', animations: 'disabled',
    mask: [p.locator(privateFields)], scale: 'css' });
  if (image.length > 4 * 1024 * 1024) throw new Error('screenshot exceeds 4 MiB limit');
  return image.toString('base64');
}
async function snapshot() {
  // Explicit private markers are excluded from both pixels and text.
  const nodes = await page.locator(privateFields).elementHandles();
  const previous = await Promise.all(nodes.map(n => n.getAttribute('aria-hidden')));
  try {
    await Promise.all(nodes.map(n => n.evaluate(e => e.setAttribute('aria-hidden', 'true'))));
    return await page.locator('body').ariaSnapshot();
  } finally {
    await Promise.all(nodes.map((n, i) => n.evaluate((e, value) => {
      if (value === null) e.removeAttribute('aria-hidden'); else e.setAttribute('aria-hidden', value);
    }, previous[i]).catch(() => {})));
    await Promise.all(nodes.map(n => n.dispose()));
  }
}
async function terminal(a) {
  await start();
  if (a.action === 'start') {
    if (terminalPage) await terminalPage.close();
    terminalPage = await context.newPage();
    await terminalPage.setContent('<html><head></head><body style="margin:0;background:#10141c"><div id="terminal"></div></body></html>');
    await terminalPage.addStyleTag({ content: fs.readFileSync(require.resolve('@xterm/xterm/css/xterm.css'), 'utf8') });
    await terminalPage.addScriptTag({ path: require.resolve('@xterm/xterm') });
    await terminalPage.evaluate(({ cols, rows }) => {
      window.term = new Terminal({ cols, rows, fontFamily: 'monospace', fontSize: 16,
        theme: { background: '#10141c', foreground: '#e8edf5' }, scrollback: 0 });
      term.open(document.getElementById('terminal'));
      window.replies = ''; term.onData(s => window.replies += s);
    }, a);
  }
  if (!terminalPage) throw new Error('no terminal session; call terminal start first');
  if (a.cols && a.rows) await terminalPage.evaluate(a => term.resize(a.cols, a.rows), a);
  if (a.output) await terminalPage.evaluate(output => new Promise(resolve => {
    const data = Uint8Array.from(atob(output), c => c.charCodeAt(0));
    term.write(data, resolve);
  }), a.output);
  if (a.action === 'type') await terminalPage.evaluate(text => term.paste(text), a.text);
  if (a.action === 'press') {
    await terminalPage.locator('.xterm-helper-textarea').focus();
    await terminalPage.keyboard.press(a.key);
  }
  if (a.action === 'feed') {
    const replies = await terminalPage.evaluate(() => {
      const result = window.replies || ''; window.replies = ''; return result;
    });
    return { text: '', replies };
  }
  const geometry = await terminalPage.locator('.xterm-screen').boundingBox();
  if (geometry) await terminalPage.setViewportSize({
    width: Math.max(1, Math.ceil(geometry.width)), height: Math.max(1, Math.ceil(geometry.height)),
  });
  // xterm replies to terminal queries are collected and sent back to the PTY.
  const replies = await terminalPage.evaluate(() => {
    const result = window.replies || ''; window.replies = ''; return result;
  });

  const text = await terminalPage.evaluate(() => {
    const b = term.buffer.active; const lines = [];
    for (let i = 0; i < term.rows; i++) lines.push(b.getLine(b.viewportY + i)?.translateToString(true) || '');
    return lines.join('\n');
  });
  const result = { text: bounded(text), replies };
  if (a.action === 'screenshot') result.image = await capture(terminalPage);
  if (a.action === 'close') { await terminalPage.close(); terminalPage = undefined; }
  return result;
}
async function execute(a) {
  if (a.tool === 'terminal') return terminal(a);
  if (a.action === 'close') {
    if (browser) await browser.close();
    browser = context = page = terminalPage = undefined;
    errors.length = 0;
    return { text: 'browser session closed' };
  }
  await start();
  switch (a.action) {
    case 'open':
      if (!allowed(a.url)) throw new Error('URL denied: only credential-free HTTP(S) loopback URLs are allowed unless browser.allow_remote is enabled');
      await page.goto(a.url, { waitUntil: 'domcontentloaded' }); break;
    case 'click': await target(a).click(); break;
    case 'fill':
      if (await target(a).evaluate((e, selector) => e.matches(selector) || e.closest('[data-sui-private]'), privateFields)) {
        throw new Error('private/password input is not accepted by this tool');
      }
      await target(a).fill(a.text); break;
    case 'press': await page.keyboard.press(a.key); break;
    case 'resize': await page.setViewportSize({ width: a.width, height: a.height }); break;
    case 'snapshot': break;
    case 'screenshot': return { text: 'browser viewport screenshot', image: await capture(page) };
    default: throw new Error('unknown browser action');
  }
  const text = await snapshot();
  return { text: bounded(text), errors: errors.splice(0), truncated: text.length > limit };
}
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let chain = Promise.resolve();
input.on('line', line => {
  chain = chain.then(async () => {
    try {
      const result = await execute(JSON.parse(line));
      process.stdout.write(JSON.stringify({ ok: true, ...result }) + '\n');
    } catch (error) {
      process.stdout.write(JSON.stringify({ ok: false, error: bounded(error.message) }) + '\n');
    }
  });
});
input.on('close', () => { chain.finally(async () => { if (browser) await browser.close(); }); });
