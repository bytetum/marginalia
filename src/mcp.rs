//! MCP access layer: the same operations as REST, shaped for agents. Read and add only, no deletes.
//! Every tool calls `App`; failures come back as MCP tool errors (`is_error`), not protocol errors.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::domain::*;
use crate::service::{App, AppError};

const INSTRUCTIONS: &str = "Marginalia is the user's personal reading library: articles they saved, the passages they \
highlighted, and synthesis products (analyses and briefs) written about them. Always search the library before answering \
a question about it, then read the relevant articles with get_article, and cite article titles. Books (kind \"book\", \
uploaded EPUBs) are articles too but can be hundreds of thousands of characters: get_article returns them in pages \
(see its offset, max_chars and next_offset), so read the part you need, using search snippets to choose where. Never invent articles or \
quotes; if the library has nothing relevant, say so. Use add_highlight to record a passage the user wants to keep, and \
save_synthesis to store a finished analysis or brief worth keeping.";

#[derive(Clone)]
pub struct MarginaliaMcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
}

impl MarginaliaMcp {
    pub fn new(app: Arc<App>) -> Self {
        Self { app, tool_router: Self::tool_router() }
    }
}

/// Pretty JSON text for a tool result; failures become the tool error text.
fn json_text<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

/// Characters of a book that `get_article` returns per call unless told otherwise.
const BOOK_PAGE_CHARS: usize = 40_000;

/// The article with its body cut to `max_chars` characters from character `offset`, plus where that window sits:
/// `offset`, `total_chars` and `next_offset` (null when the window reaches the end).
fn page_of(article: &Article, offset: usize, max_chars: Option<usize>) -> serde_json::Value {
    let total = article.markdown.chars().count();
    let start = offset.min(total);
    let len = max_chars.unwrap_or(usize::MAX).min(total - start);
    let byte_at = |chars: usize| article.markdown.char_indices().nth(chars).map_or(article.markdown.len(), |(i, _)| i);
    let (from, to) = (byte_at(start), byte_at(start + len));
    let mut value = serde_json::to_value(article).unwrap_or_default();
    value["markdown"] = article.markdown[from..to].into();
    value["offset"] = start.into();
    value["total_chars"] = total.into();
    value["next_offset"] = if start + len < total { (start + len).into() } else { serde_json::Value::Null };
    value
}

