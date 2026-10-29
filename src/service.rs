//! Application service: every use case lives here. REST, MCP and the CLI are thin callers of `App`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::{Agent, RunOutcome, new_conversation};
pub use crate::agent::LlmConfig;
use crate::domain::*;
use crate::extract::extract;
use crate::repo::{BlobStore, Repo, RepoError, export_library};

const ANALYSE_PROMPT: &str = include_str!("../prompts/analyse.md");
const BRIEF_PROMPT: &str = include_str!("../prompts/brief.md");

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub llm: Option<LlmConfig>,
    pub firecrawl_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("no LLM configured")]
    NoLlm,
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error(transparent)]
    Repo(RepoError),
}

impl From<RepoError> for AppError {
    fn from(e: RepoError) -> Self {
        match e {
            RepoError::NotFound(m) => AppError::NotFound(m),
            RepoError::Invalid(m) => AppError::Invalid(m),
            other @ RepoError::Backend(_) => AppError::Repo(other),
        }
    }
}

pub struct App {
    repo: Arc<dyn Repo>,
    blobs: Arc<dyn BlobStore>,
    client: reqwest::Client,
    config: AppConfig,
    /// Chat sessions: full message lists (system prompt first). Never held across an `.await`.
    sessions: Mutex<HashMap<String, Vec<Value>>>,
}

