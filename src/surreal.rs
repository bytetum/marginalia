//! `Repo` adapter on embedded SurrealDB 3 (`surrealkv` on disk, `mem` for tests).
//!
//! Layout: one table per kind (`article`, `highlight`, `synthesis`, `usage`). The app's id string is the
//! record key (`article:<id>`), read back with `record::id(id)`. Highlights point at their article with a
//! plain `article_id` string field, so cascade delete is one `DELETE ... WHERE`.
//!
//! Search runs one BM25 query per (term, field) over three FULLTEXT indexes (article title, article
//! markdown, highlight text), then merges in Rust: `3 * title + body + 2 * highlight`, the same field
//! weights as `MemRepo`. Terms are OR-ed like in `MemRepo`.
//!
//! Serialization: the SDK moved from serde to its own `SurrealValue` trait. Private row structs derive
//! it and are the only types that cross the SDK boundary; the domain types stay storage-agnostic.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use surrealdb::types::{RecordId, SurrealValue};
use tokio::sync::Mutex;

use crate::domain::*;
use crate::repo::{self, Repo, RepoError, RepoResult};

const NAMESPACE: &str = "marginalia";
const DATABASE: &str = "main";

/// Maximum snippet length in characters (before whitespace collapsing).
const SNIPPET_CHARS: usize = 200;
/// Characters of context kept before the first match in a snippet.
const SNIPPET_BEFORE: usize = 60;

/// Field weights, as in `MemRepo`.
const TITLE_WEIGHT: f64 = 3.0;
const BODY_WEIGHT: f64 = 1.0;
const HIGHLIGHT_WEIGHT: f64 = 2.0;

/// SurrealDB's BM25 clamps idf at 0, so a term present in half the documents or more scores 0.
/// Every matched (term, field) therefore also earns this small floor: hits always score above 0
/// and the field weights still order otherwise-equal matches.
const MATCH_FLOOR: f64 = 0.01;

/// How long `open` waits for a just-dropped repo to release the directory lock.
const LOCK_RETRIES: u32 = 100;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Idempotent: every statement is `IF NOT EXISTS`.
const SCHEMA: &str = "
DEFINE ANALYZER IF NOT EXISTS marginalia_text TOKENIZERS blank, class, punct FILTERS lowercase;

DEFINE TABLE IF NOT EXISTS article SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS url ON article TYPE string;
DEFINE FIELD IF NOT EXISTS title ON article TYPE string;
DEFINE FIELD IF NOT EXISTS markdown ON article TYPE string;
DEFINE FIELD IF NOT EXISTS words ON article TYPE int;
DEFINE FIELD IF NOT EXISTS via ON article TYPE string;
DEFINE FIELD IF NOT EXISTS blob_key ON article TYPE option<string>;
DEFINE FIELD IF NOT EXISTS saved_at ON article TYPE datetime;
DEFINE INDEX IF NOT EXISTS article_url ON article FIELDS url UNIQUE;
DEFINE INDEX IF NOT EXISTS article_saved_at ON article FIELDS saved_at;
DEFINE INDEX IF NOT EXISTS article_title_ft ON article FIELDS title FULLTEXT ANALYZER marginalia_text BM25;
DEFINE INDEX IF NOT EXISTS article_markdown_ft ON article FIELDS markdown FULLTEXT ANALYZER marginalia_text BM25;

DEFINE TABLE IF NOT EXISTS highlight SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS article_id ON highlight TYPE string;
DEFINE FIELD IF NOT EXISTS text ON highlight TYPE string;
DEFINE FIELD IF NOT EXISTS note ON highlight TYPE option<string>;
DEFINE FIELD IF NOT EXISTS created_at ON highlight TYPE datetime;
DEFINE INDEX IF NOT EXISTS highlight_article ON highlight FIELDS article_id;
DEFINE INDEX IF NOT EXISTS highlight_created_at ON highlight FIELDS created_at;
DEFINE INDEX IF NOT EXISTS highlight_text_ft ON highlight FIELDS text FULLTEXT ANALYZER marginalia_text BM25;

DEFINE TABLE IF NOT EXISTS synthesis SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS kind ON synthesis TYPE string;
DEFINE FIELD IF NOT EXISTS title ON synthesis TYPE string;
DEFINE FIELD IF NOT EXISTS direction ON synthesis TYPE option<string>;
DEFINE FIELD IF NOT EXISTS body ON synthesis TYPE string;
DEFINE FIELD IF NOT EXISTS article_ids ON synthesis TYPE array<string>;
DEFINE FIELD IF NOT EXISTS created_at ON synthesis TYPE datetime;
DEFINE INDEX IF NOT EXISTS synthesis_created_at ON synthesis FIELDS created_at;

