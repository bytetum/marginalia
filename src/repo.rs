//! Ports: the storage interfaces the rest of the app depends on.
//! Adapters: `mem::MemRepo` (tests, reference behaviour), `surreal::SurrealRepo` (real, embedded SurrealDB), `blob::FsBlobStore`.
//! Every Repo adapter must pass `tests/repo_contract.rs`.
//!
//! Backups: `export_library` reads everything through the ordinary read methods (no extra trait method);
//! `Repo::restore` is the one write path that keeps ids and timestamps, for loading an export into an empty repo.

use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::*;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("storage error: {0}")]
    Backend(String),
}

pub type RepoResult<T> = Result<T, RepoError>;

#[async_trait]
pub trait Repo: Send + Sync {
    /// Insert, or update in place when an article with the same `url` exists (its `id` is kept).
    async fn upsert_article(&self, new: NewArticle) -> RepoResult<Article>;
    async fn get_article(&self, id: &str) -> RepoResult<Option<Article>>;
    /// Newest first.
    async fn list_articles(&self, limit: u32, offset: u32) -> RepoResult<Vec<ArticleSummary>>;
    /// Also deletes the article's highlights. `NotFound` if missing.
    async fn delete_article(&self, id: &str) -> RepoResult<()>;

    /// `NotFound` if the article does not exist; `Invalid` if `text` is blank.
    async fn add_highlight(&self, new: NewHighlight) -> RepoResult<Highlight>;
    /// Oldest first. `None` = all highlights.
    async fn list_highlights(&self, article_id: Option<&str>) -> RepoResult<Vec<Highlight>>;

    /// Full-text search over article titles, bodies and highlight text. Best first.
    /// Blank queries return an empty list, never an error.
    async fn search(&self, query: &str, limit: u32) -> RepoResult<Vec<SearchHit>>;

    async fn save_synthesis(&self, new: NewSynthesis) -> RepoResult<Synthesis>;
    async fn get_synthesis(&self, id: &str) -> RepoResult<Option<Synthesis>>;
    /// Newest first, optionally filtered by kind.
    async fn list_syntheses(&self, kind: Option<SynthesisKind>, limit: u32) -> RepoResult<Vec<Synthesis>>;

    /// Append-only usage log, used to judge whether the tool gets used.
    async fn log_usage(&self, kind: &str, detail: Value) -> RepoResult<()>;

    /// Load a [`LibraryExport`] into an **empty** repo, keeping every id and timestamp exactly as exported
    /// (unlike `upsert_article` and friends, which generate them). All or nothing: on error nothing is written.
    /// `Invalid` if the repo already holds any article, highlight or synthesis (the usage log does not count),
    /// or if the export fails `validate_export`. The usage log is not part of an export and is not restored.
    async fn restore(&self, export: &LibraryExport) -> RepoResult<()>;
}

/// Raw files (fetched HTML, later PDF/EPUB/images). Content-addressed: same bytes, same key.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Returns the key, e.g. "<sha256 hex>.html".
    async fn put(&self, bytes: &[u8], extension: &str) -> RepoResult<String>;
    async fn get(&self, key: &str) -> RepoResult<Option<Vec<u8>>>;
}

/// Word count used by every adapter, so they agree.
pub fn count_words(markdown: &str) -> u32 {
    markdown.split_whitespace().count() as u32
}

/// New random id. All adapters use this format.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Articles are fetched this many at a time by `export_library`.
const EXPORT_PAGE: u32 = 200;

/// Snapshot of the whole library using only the ordinary read methods of `repo` (paged `list_articles` plus
/// `get_article`, `list_highlights(None)`, `list_syntheses(None, u32::MAX)`), so every adapter exports the same way.
/// Rows are sorted oldest first with the id as tie-break, so equal libraries give equal exports.
/// Not a transaction: a write racing the export may or may not be included, but highlights of articles
/// that are not in the export are dropped, so the result always passes `validate_export`.
pub async fn export_library(repo: &dyn Repo) -> RepoResult<LibraryExport> {
    let mut articles = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = 0u32;
    loop {
        let page = repo.list_articles(EXPORT_PAGE, offset).await?;
        let full = page.len() >= EXPORT_PAGE as usize;
        for summary in page {
            // A save landing mid-export shifts later pages, so the same row can show up twice.
            if !seen.insert(summary.id.clone()) {
                continue;
            }
            // Deleted since it was listed: gone from the snapshot too.
            if let Some(article) = repo.get_article(&summary.id).await? {
                articles.push(article);
            }
        }
        if !full {
            break;
        }
        offset = offset.saturating_add(EXPORT_PAGE);
    }
    let kept: HashSet<&str> = articles.iter().map(|a| a.id.as_str()).collect();
    let mut highlights = repo.list_highlights(None).await?;
    highlights.retain(|h| kept.contains(h.article_id.as_str()));
    let mut syntheses = repo.list_syntheses(None, u32::MAX).await?;

    articles.sort_by(|a, b| a.saved_at.cmp(&b.saved_at).then_with(|| a.id.cmp(&b.id)));
    highlights.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    syntheses.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    Ok(LibraryExport { version: EXPORT_VERSION, exported_at: chrono::Utc::now(), articles, highlights, syntheses })
}

/// Checks an export before `Repo::restore` writes it, shared by every adapter: supported `version`, no blank or
/// duplicate ids, no duplicate article urls, and every highlight points at an article in the export.
/// (A synthesis may cite article ids that no longer exist: the article may have been deleted since.)
pub fn validate_export(export: &LibraryExport) -> RepoResult<()> {
    let invalid = |m: String| Err(RepoError::Invalid(m));
    if export.version != EXPORT_VERSION {
        return invalid(format!("unsupported export version {} (this build reads version {EXPORT_VERSION})", export.version));
    }
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    for a in &export.articles {
        if a.id.is_empty() || !ids.insert(a.id.as_str()) {
            return invalid(format!("blank or duplicate article id {:?}", a.id));
        }
        if !urls.insert(a.url.as_str()) {
            return invalid(format!("duplicate article url {:?}", a.url));
        }
    }
    let mut hl_ids = HashSet::new();
    for h in &export.highlights {
        if h.id.is_empty() || !hl_ids.insert(h.id.as_str()) {
            return invalid(format!("blank or duplicate highlight id {:?}", h.id));
        }
        if !ids.contains(h.article_id.as_str()) {
            return invalid(format!("highlight {} points at article {}, which is not in the export", h.id, h.article_id));
        }
    }
    let mut syn_ids = HashSet::new();
    for s in &export.syntheses {
        if s.id.is_empty() || !syn_ids.insert(s.id.as_str()) {
            return invalid(format!("blank or duplicate synthesis id {:?}", s.id));
        }
    }
    Ok(())
}
