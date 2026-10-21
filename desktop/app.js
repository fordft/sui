const $ = id => document.getElementById(id);
const state = {
  target: null, hello: null, activeRun: null, nextRun: 1, assistant: null, annotation: null,
  browserOpen: false, browserShown: false, permissionId: null, permissionSending: false, log: null, messages: 0, projects: [], profiles: null, settingsPending: false, images: [], imagesLoading: 0, pendingSubmission: null,
  conversationMode: 'solo', lastMissionTask: '', missionInfo: null, missionInfoPending: false, activeMission: false, missionCard: null, missionStreams: new Map(), permissionQueue: []
};
const SUGGESTIONS = [
  { title: 'Explore code', detail: 'Find your way around a codebase', icon: 'folder', prompt: 'Explain how this codebase is organized' },
  { title: 'Fix a bug', detail: 'Track down a failing test', icon: 'terminal', prompt: 'Find and fix a failing test' },
  { title: 'Review changes', detail: 'Get a second look before you ship', icon: 'chat', prompt: 'Review my uncommitted changes' },
  { title: 'Add tests', detail: 'Cover the code that matters most', icon: 'shield', prompt: 'Add tests for the riskiest code' }
];
const pet = () => window.SuiUI.pet;

function h(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('class', 'i');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', `#i-${name}`);
  svg.append(use);
  return svg;
}

// Screen readers get state changes here instead of every streamed token.
function announce(text) {
  const region = $('sr-status');
  region.textContent = '';
  setTimeout(() => { region.textContent = text; }, 40);
}

const projectName = item => item.workspace.split(/[\\/]/).filter(Boolean).at(-1) || item.workspace;
const hostLabel = item => (item.kind === 'ssh' ? item.host : window.SuiUI.localLabel());
const sameTarget = (a, b) => a && b && a.kind === b.kind && (a.host || '') === (b.host || '') && a.workspace === b.workspace;

function setStatus(label, connected, busy = false) {
  const pill = $('connection-pill');
  pill.textContent = label;
  pill.classList.toggle('connected', connected);
  pill.classList.toggle('busy', busy);
}

function updateSend() {
  const draft = $('message').value.trim() || state.annotation || state.images.length;
  $('run-mission').disabled = !state.hello || state.missionInfoPending || state.missionInfo?.ready !== true || state.activeRun !== null || state.settingsPending || state.imagesLoading > 0 || state.images.length > 0 || !(String($('message').value).trim() || state.lastMissionTask);
  $('send').disabled = !state.hello || state.activeRun !== null || state.settingsPending || state.imagesLoading > 0 || (state.images.length > 0 && !state.hello?.image_input) || !draft;
}

function setBusy(busy) {
  const ready = !!state.hello;
  $('refresh-mission').disabled = !ready || busy || state.missionInfoPending;
  $('composer-execution').disabled = !ready || busy || state.settingsPending;
  for (const id of ['mission-worker', 'mission-auditor', 'mission-workers']) $(id).disabled = !ready || busy;
  $('choose-model').disabled = !ready || busy || state.settingsPending;
  $('composer-reasoning').disabled = !ready || busy || state.settingsPending || (state.hello?.reasoning_choices || []).length < 2;
  $('composer-mode').disabled = !ready || (busy && state.permissionId === null) || state.settingsPending || state.permissionSending;
  document.querySelectorAll('[data-choice]').forEach(button => { button.disabled = state.settingsPending || state.permissionSending; });
  $('attach-image').disabled = !ready || busy || state.imagesLoading > 0 || !state.hello?.image_input;
  if (busy) $('model-dialog').close();
  $('message').disabled = !ready;
  $('composer-context').classList.toggle('hidden', !ready);
  $('stop').classList.toggle('hidden', !busy);
  $('composer-hint').textContent = busy ? 'SUI is working…' : ready ? state.conversationMode === 'mission' ? 'Enter to chat · Run mission starts workers' : 'Enter to send · Shift+Enter for a new line' : 'Connect a project to start';
  $('connection-pill').classList.toggle('busy', busy && ready);
  updateSend();
  pet().setBusy(busy);
  updateProviderForm();
}

// ── conversation ─────────────────────────────────────────────────────────

const transcript = () => $('transcript');
const pinned = () => { const t = transcript(); return t.scrollHeight - t.scrollTop - t.clientHeight < 90; };
function scrollToEnd(force = true) { if (force) transcript().scrollTop = transcript().scrollHeight; }
function updateScrollButton() {
  $('jump-to-latest').classList.toggle('hidden', state.messages === 0 || pinned());
}

function setStage() {
  $('stage').dataset.stage = state.messages === 0 ? 'hero' : 'chat';
  updateScrollButton();
  pet().refresh();
}

function appendMessage(role, content) {
  const stick = pinned();
  const row = h('div', `msg ${role === 'user' ? 'user' : 'assistant'}`);
  const body = h('div', 'message-body');
  if (role === 'user') body.textContent = content;
  else window.SuiMarkdown.render(body, content);
  row.append(body);
  transcript().append(row);
  state.messages++;
  setStage();
  scrollToEnd(stick || role === 'user');
  return { body, raw: content, queued: false };
}

function appendNotice(text, error = false) {
  const stick = pinned();
  transcript().append(h('div', `notice${error ? ' err' : ''}`, text));
  scrollToEnd(stick);
}

function streamDelta(text, agent) {
  const stick = pinned();
  let message;
  if (state.activeMission) {
    const key = String(agent || 'mission').slice(0, 120);
    message = state.missionStreams.get(key);
    if (!message) { message = appendMessage('assistant', ''); message.body.before(h('div', 'mission-agent', key)); state.missionStreams.set(key, message); }
  } else {
    if (!state.assistant) { closeLog(true); state.assistant = appendMessage('assistant', ''); }
    message = state.assistant;
  }
  message.raw += text;
  if (!message.queued) {
    message.queued = true;
    requestAnimationFrame(() => {
      message.queued = false;
      window.SuiMarkdown.render(message.body, message.raw);
      scrollToEnd(stick);
    });
  }
}

// One collapsible work log per burst of tool activity, so a long run reads as
// "N steps" instead of a wall of status lines.
function openLog() {
  if (state.log) return state.log;
  const details = h('details', 'worklog');
  details.open = true;
  const summary = h('summary');
  const chevron = icon('chevron');
  chevron.classList.add('chev');
  const stateBox = h('span', 'wl-state');
  const done = icon('check');
  done.classList.add('wl-done');
  stateBox.append(h('span', 'wl-spin'), done);
  const label = h('span', 'wl-label', 'Working…');
  const count = h('span', 'wl-count');
  summary.append(chevron, stateBox, label, count);
  const body = h('div', 'wl-body');
  details.append(summary, body);
  const stick = pinned();
  transcript().append(details);
  scrollToEnd(stick);
  state.log = { details, body, label, count, steps: 0 };
  return state.log;
}

function logLine(text, kind = '') {
  const log = openLog();
  const stick = log.body.scrollHeight - log.body.scrollTop - log.body.clientHeight < 40;
  log.body.append(h('div', `wl-line${kind ? ' ' + kind : ''}`, text));
  log.steps++;
  log.count.textContent = `${log.steps} step${log.steps === 1 ? '' : 's'}`;
  log.label.textContent = text.length > 90 ? text.slice(0, 89) + '…' : text;
  if (stick) log.body.scrollTop = log.body.scrollHeight;
  if (kind === 'fail') log.failed = true;
}

function closeLog(collapse, failed = false) {
  const log = state.log;
  if (!log) return;
  const failedSteps = failed || log.failed;
  const steps = `${log.steps} step${log.steps === 1 ? '' : 's'}`;
  log.details.classList.add('done');
  if (failedSteps) log.details.classList.add('failed');
  log.label.textContent = failedSteps ? `Stopped after ${steps}` : `Worked through ${steps}`;
  log.count.textContent = '';
  if (collapse && !failedSteps) log.details.open = false;
  state.log = null;
}

