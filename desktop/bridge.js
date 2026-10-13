const { spawn } = require('node:child_process');
const path = require('node:path');

function validateHost(host) {
  if (typeof host !== 'string' || !/^(?:[A-Za-z_][A-Za-z0-9_.-]*@)?[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(host)) {
    throw new Error('Use an SSH host name, IP address, or user@host.');
  }
  return host;
}

function shellQuote(value) {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0')) throw new Error('Invalid SSH argument');
  return "'" + value.replaceAll("'", "'\\''") + "'";
}

function remoteCommand(workspace, profile, resume) {
  const args = ` desktop-bridge --workspace ${shellQuote(workspace)} --profile ${shellQuote(profile)}` +
    (resume ? ` --resume ${shellQuote(resume)}` : '');
  return `if test -x "$HOME/.sui/bin/sui"; then exec "$HOME/.sui/bin/sui"${args}; ` +
    `elif command -v sui >/dev/null 2>&1; then exec sui${args}; ` +
    `else echo 'SUI is not installed on this SSH host' >&2; exit 127; fi`;
}

function remoteSetupCommand() {
  return `if test -x "$HOME/.sui/bin/sui"; then exec "$HOME/.sui/bin/sui" desktop-setup; ` +
    `elif command -v sui >/dev/null 2>&1; then exec sui desktop-setup; ` +
    `else echo 'SUI is not installed on this SSH host' >&2; exit 127; fi`;
}

function localBinary(app) {
  if (process.env.SUI_DESKTOP_CLI) return process.env.SUI_DESKTOP_CLI;
  return app.isPackaged ? path.join(process.resourcesPath, 'sui') : path.resolve(__dirname, '../target/debug/sui');
}

function bridgeSpec(app, target, resume) {
  const workspace = String(target.workspace || '').trim();
  const profile = String(target.profile || '').trim();
  if (!workspace || !profile) throw new Error('Workspace and model profile are required.');
  if (target.kind === 'ssh') {
    const host = validateHost(target.host);
    return { command: 'ssh', args: ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12', '--', host, remoteCommand(workspace, profile, resume)] };
  }
  if (target.kind !== 'local') throw new Error('Choose Local or SSH.');
  return { command: localBinary(app), args: ['desktop-bridge', '--workspace', workspace, '--profile', profile, ...(resume ? ['--resume', resume] : [])] };
}

function spawnBridge(app, target, resume) {
  const spec = bridgeSpec(app, target, resume);
  return spawn(spec.command, spec.args, { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
}

function spawnSetup(app, target) {
  if (target.kind === 'ssh') return spawn('ssh', ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12', '--', validateHost(target.host), remoteSetupCommand()], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
  if (target.kind === 'local') return spawn(localBinary(app), ['desktop-setup'], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
  throw new Error('Choose Local or SSH.');
}

module.exports = { validateHost, shellQuote, remoteCommand, bridgeSpec, spawnBridge, spawnSetup };
