// Marginalia web page. Plain JS, no dependencies. All data comes from the REST API with a bearer token, or with none
// on a device the server recognises as the owner's (Tailscale identity, see probeTailscale); the only HTML ever
// inserted with innerHTML is what the server sanitised (/articles/{id}/html).
//
// Books (kind "book", EPUB) open in a full-bleed reader built on the vendored foliate-js (see the "books" section).
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
  const REJECTED = 'Token rejected, enter it again.';
  const NEEDED = 'Enter the access token.';

  // ---------- environment ----------

  const IN_APP = !!(window.__TAURI__ || window.__TAURI_INTERNALS__);
  const invoke = (cmd, args) => (window.__TAURI__ && window.__TAURI__.core ? window.__TAURI__.core.invoke : window.__TAURI_INTERNALS__.invoke)(cmd, args);
  let API_BASE = ''; // '' = same origin (the browser page); the server URL, with no trailing slash, in the app
  let appToken = null; // the app's copy of the keychain token
  let tokenless = false; // the server let this device in with no token (Tailscale identity), so none is needed
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

  const isConfigured = () => (!!getToken() || tokenless) && (!IN_APP || !!API_BASE);

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

  // With no stored token, ask GET /me without one. 200 means the server recognised this device (a Tailscale login it
  // trusts, or it has no token at all), so no token is needed; 401 or no answer means show the token form. In the app
  // the server is the saved URL, or the default one before first setup (kept once it answers).
  async function probeTailscale() {
    if (getToken()) return;
    const base = IN_APP ? API_BASE || DEFAULT_SERVER : API_BASE;
    try {
      tokenless = (await request(base, null, '/me', { timeout: 10000 })).ok;
    } catch (_) {
      tokenless = false;
    }
    if (tokenless && IN_APP && !API_BASE) {
      API_BASE = base;
      localStorage.setItem(SERVER_KEY, base);
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

  // The one place that talks to the server: adds the base URL and the bearer token (none on a Tailscale-recognised
  // device: the Authorization header is left out). A network failure becomes a readable Error (browsers say "Failed
  // to fetch" or "Load failed").
  // `body` (a Blob/File, sent as is, with `headers`) is for the EPUB upload; `keepalive` lets the last progress save
  // outlive a closing page.
  async function request(base, token, path, { json, method, timeout, body, headers, keepalive } = {}) {
    const init = { method: method || (json === undefined && body === undefined ? 'GET' : 'POST'), headers: { ...headers } };
    if (token) init.headers.Authorization = 'Bearer ' + token;
    if (json !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(json);
    } else if (body !== undefined) {
      init.body = body;
    }
    if (keepalive) init.keepalive = true;
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

  // `blob: true` returns the body as a Blob (the EPUB file) instead of parsing it by content type.
  async function api(path, { json, method, body, headers, keepalive, blob } = {}) {
    const token = getToken();
    if (!token && !tokenless) throw new AuthError(NEEDED);
    const res = await request(API_BASE, token, path, { json, method, body, headers, keepalive });
    if (res.status === 401) {
      await clearToken();
      tokenless = false; // show the token form rather than retry the same unauthorised request
      throw new AuthError(token ? REJECTED : NEEDED);
    }
    if (!res.ok) throw new Error(await errorMessage(res));
    if (blob) return res.blob();
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
    $app.replaceChildren(...nodes.filter((n) => n != null && n !== false));
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
    show(h('h1', null, 'Marginalia'), h('p', { class: 'muted' }, 'Enter the access token. It is kept in this browser only. Devices signed in to Tailscale as the owner get in without a token.'), form, status);
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
    if (res.status === 401) throw new Error(token ? 'The server is reachable, but it rejected the token.'
      : 'The server is reachable, but needs an access token: this device is not signed in to Tailscale as its owner.');
    if (!res.ok) throw new Error(await errorMessage(res));
    return 'Connected. ' + (token ? 'The token is accepted' : 'No token needed on this device') + (body.llm ? '.' : ' (the server has no LLM configured, so chat, analyses and briefs are off).');
  }

  // First-run screen and settings in one: server URL + token, test, save, and sign out.
  function showSetup({ message, settings } = {}) {
    $nav.hidden = !settings;
    const hasToken = !!getToken();
    const server = h('input', { type: 'url', inputmode: 'url', autocapitalize: 'none', autocorrect: 'off', spellcheck: 'false', autocomplete: 'off',
      placeholder: DEFAULT_SERVER, value: API_BASE || DEFAULT_SERVER, 'aria-label': 'Server URL', required: true });
    const token = h('input', { type: 'password', autocomplete: 'off', autocapitalize: 'none',
      placeholder: settings && hasToken ? 'Leave empty to keep the current token' : 'Access token (not needed when signed in to Tailscale as the owner)', 'aria-label': 'Access token' });
    const status = h('p', { class: 'status' + (message ? ' error' : ' muted'), 'aria-live': 'polite' }, message || '');
    const testBtn = h('button', { type: 'button', class: 'secondary' }, 'Test connection');
    const saveBtn = h('button', { type: 'submit' }, settings ? 'Save' : 'Connect');

    const values = () => {
      const base = normalizeServer(server.value);
      const tok = token.value.trim() || (settings && hasToken ? getToken() : '');
      if (!base) throw new Error('Enter the server URL, for example ' + DEFAULT_SERVER);
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
        if (tok) {
          await setToken(tok);
          tokenless = false;
        } else {
          await clearToken(); // it got in with no token: Tailscale knows this device
          tokenless = true;
        }
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
        : 'Connect to your Marginalia server. This device must be on Tailscale; devices signed in as the owner need no token.' + where),
      form,
      settings ? h('div', { class: 'row' }, h('button', { type: 'button', class: 'secondary', onclick: async () => {
        await clearToken();
        tokenless = false;
        route();
      } }, 'Sign out')) : null,
      settings && appInfo.version ? h('p', { class: 'meta' }, 'Marginalia app ' + appInfo.version) : null);
    if (!settings) token.focus({ preventScroll: true });
  }

  // ---------- library ----------

  function articleItem(a) {
    const book = a.kind === 'book';
    const bits = [book ? a.author : host(a.url), book && typeof a.progress === 'number' ? Math.round(a.progress * 100) + '% read' : '',
      a.words + ' words', a.highlight_count + (a.highlight_count === 1 ? ' highlight' : ' highlights'), fmtDate(a.saved_at)];
    return h('li', null, h('a', { class: 'item', href: '#/a/' + encodeURIComponent(a.id) },
      h('span', { class: 'title' }, book ? h('span', { class: 'tag' }, 'Book') : null, a.title),
      h('span', { class: 'meta' }, bits.filter(Boolean).join(' · '))));
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
        if (e instanceof AuthError) return showLogin(e.message);
        saveStatus.className = 'status error';
        saveStatus.textContent = 'Could not save: ' + e.message;
        saveBtn.disabled = false;
      }
    } }, saveInput, saveBtn);

    // Add book: the raw EPUB is the request body (no multipart). fetch has no upload progress events, so the status
    // names the file and its size and shows an indeterminate bar until the server answers.
    const bookInput = h('input', { type: 'file', accept: '.epub,application/epub+zip', hidden: true, 'aria-label': 'EPUB file' });
    const bookBtn = h('button', { type: 'button', class: 'secondary' }, 'Add book');
    const bookStatus = h('p', { class: 'status muted', 'aria-live': 'polite' });
    bookBtn.addEventListener('click', () => bookInput.click());
    bookInput.addEventListener('change', async () => {
      const file = bookInput.files[0];
      bookInput.value = '';
      if (!file) return;
      bookBtn.disabled = true;
      bookStatus.className = 'status muted';
      bookStatus.replaceChildren(h('progress', { 'aria-label': 'Uploading' }), ' Uploading ' + file.name + ' (' + (file.size / 1048576).toFixed(1) + ' MB) and indexing it...');
      try {
        const book = await api('/books', { body: file, headers: { 'Content-Type': 'application/epub+zip', 'X-Filename': encodeURIComponent(file.name) } });
        bookCachePut(book.id, file);
        location.hash = '#/a/' + encodeURIComponent(book.id);
      } catch (e) {
        if (e instanceof AuthError) return showLogin(e.message);
        bookStatus.className = 'status error';
        bookStatus.textContent = 'Could not add the book: ' + e.message;
        bookBtn.disabled = false;
      }
    });

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
        if (e instanceof AuthError) return showLogin(e.message);
        results.replaceChildren(errorNode(e));
      }
    };
    const searchForm = h('form', { class: 'row', role: 'search', onsubmit: (ev) => { ev.preventDefault(); clearTimeout(timer); runSearch(); } }, searchInput);
    searchInput.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(runSearch, 300); });

    const listBox = h('div', null, h('p', { class: 'muted' }, 'Loading...'));
    show(h('h1', null, 'Library'), saveForm, saveStatus, h('div', { class: 'row' }, bookBtn), bookInput, bookStatus, searchForm, results, listBox);

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
      if (offset === 0) listBox.replaceChildren(h('p', { class: 'muted' }, 'Nothing saved yet. Paste a URL or add a book above.'));
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
    // All three start at once; a book has no /html, so that request is dropped when the article turns out to be one.
    const htmlReq = api('/articles/' + q + '/html');
    const hlReq = api('/articles/' + q + '/highlights');
    const article = await api('/articles/' + q).catch((e) => { htmlReq.catch(() => {}); hlReq.catch(() => {}); throw e; });
    if (article.kind === 'book') {
      htmlReq.catch(() => {});
      const highlights = await hlReq;
      if (my !== nav) return;
      return bookView(article, highlights, my);
    }
    const [html, highlights] = await Promise.all([htmlReq, hlReq]);
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
      if (!reader && !book) return;
      save.disabled = true;
      err.textContent = '';
      try {
        if (book) { // a book's highlight carries its CFI anchor and chapter; the text is drawn in place, no reload
          await book.saveSelection(note.value.trim() || null);
          hidePop();
          return;
        }
        await api('/articles/' + encodeURIComponent(reader.id) + '/highlights', { json: { text: pending, note: note.value.trim() || null } });
        const y = window.scrollY;
        hidePop();
        window.getSelection().removeAllRanges();
        await route();
        window.scrollTo(0, y);
      } catch (e) {
        if (e instanceof AuthError) return showLogin(e.message);
        err.textContent = e.message;
        save.disabled = false;
      }
    } }, note, save, h('button', { type: 'button', class: 'secondary', onclick: () => { hidePop(); if (book) book.view.deselect(); else window.getSelection().removeAllRanges(); } }, 'Cancel'));
    pop.replaceChildren(form, err);
    placePop(rect);
    note.focus();
  }

  function onSelectionSettled() {
    if (book) return; // inside a book the selection lives in the section's iframe (bookSelectionSettled)
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

  // ---------- books (EPUB reader) ----------
  //
  // Books are articles with kind "book". foliate-js (web/vendor/foliate-js, MIT) parses the EPUB and renders each
  // section into an iframe; it is loaded on demand with a relative import(), so it works from the server and from the
  // Tauri bundle alike. The EPUB comes from GET /articles/{id}/file through api() (so it carries the bearer token or none
  // on a Tailscale device, never a URL token) and is kept in IndexedDB, content-addressed by article id. Scripts inside
  // an EPUB cannot run: the page CSP has script-src 'self' without blob:, which the section iframes inherit, and
  // script resources are refused in the loader as well.
  //
  // Positions are EPUB CFIs. Progress {position, fraction} is PUT to /articles/{id}/progress (debounced, flushed when the
  // page is hidden). A highlight's anchor is the CFI of the selected range (view.getCFI), drawn with view.addAnnotation.

  const READER_KEY = 'marginalia.reader'; // reading settings, per device
  const DEFAULT_SETTINGS = { flow: 'paginated', theme: 'auto', font: 'publisher', size: 18, line: 1.5, margin: 'medium', columns: 2, align: 'publisher', hyphenate: true };
  const THEMES = { // book page colours; the app chrome follows through #book[data-theme] in app.css
    light: { bg: '#fbfaf7', fg: '#1d1d1b', link: '#1a5fb4' },
    dark: { bg: '#17171a', fg: '#e6e4dc', link: '#7db2f5' },
    sepia: { bg: '#f4ecd8', fg: '#5b4636', link: '#8a4b12' },
  };
  const MARGINS = { narrow: { gap: '3%', margin: '28px' }, medium: { gap: '7%', margin: '48px' }, wide: { gap: '12%', margin: '64px' } };
  const FONTS = {
    serif: 'Georgia, "Iowan Old Style", Cambria, "Times New Roman", serif',
    sans: 'system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
  };
  const HIGHLIGHT_COLOR = '#f2b705';
  const BARS_MS = 3500; // how long the toolbars stay after the last interaction
  const SAVE_MS = 2000; // progress debounce
  const CACHE_BOOKS = 6; // EPUBs kept in IndexedDB

  let book = null; // {id, view, saveSelection, destroy} while a book is open

  function loadSettings() {
    let saved = {};
    try { saved = JSON.parse(localStorage.getItem(READER_KEY)) || {}; } catch (_) { /* keep the defaults */ }
    const s = { ...DEFAULT_SETTINGS, ...saved };
    s.size = Math.min(34, Math.max(12, +s.size || DEFAULT_SETTINGS.size));
    s.line = Math.min(2.2, Math.max(1.1, +s.line || DEFAULT_SETTINGS.line));
    if (!MARGINS[s.margin]) s.margin = DEFAULT_SETTINGS.margin;
    if (s.columns !== 1) s.columns = 2;
    return s;
  }

  // ----- IndexedDB cache of the EPUB files (best effort: any failure just means a download) -----

  function idb() {
    return new Promise((resolve, reject) => {
      const req = indexedDB.open('marginalia', 1);
      req.onupgradeneeded = () => req.result.createObjectStore('books');
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }

  async function bookCacheGet(id) {
    try {
      const db = await idb();
      return await new Promise((resolve, reject) => {
        const req = db.transaction('books').objectStore('books').get(id);
        req.onsuccess = () => resolve(req.result ? req.result.blob : null);
        req.onerror = () => reject(req.error);
      });
    } catch (_) { return null; }
  }

  async function bookCachePut(id, blob) {
    try {
      const db = await idb();
      const store = db.transaction('books', 'readwrite').objectStore('books');
      store.put({ blob, at: Date.now() }, id);
      const seen = [];
      store.openCursor().onsuccess = (ev) => { // keys and times only: the blobs are not read
        const cur = ev.target.result;
        if (cur) { seen.push([cur.key, cur.value.at]); cur.continue(); return; }
        seen.sort((a, b) => b[1] - a[1]);
        for (const [key] of seen.slice(CACHE_BOOKS)) store.delete(key);
      };
    } catch (_) { /* not cached */ }
  }

  let foliate = null;
  function loadFoliate() {
    if (!foliate) {
      foliate = Promise.all([import('./vendor/foliate-js/view.js'), import('./vendor/foliate-js/overlayer.js'), import('./vendor/foliate-js/footnotes.js')])
        .then(([, o, f]) => ({ Overlayer: o.Overlayer, FootnoteHandler: f.FootnoteHandler }))
        .catch((e) => { foliate = null; throw new Error('Could not load the EPUB reader: ' + (e.message || e)); });
    }
    return foliate;
  }

  // Style sheet injected into every section (renderer.setStyles). Light keeps the book's own colours on a light page;
  // dark and sepia force colours so text stays readable.
  function readerCSS(c, theme) {
    const t = THEMES[theme];
    const family = FONTS[c.font];
    const align = c.align === 'publisher' ? '' : 'text-align: ' + (c.align === 'justify' ? 'justify' : 'start') + ' !important;';
    const hyph = c.hyphenate ? 'auto' : 'manual';
    return `
      @namespace epub "http://www.idpf.org/2007/ops";
      html { color-scheme: ${theme === 'dark' ? 'dark' : 'light'}; background: ${t.bg} !important; color: ${t.fg} !important; font-size: ${c.size}px !important; }
      body { background: transparent !important; color: ${t.fg} !important; }
      ${theme === 'light' ? '' : 'body * { color: inherit !important; background-color: transparent !important; }'}
      a:link, a:visited, a:link *, a:visited * { color: ${t.link} !important; }
      ${family ? `body, body *:not(pre):not(code):not(kbd):not(samp):not(tt) { font-family: ${family} !important; }` : ''}
      p, li, blockquote, dd {
        line-height: ${c.line} !important;
        ${align}
        -webkit-hyphens: ${hyph}; hyphens: ${hyph};
        -webkit-hyphenate-limit-before: 3; -webkit-hyphenate-limit-after: 2;
        widows: 2; orphans: 2;
      }
      pre { white-space: pre-wrap !important; }
      aside[epub|type~="endnote"], aside[epub|type~="footnote"], aside[epub|type~="note"], aside[epub|type~="rearnote"] { display: none; }
    `;
  }

  // Rectangle of something inside a section iframe, in page coordinates (what placePop expects).
  function pageRect(doc, r) {
    const fr = doc.defaultView && doc.defaultView.frameElement;
    const o = fr ? fr.getBoundingClientRect() : { left: 0, top: 0 };
    return { left: r.left + o.left, right: r.right + o.left, top: r.top + o.top, bottom: r.bottom + o.top };
  }

  function closeBook() {
    if (book) book.destroy();
    book = null;
    document.body.classList.remove('reading');
  }

  async function bookView(article, highlights, my) {
    const id = article.id;
    const q = encodeURIComponent(id);
    const status = h('p', { class: 'muted', 'aria-live': 'polite' }, 'Opening the book...');
    show(h('p', null, h('a', { href: '#/' }, '← Library')), h('h1', null, article.title), status);

    const lib = loadFoliate();
    lib.catch(() => {});
    let blob = await bookCacheGet(id);
    if (!blob) {
      status.textContent = 'Downloading the book...';
      blob = await api('/articles/' + q + '/file', { blob: true });
      bookCachePut(id, blob);
    }
    let progress = null;
    let syncOn = true; // false when the saved position could not be read: do not overwrite it
    try {
      progress = await api('/articles/' + q + '/progress');
    } catch (e) {
      if (e instanceof AuthError) throw e;
      syncOn = false;
    }
    const { Overlayer, FootnoteHandler } = await lib;
    if (my !== nav) return;

    const cfg = loadSettings();
    const view = document.createElement('foliate-view');
    const hls = highlights.slice();
    const hlIndex = new Map(); // highlight -> section index, for those with an anchor that resolves
    const cleanups = [];
    const listen = (target, type, fn, opts) => { target.addEventListener(type, fn, opts); cleanups.push(() => target.removeEventListener(type, fn, opts)); };

    // ----- chrome -----
    const iconBtn = (label, text, onclick) => h('button', { type: 'button', class: 'tb', 'aria-label': label, title: label, onclick }, text);
    const title = h('span', { class: 'book-title' }, article.title);
    const hlBtn = iconBtn('Highlights and notes', 'Notes', () => openPanel('highlights', hlBtn));
    const top = h('div', { class: 'book-top' },
      h('a', { class: 'tb', href: '#/', 'aria-label': 'Back to the library', title: 'Library' }, '‹'), title,
      iconBtn('Table of contents', '☰', function () { openPanel('toc', this); }),
      iconBtn('Search in the book', 'Find', function () { openPanel('search', this); }),
      hlBtn,
      iconBtn('Reading settings', 'Aa', function () { openPanel('settings', this); }));
    const chapterEl = h('span', { class: 'book-chapter' });
    const percentEl = h('span', { class: 'book-percent' });
    const slider = h('input', { type: 'range', min: 0, max: 1, step: 'any', value: 0, class: 'book-slider', 'aria-label': 'Position in the book', list: 'book-ticks' });
    const ticks = h('datalist', { id: 'book-ticks' });
    const bottom = h('div', { class: 'book-bottom' },
      h('div', { class: 'book-where' }, chapterEl, percentEl),
      h('div', { class: 'book-seek' }, iconBtn('Previous page', '‹', () => view.goLeft()), slider, ticks, iconBtn('Next page', '›', () => view.goRight())));
    const backBtn = h('button', { type: 'button', class: 'book-back', hidden: true, onclick: () => view.history.back() }, '← Back');
    const stage = h('div', { class: 'book-stage' }, view);
    const sheet = h('div', { class: 'book-sheet', role: 'dialog', 'aria-modal': 'true', hidden: true });
    const scrim = h('div', { class: 'book-scrim', hidden: true, onclick: () => closePanel() });
    const fnBox = h('div', { class: 'book-fn', role: 'dialog', 'aria-label': 'Note', hidden: true });
    const root = h('div', { id: 'book', class: 'book bars' }, stage, top, bottom, backBtn, scrim, sheet, fnBox);
    document.body.append(root);
    document.body.classList.add('reading');

    // ----- toolbars -----
    let barsTimer = 0;
    const barsOn = () => root.classList.contains('bars');
    function hideBars() {
      clearTimeout(barsTimer);
      if (sheet.hidden) root.classList.remove('bars');
    }
    function showBars() {
      root.classList.add('bars');
      clearTimeout(barsTimer);
      barsTimer = setTimeout(() => {
        if (!top.matches(':hover') && !bottom.matches(':hover')) hideBars();
        else showBars();
      }, BARS_MS);
    }
    const toggleBars = () => (barsOn() ? hideBars() : showBars());
    for (const el of [top, bottom]) el.addEventListener('pointerdown', () => { if (barsOn()) showBars(); });
    showBars();

    // ----- settings -----
    const dark = window.matchMedia('(prefers-color-scheme: dark)');
    const theme = () => (cfg.theme === 'auto' ? (dark.matches ? 'dark' : 'light') : cfg.theme);
    function applySettings() {
      root.dataset.theme = theme();
      const r = view.renderer;
      if (!r) return;
      const m = MARGINS[cfg.margin];
      r.setAttribute('flow', cfg.flow);
      r.setAttribute('gap', m.gap);
      r.setAttribute('margin', m.margin);
      r.setAttribute('max-column-count', String(cfg.columns));
      r.setStyles && r.setStyles(readerCSS(cfg, theme()));
    }
    const saveSettings = () => { try { localStorage.setItem(READER_KEY, JSON.stringify(cfg)); } catch (_) { /* private mode */ } };
    const onScheme = () => { if (cfg.theme === 'auto') applySettings(); };
    dark.addEventListener('change', onScheme);
    cleanups.push(() => dark.removeEventListener('change', onScheme));
    root.dataset.theme = theme();

    // ----- progress sync -----
    let cur = null; // {cfi, fraction} of the page on screen
    let saved = progress ? progress.position : null; // last position the server has
    let saveTimer = 0;
    function pushProgress(keepalive) {
      clearTimeout(saveTimer);
      if (!syncOn || !cur || cur.cfi === saved || (!progress && !saved && cur.fraction < 0.001)) return;
      const before = saved;
      saved = cur.cfi;
      // fetch starts synchronously inside api(), which a keepalive request needs while the page is closing
      api('/articles/' + q + '/progress', { method: 'PUT', json: { position: cur.cfi, fraction: cur.fraction }, keepalive })
        .catch(() => { if (saved === cur.cfi) saved = before; });
    }
    const flush = () => pushProgress(true);
    listen(document, 'visibilitychange', () => { if (document.hidden) flush(); });
    listen(window, 'pagehide', flush);

    // ----- panels -----
    let panel = null;
    let opener = null;
    function closePanel() {
      if (!panel) return;
      panel = null;
      sheet.hidden = scrim.hidden = true;
      sheet.replaceChildren();
      if (opener && opener.isConnected) opener.focus({ preventScroll: true });
      showBars();
    }
    function openPanel(name, from) {
      hidePop();
      opener = from || opener;
      panel = name;
      clearTimeout(barsTimer);
      const builders = { toc: tocPanel, search: searchPanel, highlights: highlightsPanel, settings: settingsPanel };
      const [heading, content] = builders[name]();
      const close = h('button', { type: 'button', class: 'secondary', onclick: () => closePanel() }, 'Close');
      sheet.setAttribute('aria-label', heading);
      sheet.replaceChildren(h('div', { class: 'sheet-head' }, h('h2', null, heading), close), h('div', { class: 'sheet-body' }, content));
      sheet.hidden = scrim.hidden = false;
      const first = sheet.querySelector('input[type=search], .toc-cur, .sheet-body button, .sheet-body input') || close;
      first.focus({ preventScroll: true });
      if (name === 'toc') { const c = sheet.querySelector('.toc-cur'); if (c) c.scrollIntoView({ block: 'center' }); }
    }
    const jump = (target) => { closePanel(); hideBars(); return view.goTo(target); };

    // Table of contents (nested)
    let currentHref = null;
    function tocList(items) {
      return h('ul', { class: 'toc' }, items.map((it) => h('li', null,
        h('button', { type: 'button', class: 'toc-item' + (it.href && it.href === currentHref ? ' toc-cur' : ''), 'aria-current': it.href && it.href === currentHref ? 'location' : null,
          onclick: () => jump(it.href) }, (it.label || '').trim() || 'Untitled'),
        it.subitems && it.subitems.length ? tocList(it.subitems) : null)));
    }
    function tocPanel() {
      const toc = view.book.toc || [];
      return ['Contents', toc.length ? tocList(toc) : h('p', { class: 'muted' }, 'This book has no table of contents.')];
    }

    // Search: results grouped by chapter, matches outlined in the text until cleared
    let searchState = { query: '', box: null };
    let searchSeq = 0;
    function searchPanel() {
      const input = h('input', { type: 'search', placeholder: 'Search in this book', 'aria-label': 'Search in this book', enterkeyhint: 'search', value: searchState.query });
      const bar = h('progress', { max: 1, value: 0, hidden: true, 'aria-label': 'Search progress' });
      const info = h('p', { class: 'status muted', 'aria-live': 'polite' });
      const results = searchState.box || h('div', { class: 'results' });
      searchState.box = results;
      const clear = h('button', { type: 'button', class: 'secondary', onclick: () => {
        searchSeq++;
        view.clearSearch();
        searchState = { query: '', box: null };
        results.replaceChildren();
        input.value = '';
        info.textContent = '';
        bar.hidden = true;
        input.focus();
      } }, 'Clear');
      const run = async () => {
        const query = input.value.trim();
        const seq = ++searchSeq;
        results.replaceChildren();
        searchState.query = query;
        if (!query) { view.clearSearch(); info.textContent = ''; return; }
        info.textContent = 'Searching...';
        bar.hidden = false;
        let count = 0;
        try {
          for await (const r of view.search({ query })) {
            if (seq !== searchSeq) return;
            if (r === 'done') break;
            if (r.progress != null) { bar.value = r.progress; continue; }
            if (!r.subitems) continue;
            count += r.subitems.length;
            results.append(h('h3', null, r.label || 'Untitled'),
              h('ul', { class: 'hits' }, r.subitems.map((s) => h('li', null,
                h('button', { type: 'button', class: 'hit', onclick: () => jump(s.cfi) },
                  s.excerpt.pre, h('mark', null, s.excerpt.match), s.excerpt.post)))));
            info.textContent = 'Searching... ' + count + ' found';
            if (count >= 500) break;
          }
          if (seq === searchSeq) info.textContent = count ? count + (count >= 500 ? '+' : '') + ' matches' : 'No matches.';
        } catch (e) {
          if (seq === searchSeq) info.textContent = 'Search failed: ' + e.message;
        } finally {
          if (seq === searchSeq) bar.hidden = true;
        }
      };
      const form = h('form', { class: 'row', role: 'search', onsubmit: (ev) => { ev.preventDefault(); run(); } }, input, h('button', { type: 'submit' }, 'Search'), clear);
      return ['Search', [form, bar, info, results]];
    }

    // Highlights and notes
    function highlightsPanel() {
      if (!hls.length) return ['Highlights', h('p', { class: 'muted' }, 'Select text in the book to highlight it, with an optional note.')];
      const rows = hls.map((hl) => {
        const body = [
          hl.location ? h('div', { class: 'meta' }, hl.location) : null,
          h('div', { class: 'quote' }, hl.text),
          hl.note ? h('div', { class: 'note' }, hl.note) : null,
        ];
        return h('li', null, hl.anchor && hlIndex.has(hl)
          ? h('button', { type: 'button', class: 'hl-row', onclick: () => jump(hl.anchor) }, body)
          : h('div', { class: 'hl-row' }, body, h('div', { class: 'meta' }, 'No position in the book (added elsewhere).')));
      });
      return ['Highlights (' + hls.length + ')', h('ul', { class: 'highlights' }, rows)];
    }

    // Settings
    function settingsPanel() {
      const set = (key, value) => { cfg[key] = value; saveSettings(); applySettings(); };
      const seg = (label, key, options) => {
        const btns = options.map(([text, value]) => h('button', { type: 'button', class: 'secondary', 'aria-pressed': String(cfg[key] === value), onclick: () => {
          set(key, value);
          btns.forEach((b, i) => b.setAttribute('aria-pressed', String(options[i][1] === value)));
        } }, text));
        return h('div', { class: 'set-row', role: 'group', 'aria-label': label }, h('span', { class: 'set-label' }, label), h('div', { class: 'seg' }, btns));
      };
      const range = (label, key, min, max, step, fmt) => {
        const out = h('output', null, fmt(cfg[key]));
        const input = h('input', { type: 'range', min, max, step, value: cfg[key], 'aria-label': label, oninput: () => {
          out.textContent = fmt(+input.value);
          set(key, +input.value);
        } });
        return h('div', { class: 'set-row' }, h('span', { class: 'set-label' }, label), h('div', { class: 'slide' }, input, out));
      };
      const hyph = h('input', { type: 'checkbox', checked: cfg.hyphenate, id: 'set-hyph', onchange: () => set('hyphenate', hyph.checked) });
      return ['Reading settings', [
        seg('Layout', 'flow', [['Paginated', 'paginated'], ['Scrolled', 'scrolled']]),
        seg('Theme', 'theme', [['Auto', 'auto'], ['Light', 'light'], ['Dark', 'dark'], ['Sepia', 'sepia']]),
        seg('Font', 'font', [['Publisher', 'publisher'], ['Serif', 'serif'], ['Sans', 'sans']]),
        range('Font size', 'size', 12, 34, 1, (v) => v + ' px'),
        range('Line height', 'line', 1.1, 2.2, 0.05, (v) => v.toFixed(2)),
        seg('Margins', 'margin', [['Narrow', 'narrow'], ['Medium', 'medium'], ['Wide', 'wide']]),
        seg('Columns (wide screens)', 'columns', [['1', 1], ['2', 2]]),
        seg('Alignment', 'align', [['Publisher', 'publisher'], ['Left', 'start'], ['Justified', 'justify']]),
        h('div', { class: 'set-row' }, h('label', { for: 'set-hyph', class: 'set-label' }, 'Hyphenation'), hyph),
      ]];
    }

    // ----- highlights in the text -----
    function indexHighlight(hl) {
      if (!hl.anchor) return;
      try {
        const r = view.resolveNavigation(hl.anchor);
        if (r && typeof r.index === 'number') hlIndex.set(hl, r.index);
      } catch (_) { /* stays in the panel only */ }
    }
    const hlByAnchor = (value) => hls.find((hl) => hl.anchor === value);
    let annotationAt = 0; // a tap on a highlight must not also count as a tap on the page

    let sel = null; // {text, cfi, location} of the current selection, kept because tapping the popover can clear it
    function bookSelectionSettled(doc, index) {
      if (popBusy || mouseDown || Date.now() < popHoldUntil) return;
      const s = doc.getSelection();
      if (!s || s.rangeCount === 0 || s.isCollapsed) return hidePop();
      const range = s.getRangeAt(0);
      const text = s.toString().trim();
      if (!text) return hidePop();
      let cfi = null;
      let location = null;
      try {
        cfi = view.getCFI(index, range);
        const item = view.getProgressOf(index, range).tocItem;
        location = item && item.label ? item.label.trim() : null;
      } catch (e) { console.error(e); }
      sel = { text: text.slice(0, 5000), cfi, location };
      pending = sel.text;
      showPopButton(pageRect(doc, range.getBoundingClientRect()));
    }

    async function saveSelection(note) {
      if (!sel) return;
      const s = sel;
      const res = await api('/articles/' + q + '/highlights', { json: { text: s.text, note, anchor: s.cfi, location: s.location } });
      const hl = { text: s.text, note, anchor: s.cfi, location: s.location, ...(res && typeof res === 'object' ? res : {}) };
      hls.push(hl);
      indexHighlight(hl);
      if (hl.anchor && hlIndex.has(hl)) view.addAnnotation({ value: hl.anchor });
      view.deselect();
      sel = null;
    }

    function showNote(hl, range) {
      const doc = range.startContainer.ownerDocument;
      popBusy = true;
      pop.replaceChildren(
        h('div', { class: 'hl-note' }, hl.note ? hl.note : h('span', { class: 'muted' }, 'No note on this highlight.')),
        h('button', { type: 'button', class: 'secondary', onclick: () => hidePop() }, 'Close'));
      placePop(pageRect(doc, range.getBoundingClientRect()));
    }

    // ----- taps, keys, selection inside the section iframes -----
    function tap(x) {
      const f = x / window.innerWidth;
      if (cfg.flow === 'scrolled' || (f > 1 / 3 && f < 2 / 3)) return toggleBars();
      hideBars();
      return f < 0.5 ? view.goLeft() : view.goRight();
    }

    function onKey(ev) {
      if (ev.defaultPrevented || ev.ctrlKey || ev.metaKey || ev.altKey) return;
      const t = ev.target;
      if (t && t.closest && t.closest('input, textarea, select, [contenteditable]')) return;
      const k = ev.key;
      if (k === 'Escape') {
        if (!fnBox.hidden) closeFootnote();
        else if (panel) closePanel();
        else if (!pop.hidden) hidePop();
        else toggleBars();
        return;
      }
      if (k === 'Tab') return showBars();
      if (panel || !fnBox.hidden) return;
      if ((k === ' ' || k === 'Enter') && t && t.closest && t.closest('button, a, summary')) return;
      let act = null;
      if (k === 'ArrowLeft') act = () => view.goLeft();
      else if (k === 'ArrowRight') act = () => view.goRight();
      else if (k === 'PageUp' || (k === ' ' && ev.shiftKey)) act = () => view.prev();
      else if (k === 'PageDown' || k === ' ') act = () => view.next();
      if (!act) return;
      ev.preventDefault();
      hideBars();
      act();
    }
    listen(document, 'keydown', onKey);

    view.addEventListener('load', (ev) => {
      const { doc, index } = ev.detail;
      const settle = (ms) => { clearTimeout(selTimer); selTimer = setTimeout(() => bookSelectionSettled(doc, index), ms); };
      doc.addEventListener('selectionchange', () => settle(touchUi.matches ? 300 : 200));
      doc.addEventListener('pointerdown', (e) => { if (e.pointerType === 'mouse') mouseDown = true; });
      doc.addEventListener('pointerup', (e) => { if (e.pointerType === 'mouse' && mouseDown) { mouseDown = false; settle(0); } });
      doc.addEventListener('pointercancel', () => { mouseDown = false; });
      doc.addEventListener('keydown', onKey);
      doc.addEventListener('click', (e) => {
        const s = doc.getSelection();
        if ((s && !s.isCollapsed) || e.defaultPrevented) return; // finishing a selection, or a link foliate handled
        const fr = doc.defaultView.frameElement;
        const x = (fr ? fr.getBoundingClientRect().left : 0) + e.clientX;
        setTimeout(() => { // after the overlayer had its chance to report a tap on a highlight
          if (Date.now() - annotationAt < 100) return;
          if (!pop.hidden) return hidePop();
          tap(x);
        }, 0);
      });
    });
    // Clicks in the page margins around the section land on the view itself.
    view.addEventListener('click', (e) => { if (e.target === view) tap(e.clientX); });

    // ----- footnotes -----
    const footnotes = new FootnoteHandler();
    let fnView = null;
    function closeFootnote() {
      fnBox.hidden = true;
      scrim.hidden = !panel;
      if (fnView) { try { fnView.close(); } catch (_) { /* already gone */ } fnView.remove(); fnView = null; }
      fnBox.replaceChildren();
    }
    footnotes.addEventListener('before-render', (ev) => {
      closeFootnote();
      const v = ev.detail.view;
      fnView = v;
      fnBox.classList.add('loading');
      fnBox.hidden = false;
      fnBox.replaceChildren(v, h('button', { type: 'button', class: 'secondary fn-close', onclick: () => closeFootnote() }, 'Close'));
      const r = v.renderer;
      r.setAttribute('flow', 'scrolled');
      r.setAttribute('margin', '0px');
      r.setAttribute('gap', '4%');
      r.setStyles && r.setStyles(readerCSS(cfg, theme()).replace(/aside\[epub[^}]*}/, ''));
      v.addEventListener('link', (e) => { // a link inside a note: leave the note and follow it in the book
        e.preventDefault();
        closeFootnote();
        view.goTo(e.detail.href);
      });
    });
    footnotes.addEventListener('render', () => { fnBox.classList.remove('loading'); scrim.hidden = false; });
    scrim.addEventListener('click', () => closeFootnote());
    view.addEventListener('link', (ev) => {
      const p = footnotes.handle(view.book, ev);
      if (p) p.catch((e) => { console.error(e); closeFootnote(); view.goTo(ev.detail.href); });
    });
    view.addEventListener('external-link', (ev) => {
      ev.preventDefault();
      const url = webUrl(ev.detail.a.href);
      if (!url) return;
      if (IN_APP) invoke('plugin:opener|open_url', { url }).catch((e) => toast('Could not open the link: ' + e, { error: true }));
      else window.open(url, '_blank', 'noopener,noreferrer');
    });

    // ----- view events -----
    view.addEventListener('relocate', (ev) => {
      const loc = ev.detail;
      cur = { cfi: loc.cfi, fraction: loc.fraction };
      const pct = Math.round(loc.fraction * 100);
      percentEl.textContent = pct + '%';
      chapterEl.textContent = loc.tocItem && loc.tocItem.label ? loc.tocItem.label.trim() : '';
      slider.setAttribute('aria-valuetext', pct + '%' + (chapterEl.textContent ? ', ' + chapterEl.textContent : ''));
      if (!dragging) slider.value = loc.fraction;
      currentHref = loc.tocItem ? loc.tocItem.href : null;
      clearTimeout(saveTimer);
      saveTimer = setTimeout(pushProgress, SAVE_MS);
    });
    view.addEventListener('create-overlay', (ev) => { // a section was (re)loaded: draw its saved highlights
      for (const hl of hls) if (hlIndex.get(hl) === ev.detail.index) view.addAnnotation({ value: hl.anchor });
    });
    view.addEventListener('draw-annotation', (ev) => ev.detail.draw(Overlayer.highlight, { color: HIGHLIGHT_COLOR }));
    view.addEventListener('show-annotation', (ev) => {
      const hl = hlByAnchor(ev.detail.value);
      if (!hl) return;
      annotationAt = Date.now();
      showNote(hl, ev.detail.range);
    });

    let dragging = false;
    slider.addEventListener('input', () => { dragging = true; percentEl.textContent = Math.round(slider.value * 100) + '%'; showBars(); });
    slider.addEventListener('change', async () => {
      await view.goToFraction(parseFloat(slider.value)).catch((e) => console.error(e));
      dragging = false;
    });
    let backTimer = 0;
    view.history.addEventListener('index-change', () => {
      clearTimeout(backTimer);
      backBtn.hidden = !view.history.canGoBack;
      if (!backBtn.hidden) backTimer = setTimeout(() => { backBtn.hidden = true; }, 8000);
    });

    // ----- open -----
    book = {
      id, view, saveSelection,
      destroy() {
        flush();
        clearTimeout(barsTimer); clearTimeout(backTimer); clearTimeout(saveTimer); clearTimeout(selTimer);
        closeFootnote();
        for (const fn of cleanups) fn();
        try { view.close(); } catch (_) { /* never opened */ }
        root.remove();
      },
    };
    try {
      const file = new File([blob], article.title.replace(/[\\/:*?"<>|]+/g, ' ') + '.epub', { type: 'application/epub+zip' });
      await view.open(file);
      if (my !== nav) return;
      view.book.transformTarget?.addEventListener('load', (e) => { if (e.detail.isScript) e.detail.allow = false; }); // no EPUB scripts
      applySettings();
      hls.forEach(indexHighlight); // needs the opened book to resolve CFIs
      for (const f of view.getSectionFractions()) ticks.append(h('option', { value: f }));
      slider.dir = view.book.dir === 'rtl' ? 'rtl' : 'ltr';
      title.textContent = article.title;
      await view.init({ lastLocation: progress && progress.position ? progress.position : null, showTextStart: false });
      if (view.renderer.focusView) view.renderer.focusView();
    } catch (e) {
      if (my !== nav) return;
      console.error(e);
      closeBook();
      show(h('p', null, h('a', { href: '#/' }, '← Library')), h('h1', null, article.title), errorNode(new Error('Could not open this book: ' + (e.message || e))));
    }
  }

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
    $toast.replaceChildren(...[h('span', null, text), action ? h('a', { href: action.href }, action.label) : null].filter(Boolean));
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
      if (e instanceof AuthError) return showLogin(e.message);
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
    closeBook();
    hidePop();
    if (!isConfigured()) return showLogin();
    $nav.hidden = false;
    $logout.hidden = !IN_APP && tokenless && !getToken(); // nothing to sign out of without a token
    const parts = location.hash.replace(/^#\/?/, '').split('/');
    try {
      if (parts[0] === 'settings' && IN_APP) showSetup({ settings: true });
      else if (parts[0] === 'a' && parts[1]) await readerView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's' && parts[1]) await synthesisView(decodeURIComponent(parts[1]), my);
      else if (parts[0] === 's') await synthesesView(my);
      else await libraryView(my);
    } catch (e) {
      if (my !== nav) return;
      if (e instanceof AuthError) return showLogin(e.message);
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
    await probeTailscale();
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