// ── projects and sessions ────────────────────────────────────────────────

function displayProject(item, className = 'project-row') {
  const row = h('button', className);
  row.type = 'button';
  const active = sameTarget(state.target, item);
  row.classList.toggle('active', active);
  if (active) row.setAttribute('aria-current', 'true');
  const ico = h('span', 'project-ico');
  ico.append(icon(item.kind === 'ssh' ? 'server' : 'folder'));
  const text = h('span', 'project-text');
  text.append(h('strong', '', projectName(item)), h('small', '', hostLabel(item)));
  row.append(ico, text);
  row.title = `${hostLabel(item)} · ${item.workspace}`;
  row.addEventListener('click', () => connect(item, 'latest'));
  return row;
}

function renderProjects() {
  const query = $('project-filter').value.trim().toLocaleLowerCase();
  const projects = state.projects.filter(item => `${projectName(item)} ${hostLabel(item)} ${item.workspace}`.toLocaleLowerCase().includes(query));
  $('project-list').replaceChildren(...projects.map(item => displayProject(item)));
  $('project-count').textContent = String(state.projects.length);
  $('clear-project-filter').classList.toggle('hidden', !query);
  if (!projects.length) $('project-list').append(h('div', 'sidebar-empty', query ? 'No matching projects' : 'No projects yet'));
  renderWelcome();
}

async function refreshProjects() {
  state.projects = await window.sui.projects();
  renderProjects();
}

function renderWelcome() {
  const connected = !!state.hello;
  $('welcome-title').textContent = connected ? 'What are we building?' : 'Build from anywhere.';
  $('welcome-text').textContent = connected
    ? `${projectName(state.target)} · ${state.hello.model}`
    : 'Connect SUI to a workspace, talk to your agent, and review your app in the preview beside the chat.';
  $('welcome-actions').classList.toggle('hidden', connected);
  const suggestions = $('suggestions');
  suggestions.classList.toggle('hidden', !connected);
  if (connected && !suggestions.childElementCount) {
    for (const suggestion of SUGGESTIONS) {
      const chip = h('button', 'suggestion');
      chip.type = 'button';
      const mark = h('span', 'suggestion-icon');
      mark.append(icon(suggestion.icon));
      const text = h('span', 'suggestion-text');
      text.append(h('strong', '', suggestion.title), h('small', '', suggestion.detail));
      chip.append(mark, text, icon('arrow-right'));
      chip.addEventListener('click', () => { $('message').value = suggestion.prompt; window.SuiUI.autosize($('message')); updateSend(); $('message').focus(); });
      suggestions.append(chip);
    }
  }
  const recents = $('welcome-recents');
  const list = connected ? [] : state.projects.slice(0, 3);
  recents.classList.toggle('hidden', !list.length);
  recents.replaceChildren(...(list.length ? [h('div', 'recents-title', 'Recent projects'), ...list.map(item => displayProject(item, 'recent-card'))] : []));
}

function renderSessions(items) {
  $('session-list').replaceChildren();
  for (const item of items) {
    const row = h('button', 'session-row', item.label);
    row.type = 'button';
    row.title = item.label;
    row.addEventListener('click', () => { if (state.target) connect({ ...state.target, profile: item.profile || state.target.profile }, item.id); });
    $('session-list').append(row);
  }
  if (!items.length) $('session-list').append(h('div', 'sidebar-empty', 'No earlier chats here'));
}

// ── connection dialog ────────────────────────────────────────────────────

let folderListing = null, folderRequest = 0, hostRequest = 0, connectionBusy = false, selectedWorkspace = '';
let dialogLocation = '';

function closeFolders() {
  folderRequest++;
  folderListing = null;
  $('folder-browser').classList.add('hidden');
  $('browse-workspace').disabled = false;
  window.sui.cancelFolderBrowse().catch(() => {});
}

function setWorkspace(directory) {
  selectedWorkspace = directory;
  $('workspace').value = directory;
  $('workspace-selected').textContent = directory;
  $('workspace-selected').classList.toggle('hidden', !directory);
  updateConnectForm();
}

function updateConnectForm() {
  const item = formTarget();
  const browsing = !$('folder-browser').classList.contains('hidden');
  $('connect-button').disabled = connectionBusy || providerBusy || savedProfiles.some(p => p.name === item.profile && p.kind === 'chatgpt' && !p.account?.sharing) || browsing || !item.workspace.trim() || !item.profile || (item.kind === 'ssh' && !item.host);
  $('browse-workspace').disabled = connectionBusy || browsing || (item.kind === 'ssh' && !item.host);
}

function renderRecentWorkspaces() {
  const item = formTarget();
  const recents = state.projects.filter(p => p.kind === item.kind && (p.host || '') === item.host);
  $('recent-workspaces').replaceChildren(h('option', '', 'Choose a recent folder…'));
  $('recent-workspaces').firstChild.value = '';
  for (const project of recents) {
    const option = h('option', '', project.workspace);
    option.value = project.workspace;
    $('recent-workspaces').append(option);
  }
  $('recent-workspaces-label').classList.toggle('hidden', !recents.length);
}

function hostChanged() {
  const item = formTarget();
  const location = `${item.kind}:${item.host}`;
  if (location !== dialogLocation) {
    dialogLocation = location;
    resetProvider();
    savedProfiles = [];
    $('saved-profile').replaceChildren(new Option('Loading profiles…', ''));
    clearTimeout(profileTimer);
    profileTimer = setTimeout(refreshProfiles, 300);
    closeFolders();
    setWorkspace('');
    $('connect-error').classList.add('hidden');
  }
  renderRecentWorkspaces();
  updateConnectForm();
}

function selectHost() {
  const choice = $('host-choice').value;
  $('host-label').classList.toggle('hidden', choice !== ':manual');
  $('host').value = choice === ':manual' ? '' : choice;
  hostChanged();
  if (choice === ':manual') $('host').focus();
}

async function refreshHosts() {
  const request = ++hostRequest;
  const original = $('host').value;
  $('refresh-hosts').disabled = true;
  try {
    const result = await window.sui.sshHosts();
    if (request !== hostRequest) return;
    const select = $('host-choice');
    const manual = select.value === ':manual';
    const option = (value, label) => { const node = h('option', '', label); node.value = value; return node; };
    const recent = [...new Set(state.projects.filter(p => p.kind === 'ssh').map(p => p.host))].filter(host => !result.hosts.includes(host));
    select.replaceChildren(option('', 'Choose an SSH host…'));
    for (const [label, hosts] of [['From ~/.ssh/config', result.hosts], ['Recent hosts', recent]]) {
      if (!hosts.length) continue;
      const group = h('optgroup'); group.label = label;
      group.append(...hosts.map(host => option(host, host)));
      select.append(group);
    }
    select.append(option(':manual', 'Enter another host…'));
    // Do not overwrite a host typed or selected while discovery was pending.
    const current = $('host').value;
    const known = [...result.hosts, ...recent].includes(current);
    select.value = known ? current : (current || manual || (!result.hosts.length && !recent.length)) ? ':manual' : '';
    const remote = formTarget().kind === 'ssh';
    $('host-label').classList.toggle('hidden', !remote || select.value !== ':manual');
    $('host-help').textContent = result.warnings.length ? result.warnings.join(' ') : result.hosts.length
      ? `${result.hosts.length} hosts from ~/.ssh/config and its Include files. Your SSH settings are used automatically.`
      : 'No saved SSH aliases found in ~/.ssh/config. Enter a host below, or reload after adding one.';
    if (current === original) updateConnectForm();
  } catch (error) {
    if (request !== hostRequest) return;
    $('host-choice').replaceChildren(h('option', '', 'Enter another host…'));
    $('host-choice').firstChild.value = ':manual';
    $('host-label').classList.toggle('hidden', formTarget().kind !== 'ssh');
    $('host-help').textContent = String(error.message || error);
  } finally { if (request === hostRequest) $('refresh-hosts').disabled = false; }
}