DEFINE TABLE IF NOT EXISTS usage SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS kind ON usage TYPE string;
DEFINE FIELD IF NOT EXISTS detail ON usage TYPE any;
DEFINE FIELD IF NOT EXISTS at ON usage TYPE datetime;
";

const ARTICLE_COLS: &str = "record::id(id) AS id, url, title, markdown, words, via, blob_key, saved_at";
const HIGHLIGHT_COLS: &str = "record::id(id) AS id, article_id, text, note, created_at";
const SYNTHESIS_COLS: &str = "record::id(id) AS id, kind, title, direction, body, article_ids, created_at";

// ---------------------------------------------------------------------------------------------
// Row types (the SDK boundary)
// ---------------------------------------------------------------------------------------------

/// Article fields as stored (no id: the id is the record key).
#[derive(Debug, SurrealValue)]
struct ArticleDoc {
    url: String,
    title: String,
    markdown: String,
    words: i64,
    via: String,
    blob_key: Option<String>,
    saved_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct ArticleRow {
    id: String,
    url: String,
    title: String,
    markdown: String,
    words: i64,
    via: String,
    blob_key: Option<String>,
    saved_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct SummaryRow {
    id: String,
    url: String,
    title: String,
    words: i64,
    via: String,
    saved_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct CountRow {
    article_id: String,
    n: i64,
}

#[derive(Debug, SurrealValue)]
struct HighlightDoc {
    article_id: String,
    text: String,
    note: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct HighlightRow {
    id: String,
    article_id: String,
    text: String,
    note: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct SynthesisDoc {
    kind: String,
    title: String,
    direction: Option<String>,
    body: String,
    article_ids: Vec<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct SynthesisRow {
    id: String,
    kind: String,
    title: String,
    direction: Option<String>,
    body: String,
    article_ids: Vec<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct UsageDoc {
    kind: String,
    detail: Value,
    at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct UsageRow {
    kind: String,
    detail: Value,
}

#[derive(Debug, SurrealValue)]
struct ScoreRow {
    id: String,
    score: f64,
}

#[derive(Debug, SurrealValue)]
struct TitleRow {
    id: String,
    title: String,
    saved_at: DateTime<Utc>,
}

#[derive(Debug, SurrealValue)]
struct BodyRow {
    id: String,
    markdown: String,
}

fn to_u32(n: i64) -> u32 {
    u32::try_from(n).unwrap_or_default()
}

fn backend(e: impl std::fmt::Display) -> RepoError {
    RepoError::Backend(e.to_string())
}

impl From<ArticleRow> for Article {
    fn from(r: ArticleRow) -> Self {
        Article {
            id: r.id,
            url: r.url,
            title: r.title,
            markdown: r.markdown,
            words: to_u32(r.words),
            via: r.via,
            blob_key: r.blob_key,
            saved_at: r.saved_at,
        }
    }
}

impl From<HighlightRow> for Highlight {
    fn from(r: HighlightRow) -> Self {
        Highlight {
            id: r.id,
            article_id: r.article_id,
            text: r.text,
            note: r.note,
            created_at: r.created_at,
        }
    }
}

impl TryFrom<SynthesisRow> for Synthesis {
    type Error = RepoError;

    fn try_from(r: SynthesisRow) -> RepoResult<Self> {
        let kind = SynthesisKind::parse(&r.kind)
            .ok_or_else(|| RepoError::Backend(format!("unknown synthesis kind {:?}", r.kind)))?;
        Ok(Synthesis {
            id: r.id,
            kind,
            title: r.title,
            direction: r.direction,
            body: r.body,
            article_ids: r.article_ids,
            created_at: r.created_at,
        })
    }
}

fn record(table: &str, id: &str) -> RecordId {
    RecordId::new(table, id)
}

// ---------------------------------------------------------------------------------------------
// Repo
// ---------------------------------------------------------------------------------------------

/// Embedded SurrealDB behind the `Repo` port.
pub struct SurrealRepo {
    db: Surreal<Db>,
    /// Serialises the read-then-write operations (upsert by url, add highlight, delete) within this process.
    write: Mutex<()>,
}

impl SurrealRepo {
    /// Durable store in the directory `path` (`surrealkv` engine). Created if missing.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, RepoError> {
        let path = path.as_ref().to_path_buf();
        // Dropping a `SurrealRepo` hands the datastore to a background task that releases the
        // directory lock a moment later, so a prompt re-open can see the old lock. Retry briefly.
        let mut attempt = 0;
        let db = loop {
            match Surreal::new::<SurrealKv>(path.clone()).await {
                Ok(db) => break db,
                Err(e) if attempt < LOCK_RETRIES && e.to_string().contains("already locked") => {
                    attempt += 1;
                    tokio::time::sleep(LOCK_RETRY_DELAY).await;
                }
                Err(e) => return Err(backend(e)),
            }
        };
        Self::init(db).await
    }

    /// Throw-away in-memory store (`mem` engine), for tests.
    pub async fn memory() -> Result<Self, RepoError> {
        let db = Surreal::new::<Mem>(()).await.map_err(backend)?;
        Self::init(db).await
    }

    async fn init(db: Surreal<Db>) -> Result<Self, RepoError> {
        db.use_ns(NAMESPACE).use_db(DATABASE).await.map_err(backend)?;
        db.query(SCHEMA)
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(Self {
            db,
            write: Mutex::new(()),
        })
    }

    /// The usage log, oldest first.
    pub async fn usage(&self) -> RepoResult<Vec<(String, Value)>> {
        let mut res = self
            .db
            .query("SELECT kind, detail FROM usage ORDER BY at ASC")
            .await
            .map_err(backend)?;
        let rows: Vec<UsageRow> = res.take(0).map_err(backend)?;
        Ok(rows.into_iter().map(|r| (r.kind, r.detail)).collect())
    }

    async fn article_exists(&self, id: &str) -> RepoResult<bool> {
        let mut res = self
            .db
            .query("SELECT VALUE record::id(id) FROM type::record('article', $id)")
            .bind(("id", id.to_string()))
            .await
            .map_err(backend)?;
        let rows: Vec<String> = res.take(0).map_err(backend)?;
        Ok(!rows.is_empty())
    }
}

#[async_trait]
impl Repo for SurrealRepo {
    async fn upsert_article(&self, new: NewArticle) -> RepoResult<Article> {
        let _guard = self.write.lock().await;
        let doc = ArticleDoc {
            words: i64::from(repo::count_words(&new.markdown)),
            url: new.url,
            title: new.title,
            markdown: new.markdown,
            via: new.via,
            blob_key: new.blob_key,
            saved_at: Utc::now(),
        };

        let mut res = self
            .db
            .query("SELECT VALUE record::id(id) FROM article WHERE url = $url LIMIT 1")
            .bind(("url", doc.url.clone()))
            .await
            .map_err(backend)?;
        let existing: Vec<String> = res.take(0).map_err(backend)?;

        let id = existing.into_iter().next().unwrap_or_else(repo::new_id);
        let article = Article {
            id: id.clone(),
            url: doc.url.clone(),
            title: doc.title.clone(),
            markdown: doc.markdown.clone(),
            words: to_u32(doc.words),
            via: doc.via.clone(),
            blob_key: doc.blob_key.clone(),
            saved_at: doc.saved_at,
        };
        // UPSERT ... CONTENT creates or replaces the whole record under the same key.
        self.db
            .query("UPSERT type::record('article', $id) CONTENT $doc")
            .bind(("id", id))
            .bind(("doc", doc))
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(article)
    }

    async fn get_article(&self, id: &str) -> RepoResult<Option<Article>> {
        let sql = format!("SELECT {ARTICLE_COLS} FROM type::record('article', $id)");
        let mut res = self
            .db
            .query(sql)
            .bind(("id", id.to_string()))
            .await
            .map_err(backend)?;
        let rows: Vec<ArticleRow> = res.take(0).map_err(backend)?;
        Ok(rows.into_iter().next().map(Article::from))
    }

    async fn list_articles(&self, limit: u32, offset: u32) -> RepoResult<Vec<ArticleSummary>> {
        let mut res = self
            .db
            .query(
                "SELECT record::id(id) AS id, url, title, words, via, saved_at FROM article \
                 ORDER BY saved_at DESC LIMIT $limit START $offset",
            )
            .bind(("limit", i64::from(limit)))
            .bind(("offset", i64::from(offset)))
            .await
            .map_err(backend)?;
        let rows: Vec<SummaryRow> = res.take(0).map_err(backend)?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let mut res = self
            .db
            .query(
                "SELECT article_id, count() AS n FROM highlight \
                 WHERE article_id IN $ids GROUP BY article_id",
            )
            .bind(("ids", ids))
            .await
            .map_err(backend)?;
        let counts: Vec<CountRow> = res.take(0).map_err(backend)?;
        let counts: HashMap<String, i64> = counts.into_iter().map(|c| (c.article_id, c.n)).collect();

        Ok(rows
            .into_iter()
            .map(|r| ArticleSummary {
                highlight_count: to_u32(counts.get(&r.id).copied().unwrap_or(0)),
                id: r.id,
                url: r.url,
                title: r.title,
                words: to_u32(r.words),
                via: r.via,
                saved_at: r.saved_at,
            })
            .collect())
    }

    async fn delete_article(&self, id: &str) -> RepoResult<()> {
        let _guard = self.write.lock().await;
        if !self.article_exists(id).await? {
            return Err(RepoError::NotFound(format!("article {id}")));
        }
        self.db
            .query(
                "BEGIN; \
                 DELETE highlight WHERE article_id = $id; \
                 DELETE type::record('article', $id); \
                 COMMIT;",
            )
            .bind(("id", id.to_string()))
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(())
    }

    async fn add_highlight(&self, new: NewHighlight) -> RepoResult<Highlight> {
        if new.text.trim().is_empty() {
            return Err(RepoError::Invalid("highlight text is blank".into()));
        }
        let _guard = self.write.lock().await;
        if !self.article_exists(&new.article_id).await? {
            return Err(RepoError::NotFound(format!("article {}", new.article_id)));
        }
        let id = repo::new_id();
        let highlight = Highlight {
            id: id.clone(),
            article_id: new.article_id,
            text: new.text,
            note: new.note,
            created_at: Utc::now(),
        };
        let doc = HighlightDoc {
            article_id: highlight.article_id.clone(),
            text: highlight.text.clone(),
            note: highlight.note.clone(),
            created_at: highlight.created_at,
        };
        self.db
            .query("CREATE type::record('highlight', $id) CONTENT $doc")
            .bind(("id", id))
            .bind(("doc", doc))
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(highlight)
    }

    async fn list_highlights(&self, article_id: Option<&str>) -> RepoResult<Vec<Highlight>> {
        let mut res = match article_id {
            Some(aid) => self
                .db
                .query(format!(
                    "SELECT {HIGHLIGHT_COLS} FROM highlight WHERE article_id = $aid ORDER BY created_at ASC"
                ))
                .bind(("aid", aid.to_string()))
                .await
                .map_err(backend)?,
            None => self
                .db
                .query(format!(
                    "SELECT {HIGHLIGHT_COLS} FROM highlight ORDER BY created_at ASC"
                ))
                .await
                .map_err(backend)?,
        };
        let rows: Vec<HighlightRow> = res.take(0).map_err(backend)?;
        Ok(rows.into_iter().map(Highlight::from).collect())
    }

    async fn search(&self, query: &str, limit: u32) -> RepoResult<Vec<SearchHit>> {
        let terms = query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        // One statement per (term, field); statement `3 * i + f` answers term i on field f.
        let mut sql = String::new();
        let mut binds: Vec<(String, String)> = Vec::new();
        for (i, term) in terms.iter().enumerate() {
            sql.push_str(&format!(
                "SELECT record::id(id) AS id, search::score(0) AS score FROM article WHERE title @0@ $t{i};\n\
                 SELECT record::id(id) AS id, search::score(0) AS score FROM article WHERE markdown @0@ $t{i};\n\
                 SELECT article_id AS id, search::score(0) AS score FROM highlight WHERE text @0@ $t{i};\n"
            ));
            binds.push((format!("t{i}"), term.iter().collect()));
        }
        let mut req = self.db.query(sql);
        for (name, value) in binds {
            req = req.bind((name, value));
        }
        let mut res = req.await.map_err(backend)?;

        let mut scores: HashMap<String, f64> = HashMap::new();
        for i in 0..terms.len() {
            for (f, weight) in [TITLE_WEIGHT, BODY_WEIGHT, HIGHLIGHT_WEIGHT].into_iter().enumerate() {
                let rows: Vec<ScoreRow> = res.take(3 * i + f).map_err(backend)?;
                for row in rows {
                    *scores.entry(row.id).or_insert(0.0) += weight * (row.score.max(0.0) + MATCH_FLOOR);
                }
            }
        }
        if scores.is_empty() {
            return Ok(Vec::new());
        }

        // Titles and dates for every candidate (tie-break: newest first, as MemRepo).
        let recs: Vec<RecordId> = scores.keys().map(|id| record("article", id)).collect();
        let mut res = self
            .db
            .query("SELECT record::id(id) AS id, title, saved_at FROM $recs")
            .bind(("recs", recs))
            .await
            .map_err(backend)?;
        let titles: Vec<TitleRow> = res.take(0).map_err(backend)?;

        let mut ranked: Vec<(TitleRow, f64)> = titles
            .into_iter()
            .filter_map(|t| scores.get(&t.id).copied().map(|s| (t, s)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        ranked.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then_with(|| b.saved_at.cmp(&a.saved_at)));
        ranked.truncate(limit as usize);
        if ranked.is_empty() {
            return Ok(Vec::new());
        }

        // Bodies only for the hits we return, for snippets.
        let recs: Vec<RecordId> = ranked.iter().map(|(t, _)| record("article", &t.id)).collect();
        let mut res = self
            .db
            .query("SELECT record::id(id) AS id, markdown FROM $recs")
            .bind(("recs", recs))
            .await
            .map_err(backend)?;
        let bodies: Vec<BodyRow> = res.take(0).map_err(backend)?;
        let bodies: HashMap<String, String> = bodies.into_iter().map(|b| (b.id, b.markdown)).collect();

        Ok(ranked
            .into_iter()
            .map(|(t, score)| {
                let markdown = bodies.get(&t.id).map(String::as_str).unwrap_or("");
                let first_match = terms.iter().filter_map(|term| find_first(markdown, term)).min();
                SearchHit {
                    snippet: snippet(markdown, first_match),
                    article_id: t.id,
                    title: t.title,
                    score,
                }
            })
            .collect())
    }

    async fn save_synthesis(&self, new: NewSynthesis) -> RepoResult<Synthesis> {
        let id = repo::new_id();
        let synthesis = Synthesis {
            id: id.clone(),
            kind: new.kind,
            title: new.title,
            direction: new.direction,
            body: new.body,
            article_ids: new.article_ids,
            created_at: Utc::now(),
        };
        let doc = SynthesisDoc {
            kind: synthesis.kind.as_str().to_string(),
            title: synthesis.title.clone(),
            direction: synthesis.direction.clone(),
            body: synthesis.body.clone(),
            article_ids: synthesis.article_ids.clone(),
            created_at: synthesis.created_at,
        };
        self.db
            .query("CREATE type::record('synthesis', $id) CONTENT $doc")
            .bind(("id", id))
            .bind(("doc", doc))
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(synthesis)
    }

    async fn get_synthesis(&self, id: &str) -> RepoResult<Option<Synthesis>> {
        let sql = format!("SELECT {SYNTHESIS_COLS} FROM type::record('synthesis', $id)");
        let mut res = self
            .db
            .query(sql)
            .bind(("id", id.to_string()))
            .await
            .map_err(backend)?;
        let rows: Vec<SynthesisRow> = res.take(0).map_err(backend)?;
        rows.into_iter().next().map(Synthesis::try_from).transpose()
    }

    async fn list_syntheses(
        &self,
        kind: Option<SynthesisKind>,
        limit: u32,
    ) -> RepoResult<Vec<Synthesis>> {
        let mut res = match kind {
            Some(k) => self
                .db
                .query(format!(
                    "SELECT {SYNTHESIS_COLS} FROM synthesis WHERE kind = $kind \
                     ORDER BY created_at DESC LIMIT $limit"
                ))
                .bind(("kind", k.as_str().to_string()))
                .bind(("limit", i64::from(limit)))
                .await
                .map_err(backend)?,
            None => self
                .db
                .query(format!(
                    "SELECT {SYNTHESIS_COLS} FROM synthesis ORDER BY created_at DESC LIMIT $limit"
                ))
                .bind(("limit", i64::from(limit)))
                .await
                .map_err(backend)?,
        };
        let rows: Vec<SynthesisRow> = res.take(0).map_err(backend)?;
        rows.into_iter().map(Synthesis::try_from).collect()
    }

    async fn log_usage(&self, kind: &str, detail: Value) -> RepoResult<()> {
        let doc = UsageDoc {
            kind: kind.to_string(),
            detail,
            at: Utc::now(),
        };
        self.db
            .query("CREATE usage CONTENT $doc")
            .bind(("doc", doc))
            .await
            .map_err(backend)?
            .check()
            .map_err(backend)?;
        Ok(())
    }

    async fn restore(&self, export: &LibraryExport) -> RepoResult<()> {
        repo::validate_export(export)?;
        let _guard = self.write.lock().await;

        let mut res = self
            .db
            .query(
                "SELECT VALUE record::id(id) FROM article LIMIT 1; \
                 SELECT VALUE record::id(id) FROM highlight LIMIT 1; \
                 SELECT VALUE record::id(id) FROM synthesis LIMIT 1;",
            )
            .await
            .map_err(backend)?;
        for statement in 0..3 {
            let rows: Vec<String> = res.take(statement).map_err(backend)?;
            if !rows.is_empty() {
                return Err(RepoError::Invalid("restore needs an empty library, but this one already has data".into()));
            }
        }
        if export.articles.is_empty() && export.highlights.is_empty() && export.syntheses.is_empty() {
            return Ok(());
        }

        // One transaction, so a failure leaves the library empty. Statements only name their binds.
        let mut sql = String::from("BEGIN;\n");
        for i in 0..export.articles.len() {
            sql.push_str(&format!("CREATE type::record('article', $ai{i}) CONTENT $ad{i};\n"));
        }
        for i in 0..export.highlights.len() {
            sql.push_str(&format!("CREATE type::record('highlight', $hi{i}) CONTENT $hd{i};\n"));
        }
        for i in 0..export.syntheses.len() {
            sql.push_str(&format!("CREATE type::record('synthesis', $si{i}) CONTENT $sd{i};\n"));
        }
        sql.push_str("COMMIT;");

        let mut req = self.db.query(sql);
        for (i, a) in export.articles.iter().enumerate() {
            let doc = ArticleDoc {
                url: a.url.clone(),
                title: a.title.clone(),
                markdown: a.markdown.clone(),
                words: i64::from(a.words),
                via: a.via.clone(),
                blob_key: a.blob_key.clone(),
                saved_at: a.saved_at,
            };
            req = req.bind((format!("ai{i}"), a.id.clone())).bind((format!("ad{i}"), doc));
        }
        for (i, h) in export.highlights.iter().enumerate() {
            let doc = HighlightDoc { article_id: h.article_id.clone(), text: h.text.clone(), note: h.note.clone(), created_at: h.created_at };
            req = req.bind((format!("hi{i}"), h.id.clone())).bind((format!("hd{i}"), doc));
        }
        for (i, s) in export.syntheses.iter().enumerate() {
            let doc = SynthesisDoc {
                kind: s.kind.as_str().to_string(),
                title: s.title.clone(),
                direction: s.direction.clone(),
                body: s.body.clone(),
                article_ids: s.article_ids.clone(),
                created_at: s.created_at,
            };
            req = req.bind((format!("si{i}"), s.id.clone())).bind((format!("sd{i}"), doc));
        }
        req.await.map_err(backend)?.check().map_err(backend)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Query terms and snippets (same rules as MemRepo, so both adapters agree)
// ---------------------------------------------------------------------------------------------

/// Lowercased characters, one char at a time, so byte offsets in the original stay valid.
fn lower(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().flat_map(char::to_lowercase)
}

/// Lowercased query terms with at least two characters.
fn query_terms(query: &str) -> Vec<Vec<char>> {
    query
        .split_whitespace()
        .map(|w| lower(w).collect::<Vec<char>>())
        .filter(|t| t.len() >= 2)
        .collect()
}

/// Byte offset in `haystack` (original case) of the first case-insensitive match of `needle`.
fn find_first(haystack: &str, needle: &[char]) -> Option<usize> {
    haystack.char_indices().map(|(i, _)| i).find(|&i| {
        let mut h = lower(&haystack[i..]);
        needle.iter().all(|&n| h.next() == Some(n))
    })
}

/// Up to `SNIPPET_CHARS` characters of `markdown`, starting shortly before `first_match`
/// (or at the start when there is no match in the body), with whitespace collapsed.
fn snippet(markdown: &str, first_match: Option<usize>) -> String {
    let start = match first_match {
        Some(at) => markdown[..at]
            .char_indices()
            .rev()
            .nth(SNIPPET_BEFORE - 1)
            .map_or(0, |(i, _)| i),
        None => 0,
    };
    let end = markdown[start..]
        .char_indices()
        .nth(SNIPPET_CHARS)
        .map_or(markdown.len(), |(i, _)| start + i);
    markdown[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
