//! Contract every `Repo` adapter must satisfy. Each check is written once against `&dyn Repo`
//! and instantiated for `MemRepo` and `SurrealRepo::memory()`.
//!
//! Search checks only assert orderings that hold under any sensible ranking (BM25 and MemRepo's
//! term counts legitimately differ), and never exact scores.

use marginalia::domain::*;
use marginalia::mem::MemRepo;
use marginalia::repo::{Repo, RepoError, export_library, validate_export};
use marginalia::surreal::SurrealRepo;
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

async fn article(repo: &dyn Repo, url: &str, title: &str, markdown: &str) -> Article {
    repo.upsert_article(new_article(url, title, markdown)).await.unwrap()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------- checks

async fn upsert_dedupes_by_url_and_keeps_id(repo: &dyn Repo) {
    let a = repo
        .upsert_article(NewArticle {
            blob_key: Some("abc.html".into()),
            ..new_article("https://x/1", "Old", "one two")
        })
        .await
        .unwrap();
    assert_eq!(a.id.len(), 32);
    assert!(a.id.chars().all(|c| c.is_ascii_hexdigit()), "id {:?}", a.id);
    assert_eq!(a.words, 2);
    assert_eq!(a.blob_key.as_deref(), Some("abc.html"));

    let b = article(repo, "https://x/1", "New", "one two three").await;
    assert_eq!(a.id, b.id);
    assert_eq!(b.title, "New");
    assert_eq!(b.words, 3);
    assert_eq!(b.blob_key, None, "upsert replaces blob_key");
    assert!(b.saved_at >= a.saved_at);
    assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 1);

    let got = repo.get_article(&a.id).await.unwrap().unwrap();
    assert_eq!(got, b);

    let other = article(repo, "https://x/2", "Other", "x").await;
    assert_ne!(other.id, a.id);
    assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 2);
    assert!(repo.get_article("no-such-id").await.unwrap().is_none());
}

async fn article_roundtrips_unicode_and_all_fields(repo: &dyn Repo) {
    let md = "# Kinh tế\n\nLãi suất ngân hàng tăng.\n\n```rust\nfn main() {}\n```\n\"quotes\" 'single' \\ back";
    let a = repo
        .upsert_article(NewArticle {
            url: "https://x/vi?a=1&b=2#frag".into(),
            title: "Tiêu đề: \"đặc biệt\"".into(),
            markdown: md.into(),
            via: "readability".into(),
            blob_key: Some("k.html".into()),
        })
        .await
        .unwrap();
    let got = repo.get_article(&a.id).await.unwrap().unwrap();
    assert_eq!(got, a);
    assert_eq!(got.markdown, md);
    assert_eq!(got.via, "readability");
}

async fn list_is_newest_first_with_paging_and_highlight_counts(repo: &dyn Repo) {
    assert!(repo.list_articles(10, 0).await.unwrap().is_empty());
    let a = article(repo, "https://x/a", "A", "a").await;
    let b = article(repo, "https://x/b", "B", "b").await;
    let c = article(repo, "https://x/c", "C", "c").await;
    repo.add_highlight(new_highlight(&b.id, "first")).await.unwrap();
    repo.add_highlight(new_highlight(&b.id, "second")).await.unwrap();

    let ids = |v: Vec<ArticleSummary>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
    let all = repo.list_articles(10, 0).await.unwrap();
    assert_eq!(ids(all.clone()), vec![c.id.clone(), b.id.clone(), a.id.clone()]);
    assert_eq!(all[1].highlight_count, 2);
    assert_eq!(all[0].highlight_count, 0);
    assert_eq!(all[2].highlight_count, 0);
    assert_eq!(all[0].url, "https://x/c");
    assert_eq!(all[0].title, "C");
    assert_eq!(all[0].words, 1);
    assert_eq!(all[0].via, "manual");
    assert_eq!(all[0].saved_at, c.saved_at);

    assert_eq!(ids(repo.list_articles(2, 0).await.unwrap()), vec![c.id.clone(), b.id.clone()]);
    assert_eq!(ids(repo.list_articles(2, 2).await.unwrap()), vec![a.id.clone()]);
    assert!(repo.list_articles(10, 5).await.unwrap().is_empty());
    assert!(repo.list_articles(0, 0).await.unwrap().is_empty());
}