function renderFolders() {
  const query = $('folder-filter').value.toLocaleLowerCase();
  const list = $('folder-list');
  list.replaceChildren();
  if (!folderListing) return;
  for (const name of folderListing.directories.filter(name => name.toLocaleLowerCase().includes(query))) {
    const row = h('button', 'folder-row'); row.type = 'button';
    row.append(icon('folder'), h('span', '', name)); row.title = name;
    row.addEventListener('click', () => browseFolders(`${folderListing.path.replace(/\/$/, '')}/${name}`));
    list.append(row);
  }
  if (!list.childElementCount) list.append(h('p', 'field-help', query ? 'No matching folders' : 'No subfolders. You can use this folder.'));
}

async function browseFolders(directory = '') {
  const item = formTarget();
  const request = ++folderRequest;
  $('connect-error').classList.add('hidden');
  if (item.kind === 'local') {
    $('browse-workspace').disabled = true;
    try {
      const chosen = await window.sui.pickWorkspace();
      if (request === folderRequest && chosen) setWorkspace(chosen);
    } catch (error) {
      if (request === folderRequest) showConnectError(error);
    } finally { if (request === folderRequest) updateConnectForm(); }
    return;
  }
  if (!item.host) return;
  folderListing = null;
  $('folder-browser').classList.remove('hidden');
  $('folder-browser').setAttribute('aria-busy', 'true');
  $('folder-list').replaceChildren();
  $('folder-status').textContent = `Connecting to ${item.host}…`;
  $('folder-path').textContent = directory || 'Home folder';
  for (const id of ['choose-folder', 'folder-up', 'folder-home', 'folder-show-hidden']) $(id).disabled = true;
  updateConnectForm();
  try {
    const listing = await window.sui.remoteFolders(item.host, directory, $('folder-show-hidden').checked);
    if (request !== folderRequest) return;
    folderListing = listing;
    $('folder-path').textContent = listing.path;
    $('folder-filter').value = '';
    renderFolders();
    $('folder-status').textContent = `Connected to ${item.host}. ${listing.truncated ? 'Showing the first 500 folders; enter a path for folders outside this list.' : 'Open a subfolder or use the current folder.'}`;
    $('choose-folder').disabled = false;
    $('folder-up').disabled = listing.parent === listing.path;
    $('folder-home').disabled = false;
    $('folder-show-hidden').disabled = false;
    $('folder-filter').focus();
  } catch (error) {
    if (request !== folderRequest) return;
    folderListing = null;
    $('folder-status').textContent = 'Could not list this folder. Retry Home or enter a path manually.';
    $('folder-home').disabled = false;
    showConnectError(error);
  } finally {
    if (request === folderRequest) $('folder-browser').setAttribute('aria-busy', 'false');
  }
}

function showConnectError(error) {
  $('connect-error').textContent = String(error.message || error);
  $('connect-error').classList.remove('hidden', 'success');
}

async function showDialog(item) {
  if (item && providerLocation(item) !== dialogLocation) {
    resetProvider();
    savedProfiles = [];
  }
  closeFolders();
  $('connect-error').classList.add('hidden');
  if (item) {
    document.querySelector(`input[name="kind"][value="${item.kind}"]`).checked = true;
    $('host').value = item.host || '';
    selectedWorkspace = item.workspace;
    $('profile').value = item.profile || 'codex';
  }
  const current = formTarget();
  dialogLocation = `${current.kind}:${current.host}`;
  updateMode();
  setWorkspace(selectedWorkspace);
  window.SuiUI.openDialog($('connect-dialog'));
  refreshHosts();
  await refreshProfiles();
  updateProviderForm();
}

function updateMode() {
  const remote = document.querySelector('input[name="kind"]:checked').value === 'ssh';
  $('ssh-hosts').classList.toggle('hidden', !remote);
  $('host-label').classList.toggle('hidden', !remote || $('host-choice').value !== ':manual');
  $('install-remote').classList.toggle('hidden', !remote);
  $('browse-workspace-label').textContent = remote ? 'Connect & choose folder' : 'Choose a folder…';
  hostChanged();
}

function formTarget() {
  const kind = document.querySelector('input[name="kind"]:checked').value;
  return { kind, host: kind === 'ssh' ? $('host').value.trim() : '', workspace: selectedWorkspace, profile: $('profile').value.trim() };
}

async function installRemote() {
  const item = formTarget();
  $('install-remote').disabled = true;
  $('connect-error').classList.remove('hidden', 'success');
  $('connect-error').textContent = 'Installing SUI on the SSH host…';
  try {
    const output = await window.sui.installRemote(item);
    $('connect-error').classList.add('success');
    $('connect-error').textContent = `SUI installed on ${item.host}. ${output.slice(-350)}`;
  } catch (error) { $('connect-error').textContent = String(error.message || error); }
  finally { $('install-remote').disabled = false; }
}

let providerBusy = false;
let profileRequest = 0;
let providerGeneration = 0;
let profileTimer;
let savedProfiles = [];
let setupAccount = null;

function providerLocation(item = formTarget()) { return `${item.kind}:${item.host || ''}`; }
function providerTarget() { return { ...formTarget(), profile: $('setup-profile').value.trim() }; }
function updateProviderForm() {
  const chatgpt = $('model-kind').value === 'chatgpt';
  $('chatgpt-setup').classList.toggle('hidden', !chatgpt);
  $('api-setup').classList.toggle('hidden', chatgpt);
  $('model-url-field').classList.toggle('hidden', $('model-kind').value === 'openai-responses');
  $('model-key-field').classList.toggle('hidden', $('model-kind').value === 'ollama');
  const name = $('setup-profile').value.trim();
  const hostReady = formTarget().kind === 'local' || !!formTarget().host;
  const available = hostReady && !providerBusy && state.activeRun === null;
  const validName = /^[A-Za-z0-9_-]{1,64}$/.test(name);
  $('chatgpt-login').disabled = !available || !validName;
  $('reload-chatgpt-models').disabled = !available || !validName;
  $('chatgpt-logout').classList.toggle('hidden', !setupAccount?.connected);
  $('chatgpt-logout').disabled = !available;
  $('cancel-sign-in').classList.toggle('hidden', !providerBusy);
  $('model-kind').disabled = providerBusy;
  $('setup-profile').disabled = providerBusy;
  $('save-model').textContent = chatgpt ? 'Use this account' : 'Use this model';
  $('save-model').disabled = !available || !validName || (chatgpt
    ? !setupAccount?.sharing
    : !$('model-id').value.trim() || !$('model-url').value.trim() || ($('model-kind').value === 'openai-responses' && !$('model-key').value.trim()));
  updateConnectForm();
}

function resetProvider() {
  providerGeneration++;
  window.sui.cancelProvider().catch(() => {});
  providerBusy = false;
  setupAccount = null;
  $('model-key').value = '';
  $('chatgpt-status').textContent = '';
  updateProviderForm();
}

async function refreshProfiles() {
  const item = formTarget();
  const request = ++profileRequest;
  if (item.kind === 'ssh' && !item.host) {
    savedProfiles = [];
    $('saved-profile').replaceChildren(new Option('Choose a host first…', ''));
    $('profiles-status').textContent = '';
    return;
  }
  $('profiles-status').textContent = 'Loading profiles from this host…';
  $('refresh-profiles').disabled = true;
  try {
    const result = await window.sui.provider(item, { action: 'profiles' });
    if (request !== profileRequest || providerLocation(item) !== providerLocation()) return;
    savedProfiles = result.items || [];
    const select = $('saved-profile');
    select.replaceChildren(new Option('Choose a saved profile…', ''));
    for (const p of savedProfiles) select.append(new Option(`${p.kind === 'chatgpt' ? 'ChatGPT account' : p.model || 'Model profile'} · ${p.name}${p.account?.email ? ' · ' + p.account.email : ''}`, p.name));
    const current = savedProfiles.find(p => p.name === $('profile').value);
    if (current) select.value = current.name;
    else if (savedProfiles.length) { select.value = savedProfiles[0].name; $('profile').value = select.value; }
    $('profiles-status').textContent = result.truncated ? 'Showing the first 200 profiles.' : savedProfiles.length ? '' : 'Add a provider below to get started.';
    if (!savedProfiles.length) $('profiles-status').textContent = 'Choose Add a provider to get started.';
    const chosen = savedProfiles.find(p => p.name === $('profile').value);
    if (chosen?.kind === 'chatgpt' && !chosen.account?.sharing) $('profiles-status').textContent = 'Select this profile to reconnect and enable ChatGPT plan usage.';
    updateConnectForm();
  } catch (error) {
    if (request === profileRequest) $('profiles-status').textContent = String(error.message || error);
  } finally { if (request === profileRequest) $('refresh-profiles').disabled = false; }
}

