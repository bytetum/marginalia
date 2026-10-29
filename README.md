# Marginalia

A personal Readwise-like library. Save articles, highlight passages, discuss the library with an agent, have it
analyse an article or write a brief (steered by a one-line direction), and keep those products. Everything is reachable
over REST and over MCP, so external agent harnesses (opencode, Codex, Hermes) can use the library too.

## Architecture

Ports and adapters in one Rust crate (library plus the `marginalia` binary). `service::App` holds every use case;
the access layers (`api.rs` REST, `mcp.rs` MCP, `main.rs` CLI) are thin callers of it. `App` uses `extract.rs`
(Firecrawl or a local Readability port), `agent.rs` (the built-in tool-calling loop against any OpenAI-compatible API)
and two ports from `repo.rs`: `Repo` (embedded SurrealDB 3 in `surreal.rs`, an in-memory reference in `mem.rs`) and
`BlobStore` (content-addressed raw pages in `blob.rs`). Search is keyword full-text (BM25). See `DESIGN.md`.

## Build and run

The toolchain pinned in `rust-toolchain.toml` (Rust 1.99.1) is required: the nixpkgs `rustc` cannot compile
SurrealDB's AVX-512 intrinsics. On NixOS go through the flake (below) or `nix-shell` (rustup based).

```sh
nix-shell --run 'cargo build --release'     # target/release/marginalia
nix-shell --run 'cargo test'
nix-shell --run 'cargo run -- serve'
```

Elsewhere, `rustup` picks the pinned toolchain by itself, and you need `cmake` and a C compiler for the build scripts.

## Nix

`flake.nix` builds the app with the toolchain from `rust-toolchain.toml` (via `oxalica/rust-overlay`), so no rustup is
needed. Systems: x86_64-linux, aarch64-linux, aarch64-darwin, x86_64-darwin (only x86_64-linux has been built and tested so far).

```sh
nix build                         # ./result/bin/marginalia; runs the test suite (no network needed)
nix run . -- serve                # run it
nix run . -- --help
nix develop                       # shell with the pinned toolchain, clippy, rustfmt, rust-analyzer, cmake
nix flake check                   # builds the package with tests enabled
```

The package's main program is `marginalia`. A service manager runs it as `marginalia serve` with `HOST` and `PORT` set
(the app binds `$HOST:$PORT`), secrets as files (`MARGINALIA_TOKEN_FILE`, `LLM_API_KEY_FILE`, `FIRECRAWL_API_KEY_FILE`)
and `MARGINALIA_DATA` pointing at a state directory. Logs go to stderr, SIGTERM stops it cleanly, and `GET /health`
answers without a token. HTTPS fetches need a CA bundle (`/etc/ssl/certs` on NixOS, or `SSL_CERT_FILE`).

## Configuration

Environment variables, or a `.env` file in the current directory (real variables win). See `.env.example` and
`deploy/env.example`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MARGINALIA_DATA` (`--data-dir`) | `./data` | `db/` is SurrealDB, `blobs/` raw pages |
| `MARGINALIA_BIND` (`serve --bind`) | none | listen address of `serve`, e.g. `100.103.153.13:7878`; wins over `HOST`/`PORT` |
| `HOST` | `127.0.0.1` | listen host, used when `MARGINALIA_BIND` is not set and `HOST` or `PORT` is |
| `PORT` | `7878` | listen port, same rule |
| `MARGINALIA_TOKEN` (`serve --token`) | none | bearer token for REST and `/mcp`; unset means no auth |
| `MARGINALIA_TOKEN_FILE` | none | path of a file holding the token (trailing newline dropped); not together with `MARGINALIA_TOKEN` |
| `LLM_BASE_URL` | `https://api.deepseek.com` | OpenAI-compatible chat API |
| `LLM_MODEL` | `deepseek-chat` | model name sent to that API |
| `LLM_API_KEY` | none | without it the built-in agent is off (chat, analyse, brief answer 503) |
| `LLM_API_KEY_FILE` | none | path of a file holding the key; not together with `LLM_API_KEY` |
| `FIRECRAWL_API_KEY` | none | optional; otherwise pages are extracted locally |
| `FIRECRAWL_API_KEY_FILE` | none | path of a file holding the key; not together with `FIRECRAWL_API_KEY` |

