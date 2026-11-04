//! REST access layer. Thin: every handler parses input, calls `App`, and serialises the result.
//! The MCP server is mounted at `/mcp` behind the same auth middleware (bearer token, or a trusted Tailscale login).

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use include_dir::{Dir, include_dir};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::domain::*;
use crate::epub::percent_decode;
use crate::mcp;
use crate::render::{escape_html, markdown_to_html};
use crate::repo::new_id;
use crate::service::{App, AppError};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SEARCH_LIMIT: u32 = 10;
const DEFAULT_SYNTHESIS_LIMIT: u32 = 20;

/// Largest EPUB upload. Only `POST /books` gets this; every other route keeps axum's small default body limit.
const MAX_BOOK_BYTES: usize = 200 * 1024 * 1024;

/// The whole `web/` directory, compiled into the binary (see `build.rs`, which makes new files trigger a rebuild).
static WEB: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/web");

/// Paths that never need the bearer token: the health check, the static web files (no data in them: exactly the
/// paths that exist in the embedded `web/` table), and `/save`, which checks its own `?token=` because bookmarklets
/// and share sheets cannot set headers.
fn is_public(path: &str) -> bool {
    matches!(path, "/health" | "/save") || static_asset(path).is_some()
}

/// Origins the Marginalia desktop and mobile app's webview runs on: `tauri://localhost` (macOS, iOS, Linux) and
/// `http(s)://tauri.localhost` (Windows, Android). The app is a different origin from the server, so the browser
/// engine needs CORS to let its script read the API's answers. Nothing else is allowed: another website cannot read
/// the API from a browser, and since auth is a bearer header (never a cookie) there is no ambient credential to abuse.
const APP_ORIGINS: [&str; 3] = ["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(APP_ORIGINS.map(header::HeaderValue::from_static)))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, X_FILENAME])
        .max_age(std::time::Duration::from_secs(600))
}

/// How requests are authenticated. `token` is the static bearer token (`None` = open). `tailscale_logins` are the
/// `Tailscale-User-Login` values trusted as the owner: a request carrying one of them needs no token. Only pass them
/// when the listener is a Unix socket that nobody but the owner and tailscaled can open; over TCP any local account
/// could forge the header, so the TCP path passes an empty list.
#[derive(Clone, Default)]
pub struct Auth {
    pub token: Option<String>,
    pub tailscale_logins: Vec<String>,
}

/// `Auth` as the middleware and `/save` use it: the token shared, logins trimmed and lower-cased (blanks dropped).
#[derive(Clone)]
struct AuthState {
    token: Option<Arc<str>>,
    logins: Arc<[String]>,
}

impl From<Auth> for AuthState {
    fn from(auth: Auth) -> Self {
        let logins =
            auth.tailscale_logins.iter().map(|l| l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty()).collect();
        Self { token: auth.token.map(Arc::from), logins }
    }
}

/// How a request got in; the middleware stores it as a request extension and `GET /me` reports it.
#[derive(Clone)]
enum Granted {
    /// No token is configured.
    Open,
    Token,
    Tailscale(String),
}

/// Original file name of an uploaded book, URL-encoded.
const X_FILENAME: HeaderName = HeaderName::from_static("x-filename");

/// Set by tailscaled on requests from personal (untagged) devices; it removes any client-sent copy.
const TAILSCALE_LOGIN: HeaderName = HeaderName::from_static("tailscale-user-login");

#[derive(Clone)]
struct SaveState {
    app: Arc<App>,
    auth: AuthState,
}

