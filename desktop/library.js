// Private, device-local drafts and chat labels. Nothing here grants a runtime
// permission or is sent to a provider until the user presses Send.
const fs = require('node:fs');
const path = require('node:path');
const { createHash, randomUUID } = require('node:crypto');
const LIMIT = 12 * 1024 * 1024, TOTAL = 64 * 1024 * 1024, ENTRIES = 1000;

function text(value, limit, label) {
  if (typeof value !== 'string' || Buffer.byteLength(value) > limit || value.includes('\0')) throw new Error(`Invalid ${label}.`);
  return value;
}
function scope(value) {
  if (!value || !['local', 'ssh'].includes(value.kind)) throw new Error('Invalid chat workspace.');
  const workspace = text(value.workspace, 4096, 'workspace');
  if (!workspace.startsWith('/')) throw new Error('Choose an absolute workspace path.');
  return { kind: value.kind, host: value.kind === 'ssh' ? text(value.host, 512, 'host') : '', workspace,
    conversation: text(value.conversation, 256, 'conversation') };
}
const key = value => createHash('sha256').update(JSON.stringify(scope(value))).digest('hex');
const sameWorkspace = (a, b) => a.kind === b.kind && a.host === (b.kind === 'ssh' ? b.host : '') && a.workspace === b.workspace;
function image(value) {
  const name = text(value?.name, 255, 'image name');
  if (!['image/png', 'image/jpeg', 'image/webp'].includes(value.mime)) throw new Error('Unsupported draft image.');
  const data = text(value.data, Math.ceil(4 * 1024 * 1024 / 3) * 4, 'image');
  const bytes = Buffer.from(data, 'base64');
  if (!bytes.length || bytes.length > 4 * 1024 * 1024 || bytes.toString('base64') !== data) throw new Error('Invalid draft image.');
  return { name, mime: value.mime, data, size: bytes.length };
}
function draft(value) {
  if (!value) return { text: '', images: [], annotation: null, comment: '' };
  if (!Array.isArray(value.images) || value.images.length > 4) throw new Error('Attach at most four draft images.');
  const result = { text: text(value.text, 128 * 1024, 'draft text'), images: value.images.map(image), annotation: null, comment: text(value.comment || '', 128 * 1024, 'annotation comment') };
  if (value.annotation) {
    const a = value.annotation;
    const numbers = (v, names) => Object.fromEntries(names.map(name => {
      if (!Number.isFinite(v?.[name])) throw new Error('Invalid annotation geometry.');
      return [name, v[name]];
    }));
    result.annotation = { kind: text(a.kind, 32, 'annotation kind'), url: text(a.url, 8192, 'annotation URL'), selector: text(a.selector || '', 4096, 'annotation selector'), text: text(a.text || '', 32768, 'annotation text'),
      rect: numbers(a.rect, ['x','y','width','height']), viewport: numbers(a.viewport, ['width','height']), resized: a.resized === true, ...(a.image ? { image: image(a.image) } : {}) };
  }
  const images = [...result.images, ...(result.annotation?.image ? [result.annotation.image] : [])];
  if (images.length > 4 || images.reduce((n, i) => n + i.size, 0) > 8 * 1024 * 1024) throw new Error('Draft images exceed four images or 8 MiB.');
  return result;
}
const hasDraft = d => !!(d.text || d.images.length || d.annotation);
function connection(value) {
  if (!value) return null;
  const result = { profile: text(value.profile, 64, 'profile'), run: value.run == null ? null : text(value.run, 160, 'saved run') };
  for (const field of ['model', 'reasoning_effort']) if (value[field]) result[field] = text(value[field], 256, field);
  return result;
}

