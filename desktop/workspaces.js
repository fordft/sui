const fs = require('node:fs/promises');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { validateHost, shellQuote, sshArgs } = require('./bridge');

// Only discover literal aliases. OpenSSH owns all connection settings, Match
// rules and authentication; discovery never evaluates commands in the config.
async function sshHosts(home) {
  const hosts = new Set(), visited = new Set(), warnings = new Set();
  let bytes = 0;
  async function read(file, root = false) {
    let real;
    try { real = await fs.realpath(file); }
    catch (error) {
      if (!root || error.code !== 'ENOENT') warnings.add('Some SSH config files could not be read.');
      return;
    }
    if (visited.has(real)) return;
    if (visited.size >= 64) { warnings.add('SSH config discovery stopped at 64 files.'); return; }
    visited.add(real);
    const handle = await fs.open(real, 'r');
    let source;
    try {
      const info = await handle.stat();
      if (!info.isFile() || info.size > 256 * 1024 || bytes + info.size > 2 * 1024 * 1024) {
        warnings.add('An SSH config file exceeded the discovery size limit.'); return;
      }
      bytes += info.size;
      // Read at most the limit, even if a file grows after stat.
      const buffer = Buffer.alloc(256 * 1024 + 1);
      const result = await handle.read(buffer, 0, buffer.length, 0);
      if (result.bytesRead > 256 * 1024) { warnings.add('An SSH config file exceeded the discovery size limit.'); return; }
      source = buffer.toString('utf8', 0, result.bytesRead);
    } finally { await handle.close(); }
    for (const line of source.split(/\r?\n/)) {
      const match = line.match(/^\s*(host|include)\s*(?:=\s*|\s+)(.*)$/i);
      if (!match) continue;
      const values = [];
      // SSH config quotes preserve spaces; a comment starts outside quotes.
      let token = '', quoted = false;
      for (const ch of match[2]) {
        if (ch === '"') { quoted = !quoted; continue; }
        if (ch === '#' && !quoted) break;
        if (/\s/.test(ch) && !quoted) { if (token) values.push(token); token = ''; }
        else token += ch;
      }
      if (token) values.push(token);
      for (const value of values) {
        if (match[1].toLowerCase() === 'host') {
          try {
            validateHost(value);
            if (!value.includes('@')) {
              if (hosts.size < 500) hosts.add(value);
              else if (!hosts.has(value)) warnings.add('Showing the first 500 SSH aliases.');
            }
          } catch { /* wildcard / negated patterns are not destinations */ }
        } else {
          let expanded = value.replace(/^~(?=\/|$)/, home).replaceAll('%d', home);
          expanded = expanded.replace(/\$\{([^}]+)\}/g, (_all, name) => process.env[name] || '');
          const pattern = path.isAbsolute(expanded) ? expanded : path.join(home, '.ssh', expanded);
          let count = 0;
          try {
            for await (const included of fs.glob(pattern)) {
              if (++count > 64) { warnings.add('An SSH Include matched more than 64 files.'); break; }
              await read(included);
            }
          } catch { warnings.add('Some SSH Include paths could not be read.'); }
        }
      }
    }
  }
  try { await read(path.join(home, '.ssh', 'config'), true); }
  catch { warnings.add('SSH config could not be read. You can still enter a host.'); }
  return { hosts: [...hosts].sort((a, b) => a.localeCompare(b)), warnings: [...warnings] };
}

// NUL framing keeps spaces, quotes and newlines in names as data. Listing needs
// only a POSIX shell on the host, so it also works before SUI is installed.
const DIRECTORY_SCRIPT = `set -eu
dir=$1
case "$dir" in
  ''|'~') dir=$HOME ;;
  '~/'*) dir=$HOME/\${dir#??} ;;
  /*) ;;
  *) printf '%s\\n' 'Use an absolute folder path or ~/path.' >&2; exit 1 ;;
esac
CDPATH= cd -P -- "$dir"
if [ ! -r . ] || [ ! -x . ]; then printf '%s\\n' 'This folder cannot be read.' >&2; exit 1; fi
printf 'SUI-DIRS-1\\000%s\\000' "$PWD"
count=0
more=0
for entry in ./* ./.[!.]* ./..?*; do
  [ -d "$entry" ] || continue
  name=\${entry#./}
  case "$name" in .*) [ "$2" = 1 ] || continue ;; esac
  if [ "$count" -ge 500 ]; then more=1; break; fi
  printf '%s\\000' "$name"
  count=$((count + 1))
done
printf '\\000%s\\000' "$more"`;

function directoryCommand(directory = '', hidden = false) {
  if (typeof directory !== 'string' || directory.length > 8192 || directory.includes('\0')) throw new Error('Invalid folder path.');
  return `sh -c ${shellQuote(DIRECTORY_SCRIPT)} sh ${shellQuote(directory || '~')} ${hidden ? '1' : '0'}`;
}

function parseDirectories(output) {
  const marker = output.indexOf('SUI-DIRS-1\0');
  const fields = output.slice(marker).split('\0');
  if (marker < 0 || fields[0] !== 'SUI-DIRS-1' || !fields[1]?.startsWith('/') || fields.at(-1) !== '' || !['0', '1'].includes(fields.at(-2)) || fields.at(-3) !== '') {
    throw new Error('The SSH host returned an incomplete folder listing.');
  }
  const names = fields.slice(2, -3);
  if (names.length > 500 || names.some(name => !name || name.includes('/') || name === '.' || name === '..')) throw new Error('The SSH host returned an invalid folder listing.');
  return { path: fields[1], parent: path.posix.dirname(fields[1]), directories: names.sort((a, b) => a.localeCompare(b)), truncated: fields.at(-2) === '1' };
}

function remoteFolders(host, directory, hidden, { signal, spawnImpl = spawn, timeout = 20000 } = {}) {
  const args = sshArgs(host, directoryCommand(directory, hidden));
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error('Folder browsing cancelled.')); return; }
    const child = spawnImpl('ssh', args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '', bytes = 0, settled = false;
    const timer = setTimeout(() => done(new Error('SSH folder browsing timed out. Check that the host is reachable.')), timeout);
    const abort = () => done(new Error('Folder browsing cancelled.'));
    function done(error, value) {
      if (settled) return;
      settled = true; clearTimeout(timer);
      signal?.removeEventListener('abort', abort);
      if (error) { child.kill(); reject(error); } else resolve(value);
    }
    signal?.addEventListener('abort', abort, { once: true });
    child.on('error', done);
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      bytes += Buffer.byteLength(chunk);
      if (bytes > 1024 * 1024) { done(new Error('The SSH folder listing exceeded 1 MiB.')); return; }
      stdout += chunk;
    });
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
    child.on('close', code => {
      if (settled) return;
      if (code !== 0) {
        const detail = stderr.trim() || `SSH exited with code ${code}.`;
        done(new Error(`${detail}\nCheck your SSH config and loaded keys (ssh-agent / macOS Keychain).`));
      } else {
        try { done(null, parseDirectories(stdout)); } catch (error) { done(error); }
      }
    });
  });
}

module.exports = { sshHosts, directoryCommand, parseDirectories, remoteFolders };