fn fail(e: impl Into<AppError>) -> String {
    e.into().to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListArticlesArgs {
    /// Maximum number of articles to return (default 50).
    limit: Option<u32>,
    /// Number of articles to skip (default 0).
    offset: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IdArgs {
    /// The id.
    id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetArticleArgs {
    /// The article id.
    id: String,
    /// Character offset into the Markdown body to start at (default 0).
    offset: Option<usize>,
    /// Most characters of the body to return. Defaults to 40000 for books and to the whole body for web articles.
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    /// Keywords to look for in article titles, bodies and highlights.
    query: String,
    /// Maximum number of hits (default 10).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListHighlightsArgs {
    /// Only highlights of this article; omit for all highlights.
    article_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddHighlightArgs {
    /// The article the passage comes from.
    article_id: String,
    /// The highlighted passage, verbatim.
    text: String,
    /// Optional note about why it matters.
    note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SaveUrlArgs {
    /// http(s) URL of the page to save.
    url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SaveMarkdownArgs {
    /// Source URL (also the article's identity: saving the same URL again updates it).
    url: String,
    title: String,
    /// The article text as Markdown.
    markdown: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListSynthesesArgs {
    /// "analysis" or "brief"; omit for both.
    kind: Option<String>,
    /// Maximum number of results (default 20).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SaveSynthesisArgs {
    /// "analysis" (about one article) or "brief" (across the library).
    kind: String,
    title: String,
    /// The finished text, Markdown.
    body: String,
    /// The steer the user gave, if any.
    direction: Option<String>,
    /// Ids of the articles the synthesis drew on.
    article_ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AnalyseArgs {
    article_id: String,
    /// Optional one-line steer, e.g. "focus on the methodology".
    direction: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BriefArgs {
    /// Optional one-line steer, e.g. "what do my articles say about pricing?".
    direction: Option<String>,
}

#[tool_router]
impl MarginaliaMcp {
    #[tool(description = "List saved articles and books, newest first (id, url, title, kind \"web\" or \"book\", author, words, highlight_count; no body).")]
    async fn list_articles(&self, Parameters(a): Parameters<ListArticlesArgs>) -> Result<String, String> {
        let rows = self.app.repo().list_articles(a.limit.unwrap_or(50), a.offset.unwrap_or(0)).await.map_err(fail)?;
        json_text(&rows)
    }

    #[tool(description = "Read one article by id (Markdown body included). A web article comes back whole. A book (kind \"book\") \
comes back one page at a time: at most max_chars (default 40000) characters of the body from offset (default 0), with \
total_chars and next_offset (null at the end); call again with offset = next_offset to continue. Chapters start with a \
\"## \" heading.")]
    async fn get_article(&self, Parameters(a): Parameters<GetArticleArgs>) -> Result<String, String> {
        let Some(article) = self.app.repo().get_article(&a.id).await.map_err(fail)? else {
            return Err(format!("not found: no article with id {}", a.id));
        };
        if article.kind != KIND_BOOK && a.offset.is_none() && a.max_chars.is_none() {
            return json_text(&article);
        }
        json_text(&page_of(&article, a.offset.unwrap_or(0), a.max_chars.or((article.kind == KIND_BOOK).then_some(BOOK_PAGE_CHARS))))
    }

    #[tool(description = "Keyword search over article titles, bodies and highlights. Returns best hits first with snippets.")]
    async fn search(&self, Parameters(a): Parameters<SearchArgs>) -> Result<String, String> {
        let hits = self.app.repo().search(&a.query, a.limit.unwrap_or(10)).await.map_err(fail)?;
        json_text(&hits)
    }

    #[tool(description = "List highlights, oldest first. Pass article_id for one article, or omit for all.")]
    async fn list_highlights(&self, Parameters(a): Parameters<ListHighlightsArgs>) -> Result<String, String> {
        let rows = self.app.repo().list_highlights(a.article_id.as_deref()).await.map_err(fail)?;
        json_text(&rows)
    }

    #[tool(description = "Add a highlight (a verbatim passage, optional note) to an article.")]
    async fn add_highlight(&self, Parameters(a): Parameters<AddHighlightArgs>) -> Result<String, String> {
        let h = self.app.add_highlight(&a.article_id, &a.text, a.note.as_deref()).await.map_err(fail)?;
        json_text(&h)
    }

    #[tool(description = "Fetch a web page by URL, extract its text and save it as an article.")]
    async fn save_url(&self, Parameters(a): Parameters<SaveUrlArgs>) -> Result<String, String> {
        let article = self.app.save_url(&a.url).await.map_err(fail)?;
        json_text(&summary_of(&article))
    }

    #[tool(description = "Save an article from Markdown you already have (url, title, markdown).")]
    async fn save_markdown(&self, Parameters(a): Parameters<SaveMarkdownArgs>) -> Result<String, String> {
        let article = self.app.save_markdown(&a.url, &a.title, &a.markdown).await.map_err(fail)?;
        json_text(&summary_of(&article))
    }

    #[tool(description = "List synthesis products (analyses and briefs), newest first, with their bodies.")]
    async fn list_syntheses(&self, Parameters(a): Parameters<ListSynthesesArgs>) -> Result<String, String> {
        let kind = parse_kind(a.kind.as_deref())?;
        let rows = self.app.repo().list_syntheses(kind, a.limit.unwrap_or(20)).await.map_err(fail)?;
        json_text(&rows)
    }

    #[tool(description = "Read one synthesis product by id.")]
    async fn get_synthesis(&self, Parameters(a): Parameters<IdArgs>) -> Result<String, String> {
        match self.app.repo().get_synthesis(&a.id).await.map_err(fail)? {
            Some(s) => json_text(&s),
            None => Err(format!("not found: no synthesis with id {}", a.id)),
        }
    }

    #[tool(description = "Store a finished analysis or brief that you wrote, so it is kept and served to others. kind is \"analysis\" or \"brief\".")]
    async fn save_synthesis(&self, Parameters(a): Parameters<SaveSynthesisArgs>) -> Result<String, String> {
        let kind = parse_kind(Some(&a.kind))?.ok_or("kind is required: analysis or brief")?;
        if a.title.trim().is_empty() || a.body.trim().is_empty() {
            return Err("invalid input: title and body must not be blank".into());
        }
        let direction = a.direction.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        let s = self
            .app
            .repo()
            .save_synthesis(NewSynthesis { kind, title: a.title, direction, body: a.body, article_ids: a.article_ids })
            .await
            .map_err(fail)?;
        json_text(&s)
    }

    #[tool(description = "Have Marginalia's built-in agent write a deep analysis of one article (needs an LLM on the server). Saved as a synthesis.")]
    async fn analyse_article(&self, Parameters(a): Parameters<AnalyseArgs>) -> Result<String, String> {
        let s = self.app.analyse(&a.article_id, a.direction.as_deref()).await.map_err(fail)?;
        json_text(&s)
    }

    #[tool(description = "Have Marginalia's built-in agent write a brief across the whole library (needs an LLM on the server). Saved as a synthesis.")]
    async fn write_brief(&self, Parameters(a): Parameters<BriefArgs>) -> Result<String, String> {
        let s = self.app.brief(a.direction.as_deref()).await.map_err(fail)?;
        json_text(&s)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MarginaliaMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("marginalia", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

fn parse_kind(kind: Option<&str>) -> Result<Option<SynthesisKind>, String> {
    match kind.map(str::trim) {
        None | Some("") => Ok(None),
        Some(k) => SynthesisKind::parse(k).map(Some).ok_or_else(|| format!("invalid input: unknown kind {k:?}; use analysis or brief")),
    }
}

/// What `save_*` report back: enough to refer to the article, without echoing its whole body.
#[derive(Serialize)]
struct Saved<'a> {
    id: &'a str,
    url: &'a str,
    title: &'a str,
    words: u32,
    via: &'a str,
}

fn summary_of(a: &Article) -> Saved<'_> {
    Saved { id: &a.id, url: &a.url, title: &a.title, words: a.words, via: &a.via }
}

/// MCP over streamable HTTP, to be mounted at `/mcp` (see `api::router`). Authentication is the caller's
/// bearer middleware; the Host-header allow-list is disabled because the server is reached by its LAN or
/// Tailscale address, which rmcp's localhost-only default would reject.
pub fn http_service(app: Arc<App>) -> StreamableHttpService<MarginaliaMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(MarginaliaMcp::new(app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}

/// Serve MCP over stdin/stdout until the client disconnects. Nothing else may write to stdout.
pub async fn serve_stdio(app: Arc<App>) -> std::io::Result<()> {
    let running = MarginaliaMcp::new(app).serve(rmcp::transport::stdio()).await.map_err(std::io::Error::other)?;
    running.waiting().await.map_err(std::io::Error::other)?;
    Ok(())
}
