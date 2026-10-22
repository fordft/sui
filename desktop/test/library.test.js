const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createLibrary } = require('../library');
const scope = { kind: 'ssh', host: 'work', workspace: '/code/project', conversation: 'stable-chat' };
const empty = { text: '', images: [], annotation: null, comment: '' };
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sui-drafts-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return { root, store: createLibrary(root) };
}
test('drafts, attachments and labels survive restart and stay within a chat/workspace/host', t => {
  const { root, store } = fixture(t);
  const pixels = { name: 'area.png', mime: 'image/png', data: Buffer.from('fixture pixels').toString('base64') };
  const draft = { ...empty, text: 'Continue tomorrow', images: [pixels], annotation: { kind: 'element', url: 'http://localhost:3000', selector: '#area', text: 'label', rect: { x: 1,y: 2,width: 3,height: 4 }, viewport: { width: 800,height: 600 } }, comment: 'Make this wider' };
  store.write(scope, { draft, title: 'My project', pinned: true, connection: { profile: 'chatgpt', run: 'native-run' } });
  const restored = createLibrary(root).read(scope);
  assert.equal(restored.draft.images[0].data, pixels.data);
  assert.equal(restored.draft.comment, draft.comment);
  for (const field of ['host','workspace','conversation']) assert.equal(store.read({ ...scope, [field]: field === 'workspace' ? '/code/another' : 'another' }), null);
  assert.equal(store.list({ ...scope, host: 'another' }).items.length, 0);
  assert.equal(store.list(scope).items[0].pinned, true);
  assert.ok(!JSON.stringify(store.list(scope)).includes(pixels.data));
  store.write(scope, { draft: empty });
  assert.equal(store.read(scope).title, 'My project');
  assert.equal(store.list(scope).items[0].hasDraft, false);
  store.write(scope, { title: '', pinned: false });
  assert.equal(store.read(scope), null);
});
test('invalid or oversized saves leave the last saved draft intact', t => {
  const { root, store } = fixture(t);
  store.write(scope, { draft: { ...empty, text: 'keep' } });
  for (const draft of [{ ...empty, text: 'x'.repeat(128*1024+1) }, { ...empty, images: [{ name:'x',mime:'image/png',data:'%%%'}] }, { ...empty, images: Array(5).fill({}) }]) assert.throws(() => store.write(scope, { draft }));
  assert.equal(store.read(scope).draft.text, 'keep');
  const file = path.join(root, 'chats', fs.readdirSync(path.join(root, 'chats'))[0]);
  if (process.platform !== 'win32') {
    assert.equal(fs.statSync(file).mode & 0o777, 0o600);
    assert.equal(fs.statSync(path.dirname(file)).mode & 0o777, 0o700);
  }
  fs.writeFileSync(file, 'broken');
  assert.throws(() => store.write(scope, { draft: empty }));
  assert.equal(fs.readFileSync(file, 'utf8'), 'broken');
});
test('storage never follows a substituted directory or record symlink', { skip: process.platform === 'win32' }, t => {
  const { root, store } = fixture(t);
  store.write(scope, { draft: { ...empty, text: 'keep' } });
  const directory = path.join(root, 'chats'), original = path.join(root, 'old');
  fs.renameSync(directory, original); fs.symlinkSync(original, directory);
  assert.throws(() => store.read(scope)); assert.throws(() => store.write(scope, { draft: empty }));
  fs.unlinkSync(directory); fs.renameSync(original, directory);
  const file = path.join(directory, fs.readdirSync(directory)[0]), other = path.join(root, 'other');
  fs.renameSync(file, other); fs.symlinkSync(other, file);
  assert.throws(() => store.read(scope)); assert.throws(() => store.list(scope));
  assert.ok(fs.readFileSync(other, 'utf8').includes('keep'));
});
