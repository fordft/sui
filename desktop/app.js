const $ = id => document.getElementById(id);
const state = {
  target: null, hello: null, activeRun: null, nextRun: 1, assistant: null, annotation: null,
  browserOpen: false, browserShown: false, permissionId: null, log: null, messages: 0, projects: []
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
  const draft = $('message').value.trim() || state.annotation;
  $('send').disabled = !state.hello || state.activeRun !== null || !draft;
}

function setBusy(busy) {
  const ready = !!state.hello;
  $('message').disabled = !ready;
  $('composer-context').classList.toggle('hidden', !ready);
  $('stop').classList.toggle('hidden', !busy);
  $('composer-hint').textContent = busy ? 'SUI is working…' : ready ? 'Enter to send · Shift+Enter for a new line' : 'Connect a project to start';
  $('connection-pill').classList.toggle('busy', busy && ready);
  updateSend();
  pet().setBusy(busy);
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

function streamDelta(text) {
  const stick = pinned();
  if (!state.assistant) { closeLog(true); state.assistant = appendMessage('assistant', ''); }
  const message = state.assistant;
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
  row.addEventListener('click', () => connect(item));
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
    row.addEventListener('click', () => { if (state.target) connect(state.target, item.id); });
    $('session-list').append(row);
  }
  if (!items.length) $('session-list').append(h('div', 'sidebar-empty', 'No earlier chats here'));
}

// ── connection dialog ────────────────────────────────────────────────────

function showDialog(item) {
  $('connect-error').classList.add('hidden');
  if (item) {
    document.querySelector(`input[name="kind"][value="${item.kind}"]`).checked = true;
    $('host').value = item.host || '';
    $('workspace').value = item.workspace;
    $('profile').value = item.profile || 'codex';
  }
  updateMode();
  window.SuiUI.openDialog($('connect-dialog'));
}

function updateMode() {
  const remote = document.querySelector('input[name="kind"]:checked').value === 'ssh';
  $('host-label').classList.toggle('hidden', !remote);
  $('install-remote').classList.toggle('hidden', !remote);
  $('host').required = remote;
}