async fn upsert_moves_article_to_top_of_list(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "a").await;
    let b = article(repo, "https://x/b", "B", "b").await;
    article(repo, "https://x/a", "A2", "a").await;
    let ids: Vec<_> = repo.list_articles(10, 0).await.unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![a.id, b.id]);
}

async fn delete_removes_article_and_its_highlights(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "a").await;
    let b = article(repo, "https://x/b", "B", "b").await;
    repo.add_highlight(new_highlight(&a.id, "one")).await.unwrap();
    repo.add_highlight(new_highlight(&a.id, "two")).await.unwrap();
    let kept = repo.add_highlight(new_highlight(&b.id, "keep")).await.unwrap();

    repo.delete_article(&a.id).await.unwrap();

    assert!(repo.get_article(&a.id).await.unwrap().is_none());
    assert_eq!(repo.list_highlights(None).await.unwrap(), vec![kept]);
    assert!(repo.list_highlights(Some(&a.id)).await.unwrap().is_empty());
    assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 1);
    assert!(matches!(repo.delete_article(&a.id).await.unwrap_err(), RepoError::NotFound(_)));
    assert!(matches!(repo.delete_article("never-existed").await.unwrap_err(), RepoError::NotFound(_)));

    // The url is free again after deletion.
    let again = article(repo, "https://x/a", "A again", "a").await;
    assert_ne!(again.id, a.id);
}

async fn add_highlight_rejects_blank_text_and_missing_article(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "a").await;

    for blank in ["", "   ", "\n\t "] {
        let err = repo.add_highlight(new_highlight(&a.id, blank)).await.unwrap_err();
        assert!(matches!(err, RepoError::Invalid(_)), "blank {blank:?}: {err:?}");
    }
    let err = repo.add_highlight(new_highlight("no-such-article", "text")).await.unwrap_err();
    assert!(matches!(err, RepoError::NotFound(_)), "{err:?}");
    // Blank text is checked before the article lookup.
    let err = repo.add_highlight(new_highlight("no-such-article", "  ")).await.unwrap_err();
    assert!(matches!(err, RepoError::Invalid(_)), "{err:?}");

    assert!(repo.list_highlights(None).await.unwrap().is_empty());
}

async fn add_highlight_keeps_text_and_note(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "a").await;
    let h = repo
        .add_highlight(NewHighlight {
            article_id: a.id.clone(),
            text: "  keep my spacing \"as is\"  ".into(),
            note: Some("a note".into()),
        })
        .await
        .unwrap();
    assert_eq!(h.id.len(), 32);
    assert_eq!(h.article_id, a.id);
    assert_eq!(h.text, "  keep my spacing \"as is\"  ");
    assert_eq!(h.note.as_deref(), Some("a note"));
    let plain = repo.add_highlight(new_highlight(&a.id, "no note")).await.unwrap();
    assert_eq!(plain.note, None);
    assert_eq!(repo.list_highlights(Some(&a.id)).await.unwrap(), vec![h, plain]);
}

async fn list_highlights_is_oldest_first_and_filters_by_article(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "a").await;
    let b = article(repo, "https://x/b", "B", "b").await;
    let h1 = repo.add_highlight(new_highlight(&a.id, "one")).await.unwrap();
    let h2 = repo.add_highlight(new_highlight(&b.id, "two")).await.unwrap();
    let h3 = repo.add_highlight(new_highlight(&a.id, "three")).await.unwrap();

    assert_eq!(repo.list_highlights(Some(&a.id)).await.unwrap(), vec![h1.clone(), h3.clone()]);
    assert_eq!(repo.list_highlights(Some(&b.id)).await.unwrap(), vec![h2.clone()]);
    assert_eq!(repo.list_highlights(None).await.unwrap(), vec![h1, h2, h3]);
    assert!(repo.list_highlights(Some("unknown")).await.unwrap().is_empty());
}

async fn search_blank_and_no_match_return_empty(repo: &dyn Repo) {
    article(repo, "https://x/a", "Rust async", "Notes about tokio.").await;
    for q in ["", "   ", "\t\n "] {
        assert!(repo.search(q, 10).await.unwrap().is_empty(), "query {q:?}");
    }
    assert!(repo.search("zzzqqq", 10).await.unwrap().is_empty());
    assert!(repo.search("tokio", 0).await.unwrap().is_empty());
}