impl App {
    pub fn new(repo: Arc<dyn Repo>, blobs: Arc<dyn BlobStore>, config: AppConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) marginalia/0.1")
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self { repo, blobs, client, config, sessions: Mutex::new(HashMap::new()) }
    }

    pub fn repo(&self) -> &Arc<dyn Repo> {
        &self.repo
    }

    pub fn has_llm(&self) -> bool {
        self.config.llm.is_some()
    }

    /// Extract the page (Firecrawl if a key is set, else Readability), keep the raw HTML as a blob when
    /// the Readability path fetched it, upsert by URL, and log usage.
    pub async fn save_url(&self, url: &str) -> Result<Article, AppError> {
        save_url_with(&self.client, &*self.repo, &*self.blobs, self.config.firecrawl_key.as_deref(), url, "user").await
    }

    pub async fn save_markdown(&self, url: &str, title: &str, markdown: &str) -> Result<Article, AppError> {
        let (url, title) = (url.trim(), title.trim());
        if url.is_empty() {
            return Err(AppError::Invalid("url is blank".into()));
        }
        if title.is_empty() {
            return Err(AppError::Invalid("title is blank".into()));
        }
        if markdown.trim().is_empty() {
            return Err(AppError::Invalid("markdown is blank".into()));
        }
        let article = self
            .repo
            .upsert_article(NewArticle { url: url.into(), title: title.into(), markdown: markdown.into(), via: "manual".into(), blob_key: None })
            .await?;
        self.log("save", json!({ "id": article.id, "via": "manual", "by": "user" })).await;
        Ok(article)
    }

    pub async fn add_highlight(&self, article_id: &str, text: &str, note: Option<&str>) -> Result<Highlight, AppError> {
        if article_id.trim().is_empty() {
            return Err(AppError::Invalid("article_id is blank".into()));
        }
        if text.trim().is_empty() {
            return Err(AppError::Invalid("highlight text is blank".into()));
        }
        let note = note.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
        let highlight = self
            .repo
            .add_highlight(NewHighlight { article_id: article_id.trim().into(), text: text.into(), note })
            .await?;
        self.log("highlight", json!({ "id": highlight.id, "article_id": highlight.article_id, "by": "user" })).await;
        Ok(highlight)
    }

    /// Discuss the library with the built-in agent. Each session keeps its conversation in memory.
    /// Two simultaneous calls on the same session do not serialise: the later one to finish wins.
    pub async fn chat(&self, session_id: &str, message: &str) -> Result<String, AppError> {
        let agent = self.agent()?;
        if message.trim().is_empty() {
            return Err(AppError::Invalid("message is blank".into()));
        }
        // Take the history out so no std Mutex guard lives across the await.
        let taken = self.sessions().remove(session_id);
        let mut messages = taken.unwrap_or_else(new_conversation);
        let checkpoint = messages.len();
        let result = agent.run(&mut messages, message).await;
        if result.is_err() {
            // Drop the half-finished exchange so the session stays valid for the next message.
            messages.truncate(checkpoint);
        }
        self.sessions().insert(session_id.to_string(), messages);
        let outcome = result?;
        self.log("chat", json!({ "session": session_id, "articles_read": outcome.articles_read.len() })).await;
        Ok(outcome.answer)
    }

    pub fn reset_chat(&self, session_id: &str) {
        self.sessions().remove(session_id);
    }

    /// Deep read of one article. A fresh agent run (no session); the result is saved as an Analysis.
    pub async fn analyse(&self, article_id: &str, direction: Option<&str>) -> Result<Synthesis, AppError> {
        let agent = self.agent()?;
        let article = self
            .repo
            .get_article(article_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no article with id {article_id}")))?;
        let direction = clean(direction);
        let prompt = ANALYSE_PROMPT
            .replace("{{article_id}}", &article.id)
            .replace("{{title}}", &article.title)
            .replace("{{direction}}", &direction_text(direction.as_deref()));
        let outcome = fresh_run(&agent, &prompt).await?;
        let mut article_ids = outcome.articles_read;
        if !article_ids.contains(&article.id) {
            article_ids.insert(0, article.id.clone());
        }
        let synthesis = self
            .repo
            .save_synthesis(NewSynthesis {
                kind: SynthesisKind::Analysis,
                title: format!("Analysis: {}", article.title),
                direction: direction.clone(),
                body: outcome.answer,
                article_ids,
            })
            .await?;
        self.log("analyse", json!({ "id": synthesis.id, "article_id": article.id, "has_direction": direction.is_some() })).await;
        Ok(synthesis)
    }

    /// Cross-library synthesis. A fresh agent run; the result is saved as a Brief.
    pub async fn brief(&self, direction: Option<&str>) -> Result<Synthesis, AppError> {
        let agent = self.agent()?;
        let direction = clean(direction);
        let prompt = BRIEF_PROMPT.replace("{{direction}}", &direction_text(direction.as_deref()));
        let outcome = fresh_run(&agent, &prompt).await?;
        let title = match &direction {
            Some(d) => format!("Brief: {d}"),
            None => format!("Brief: {}", chrono::Utc::now().format("%Y-%m-%d")),
        };
        let synthesis = self
            .repo
            .save_synthesis(NewSynthesis {
                kind: SynthesisKind::Brief,
                title,
                direction: direction.clone(),
                body: outcome.answer,
                article_ids: outcome.articles_read,
            })
            .await?;
        self.log("brief", json!({ "id": synthesis.id, "articles": synthesis.article_ids.len(), "has_direction": direction.is_some() })).await;
        Ok(synthesis)
    }

    /// Engine-neutral snapshot of the whole library (articles, highlights, syntheses; blobs stay as keys).
    /// Read-only and safe while serving, but not a transaction.
    pub async fn export(&self) -> Result<LibraryExport, AppError> {
        Ok(export_library(&*self.repo).await?)
    }

    /// Load a snapshot into an empty library, keeping ids and timestamps. `Invalid` if the library already
    /// holds data or the snapshot is malformed or from an unsupported version. Nothing is written on error.
    pub async fn import(&self, export: LibraryExport) -> Result<(), AppError> {
        self.repo.restore(&export).await?;
        self.log(
            "import",
            json!({ "articles": export.articles.len(), "highlights": export.highlights.len(), "syntheses": export.syntheses.len() }),
        )
        .await;
        Ok(())
    }

    fn agent(&self) -> Result<Agent, AppError> {
        let llm = self.config.llm.clone().ok_or(AppError::NoLlm)?;
        Ok(Agent::new(self.client.clone(), llm, self.repo.clone(), self.blobs.clone(), self.config.firecrawl_key.clone()))
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Vec<Value>>> {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Usage logging is best-effort: it must never fail the operation it describes.
    async fn log(&self, kind: &str, detail: Value) {
        let _ = self.repo.log_usage(kind, detail).await;
    }
}

/// Shared by `App::save_url` and the agent's `save_url` tool. `by` is "user" or "agent".
pub(crate) async fn save_url_with(
    client: &reqwest::Client,
    repo: &dyn Repo,
    blobs: &dyn BlobStore,
    firecrawl_key: Option<&str>,
    url: &str,
    by: &str,
) -> Result<Article, AppError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(AppError::Invalid("url is blank".into()));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AppError::Invalid("url must start with http:// or https://".into()));
    }
    let got = extract(client, url, firecrawl_key).await.map_err(|e| AppError::Upstream(e.to_string()))?;
    let blob_key = match &got.html {
        Some(html) => Some(blobs.put(html.as_bytes(), "html").await?),
        None => None,
    };
    let article = repo
        .upsert_article(NewArticle { url: url.into(), title: got.title, markdown: got.markdown, via: got.via.as_str().into(), blob_key })
        .await?;
    let _ = repo.log_usage("save", json!({ "id": article.id, "via": article.via, "by": by })).await;
    Ok(article)
}

/// Run the loop on a one-off conversation. A run that hit the step limit or ended blank is an error,
/// because its text is not worth saving as a synthesis.
async fn fresh_run(agent: &Agent, prompt: &str) -> Result<RunOutcome, AppError> {
    let mut messages = new_conversation();
    let outcome = agent.run(&mut messages, prompt).await?;
    if !outcome.completed {
        return Err(AppError::Upstream("the agent ran out of steps before finishing".into()));
    }
    if outcome.answer.trim().is_empty() {
        return Err(AppError::Upstream("the model returned an empty answer".into()));
    }
    Ok(outcome)
}

fn clean(direction: Option<&str>) -> Option<String> {
    direction.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string)
}

fn direction_text(direction: Option<&str>) -> String {
    match direction {
        Some(d) => format!("My direction for this, which should shape what you focus on: \"{d}\""),
        None => "I have no particular direction; give a balanced treatment.".into(),
    }
}
