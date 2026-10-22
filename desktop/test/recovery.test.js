const test = require('node:test');
const assert = require('node:assert/strict');
const { actions } = require('../recovery');

test('terminal failures offer specific repairs without retrying commands', () => {
  assert.equal(actions('provider http 401', { connected: true, chatgpt: true })[0].label, 'Sign in again');
  assert.equal(actions('warning: no API key set', { connected: true })[0].id, 'configure');
  assert.equal(actions('provider http 400: unsupported reasoning', { connected: true })[0].id, 'model');
  assert.equal(actions('subscription_sharing_usage_limit', { connected: true, chatgpt: true })[0].id, 'usage');
  assert.equal(actions('Connection closed')[0].id, 'reconnect');
  assert.equal(actions('Update SUI for desktop-protocol', { ssh: true })[0].id, 'connection');
  for (const message of ['401', '429', '400', 'lost connection', 'unknown error']) {
    assert.ok(actions(message).every(action => ['configure', 'model', 'usage', 'reconnect', 'connection'].includes(action.id)));
  }
});
