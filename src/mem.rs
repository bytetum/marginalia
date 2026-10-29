//! In-memory `Repo` adapter. Reference behaviour for tests; not durable.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::domain::*;
use crate::repo::{self, Repo, RepoError, RepoResult};

/// Maximum snippet length in characters (before whitespace collapsing).
const SNIPPET_CHARS: usize = 200;
/// Characters of context kept before the first match in a snippet.
const SNIPPET_BEFORE: usize = 60;

#[derive(Default)]
struct State {
    /// Insertion order. Newest-first views are derived by reversing.
    articles: Vec<Article>,
    highlights: Vec<Highlight>,
    syntheses: Vec<Synthesis>,
    usage: Vec<(String, Value)>,
}

/// Everything in memory, behind one lock. Not durable.
pub struct MemRepo {
    state: RwLock<State>,
}

impl MemRepo {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(State::default()),
        }
    }

    /// The usage log, oldest first.
    pub async fn usage(&self) -> Vec<(String, Value)> {
        self.state.read().await.usage.clone()
    }
}

impl Default for MemRepo {
    fn default() -> Self {
        Self::new()
    }
}

/// Newest first by `key`. Ties are broken by insertion order, newest first.
fn newest_first<T>(items: &[T], key: impl Fn(&T) -> DateTime<Utc>) -> Vec<&T> {
    let mut v: Vec<&T> = items.iter().rev().collect();
    // Stable sort, so equal keys keep the reversed (newest-inserted first) order.
    v.sort_by_key(|t| std::cmp::Reverse(key(t)));
    v
}

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

