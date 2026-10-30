// Marginalia web page. Plain JS, no dependencies. All data comes from the REST API with a bearer token; the only HTML
// ever inserted with innerHTML is what the server sanitised (/articles/{id}/html).
//
// The same files run in two places:
//   - the browser page served by the server itself: API calls are same-origin (API_BASE is ''), and the token lives in
//     localStorage;
//   - the Tauri app (app/): the page comes from the app bundle, API_BASE is the server URL the user entered (localStorage),
//     and the token lives in the OS keychain, reached through the app's own get_token/set_token/clear_token commands.
(() => {
  'use strict';

  const TOKEN_KEY = 'marginalia.token';
  const SERVER_KEY = 'marginalia.server';
  const DEFAULT_SERVER = 'https://marginalia-bytetum.taild98079.ts.net';
  const PAGE = 50;
  const BLOCKS = 'p,li,h1,h2,h3,h4,h5,h6,blockquote,pre,td,th,div,tr';

  const $app = document.getElementById('app');
  const $nav = document.getElementById('nav');
  let nav = 0; // incremented per navigation, so a slow response for an old page is dropped
  let reader = null; // {id, body} while the reader view is showing

  class AuthError extends Error {}

  // ---------- environment ----------

  const IN_APP = !!(window.__TAURI__ || window.__TAURI_INTERNALS__);
  const invoke = (cmd, args) => (window.__TAURI__ && window.__TAURI__.core ? window.__TAURI__.core.invoke : window.__TAURI_INTERNALS__.invoke)(cmd, args);
  let API_BASE = ''; // '' = same origin (the browser page); the server URL, with no trailing slash, in the app
  let appToken = null; // the app's copy of the keychain token
  let appInfo = { version: '', tokenStorage: '', share: false };

  const getToken = () => (IN_APP ? appToken : localStorage.getItem(TOKEN_KEY));

  async function setToken(token) {
    if (IN_APP) {
      appInfo.tokenStorage = await invoke('set_token', { token });
      appToken = token;
    } else {
      localStorage.setItem(TOKEN_KEY, token);
    }
  }

  async function clearToken() {
    appToken = null;
    if (IN_APP) await invoke('clear_token').catch(() => {});
    else localStorage.removeItem(TOKEN_KEY);
  }

  const isConfigured = () => !!getToken() && (!IN_APP || !!API_BASE);

  async function loadConfig() {
    if (!IN_APP) return;
    API_BASE = localStorage.getItem(SERVER_KEY) || '';
    try {
      appInfo = await invoke('app_info');
      appToken = await invoke('get_token');
    } catch (e) {
      console.error('app startup failed', e);
    }
  }

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

  // The one place that talks to the server: adds the base URL and the bearer token. A network failure becomes a
  // readable Error (browsers say "Failed to fetch" or "Load failed").
  async function request(base, token, path, { json, method, timeout } = {}) {
    const init = { method: method || (json === undefined ? 'GET' : 'POST'), headers: {} };
    if (token) init.headers.Authorization = 'Bearer ' + token;
    if (json !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(json);
    }
    if (timeout && typeof AbortSignal.timeout === 'function') init.signal = AbortSignal.timeout(timeout);
    try {
      return await fetch(base + path, init);
    } catch (e) {
      throw new Error(e && e.name === 'TimeoutError' ? 'The server did not answer in time.'
        : 'Cannot reach the server' + (IN_APP ? '. Is this device on Tailscale?' : '.'));
    }
  }

  async function errorMessage(res) {
    let msg = res.status + ' ' + res.statusText;
    try {
      const body = await res.json();
      if (body && body.error) msg = body.error;
    } catch (_) { /* keep the status line */ }
    return msg;
  }

  async function api(path, { json, method } = {}) {
    const token = getToken();
    if (!token) throw new AuthError();
    const res = await request(API_BASE, token, path, { json, method });
    if (res.status === 401) {
      await clearToken();
      throw new AuthError();
    }
    if (!res.ok) throw new Error(await errorMessage(res));
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

  // ---------- login (browser) and setup (app) ----------

  // Both the "token rejected" paths and the first visit end up here; the app asks for the server URL as well.
  function showLogin(message) {
    if (IN_APP) return showSetup({ message });
    $nav.hidden = true;
    const input = h('input', { type: 'password', placeholder: 'Access token', autocomplete: 'current-password', 'aria-label': 'Access token', required: true });
    const status = h('p', { class: 'status error' }, message || '');
    const form = h('form', { class: 'row', onsubmit: async (ev) => {
      ev.preventDefault();
      status.textContent = '';
      await setToken(input.value.trim());
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

  // Server URL as typed by the user -> an origin-style base ('' when empty or not an http(s) URL).
  function normalizeServer(text) {
    let t = text.trim();
    if (!t) return '';
    if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(t)) t = 'https://' + t;
    try {
      const u = new URL(t);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return '';
      return (u.origin + u.pathname).replace(/\/+$/, '');
    } catch (_) { return ''; }
  }

  // /health (no token needed), then an authenticated call. Resolves with a message, rejects with a readable Error.
  async function testConnection(base, token) {
    const health = await request(base, null, '/health', { timeout: 10000 });
    if (!health.ok) throw new Error('The server answered ' + health.status + ' on /health. Is this the Marginalia URL?');
    const body = await health.json().catch(() => null);
    if (!body || body.ok !== true) throw new Error('That does not look like a Marginalia server.');
    const res = await request(base, token, '/articles?limit=1', { timeout: 10000 });
    if (res.status === 401) throw new Error('The server is reachable, but it rejected the token.');
    if (!res.ok) throw new Error(await errorMessage(res));
    return 'Connected. The token is accepted' + (body.llm ? '.' : ' (the server has no LLM configured, so chat, analyses and briefs are off).');
  }

  // First-run screen and settings in one: server URL + token, test, save, and sign out.
  function showSetup({ message, settings } = {}) {
    $nav.hidden = !settings;
    const hasToken = !!getToken();
    const server = h('input', { type: 'url', inputmode: 'url', autocapitalize: 'none', autocorrect: 'off', spellcheck: 'false', autocomplete: 'off',
      placeholder: DEFAULT_SERVER, value: API_BASE || DEFAULT_SERVER, 'aria-label': 'Server URL', required: true });
    const token = h('input', { type: 'password', autocomplete: 'off', autocapitalize: 'none',
      placeholder: settings && hasToken ? 'Leave empty to keep the current token' : 'Access token', 'aria-label': 'Access token' });
    const status = h('p', { class: 'status' + (message ? ' error' : ' muted'), 'aria-live': 'polite' }, message || '');
    const testBtn = h('button', { type: 'button', class: 'secondary' }, 'Test connection');
    const saveBtn = h('button', { type: 'submit' }, settings ? 'Save' : 'Connect');

    const values = () => {
      const base = normalizeServer(server.value);
      const tok = token.value.trim() || (settings && hasToken ? getToken() : '');
      if (!base) throw new Error('Enter the server URL, for example ' + DEFAULT_SERVER);
      if (!tok) throw new Error('Enter the access token.');
      return { base, tok };
    };
    const run = async (save) => {
      testBtn.disabled = saveBtn.disabled = true;
      status.className = 'status muted';
      status.textContent = 'Connecting...';
      try {
        const { base, tok } = values();
        const ok = await testConnection(base, tok);
        if (!save) {
          status.className = 'status';
          status.textContent = ok;
          return;
        }
        localStorage.setItem(SERVER_KEY, base);
        API_BASE = base;
        await setToken(tok);
        if (location.hash === '#/') await route();
        else location.hash = '#/';
        drainShares();
      } catch (e) {
        status.className = 'status error';
        status.textContent = e.message;
      } finally {
        testBtn.disabled = saveBtn.disabled = false;
      }
    };
    testBtn.addEventListener('click', () => run(false));
    const form = h('form', { class: 'stack', onsubmit: (ev) => { ev.preventDefault(); run(true); } },
      h('label', null, h('span', null, 'Server URL'), server),
      h('label', null, h('span', null, 'Access token'), token),
      h('div', { class: 'row' }, testBtn, saveBtn), status);

    const where = appInfo.tokenStorage ? ' The token is kept in ' + appInfo.tokenStorage + '.' : '';
    show(
      h('h1', null, settings ? 'Settings' : 'Marginalia'),
      h('p', { class: 'muted' }, settings
        ? 'Server and access token for this app.' + where
        : 'Connect to your Marginalia server. This device must be on Tailscale.' + where),
      form,
      settings ? h('div', { class: 'row' }, h('button', { type: 'button', class: 'secondary', onclick: async () => {
        await clearToken();
        route();
      } }, 'Sign out')) : null,
      settings && appInfo.version ? h('p', { class: 'meta' }, 'Marginalia app ' + appInfo.version) : null);
    if (!settings) token.focus({ preventScroll: true });
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

  // ---------- reader ----------

  // Text of `root` with whitespace collapsed, plus for each character where it came from. A virtual space is
  // inserted between text nodes of different blocks, because a selection spanning paragraphs has a newline there.
  function indexText(root) {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
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
  //
  // Selecting text works with a mouse, with a finger (long-press, then drag the native handles) and with the keyboard,
  // so the trigger is `selectionchange`, which every engine fires for all three, rather than mouseup or touchend.
  // It fires continuously while a handle is dragged, hence the debounce. With a mouse the button is held back until the
  // button is released, so it does not appear under the cursor mid-drag.
  //
  // On a touch screen (pointer: coarse) the native selection handles and the system callout (Copy, Share, ...) sit
  // right around the selection, so the control is docked as a bar on the bottom edge of the visual viewport instead of
  // floating next to the text; the note form keeps above the on-screen keyboard the same way.

  const pop = h('div', { id: 'hl-pop', hidden: true });
  document.body.append(pop);
  const touchUi = window.matchMedia('(pointer: coarse)');
  let pending = ''; // the selected text, kept because tapping the button may clear the selection
  let popBusy = false; // true while the note form is open; selection changes are ignored
  let popHoldUntil = 0; // a tap on the popover can clear the selection before its click lands; ignore that for a moment
  let mouseDown = false;
  let selTimer = 0;
  let popRect = null;

  function hidePop() {
    pop.hidden = true;
    pop.replaceChildren();
    pending = '';
    popBusy = false;
    popRect = null;
  }

  function placePop(rect) {
    popRect = rect;
    pop.hidden = false;
    const vv = window.visualViewport;
    if (touchUi.matches) {
      pop.classList.add('dock');
      pop.style.left = '';
      pop.style.top = (vv ? vv.offsetTop + vv.height : window.innerHeight) - pop.offsetHeight + 'px';
      return;
    }
    pop.classList.remove('dock');
    const w = pop.offsetWidth;
    const hgt = pop.offsetHeight;
    const below = rect.bottom + 10;
    const top = below + hgt + 8 < window.innerHeight ? below : Math.max(8, rect.top - hgt - 10);
    const left = Math.min(Math.max(8, rect.left), Math.max(8, window.innerWidth - w - 8));
    pop.style.top = top + 'px';
    pop.style.left = left + 'px';
  }

  if (window.visualViewport) {
    const replace = () => { if (!pop.hidden && popRect) placePop(popRect); };
    window.visualViewport.addEventListener('resize', replace);
    window.visualViewport.addEventListener('scroll', replace);
  }

  function showPopButton(rect) {
    const btn = h('button', { type: 'button', onclick: () => showNoteForm(rect) }, 'Highlight');
    pop.replaceChildren(btn);
    placePop(rect);
  }

  function showNoteForm(rect) {
    popBusy = true; // typing a note must not be interrupted by selection changes
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
    } }, note, save, h('button', { type: 'button', class: 'secondary', onclick: () => { hidePop(); window.getSelection().removeAllRanges(); } }, 'Cancel'));
    pop.replaceChildren(form, err);
    placePop(rect);
    note.focus();
  }

  function onSelectionSettled() {
    if (popBusy || mouseDown || Date.now() < popHoldUntil) return;
    const sel = window.getSelection();
    if (!reader || !sel || sel.rangeCount === 0 || sel.isCollapsed) return hidePop();
    const range = sel.getRangeAt(0);
    const text = sel.toString().trim();
    if (!text || !reader.body.contains(range.commonAncestorContainer)) return hidePop();
    pending = text.slice(0, 5000);
    showPopButton(range.getBoundingClientRect());
  }

  const settleSoon = (ms) => { clearTimeout(selTimer); selTimer = setTimeout(onSelectionSettled, ms); };

  document.addEventListener('selectionchange', () => settleSoon(touchUi.matches ? 300 : 200));
  // A press on the popover can collapse the selection before the click arrives; do not hide the popover for that.
  pop.addEventListener('pointerdown', () => { popHoldUntil = Date.now() + 1000; });
  // Keep the selection alive while a mouse presses the button.
  pop.addEventListener('mousedown', (ev) => { if (ev.target.tagName !== 'INPUT') ev.preventDefault(); });
  document.addEventListener('pointerdown', (ev) => { if (ev.pointerType === 'mouse' && !pop.contains(ev.target)) mouseDown = true; });
  document.addEventListener('pointerup', (ev) => { if (ev.pointerType === 'mouse' && mouseDown) { mouseDown = false; settleSoon(0); } });
  document.addEventListener('pointercancel', () => { mouseDown = false; });
  window.addEventListener('blur', () => { mouseDown = false; });

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

  // ---------- saving shared links (the app's share target) ----------

  const $toast = h('div', { id: 'toast', role: 'status', 'aria-live': 'polite', hidden: true });
  document.body.append($toast);
  let toastTimer = 0;

  function toast(text, { error, action, ms } = {}) {
    clearTimeout(toastTimer);
    $toast.className = error ? 'error' : '';
    $toast.replaceChildren(h('span', null, text), action ? h('a', { href: action.href }, action.label) : null);
    $toast.hidden = false;
    toastTimer = setTimeout(() => { $toast.hidden = true; }, ms || (error ? 12000 : 7000));
  }
  $toast.addEventListener('click', () => { $toast.hidden = true; });

  // The first web link in some shared text ("Title\nhttps://example.com/x" is what most apps send).
  function firstUrl(text) {
    const m = /https?:\/\/[^\s<>"']+/i.exec(text || '');
    if (!m) return null;
    let url = m[0].replace(/[.,;:!?]+$/, '');
    if (url.endsWith(')') && !url.includes('(')) url = url.slice(0, -1);
    return webUrl(url);
  }

  // An intent from the share-target plugin is either an Android intent URI
  // (#Intent;action=android.intent.action.SEND;S.android.intent.extra.TEXT=<encoded text>;end) or, on iOS,
  // the share extension's deep link <scheme>://share?url=<encoded url>.
  function sharedText(raw) {
    const decode = (v) => { try { return decodeURIComponent(v); } catch (_) { return v; } };
    const ios = /^[a-z][a-z0-9+.-]*:\/\/share\?url=(.*)$/is.exec(raw);
    if (ios) return decode(ios[1]);
    const android = /(?:^|;)S\.android\.intent\.extra\.TEXT=([^;]*)/.exec(raw);
    return android ? decode(android[1]) : null;
  }

  let draining = false;
  async function drainShares() {
    if (!appInfo.share || draining || !isConfigured()) return; // not signed in yet: leave them queued
    draining = true;
    try {
      for (let raw = await invoke('plugin:mobile-sharetarget|pop_intent_queue'); raw; raw = await invoke('plugin:mobile-sharetarget|pop_intent_queue')) {
        await saveShared(sharedText(raw));
      }
    } catch (e) {
      console.error('share queue', e);
    } finally {
      draining = false;
    }
  }

  async function saveShared(text) {
    const url = firstUrl(text);
    if (!url) return toast('Nothing to save: the shared text has no web link.', { error: true });
    toast('Saving ' + (host(url) || url) + '... fetching and cleaning the page can take a few seconds.', { ms: 60000 });
    try {
      const article = await api('/articles', { json: { url } });
      toast('Saved: ' + article.title, { action: { href: '#/a/' + encodeURIComponent(article.id), label: 'Open' } });
      if (/^#?\/?$/.test(location.hash)) route(); // the library list is showing: refresh it
    } catch (e) {
      if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
      toast('Could not save ' + (host(url) || url) + ': ' + e.message, { error: true });
    }
  }

  // Links in articles open in the system browser, never inside the app's webview.
  function openExternally() {
    document.addEventListener('click', (ev) => {
      const a = ev.target.closest && ev.target.closest('a[href]');
      if (!a || ev.defaultPrevented) return;
      const url = webUrl(a.href);
      if (!url || a.origin === location.origin) return;
      ev.preventDefault();
      invoke('plugin:opener|open_url', { url }).catch((e) => toast('Could not open the link: ' + e, { error: true }));
    });
  }

  // ---------- routing ----------

  async function route() {
    const my = ++nav;
    reader = null;
    hidePop();
    if (!isConfigured()) return showLogin();
    $nav.hidden = false;
    const parts = location.hash.replace(/^#\/?/, '').split('/');
    try {
      if (parts[0] === 'settings' && IN_APP) showSetup({ settings: true });
      else if (parts[0] === 'a' && parts[1]) await readerView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's' && parts[1]) await synthesisView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's') await synthesesView(my);
      else await libraryView(my);
    } catch (e) {
      if (my !== nav) return;
      if (e instanceof AuthError) return showLogin('Token rejected, enter it again.');
      show(h('p', null, h('a', { href: '#/' }, '← Library')), errorNode(e));
    }
  }

  const $logout = document.getElementById('logout');
  if (IN_APP) {
    $logout.textContent = 'Settings';
    $logout.addEventListener('click', () => { location.hash = '#/settings'; });
  } else {
    $logout.addEventListener('click', async () => {
      await clearToken();
      route();
    });
  }
  window.addEventListener('hashchange', route);

  (async () => {
    await loadConfig();
    await route();
    if (IN_APP) {
      openExternally();
      if (appInfo.share) {
        drainShares();
        document.addEventListener('visibilitychange', () => { if (!document.hidden) drainShares(); });
        window.addEventListener('focus', drainShares);
        if (window.__TAURI__ && window.__TAURI__.event) window.__TAURI__.event.listen('new-intent', drainShares);
      }
    }
  })();
})();