function showProviderSetup() {
  if (state.activeRun !== null) return;
  $('model-setup').classList.remove('hidden');
  $('provider-error').classList.add('hidden');
  window.SuiUI.openDialog($('provider-dialog'));
  updateProviderForm();
}

function closeProviderSetup() {
  resetProvider();
  $('provider-dialog').close();
}

function changeProvider() {
  resetProvider();
  const kind = $('model-kind').value;
  $('setup-profile').value = { chatgpt: 'chatgpt', 'openai-responses': 'openai', ollama: 'ollama', 'chat-completions': 'custom', 'responses-compatible': 'responses' }[kind];
  $('model-url').value = kind === 'ollama' ? 'http://127.0.0.1:11434/v1' : ['chat-completions', 'responses-compatible'].includes(kind) ? '' : 'https://api.openai.com/v1';
  $('model-id').value = '';
  $('model-id').placeholder = kind === 'ollama' ? 'Installed Ollama model name' : 'Exact model ID from your provider';
  updateProviderForm();
}

async function loadChatgptModels(item = providerTarget(), generation = providerGeneration, refresh = false) {
  $('chatgpt-status').textContent = 'Loading account models…';
  const result = await window.sui.provider(item, { action: 'models', name: item.profile, refresh });
  if (generation !== providerGeneration || providerLocation(item) !== providerLocation() || item.profile !== $('setup-profile').value.trim()) return;
  $('chatgpt-status').textContent = result.warning || (!result.items?.length ? 'No models available for this account. Check ChatGPT plan usage.' : `${setupAccount?.email || 'ChatGPT account'} · ${result.items.length} models ready. Choose a model beside the message box.${result.truncated ? ' Showing the first 200 models.' : ''}`);
  updateProviderForm();
}

async function signInChatgpt() {
  if (providerBusy || state.activeRun !== null) return;
  const item = providerTarget();
  const generation = providerGeneration;
  providerBusy = true;
  $('chatgpt-status').textContent = 'Finish signing in and allow ChatGPT plan usage in your browser…';
  updateProviderForm();
  try {
    const result = await window.sui.provider(item, { action: 'login', name: item.profile });
    if (generation !== providerGeneration) return;
    setupAccount = result.account;
    if (setupAccount.sharing) {
      await loadChatgptModels(item, generation).catch(error => { $('chatgpt-status').textContent = String(error.message || error); });
      if (generation !== providerGeneration) return;
      $('profile').value = item.profile;
      await refreshProfiles();
      closeProviderSetup();
      $('connect-button').focus();
    } else $('chatgpt-status').textContent = 'Signed in. Reconnect and allow ChatGPT plan usage to load models.';
  } catch (error) { if (generation === providerGeneration) $('chatgpt-status').textContent = String(error.message || error); }
  finally { if (generation === providerGeneration) { providerBusy = false; updateProviderForm(); } }
}

async function saveModel() {
  if ($('save-model').disabled || state.activeRun !== null) return;
  const item = providerTarget();
  const generation = providerGeneration;
  providerBusy = true;
  $('connect-error').classList.add('hidden');
  $('provider-error').classList.add('hidden');
  updateProviderForm();
  try {
    const kind = $('model-kind').value;
    if (kind !== 'chatgpt') await window.sui.configure(item, { name: item.profile, kind: kind === 'responses-compatible' ? 'openai-responses' : kind, base_url: $('model-url').value.trim(), model: $('model-id').value.trim(), api_key: $('model-key').value });
    if (generation !== providerGeneration) return;
    $('profile').value = item.profile;
    $('model-key').value = '';
    $('show-model-setup').textContent = '+ Add a provider';
    await refreshProfiles();
    closeProviderSetup();
    $('connect-button').focus();
  } catch (error) {
    if (generation === providerGeneration) {
      $('provider-error').textContent = String(error.message || error);
      $('provider-error').classList.remove('hidden');
    }
  }
  finally { if (generation === providerGeneration) { providerBusy = false; updateProviderForm(); } }
}

let modelPickerRequest = 0;
function renderModelOptions() {
  const list = $('model-options');
  list.replaceChildren();
  if (!state.profiles) return;
  const query = $('model-filter').value.trim().toLowerCase();
  const matches = state.profiles.filter(item => `${item.profile} ${item.model || ''} ${item.name || ''}`.toLowerCase().includes(query));
  for (const item of matches) {
    const current = item.profile === state.target?.profile && item.model === state.hello?.model;
    const button = h('button', 'model-option');
    button.type = 'button';
    button.setAttribute('aria-current', String(current));
    button.append(h('span', '', item.name || item.model), h('small', '', `${item.profile}${item.email ? ' · ' + item.email : ''}${current ? ' · Current' : ''}`));
    if (item.reasoning_levels?.length) button.title = 'Reasoning: ' + item.reasoning_levels.map(l => l.effort).join(', ');
    button.addEventListener('click', () => {
      if (!state.hello || connectionBusy || state.activeRun !== null || state.settingsPending) return;
      $('model-dialog').close();
      if (current) { $('message').focus(); return; }
      const target = { ...state.target, profile: item.profile };
      if (item.kind === 'chatgpt') target.model = item.model;
      else delete target.model;
      delete target.reasoning_effort;
      connect(target);
    });
    list.append(button);
  }
  $('model-picker-status').textContent = state.modelCatalogNote || (!state.profiles.length ? 'No models available on this host.' : !matches.length ? 'No matching models.' : state.profilesTruncated ? 'Showing the first 200 models per account.' : '');
}

async function loadModelPicker(refresh = false) {
  const request = ++modelPickerRequest;
  const target = state.target;
  const current = () => request === modelPickerRequest && $('model-dialog').open && state.hello && state.target === target;
  $('refresh-model-catalog').disabled = true;
  $('model-picker-status').textContent = 'Loading models…';
  try {
    const result = await window.sui.provider(target, { action: 'profiles' });
    if (!current()) return;
    const warnings = [];
    let truncated = !!result.truncated;
    const catalogs = await Promise.all((result.items || []).map(async profile => {
      if (profile.kind !== 'chatgpt') return [{ profile: profile.name, model: profile.model, name: profile.model || profile.name, kind: profile.kind }];
      if (!profile.account?.sharing) return [];
      try {
        const models = await window.sui.provider(target, { action: 'models', name: profile.name, refresh });
        if (models.warning) warnings.push(`${profile.name}: ${models.warning}`);
        truncated ||= !!models.truncated;
        return (models.items || []).map(m => ({ ...m, profile: profile.name, model: m.id, kind: 'chatgpt', email: profile.account.email }));
      } catch (error) {
        warnings.push(`${profile.name}: ${String(error.message || error)}`);
        // Keep the active model visible while refresh is unavailable.
        return profile.name === target.profile ? [{ profile: profile.name, model: state.hello?.model, name: state.hello?.model_name || state.hello?.model, kind: 'chatgpt' }] : [];
      }
    }));
    if (!current()) return;
    state.profiles = catalogs.flat();
    state.profilesTruncated = truncated;
    state.modelCatalogNote = warnings.join(' ').slice(0, 4000);
    renderModelOptions();
  } catch (error) {
    if (current()) $('model-picker-status').textContent = String(error.message || error);
  } finally { if (current()) $('refresh-model-catalog').disabled = false; }
}

