// Device-local presentation state, scoped by the native conversation identity.
(() => {
  const $ = id => document.getElementById(id);
  const empty = () => ({ text: '', images: [], annotation: null, comment: '' });
  const hasDraft = d => !!(d?.text || d?.images?.length || d?.annotation);
  let api, current = null, connectionInfo = null, loading = false, timer, pending = Promise.resolve(), last = '', request = 0;
  let sessions = [], metadata = [], editing = null, listTruncated = false, listError = '';
  const identity = (target, conversation) => ({ kind: target.kind, host: target.kind === 'ssh' ? target.host : '', workspace: target.workspace, conversation });
  const same = (a, b) => a && b && a.kind === b.kind && a.host === b.host && a.workspace === b.workspace;
  function status(message, error = false) {
    $('draft-status').textContent = message; $('draft-status').title = message; $('draft-status').classList.toggle('error-text', error);
  }
  function payload() {
    const { target, hello, history } = api.context();
    if (hello) connectionInfo = { profile: target.profile, model: hello.model, reasoning_effort: hello.reasoning_effort, run: history ? hello.run_id : null };
    return { draft: api.draft(), connection: connectionInfo };
  }
  function remember(scope, patch) {
    let row = metadata.find(r => r.scope.conversation === scope.conversation);
    if (!row) { row = { scope, title: '', pinned: false }; metadata.push(row); }
    Object.assign(row, patch, { updated: Date.now() });
    if (patch.draft) {
      row.hasDraft = hasDraft(patch.draft); row.draftTitle = (patch.draft.text || patch.draft.comment || 'Image draft').slice(0, 80); delete row.draft;
    }
    if (!row.hasDraft && !row.title && !row.pinned) metadata = metadata.filter(r => r !== row);
    render();
  }
  async function flush() {
    clearTimeout(timer);
    if (!current || loading) return;
    const scope = current, patch = payload(), serialized = JSON.stringify(patch);
    if (last === serialized) return pending;
    last = serialized; status('Saving draft…');
    const operation = window.sui.writeChat(scope, patch); pending = operation;
    try {
      await operation;
      if (scope !== current) return;
      remember(scope, patch);
      if (last === serialized) status(hasDraft(patch.draft) ? 'Draft saved on this device' : '');
    } catch (error) { if (scope === current) { last = ''; status(`Draft not saved: ${error.message}`, true); } throw error; }
  }
  function changed() {
    if (!current || loading) return;
    clearTimeout(timer); status(hasDraft(api.draft()) ? 'Draft changed' : '');
    $('clear-draft').classList.toggle('hidden', !hasDraft(api.draft()));
    timer = setTimeout(() => flush().catch(() => {}), 400);
  }
  async function load(target, hello, options = {}) {
    const previous = current;
    loading = true; clearTimeout(timer); last = ''; sessions = []; metadata = []; listTruncated = false; listError = '';
    $('message').disabled = true;
    current = identity(target, hello.conversation_id || hello.run_id);
    const scope = current;
    try {
      let record = await window.sui.readChat(scope);
      const source = options.draft ? identity(target, options.draft) : options.carry && same(previous, scope) ? previous : null;
      if (source && source.conversation !== scope.conversation && !hasDraft(record?.draft)) {
        const old = await window.sui.readChat(source);
        if (old) {
          const patch = { draft: old.draft, title: record?.title || old.title, pinned: record?.pinned || old.pinned, connection: payload().connection };
          await window.sui.writeChat(scope, patch);
          // A conversation with history keeps its label. An unsent chat has
          // no resumable journal, so move its entire local identity on reopen.
          await window.sui.writeChat(source, { draft: empty(), ...(!old.connection?.run ? { title: '', pinned: false } : {}) });
          record = patch;
        }
      }
      api.apply(record?.draft || empty());
      status(hasDraft(record?.draft) ? 'Draft restored on this device' : '');
    } catch (error) { api.apply(empty()); status(`Could not load draft: ${error.message}`, true); }
    finally { loading = false; $('clear-draft').classList.toggle('hidden', !hasDraft(api.draft())); }
    await refresh();
  }
  async function refresh() {
    if (!current) return;
    const id = ++request, scope = current;
    try {
      const result = await window.sui.listChats(scope);
      if (id !== request || scope !== current) return;
      metadata = result.items;
      listTruncated = result.truncated; listError = '';
      render();
    } catch (error) { if (scope === current) { listError = error.message; render(); } }
  }
  function render() {
    if (!current) return;
    const entries = new Map();
    for (const row of sessions) {
      const id = row.conversation_id || row.id;
      if (!entries.has(id)) entries.set(id, { ...row, scope: { ...current, conversation: id } });
    }
    for (const meta of metadata) {
      const id = meta.scope.conversation;
      entries.set(id, { label: meta.draftTitle || 'Saved chat', ...entries.get(id), ...meta });
    }
    const query = $('chat-filter').value.trim().toLocaleLowerCase();
    const rows = [...entries.values()].sort((a, b) => Number(!!b.pinned) - Number(!!a.pinned));
    $('chat-list-status').textContent = listError || (listTruncated || sessions.length >= 200 || rows.length > 200 ? 'Limited to 200 recent runs and 200 saved chats; showing up to 200 matches.' : '');
    $('session-list').replaceChildren();
    for (const row of rows.filter(r => `${r.title || ''} ${r.label} ${r.draftTitle || ''} ${r.profile || r.connection?.profile || ''}`.toLocaleLowerCase().includes(query)).slice(0, 200)) {
      const wrapper = document.createElement('div'); wrapper.className = 'session-entry';
      const open = document.createElement('button'); open.type = 'button'; open.className = 'session-row';
      open.textContent = `${row.pinned ? '★ ' : ''}${row.title || row.label}${row.hasDraft ? ' · Draft' : ''}`; open.title = open.textContent;
      open.setAttribute('aria-current', String(row.scope.conversation === current.conversation));
      open.addEventListener('click', () => {
        if (row.scope.conversation === current.conversation) { $('message').focus(); return; }
        const target = { ...api.context().target, ...row.connection, profile: row.profile || row.connection?.profile || api.context().target.profile };
        api.connect(target, row.id || row.connection?.run || null, null, { draft: row.scope.conversation });
      });
      const edit = document.createElement('button'); edit.type = 'button'; edit.className = 'chat-edit'; edit.textContent = '⋯'; edit.setAttribute('aria-label', `Rename or pin ${row.title || row.label}`);
      edit.addEventListener('click', () => {
        editing = row; $('chat-title').value = row.title || ''; $('chat-title').placeholder = row.label;
        $('chat-pinned').checked = !!row.pinned; $('chat-edit-error').textContent = '';
        window.SuiUI.openDialog($('chat-edit-dialog')); $('chat-title').focus();
      });
      wrapper.append(open, edit); $('session-list').append(wrapper);
    }
    if (!$('session-list').childElementCount) {
      const notice = document.createElement('div'); notice.className = 'sidebar-empty'; notice.textContent = query ? 'No matching recent chats' : 'No earlier chats here'; $('session-list').append(notice);
    }
  }
  function init(callbacks) {
    api = callbacks;
    $('chat-filter').addEventListener('input', render);
    $('clear-draft').addEventListener('click', () => { api.apply(empty()); changed(); flush().catch(() => {}); });
    $('close-chat-edit').addEventListener('click', () => $('chat-edit-dialog').close());
    $('chat-edit-form').addEventListener('submit', async event => {
      event.preventDefault(); if (!editing) return;
      const row = editing, patch = { title: $('chat-title').value, pinned: $('chat-pinned').checked };
      if (row.connection) patch.connection = row.connection;
      else patch.connection = { profile: row.profile || api.context().target.profile, run: row.id };
      try { await window.sui.writeChat(row.scope, patch); remember(row.scope, patch); $('chat-edit-dialog').close(); }
      catch (error) { $('chat-edit-error').textContent = error.message; }
    });
    window.addEventListener('beforeunload', event => {
      if (!current || loading) return;
      const patch = payload();
      const result = window.sui.writeChatSync(current, patch);
      if (result?.error) { status(`Draft not saved: ${result.error}`, true); event.preventDefault(); event.returnValue = ''; }
    });
  }
  window.SuiChats = { init, flush, changed, load, refresh, sessions(items) { sessions = items; refresh(); } };
})();
