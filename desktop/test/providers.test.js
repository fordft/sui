const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { PassThrough } = require('node:stream');
const { createProviders } = require('../providers');

function fixture(handle) {
  const requests = [], opened = [];
  const service = createProviders({
    openExternal: async url => opened.push(url),
    spawn: target => {
      const child = new EventEmitter();
      child.stdin = new PassThrough(); child.stdout = new PassThrough(); child.stderr = new PassThrough();
      child.kill = () => { setImmediate(() => child.emit('close', 1)); };
      let input = '';
      child.stdin.on('data', chunk => { input += chunk; });
      child.stdin.on('finish', () => setImmediate(() => {
        const request = JSON.parse(input);
        requests.push({ target, request });
        const result = handle(target, request, child);
        if (result !== undefined) {
          child.stdout.write(JSON.stringify(result) + '\n');
          child.emit('close', 0);
        }
      }));
      return child;
    }
  });
  return { service, requests, opened };
}

test('local ChatGPT sign-in opens a trusted browser URL and returns only display metadata', async () => {
  const { service, requests, opened } = fixture((_target, request, child) => {
    assert.equal(request.transfer, undefined);
    assert.equal(request.registration, undefined);
    child.stdout.write(JSON.stringify({ type: 'authorize', url: 'https://auth.openai.com/api/accounts/authorize?id_token_hint=private-hint' }) + '\n');
    return { type: 'account', account: { connected: true, sharing: true, email: 'test@example.com', access_token: 'private-access' } };
  });
  const result = await service.request({ kind: 'local' }, { action: 'login', name: 'chatgpt', transfer: true, registration: { access_token: 'renderer-injection' } });
  assert.equal(requests.length, 1);
  assert.equal(opened.length, 1);
  assert.equal(result.account.email, 'test@example.com');
  assert.ok(!JSON.stringify(result).includes('private-'));
});

test('SSH sign-in reauthorizes locally and transfers credentials only over private host stdio', async () => {
  const { service, requests } = fixture((target, request, child) => {
    if (request.action === 'registration') return { type: 'registration', record: { client_id: 'oaiapp_previous', id_token: 'private-hint' } };
    if (request.action === 'login') {
      assert.equal(target.kind, 'local');
      assert.equal(request.transfer, true);
      assert.equal(request.registration.client_id, 'oaiapp_previous');
      assert.match(request.name, /^transfer-/);
      child.stdout.write(JSON.stringify({ type: 'authorize', url: 'https://auth.openai.com/api/accounts/authorize' }) + '\n');
      return { type: 'transfer', record: { access_token: 'private-access', refresh_token: 'private-refresh' } };
    }
    assert.equal(target.host, 'work');
    assert.equal(request.action, 'import');
    assert.equal(request.record.refresh_token, 'private-refresh');
    return { type: 'account', account: { email: 'test@example.com', connected: true, sharing: true } };
  });
  const result = await service.request({ kind: 'ssh', host: 'work' }, { action: 'login', name: 'chatgpt' });
  assert.deepEqual(requests.map(r => r.request.action), ['registration', 'login', 'import']);
  assert.ok(!JSON.stringify(result).includes('private-'));
});

test('renderer cannot import credentials or replace the authorization origin', async () => {
  const { service } = fixture((_target, _request, child) => {
    child.stdout.write(JSON.stringify({ type: 'authorize', url: 'https://attacker.example/api/accounts/authorize' }) + '\n');
    return { type: 'account', account: {} };
  });
  await assert.rejects(service.request({ kind: 'local' }, { action: 'import', name: 'chatgpt', record: {} }), /Unsupported/);
  await assert.rejects(service.request({ kind: 'local' }, { action: 'login', name: '../escape' }), /profile name/);
  await assert.rejects(service.request({ kind: 'local' }, { action: 'login', name: 'chatgpt' }), /Untrusted authorization/);
});

test('cancelling sign-in kills the pending process and never returns a partial result', async () => {
  let started;
  const ready = new Promise(resolve => { started = resolve; });
  const { service } = fixture(() => { started(); });
  const pending = service.request({ kind: 'local' }, { action: 'login', name: 'chatgpt' });
  await ready;
  service.cancel();
  await assert.rejects(pending, /cancelled/);
});

test('malformed frames and oversized responses fail without echoing the payload', async () => {
  for (const payload of ['private-secret-not-json\n', 'x'.repeat(256 * 1024 + 1)]) {
    const { service } = fixture((_target, _request, child) => { child.stdout.write(payload); });
    await assert.rejects(service.request({ kind: 'local' }, { action: 'profiles' }), error => !error.message.includes('private-secret') && /response/i.test(error.message));
  }
});

test('old host CLI errors explain how to update instead of exposing a model authentication error', async () => {
  const { service } = fixture((_target, _request, child) => {
    child.stderr.write('older SUI CLI');
    child.emit('close', 78);
  });
  await assert.rejects(service.request({ kind: 'ssh', host: 'test' }, { action: 'profiles' }), /Install or update SUI on SSH host/);
});

test('catalog metadata survives the public boundary without tokens or model instructions', async () => {
  const { service, requests } = fixture(() => ({ type: 'models', cached: true, fetched_at: 123, warning: 'Using saved catalog', items: [{ id: 'exact-id', name: 'Display label', access_token: 'private-token', base_instructions: 'private-instructions', default_reasoning: 'high', reasoning_levels: [{ effort: 'high', description: 'Deep thinking', private_field: 'private-description' }] }] }));
  const result = await service.request({ kind: 'local' }, { action: 'models', name: 'chatgpt', refresh: true, access_token: 'private-injection' });
  assert.equal(requests[0].request.refresh, true);
  assert.equal(result.cached, true);
  assert.equal(result.items[0].reasoning_levels[0].effort, 'high');
  assert.equal(result.items[0].default_reasoning, 'high');
  assert.ok(!JSON.stringify(result).includes('private-'));
  assert.ok(!JSON.stringify(requests).includes('private-'));
});

test('provider repair receives the endpoint but no credentials or credential-bearing URLs', async () => {
  const urls = ['http://127.0.0.1:11434/v1', 'https://private-user:private-pass@example.test/v1', 'https://example.test/v1?key=private-query', 'https://example.test/#private-fragment', 'file:///private-path', 'invalid'];
  const { service } = fixture(() => ({ type: 'profiles', items: urls.map(base_url => ({ name: 'test', model: 'test-model', kind: 'ollama', base_url, api_key: 'private-key', key_env: 'private-env' })) }));
  const result = await service.request({ kind: 'local' }, { action: 'profiles' });
  assert.equal(result.items[0].base_url, urls[0]);
  assert.ok(result.items.slice(1).every(p => p.base_url === null));
  assert.ok(!JSON.stringify(result).includes('private-'));
});