async fn search_finds_title_body_and_highlight_matches(repo: &dyn Repo) {
    let by_title = article(repo, "https://x/t", "Zeppelin history", "Nothing relevant here.").await;
    let by_body = article(repo, "https://x/b", "Gardening", "Tomatoes need a lot of sunlight.").await;
    let by_highlight = article(repo, "https://x/h", "Cooking", "soup and bread").await;
    repo.add_highlight(new_highlight(&by_highlight.id, "Fermentation is the secret"))
        .await
        .unwrap();
    article(repo, "https://x/n", "Unrelated", "completely different words").await;

    let only = |hits: Vec<SearchHit>| {
        assert_eq!(hits.len(), 1, "{hits:?}");
        hits.into_iter().next().unwrap()
    };

    let hit = only(repo.search("zeppelin", 10).await.unwrap());
    assert_eq!(hit.article_id, by_title.id);
    assert_eq!(hit.title, "Zeppelin history");
    assert!(hit.score > 0.0);

    let hit = only(repo.search("sunlight", 10).await.unwrap());
    assert_eq!(hit.article_id, by_body.id);
    assert!(hit.snippet.contains("sunlight"), "{:?}", hit.snippet);

    // A highlight-only match surfaces its article; the snippet comes from the markdown.
    let hit = only(repo.search("fermentation", 10).await.unwrap());
    assert_eq!(hit.article_id, by_highlight.id);
    assert_eq!(hit.title, "Cooking");
    assert!(collapse("soup and bread").contains(&hit.snippet));

    // Case-insensitive.
    assert_eq!(only(repo.search("ZEPPELIN", 10).await.unwrap()).article_id, by_title.id);
}

async fn search_highlights_of_deleted_article_are_gone(repo: &dyn Repo) {
    let a = article(repo, "https://x/a", "A", "plain body").await;
    repo.add_highlight(new_highlight(&a.id, "Obscurantism")).await.unwrap();
    assert_eq!(repo.search("obscurantism", 10).await.unwrap().len(), 1);
    repo.delete_article(&a.id).await.unwrap();
    assert!(repo.search("obscurantism", 10).await.unwrap().is_empty());
    assert!(repo.search("plain", 10).await.unwrap().is_empty());
}

async fn search_ranks_dominant_match_first_and_respects_limit(repo: &dyn Repo) {
    // Strong: in the title, repeated in a short body, and in a highlight.
    let strong = article(
        repo,
        "https://x/strong",
        "Quantum quantum",
        "Quantum computing. Quantum bits. Quantum gates. Quantum quantum quantum.",
    )
    .await;
    repo.add_highlight(new_highlight(&strong.id, "quantum supremacy")).await.unwrap();
    // Weak: a single mention buried in a long unrelated body.
    let filler = "bread flour water yeast salt oven loaf crust ".repeat(60);
    let weak = article(repo, "https://x/weak", "Baking", &format!("{filler} quantum {filler}")).await;
    article(repo, "https://x/none", "Other", "nothing to see").await;

    let hits = repo.search("quantum", 10).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.article_id.as_str()).collect();
    assert_eq!(ids, vec![strong.id.as_str(), weak.id.as_str()]);
    assert!(hits[0].score >= hits[1].score);
    assert!(hits.iter().all(|h| h.score > 0.0));

    let one = repo.search("quantum", 1).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].article_id, strong.id);
}

async fn search_snippet_is_utf8_safe_collapsed_and_bounded(repo: &dyn Repo) {
    let filler = "Đây là một đoạn văn dài về kinh tế vĩ mô của quốc gia. ".repeat(10);
    let markdown = format!("{filler}Lãi suất ngân hàng tăng mạnh trong quý này.\n\n{filler}");
    let a = article(repo, "https://x/vi", "Kinh tế", &markdown).await;

    let hits = repo.search("lãi", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].article_id, a.id);
    let snippet = &hits[0].snippet;
    assert!(snippet.contains("Lãi suất"), "snippet was {snippet:?}");
    assert!(!snippet.contains('\n'));
    assert!(!snippet.contains("  "));
    assert!(snippet.chars().count() <= 200, "{} chars", snippet.chars().count());
    assert!(collapse(&markdown).contains(snippet.as_str()));

    // Match at the very start, with newlines and runs of spaces collapsed.
    article(repo, "https://x/s", "Tiêu đề", "Ngân hàng\n  giữ nguyên\t\tlãi").await;
    let hits = repo.search("ngân", 10).await.unwrap();
    let short = hits.iter().find(|h| h.title == "Tiêu đề").unwrap();
    assert_eq!(short.snippet, "Ngân hàng giữ nguyên lãi");
}

