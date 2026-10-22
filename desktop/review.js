// Mission records and diff text are rendered as text, never executable markup.
(() => {
  const $ = id => document.getElementById(id);
  const node = (tag, text, className = '') => {
    const element = document.createElement(tag); element.textContent = text; element.className = className; return element;
  };
  let sequence = 0, pending = 0, directory = null, timer;
  const reset = () => { pending = ++sequence; clearTimeout(timer); $('mission-review').close(); directory = null; };
  async function request(extra = {}) {
    const id = pending = ++sequence;
    clearTimeout(timer);
    $('review-status').textContent = 'Loading recorded evidence…';
    $('review-content').textContent = '';
    timer = setTimeout(() => { if (pending === id) { pending = ++sequence; $('review-status').textContent = 'Review timed out. Close and reopen the review to try again.'; } }, 15000);
    try { await window.sui.command({ type: 'review_mission', request: id, directory, ...($('review-worktree').value ? { worktree: $('review-worktree').value } : {}), ...extra }); }
    catch (error) { if (pending === id) { clearTimeout(timer); $('review-status').textContent = error.message; } }
  }
  function event(value) {
    if (value.type !== 'mission_review' || value.request !== pending || !$('mission-review').open) return;
    clearTimeout(timer);
    if (value.error) { $('review-status').textContent = value.error; return; }
    const data = value.data;
    if (Object.hasOwn(data, 'diff')) {
      $('review-status').textContent = `${data.file}${data.worktree ? ` · Current worktree: ${data.worktree} · Not an accepted result` : ''}${data.truncated ? ' · Diff truncated at 128 KiB; inspect the branch for the complete diff.' : ''}`;
      $('review-content').textContent = data.diff || 'No text diff (the file may be binary).';
    } else if (data.agent) {
      $('review-status').textContent = `${data.agent}${data.omitted ? ` · ${data.omitted} earlier entries omitted; full record in the run journal.` : ''}`;
      $('review-content').textContent = data.entries.map(row => `${row.kind}\n${row.text}`).join('\n\n');
    } else {
      $('review-worktree').replaceChildren(...(data.worktrees || []).map(name => new Option(name, name)));
      $('review-worktree').value = data.worktree || '';
      $('review-worktree-label').classList.toggle('hidden', !data.worktrees?.length);
      $('review-files').replaceChildren(new Option('Choose a changed file…', ''), ...data.files.map(file => new Option(file, file)));
      $('review-agents').replaceChildren(new Option('Choose an agent…', ''), ...data.agents.map(agent => new Option(agent.replace(/\.jsonl$/, ''), agent)));
      $('review-files').disabled = !data.files.length;
      $('review-agents').disabled = !data.agents.length;
      $('review-status').textContent = `${data.result.outcome} · ${data.files.length} files${data.files_truncated ? ' (file list truncated)' : ''}${data.omitted ? ` · ${data.omitted} earlier evidence records omitted` : ''}`;
      $('review-content').textContent = `${data.worktree ? `Retained worktree: ${data.worktree}\nCurrent files, including uncommitted work. These changes have not been accepted.\n` : ''}Branch: ${data.result.branch || 'none'}\nBase: ${data.base || 'unknown'}\nCandidate: ${data.candidate || 'none'}\n\nRecorded evidence (historical; no commands are rerun):\n\n${data.evidence.map(row => `${row.kind}\n${row.text}`).join('\n\n') || 'No gate or audit records.'}`;
    }
  }
  function live(root) {
    const details = node('details', '', 'worker-review');
    const select = document.createElement('select'); select.setAttribute('aria-label', 'Agent activity');
    const output = node('pre', '', 'review-text');
    details.append(node('summary', 'Inspect agent activity'), select, output); root.append(details);
    const agents = new Map(); let queued = false, omittedAgents = false;
    function render() {
      queued = false;
      const row = agents.get(select.value);
      output.textContent = row ? row.text + (row.omitted ? `\n… ${row.omitted} characters omitted; open the recorded agent journal after the run.` : '') + (omittedAgents ? '\nAdditional agents omitted from this live view.' : '') : 'Waiting for agent activity…';
    }
    select.addEventListener('change', render); details.addEventListener('toggle', render);
    return value => {
      if (!value.agent) return;
      let text;
      if (value.type === 'delta') text = value.text;
      else if (value.type === 'tool_start') text = `\n\n→ ${value.name}: ${value.summary}\n`;
      else if (value.type === 'tool_done') text = `\n${value.name}: ${value.status} · exit ${value.exit ?? 'unknown'}\n${value.result || ''}${value.truncated || value.dropped ? '\n[tool output truncated]' : ''}\n`;
      else if (value.type === 'phase' || value.type === 'error') text = `\n${value.text || value.message}\n`;
      else return;
      if (!agents.has(value.agent)) {
        if (agents.size >= 16) { omittedAgents = true; return; }
        agents.set(value.agent, { text: '', omitted: 0 }); select.add(new Option(value.agent, value.agent));
      }
      const row = agents.get(value.agent);
      const remaining = Math.max(0, 65536 - row.text.length);
      row.text += String(text).slice(0, remaining); row.omitted += Math.max(0, String(text).length - remaining);
      if (details.open && !queued) { queued = true; requestAnimationFrame(render); }
    };
  }
  window.addEventListener('DOMContentLoaded', () => {
    $('close-mission-review').addEventListener('click', reset);
    $('mission-review').addEventListener('cancel', reset);
    $('review-files').addEventListener('change', () => { if ($('review-files').value) request({ file: $('review-files').value }); });
    $('review-agents').addEventListener('change', () => { if ($('review-agents').value) request({ agent: $('review-agents').value }); });
    $('review-evidence').addEventListener('click', () => request());
    $('review-worktree').addEventListener('change', () => request());
  });
  window.SuiReview = {
    reset, event, live,
    open(path) {
      directory = path;
      $('review-files').replaceChildren(); $('review-agents').replaceChildren(); $('review-worktree').replaceChildren();
      window.SuiUI.openDialog($('mission-review')); request();
    }
  };
})();
