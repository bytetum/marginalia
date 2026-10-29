# Marginalia MVP — design contract

Built to the owner's whiteboard. Rust, single crate (lib + bin `marginalia`).

## Use cases
Store article · Highlight from sources · **Discuss with agent** (main) · Analyse article (human steers the direction) ·
Easy access (REST + our MCP) · Synthesis products served to others.

## Decisions (owner, 2026-10-25)
| Topic | Decision |
| --- | --- |
| Database | SurrealDB 3, embedded (`surrealkv`), behind `Repo` |
| Agent | Both: our MCP + skill for external harnesses, and a built-in loop for synthesis products |
| Steering | Optional one-line `direction` given up front; the run then completes and saves |
| Access | Bind to a configurable address (Tailscale/LAN), static bearer token on REST and MCP HTTP |
| MCP transports | Streamable HTTP at `/mcp` (same server as REST) **and** `marginalia mcp` over stdio |
| Search | Keyword only (SurrealDB full-text, BM25); vectors later behind the same `search()` |
| MCP scope | Read + add: read/search articles, highlights, syntheses; save_url; add_highlight; save_synthesis. No deletes |
| Built-in runs via MCP | Yes: `analyse_article` and `write_brief` tools call the built-in loop |
| Web page | Static `web/` files embedded in the binary, served at `/` without auth; data via the REST API with a bearer token in localStorage. Plain http, so no service worker/PWA. Markdown rendered server-side (`render.rs`, pulldown-cmark, raw HTML escaped) at `/articles/{id}/html`, `/syntheses/{id}/html`. ```mermaid fences are drawn client-side by a vendored `web/vendor/mermaid.min.js`, loaded lazily |
| Quick save | `GET /save?url=&token=` (token in the query only on this route, constant-time check) saves then 303s to `/#/a/<id>`; iOS Shortcut / Android use `POST /articles` |
| Deploy | systemd user service `marginalia` (linger already on), bound to Tailscale `100.103.153.13`, token in an env file |

## Architecture (ports and adapters)

```
 REST (api.rs)   MCP (mcp.rs)   CLI (main.rs)      <- access: thin, no business logic
        \             |             /
              service::App                          <- all use cases live here
          /        |          \
   agent.rs    extract.rs    Repo + BlobStore       <- built-in loop / Firecrawl+Readability / ports (repo.rs)
                               |         |
                      surreal.rs, mem.rs  blob.rs   <- adapters
```

- `domain.rs`, `repo.rs` are the contract. **Do not change their public items** without the lead's approval.
  Approved addition (backups): `domain::LibraryExport` / `EXPORT_VERSION`, `Repo::restore`, and in `repo.rs` the free
  functions `export_library` and `validate_export`. Existing items are unchanged.
- REST and MCP expose the *same* operations by calling `App`; MCP is "REST evolved for agents".
- External harnesses (opencode go, Codex, Hermes) discuss the library through our MCP + `skills/marginalia/SKILL.md`.
- The built-in loop (`agent.rs`) produces synthesis products (analyses, briefs) and powers CLI/REST chat.

## Data
- `data/db/` — embedded SurrealDB (`surrealkv` engine). Tests use the `mem` engine.
- `data/blobs/` — content-addressed raw files: `<sha256hex>.<ext>`, e.g. fetched HTML; later PDF/EPUB/images.

## Backups (engine-neutral snapshot)
- Format: `LibraryExport { version: 1, exported_at, articles, highlights, syntheses }` in `domain.rs`: plain JSON, ids and
  timestamps preserved, rows sorted oldest first (id as tie-break). Blobs are **not** inlined (`blob_key` only); the backup
  script copies the immutable, content-addressed files. Usage log and chat sessions are not exported.
- Export reads through the existing `Repo` methods (`repo::export_library`: paged `list_articles` + `get_article`,
  `list_highlights(None)`, `list_syntheses(None, u32::MAX)`). Not a transaction.
