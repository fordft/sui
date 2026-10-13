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