async fn search_snippet_for_title_only_match_is_still_a_markdown_excerpt(repo: &dyn Repo) {
    let body = format!("{}\n\nunrelated   words here", "lorem ipsum ".repeat(100));
    article(repo, "https://x/a", "Platypus", &body).await;
    let hits = repo.search("platypus", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    let s = &hits[0].snippet;
    assert!(!s.is_empty());
    assert!(s.chars().count() <= 200);
    assert!(!s.contains('\n'));
    assert!(collapse(&body).contains(s.as_str()), "{s:?}");
}

async fn synthesis_roundtrip_list_is_newest_first_and_filters_by_kind(repo: &dyn Repo) {
    assert!(repo.list_syntheses(None, 10).await.unwrap().is_empty());
    let mk = |kind, title: &str| NewSynthesis {
        kind,
        title: title.into(),
        direction: None,
        body: "body".into(),
        article_ids: vec![],
    };
    let b1 = repo.save_synthesis(mk(SynthesisKind::Brief, "b1")).await.unwrap();
    let a1 = repo
        .save_synthesis(NewSynthesis {
            kind: SynthesisKind::Analysis,
            title: "a1".into(),
            direction: Some("focus on \"costs\"".into()),
            body: "# Body\n\nwith markdown".into(),
            article_ids: vec!["id-one".into(), "id-two".into()],
        })
        .await
        .unwrap();
    let b2 = repo.save_synthesis(mk(SynthesisKind::Brief, "b2")).await.unwrap();
    assert_eq!(b1.id.len(), 32);
    assert_eq!(a1.direction.as_deref(), Some("focus on \"costs\""));

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
    assert!(repo.list_syntheses(None, 0).await.unwrap().is_empty());

    assert_eq!(repo.get_synthesis(&a1.id).await.unwrap(), Some(a1.clone()));
    assert_eq!(repo.get_synthesis(&b1.id).await.unwrap(), Some(b1));
    assert!(repo.get_synthesis("missing").await.unwrap().is_none());
    let listed = repo.list_syntheses(Some(SynthesisKind::Analysis), 1).await.unwrap();
    assert_eq!(listed, vec![a1]);
}

async fn log_usage_accepts_any_json_detail(repo: &dyn Repo) {
    repo.log_usage("save_url", json!({"url": "https://x/1", "n": 3, "nested": {"a": [1, 2, null]}}))
        .await
        .unwrap();
    repo.log_usage("chat", json!(null)).await.unwrap();
    repo.log_usage("brief", json!("plain string")).await.unwrap();
    repo.log_usage("brief", json!({})).await.unwrap();
    repo.log_usage("brief", json!([1, "two"])).await.unwrap();
}

// ---------------------------------------------------------------- backup and restore (two repos)

/// A library with every field shape: unicode, blob key, highlights with and without notes, both synthesis kinds,
/// a dangling article id in a synthesis, and an upserted article (so saved_at is not insertion order).
async fn populate(repo: &dyn Repo) {
    let a = repo
        .upsert_article(NewArticle {
            url: "https://x/vi".into(),
            title: "Tiêu đề: \"đặc biệt\"".into(),
            markdown: "# Kinh tế\n\nLãi suất ngân hàng tăng.".into(),
            via: "readability".into(),
            blob_key: Some("0123abcd.html".into()),
        })
        .await
        .unwrap();
    let b = article(repo, "https://x/b", "Beta", "beta body text").await;
    article(repo, "https://x/c", "Gamma", "gamma").await;
    article(repo, "https://x/b", "Beta renamed", "beta body text again").await;
    repo.add_highlight(new_highlight(&a.id, "Lãi suất")).await.unwrap();
    repo.add_highlight(NewHighlight { article_id: a.id.clone(), text: "ngân hàng".into(), note: Some("why?".into()) })
        .await
        .unwrap();
    repo.add_highlight(new_highlight(&b.id, "beta")).await.unwrap();
    repo.save_synthesis(NewSynthesis {
        kind: SynthesisKind::Analysis,
        title: "Analysis: A".into(),
        direction: Some("focus on \"costs\"".into()),
        body: "# Body\n\nwith markdown".into(),
        article_ids: vec![a.id.clone(), "long-gone".into()],
    })
    .await
    .unwrap();
    repo.save_synthesis(NewSynthesis {
        kind: SynthesisKind::Brief,
        title: "Brief".into(),
        direction: None,
        body: "short".into(),
        article_ids: vec![],
    })
    .await
    .unwrap();
}

/// Same rows, ignoring when the export was taken.
fn same_rows(a: &LibraryExport, b: &LibraryExport) {
    assert_eq!(a.version, b.version);
    assert_eq!(a.articles, b.articles);
    assert_eq!(a.highlights, b.highlights);
    assert_eq!(a.syntheses, b.syntheses);
}

async fn restore_roundtrip_keeps_ids_and_timestamps(src: &dyn Repo, dst: &dyn Repo) {
    populate(src).await;
    let before = export_library(src).await.unwrap();
    assert_eq!(before.version, 1);
    assert_eq!((before.articles.len(), before.highlights.len(), before.syntheses.len()), (3, 3, 2));

    // The export survives JSON, which is what the backup script stores.
    let json = serde_json::to_string(&before).unwrap();
    let parsed: LibraryExport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, before);

    dst.restore(&parsed).await.unwrap();
    let after = export_library(dst).await.unwrap();
    same_rows(&before, &after);

    // Reads agree with the source row for row.
    for a in &before.articles {
        assert_eq!(dst.get_article(&a.id).await.unwrap().as_ref(), Some(a));
    }
    let by_id = |mut v: Vec<ArticleSummary>| {
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    };
    let listed = by_id(dst.list_articles(10, 0).await.unwrap());
    assert_eq!(listed, by_id(src.list_articles(10, 0).await.unwrap()));
    assert_eq!(listed.iter().map(|s| s.highlight_count).sum::<u32>(), 3);
    for s in &before.syntheses {
        assert_eq!(dst.get_synthesis(&s.id).await.unwrap().as_ref(), Some(s));
    }

    // The restored library is a normal one: search works, urls still dedupe onto the restored ids, new rows add.
    assert_eq!(dst.search("Gamma", 5).await.unwrap().len(), 1);
    assert_eq!(dst.search("suất", 5).await.unwrap().len(), 1);
    let vi = before.articles.iter().find(|a| a.url == "https://x/vi").unwrap();
    let again = article(dst, "https://x/vi", "Renamed", "new text").await;
    assert_eq!(again.id, vi.id);
    let h = dst.add_highlight(new_highlight(&vi.id, "new")).await.unwrap();
    assert_eq!(dst.list_highlights(Some(&vi.id)).await.unwrap().len(), 3);
    assert_eq!(h.id.len(), 32);
    assert_eq!(dst.list_articles(10, 0).await.unwrap().len(), 3);
}