- Import needs ids and times preserved, which `upsert_article` and friends cannot do, so the contract gained one method:
  `Repo::restore(&LibraryExport)`. It requires an empty repo (articles, highlights, syntheses; the usage log does not count),
  validates with `repo::validate_export` (version, unique ids and urls, highlights point at exported articles), writes
  all-or-nothing, and returns `Invalid` otherwise. Both adapters implement it; `tests/repo_contract.rs` checks
  export, restore into a fresh repo, identical export.
- Access: `App::export()` / `App::import()`; `GET /export` (bearer token as usual); `marginalia import <file.json>` (CLI only,
  offline, opens the data dir so it fails while `serve` holds the lock). There is no REST or MCP import.
- Ops: `deploy/marginalia-backup.{sh,service,timer}` (nightly, 14 dated dirs, hard-linked blobs). See README.

## Service API (`src/service.rs`) — what api.rs, mcp.rs and main.rs call

```rust
pub struct LlmConfig { pub base_url: String, pub api_key: String, pub model: String }   // defined in agent.rs, re-exported
pub struct AppConfig { pub llm: Option<LlmConfig>, pub firecrawl_key: Option<String> }

#[derive(Debug, thiserror::Error)]
pub enum AppError { NotFound(String), Invalid(String), NoLlm, Upstream(String), Repo(RepoError) }
// From<RepoError> maps NotFound/Invalid to the matching AppError variant, Backend to Repo.

pub struct App { /* Arc<dyn Repo>, Arc<dyn BlobStore>, reqwest::Client, AppConfig, chat sessions */ }
impl App {
    pub fn new(repo: Arc<dyn Repo>, blobs: Arc<dyn BlobStore>, config: AppConfig) -> Self;
    pub fn repo(&self) -> &Arc<dyn Repo>;
    pub fn has_llm(&self) -> bool;
    pub async fn save_url(&self, url: &str) -> Result<Article, AppError>;            // extract, keep raw HTML blob when available, upsert, log usage
    pub async fn save_markdown(&self, url: &str, title: &str, markdown: &str) -> Result<Article, AppError>; // via = "manual"
    pub async fn add_highlight(&self, article_id: &str, text: &str, note: Option<&str>) -> Result<Highlight, AppError>;
    pub async fn chat(&self, session_id: &str, message: &str) -> Result<String, AppError>;   // NoLlm if no LLM configured
    pub fn reset_chat(&self, session_id: &str);
    pub async fn analyse(&self, article_id: &str, direction: Option<&str>) -> Result<Synthesis, AppError>; // saved as SynthesisKind::Analysis
    pub async fn brief(&self, direction: Option<&str>) -> Result<Synthesis, AppError>;                     // saved as SynthesisKind::Brief
    pub async fn export(&self) -> Result<LibraryExport, AppError>;                                         // whole-library snapshot, no blobs
    pub async fn import(&self, export: LibraryExport) -> Result<(), AppError>;                             // into an EMPTY library, else Invalid
}
```

## Prompts are files (the board's "rules, system prompt, skill")
`prompts/system.md` (rules), `prompts/analyse.md`, `prompts/brief.md`, loaded with `include_str!`. Editing a prompt needs no
code change beyond a rebuild.

## Conventions
- Errors: `anyhow` only in `main.rs`; library code uses `RepoError` / `AppError`.
- No `unwrap()` outside tests. No new dependencies without saying why in your report.
- Tests never touch the network or a real LLM: fake OpenAI-compatible server (axum) and `MemRepo`/Surreal `mem`.
- NixOS: build and test inside `nix-shell --run '...'` (shell.nix supplies rustup with the pinned toolchain in rust-toolchain.toml, plus gcc/cmake). `nix develop` (flake.nix) gives the same toolchain, and `nix build` / `nix flake check` build and test the package. Never use the system cargo: its rustc cannot compile SurrealDB. Stdin must be `</dev/null`.
- `cat` is aliased to `bat` in this shell; use the Read tool or `command cat`.
- Reference implementation of extraction and the agent loop: `../marginalia-poc/` (working, tested).
