// A small, safe markdown renderer for assistant messages: paragraphs,
// headings, lists, quotes, rules, fenced code (with copy), and inline code,
// bold, italics and link labels. Everything is built with createElement and
// textContent — model output never reaches innerHTML — and a half-streamed
// message always renders sensibly (an open fence is just a code block).
(() => {
  'use strict';

  const BLOCK_START = /^(\s{0,3}(`{3,}|~{3,})|\s{0,3}#{1,6}\s|\s{0,3}>|\s{0,3}([-*+]|\d+[.)])\s|\s{0,3}([-*_])( *\4){2,} *$)/;

  function parse(src) {
    const lines = String(src).replace(/\r\n?/g, '\n').split('\n');
    const out = [];
    let i = 0;
    while (i < lines.length) {
      const line = lines[i];
      const fence = /^ {0,3}(`{3,}|~{3,})\s*([\w+#.-]*)[^\n]*$/.exec(line);
      if (fence) {
        const mark = fence[1];
        const close = new RegExp('^ {0,3}' + (mark[0] === '`' ? '`' : '~') + '{' + mark.length + ',}\\s*$');
        const body = [];
        let closed = false;
        for (i++; i < lines.length; i++) {
          if (close.test(lines[i])) { closed = true; i++; break; }
          body.push(lines[i]);
        }
        out.push({ type: 'code', lang: fence[2], text: body.join('\n'), closed });
        continue;
      }
      if (!line.trim()) { i++; continue; }
      const heading = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
      if (heading) { out.push({ type: 'h', level: Math.min(heading[1].length, 3), text: heading[2] }); i++; continue; }
      if (/^\s{0,3}([-*_])( *\1){2,} *$/.test(line)) { out.push({ type: 'hr' }); i++; continue; }
      if (/^\s{0,3}>/.test(line)) {
        const quote = [];
        for (; i < lines.length && /^\s{0,3}>/.test(lines[i]); i++) quote.push(lines[i].replace(/^\s{0,3}>\s?/, ''));
        out.push({ type: 'quote', text: quote.join('\n') });
        continue;
      }
      const item = /^\s{0,3}([-*+]|\d+[.)])\s+(.*)$/.exec(line);
      if (item) {
        const ordered = /\d/.test(item[1]);
        const items = [];
        for (; i < lines.length; i++) {
          const m = /^\s{0,3}([-*+]|\d+[.)])\s+(.*)$/.exec(lines[i]);
          if (m && /\d/.test(m[1]) === ordered) items.push(m[2]);
          else if (items.length && /^\s{2,}\S/.test(lines[i]) && !m) items[items.length - 1] += '\n' + lines[i].trim();
          else break;
        }
        out.push({ type: ordered ? 'ol' : 'ul', items });
        continue;
      }
      const para = [];
      for (; i < lines.length && lines[i].trim() && (!para.length || !BLOCK_START.test(lines[i])); i++) para.push(lines[i]);
      out.push({ type: 'p', text: para.join('\n') });
    }
    return out;
  }

  const isSpace = ch => ch === undefined || /\s/.test(ch);
  const isWord = ch => ch !== undefined && /\w/.test(ch);

  function inline(text) {
    const out = [];
    let buf = '';
    const flush = () => { if (buf) { out.push({ t: 'text', v: buf }); buf = ''; } };
    let i = 0;
    while (i < text.length) {
      const c = text[i];
      if (c === '`') {
        let n = 1;
        while (text[i + n] === '`') n++;
        const ticks = '`'.repeat(n);
        const end = text.indexOf(ticks, i + n);
        if (end > i + n) { flush(); out.push({ t: 'code', v: text.slice(i + n, end).replace(/^ (.+) $/, '$1') }); i = end + n; continue; }
        buf += ticks; i += n; continue;
      }
      if (c === '*' && text[i + 1] === '*' && !isSpace(text[i + 2])) {
        const end = text.indexOf('**', i + 2);
        if (end > i + 2 && !isSpace(text[end - 1])) { flush(); out.push({ t: 'b', v: text.slice(i + 2, end) }); i = end + 2; continue; }
      }
      if (c === '*' && !isSpace(text[i + 1]) && text[i + 1] !== '*' && !isWord(text[i - 1])) {
        const end = text.indexOf('*', i + 1);
        if (end > i + 1 && !isSpace(text[end - 1]) && text[end + 1] !== '*') { flush(); out.push({ t: 'i', v: text.slice(i + 1, end) }); i = end + 1; continue; }
      }
      if (c === '[') {
        const m = /^\[([^\]\n]+)\]\(([^)\s]+)\)/.exec(text.slice(i));
        if (m) { flush(); out.push({ t: 'link', v: m[1], href: m[2] }); i += m[0].length; continue; }
      }
      buf += c; i++;
    }
    flush();
    return out;
  }

  // ── DOM (browser only) ────────────────────────────────────────────────

  function icon(name) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('class', 'i');
    const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
    use.setAttribute('href', '#i-' + name);
    svg.append(use);
    return svg;
  }

  function renderInline(parent, text) {
    for (const t of inline(text)) {
      if (t.t === 'text') parent.append(t.v);
      else if (t.t === 'code') { const el = document.createElement('code'); el.textContent = t.v; parent.append(el); }
      else if (t.t === 'b') { const el = document.createElement('strong'); renderInline(el, t.v); parent.append(el); }
      else if (t.t === 'i') { const el = document.createElement('em'); renderInline(el, t.v); parent.append(el); }
      else { const el = document.createElement('span'); el.className = 'md-link'; el.title = t.href; el.textContent = t.v; parent.append(el); }
    }
  }

  function codeBlock(b) {
    const box = document.createElement('div');
    box.className = 'codeblock';
    const head = document.createElement('div');
    head.className = 'codeblock-head';
    const label = document.createElement('span');
    label.textContent = b.lang || 'code';
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'copy-btn';
    copy.setAttribute('aria-label', 'Copy code');
    const word = document.createElement('span');
    word.textContent = 'Copy';
    copy.append(icon('copy'), word);
    copy.addEventListener('click', async () => {
      try { await navigator.clipboard.writeText(b.text); } catch { return; }
      copy.classList.add('copied');
      word.textContent = 'Copied';
      setTimeout(() => { copy.classList.remove('copied'); word.textContent = 'Copy'; }, 1400);
    });
    head.append(label, copy);
    const pre = document.createElement('pre');
    const code = document.createElement('code');
    code.textContent = b.text;
    pre.append(code);
    box.append(head, pre);
    return box;
  }

  function block(b) {
    if (b.type === 'code') return codeBlock(b);
    if (b.type === 'hr') return document.createElement('hr');
    if (b.type === 'ul' || b.type === 'ol') {
      const list = document.createElement(b.type);
      for (const text of b.items) { const li = document.createElement('li'); renderInline(li, text); list.append(li); }
      return list;
    }
    const el = document.createElement(b.type === 'h' ? 'h' + (2 + b.level) : b.type === 'quote' ? 'blockquote' : 'p');
    renderInline(el, b.text);
    return el;
  }

  function render(container, src) {
    container.classList.add('prose');
    const frag = document.createDocumentFragment();
    for (const b of parse(src)) frag.append(block(b));
    container.replaceChildren(frag);
  }

  const api = { parse, inline, render };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else window.SuiMarkdown = api;
})();
