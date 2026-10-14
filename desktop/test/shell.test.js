// The renderer is plain files under a strict CSP; these checks catch the
// mistakes that otherwise only show up as a blank or half-working window.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const read = file => fs.readFileSync(path.join(root, file), 'utf8');
const html = read('index.html');
const pkg = JSON.parse(read('package.json'));

function covered(file) {
  return pkg.build.files.some(pattern => pattern === file || (pattern.endsWith('/*') && file.startsWith(pattern.slice(0, -1))));
}

test('every asset the page loads exists and is packaged', () => {
  const assets = [...html.matchAll(/(?:src|href)="([^"#]+)"/g)].map(m => m[1]).filter(a => !a.startsWith('http'));
  assert.ok(assets.length >= 6);
  for (const asset of assets) {
    assert.ok(fs.existsSync(path.join(root, asset)), `${asset} exists`);
    assert.ok(covered(asset), `${asset} is listed in build.files`);
  }
  const fonts = [...read('styles.css').matchAll(/url\("([^"]+)"\)/g)].map(m => m[1]);
  assert.ok(fonts.length >= 1);
  for (const font of fonts) { assert.ok(fs.existsSync(path.join(root, font)), font); assert.ok(covered(font), `${font} is packaged`); }
  for (const main of [pkg.main, 'preload.js', 'bridge.js', 'update.js']) assert.ok(covered(main), `${main} is packaged`);
  assert.ok(fs.existsSync(path.join(root, 'build/icon.svg')), 'the packaged app has a SUI icon');
});

test('markup stays inside the CSP: no inline script, style, or handlers', () => {
  assert.match(html, /script-src 'self'/);
  assert.doesNotMatch(html, /<script(?![^>]*\ssrc=)/);
  assert.doesNotMatch(html, /<style[\s>]/);
  assert.doesNotMatch(html, /\sstyle="/);
  assert.doesNotMatch(html, /\son[a-z]+="/);
});

test('every element id the scripts reach for exists in the page', () => {
  const ids = new Set([...html.matchAll(/\sid="([^"]+)"/g)].map(m => m[1]));
  for (const file of ['app.js', 'ui.js']) {
    const source = read(file);
    const used = [...source.matchAll(/\$\('([\w-]+)'\)/g), ...source.matchAll(/getElementById\('([\w-]+)'\)/g), ...source.matchAll(/\bfor \(const id of \[([^\]]+)\]/g)]
      .flatMap(m => (m[0].startsWith('for') ? [...m[1].matchAll(/'([\w-]+)'/g)].map(x => x[1]) : [m[1]]));
    assert.ok(used.length > 10, `${file} references ids`);
    for (const id of used) assert.ok(ids.has(id), `${file} uses #${id}`);
  }
  for (const id of ['connect-dialog', 'connect-button', 'connection-pill', 'browser-url', 'go', 'annotate', 'annotation-card', 'annotation-target', 'workspace', 'profile']) {
    assert.ok(ids.has(id), `the GUI smoke test needs #${id}`);
  }
});

test('the preload API covers every window.sui call the renderer makes', () => {
  const exposed = new Set([...read('preload.js').matchAll(/^\s{2}(\w+):/gm)].map(m => m[1]));
  const calls = new Set();
  for (const file of ['app.js', 'ui.js', 'boot.js']) for (const m of read(file).matchAll(/window\.sui\??\.(\w+)/g)) calls.add(m[1]);
  assert.ok(calls.size > 8);
  for (const call of calls) assert.ok(exposed.has(call), `window.sui.${call} is exposed by preload.js`);
});