Listen address precedence: `--bind` / `MARGINALIA_BIND`, then `$HOST:$PORT` (either may be missing), then `127.0.0.1:7878`.
Setting both a variable and its `_FILE` twin, or pointing a `_FILE` at an empty or missing file, stops the start with an error.

The embedded database admits one process at a time: do not run `marginalia mcp` or the CLI against a data directory
while `marginalia serve` is using it.

## CLI

```
marginalia [--data-dir DIR] serve [--bind ADDR] [--token TOKEN]   REST + MCP (/mcp) on one listener (stops on Ctrl-C or SIGTERM)
marginalia mcp                                                   MCP over stdio (logs go to stderr)
marginalia add <url>                                             save an article
marginalia chat                                                  REPL: /add /list /analyse <id> [direction] /brief [direction] /new /quit
marginalia brief [direction...]                                  write and save a brief
marginalia import <file.json>                                    restore a snapshot into an empty library (offline, see Backup & restore)
```

`serve` warns loudly if it binds a non-loopback address without a token. On a stop signal it finishes in-flight requests and
closes lingering MCP event streams after 5 seconds.

## REST

JSON in and out. With a token set, send `Authorization: Bearer <token>` on everything except `GET /health`, the web
page files (`/`, `/app.js`, `/app.css`; they hold no data) and `GET /save` (see Quick save).
Errors are `{"error": "..."}`: 400 invalid input, 401 auth, 404 not found, 502 upstream (fetch or LLM), 503 no LLM, 500 storage.

| Method and path | Body / query | Result |
| --- | --- | --- |
| `GET /health` | | `{ok, llm}` |
| `GET /articles` | `limit` (50), `offset` (0) | article summaries, newest first |
| `POST /articles` | `{url}` (fetch) or `{url, title, markdown}` | 201, the article |
| `GET /articles/{id}` | | article with Markdown body |
| `DELETE /articles/{id}` | | 204 (also removes its highlights) |
| `GET /articles/{id}/html` | | the article Markdown as sanitised HTML (a fragment, `text/html`) |
| `GET /articles/{id}/highlights` | | highlights |
| `POST /articles/{id}/highlights` | `{text, note?}` | 201, the highlight |
| `GET /highlights` | | all highlights |
| `GET /search` | `q`, `limit` (10) | hits with snippets |
| `POST /chat` | `{session_id?, message}` | `{session_id, reply}` |
| `DELETE /chat/{session_id}` | | 204, forgets the conversation |
| `POST /articles/{id}/analyse` | `{direction?}` (body optional) | the saved synthesis |
| `POST /briefs` | `{direction?}` (body optional) | the saved synthesis |
| `GET /syntheses` | `kind` (analysis or brief), `limit` (20) | syntheses, newest first |
| `GET /syntheses/{id}` | | one synthesis |
| `GET /syntheses/{id}/html` | | the synthesis body as sanitised HTML |
| `GET /save` | `url`, `token` (query only here) | saves the page, then 303 to `/#/a/<id>` (see Quick save) |
| `GET /export` | | whole-library snapshot `{version, exported_at, articles, highlights, syntheses}` (see Backup & restore) |

## Web page

`http://<bind>/` serves a small page from the same server: library (save box, search, article list), reader
(highlights marked in the text; select text and tap Highlight, with an optional note), and syntheses. Plain JS and CSS
embedded from `web/`, no build step, nothing loaded from other hosts. On first visit it asks for the token and keeps it in
the browser's localStorage; a 401 sends you back to that prompt. There is no HTTPS, so there is no service worker or
PWA install: use it as a normal page (on a phone, "Add to Home Screen" gives a plain shortcut).

