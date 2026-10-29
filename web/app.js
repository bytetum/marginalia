// Marginalia web page. Plain JS, no dependencies. All data comes from the REST API with a bearer token kept in
// localStorage; the only HTML ever inserted with innerHTML is what the server sanitised (/articles/{id}/html).
(() => {
  'use strict';

  const TOKEN_KEY = 'marginalia.token';
  const PAGE = 50;
  const BLOCKS = 'p,li,h1,h2,h3,h4,h5,h6,blockquote,pre,td,th,div,tr';

  const $app = document.getElementById('app');
  const $nav = document.getElementById('nav');
  let nav = 0; // incremented per navigation, so a slow response for an old page is dropped
  let reader = null; // {id, body} while the reader view is showing

  class AuthError extends Error {}

  // ---------- helpers ----------

  function h(tag, props, ...kids) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(props || {})) {
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? '' : v);
    }
    for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid);
    return el;
  }

  const getToken = () => localStorage.getItem(TOKEN_KEY);

  async function api(path, { json, method } = {}) {
    const token = getToken();
    if (!token) throw new AuthError();
    const init = { method: method || (json === undefined ? 'GET' : 'POST'), headers: { Authorization: 'Bearer ' + token } };
    if (json !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(json);
    }
    const res = await fetch(path, init);
    if (res.status === 401) {
      localStorage.removeItem(TOKEN_KEY);
      throw new AuthError();
    }
    if (!res.ok) {
      let msg = res.status + ' ' + res.statusText;
      try {
        const body = await res.json();
        if (body && body.error) msg = body.error;
      } catch (_) { /* keep the status line */ }
      throw new Error(msg);
    }
    return (res.headers.get('content-type') || '').includes('json') ? res.json() : res.text();
  }

  function host(url) {
    try { return new URL(url).hostname.replace(/^www\./, ''); } catch (_) { return ''; }
  }

  function webUrl(url) {
    try {
      const u = new URL(url);
      return u.protocol === 'http:' || u.protocol === 'https:' ? u.href : null;
    } catch (_) { return null; }
  }

  const fmtDate = (iso) => new Date(iso).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });

  function show(...nodes) {
    $app.replaceChildren(...nodes);
  }

  function errorNode(e) {
    return h('p', { class: 'error' }, e.message || String(e));
  }

  // ---------- login ----------

  function showLogin(message) {
    $nav.hidden = true;
    const input = h('input', { type: 'password', placeholder: 'Access token', autocomplete: 'current-password', 'aria-label': 'Access token', required: true });
    const status = h('p', { class: 'status error' }, message || '');
    const form = h('form', { class: 'row', onsubmit: async (ev) => {
      ev.preventDefault();
      status.textContent = '';
      localStorage.setItem(TOKEN_KEY, input.value.trim());
      try {
        await api('/articles?limit=1');
        route();
      } catch (e) {
        status.textContent = e instanceof AuthError ? 'That token was not accepted.' : e.message;
      }
    } }, input, h('button', { type: 'submit' }, 'Sign in'));
    show(h('h1', null, 'Marginalia'), h('p', { class: 'muted' }, 'Enter the access token. It is kept in this browser only.'), form, status);
    input.focus();
  }

  // ---------- library ----------

  function articleItem(a) {
    const bits = [host(a.url), a.words + ' words', a.highlight_count + (a.highlight_count === 1 ? ' highlight' : ' highlights'), fmtDate(a.saved_at)];
    return h('li', null, h('a', { class: 'item', href: '#/a/' + encodeURIComponent(a.id) },
      h('span', { class: 'title' }, a.title), h('span', { class: 'meta' }, bits.filter(Boolean).join(' · '))));
  }

  function hitItem(hit) {
    return h('li', null, h('a', { class: 'item', href: '#/a/' + encodeURIComponent(hit.article_id) },
      h('span', { class: 'title' }, hit.title), h('span', { class: 'snippet' }, hit.snippet)));
  }

  async function libraryView(my) {
    const saveInput = h('input', { type: 'text', inputmode: 'url', autocapitalize: 'none', autocorrect: 'off', spellcheck: 'false', placeholder: 'Paste a URL to save', 'aria-label': 'URL to save' });
    const saveBtn = h('button', { type: 'submit' }, 'Save');
    const saveStatus = h('p', { class: 'status muted', 'aria-live': 'polite' });
    const saveForm = h('form', { class: 'row', onsubmit: async (ev) => {
      ev.preventDefault();
      const url = saveInput.value.trim();
      if (!url) return;
      saveBtn.disabled = true;
      saveStatus.className = 'status muted';
      saveStatus.textContent = 'Saving... fetching and cleaning the page can take a few seconds.';
      try {
        const article = await api('/articles', { json: { url } });
        location.hash = '#/a/' + encodeURIComponent(article.id);
      } catch (e) {
        if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
        saveStatus.className = 'status error';
        saveStatus.textContent = 'Could not save: ' + e.message;
        saveBtn.disabled = false;
      }
    } }, saveInput, saveBtn);

    const searchInput = h('input', { type: 'search', placeholder: 'Search the library', 'aria-label': 'Search', enterkeyhint: 'search' });
    const results = h('div');
    let timer = 0;
    let searchSeq = 0;
    const runSearch = async () => {
      const q = searchInput.value.trim();
      const seq = ++searchSeq;
      if (!q) { listBox.hidden = false; results.replaceChildren(); return; }
      try {
        const hits = await api('/search?limit=20&q=' + encodeURIComponent(q));
        if (seq !== searchSeq || my !== nav) return;
        listBox.hidden = true;
        results.replaceChildren(hits.length ? h('ul', { class: 'list' }, hits.map(hitItem)) : h('p', { class: 'muted' }, 'No matches.'));
      } catch (e) {
        if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
        results.replaceChildren(errorNode(e));
      }
    };
    const searchForm = h('form', { class: 'row', role: 'search', onsubmit: (ev) => { ev.preventDefault(); clearTimeout(timer); runSearch(); } }, searchInput);
    searchInput.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(runSearch, 300); });

    const listBox = h('div', null, h('p', { class: 'muted' }, 'Loading...'));
    show(h('h1', null, 'Library'), saveForm, saveStatus, searchForm, results, listBox);

    const list = h('ul', { class: 'list' });
    const moreBtn = h('button', { type: 'button', class: 'secondary' }, 'Show more');
    let offset = 0;
    const loadPage = async () => {
      moreBtn.disabled = true;
      const rows = await api('/articles?limit=' + PAGE + '&offset=' + offset);
      if (my !== nav) return;
      offset += rows.length;
      list.append(...rows.map(articleItem));
      moreBtn.hidden = rows.length < PAGE;
      moreBtn.disabled = false;
      if (offset === 0) listBox.replaceChildren(h('p', { class: 'muted' }, 'Nothing saved yet. Paste a URL above.'));
      else listBox.replaceChildren(list, moreBtn);
    };
    moreBtn.addEventListener('click', () => loadPage().catch((e) => results.replaceChildren(errorNode(e))));
    await loadPage();
  }

  // ---------- diagrams ----------

  // ```mermaid fences come out of the server as <pre><code class="language-mermaid">. Mermaid (vendored, ~3 MB) is
  // loaded only when a page has such a block, once per page load.
  const DIAGRAM = 'pre > code.language-mermaid';
  const NOT_TEXT = '.mermaid-diagram, .diagram-error'; // never searched for highlights
  let mermaidLoad = null;
  let diagramId = 0;

  function loadMermaid() {
    if (!mermaidLoad) {
      mermaidLoad = new Promise((resolve, reject) => {
        if (window.mermaid) return resolve(window.mermaid);
        const script = document.createElement('script');
        script.src = '/vendor/mermaid.min.js';
        script.onload = () => (window.mermaid ? resolve(window.mermaid) : reject(new Error('mermaid did not start')));
        script.onerror = () => reject(new Error('mermaid could not be loaded'));
        document.head.append(script);
      });
      mermaidLoad.catch(() => {
        mermaidLoad = null; // the next page may try again
        for (const s of document.querySelectorAll('script[src="/vendor/mermaid.min.js"]')) s.remove();
      });
    }
    return mermaidLoad;
  }

  function diagramError(pre, e) {
    const first = String((e && e.message) || e || 'unknown error').split('\n').find((l) => l.trim()) || 'unknown error';
    pre.after(h('p', { class: 'meta diagram-error' }, 'Diagram error: ' + first.trim().replace(/:$/, '').slice(0, 200)));
  }

  // Replaces each mermaid code block under `root` with its SVG. A block that does not parse, or a script that does not
  // load, leaves the code block in place with a short note below it. Never throws.
  async function renderDiagrams(root) {
    const blocks = [...root.querySelectorAll(DIAGRAM)].map((code) => code.parentElement);
    if (!blocks.length) return;
    let mermaid;
    try {
      mermaid = await loadMermaid();
      const dark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
      mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: dark ? 'dark' : 'default', suppressErrorRendering: true });
    } catch (e) {
      for (const pre of blocks) diagramError(pre, e);
      return;
    }
    for (const pre of blocks) {
      const id = 'mermaid-' + ++diagramId;
      try {
        const { svg } = await mermaid.render(id, pre.textContent);
        const box = h('div', { class: 'mermaid-diagram' });
        box.innerHTML = svg; // produced by Mermaid in strict mode from this page's own text
        pre.replaceWith(box);
      } catch (e) {
        // A failed render can leave its scratch element behind in <body>.
        for (const stray of document.querySelectorAll('#d' + id + ', #' + id)) if (!root.contains(stray)) stray.remove();
        diagramError(pre, e);
      }
    }
  }

  // ---------- reader ----------

  // Text of `root` with whitespace collapsed, plus for each character where it came from. A virtual space is
  // inserted between text nodes of different blocks, because a selection spanning paragraphs has a newline there.
  function indexText(root) {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
      acceptNode: (n) => (n.parentElement && n.parentElement.closest(NOT_TEXT) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
    });
    const chars = [];
    const map = [];
    let lastSpace = true;
    let prevBlock = null;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const block = node.parentElement ? node.parentElement.closest(BLOCKS) : null;
      if (prevBlock !== null && block !== prevBlock && !lastSpace) {
        chars.push(' ');
        map.push(null);
        lastSpace = true;
      }
      prevBlock = block;
      const s = node.nodeValue;
      for (let i = 0; i < s.length; i++) {
        if (/\s/.test(s[i])) {
          if (lastSpace) continue;
          chars.push(' ');
          lastSpace = true;
        } else {
          chars.push(s[i]);
          lastSpace = false;
        }
        map.push({ node, i });
      }
    }
    return { text: chars.join(''), map };
  }

  // Wraps the first occurrence of `text` in <mark> elements (one per text node it crosses). Returns the first mark or null.
  function markText(root, text, note) {
    const needle = text.replace(/\s+/g, ' ').trim();
    if (!needle) return null;
    const { text: haystack, map } = indexText(root);
    const at = haystack.indexOf(needle);
    if (at < 0) return null;
    const spans = new Map(); // text node -> [first, last] offsets
    for (let k = at; k < at + needle.length; k++) {
      const m = map[k];
      if (!m) continue;
      const span = spans.get(m.node);
      if (span) span[1] = m.i;
      else spans.set(m.node, [m.i, m.i]);
    }
    let first = null;
    for (const [node, [lo, hi]] of spans) {
      const range = document.createRange();
      range.setStart(node, lo);
      range.setEnd(node, hi + 1);
      const mark = document.createElement('mark');
      if (note) mark.title = note;
      range.surroundContents(mark);
      first = first || mark;
    }
    return first;
  }

  // Extracted pages often open with their own title as a heading; the page header already shows it.
  function dropRepeatedTitle(body, title) {
    const first = body.firstElementChild;
    const norm = (t) => t.replace(/\s+/g, ' ').trim().toLowerCase();
    if (first && /^H[12]$/.test(first.tagName) && norm(first.textContent) === norm(title)) first.remove();
  }

  async function readerView(id, my) {
    show(h('p', { class: 'muted' }, 'Loading...'));
    const q = encodeURIComponent(id);
    const [article, html, highlights] = await Promise.all([
      api('/articles/' + q), api('/articles/' + q + '/html'), api('/articles/' + q + '/highlights'),
    ]);
    if (my !== nav) return;

    const body = h('div', { class: 'article' });
    body.innerHTML = html; // sanitised on the server (src/render.rs)
    dropRepeatedTitle(body, article.title);
    await renderDiagrams(body); // before marking, so highlights see the final text
    if (my !== nav) return;
    const link = webUrl(article.url);
    const meta = h('p', { class: 'meta' },
      link ? h('a', { href: link, target: '_blank', rel: 'noopener noreferrer' }, host(link) || 'Original') : article.url,
      ' · ' + article.words + ' words · saved ' + fmtDate(article.saved_at));

    const items = highlights.map((hl) => {
      const mark = markText(body, hl.text, hl.note);
      return h('li', null,
        h('div', { class: 'quote' }, hl.text),
        hl.note ? h('div', { class: 'note' }, hl.note) : null,
        mark
          ? h('button', { type: 'button', class: 'link', onclick: () => mark.scrollIntoView({ block: 'center', behavior: 'smooth' }) }, 'Show in text')
          : h('div', { class: 'note' }, '(not found in the text above)'));
    });

    show(
      h('p', null, h('a', { href: '#/' }, '← Library')),
      h('h1', null, article.title), meta, body,
      h('h2', null, 'Highlights (' + highlights.length + ')'),
      highlights.length ? h('ul', { class: 'highlights' }, items) : h('p', { class: 'muted' }, 'Select text above to highlight it.'));
    reader = { id, body };
  }

  // ---------- highlight popover ----------

  const pop = h('div', { id: 'hl-pop', hidden: true });
  document.body.append(pop);
  let pending = ''; // the selected text, kept because tapping the button may clear the selection
  let popBusy = false; // true while the note form is open; selection changes are ignored
  let selTimer = 0;

  function hidePop() {
    pop.hidden = true;
    pop.replaceChildren();
    pending = '';
    popBusy = false;
  }

  function placePop(rect) {
    pop.hidden = false;
    const w = pop.offsetWidth;
    const hgt = pop.offsetHeight;
    const below = rect.bottom + 10;
    // Native selection handles and menus sit around the selection; prefer just below it, else above.
    const top = below + hgt + 8 < window.innerHeight ? below : Math.max(8, rect.top - hgt - 10);
    const left = Math.min(Math.max(8, rect.left), Math.max(8, window.innerWidth - w - 8));
    pop.style.top = top + 'px';
    pop.style.left = left + 'px';
  }

  function showPopButton(rect) {
    const btn = h('button', { type: 'button', onclick: () => showNoteForm(rect) }, 'Highlight');
    pop.replaceChildren(btn);
    // Keep the selection alive while the button is pressed.
    pop.onmousedown = (ev) => ev.preventDefault();
    placePop(rect);
  }

  function showNoteForm(rect) {
    popBusy = true; // typing a note must not be interrupted by selection changes
    pop.onmousedown = null;
    const note = h('input', { type: 'text', placeholder: 'Note (optional)', 'aria-label': 'Note', enterkeyhint: 'done' });
    const err = h('span', { class: 'error' });
    const save = h('button', { type: 'submit' }, 'Save');
    const form = h('form', { class: 'row', onsubmit: async (ev) => {
      ev.preventDefault();
      if (!reader) return;
      save.disabled = true;
      err.textContent = '';
      try {
        await api('/articles/' + encodeURIComponent(reader.id) + '/highlights', { json: { text: pending, note: note.value.trim() || null } });
        const y = window.scrollY;
        hidePop();
        window.getSelection().removeAllRanges();
        await route();
        window.scrollTo(0, y);
      } catch (e) {
        if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
        err.textContent = e.message;
        save.disabled = false;
      }
    } }, note, save, h('button', { type: 'button', class: 'secondary', onclick: hidePop }, 'Cancel'));
    pop.replaceChildren(form, err);
    placePop(rect);
    note.focus();
  }

  document.addEventListener('selectionchange', () => {
    clearTimeout(selTimer);
    selTimer = setTimeout(() => {
      if (popBusy) return;
      const sel = window.getSelection();
      if (!reader || !sel || sel.rangeCount === 0 || sel.isCollapsed) return hidePop();
      const range = sel.getRangeAt(0);
      const text = sel.toString().trim();
      if (!text || !reader.body.contains(range.commonAncestorContainer)) return hidePop();
      pending = text.slice(0, 5000);
      showPopButton(range.getBoundingClientRect());
    }, 200);
  });

  // ---------- syntheses ----------

  async function synthesesView(my) {
    show(h('h1', null, 'Syntheses'), h('p', { class: 'muted' }, 'Loading...'));
    const rows = await api('/syntheses?limit=100');
    if (my !== nav) return;
    show(h('h1', null, 'Syntheses'), rows.length
      ? h('ul', { class: 'list' }, rows.map((s) => h('li', null, h('a', { class: 'item', href: '#/s/' + encodeURIComponent(s.id) },
        h('span', { class: 'title' }, s.title), h('span', { class: 'meta' }, s.kind + ' · ' + fmtDate(s.created_at))))))
      : h('p', { class: 'muted' }, 'No analyses or briefs yet.'));
  }

  async function synthesisView(id, my) {
    show(h('p', { class: 'muted' }, 'Loading...'));
    const q = encodeURIComponent(id);
    const [s, html] = await Promise.all([api('/syntheses/' + q), api('/syntheses/' + q + '/html')]);
    if (my !== nav) return;
    const body = h('div', { class: 'article' });
    body.innerHTML = html; // sanitised on the server
    dropRepeatedTitle(body, s.title);
    await renderDiagrams(body);
    if (my !== nav) return;
    show(
      h('p', null, h('a', { href: '#/s' }, '← Syntheses')),
      h('h1', null, s.title),
      h('p', { class: 'meta' }, s.kind + ' · ' + fmtDate(s.created_at) + (s.direction ? ' · direction: ' + s.direction : '')),
      body,
      s.article_ids.length ? h('h2', null, 'Sources') : null,
      s.article_ids.length
        ? h('p', null, s.article_ids.map((aid, i) => [h('a', { href: '#/a/' + encodeURIComponent(aid) }, 'article ' + (i + 1)), i < s.article_ids.length - 1 ? ' · ' : '']))
        : null);
  }

  // ---------- routing ----------

  async function route() {
    const my = ++nav;
    reader = null;
    hidePop();
    if (!getToken()) return showLogin();
    $nav.hidden = false;
    const parts = location.hash.replace(/^#\/?/, '').split('/');
    try {
      if (parts[0] === 'a' && parts[1]) await readerView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's' && parts[1]) await synthesisView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's') await synthesesView(my);
      else await libraryView(my);
    } catch (e) {
      if (my !== nav) return;
      if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
      show(h('p', null, h('a', { href: '#/' }, '← Library')), errorNode(e));
    }
  }

  document.getElementById('logout').addEventListener('click', () => {
    localStorage.removeItem(TOKEN_KEY);
    route();
  });
  window.addEventListener('hashchange', route);
  route();
})();
