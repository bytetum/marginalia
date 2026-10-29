//! REST access layer. Thin: every handler parses input, calls `App`, and serialises the result.
//! The MCP server is mounted at `/mcp` behind the same bearer-token middleware.

use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::domain::*;
use crate::mcp;
use crate::render::{escape_html, markdown_to_html};
use crate::repo::new_id;
use crate::service::{App, AppError};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SEARCH_LIMIT: u32 = 10;
const DEFAULT_SYNTHESIS_LIMIT: u32 = 20;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// Paths that never need the bearer token: the health check, the static web page (no data in it), and
/// `/save`, which checks its own `?token=` because bookmarklets and share sheets cannot set headers.
fn is_public(path: &str) -> bool {
    matches!(path, "/health" | "/" | "/app.js" | "/app.css" | "/save")
}

#[derive(Clone)]
struct SaveState {
    app: Arc<App>,
    token: Option<Arc<str>>,
}

/// REST routes, the web page, and the MCP endpoint at `/mcp`. With `token` set, everything except `GET /health`,
/// the static page files and `GET /save` (which checks `?token=` itself) needs `Authorization: Bearer <token>`.
pub fn router(app: Arc<App>, token: Option<String>) -> Router {
    let token: Option<Arc<str>> = token.map(Arc::from);
    let save = Router::new()
        .route("/save", get(save_link))
        .with_state(SaveState { app: app.clone(), token: token.clone() });
    Router::new()
        .route("/", get(index_page))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/health", get(health))
        .route("/articles", get(list_articles).post(save_article))
        .route("/articles/{id}", get(get_article).delete(delete_article))
        .route("/articles/{id}/html", get(article_html))
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
        .with_state(app.clone())
        .merge(save)
        .nest_service("/mcp", mcp::http_service(app))
        .layer(middleware::from_fn_with_state(token, require_bearer))
}

// ---------- auth ----------

async fn require_bearer(State(token): State<Option<Arc<str>>>, req: Request, next: Next) -> Response {
    let Some(token) = token else { return next.run(req).await };
    if req.method() == Method::GET && is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(p) if constant_time_eq(p.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => {
            let mut res = (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or invalid bearer token" }))).into_response();
            res.headers_mut().insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
            res
        }
    }
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

async fn get_article(State(app): Shared, Path(id): Path<String>) -> ApiResult<Article> {
    app.repo()
        .get_article(&id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("no article with id {id}")).into())
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
}

async fn add_highlight(
    State(app): Shared,
    Path(id): Path<String>,
    Body(b): Body<AddHighlight>,
) -> Result<(StatusCode, Json<Highlight>), ApiError> {
    let highlight = app.add_highlight(&id, &b.text, b.note.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(highlight)))
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

// ---------- web page ----------

/// Headers for the static files. The CSP allows only our own script and style, so even a sanitiser bug in the
/// rendered Markdown could not run script; images may come from anywhere because articles link them.
fn static_file(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: http: https:; \
                 connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
            ),
        ],
        body,
    )
        .into_response()
}

async fn index_page() -> Response {
    static_file("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    static_file("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> Response {
    static_file("text/css; charset=utf-8", APP_CSS)
}

/// Sanitised HTML for the article body (see `render`). A fragment, meant for `innerHTML`.
async fn article_html(State(app): Shared, Path(id): Path<String>) -> Result<Html<String>, ApiError> {
    let article = app
        .repo()
        .get_article(&id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no article with id {id}")))?;
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
/// takes the token in the query string. With a token configured it is always required, and the response never
/// echoes it. Failures are small HTML pages because the browser is navigating here.
async fn save_link(State(state): State<SaveState>, Params(q): Params<SaveLink>) -> Response {
    if let Some(expected) = &state.token {
        let presented = q.token.as_deref().unwrap_or("");
        if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return save_page(StatusCode::UNAUTHORIZED, "Missing or invalid token.");
        }
    }
    let Some(url) = q.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
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
