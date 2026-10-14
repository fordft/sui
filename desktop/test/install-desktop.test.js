const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createHash } = require('node:crypto');
const { spawnSync } = require('node:child_process');

const installer = path.resolve(__dirname, '../../install-desktop.sh');
const version = require('../package.json').version;

// Use real shell, checksum, staging, and cleanup behavior. Only network and
// macOS utilities are substituted; the Mac workflow verifies real signatures.
function fixture(t, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-desktop-install-test-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const bin = path.join(root, 'bin');
  const prefix = path.join(root, 'My Applications');
  const temp = path.join(root, 'temp');
  const destination = path.join(prefix, 'SUI Desktop.app');
  for (const folder of [bin, temp, destination]) fs.mkdirSync(folder, { recursive: true });
  fs.writeFileSync(path.join(destination, 'existing'), 'old SUI');
  const dmg = 'test DMG bytes';
  const checksum = createHash('sha256').update(dmg).digest('hex');
  const mock = path.join(root, 'mock.js');
  fs.writeFileSync(mock, `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const name = path.basename(process.argv[1]);
const args = process.argv.slice(2);
const env = process.env;
fs.appendFileSync(env.MOCK_LOG, JSON.stringify({ name, args }) + '\\n');
if (env.MOCK_FAIL === name) process.exit(1);
switch (name) {
  case 'uname': console.log(args[0] === '-s' ? (env.MOCK_OS || 'Darwin') : env.MOCK_CPU); break;
  case 'curl': {
    const url = args.at(-1);
    if (url.endsWith('/latest')) { process.stdout.write('https://github.com/fordft/sui/releases/tag/v' + env.MOCK_VERSION); break; }
    const filename = args[args.indexOf('-o') + 1];
    const asset = 'SUI.Desktop-' + env.MOCK_VERSION + '-' + (env.MOCK_CPU === 'arm64' ? 'arm64' : 'x64') + '.dmg';
    if (url.endsWith('-SHA256SUMS.txt')) {
      let line = env.MOCK_CHECKSUM + '  ' + asset + '\\n';
      if (env.MOCK_MANIFEST === 'missing') line = env.MOCK_CHECKSUM + '  other.dmg\\n';
      if (env.MOCK_MANIFEST === 'duplicate') line += line;
      fs.writeFileSync(filename, line);
    } else fs.writeFileSync(filename, env.MOCK_DMG);
    break;
  }
  case 'hdiutil': {
    if (args[0] === 'detach') break;
    const bundle = path.join(args[args.indexOf('-mountpoint') + 1], 'SUI Desktop.app');
    fs.mkdirSync(path.join(bundle, 'Contents/MacOS'), { recursive: true });
    fs.mkdirSync(path.join(bundle, 'Contents/Resources'), { recursive: true });
    fs.writeFileSync(path.join(bundle, 'Contents/Info.plist'), 'test plist');
    fs.writeFileSync(path.join(bundle, 'Contents/MacOS/SUI Desktop'), 'test Electron binary');
    fs.writeFileSync(path.join(bundle, 'Contents/Resources/sui'), '#!/bin/sh\\nprintf "sui ' + (env.MOCK_BINARY_VERSION || env.MOCK_VERSION) + '\\n"\\n', { mode: 0o755 });
    break;
  }
  case 'plutil': console.log(env.MOCK_APP_ID || 'dev.sui.desktop'); break;
  case 'lipo': case 'codesign': case 'xattr': break;
  case 'ditto': fs.cpSync(args[0], args[1], { recursive: true }); break;
  case 'mv':
    if (env.MOCK_REPLACE_FAIL && args[0].includes('/.sui-desktop.') && args[0].endsWith('/SUI Desktop.app')) process.exit(1);
    fs.renameSync(args[0], args[1]);
    break;
  default: throw new Error('unexpected command: ' + name);
}
`, { mode: 0o755 });
  for (const name of ['uname', 'curl', 'hdiutil', 'plutil', 'lipo', 'codesign', 'ditto', 'xattr', 'mv']) fs.symlinkSync(mock, path.join(bin, name));
  const log = path.join(root, 'calls.jsonl');
  const env = {
    ...process.env, PATH: bin + path.delimiter + process.env.PATH, TMPDIR: temp,
    SUI_TAG: `v${version}`, MOCK_LOG: log, MOCK_VERSION: version,
    MOCK_CPU: 'arm64', MOCK_CHECKSUM: checksum, MOCK_DMG: dmg, ...options,
  };
  return {
    root, prefix, temp, destination,
    run: () => spawnSync('sh', [installer, prefix], { env, encoding: 'utf8', timeout: 10000 }),
    calls: () => fs.existsSync(log) ? fs.readFileSync(log, 'utf8').trim().split('\n').map(JSON.parse) : [],
    oldApp: () => assert.equal(fs.readFileSync(path.join(destination, 'existing'), 'utf8'), 'old SUI'),
    cleaned: () => {
      assert.deepEqual(fs.readdirSync(temp), []);
      assert.deepEqual(fs.readdirSync(prefix), ['SUI Desktop.app']);
    },
  };
}