/// REST routes, the web files, and the MCP endpoint at `/mcp`. With a token set, everything except `GET /health`,
/// the static web files and `GET /save` (which checks `?token=` itself) needs `Authorization: Bearer <token>` or a
/// trusted `Tailscale-User-Login` (see `Auth`). CORS is open to the app's own origins only (see `APP_ORIGINS`).
pub fn router(app: Arc<App>, auth: Auth) -> Router {
    let auth = AuthState::from(auth);
    let save =
        Router::new().route("/save", get(save_link)).with_state(SaveState { app: app.clone(), auth: auth.clone() });
    Router::new()
        .route("/health", get(health))
        .route("/me", get(me))
        .route("/articles", get(list_articles).post(save_article))
        .route("/articles/{id}", get(get_article).delete(delete_article))
        .route("/books", post(upload_book).layer(DefaultBodyLimit::max(MAX_BOOK_BYTES)))
        .route("/articles/{id}/html", get(article_html))
        .route("/articles/{id}/file", get(article_file))
        .route("/articles/{id}/progress", get(get_progress).put(put_progress))
        .route("/articles/{id}/highlights", get(article_highlights).post(add_highlight))
        .route("/articles/{id}/analyse", post(analyse))
        .route("/highlights", get(all_highlights))
        .route("/search", get(search))
        .route("/chat", post(chat))
        .route("/chat/{session_id}", axum::routing::delete(reset_chat))
        .route("/briefs", post(brief))
        .route("/syntheses", get(list_syntheses))
        .route("/syntheses/{id}", get(get_synthesis))
        .route("/syntheses/{id}/html", get(synthesis_html))
        .route("/export", get(export))
        .fallback(static_files)
        .with_state(app.clone())
        .merge(save)
        .nest_service("/mcp", mcp::http_service(app))
        .layer(middleware::from_fn_with_state(auth, require_auth))
        // Outermost, so a preflight (which carries no Authorization header) is answered before the token check.
        .layer(cors())
}

// ---------- auth ----------

async fn require_auth(State(auth): State<AuthState>, mut req: Request, next: Next) -> Response {
    if auth.token.is_none() {
        req.extensions_mut().insert(Granted::Open);
        return next.run(req).await;
    }
    if req.method() == Method::GET && is_public(req.uri().path()) {
        return next.run(req).await;
    }
    match authorize(&auth, req.headers()) {
        // The Tailscale login is an ambient credential, which a bearer token is not: a page on another site could make
        // the owner's browser send it. Browsers label such requests, so refuse the cross-site ones that change state.
        Some(Granted::Tailscale(_)) if !req.method().is_safe() && is_cross_site(req.headers()) => {
            (StatusCode::FORBIDDEN, Json(json!({ "error": "cross-site request refused; use the bearer token" })))
                .into_response()
        }
        Some(granted) => {
            req.extensions_mut().insert(granted);
            next.run(req).await
        }
        None => {
            let mut res =
                (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or invalid bearer token" }))).into_response();
            res.headers_mut().insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
            res
        }
    }
}

/// A valid bearer token always works; failing that, a trusted Tailscale login. Call only with a token configured.
fn authorize(auth: &AuthState, headers: &HeaderMap) -> Option<Granted> {
    let token = auth.token.as_deref()?;
    let presented =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if presented.is_some_and(|p| constant_time_eq(p.as_bytes(), token.as_bytes())) {
        return Some(Granted::Token);
    }
    tailscale_login(auth, headers).map(Granted::Tailscale)
}

/// The `Tailscale-User-Login` value if it is exactly one header and names one of the trusted logins (ASCII
/// case-insensitive, trimmed). Always `None` with no trusted logins.
fn tailscale_login(auth: &AuthState, headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(TAILSCALE_LOGIN).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let login = value.to_str().ok()?.trim();
    auth.logins.iter().any(|l| l.eq_ignore_ascii_case(login)).then(|| login.to_string())
}

/// A browser says `Sec-Fetch-Site: cross-site` (or `same-site`, another host under the same domain) when a page from
/// elsewhere sent the request. The app's own origins are the exception: its webview is a different origin by design.
fn is_cross_site(headers: &HeaderMap) -> bool {
    let from = |name: HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    matches!(from(HeaderName::from_static("sec-fetch-site")), Some("cross-site" | "same-site"))
        && !from(header::ORIGIN).is_some_and(|o| APP_ORIGINS.contains(&o))
}

/// Compares every byte of the longer input, so timing does not reveal how much of a guess matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

// ---------- errors and extractors ----------

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        let status = match &e {
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Invalid(_) => StatusCode::BAD_REQUEST,
            AppError::NoLlm => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Repo(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self { status, message: e.to_string() }
    }
}

impl From<crate::repo::RepoError> for ApiError {
    fn from(e: crate::repo::RepoError) -> Self {
        AppError::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError { status: StatusCode::BAD_REQUEST, message: message.into() }
}

/// `Json` whose rejection is our `{error}` body with 400.
struct Body<T>(T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Body(v)),
            Err(e) => Err(bad_request(e.body_text())),
        }
    }
}

/// `Query` whose rejection is our `{error}` body with 400.
struct Params<T>(T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Params(v)),
            Err(e) => Err(bad_request(e.body_text())),
        }
    }
}