/// Non-overlapping occurrences of `needle` in `hay`. Both are already lowercased.
fn count_matches(hay: &[char], needle: &[char]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == *needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
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

#[async_trait]
impl Repo for MemRepo {
    async fn upsert_article(&self, new: NewArticle) -> RepoResult<Article> {
        let mut st = self.state.write().await;
        let words = repo::count_words(&new.markdown);
        let now = Utc::now();

        if let Some(existing) = st.articles.iter_mut().find(|a| a.url == new.url) {
            existing.title = new.title;
            existing.markdown = new.markdown;
            existing.via = new.via;
            existing.blob_key = new.blob_key;
            existing.words = words;
            existing.saved_at = now;
            return Ok(existing.clone());
        }

        let article = Article {
            id: repo::new_id(),
            url: new.url,
            title: new.title,
            markdown: new.markdown,
            words,
            via: new.via,
            blob_key: new.blob_key,
            saved_at: now,
        };
        st.articles.push(article.clone());
        Ok(article)
    }

    async fn get_article(&self, id: &str) -> RepoResult<Option<Article>> {
        let st = self.state.read().await;
        Ok(st.articles.iter().find(|a| a.id == id).cloned())
    }

    async fn list_articles(&self, limit: u32, offset: u32) -> RepoResult<Vec<ArticleSummary>> {
        let st = self.state.read().await;
        Ok(newest_first(&st.articles, |a| a.saved_at)
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|a| ArticleSummary {
                id: a.id.clone(),
                url: a.url.clone(),
                title: a.title.clone(),
                words: a.words,
                via: a.via.clone(),
                saved_at: a.saved_at,
                highlight_count: st
                    .highlights
                    .iter()
                    .filter(|h| h.article_id == a.id)
                    .count() as u32,
            })
            .collect())
    }

    async fn delete_article(&self, id: &str) -> RepoResult<()> {
        let mut st = self.state.write().await;
        let idx = st
            .articles
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| RepoError::NotFound(format!("article {id}")))?;
        st.articles.remove(idx);
        st.highlights.retain(|h| h.article_id != id);
        Ok(())
    }

    async fn add_highlight(&self, new: NewHighlight) -> RepoResult<Highlight> {
        if new.text.trim().is_empty() {
            return Err(RepoError::Invalid("highlight text is blank".into()));
        }
        let mut st = self.state.write().await;
        if !st.articles.iter().any(|a| a.id == new.article_id) {
            return Err(RepoError::NotFound(format!("article {}", new.article_id)));
        }
        let highlight = Highlight {
            id: repo::new_id(),
            article_id: new.article_id,
            text: new.text,
            note: new.note,
            created_at: Utc::now(),
        };
        st.highlights.push(highlight.clone());
        Ok(highlight)
    }

    async fn list_highlights(&self, article_id: Option<&str>) -> RepoResult<Vec<Highlight>> {
        let st = self.state.read().await;
        let mut out: Vec<Highlight> = st
            .highlights
            .iter()
            .filter(|h| article_id.is_none_or(|id| h.article_id == id))
            .cloned()
            .collect();
        out.sort_by_key(|h| h.created_at);
        Ok(out)
    }

    async fn search(&self, query: &str, limit: u32) -> RepoResult<Vec<SearchHit>> {
        let terms = query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let st = self.state.read().await;
        let mut hits = Vec::new();

        for article in newest_first(&st.articles, |a| a.saved_at) {
            let title = lower(&article.title).collect::<Vec<char>>();
            let body = lower(&article.markdown).collect::<Vec<char>>();
            let notes = st
                .highlights
                .iter()
                .filter(|h| h.article_id == article.id)
                .map(|h| lower(&h.text).collect::<Vec<char>>())
                .collect::<Vec<_>>();

            let mut score = 0usize;
            let mut first_match: Option<usize> = None;
            for term in &terms {
                score += 3 * count_matches(&title, term);
                score += count_matches(&body, term);
                score += 2 * notes.iter().map(|n| count_matches(n, term)).sum::<usize>();
                if let Some(at) = find_first(&article.markdown, term) {
                    first_match = Some(first_match.map_or(at, |f| f.min(at)));
                }
            }
            if score == 0 {
                continue;
            }
            hits.push(SearchHit {
                article_id: article.id.clone(),
                title: article.title.clone(),
                snippet: snippet(&article.markdown, first_match),
                score: score as f64,
            });
        }

        // Stable sort: equal scores keep newest-first order.
        hits.sort_by(|a: &SearchHit, b: &SearchHit| b.score.total_cmp(&a.score));
        hits.truncate(limit as usize);
        Ok(hits)
    }

    async fn save_synthesis(&self, new: NewSynthesis) -> RepoResult<Synthesis> {
        let synthesis = Synthesis {
            id: repo::new_id(),
            kind: new.kind,
            title: new.title,
            direction: new.direction,
            body: new.body,
            article_ids: new.article_ids,
            created_at: Utc::now(),
        };
        self.state.write().await.syntheses.push(synthesis.clone());
        Ok(synthesis)
    }

    async fn get_synthesis(&self, id: &str) -> RepoResult<Option<Synthesis>> {
        let st = self.state.read().await;
        Ok(st.syntheses.iter().find(|s| s.id == id).cloned())
    }

    async fn list_syntheses(
        &self,
        kind: Option<SynthesisKind>,
        limit: u32,
    ) -> RepoResult<Vec<Synthesis>> {
        let st = self.state.read().await;
        Ok(newest_first(&st.syntheses, |s| s.created_at)
            .into_iter()
            .filter(|s| kind.is_none_or(|k| s.kind == k))
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn log_usage(&self, kind: &str, detail: Value) -> RepoResult<()> {
        self.state
            .write()
            .await
            .usage
            .push((kind.to_string(), detail));
        Ok(())
    }

    async fn restore(&self, export: &LibraryExport) -> RepoResult<()> {
        repo::validate_export(export)?;
        let mut st = self.state.write().await;
        if !(st.articles.is_empty() && st.highlights.is_empty() && st.syntheses.is_empty()) {
            return Err(RepoError::Invalid("restore needs an empty library, but this one already has data".into()));
        }
        st.articles = export.articles.clone();
        st.highlights = export.highlights.clone();
        st.syntheses = export.syntheses.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_article(url: &str, title: &str, markdown: &str) -> NewArticle {
        NewArticle {
            url: url.into(),
            title: title.into(),
            markdown: markdown.into(),
            via: "manual".into(),
            blob_key: None,
        }
    }

    fn new_highlight(article_id: &str, text: &str) -> NewHighlight {
        NewHighlight {
            article_id: article_id.into(),
            text: text.into(),
            note: None,
        }
    }

    #[tokio::test]
    async fn upsert_dedupes_by_url_and_keeps_id() {
        let repo = MemRepo::new();
        let a = repo
            .upsert_article(new_article("https://x/1", "Old", "one two"))
            .await
            .unwrap();
        let b = repo
            .upsert_article(new_article("https://x/1", "New", "one two three"))
            .await
            .unwrap();

        assert_eq!(a.id, b.id);
        assert_eq!(b.title, "New");
        assert_eq!(b.words, 3);
        assert!(b.saved_at >= a.saved_at);
        assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 1);
        assert_eq!(repo.get_article(&a.id).await.unwrap().unwrap().title, "New");

        let other = repo
            .upsert_article(new_article("https://x/2", "Other", "x"))
            .await
            .unwrap();
        assert_ne!(other.id, a.id);
    }

    #[tokio::test]
    async fn list_is_newest_first_with_paging_and_highlight_counts() {
        let repo = MemRepo::new();
        let a = repo.upsert_article(new_article("https://x/a", "A", "a")).await.unwrap();
        let b = repo.upsert_article(new_article("https://x/b", "B", "b")).await.unwrap();
        let c = repo.upsert_article(new_article("https://x/c", "C", "c")).await.unwrap();
        repo.add_highlight(new_highlight(&b.id, "first")).await.unwrap();
        repo.add_highlight(new_highlight(&b.id, "second")).await.unwrap();

        let ids = |v: Vec<ArticleSummary>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
        let all = repo.list_articles(10, 0).await.unwrap();
        assert_eq!(ids(all.clone()), vec![c.id.clone(), b.id.clone(), a.id.clone()]);
        assert_eq!(all[1].highlight_count, 2);
        assert_eq!(all[0].highlight_count, 0);

        let page1 = repo.list_articles(2, 0).await.unwrap();
        assert_eq!(ids(page1), vec![c.id.clone(), b.id.clone()]);
        let page2 = repo.list_articles(2, 2).await.unwrap();
        assert_eq!(ids(page2), vec![a.id.clone()]);
        assert!(repo.list_articles(10, 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_removes_article_and_its_highlights() {
        let repo = MemRepo::new();
        let a = repo.upsert_article(new_article("https://x/a", "A", "a")).await.unwrap();
        let b = repo.upsert_article(new_article("https://x/b", "B", "b")).await.unwrap();
        repo.add_highlight(new_highlight(&a.id, "one")).await.unwrap();
        repo.add_highlight(new_highlight(&a.id, "two")).await.unwrap();
        let kept = repo.add_highlight(new_highlight(&b.id, "keep")).await.unwrap();

        repo.delete_article(&a.id).await.unwrap();

        assert!(repo.get_article(&a.id).await.unwrap().is_none());
        let remaining = repo.list_highlights(None).await.unwrap();
        assert_eq!(remaining, vec![kept]);
        assert!(repo.list_highlights(Some(&a.id)).await.unwrap().is_empty());
        assert!(matches!(
            repo.delete_article(&a.id).await.unwrap_err(),
            RepoError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn add_highlight_rejects_blank_text_and_missing_article() {
        let repo = MemRepo::new();
        let a = repo.upsert_article(new_article("https://x/a", "A", "a")).await.unwrap();

        for blank in ["", "   ", "\n\t "] {
            let err = repo.add_highlight(new_highlight(&a.id, blank)).await.unwrap_err();
            assert!(matches!(err, RepoError::Invalid(_)), "blank {blank:?}");
        }
        let err = repo
            .add_highlight(new_highlight("no-such-article", "text"))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)));

        assert!(repo.list_highlights(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_highlights_is_oldest_first() {
        let repo = MemRepo::new();
        let a = repo.upsert_article(new_article("https://x/a", "A", "a")).await.unwrap();
        let h1 = repo.add_highlight(new_highlight(&a.id, "one")).await.unwrap();
        let h2 = repo.add_highlight(new_highlight(&a.id, "two")).await.unwrap();
        assert_eq!(repo.list_highlights(Some(&a.id)).await.unwrap(), vec![h1, h2]);
    }

    #[tokio::test]
    async fn search_scores_title_body_and_highlights_and_orders_best_first() {
        let repo = MemRepo::new();
        // Title hit (3) + highlight hit (2) = 5.
        let a = repo
            .upsert_article(new_article("https://x/a", "Rust async", "Notes about tokio."))
            .await
            .unwrap();
        repo.add_highlight(new_highlight(&a.id, "async runtime tip"))
            .await
            .unwrap();
        // Two body hits = 2.
        let b = repo
            .upsert_article(new_article(
                "https://x/b",
                "Gardening",
                "async async is mentioned twice",
            ))
            .await
            .unwrap();
        // Highlight-only hit = 2.
        let c = repo
            .upsert_article(new_article("https://x/c", "Cooking", "soup and bread"))
            .await
            .unwrap();
        repo.add_highlight(new_highlight(&c.id, "Async is only here"))
            .await
            .unwrap();
        // No hit at all.
        repo.upsert_article(new_article("https://x/d", "Nothing", "unrelated"))
            .await
            .unwrap();

        let hits = repo.search("ASYNC", 10).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].article_id, a.id);
        assert_eq!(hits[0].score, 5.0);
        assert_eq!(hits[0].title, "Rust async");
        // Tie at 2.0: newest article first (c was saved after b).
        assert_eq!(hits[1].article_id, c.id);
        assert_eq!(hits[1].score, 2.0);
        assert_eq!(hits[2].article_id, b.id);
        assert_eq!(hits[2].score, 2.0);
        assert!(hits.iter().all(|h| h.score > 0.0));

        assert_eq!(repo.search("async", 1).await.unwrap().len(), 1);
        assert!(repo.search("", 10).await.unwrap().is_empty());
        assert!(repo.search("   \t ", 10).await.unwrap().is_empty());
        // Terms shorter than two characters are ignored.
        assert!(repo.search("a", 10).await.unwrap().is_empty());
        assert!(repo.search("async", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_snippet_handles_non_ascii_text() {
        let repo = MemRepo::new();
        let filler = "Đây là một đoạn văn dài về kinh tế vĩ mô của quốc gia. ".repeat(10);
        let markdown = format!("{filler}Lãi suất ngân hàng tăng mạnh trong quý này.\n\n{filler}");
        let a = repo
            .upsert_article(new_article("https://x/vi", "Kinh tế", &markdown))
            .await
            .unwrap();

        let hits = repo.search("lãi suất", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].article_id, a.id);
        assert_eq!(hits[0].score, 2.0);
        let snippet = &hits[0].snippet;
        assert!(snippet.contains("Lãi suất"), "snippet was {snippet:?}");
        assert!(!snippet.contains('\n'));
        assert!(snippet.chars().count() <= SNIPPET_CHARS);
    }

    #[tokio::test]
    async fn search_snippet_at_start_of_text() {
        let repo = MemRepo::new();
        repo.upsert_article(new_article("https://x/s", "Tiêu đề", "Lãi suất\n  giữ nguyên"))
            .await
            .unwrap();
        let hits = repo.search("suất", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Lãi suất giữ nguyên");
    }

    #[tokio::test]
    async fn synthesis_list_is_newest_first_and_filters_by_kind() {
        let repo = MemRepo::new();
        let mk = |kind, title: &str| NewSynthesis {
            kind,
            title: title.into(),
            direction: None,
            body: "body".into(),
            article_ids: vec![],
        };
        let b1 = repo.save_synthesis(mk(SynthesisKind::Brief, "b1")).await.unwrap();
        let a1 = repo.save_synthesis(mk(SynthesisKind::Analysis, "a1")).await.unwrap();
        let b2 = repo.save_synthesis(mk(SynthesisKind::Brief, "b2")).await.unwrap();

        let ids = |v: Vec<Synthesis>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(
            ids(repo.list_syntheses(None, 10).await.unwrap()),
            vec![b2.id.clone(), a1.id.clone(), b1.id.clone()]
        );
        assert_eq!(
            ids(repo.list_syntheses(Some(SynthesisKind::Brief), 10).await.unwrap()),
            vec![b2.id.clone(), b1.id.clone()]
        );
        assert_eq!(
            ids(repo.list_syntheses(Some(SynthesisKind::Analysis), 10).await.unwrap()),
            vec![a1.id.clone()]
        );
        assert_eq!(ids(repo.list_syntheses(None, 2).await.unwrap()), vec![b2.id.clone(), a1.id.clone()]);
        assert_eq!(repo.get_synthesis(&a1.id).await.unwrap(), Some(a1));
        assert!(repo.get_synthesis("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn usage_log_is_append_only_in_order() {
        let repo = MemRepo::new();
        repo.log_usage("save_url", json!({"url": "https://x/1"})).await.unwrap();
        repo.log_usage("chat", json!({"session": "s1"})).await.unwrap();
        assert_eq!(
            repo.usage().await,
            vec![
                ("save_url".to_string(), json!({"url": "https://x/1"})),
                ("chat".to_string(), json!({"session": "s1"})),
            ]
        );
    }
}