async fn restore_refuses_a_library_that_has_data(src: &dyn Repo, dst: &dyn Repo) {
    populate(src).await;
    let export = export_library(src).await.unwrap();

    // Restoring onto the source itself, and onto a repo holding any one kind of row, is Invalid and changes nothing.
    assert!(matches!(src.restore(&export).await, Err(RepoError::Invalid(_))));
    same_rows(&export, &export_library(src).await.unwrap());

    dst.save_synthesis(NewSynthesis { kind: SynthesisKind::Brief, title: "mine".into(), direction: None, body: "b".into(), article_ids: vec![] })
        .await
        .unwrap();
    let mine = export_library(dst).await.unwrap();
    let err = dst.restore(&export).await.unwrap_err();
    assert!(matches!(err, RepoError::Invalid(_)), "{err:?}");
    same_rows(&mine, &export_library(dst).await.unwrap());
}

async fn restore_rejects_a_bad_export_and_leaves_the_repo_empty(src: &dyn Repo, dst: &dyn Repo) {
    populate(src).await;
    let good = export_library(src).await.unwrap();
    validate_export(&good).unwrap();

    let mut bad_version = good.clone();
    bad_version.version = 2;
    let mut orphan = good.clone();
    orphan.highlights[0].article_id = "nope".into();
    let mut dup_id = good.clone();
    dup_id.articles[1].id = dup_id.articles[0].id.clone();
    let mut dup_url = good.clone();
    dup_url.articles[1].url = dup_url.articles[0].url.clone();
    let mut dup_hl = good.clone();
    dup_hl.highlights[1].id = dup_hl.highlights[0].id.clone();
    let mut dup_syn = good.clone();
    dup_syn.syntheses[1].id = dup_syn.syntheses[0].id.clone();
    let mut blank_id = good.clone();
    blank_id.articles[0].id = String::new();

    for (name, bad) in [
        ("version", bad_version),
        ("orphan highlight", orphan),
        ("duplicate article id", dup_id),
        ("duplicate url", dup_url),
        ("duplicate highlight id", dup_hl),
        ("duplicate synthesis id", dup_syn),
        ("blank id", blank_id),
    ] {
        let err = dst.restore(&bad).await.unwrap_err();
        assert!(matches!(err, RepoError::Invalid(_)), "{name}: {err:?}");
        let now = export_library(dst).await.unwrap();
        assert!(now.articles.is_empty() && now.highlights.is_empty() && now.syntheses.is_empty(), "{name} left rows behind");
    }

    // Still empty, so a good export loads afterwards.
    dst.restore(&good).await.unwrap();
    same_rows(&good, &export_library(dst).await.unwrap());
}