function createLibrary(root) {
  const directory = path.join(root, 'chats');
  function files() {
    if (!fs.existsSync(directory)) return [];
    if (!fs.lstatSync(directory).isDirectory()) throw new Error('Chat storage is not a private directory.');
    const entries = fs.readdirSync(directory, { withFileTypes: true });
    if (entries.length > ENTRIES) throw new Error('Chat storage exceeds 1000 entries. Remove unused drafts or labels before saving.');
    return entries.filter(e => /^[a-f0-9]{64}\.json$/.test(e.name)).map(e => {
      if (!e.isFile()) throw new Error('Invalid chat storage entry.');
      const file = path.join(directory, e.name);
      return { file, size: fs.statSync(file).size };
    });
  }
  function readFile(file) {
    let fd;
    try { fd = fs.openSync(file, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW || 0) | (fs.constants.O_NONBLOCK || 0)); }
    catch (error) { if (error.code === 'ENOENT') return null; throw error; }
    try {
      const info = fs.fstatSync(fd);
      if (!info.isFile() || info.size > LIMIT) throw new Error('Saved chat exceeds its size limit.');
      const buffer = Buffer.alloc(Math.min(info.size + 1, LIMIT + 1));
      const count = fs.readSync(fd, buffer, 0, buffer.length, 0);
      if (count !== info.size) throw new Error('Saved chat changed while loading.');
      const record = JSON.parse(buffer.subarray(0, count));
      if (record.version !== 1 || path.basename(file) !== key(record.scope) + '.json') throw new Error('Invalid saved chat.');
      return { version: 1, scope: scope(record.scope), title: text(record.title, 480, 'chat title'), pinned: record.pinned === true, updated: Number(record.updated) || 0, connection: connection(record.connection), draft: draft(record.draft) };
    } finally { fs.closeSync(fd); }
  }
  const read = value => {
    if (fs.existsSync(directory) && !fs.lstatSync(directory).isDirectory()) throw new Error('Chat storage is not a private directory.');
    return readFile(path.join(directory, key(value) + '.json'));
  };
  function write(value, patch) {
    const identity = scope(value), file = path.join(directory, key(identity) + '.json');
    const previous = read(identity);
    const record = previous || { version: 1, scope: identity, title: '', pinned: false, connection: null, draft: draft(null) };
    if (Object.hasOwn(patch, 'draft')) record.draft = draft(patch.draft);
    if (Object.hasOwn(patch, 'connection')) record.connection = connection(patch.connection);
    if (Object.hasOwn(patch, 'title')) record.title = text(patch.title, 480, 'chat title').replace(/[\r\n\t]/g, ' ').trim();
    if (Object.hasOwn(patch, 'pinned')) record.pinned = patch.pinned === true;
    record.updated = Date.now();
    if (!hasDraft(record.draft) && !record.title && !record.pinned) { fs.rmSync(file, { force: true }); return null; }
    const data = JSON.stringify(record);
    if (Buffer.byteLength(data) > LIMIT) throw new Error('Draft exceeds the 12 MiB storage limit.');
    const entries = files();
    if ((!previous && entries.length >= ENTRIES) || entries.filter(e => e.file !== file).reduce((n, e) => n + e.size, 0) + Buffer.byteLength(data) > TOTAL) throw new Error('Draft storage is full (64 MiB). Clear unused drafts before saving.');
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    fs.chmodSync(directory, 0o700);
    const temporary = path.join(directory, `${randomUUID()}.tmp`);
    try { fs.writeFileSync(temporary, data, { mode: 0o600, flag: 'wx' }); fs.renameSync(temporary, file); }
    finally { fs.rmSync(temporary, { force: true }); }
    return record;
  }
  function list(target) {
    const entries = files();
    if (entries.reduce((n, e) => n + e.size, 0) > TOTAL) throw new Error('Chat storage exceeds 64 MiB.');
    const items = entries.map(e => readFile(e.file)).filter(r => r && sameWorkspace(r.scope, target)).map(r => ({ scope: r.scope, title: r.title, pinned: r.pinned, updated: r.updated, connection: r.connection, hasDraft: hasDraft(r.draft), draftTitle: (r.draft.text || r.draft.comment || 'Image draft').slice(0, 80) }));
    items.sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.updated - a.updated);
    return { items: items.slice(0, 200), truncated: items.length > 200 };
  }
  return { read, write, list };
}
module.exports = { createLibrary };
