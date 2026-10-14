// Manual macOS updates until the app can be signed and notarized.
const { createHash } = require('node:crypto');
const { createWriteStream } = require('node:fs');
const fs = require('node:fs/promises');
const path = require('node:path');
const { Readable, Transform } = require('node:stream');
const { pipeline } = require('node:stream/promises');

const RELEASES = 'https://api.github.com/repos/fordft/sui/releases/latest';
const VERSION = /^\d+\.\d+\.\d+$/;
const MAX_DMG = 1024 * 1024 * 1024;

function newer(candidate, current) {
  if (!VERSION.test(candidate) || !VERSION.test(current)) return false;
  const a = candidate.split('.').map(Number);
  const b = current.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] > b[i];
  return false;
}

function asset(release, name) {
  const entry = release.assets?.find(item => item.name === name);
  const expected = `https://github.com/fordft/sui/releases/download/${release.tag_name}/${name}`;
  if (!entry || entry.browser_download_url !== expected) throw new Error(`Release is missing ${name}.`);
  return expected;
}

async function getText(url, limit, fetchImpl) {
  const response = await fetchImpl(url, { headers: { 'User-Agent': 'SUI-Desktop', Accept: 'application/vnd.github+json' }, signal: AbortSignal.timeout(30000) });
  if (!response.ok || !response.body) throw new Error(`Update server returned ${response.status}.`);
  const chunks = [];
  let size = 0;
  for await (const chunk of Readable.fromWeb(response.body)) {
    size += chunk.length;
    if (size > limit) throw new Error('Update metadata is too large.');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

async function latest(current, arch, fetchImpl = fetch) {
  if (!['arm64', 'x64'].includes(arch)) throw new Error('This Mac architecture is not supported.');
  const release = JSON.parse(await getText(RELEASES, 2 * 1024 * 1024, fetchImpl));
  const version = release.tag_name?.slice(1);
  if (!release.tag_name?.startsWith('v') || !VERSION.test(version) || release.draft || release.prerelease) throw new Error('No stable Desktop release is available.');
  if (!newer(version, current)) return { available: false, version: current };
  const name = `SUI.Desktop-${version}-${arch}.dmg`;
  return { available: true, version, name, url: asset(release, name), checksums: asset(release, `SUI-Desktop-${version}-SHA256SUMS.txt`) };
}

async function download(update, directory, fetchImpl = fetch) {
  if (!update?.available || !VERSION.test(update.version) || !/^SUI\.Desktop-\d+\.\d+\.\d+-(arm64|x64)\.dmg$/.test(update.name)) throw new Error('Invalid update.');
  // Reconstruct the URLs; renderer-provided URLs are never used.
  const tag = `v${update.version}`;
  const base = `https://github.com/fordft/sui/releases/download/${tag}/`;
  const manifest = await getText(base + `SUI-Desktop-${update.version}-SHA256SUMS.txt`, 1024 * 1024, fetchImpl);
  const line = manifest.split(/\r?\n/).find(row => row.endsWith(`  ${update.name}`));
  const expected = line?.match(/^([a-f0-9]{64})  /)?.[1];
  if (!expected) throw new Error('Release checksum is missing or invalid.');
  const response = await fetchImpl(base + update.name, { headers: { 'User-Agent': 'SUI-Desktop' }, signal: AbortSignal.timeout(180000) });
  if (!response.ok || !response.body) throw new Error(`Download returned ${response.status}.`);
  const folder = await fs.mkdtemp(path.join(directory, 'sui-desktop-update-'));
  const filename = path.join(folder, update.name);
  const digest = createHash('sha256');
  let size = 0;
  try {
    await pipeline(Readable.fromWeb(response.body), new Transform({ transform(chunk, _encoding, callback) {
      size += chunk.length;
      if (size > MAX_DMG) return callback(new Error('Update exceeds 1 GiB.'));
      digest.update(chunk);
      callback(null, chunk);
    } }), createWriteStream(filename, { flags: 'wx', mode: 0o600 }));
    if (digest.digest('hex') !== expected) throw new Error('Downloaded DMG failed its SHA-256 check.');
    return filename;
  } catch (error) {
    await fs.rm(folder, { recursive: true, force: true });
    throw error;
  }
}

module.exports = { newer, latest, download };