async fn restoring_an_empty_export_is_fine_and_keeps_the_repo_empty(src: &dyn Repo, dst: &dyn Repo) {
    let empty = export_library(src).await.unwrap();
    assert!(empty.articles.is_empty() && empty.highlights.is_empty() && empty.syntheses.is_empty());
    dst.restore(&empty).await.unwrap();
    assert!(dst.list_articles(10, 0).await.unwrap().is_empty());
    // Still empty, so a real restore is allowed.
    populate(src).await;
    dst.restore(&export_library(src).await.unwrap()).await.unwrap();
    assert_eq!(dst.list_articles(10, 0).await.unwrap().len(), 3);
}

async fn export_drops_deleted_articles_and_their_highlights(repo: &dyn Repo) {
    // Deleting an article takes its highlights with it, so they are absent from the export.
    let a = article(repo, "https://x/a", "A", "a").await;
    let b = article(repo, "https://x/b", "B", "b").await;
    repo.add_highlight(new_highlight(&a.id, "one")).await.unwrap();
    repo.add_highlight(new_highlight(&b.id, "two")).await.unwrap();
    repo.delete_article(&a.id).await.unwrap();
    let export = export_library(repo).await.unwrap();
    assert_eq!(export.articles.len(), 1);
    assert_eq!(export.highlights.len(), 1);
    assert_eq!(export.highlights[0].article_id, b.id);
}

// ------------------------------------------------------- instantiation

macro_rules! contract {
    ($adapter:ident, $make:expr; $($check:ident),+ $(,)?) => {
        mod $adapter {
            use super::*;
            $(
                #[tokio::test]
                async fn $check() {
                    let repo = $make;
                    super::$check(&repo).await;
                }
            )+
        }
    };
}

macro_rules! contract_all {
    ($adapter:ident, $make:expr) => {
        contract!($adapter, $make;
            upsert_dedupes_by_url_and_keeps_id,
            article_roundtrips_unicode_and_all_fields,
            list_is_newest_first_with_paging_and_highlight_counts,
            upsert_moves_article_to_top_of_list,
            delete_removes_article_and_its_highlights,
            add_highlight_rejects_blank_text_and_missing_article,
            add_highlight_keeps_text_and_note,
            list_highlights_is_oldest_first_and_filters_by_article,
            search_blank_and_no_match_return_empty,
            search_finds_title_body_and_highlight_matches,
            search_highlights_of_deleted_article_are_gone,
            search_ranks_dominant_match_first_and_respects_limit,
            search_snippet_is_utf8_safe_collapsed_and_bounded,
            search_snippet_for_title_only_match_is_still_a_markdown_excerpt,
            synthesis_roundtrip_list_is_newest_first_and_filters_by_kind,
            log_usage_accepts_any_json_detail,
            export_drops_deleted_articles_and_their_highlights,
        );
    };
}

/// Checks that need a second, fresh repo of the same adapter: `check(source, destination)`.
macro_rules! restore_contract {
    ($adapter:ident, $make:expr; $($check:ident),+ $(,)?) => {
        mod $adapter {
            use super::*;
            $(
                #[tokio::test]
                async fn $check() {
                    let src = $make;
                    let dst = $make;
                    super::$check(&src, &dst).await;
                }
            )+
        }
    };
}

