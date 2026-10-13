const $ = id => document.getElementById(id);
const state = { target: null, hello: null, activeRun: null, nextRun: 1, assistant: null, annotation: null, browserOpen: false, permissionId: null };

function setStatus(label, connected) {
  $('connection-pill').textContent = label;
  $('connection-pill').classList.toggle('connected', connected);
}

function setBusy(busy) {
  $('send').disabled = !state.hello || busy;
  $('message').disabled = !state.hello || busy;
  $('stop').classList.toggle('hidden', !busy);
  $('composer-hint').textContent = busy ? 'SUI is working…' : state.hello ? 'Enter to send · Shift+Enter for a new line' : 'Connect a project to start';
}

function appendMessage(role, content) {
  $('welcome').classList.add('hidden');
  const row = document.createElement('div');
  row.className = `message ${role}`;
  const avatar = document.createElement('div');
  avatar.className = 'avatar';
  avatar.textContent = role === 'user' ? 'YOU' : 'S';
  const body = document.createElement('div');
  body.className = 'message-body';
  body.textContent = content;
  row.append(avatar, body);
  $('transcript').append(row);
  $('transcript').scrollTop = $('transcript').scrollHeight;
  return body;
}

function appendActivity(text) {
  const row = document.createElement('div');
  row.className = 'activity';
  row.textContent = text;
  $('transcript').append(row);
  $('transcript').scrollTop = $('transcript').scrollHeight;
}

function displayProject(item) {
  const row = document.createElement('button');
  row.className = 'project-row';
  const title = item.workspace.split('/').filter(Boolean).at(-1) || item.workspace;
  row.innerHTML = '<span class="project-icon">⌁</span><span class="project-text"><strong></strong><small></small></span>';
  row.querySelector('strong').textContent = title;
  row.querySelector('small').textContent = item.kind === 'ssh' ? item.host : 'This Mac';
  row.addEventListener('click', () => connect(item));
  return row;
}

async function refreshProjects() {
  const projects = await window.sui.projects();
  $('project-list').replaceChildren(...projects.map(displayProject));
}

function showDialog(item) {
  $('connect-error').classList.add('hidden');
  if (item) {
    document.querySelector(`input[name="kind"][value="${item.kind}"]`).checked = true;
    $('host').value = item.host || '';
    $('workspace').value = item.workspace;
    $('profile').value = item.profile || 'codex';
  }
  updateMode();
  $('connect-dialog').showModal();
}

