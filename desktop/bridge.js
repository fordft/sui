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

function sshArgs(host, command) {
  return ['-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=12', '--', validateHost(host), command];
}

function remoteCommand(workspace, profile, resume, effort, model, continuation) {
  const args = `desktop-bridge --workspace ${shellQuote(workspace)} --profile ${shellQuote(profile)}` +
    (resume ? ` --resume ${shellQuote(resume)}` : continuation ? ` --continue-from ${shellQuote(continuation)}` : '');
  return guardedRemote(args + reasoningArg(resume ? null : effort, true) + modelArg(resume ? null : model, true));
}

function modelArg(model, remote = false) {
  if (model == null) return remote ? '' : [];
  if (typeof model !== 'string' || !model.length || Buffer.byteLength(model) > 200 || model.includes('\0')) throw new Error('Invalid model ID.');
  return remote ? ` --model ${shellQuote(model)}` : ['--model', model];
}

function reasoningArg(effort, remote = false) {
  if (effort == null) return remote ? '' : [];
  if (!['default', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'].includes(effort)) throw new Error('Invalid reasoning level.');
  return remote ? ` --reasoning-effort ${shellQuote(effort)}` : ['--reasoning-effort', effort];
}

function guardedRemote(args) {
  return `if test -x "$HOME/.sui/bin/sui"; then sui_bin="$HOME/.sui/bin/sui"; ` +
    `elif command -v sui >/dev/null 2>&1; then sui_bin=$(command -v sui); ` +
    `else echo 'SUI is not installed on this SSH host' >&2; exit 127; fi; ` +
    `if ! "$sui_bin" --help 2>/dev/null | grep -q -- '--desktop-protocol'; then ` +
    `echo 'This SSH host has an older SUI CLI. Choose Install or update SUI on SSH host, then retry.' >&2; exit 78; fi; ` +
    `exec "$sui_bin" --desktop-protocol 5 ${args}`;
}

function remoteSetupCommand(subcommand = 'desktop-setup') {
  if (!['desktop-setup', 'desktop-provider'].includes(subcommand)) throw new Error('Invalid setup command');
  return guardedRemote(subcommand);
}

function localBinary(app) {
  if (process.env.SUI_DESKTOP_CLI) return process.env.SUI_DESKTOP_CLI;
  return app.isPackaged ? path.join(process.resourcesPath, 'sui') : path.resolve(__dirname, '../target/debug/sui');
}

function bridgeSpec(app, target, resume, continuation) {
  if (resume && continuation) throw new Error('Choose resume or continuation, not both.');
  const workspace = String(target.workspace || '');
  const profile = String(target.profile || '').trim();
  if (!workspace.trim() || !profile) throw new Error('Workspace and model profile are required.');
  if (target.kind === 'ssh') {
    const host = validateHost(target.host);
    return { command: 'ssh', args: sshArgs(host, remoteCommand(workspace, profile, resume, target.reasoning_effort, target.model, continuation)) };
  }
  if (target.kind !== 'local') throw new Error('Choose Local or SSH.');
  return { command: localBinary(app), args: ['--desktop-protocol', '5', 'desktop-bridge', '--workspace', workspace, '--profile', profile, ...(resume ? ['--resume', resume] : [...(continuation ? ['--continue-from', continuation] : []), ...reasoningArg(target.reasoning_effort), ...modelArg(target.model)])] };
}

function spawnBridge(app, target, resume, continuation) {
  const spec = bridgeSpec(app, target, resume, continuation);
  return spawn(spec.command, spec.args, { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
}

function spawnSetup(app, target, subcommand = 'desktop-setup') {
  if (target.kind === 'ssh') return spawn('ssh', sshArgs(target.host, remoteSetupCommand(subcommand)), { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
  if (target.kind === 'local') return spawn(localBinary(app), ['--desktop-protocol', '5', subcommand], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
  throw new Error('Choose Local or SSH.');
}

module.exports = { validateHost, shellQuote, sshArgs, remoteCommand, remoteSetupCommand, bridgeSpec, spawnBridge, spawnSetup };