macro_rules! restore_all {
    ($adapter:ident, $make:expr) => {
        restore_contract!($adapter, $make;
            restore_roundtrip_keeps_ids_and_timestamps,
            restore_refuses_a_library_that_has_data,
            restore_rejects_a_bad_export_and_leaves_the_repo_empty,
            restoring_an_empty_export_is_fine_and_keeps_the_repo_empty,
        );
    };
}

contract_all!(mem, MemRepo::new());
contract_all!(surreal, SurrealRepo::memory().await.unwrap());
restore_all!(mem_restore, MemRepo::new());
restore_all!(surreal_restore, SurrealRepo::memory().await.unwrap());

// ------------------------------------------------- SurrealRepo-only

#[tokio::test]
async fn surreal_data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");

    let (a, h, s) = {
        let repo = SurrealRepo::open(&path).await.unwrap();
        let a = repo
            .upsert_article(NewArticle {
                blob_key: Some("k.html".into()),
                ..new_article("https://x/persist", "Persistent title", "durable body text")
            })
            .await
            .unwrap();
        let h = repo
            .add_highlight(NewHighlight {
                article_id: a.id.clone(),
                text: "remember this".into(),
                note: Some("n".into()),
            })
            .await
            .unwrap();
        let s = repo
            .save_synthesis(NewSynthesis {
                kind: SynthesisKind::Brief,
                title: "t".into(),
                direction: Some("d".into()),
                body: "b".into(),
                article_ids: vec![a.id.clone()],
            })
            .await
            .unwrap();
        repo.log_usage("save_url", json!({"url": "https://x/persist"})).await.unwrap();
        (a, h, s)
    };

    let repo = SurrealRepo::open(&path).await.unwrap();
    assert_eq!(repo.get_article(&a.id).await.unwrap(), Some(a.clone()));
    assert_eq!(repo.list_highlights(Some(&a.id)).await.unwrap(), vec![h]);
    assert_eq!(repo.get_synthesis(&s.id).await.unwrap(), Some(s));
    assert_eq!(repo.list_articles(10, 0).await.unwrap()[0].highlight_count, 1);
    assert_eq!(
        repo.usage().await.unwrap(),
        vec![("save_url".to_string(), json!({"url": "https://x/persist"}))]
    );

    // Full-text indexes were persisted too, and schema setup on re-open is idempotent.
    for q in ["persistent", "durable", "remember"] {
        let hits = repo.search(q, 10).await.unwrap();
        assert_eq!(hits.len(), 1, "query {q}");
        assert_eq!(hits[0].article_id, a.id);
    }
    // The unique url index still dedupes after re-open.
    let again = repo
        .upsert_article(new_article("https://x/persist", "Renamed", "durable body text"))
        .await
        .unwrap();
    assert_eq!(again.id, a.id);
    assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn surreal_usage_log_is_append_only_in_order() {
    let repo = SurrealRepo::memory().await.unwrap();
    repo.log_usage("save_url", json!({"url": "https://x/1"})).await.unwrap();
    repo.log_usage("chat", json!({"session": "s1"})).await.unwrap();
    assert_eq!(
        repo.usage().await.unwrap(),
        vec![
            ("save_url".to_string(), json!({"url": "https://x/1"})),
            ("chat".to_string(), json!({"session": "s1"})),
        ]
    );
}

#[tokio::test]
async fn surreal_unique_url_index_holds_under_concurrent_upserts() {
    let repo = std::sync::Arc::new(SurrealRepo::memory().await.unwrap());
    let mut tasks = Vec::new();
    for i in 0..8 {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            repo.upsert_article(new_article("https://x/same", &format!("T{i}"), "body")).await
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for t in tasks {
        ids.insert(t.await.unwrap().unwrap().id);
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(repo.list_articles(10, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn surreal_restored_library_survives_reopen() {
    let src = SurrealRepo::memory().await.unwrap();
    populate(&src).await;
    let export = export_library(&src).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let repo = SurrealRepo::open(&path).await.unwrap();
        repo.restore(&export).await.unwrap();
    }
    let repo = SurrealRepo::open(&path).await.unwrap();
    same_rows(&export, &export_library(&repo).await.unwrap());
    assert!(matches!(repo.restore(&export).await, Err(RepoError::Invalid(_))));
}