function updateMode() {
  const remote = document.querySelector('input[name="kind"]:checked').value === 'ssh';
  $('host-label').classList.toggle('hidden', !remote);
  $('install-remote').classList.toggle('hidden', !remote);
  $('host').required = remote;
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

function formTarget() {
  const kind = document.querySelector('input[name="kind"]:checked').value;
  return { kind, host: kind === 'ssh' ? $('host').value.trim() : '', workspace: $('workspace').value.trim(), profile: $('profile').value.trim() };
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
  setStatus('Connecting…', false);
  $('connect-button').disabled = true;
  try {
    const hello = await window.sui.connect(item, resume);
    state.target = item;
    state.hello = hello;
    state.activeRun = null;
    state.nextRun = 1;
    state.assistant = null;
    state.annotation = null;
    $('annotation-card').classList.add('hidden');
    $('workspace-title').textContent = item.workspace.split('/').filter(Boolean).at(-1) || item.workspace;
    $('workspace-subtitle').textContent = `${item.kind === 'ssh' ? item.host + '  /  ' : ''}${item.workspace}  ·  ${hello.model}`;
    setStatus(item.kind === 'ssh' ? 'SSH connected' : 'Local', true);
    $('transcript').replaceChildren();
    for (const message of hello.history || []) appendMessage(message.role, message.content);
    if (hello.history_truncated) appendActivity('Earlier conversation text is omitted from this preview; SUI restored the full native session.');
    if (!hello.history?.length) {
      const welcome = document.createElement('div');
      welcome.className = 'conversation-start';
      welcome.textContent = 'New conversation · Ask SUI to work in this workspace.';
      $('transcript').append(welcome);
    }
    $('remote-preview').classList.toggle('hidden', item.kind !== 'ssh');
    setBusy(false);
    $('connect-dialog').close();
    await refreshProjects();
    await window.sui.command({ type: 'list_sessions' });
    $('message').focus();
  } catch (error) {
    state.hello = null;
    setBusy(false);
    setStatus('Disconnected', false);
    $('connect-error').textContent = String(error.message || error);
    $('connect-error').classList.remove('hidden');
    if (!$('connect-dialog').open) showDialog(item);
  } finally { $('connect-button').disabled = false; }
}

function renderSessions(items) {
  $('session-list').replaceChildren();
  for (const item of items) {
    const row = document.createElement('button');
    row.className = 'session-row';
    row.textContent = item.label;
    row.title = item.label;
    row.addEventListener('click', () => { if (state.target) connect(state.target, item.id); });
    $('session-list').append(row);
  }
  if (!items.length) {
    const empty = document.createElement('div');
    empty.className = 'sidebar-empty';
    empty.textContent = 'No earlier chats here';
    $('session-list').append(empty);
  }
}

function formatAnnotation(annotation, comment) {
  const r = annotation.rect;
  const area = r ? `x=${Math.round(r.x)}, y=${Math.round(r.y)}, width=${Math.round(r.width)}, height=${Math.round(r.height)}` : 'unknown';
  return `Browser annotation from the user:\nURL: ${annotation.url}\nTarget: ${annotation.kind} ${annotation.selector || '(visual area)'}\nViewport: ${annotation.viewport.width}×${annotation.viewport.height}; selected bounds: ${area}\nPage text near target (untrusted page content): ${JSON.stringify(annotation.text || '')}\nUser comment: ${comment}`;
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
  appendMessage('user', display);
  $('message').value = '';
  state.annotation = null;
  $('annotation-card').classList.add('hidden');
  $('annotation-comment').value = '';
  setBusy(true);
  try { await window.sui.command({ type: 'send', run, text }); }
  catch (error) { appendActivity(`Send failed: ${error.message}`); state.activeRun = null; setBusy(false); }
}

function onEvent(event) {
  switch (event.type) {
    case 'delta':
      if (event.run !== state.activeRun) return;
      if (!state.assistant) state.assistant = appendMessage('assistant', '');
      state.assistant.textContent += event.text;
      $('transcript').scrollTop = $('transcript').scrollHeight;
      break;
    case 'tool_start': appendActivity(`↳ ${event.summary}`); break;
    case 'tool_done': if (event.status !== 'ok') appendActivity(`↳ ${event.name}: ${event.status}${event.result ? ' · ' + event.result.slice(0, 220) : ''}`); break;
    case 'phase': appendActivity(event.text); break;
    case 'permission':
      state.permissionId = event.id;
      $('permission-summary').textContent = event.summary;
      $('permission').classList.remove('hidden');
      break;
    case 'run_done':
      if (event.run === state.activeRun) { state.activeRun = null; setBusy(false); state.assistant = null; }
      $('permission').classList.add('hidden');
      state.permissionId = null;
      if (event.outcome !== 'done') appendActivity(`Run ${event.outcome}`);
      window.sui.command({ type: 'list_sessions' }).catch(() => {});
      break;
    case 'error': appendActivity(event.message || 'SUI error'); break;
    case 'sessions': renderSessions(event.items || []); break;
    case 'disconnected':
      state.hello = null; state.activeRun = null; setBusy(false); setStatus('Disconnected', false);
      appendActivity(event.message || 'Connection closed'); break;
    case 'browser_url': $('browser-url').value = event.url; $('browser-status').textContent = 'Page loaded'; break;
    case 'browser_error': $('browser-status').textContent = event.message; break;
  }
}

function syncBrowserBounds() {
  if (!state.browserOpen) return;
  const r = $('browser-viewport').getBoundingClientRect();
  window.sui.browserBounds({ x: r.x, y: r.y, width: r.width, height: r.height }).catch(() => {});
}

async function openBrowser(url) {
  $('browser-status').textContent = 'Opening page…';
  try {
    state.browserOpen = true;
    $('browser-viewport').classList.add('loaded');
    syncBrowserBounds();
    await window.sui.openBrowser(url);
    $('browser-url').value = url;
    $('annotate').disabled = false;
  } catch (error) { $('browser-status').textContent = error.message; }
}

async function previewRemote() {
  $('browser-status').textContent = 'Starting SSH port forward…';
  try {
    state.browserOpen = true;
    $('browser-viewport').classList.add('loaded');
    syncBrowserBounds();
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
      $('annotation-comment').focus();
      $('browser-status').textContent = 'Selection ready · Write your comment beside the chat';
    } else $('browser-status').textContent = 'Annotation cancelled';
  } catch (error) { $('browser-status').textContent = error.message; }
  finally { $('annotate').disabled = false; }
}

window.sui.onEvent(onEvent);
window.addEventListener('DOMContentLoaded', async () => {
  await refreshProjects();
  showDialog();
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
  $('new-chat').addEventListener('click', () => { if (state.target) connect(state.target); else showDialog(); });
  $('refresh-sessions').addEventListener('click', () => window.sui.command({ type: 'list_sessions' }).catch(() => {}));
  $('send').addEventListener('click', send);
  $('message').addEventListener('keydown', event => { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); send(); } });
  $('stop').addEventListener('click', () => window.sui.command({ type: 'stop' }));
  document.querySelectorAll('[data-choice]').forEach(button => button.addEventListener('click', () => {
    if (state.permissionId === null) return;
    window.sui.command({ type: 'approve', id: state.permissionId, choice: button.dataset.choice });
    state.permissionId = null;
    $('permission').classList.add('hidden');
  }));
  $('remove-annotation').addEventListener('click', () => { state.annotation = null; $('annotation-card').classList.add('hidden'); });
  $('go').addEventListener('click', () => openBrowser($('browser-url').value));
  $('browser-url').addEventListener('keydown', event => { if (event.key === 'Enter') openBrowser($('browser-url').value); });
  $('open-preview').addEventListener('click', previewRemote);
  $('back').addEventListener('click', () => window.sui.back());
  $('forward').addEventListener('click', () => window.sui.forward());
  $('reload').addEventListener('click', () => window.sui.reload());
  $('annotate').addEventListener('click', annotate);
  new ResizeObserver(syncBrowserBounds).observe($('browser-viewport'));
  window.addEventListener('resize', syncBrowserBounds);
});