type Shared = State<Arc<App>>;
type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------- handlers ----------

#[derive(Serialize)]
struct Health {
    ok: bool,
    llm: bool,
}

async fn health(State(app): Shared) -> Json<Health> {
    Json(Health { ok: true, llm: app.has_llm() })
}

/// Who the caller is, so a client can tell whether it is already signed in without sending a token.
async fn me(Extension(granted): Extension<Granted>) -> Json<serde_json::Value> {
    Json(match granted {
        Granted::Open => json!({ "auth": "none", "login": null }),
        Granted::Token => json!({ "auth": "token", "login": null }),
        Granted::Tailscale(login) => json!({ "auth": "tailscale", "login": login }),
    })
}

#[derive(Deserialize)]
struct Page {
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_articles(State(app): Shared, Params(p): Params<Page>) -> ApiResult<Vec<ArticleSummary>> {
    let rows = app.repo().list_articles(p.limit.unwrap_or(DEFAULT_LIST_LIMIT), p.offset.unwrap_or(0)).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct SaveArticle {
    url: String,
    title: Option<String>,
    markdown: Option<String>,
}

async fn save_article(State(app): Shared, Body(b): Body<SaveArticle>) -> Result<(StatusCode, Json<Article>), ApiError> {
    let article = match (b.title, b.markdown) {
        (Some(title), Some(markdown)) => app.save_markdown(&b.url, &title, &markdown).await?,
        (None, None) => app.save_url(&b.url).await?,
        _ => return Err(bad_request("send either {url}, or {url, title, markdown}")),
    };
    Ok((StatusCode::CREATED, Json(article)))
}

#[derive(Deserialize)]
struct ArticleQuery {
    /// `false` leaves `markdown` empty: the web page only needs the metadata, and a book's text runs to megabytes.
    body: Option<bool>,
}

async fn get_article(State(app): Shared, Path(id): Path<String>, Params(q): Params<ArticleQuery>) -> ApiResult<Article> {
    let mut article =
        app.repo().get_article(&id).await?.ok_or_else(|| AppError::NotFound(format!("no article with id {id}")))?;
    if q.body == Some(false) {
        article.markdown.clear();
    }
    Ok(Json(article))
}

async fn delete_article(State(app): Shared, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    app.repo().delete_article(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn article_highlights(State(app): Shared, Path(id): Path<String>) -> ApiResult<Vec<Highlight>> {
    Ok(Json(app.repo().list_highlights(Some(&id)).await?))
}

#[derive(Deserialize)]
struct AddHighlight {
    text: String,
    note: Option<String>,
    anchor: Option<String>,
    location: Option<String>,
}

async fn add_highlight(
    State(app): Shared,
    Path(id): Path<String>,
    Body(b): Body<AddHighlight>,
) -> Result<(StatusCode, Json<Highlight>), ApiError> {
    let highlight = app.add_highlight_at(&id, &b.text, b.note.as_deref(), b.anchor.as_deref(), b.location.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(highlight)))
}

/// `POST /books`: the body is the raw EPUB. 201 for a new book, 200 with the existing one for the same bytes again.
async fn upload_book(
    State(app): Shared,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<ArticleSummary>), ApiError> {
    let filename = headers.get(X_FILENAME).and_then(|v| v.to_str().ok()).map(percent_decode);
    let (summary, created) = app.save_book(Vec::from(body), filename.as_deref()).await?;
    Ok((if created { StatusCode::CREATED } else { StatusCode::OK }, Json(summary)))
}

/// The stored EPUB. Content-addressed, so the browser may keep it forever.
async fn article_file(State(app): Shared, Path(id): Path<String>) -> Result<Response, ApiError> {
    let bytes = app.book_file(&id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/epub+zip"),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

/// `null` until the reader has reported a position.
async fn get_progress(State(app): Shared, Path(id): Path<String>) -> ApiResult<Option<Progress>> {
    Ok(Json(app.repo().get_progress(&id).await?))
}

#[derive(Deserialize)]
struct PutProgress {
    position: String,
    fraction: f64,
}

async fn put_progress(State(app): Shared, Path(id): Path<String>, Body(b): Body<PutProgress>) -> ApiResult<Progress> {
    Ok(Json(app.repo().put_progress(&id, &b.position, b.fraction).await?))
}

async fn all_highlights(State(app): Shared) -> ApiResult<Vec<Highlight>> {
    Ok(Json(app.repo().list_highlights(None).await?))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    limit: Option<u32>,
}

async fn search(State(app): Shared, Params(p): Params<SearchQuery>) -> ApiResult<Vec<SearchHit>> {
    Ok(Json(app.repo().search(&p.q, p.limit.unwrap_or(DEFAULT_SEARCH_LIMIT)).await?))
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: Option<String>,
    message: String,
}

#[derive(Serialize)]
struct ChatReply {
    session_id: String,
    reply: String,
}

async fn chat(State(app): Shared, Body(b): Body<ChatRequest>) -> ApiResult<ChatReply> {
    let session_id = b.session_id.filter(|s| !s.trim().is_empty()).unwrap_or_else(new_id);
    let reply = app.chat(&session_id, &b.message).await?;
    Ok(Json(ChatReply { session_id, reply }))
}

async fn reset_chat(State(app): Shared, Path(session_id): Path<String>) -> StatusCode {
    app.reset_chat(&session_id);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize, Default)]
struct DirectionBody {
    direction: Option<String>,
}

/// The body is optional for the run endpoints: an empty body means no direction.
async fn optional_direction(req: Request) -> Result<Option<String>, ApiError> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024).await.map_err(|e| bad_request(e.to_string()))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let body: DirectionBody = serde_json::from_slice(&bytes).map_err(|e| bad_request(e.to_string()))?;
    Ok(body.direction)
}

async fn analyse(State(app): Shared, Path(id): Path<String>, req: Request) -> ApiResult<Synthesis> {
    let direction = optional_direction(req).await?;
    Ok(Json(app.analyse(&id, direction.as_deref()).await?))
}

async fn brief(State(app): Shared, req: Request) -> ApiResult<Synthesis> {
    let direction = optional_direction(req).await?;
    Ok(Json(app.brief(direction.as_deref()).await?))
}

#[derive(Deserialize)]
struct SynthesisQuery {
    kind: Option<String>,
    limit: Option<u32>,
}

async fn list_syntheses(State(app): Shared, Params(p): Params<SynthesisQuery>) -> ApiResult<Vec<Synthesis>> {
    let kind = match p.kind.as_deref() {
        None | Some("") => None,
        Some(k) => Some(SynthesisKind::parse(k).ok_or_else(|| bad_request(format!("unknown kind {k:?}; use analysis or brief")))?),
    };
    Ok(Json(app.repo().list_syntheses(kind, p.limit.unwrap_or(DEFAULT_SYNTHESIS_LIMIT)).await?))
}

async fn get_synthesis(State(app): Shared, Path(id): Path<String>) -> ApiResult<Synthesis> {
    app.repo()
        .get_synthesis(&id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("no synthesis with id {id}")).into())
}

/// Whole-library JSON snapshot for backups. There is deliberately no REST import: restoring is offline, via the CLI.
async fn export(State(app): Shared) -> ApiResult<LibraryExport> {
    Ok(Json(app.export().await?))
}

// ---------- web files ----------

/// A file of the embedded `web/` directory and its content type. `/` is `index.html`. Hidden files (a segment that
/// starts with a dot, so also `..`) and directories are never served.
fn static_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    let rel = if path == "/" { "index.html" } else { path.strip_prefix('/')? };
    if rel.split('/').any(|segment| segment.is_empty() || segment.starts_with('.')) {
        return None;
    }
    Some((content_type(rel), WEB.get_file(rel)?.contents()))
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Headers for the static files. The CSP keeps `script-src 'self'` (no `blob:`, no inline), so script inside an
/// EPUB cannot run even though the reader renders each book section in a `blob:` iframe, which inherits this policy;
/// styles, images and fonts are opened up because books carry their own. Images may come from anywhere because
/// articles link them. `frame-ancestors` is `'self'`, not `'none'`: WebKit enforces the inherited policy on those
/// `blob:` sections too, so `'none'` stops Safari from showing any page of a book.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline' blob:; \
                   img-src 'self' data: blob: http: https:; font-src 'self' data: blob:; frame-src 'self' blob:; \
                   connect-src 'self' blob:; base-uri 'none'; form-action 'self'; frame-ancestors 'self'";

fn static_file(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, CSP),
        ],
        body,
    )
        .into_response()
}

