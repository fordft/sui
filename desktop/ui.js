// Appearance and layout: preferences (theme, motion, sidebar, preview,
// companion), platform chrome, keyboard shortcuts, dialogs, and the
// appearance panel. Preferences live in localStorage on this device only.
// app.js owns the conversation and bridge; it uses this through window.SuiUI.
(() => {
  'use strict';

  const KEY = 'sui.desktop.ui.v1';
  const root = document.documentElement;
  const $ = id => document.getElementById(id);
  const platform = ['darwin', 'win32', 'linux'].includes(window.sui?.platform) ? window.sui.platform : 'linux';
  const isMac = platform === 'darwin';
  const dark = matchMedia('(prefers-color-scheme: dark)');
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  const listeners = new Set();

  function load() {
    let saved = {};
    try { saved = JSON.parse(localStorage.getItem(KEY) || '{}') || {}; } catch { /* corrupt prefs fall back to defaults */ }
    return {
      theme: ['system', 'light', 'dark'].includes(saved.theme) ? saved.theme : 'system',
      motion: ['full', 'calm', 'off'].includes(saved.motion) ? saved.motion : null,
      sidebar: saved.sidebar !== false,
      preview: saved.preview === true,
      pet: SuiPet.sanitize(saved.pet)
    };
  }
  let prefs = load();
  const save = () => { try { localStorage.setItem(KEY, JSON.stringify(prefs)); } catch { /* storage unavailable: preferences just won't persist */ } };
  const notify = () => listeners.forEach(fn => fn());

  const fmtKey = k => (isMac ? '⌘' + k.toUpperCase() : 'Ctrl+' + k.toUpperCase());
  const localLabel = () => (platform === 'darwin' ? 'This Mac' : platform === 'win32' ? 'This PC' : 'This computer');
  const motionOf = () => prefs.motion || (reduced.matches ? 'calm' : 'full');
  const themeOf = () => (prefs.theme === 'system' ? (dark.matches ? 'dark' : 'light') : prefs.theme);

  // ── theme and motion ──────────────────────────────────────────────────

  function applyTheme() {
    root.dataset.theme = themeOf();
    window.sui?.setChrome?.(root.dataset.theme).catch(() => {});
  }
  function applyMotion() {
    root.dataset.motion = motionOf();
    api.pet?.setMotion(root.dataset.motion);
  }

  // ── sidebar and preview ───────────────────────────────────────────────

  // Sidebar and preview together need about 1000px beside a readable chat;
  // below that, opening one makes room by putting the other away.
  const CRAMPED = 1000;
  let autoCollapsed = false;
  function applySidebar(open) {
    root.dataset.sidebar = open ? 'open' : 'closed';
    $('toggle-sidebar').setAttribute('aria-expanded', String(open));
    $('sidebar').inert = !open;
    api.pet?.refresh();
    notify();
  }
  function setSidebar(open) {
    autoCollapsed = false;
    if (open && previewOpen() && innerWidth < CRAMPED) setPreview(false);
    prefs.sidebar = open;
    save();
    applySidebar(open);
  }
  const toggleSidebar = () => setSidebar(root.dataset.sidebar !== 'open');

  function setPreview(open, { instant = false } = {}) {
    if (open && root.dataset.sidebar === 'open' && innerWidth < CRAMPED) { autoCollapsed = true; applySidebar(false); }
    const body = document.querySelector('.body');
    if (instant) body.classList.add('snap');
    root.dataset.preview = open ? 'open' : 'closed';
    $('toggle-preview').setAttribute('aria-pressed', String(open));
    $('toggle-preview').classList.toggle('on', open);
    prefs.preview = open;
    save();
    if (instant) { body.getBoundingClientRect(); requestAnimationFrame(() => body.classList.remove('snap')); }
    api.pet?.refresh();
    notify();
  }
  const previewOpen = () => root.dataset.preview === 'open';
  const togglePreview = () => setPreview(!previewOpen());

  // ── dialogs ───────────────────────────────────────────────────────────

  function openDialog(dialog) {
    if (!dialog.open) dialog.showModal();
    notify();
  }
  const overlayActive = () => document.querySelector('dialog[open]') !== null;

  // ── appearance panel ──────────────────────────────────────────────────

  let previewSlime;
  function buildAppearance() {
    const colors = $('pet-colors');
    for (const [id, c] of Object.entries(SuiPet.COLORS)) {
      const label = document.createElement('label');
      label.className = 'swatch';
      label.title = c.label;
      label.style.setProperty('--sw-light', c.light);
      label.style.setProperty('--sw-body', c.body);
      label.style.setProperty('--sw-deep', c.deep);
      const input = document.createElement('input');
      input.type = 'radio'; input.name = 'pet-color'; input.value = id;
      input.setAttribute('aria-label', c.label);
      label.append(input, document.createElement('span'));
      colors.append(label);
    }
    const accs = $('pet-accessories');
    for (const [id, name] of Object.entries(SuiPet.ACCESSORIES)) {
      const label = document.createElement('label');
      const input = document.createElement('input');
      input.type = 'radio'; input.name = 'pet-acc'; input.value = id;
      const span = document.createElement('span');
      span.textContent = name;
      label.append(input, span);
      accs.append(label);
    }
    previewSlime = SuiPet.createSlime({ demo: true });
    $('pet-preview-stage').append(previewSlime.el);
    $('pet-preview-stage').addEventListener('pointermove', e => {
      const r = previewSlime.el.getBoundingClientRect();
      previewSlime.setLook(Math.max(-1, Math.min(1, (e.clientX - (r.left + r.width / 2)) / 160)), Math.max(-1, Math.min(1, (e.clientY - (r.top + r.height / 2)) / 120)));
    });
  }

  function syncAppearance() {
    const check = (name, value) => { const el = document.querySelector(`input[name="${name}"][value="${value}"]`); if (el) el.checked = true; };
    check('theme', prefs.theme);
    check('motion', motionOf());
    check('pet-color', prefs.pet.color);
    check('pet-acc', prefs.pet.accessory);
    check('pet-size', prefs.pet.size);
    $('pet-on').checked = prefs.pet.on;
    $('pet-follow').checked = prefs.pet.follow;
    $('pet-name').value = prefs.pet.name;
    previewSlime.setAppearance(prefs.pet);
    document.querySelectorAll('.pet-moods .chip').forEach(chip => chip.classList.toggle('on', chip.dataset.mood === previewSlime.el.dataset.mood));
    previewSlime.setLook(0, 0.1);
  }

  function setPet(patch) {
    prefs.pet = SuiPet.sanitize({ ...prefs.pet, ...patch });
    root.dataset.pet = prefs.pet.on ? 'on' : 'off';
    save();
    api.pet?.configure(prefs.pet);
    previewSlime.setAppearance(prefs.pet);
    notify();
  }

  function wireAppearance() {
    buildAppearance();
    const open = () => { syncAppearance(); openDialog($('appearance-dialog')); };
    for (const id of ['open-appearance', 'sb-appearance']) $(id).addEventListener('click', open);
    for (const id of ['close-appearance', 'done-appearance']) $(id).addEventListener('click', () => $('appearance-dialog').close());
    const onRadio = (name, fn) => document.querySelectorAll(`input[name="${name}"]`).forEach(el => el.addEventListener('change', () => el.checked && fn(el.value)));
    onRadio('theme', v => { prefs.theme = v; save(); applyTheme(); });
    onRadio('motion', v => { prefs.motion = v; save(); applyMotion(); });
    onRadio('pet-color', v => setPet({ color: v }));
    onRadio('pet-acc', v => setPet({ accessory: v }));
    onRadio('pet-size', v => setPet({ size: v }));
    $('pet-on').addEventListener('change', e => setPet({ on: e.target.checked }));
    $('pet-follow').addEventListener('change', e => setPet({ follow: e.target.checked }));
    $('pet-name').addEventListener('input', e => setPet({ name: e.target.value }));
    $('pet-name').addEventListener('blur', () => { $('pet-name').value = prefs.pet.name; });
    $('pet-reset').addEventListener('click', () => { setPet(SuiPet.DEFAULTS); syncAppearance(); });
    document.querySelectorAll('.pet-moods .chip').forEach(chip => chip.addEventListener('click', () => {
      previewSlime.setMood(chip.dataset.mood);
      document.querySelectorAll('.pet-moods .chip').forEach(c => c.classList.toggle('on', c === chip));
    }));
    window.addEventListener('keydown', e => {
      if ((isMac ? e.metaKey : e.ctrlKey) && e.key === ',' && !e.shiftKey && !e.altKey && !overlayActive()) { e.preventDefault(); open(); }
    });
  }

  // ── boot ──────────────────────────────────────────────────────────────

  function wireUpdates() {
    if (!isMac) return;
    const button = $('update-button');
    const label = button.querySelector('span');
    button.classList.remove('hidden');
    let available = false;
    let busy = false;
    const check = async () => {
      busy = true; button.disabled = true; label.textContent = 'Checking for updates…';
      try {
        const result = await window.sui.checkUpdate();
        available = result.available;
        label.textContent = available ? `Download v${result.version}` : 'Up to date · Check again';
        button.title = available ? 'Download and open the new DMG' : 'Check for updates';
      } catch (error) {
        available = false;
        label.textContent = 'Update check failed · Retry';
        button.title = error.message;
      } finally { busy = false; button.disabled = false; }
    };
    button.addEventListener('click', async () => {
      if (busy) return;
      if (!available) return check();
      busy = true; button.disabled = true; label.textContent = 'Downloading update…';
      try {
        await window.sui.downloadUpdate();
        label.textContent = 'DMG opened · Drag to Applications';
        button.title = 'After copying, reopen SUI Desktop from Applications.';
      } catch (error) {
        label.textContent = 'Download failed · Retry';
        button.title = error.message;
      } finally { busy = false; button.disabled = false; }
    });
    check();
  }

  function autosize(el, max = 220) {
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, max) + 'px';
  }

  function init() {
    for (const el of document.querySelectorAll('[data-tip]')) el.title = el.dataset.tip + (el.dataset.key ? ` (${fmtKey(el.dataset.key)})` : '');
    for (const el of document.querySelectorAll('[data-kbd]')) el.textContent = fmtKey(el.dataset.kbd);
    $('local-label').textContent = localLabel();
    $('connect-lead').textContent = `Work on ${localLabel().replace(/^This/, 'this')} or connect to a machine over SSH. The SUI agent runs where your code lives.`;

    applyTheme();
    root.dataset.pet = prefs.pet.on ? 'on' : 'off';
    root.dataset.motion = motionOf();
    root.dataset.sidebar = prefs.sidebar ? 'open' : 'closed';
    root.dataset.preview = prefs.preview ? 'open' : 'closed';
    $('sidebar').inert = !prefs.sidebar;
    $('toggle-sidebar').setAttribute('aria-expanded', String(prefs.sidebar));
    $('toggle-preview').setAttribute('aria-pressed', String(prefs.preview));
    $('toggle-preview').classList.toggle('on', prefs.preview);

    api.pet = new SuiPet.Companion({ layer: $('pet-layer'), lane: $('pet-lane'), hero: $('hero-slot'), stage: $('stage'), prefs: prefs.pet, motion: motionOf() });

    dark.addEventListener('change', () => { if (prefs.theme === 'system') applyTheme(); });
    reduced.addEventListener('change', () => { if (!prefs.motion) applyMotion(); });
    const narrow = matchMedia(`(max-width: ${CRAMPED}px)`);
    const fit = () => {
      if (narrow.matches && root.dataset.sidebar === 'open') { autoCollapsed = true; applySidebar(false); }
      else if (!narrow.matches && autoCollapsed) { autoCollapsed = false; applySidebar(true); }
    };
    narrow.addEventListener('change', fit);
    fit();

    for (const id of ['toggle-sidebar']) $(id).addEventListener('click', toggleSidebar);
    $('toggle-preview').addEventListener('click', togglePreview);
    document.querySelector('.body').addEventListener('transitionend', e => { if (e.propertyName === 'grid-template-columns') notify(); });
    document.querySelectorAll('dialog').forEach(d => d.addEventListener('close', notify));
    window.addEventListener('keydown', e => {
      if (!(isMac ? e.metaKey : e.ctrlKey) || e.shiftKey || e.altKey || overlayActive()) return;
      const k = e.key.toLowerCase();
      if (k === 'b') toggleSidebar();
      else if (k === 'j') togglePreview();
      else if (k === 'n') $('new-chat').click();
      else if (k === 'k') { setSidebar(true); $('project-filter').focus(); $('project-filter').select(); }
      else return;
      e.preventDefault();
    });
    wireAppearance();
    wireUpdates();
  }

  const api = {
    platform, localLabel, prefs: () => prefs, pet: null,
    setSidebar, toggleSidebar, setPreview, previewOpen, togglePreview, openDialog, overlayActive, autosize,
    onLayout: fn => listeners.add(fn)
  };
  window.SuiUI = api;
  document.addEventListener('DOMContentLoaded', init);
})();