Markdown is rendered on the server (`src/render.rs`, `pulldown-cmark`): raw HTML in an article is shown as text, links
open in a new tab with `rel="noopener noreferrer"`, and links that are not http, https or mailto lose their `href`.
The page also ships a Content-Security-Policy that allows only its own script and style.

## Quick save

`GET /save?url=<url>&token=<token>` saves the page and redirects (303) to the reader. It exists because bookmarklets and
share sheets cannot set headers. It is the only route that takes the token in the query string, it is always checked when
a token is configured (constant-time compare), and the response never repeats it. Errors come back as a small HTML page.

Anything that opens such a URL leaves the token in that device's browser history and in the bookmark. Fine for a
personal Tailscale-only service; rotate `MARGINALIA_TOKEN` if a device is lost.

- **Desktop bookmarklet.** Make a bookmark whose URL is (replace `TOKEN`):
  ```
  javascript:location='http://100.103.153.13:7878/save?url='+encodeURIComponent(location.href)+'&token=TOKEN'
  ```
  The token is stored in the browser's bookmarks (and synced with them, if you sync).
- **iOS Shortcut** (no token in any URL). New shortcut, enable "Show in Share Sheet", accept URLs. Actions:
  1. Get Contents of URL: `http://100.103.153.13:7878/articles`, Method POST, Request Body JSON with a field `url`
     set to the Shortcut Input, header `Authorization` = `Bearer TOKEN`.
  2. Show Result (or Show Notification).

  Saving fetches and cleans the page, so allow a few seconds. The shortcut keeps the token inside the shortcut.