async function showModelPicker() {
  if (!state.hello || connectionBusy || state.activeRun !== null || state.settingsPending) return;
  state.profiles = null;
  state.modelCatalogNote = '';
  $('model-filter').value = '';
  $('model-options').replaceChildren();
  window.SuiUI.openDialog($('model-dialog'));
  $('model-filter').focus();
  await loadModelPicker();
}

async function connect(item, resume = null) {
  if (connectionBusy || state.activeRun !== null) return;
  connectionBusy = true;
  $('model-dialog').close();
  closeFolders();
  state.hello = null;
  setBusy(false);
  setStatus('Connecting…', false, true);
  $('connect-button').disabled = true;
  try {
    const hello = await window.sui.connect(item, resume);
    Object.assign(state, { target: item, hello, activeRun: null, nextRun: 1, assistant: null, annotation: null, log: null, messages: 0 });
    $('annotation-card').classList.add('hidden');
    $('permission').classList.add('hidden');
    state.permissionId = null;
    state.permissionSending = false;
    state.permissionQueue = []; state.activeMission = false; state.missionCard = null; state.missionStreams = new Map();
    state.missionInfoPending = false;
    state.conversationMode = hello.conversation_mode || 'solo'; state.lastMissionTask = hello.last_mission_task || '';
    setMissionInfo(hello.mission);
    pet().setAsking(false);
    $('workspace-title').textContent = projectName(item);
    $('workspace-subtitle').textContent = `${item.kind === 'ssh' ? item.host + '  /  ' : ''}${item.workspace}  ·  ${hello.model}`;
    $('composer-model').textContent = hello.model_name || hello.model;
    $('composer-model').title = hello.model;
    state.settingsPending = false;
    state.target.reasoning_effort = hello.reasoning_effort || 'default';
    if (hello.kind === 'chatgpt') state.target.model = hello.model;
    const effortLabels = { default: 'Provider default', none: 'None', minimal: 'Minimal', low: 'Low', medium: 'Medium', high: 'High', xhigh: 'Extra high', max: 'Max' };
    $('composer-reasoning').replaceChildren(...(hello.reasoning_choices || ['default']).map(value => new Option(effortLabels[value] || value, value)));
    for (const level of hello.reasoning_levels || []) {
      const existing = [...$('composer-reasoning').options].find(o => o.value === level.effort);
      if (existing) existing.title = level.description || '';
      else {
        const option = new Option(`${level.effort} (unavailable)`, level.effort);
        option.disabled = true;
        option.title = 'This catalog mode is not supported by SUI.';
        $('composer-reasoning').append(option);
      }
    }
    $('composer-reasoning').value = hello.reasoning_effort || 'default';
    $('composer-mode').value = hello.approval || 'ask';
    $('composer-usage').classList.toggle('hidden', hello.kind !== 'chatgpt');
    $('composer-target').textContent = `${projectName(item)} · ${hostLabel(item)}`;
    $('composer-target').title = `${hostLabel(item)} · ${item.workspace}`;
    document.title = `${projectName(item)} · SUI`;
    setStatus(item.kind === 'ssh' ? 'SSH connected' : 'Local', true);
    transcript().replaceChildren();
    for (const message of hello.history || []) appendMessage(message.role, message.content);
    if (hello.recovered) appendNotice('This chat was interrupted. Pending tools were not rerun; inspect existing changes before retrying.', true);
    renderImages();
    if (hello.catalog_warning) appendNotice(hello.catalog_warning, true);
    if (hello.history_truncated) appendNotice('Earlier conversation text is omitted from this preview; SUI restored the full native session.');
    setStage();
    $('remote-preview').classList.toggle('hidden', item.kind !== 'ssh');
    setBusy(false);
    renderWelcome();
    $('connect-dialog').close();
    await refreshProjects();
    await window.sui.command({ type: 'list_sessions' });
    $('message').focus();
    pet().speak('connected!', 2000);
  } catch (error) {
    state.hello = null;
    setBusy(false);
    setStatus('Disconnected', false);
    renderWelcome();
    $('connect-error').textContent = String(error.message || error);
    $('connect-error').classList.remove('hidden');
    if (!$('connect-dialog').open) showDialog(item);
  } finally { connectionBusy = false; updateConnectForm(); }
}

// Images remain draft data until Send, then travel inside the SSH stream.
function imageError(message = '') {
  $('image-error').textContent = message;
  $('image-error').classList.toggle('hidden', !message);
}
function renderImages() {
  const list = $('image-attachments');
  list.replaceChildren();
  for (const [index, image] of state.images.entries()) {
    const item = h('div', 'image-attachment');
    const preview = document.createElement('img');
    preview.src = `data:${image.mime};base64,${image.data}`;
    preview.alt = image.name;
    const remove = h('button', '', '×');
    remove.type = 'button'; remove.setAttribute('aria-label', `Remove ${image.name}`);
    remove.addEventListener('click', () => { state.images.splice(index, 1); imageError(); renderImages(); });
    item.append(preview, h('span', '', image.name), remove); list.append(item);
  }
  list.classList.toggle('hidden', !state.images.length);
  if (state.images.length && state.hello && !state.hello.image_input) imageError('This chat has no image input. Choose an image-capable model or start a new chat.');
  updateSend();
}
async function addImages(files) {
  if (!state.hello || !state.hello.image_input) { imageError('Choose an image-capable model or start a new chat before attaching images.'); return; }
  if (state.activeRun !== null || connectionBusy || state.imagesLoading) { imageError('Wait for the current operation before attaching images.'); return; }
  state.imagesLoading++; setBusy(false); imageError();
  try {
    const added = [];
    let total = state.images.reduce((n, image) => n + image.size, 0);
    if (state.images.length + files.length > 4) throw new Error('Attach at most 4 images.');
    for (const file of files) {
      if (!file.size || file.size > 4 * 1024 * 1024) throw new Error('Each image must be between 1 byte and 4 MiB.');
      if (!file.name || new TextEncoder().encode(file.name).length > 255 || /[\x00-\x1f\x7f]/.test(file.name)) throw new Error('Invalid image filename.');
      total += file.size;
      if (total > 8 * 1024 * 1024) throw new Error('Images must be at most 8 MiB in total.');
      const bytes = new Uint8Array(await file.arrayBuffer());
      const starts = prefix => prefix.every((b, n) => bytes[n] === b);
      const mime = starts([137,80,78,71,13,10,26,10]) ? 'image/png' : starts([255,216,255]) ? 'image/jpeg' : starts([82,73,70,70]) && [87,69,66,80].every((b,n) => bytes[8+n] === b) ? 'image/webp' : null;
      if (!mime) throw new Error('Use PNG, JPEG or WebP images.');
      const url = await new Promise((resolve, reject) => {
        const reader = new FileReader(); reader.onload = () => resolve(reader.result); reader.onerror = () => reject(new Error('Could not read image.')); reader.readAsDataURL(new Blob([bytes], { type: mime }));
      });
      added.push({ name: file.name, mime, data: String(url).split(',')[1], size: file.size });
    }
    state.images.push(...added); renderImages();
  } catch (error) { imageError(String(error.message || error)); }
  finally { state.imagesLoading--; setBusy(state.activeRun !== null); }
}
function clearPermission() {
  state.permissionId = null;
  state.permissionSending = false;
  $('permission').classList.add('hidden');
  pet().setAsking(false);
  setBusy(state.activeRun !== null);
}
function acceptSubmission(run) {
  const submission = state.pendingSubmission;
  if (!submission || submission.run !== run) return;
  if (submission.missionObjective !== null) state.lastMissionTask = submission.missionObjective;
  state.pendingSubmission = null;
}