for (const cpu of ['arm64', 'x86_64']) test(`installs a verified ${cpu} release and clears quarantine only on the staged SUI app`, t => {
  const f = fixture(t, { MOCK_CPU: cpu, SUI_TAG: 'latest' });
  const result = f.run();
  assert.equal(result.status, 0, result.stderr);
  assert.ok(fs.existsSync(path.join(f.destination, 'Contents/Resources/sui')));
  assert.equal(fs.existsSync(path.join(f.destination, 'existing')), false);
  const calls = f.calls();
  const xattrs = calls.filter(call => call.name === 'xattr');
  assert.equal(xattrs.length, 1);
  assert.deepEqual(xattrs[0].args.slice(0, 2), ['-dr', 'com.apple.quarantine']);
  assert.ok(xattrs[0].args[2].startsWith(f.prefix + '/.sui-desktop.'));
  assert.ok(xattrs[0].args[2].endsWith('/SUI Desktop.app'));
  assert.equal(calls.filter(call => call.name === 'codesign').length, 3);
  assert.ok(calls.filter(call => call.name === 'lipo').every(call => call.args.at(-1) === cpu));
  assert.ok(calls.some(call => call.name === 'hdiutil' && call.args[0] === 'detach'));
  f.cleaned();
});

for (const options of [
  { MOCK_CHECKSUM: '0'.repeat(64) },
  { MOCK_MANIFEST: 'missing' },
  { MOCK_MANIFEST: 'duplicate' },
]) test(`invalid download proof never mounts or modifies an app: ${JSON.stringify(options)}`, t => {
  const f = fixture(t, options);
  assert.notEqual(f.run().status, 0);
  assert.ok(f.calls().every(call => !['hdiutil', 'ditto', 'xattr', 'mv'].includes(call.name)));
  f.oldApp();
  f.cleaned();
});

for (const command of ['lipo', 'codesign', 'ditto', 'xattr']) test(`${command} failure preserves the existing installation`, t => {
  const f = fixture(t, { MOCK_FAIL: command });
  assert.notEqual(f.run().status, 0);
  assert.ok(f.calls().every(call => call.name !== 'mv'));
  if (['lipo', 'codesign', 'ditto'].includes(command)) assert.ok(f.calls().every(call => call.name !== 'xattr'));
  f.oldApp();
  f.cleaned();
});

test('a bundled SUI version mismatch preserves the existing installation', t => {
  const f = fixture(t, { MOCK_BINARY_VERSION: '0.0.1' });
  const result = f.run();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /bundled SUI could not run or has the wrong version/);
  f.oldApp();
  f.cleaned();
});

test('replacement failure restores the previous app', t => {
  const f = fixture(t, { MOCK_REPLACE_FAIL: '1' });
  assert.notEqual(f.run().status, 0);
  assert.ok(f.calls().some(call => call.name === 'mv' && call.args[0].endsWith('/previous.app')));
  f.oldApp();
  f.cleaned();
});

test('an unrelated bundle is rejected before any quarantine change', t => {
  const f = fixture(t, { MOCK_APP_ID: 'com.example.other' });
  assert.notEqual(f.run().status, 0);
  assert.ok(f.calls().every(call => !['ditto', 'xattr', 'mv'].includes(call.name)));
  f.oldApp();
  f.cleaned();
});

test('a symbolic-link destination is never replaced or followed', t => {
  const f = fixture(t);
  const other = path.join(f.root, 'other.app');
  fs.renameSync(f.destination, other);
  fs.symlinkSync(other, f.destination);
  assert.notEqual(f.run().status, 0);
  assert.equal(fs.readlinkSync(f.destination), other);
  assert.ok(f.calls().every(call => !['ditto', 'xattr', 'mv'].includes(call.name)));
  f.oldApp();
  f.cleaned();
});

for (const options of [{ MOCK_OS: 'Linux' }, { SUI_TAG: '../untrusted' }, { SUI_TAG: `v${version}\nother` }]) test(`unsupported platform or invalid tag is rejected before downloading: ${JSON.stringify(options)}`, t => {
  const f = fixture(t, options);
  assert.notEqual(f.run().status, 0);
  assert.ok(f.calls().every(call => call.name !== 'curl'));
  f.oldApp();
  f.cleaned();
});
