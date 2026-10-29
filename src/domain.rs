//! Domain types shared by every layer. No storage or transport types leak in here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A saved article. `markdown` is the cleaned text the agent reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Article {
    pub id: String,
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub words: u32,
    /// How it was extracted: "firecrawl" | "readability" | "manual".
    pub via: String,
    /// Key of the raw page in the blob store, if kept.
    pub blob_key: Option<String>,
    pub saved_at: DateTime<Utc>,
}

/// Article without its body, for lists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArticleSummary {
    pub id: String,
    pub url: String,
    pub title: String,
    pub words: u32,
    pub via: String,
    pub saved_at: DateTime<Utc>,
    pub highlight_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewArticle {
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub via: String,
    pub blob_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Highlight {
    pub id: String,
    pub article_id: String,
    pub text: String,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHighlight {
    pub article_id: String,
    pub text: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisKind {
    /// Deep read of one article.
    Analysis,
    /// Cross-library brief, optionally steered by a direction.
    Brief,
}

impl SynthesisKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SynthesisKind::Analysis => "analysis",
            SynthesisKind::Brief => "brief",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "analysis" => Some(SynthesisKind::Analysis),
            "brief" => Some(SynthesisKind::Brief),
            _ => None,
        }
    }
}

/// A "synthesis product": agent output worth keeping and serving to others.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Synthesis {
    pub id: String,
    pub kind: SynthesisKind,
    pub title: String,
    /// The human-in-the-loop steer, if any.
    pub direction: Option<String>,
    pub body: String,
    /// Articles the synthesis drew on.
    pub article_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSynthesis {
    pub kind: SynthesisKind,
    pub title: String,
    pub direction: Option<String>,
    pub body: String,
    pub article_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub article_id: String,
    pub title: String,
    pub snippet: String,
    /// Higher is better. Only comparable within one result set.
    pub score: f64,
}

/// Format version written into every [`LibraryExport`]. Bump it only for a breaking change to the JSON shape.
pub const EXPORT_VERSION: u32 = 1;

/// Engine-neutral snapshot of the whole library, for backups and for moving to another database.
/// Plain JSON, no SurrealDB types. Raw blobs are not inlined: articles carry `blob_key`, and the blob
/// files (immutable, content-addressed) are copied separately.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryExport {
    /// [`EXPORT_VERSION`] when written.
    pub version: u32,
    pub exported_at: DateTime<Utc>,
    /// Oldest first (by `saved_at`, then id).
    pub articles: Vec<Article>,
    /// Oldest first (by `created_at`, then id).
    pub highlights: Vec<Highlight>,
    /// Oldest first (by `created_at`, then id).
    pub syntheses: Vec<Synthesis>,
}