function restoreSubmission() {
  const submission = state.pendingSubmission;
  if (submission) {
    $('message').value = [submission.draft, $('message').value].filter(Boolean).join('\n');
    state.images = submission.images;
    state.pendingSubmission = null;
    renderImages();
  }
  state.activeRun = null;
  setBusy(false);
}

function renderMissionMode() {
  const enabled = state.conversationMode === 'mission';
  $('composer-execution').value = state.conversationMode;
  $('run-mission').classList.toggle('hidden', !enabled);
  $('mission-options').classList.toggle('hidden', !enabled);
  $('mission-help').classList.toggle('hidden', !enabled);
  $('refresh-mission').classList.toggle('hidden', !enabled);
  $('mission-help').textContent = state.missionInfo?.issue || 'Discuss the objective, then Run mission. Workers run from committed HEAD in a separate branch; uncommitted edits are excluded.';
  setBusy(state.activeRun !== null);
}

function refreshMissionInfo() {
  if (!state.hello || state.conversationMode !== 'mission' || state.activeRun !== null || state.missionInfoPending) return;
  const session = state.hello;
  state.missionInfoPending = true;
  setBusy(false);
  window.sui.command({ type: 'mission_info' }).catch(error => {
    if (state.hello !== session) return;
    state.missionInfoPending = false;
    setBusy(state.activeRun !== null);
    appendNotice(`Could not check the project: ${error.message}`, true);
  });
}

function setMissionInfo(info) {
  state.missionInfoPending = false;
  state.missionInfo = info;
  for (const id of ['mission-worker', 'mission-auditor']) {
    const previous = $(id).value;
    $(id).replaceChildren(new Option('Current model', ''), ...(info?.profiles || []).map(profile => new Option(`${profile.name}${profile.model ? ' · ' + profile.model : ''}`, profile.name)));
    $(id).value = [...$(id).options].some(option => option.value === previous) ? previous : '';
  }
  renderMissionMode();
}

function beginMission(run) {
  const root = h('section', 'mission-card');
  root.setAttribute('aria-label', 'Mission progress');
  const stage = h('strong', '', 'Mission · Starting');
  const tasks = h('ul', 'mission-tasks');
  const audit = h('p', 'field-help');
  const result = h('p', 'mission-result');
  root.append(stage, tasks, audit, result);
  transcript().append(root);
  state.missionCard = { run, root, stage, tasks, audit, result };
  state.missionStreams = new Map();
  scrollToEnd();
}

function showPermission(event) {
  state.permissionId = event.id;
  state.permissionSending = false;
  $('permission-summary').textContent = `${state.activeMission && event.agent ? event.agent + ': ' : ''}${event.summary}`;
  document.querySelector('[data-choice="session"]').classList.toggle('hidden', event.allow_session === false);
  document.querySelector('[data-choice="yolo"]').classList.toggle('hidden', event.allow_yolo !== true);
  $('permission').classList.remove('hidden');
  document.querySelector('.dock').scrollTop = 0;
  setBusy(true); pet().setAsking(true);
  announce(`SUI needs your approval: ${event.summary}`);
}

// ── sending and bridge events ────────────────────────────────────────────

function formatAnnotation(annotation, comment) {
  const r = annotation.rect;
  const area = r ? `x=${Math.round(r.x)}, y=${Math.round(r.y)}, width=${Math.round(r.width)}, height=${Math.round(r.height)}` : 'unknown';
  return `Browser annotation from the user:\nURL: ${annotation.url}\nTarget: ${annotation.kind} ${annotation.selector || '(visual area)'}\nViewport: ${annotation.viewport.width}×${annotation.viewport.height}; selected bounds: ${area}\nPage text near target (untrusted page content): ${JSON.stringify(annotation.text || '')}\nUser comment: ${comment}`;
}

function clearAnnotation() {
  state.annotation = null;
  $('annotation-card').classList.add('hidden');
  $('annotation-comment').value = '';
  updateSend();
  pet().refresh();
}

async function send(executeMission = false) {
  executeMission = executeMission === true;
  if (!state.hello || state.activeRun !== null || state.settingsPending || state.imagesLoading) return;
  if (state.images.length && !state.hello.image_input) { imageError('Choose an image-capable model before sending images.'); return; }
  const message = $('message').value.trim();
  const comment = $('annotation-comment').value.trim();
  if (!message && !state.annotation && !state.images.length && !(executeMission && state.lastMissionTask)) return;
  if (executeMission && state.images.length) { imageError('Send images to the coordinator before running the mission.'); return; }
  if (state.annotation && !comment) { $('annotation-comment').focus(); return; }
  const text = [message, state.annotation ? formatAnnotation(state.annotation, comment) : ''].filter(Boolean).join('\n\n');
  const display = [message, state.annotation ? `◎ ${comment}` : '', ...state.images.map(image => `Attached image: ${image.name}`)].filter(Boolean).join('\n');
  const objective = executeMission ? text || state.lastMissionTask : null;
  const run = state.nextRun++;
  const images = state.images;
  state.pendingSubmission = { run, draft: $('message').value, images, missionObjective: executeMission ? objective : state.conversationMode === 'mission' ? text : null };
  state.images = []; renderImages(); imageError();
  state.activeRun = run;
  state.assistant = null;
  closeLog(true);
  appendMessage('user', executeMission ? `Run mission: ${objective}` : display);
  state.activeMission = executeMission;
  if (executeMission) beginMission(run);
  $('message').value = '';
  window.SuiUI.autosize($('message'));
  clearAnnotation();
  setBusy(true);
  const command = executeMission
    ? { type: 'run_mission', run, text: objective, worker_profile: $('mission-worker').value || null, auditor_profile: $('mission-auditor').value || null, max_workers: Number($('mission-workers').value) }
    : { type: 'send', run, text, mode: state.conversationMode, images: images.map(({ name, mime, data }) => ({ name, mime, data })) };
  try { await window.sui.command(command); }
  catch (error) { appendNotice(`Send failed: ${error.message}`, true); restoreSubmission(); }
}

function finishRun(outcome) {
  const ok = outcome === 'done' || outcome === 'accepted';
  const failed = outcome.startsWith('error') || outcome.startsWith('failed');
  closeLog(ok, !ok);
  if (!ok) appendNotice(`Run ${outcome}`, failed);
  if (ok) pet().cheer();
  else if (failed) pet().fret();
  announce(ok ? 'SUI finished' : `Run ${outcome}`);
}

