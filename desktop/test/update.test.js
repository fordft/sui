const test = require('node:test');
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { newer, latest, download } = require('../update');

const version = '0.11.4';
const tag = `v${version}`;
const name = `SUI.Desktop-${version}-arm64.dmg`;
const base = `https://github.com/fordft/sui/releases/download/${tag}/`;
const sums = `SUI-Desktop-${version}-SHA256SUMS.txt`;
const release = {
  tag_name: tag,
  assets: [name, sums].map(filename => ({ name: filename, browser_download_url: base + filename }))
};

test('version ordering uses numeric semver and rejects prereleases', () => {
  assert.equal(newer('0.11.4', '0.11.3'), true);
  assert.equal(newer('0.12.0', '0.11.99'), true);
  assert.equal(newer('0.11.3', '0.11.4'), false);
  assert.equal(newer('0.11.4-beta', '0.11.3'), false);
});

test('update selection requires the matching architecture and exact release asset', async () => {
  const fetchRelease = async () => new Response(JSON.stringify(release));
  assert.deepEqual(await latest('0.11.4', 'arm64', fetchRelease), { available: false, version: '0.11.4' });
  assert.equal((await latest('0.11.3', 'arm64', fetchRelease)).name, name);
  await assert.rejects(latest('0.11.3', 'x64', fetchRelease), /missing/);
  await assert.rejects(latest('0.11.3', 'arm64', async () => new Response(JSON.stringify({ ...release, assets: [{ ...release.assets[0], browser_download_url: 'https://example.org/file' }, release.assets[1]] }))), /missing/);
});

test('download verifies checksum and removes a mismatched DMG', async () => {
  const folder = await fs.mkdtemp(path.join(os.tmpdir(), 'sui-update-test-'));
  const bytes = Buffer.from('fake DMG for transfer test');
  const hash = createHash('sha256').update(bytes).digest('hex');
  const fetchAsset = expected => async url => new Response(url.endsWith(sums) ? `${expected}  ${name}\n` : bytes);
  const update = { available: true, version, name };
  try {
    const filename = await download(update, folder, fetchAsset(hash));
    assert.deepEqual(await fs.readFile(filename), bytes);
    await assert.rejects(download(update, folder, fetchAsset('0'.repeat(64))), /SHA-256/);
    assert.equal((await fs.readdir(folder)).length, 1, 'failed download leaves no DMG');
  } finally { await fs.rm(folder, { recursive: true, force: true }); }
});
