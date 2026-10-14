// Runs synchronously before first paint: resolves the saved appearance into
// attributes on <html> so the window never flashes the wrong theme or layout.
// The full preference model (validation, persistence, live updates) is ui.js.
(() => {
  const root = document.documentElement;
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem('sui.desktop.ui.v1') || '{}') || {}; } catch { /* corrupt prefs fall back to defaults */ }
  const dark = matchMedia('(prefers-color-scheme: dark)').matches;
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  root.dataset.theme = saved.theme === 'light' || saved.theme === 'dark' ? saved.theme : dark ? 'dark' : 'light';
  root.dataset.motion = ['full', 'calm', 'off'].includes(saved.motion) ? saved.motion : reduced ? 'calm' : 'full';
  root.dataset.platform = ['darwin', 'win32', 'linux'].includes(window.sui?.platform) ? window.sui.platform : 'linux';
  root.dataset.sidebar = saved.sidebar === false ? 'closed' : 'open';
  root.dataset.preview = saved.preview === true ? 'open' : 'closed';
  root.dataset.pet = saved.pet?.on === false ? 'off' : 'on';
})();