- **Android.** Install the HTTP Shortcuts app: create a shortcut with POST `http://100.103.153.13:7878/articles`,
  JSON body `{"url": "<shared text>"}` (insert the app's shared-text variable), header `Authorization: Bearer TOKEN`, and enable it as a
  share target. Or use the bookmarklet above in a browser that allows `javascript:` bookmarks (Firefox does; Chrome
  only from the address bar).

## MCP

Tools: `list_articles`, `get_article`, `search`, `list_highlights`, `add_highlight`, `save_url`, `save_markdown`,
`list_syntheses`, `get_synthesis`, `save_synthesis`, `analyse_article`, `write_brief`. Read and add only, no deletes.
Failures come back as tool errors.

- HTTP: `http://<bind>/mcp` (streamable HTTP) with header `Authorization: Bearer <token>`.
- stdio: command `marginalia`, args `["mcp"]`.

`skills/marginalia/SKILL.md` teaches a harness how to use the tools (search, read, cite, store products) and has
example client configs. Copy it into the harness's skills directory.

## Deploy (systemd user service)

```sh
nix-shell --run 'cargo build --release'
install -Dm755 target/release/marginalia ~/.local/bin/marginalia
install -d ~/.local/share/marginalia ~/.config/marginalia ~/.config/systemd/user
install -m600 deploy/env.example ~/.config/marginalia/env      # then edit: set MARGINALIA_TOKEN and LLM_API_KEY
install -m644 deploy/marginalia.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now marginalia
journalctl --user -u marginalia -f
```

Data lands in `~/.local/share/marginalia/data` (the unit's working directory plus `./data`). Lingering must be enabled
(`loginctl enable-linger $USER`) for the service to run without a login session.

## Backup & restore

`GET /export` returns the whole library as one JSON document that does not depend on SurrealDB, so it serves both
nightly backups and a later move to another database engine:

```json
{ "version": 1, "exported_at": "...", "articles": [...], "highlights": [...], "syntheses": [...] }
```

Rows keep their ids and timestamps. Blobs are not inlined: an article only carries its `blob_key`. The blob files are
immutable and content-addressed, so a backup just copies them. The usage log and chat sessions are not part of the
snapshot. The export is read through the ordinary `Repo` methods and is not a transaction, which is fine for a nightly job.
There is no REST import; restoring is a CLI action on a stopped server.

### Nightly backup (systemd user timer)

`deploy/marginalia-backup.sh` fetches `/export` (it reads `MARGINALIA_BIND` and `MARGINALIA_TOKEN` from the environment
or `~/.config/marginalia/env`), refuses an empty or invalid result, and writes
`~/.local/share/marginalia/backups/<YYYY-MM-DD>/library.json` plus a hard-linked copy of `data/blobs` in that directory's
`blobs/`. It keeps the newest 14 dated directories. Running it twice on the same day replaces that day's backup.

```sh
install -Dm755 deploy/marginalia-backup.sh ~/.local/bin/marginalia-backup
install -m644 deploy/marginalia-backup.service deploy/marginalia-backup.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now marginalia-backup.timer      # 03:30 daily, up to 10 minutes late; catches up after downtime
systemctl --user start marginalia-backup.service            # run one now
```

Hard links share storage with the live `data/blobs`; that is safe because blobs are never modified. They are not a
second copy against disk failure, so also sync `~/.local/share/marginalia/backups` somewhere else (another machine,
restic, rclone). Override locations with `MARGINALIA_DATA` (default `~/.local/share/marginalia/data`) and
`MARGINALIA_BACKUP_DIR`.

### Restore

`marginalia import` opens the data directory directly, so `serve` must be stopped (it holds the database lock), and it
only restores into an empty library: it refuses a non-empty one and writes nothing.

```sh
B=~/.local/share/marginalia/backups/2026-10-26                 # the backup to restore
D=~/.local/share/marginalia/data
systemctl --user stop marginalia
mv "$D/db" "$D/db.old-$(date +%F)"                              # keep the old database aside, do not delete it yet
marginalia --data-dir "$D" import "$B/library.json"             # creates a fresh db/ and loads the snapshot
mkdir -p "$D/blobs" && cp -an "$B/blobs/." "$D/blobs/"          # blobs are content-addressed: existing files are never replaced
systemctl --user start marginalia
curl -s -H "Authorization: Bearer $MARGINALIA_TOKEN" http://$MARGINALIA_BIND/articles | head -c 300   # spot check
```

If the import fails, nothing was written: fix the cause (or move `db/` aside again) and re-run. The environment
variable `MARGINALIA_DATA` can stand in for `--data-dir`.

## Off-site copy (pulled by a Mac)

Nightly backups stay on shared-server; a Mac on the tailnet pulls them hourly while awake (`deploy/mac/`).
No SSH key is needed: the tailnet policy gives bytetum's devices Tailscale SSH to shared-server as `bytetum` in "accept"
mode (no browser check), so the unattended pull works. Test: `ssh -o BatchMode=yes bytetum@shared-server true`.

```sh
# on the Mac, from a checkout of this repo (or copy the two files over)
install -m 755 deploy/mac/marginalia-pull.sh ~/.local/bin/marginalia-pull
cp deploy/mac/com.bytetum.marginalia-pull.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bytetum.marginalia-pull.plist
cat ~/Backups/marginalia/.last-pull   # time of the last successful pull; log in /tmp/marginalia-pull.log
```

## Highlights in kb

`deploy/marginalia-kb-sync.py` copies highlights into the kb home store every 15 minutes
(`marginalia-kb-sync.timer`): a `Marginalia highlights` node, one `#article` node per highlighted article (fields `url`,
`reader`), and one `#highlight` node per highlight under it (fields `note`, `highlighted`). Node ids come from Marginalia
ids (`mg-a-…`, `mg-h-…`), so it only ever adds: never duplicates, edits or deletes. Copied ids are remembered in
`~/.local/state/marginalia/kb-synced.txt`; anything you move, edit or delete in kb stays that way.

```sh
install -m 755 deploy/marginalia-kb-sync.py ~/.local/bin/marginalia-kb-sync
cp deploy/marginalia-kb-sync.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now marginalia-kb-sync.timer
journalctl --user -u marginalia-kb-sync -n 5   # "+N articles, +N highlights"
```