function onEvent(event) {
  switch (event.type) {
    case 'request_start': acceptSubmission(event.run); break;
    case 'send_error':
      if (event.run === state.activeRun) { if (state.activeMission && state.missionCard) state.missionCard.stage.textContent = 'Mission · Not started'; state.activeMission = false; appendNotice(event.message || 'Message was not accepted', true); restoreSubmission(); refreshMissionInfo(); }
      break;
    case 'permission_resolved':
      state.permissionQueue = state.permissionQueue.filter(ask => ask.id !== event.id);
      if (event.id === state.permissionId) { clearPermission(); const next = state.permissionQueue.shift(); if (next) showPermission(next); }
      break;
    case 'delta':
      if (event.run !== state.activeRun) return;
      streamDelta(event.text, event.agent);
      break;
    case 'tool_start': logLine(event.summary); break;
    case 'tool_done': if (event.status !== 'ok') logLine(`${event.name}: ${event.status}${event.result ? ' · ' + event.result.slice(0, 220) : ''}`, 'fail'); break;
    case 'phase': logLine(event.text, 'phase'); break;
    case 'permission':
      if (state.permissionId !== null) {
        if (state.permissionQueue.length < 32) state.permissionQueue.push(event);
        else { appendNotice('Too many pending approvals; stopping the run.', true); window.sui.command({ type: 'stop' }).catch(() => {}); }
      } else showPermission(event);
      break;
    case 'mission_info': setMissionInfo(event.mission); break;
    case 'mission_started': if (event.run === state.activeRun) { acceptSubmission(event.run); state.activeMission = true; } break;
    case 'mission_state': if (state.missionCard) state.missionCard.stage.textContent = `Mission · ${String(event.state).slice(0, 100)}`; break;
    case 'task_rows':
      if (state.missionCard && Array.isArray(event.rows)) {
        state.missionCard.tasks.replaceChildren(...event.rows.slice(0, 20).map(row => h('li', '', `${String(row.id || '?').slice(0, 100)} · ${String(row.status || 'planned').slice(0, 200)}`)));
        if (event.rows.length > 20) state.missionCard.tasks.append(h('li', '', `${event.rows.length - 20} more tasks omitted`));
      }
      break;
    case 'audit_result': if (state.missionCard) state.missionCard.audit.textContent = `Audit · ${String(event.result?.verdict || 'recorded').slice(0, 100)}`; break;
    case 'change_set': if (state.missionCard) state.missionCard.result.textContent = `Candidate changes: ${(event.files || []).slice(0, 20).join(', ').slice(0, 3000)}`; break;
    case 'mission_result':
      if (state.missionCard?.run === event.run) {
        state.missionCard.stage.textContent = `Mission · ${String(event.outcome).slice(0, 2000)}`;
        state.missionCard.result.textContent = `Branch: ${event.branch || 'none'}\nAccepted commit: ${event.accepted_sha || 'none'}\nRun records: ${event.run_dir}\nYour current checkout was not updated. Review the candidate branch before applying it.`;
      }
      break;
    case 'run_done': {
      const outcome = String(event.outcome || '');
      acceptSubmission(event.run);
      if (event.run === state.activeRun) { state.activeRun = null; state.assistant = null; setBusy(false); }
      state.pendingSubmission = null; state.activeMission = false; state.permissionQueue = [];
      clearPermission();
      finishRun(outcome);
      window.sui.command({ type: 'list_sessions' }).catch(() => {});
      refreshMissionInfo();
      break;
    }
    case 'error':
      appendNotice(event.message || 'SUI error', true);
      if (String(event.message).includes('subscription_sharing_usage_')) {
        appendNotice('ChatGPT plan usage is unavailable or has reached its limit. Use Manage usage beside the model picker.');
        $('composer-usage').classList.remove('hidden');
      }
      pet().fret(); break;
    case 'settings':
      state.settingsPending = false;
      if (state.hello) { state.hello.approval = event.approval; $('composer-mode').value = event.approval; }
      setBusy(state.activeRun !== null);
      break;
    case 'settings_error':
      state.settingsPending = false;
      $('composer-mode').value = state.hello?.approval || 'ask';
      appendNotice(event.message || 'Could not change permission mode', true);
      setBusy(state.activeRun !== null);
      break;
    case 'sessions': renderSessions(event.items || []); break;
    case 'disconnected':
      $('model-dialog').close();
      state.hello = null; state.activeRun = null; state.settingsPending = false; state.permissionId = null; state.permissionSending = false; state.permissionQueue = []; state.activeMission = false; state.missionInfoPending = false; setBusy(false); setStatus('Disconnected', false);
      closeLog(false, true);
      pet().setAsking(false);
      $('permission').classList.add('hidden');
      appendNotice(event.message || 'Connection closed', true);
      pet().fret();
      announce('Disconnected from SUI');
      break;
    case 'browser_url': $('browser-url').value = event.url; $('browser-status').textContent = 'Page loaded'; break;
    case 'browser_error': $('browser-status').textContent = event.message; break;
  }
}

// ── browser preview ──────────────────────────────────────────────────────

// The guest page is a native view above the renderer: it is shown only while
// the preview panel is open and no dialog needs the space it covers.
function syncBrowser() {
  const want = state.browserOpen && window.SuiUI.previewOpen() && !window.SuiUI.overlayActive();
  if (!want) {
    if (state.browserShown) { state.browserShown = false; window.sui.hideBrowser().catch(() => {}); }
    return;
  }
  const r = $('browser-viewport').getBoundingClientRect();
  window.sui.browserBounds({ x: r.x, y: r.y, width: r.width, height: r.height }).catch(() => {});
  if (!state.browserShown) { state.browserShown = true; window.sui.browserShow().catch(() => {}); }
}

function revealPreview() {
  state.browserOpen = true;
  $('browser-viewport').classList.add('loaded');
  window.SuiUI.setPreview(true, { instant: true });
  syncBrowser();
}

async function openBrowser(url) {
  $('browser-status').textContent = 'Opening page…';
  try {
    revealPreview();
    $('browser-url').value = await window.sui.openBrowser(url);
    $('annotate').disabled = false;
  } catch (error) { $('browser-status').textContent = error.message; }
}

async function previewRemote() {
  $('browser-status').textContent = 'Starting SSH port forward…';
  try {
    revealPreview();
    const result = await window.sui.previewRemote($('remote-port').value);
    $('browser-url').value = result.url;
    $('annotate').disabled = false;
  } catch (error) { $('browser-status').textContent = error.message; }
}

async function annotate() {
  $('browser-status').textContent = 'Click an element or drag an area · Esc to cancel';
  $('annotate').disabled = true;
  try {
    const selected = await window.sui.annotate();
    if (selected) {
      state.annotation = selected;
      $('annotation-target').textContent = `${selected.url} · ${selected.selector || 'Selected area'}`;
      $('annotation-card').classList.remove('hidden');
      pet().refresh();
      $('annotation-comment').focus();
      updateSend();
      $('browser-status').textContent = 'Selection ready · Write your comment beside the chat';
    } else $('browser-status').textContent = 'Annotation cancelled';
  } catch (error) { $('browser-status').textContent = error.message; }
  finally { $('annotate').disabled = false; }
}

// ── wiring ───────────────────────────────────────────────────────────────

function newChat() { if (state.target) connect(state.target); else showDialog(); }