function formTarget() {
  const kind = document.querySelector('input[name="kind"]:checked').value;
  return { kind, host: kind === 'ssh' ? $('host').value.trim() : '', workspace: $('workspace').value.trim(), profile: $('profile').value.trim() };
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

async function saveModel() {
  const item = formTarget();
  $('connect-error').classList.add('hidden');
  $('save-model').disabled = true;
  try {
    await window.sui.configure(item, { name: item.profile, kind: $('model-kind').value, base_url: $('model-url').value.trim(), model: $('model-id').value.trim(), api_key: $('model-key').value });
    $('model-key').value = '';
    $('model-setup').classList.add('hidden');
    $('show-model-setup').textContent = '✓ Model profile saved on host';
  } catch (error) {
    $('connect-error').textContent = String(error.message || error);
    $('connect-error').classList.remove('hidden');
  } finally { $('save-model').disabled = false; }
}

async function connect(item, resume = null) {
  if (state.activeRun !== null) return;
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
    pet().setAsking(false);
    $('workspace-title').textContent = projectName(item);
    $('workspace-subtitle').textContent = `${item.kind === 'ssh' ? item.host + '  /  ' : ''}${item.workspace}  ·  ${hello.model}`;
    $('composer-model').textContent = hello.model;
    $('composer-model').title = hello.model;
    $('composer-target').textContent = `${projectName(item)} · ${hostLabel(item)}`;
    $('composer-target').title = `${hostLabel(item)} · ${item.workspace}`;
    document.title = `${projectName(item)} · SUI`;
    setStatus(item.kind === 'ssh' ? 'SSH connected' : 'Local', true);
    transcript().replaceChildren();
    for (const message of hello.history || []) appendMessage(message.role, message.content);
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
  } finally { $('connect-button').disabled = false; }
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

async function send() {
  if (!state.hello || state.activeRun !== null) return;
  const message = $('message').value.trim();
  const comment = $('annotation-comment').value.trim();
  if (!message && !state.annotation) return;
  if (state.annotation && !comment) { $('annotation-comment').focus(); return; }
  const text = [message, state.annotation ? formatAnnotation(state.annotation, comment) : ''].filter(Boolean).join('\n\n');
  const display = [message, state.annotation ? `◎ ${comment}` : ''].filter(Boolean).join('\n');
  const run = state.nextRun++;
  state.activeRun = run;
  state.assistant = null;
  closeLog(true);
  appendMessage('user', display);
  $('message').value = '';
  window.SuiUI.autosize($('message'));
  clearAnnotation();
  setBusy(true);
  try { await window.sui.command({ type: 'send', run, text }); }
  catch (error) { appendNotice(`Send failed: ${error.message}`, true); state.activeRun = null; setBusy(false); }
}

function finishRun(outcome) {
  const ok = outcome === 'done' || outcome === 'accepted';
  const failed = outcome.startsWith('error');
  closeLog(ok, !ok);
  if (!ok) appendNotice(`Run ${outcome}`, failed);
  if (ok) pet().cheer();
  else if (failed) pet().fret();
  announce(ok ? 'SUI finished' : `Run ${outcome}`);
}

function onEvent(event) {
  switch (event.type) {
    case 'delta':
      if (event.run !== state.activeRun) return;
      streamDelta(event.text);
      break;
    case 'tool_start': logLine(event.summary); break;
    case 'tool_done': if (event.status !== 'ok') logLine(`${event.name}: ${event.status}${event.result ? ' · ' + event.result.slice(0, 220) : ''}`, 'fail'); break;
    case 'phase': logLine(event.text, 'phase'); break;
    case 'permission':
      state.permissionId = event.id;
      $('permission-summary').textContent = event.summary;
      $('permission').classList.remove('hidden');
      pet().setAsking(true);
      announce(`SUI needs your approval: ${event.summary}`);
      break;
    case 'run_done': {
      const outcome = String(event.outcome || '');
      if (event.run === state.activeRun) { state.activeRun = null; state.assistant = null; setBusy(false); }
      $('permission').classList.add('hidden');
      state.permissionId = null;
      pet().setAsking(false);
      finishRun(outcome);
      window.sui.command({ type: 'list_sessions' }).catch(() => {});
      break;
    }
    case 'error': appendNotice(event.message || 'SUI error', true); pet().fret(); break;
    case 'sessions': renderSessions(event.items || []); break;
    case 'disconnected':
      state.hello = null; state.activeRun = null; setBusy(false); setStatus('Disconnected', false);
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
    await window.sui.openBrowser(url);
    $('browser-url').value = url;
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
    connect(formTarget());
  });
  document.querySelectorAll('input[name="kind"]').forEach(input => input.addEventListener('change', updateMode));
  $('show-model-setup').addEventListener('click', () => $('model-setup').classList.toggle('hidden'));
  $('save-model').addEventListener('click', saveModel);
  $('install-remote').addEventListener('click', installRemote);
  $('model-kind').addEventListener('change', () => {
    if ($('model-kind').value === 'ollama') $('model-url').value = 'http://127.0.0.1:11434/v1';
    else if ($('model-url').value === 'http://127.0.0.1:11434/v1') $('model-url').value = 'https://api.openai.com/v1';
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
  $('send').addEventListener('click', send);
  $('message').addEventListener('input', () => { ui.autosize($('message')); updateSend(); pet().typing(); });
  $('message').addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) { event.preventDefault(); send(); }
  });
  $('annotation-comment').addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) { event.preventDefault(); send(); }
  });
  $('stop').addEventListener('click', () => window.sui.command({ type: 'stop' }));
  document.querySelectorAll('[data-choice]').forEach(button => button.addEventListener('click', () => {
    if (state.permissionId === null) return;
    window.sui.command({ type: 'approve', id: state.permissionId, choice: button.dataset.choice });
    state.permissionId = null;
    $('permission').classList.add('hidden');
    pet().setAsking(false);
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
  if (!state.projects.length) showDialog();
});