/// Everything no route matched: a file from `web/` on GET, else 404.
async fn static_files(req: Request) -> Response {
    match (req.method(), static_asset(req.uri().path())) {
        (&Method::GET, Some((content_type, body))) => static_file(content_type, body),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Sanitised HTML for the article body (see `render`). A fragment, meant for `innerHTML`.
async fn article_html(State(app): Shared, Path(id): Path<String>) -> Result<Html<String>, ApiError> {
    let article = app
        .repo()
        .get_article(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no article with id {id}")))?;
    // A book is read from its EPUB (`/articles/{id}/file`); rendering the whole text here would be megabytes of HTML.
    if article.kind == "book" {
        return Err(AppError::NotFound(format!("{id} is a book: read it from /articles/{id}/file")).into());
    }
    Ok(Html(markdown_to_html(&article.markdown)))
}

async fn synthesis_html(State(app): Shared, Path(id): Path<String>) -> Result<Html<String>, ApiError> {
    let synthesis = app
        .repo()
        .get_synthesis(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no synthesis with id {id}")))?;
    Ok(Html(markdown_to_html(&synthesis.body)))
}

// ---------- quick save from a bookmarklet or share sheet ----------

#[derive(Deserialize)]
struct SaveLink {
    url: Option<String>,
    token: Option<String>,
}

/// `GET /save?url=<u>&token=<t>`: saves the page, then redirects (303) to the reader. This is the only route that
/// takes the token in the query string. With a token configured it is required unless the request carries a trusted
/// `Tailscale-User-Login` (a device signed in as the owner needs no token), and the response never echoes it.
/// Failures are small HTML pages because the browser is navigating here.
///
/// The login is an ambient credential and this GET changes state, so a page on another site could otherwise save
/// (and make the server fetch) any URL through the owner's browser, with an `<img src=/save?url=...>` for example.
/// A cross-site request let in by the login alone therefore saves nothing: it gets a page with a Save button whose
/// same-origin submit is let through. The token, and requests the owner starts (typed, a bookmark, a share sheet,
/// the app's own pages), still save at once.
async fn save_link(State(state): State<SaveState>, headers: HeaderMap, Params(q): Params<SaveLink>) -> Response {
    let url = q.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    if let Some(expected) = &state.auth.token {
        let presented = q.token.as_deref().unwrap_or("");
        if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            if tailscale_login(&state.auth, &headers).is_none() {
                return save_page(StatusCode::UNAUTHORIZED, "Missing or invalid token.");
            }
            if is_cross_site(&headers) {
                return match url {
                    Some(url) => confirm_save_page(url),
                    None => save_page(StatusCode::BAD_REQUEST, "Missing url parameter."),
                };
            }
        }
    }
    let Some(url) = url else {
        return save_page(StatusCode::BAD_REQUEST, "Missing url parameter.");
    };
    match state.app.save_url(url).await {
        Ok(article) => {
            let mut res = Redirect::to(&format!("/#/a/{}", article.id)).into_response();
            no_store(&mut res);
            res
        }
        Err(e) => {
            let err = ApiError::from(e);
            save_page(err.status, &err.message)
        }
    }
}

fn no_store(res: &mut Response) {
    let headers = res.headers_mut();
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
}

/// Asks before saving a URL that another site sent here (see `save_link`). The form submits from this origin, so the
/// browser labels that request same-origin and it saves.
fn confirm_save_page(url: &str) -> Response {
    let url = escape_html(url);
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Save to Marginalia?</title><style>body{{font:16px system-ui,sans-serif;max-width:32rem;margin:2rem auto;\
         padding:0 1rem}}p.u{{overflow-wrap:anywhere;color:#555}}button{{font:inherit;padding:.6rem 1.2rem}}</style>\
         <h1>Save this page?</h1><p>Another site asked Marginalia to save:</p><p class=\"u\">{url}</p>\
         <form method=\"get\" action=\"/save\"><input type=\"hidden\" name=\"url\" value=\"{url}\">\
         <button type=\"submit\">Save</button></form><p><a href=\"/\">Open the library</a></p>"
    );
    let mut res = (StatusCode::OK, Html(body)).into_response();
    no_store(&mut res);
    res
}

fn save_page(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Marginalia</title><p>Could not save: {}</p><p><a href=\"/\">Open the library</a></p>",
        escape_html(message)
    );
    let mut res = (status, Html(body)).into_response();
    no_store(&mut res);
    res
}
