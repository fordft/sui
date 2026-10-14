const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');
const { sshHosts, directoryCommand, parseDirectories, remoteFolders } = require('../workspaces');

async function fixture(fn) {
  const home = await fs.mkdtemp(path.join(os.tmpdir(), 'sui-folders-'));
  try { await fn(home); } finally { await fs.rm(home, { recursive: true, force: true }); }
}

test('SSH discovery reads literal aliases and nested, quoted, relative and glob Includes without evaluating Match commands', () => fixture(async home => {
  await fs.mkdir(path.join(home, '.ssh', 'hosts'), { recursive: true });
  await fs.mkdir(path.join(home, 'other configs'));
  await fs.writeFile(path.join(home, '.ssh', 'config'), `
    # Host ignored
    Host primary another *.example.org !excluded
      HostName ignored.example.org
      IdentityFile ~/.ssh/private-key
    Include hosts/*.conf "${home}/other configs/config"
    Host=equal # comment
    hOsT "quoted"
    Match exec "touch ${home}/must-not-exist"
    Include ~/other-missing-config
  `);
  await fs.writeFile(path.join(home, '.ssh', 'hosts', 'b.conf'), 'Host included\nInclude config\n');
  await fs.writeFile(path.join(home, '.ssh', 'hosts', 'a.conf'), 'Host sorted-first primary\n');
  await fs.writeFile(path.join(home, 'other configs', 'config'), 'Host external\n');
  const result = await sshHosts(home);
  assert.deepEqual(result.hosts, ['another', 'equal', 'external', 'included', 'primary', 'quoted', 'sorted-first']);
  assert.deepEqual(result.warnings, []);
  await assert.rejects(fs.stat(path.join(home, 'must-not-exist')), { code: 'ENOENT' });
}));

test('missing SSH config is an ordinary empty result; large configs give a visible warning', () => fixture(async home => {
  assert.deepEqual(await sshHosts(home), { hosts: [], warnings: [] });
  await fs.mkdir(path.join(home, '.ssh'));
  await fs.writeFile(path.join(home, '.ssh', 'config'), 'Host x\n'.repeat(40000));
  const result = await sshHosts(home);
  assert.deepEqual(result.hosts, []);
  assert.match(result.warnings.join(' '), /size limit/);
}));

test('SSH discovery caps alias count and reports truncation', () => fixture(async home => {
  await fs.mkdir(path.join(home, '.ssh'));
  await fs.writeFile(path.join(home, '.ssh', 'config'), Array.from({ length: 510 }, (_v, i) => `Host box-${i}`).join('\n'));
  const result = await sshHosts(home);
  assert.equal(result.hosts.length, 500);
  assert.match(result.warnings.join(' '), /500/);
}));

const localListing = (directory, hidden = false, env = process.env) => parseDirectories(execFileSync('sh', ['-c', directoryCommand(directory, hidden)], { encoding: 'utf8', env, stdio: ['ignore', 'pipe', 'pipe'] }));

test('remote directory script preserves spaces, quotes, Unicode, newlines and shell syntax as names; files are excluded', () => fixture(async home => {
  const names = ['my app', "quote's project", 'งานภาษาไทย', 'line\nbreak', '$(touch should-not-exist)', '-option'];
  for (const name of [...names, '.hidden']) await fs.mkdir(path.join(home, name));
  await fs.writeFile(path.join(home, 'not-a-folder'), '');
  const listing = localListing(home);
  assert.equal(listing.path, home);
  assert.deepEqual(new Set(listing.directories), new Set(names));
  assert.equal(listing.truncated, false);
  assert.ok(localListing(home, true).directories.includes('.hidden'));
  assert.equal(localListing(path.join(home, "quote's project")).directories.length, 0);
  assert.equal(localListing(`~/${path.relative(os.homedir(), path.join(home, 'my app'))}`).path, path.join(home, 'my app'));
  await assert.rejects(fs.stat(path.join(home, 'should-not-exist')), { code: 'ENOENT' });
}));

test('remote directory script bounds large listings and rejects missing or relative paths', () => fixture(async home => {
  await Promise.all(Array.from({ length: 505 }, (_v, i) => fs.mkdir(path.join(home, `folder-${i}`))));
  const result = localListing(home);
  assert.equal(result.directories.length, 500);
  assert.equal(result.truncated, true);
  assert.throws(() => localListing(path.join(home, 'missing')), /Command failed/);
  assert.throws(() => localListing('relative'), /Command failed/);
  assert.throws(() => directoryCommand('a\0b'), /Invalid folder/);
}));

test('folder protocol rejects partial or malformed output while tolerating a login banner', () => {
  assert.deepEqual(parseDirectories(['Welcome\nSUI-DIRS-1', '/', 'folder', '', '0', ''].join('\0')), { path: '/', parent: '/', directories: ['folder'], truncated: false });
  for (const text of ['', 'SUI-DIRS-1\0/home\0folder', ['SUI-DIRS-1', 'relative', '', '0', ''].join('\0'), ['SUI-DIRS-1', '/home', '../evil', '', '0', ''].join('\0')]) {
    assert.throws(() => parseDirectories(text), /folder listing/);
  }
});

test('SSH folder transport uses safe SSH options and returns real shell listing output', () => fixture(async home => {
  await fs.mkdir(path.join(home, 'project'));
  const result = await remoteFolders('configured-alias', home, false, { spawnImpl(command, args) {
    assert.equal(command, 'ssh');
    assert.deepEqual(args.slice(0, 9), ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12', '--', 'configured-alias']);
    return spawn('sh', ['-c', args.at(-1)], { stdio: ['ignore', 'pipe', 'pipe'] });
  } });
  assert.deepEqual(result.directories, ['project']);
}));

test('SSH listing handles authentication errors, timeout, cancellation and oversized output', async () => {
  const runScript = script => () => spawn(process.execPath, ['-e', script], { stdio: ['ignore', 'pipe', 'pipe'] });
  await assert.rejects(remoteFolders('host', '', false, { spawnImpl: runScript('process.stderr.write("Permission denied (publickey)"); process.exit(255)') }), /Permission denied.*\nCheck your SSH config/);
  await assert.rejects(remoteFolders('host', '', false, { timeout: 100, spawnImpl: runScript('setTimeout(()=>{},10000)') }), /timed out/);
  const controller = new AbortController();
  const pending = remoteFolders('host', '', false, { signal: controller.signal, spawnImpl: runScript('setTimeout(()=>{},10000)') });
  controller.abort();
  await assert.rejects(pending, /cancelled/);
  await assert.rejects(remoteFolders('host', '', false, { spawnImpl: runScript('process.stdout.write("x".repeat(2*1024*1024))') }), /exceeded/);
});
