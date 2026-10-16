const test = require('node:test');
const assert = require('node:assert/strict');
const { bridgeSpec, validateHost, remoteCommand } = require('../bridge');

test('SSH destination cannot become an option or shell fragment', () => {
  for (const bad of ['-oProxyCommand=evil', 'host; echo x', 'user@host name', '']) assert.throws(() => validateHost(bad));
  assert.equal(validateHost('dev@build-box.local'), 'dev@build-box.local');
});

test('remote workspace is quoted as one shell argument', () => {
  const command = remoteCommand("/home/a/b'c; touch /tmp/no", 'codex', null);
  assert.match(command, /'\/home\/a\/b'\\''c; touch \/tmp\/no'/);
  assert.match(command, /--profile 'codex'/);
});

test('bridge SSH uses noninteractive host verification and no shell interpolation locally', () => {
  const spec = bridgeSpec({ isPackaged: false }, { kind: 'ssh', host: 'user@box', workspace: '/srv/app', profile: 'codex' }, 'session-1');
  assert.equal(spec.command, 'ssh');
  assert.deepEqual(spec.args.slice(0, 7), ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12']);
  assert.match(spec.args.at(-1), /--resume 'session-1'/);
});

test('legacy host is rejected before any unknown command can become a chat prompt', () => {
  const fs = require('node:fs');
  const os = require('node:os');
  const path = require('node:path');
  const { spawnSync } = require('node:child_process');
  const { remoteSetupCommand } = require('../bridge');
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-legacy-'));
  try {
    fs.mkdirSync(path.join(home, '.sui/bin'), { recursive: true });
    fs.writeFileSync(path.join(home, '.sui/bin/sui'), '#!/bin/sh\nif [ "$1" = --help ]; then echo "Legacy CLI"; exit 0; fi\necho "MODEL REQUEST"; exit 99\n', { mode: 0o700 });
    for (const command of [remoteSetupCommand('desktop-provider'), remoteCommand('/tmp/work', 'test')]) {
      const result = spawnSync('sh', ['-c', command], { env: { ...process.env, HOME: home }, encoding: 'utf8' });
      assert.equal(result.status, 78);
      assert.match(result.stderr, /older SUI CLI/);
      assert.doesNotMatch(result.stdout, /MODEL REQUEST/);
    }
  } finally { fs.rmSync(home, { recursive: true, force: true }); }
});

test('reasoning uses argv for new chats and native stored settings for resume', () => {
  const target = { kind: 'local', workspace: '/tmp/work', profile: 'test', reasoning_effort: 'medium' };
  assert.deepEqual(bridgeSpec({ isPackaged: false }, target).args.slice(-2), ['--reasoning-effort', 'medium']);
  assert.ok(!bridgeSpec({ isPackaged: false }, target, 'saved').args.includes('--reasoning-effort'));
  assert.throws(() => bridgeSpec({ isPackaged: false }, { ...target, reasoning_effort: 'medium; touch /tmp/x' }));
});

test('account model selection is independent of its profile and quoted on SSH', () => {
  const target = { kind: 'local', workspace: '/tmp/work', profile: 'chatgpt', model: "model'$(false)", reasoning_effort: 'high' };
  const args = bridgeSpec({ isPackaged: false }, target).args;
  assert.deepEqual(args.slice(-4), ['--reasoning-effort', 'high', '--model', "model'$(false)"]);
  assert.ok(!bridgeSpec({ isPackaged: false }, target, 'saved').args.includes('--model'));
  assert.match(remoteCommand('/tmp/work', 'chatgpt', null, 'high', target.model), /--model 'model'\\''\$\(false\)'/);
});