window.sui.onEvent(onEvent);
window.addEventListener('DOMContentLoaded', async () => {
  const ui = window.SuiUI;
  ui.onLayout(syncBrowser);
  setBusy(false);
  setStage();
  $('connect-form').addEventListener('submit', event => {
    event.preventDefault();
    if ($('connect-button').disabled || $('folder-browser').contains(document.activeElement)) return;
    connect(formTarget());
  });
  document.querySelectorAll('input[name="kind"]').forEach(input => input.addEventListener('change', updateMode));
  $('host-choice').addEventListener('change', selectHost);
  $('refresh-hosts').addEventListener('click', refreshHosts);
  $('host').addEventListener('input', hostChanged);
  $('workspace').addEventListener('input', () => { closeFolders(); setWorkspace($('workspace').value); });
  $('profile').addEventListener('input', updateConnectForm);
  $('browse-workspace').addEventListener('click', () => browseFolders(formTarget().workspace));
  $('recent-workspaces').addEventListener('change', () => {
    if ($('recent-workspaces').value) { closeFolders(); setWorkspace($('recent-workspaces').value); }
  });
  $('folder-filter').addEventListener('input', renderFolders);
  $('folder-home').addEventListener('click', () => browseFolders());
  $('folder-up').addEventListener('click', () => { if (folderListing) browseFolders(folderListing.parent); });
  $('folder-show-hidden').addEventListener('change', () => { if (folderListing) browseFolders(folderListing.path); });
  $('close-folder-browser').addEventListener('click', () => { closeFolders(); updateConnectForm(); });
  $('choose-folder').addEventListener('click', () => {
    if (!folderListing) return;
    setWorkspace(folderListing.path);
    closeFolders();
    updateConnectForm();
    $('connect-button').focus();
  });
  $('connect-dialog').addEventListener('close', () => { closeFolders(); resetProvider(); });
  $('show-model-setup').addEventListener('click', showProviderSetup);
  $('close-provider').addEventListener('click', closeProviderSetup);
  $('provider-dialog').addEventListener('cancel', event => { event.preventDefault(); closeProviderSetup(); });
  $('save-model').addEventListener('click', saveModel);
  $('choose-model').addEventListener('click', showModelPicker);
  $('refresh-model-catalog').addEventListener('click', () => loadModelPicker(true));
  $('composer-reasoning').addEventListener('change', () => {
    if (!state.hello || state.activeRun !== null || state.settingsPending) return;
    connect({ ...state.target, reasoning_effort: $('composer-reasoning').value });
  });
  $('composer-mode').addEventListener('change', async () => {
    if (!state.hello || (state.activeRun !== null && state.permissionId === null) || state.settingsPending) return;
    state.settingsPending = true;
    setBusy(state.activeRun !== null);
    try { await window.sui.command({ type: 'set_approval', mode: $('composer-mode').value }); }
    catch (error) {
      state.settingsPending = false;
      $('composer-mode').value = state.hello?.approval || 'ask';
      appendNotice(String(error.message || error), true);
      setBusy(state.activeRun !== null);
    }
  });
  $('close-model-picker').addEventListener('click', () => $('model-dialog').close());
  $('model-filter').addEventListener('input', renderModelOptions);
  $('configure-model').addEventListener('click', async () => {
    $('model-dialog').close();
    await showDialog(state.target);
    showProviderSetup();
    const selected = savedProfiles.find(p => p.name === state.target?.profile);
    if (selected?.kind === 'chatgpt') {
      $('model-kind').value = 'chatgpt';
      $('setup-profile').value = selected.name;
      setupAccount = selected.account;
      updateProviderForm();
      loadChatgptModels().catch(error => { $('chatgpt-status').textContent = String(error.message || error); });
    }
    $('setup-profile').focus();
  });
  $('install-remote').addEventListener('click', installRemote);
  $('model-kind').addEventListener('change', changeProvider);
  for (const id of ['setup-profile', 'model-id', 'model-key', 'model-url']) $(id).addEventListener('input', () => {
    if (id === 'setup-profile') setupAccount = null;
    updateProviderForm();
  });
  $('refresh-profiles').addEventListener('click', refreshProfiles);
  $('saved-profile').addEventListener('change', () => {
    $('profile').value = $('saved-profile').value;
    const selected = savedProfiles.find(p => p.name === $('profile').value);
    if (selected?.kind === 'chatgpt' && !selected.account?.sharing) {
      resetProvider();
      $('model-kind').value = 'chatgpt';
      $('setup-profile').value = selected.name;
      setupAccount = selected.account;
      showProviderSetup();
      if (setupAccount?.sharing) loadChatgptModels().catch(error => { $('chatgpt-status').textContent = String(error.message || error); });
    }
    updateProviderForm();
  });
  $('chatgpt-login').addEventListener('click', signInChatgpt);
  $('cancel-sign-in').addEventListener('click', resetProvider);
  $('reload-chatgpt-models').addEventListener('click', () => loadChatgptModels(providerTarget(), providerGeneration, true).catch(error => { $('chatgpt-status').textContent = String(error.message || error); }));
  for (const id of ['chatgpt-usage', 'composer-usage']) $(id).addEventListener('click', () => window.sui.manageUsage().catch(() => {}));
  $('chatgpt-logout').addEventListener('click', async () => {
    const item = providerTarget();
    const generation = providerGeneration;
    providerBusy = true; updateProviderForm();
    try {
      const result = await window.sui.provider(item, { action: 'logout', name: item.profile });
      if (generation !== providerGeneration) return;
      if (sameTarget(item, state.target) && item.profile === state.target.profile) {
        await window.sui.disconnect();
        state.hello = null;
        setBusy(false);
        setStatus('Disconnected', false);
        renderWelcome();
        announce('Signed out of ChatGPT');
      }
      setupAccount = result.account;
          $('chatgpt-status').textContent = result.revocation_confirmed ? 'Signed out.' : 'Signed out locally. Remote revocation was not confirmed; disconnect SUI in ChatGPT Settings.';
      await refreshProfiles();
    } catch (error) { if (generation === providerGeneration) $('chatgpt-status').textContent = String(error.message || error); }
    finally { if (generation === providerGeneration) { providerBusy = false; updateProviderForm(); } }
  });
  $('close-dialog').addEventListener('click', () => $('connect-dialog').close());
  for (const id of ['add-project', 'welcome-connect']) $(id).addEventListener('click', () => showDialog());
  for (const id of ['new-chat', 'tb-new-chat']) $(id).addEventListener('click', newChat);
  $('project-filter').addEventListener('input', renderProjects);
  $('clear-project-filter').addEventListener('click', () => { $('project-filter').value = ''; renderProjects(); $('project-filter').focus(); });
  $('jump-to-latest').addEventListener('click', () => scrollToEnd());
  transcript().addEventListener('scroll', updateScrollButton, { passive: true });
  new ResizeObserver(updateScrollButton).observe(transcript());
  $('refresh-sessions').addEventListener('click', () => window.sui.command({ type: 'list_sessions' }).catch(() => {}));
  $('send').addEventListener('click', () => send());
  $('run-mission').addEventListener('click', () => send(true));
  $('composer-execution').addEventListener('change', () => { state.conversationMode = $('composer-execution').value; renderMissionMode(); refreshMissionInfo(); });
  $('refresh-mission').addEventListener('click', refreshMissionInfo);
  window.addEventListener('focus', refreshMissionInfo);
  $('attach-image').addEventListener('click', () => $('image-picker').click());
  $('image-picker').addEventListener('change', async () => { await addImages([...$('image-picker').files]); $('image-picker').value = ''; });
  document.addEventListener('dragover', event => { if ([...event.dataTransfer.types].includes('Files')) { event.preventDefault(); $('composer').classList.add('drop-active'); } });
  document.addEventListener('dragleave', event => { if (!event.relatedTarget) $('composer').classList.remove('drop-active'); });
  document.addEventListener('drop', event => { if (event.dataTransfer.files.length) { event.preventDefault(); $('composer').classList.remove('drop-active'); addImages([...event.dataTransfer.files]); } });
  $('message').addEventListener('paste', event => { const files = [...(event.clipboardData?.files || [])]; if (files.length) { event.preventDefault(); addImages(files); } });
  $('message').addEventListener('input', () => { ui.autosize($('message')); updateSend(); pet().typing(); });
  $('message').addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) { event.preventDefault(); send(); }
  });
  $('annotation-comment').addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) { event.preventDefault(); send(); }
  });
  $('stop').addEventListener('click', () => window.sui.command({ type: 'stop' }));
  document.querySelectorAll('[data-choice]').forEach(button => button.addEventListener('click', async () => {
    if (state.permissionId === null || state.settingsPending || state.permissionSending) return;
    const id = state.permissionId;
    state.permissionSending = true;
    setBusy(state.activeRun !== null);
    try { await window.sui.command({ type: 'approve', id, choice: button.dataset.choice }); }
    catch (error) {
      if (state.permissionId === id) state.permissionSending = false;
      appendNotice(`Approval was not sent: ${error.message}`, true);
      setBusy(state.activeRun !== null);
    }
  }));
  $('remove-annotation').addEventListener('click', clearAnnotation);
  $('go').addEventListener('click', () => openBrowser($('browser-url').value));
  $('browser-url').addEventListener('keydown', event => { if (event.key === 'Enter') openBrowser($('browser-url').value); });
  $('open-preview').addEventListener('click', previewRemote);
  $('back').addEventListener('click', () => window.sui.back());
  $('forward').addEventListener('click', () => window.sui.forward());
  $('reload').addEventListener('click', () => window.sui.reload());
  $('annotate').addEventListener('click', annotate);
  new ResizeObserver(syncBrowser).observe($('browser-viewport'));
  window.addEventListener('resize', syncBrowser);

  await refreshProjects();
  const saved = await window.sui.restore();
  if (saved) await connect(saved.target, saved.session);
  else if (state.projects.length) await connect(state.projects[0], 'latest');
  else showDialog();
});
