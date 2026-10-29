//! Service-layer tests: MemRepo + FsBlobStore on a tempdir + a fake OpenAI-compatible server. No network.

use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, routing::{get, post}};
use marginalia::blob::FsBlobStore;
use marginalia::domain::*;
use marginalia::extract::{Via, html_to_article};
use marginalia::mem::MemRepo;
use marginalia::repo::{BlobStore, Repo};
use marginalia::service::{App, AppConfig, AppError, LlmConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ---------- fakes ----------

type Script = Box<dyn Fn(&Value, usize) -> Value + Send + Sync>;

struct FakeLlm {
    seen: Mutex<Vec<Value>>,
    script: Script,
}

async fn completions(State(fake): State<Arc<FakeLlm>>, Json(body): Json<Value>) -> Json<Value> {
    let index = {
        let mut seen = fake.seen.lock().unwrap();
        seen.push(body.clone());
        seen.len() - 1
    };
    Json(json!({ "choices": [{ "message": (fake.script)(&body, index) }] }))
}

/// Starts a fake /chat/completions server; `script(request_body, call_index)` returns the assistant message.
async fn fake_llm(script: impl Fn(&Value, usize) -> Value + Send + Sync + 'static) -> (LlmConfig, Arc<FakeLlm>) {
    let fake = Arc::new(FakeLlm { seen: Mutex::new(Vec::new()), script: Box::new(script) });
    let app = Router::new().route("/chat/completions", post(completions)).with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let llm = LlmConfig { base_url: format!("http://{addr}"), api_key: "test-key".into(), model: "fake-model".into() };
    (llm, fake)
}

fn say(text: &str) -> Value {
    json!({ "content": text })
}

fn call_tools(calls: &[(&str, Value)]) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({ "id": format!("call{i}"), "type": "function", "function": { "name": name, "arguments": args.to_string() } })
        })
        .collect();
    json!({ "content": null, "tool_calls": calls })
}

struct Fixture {
    app: App,
    repo: Arc<MemRepo>,
    blobs: Arc<FsBlobStore>,
    _dir: tempfile::TempDir,
}

fn fixture(llm: Option<LlmConfig>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemRepo::new());
    let blobs = Arc::new(FsBlobStore::new(dir.path()));
    let app = App::new(repo.clone(), blobs.clone(), AppConfig { llm, firecrawl_key: None });
    Fixture { app, repo, blobs, _dir: dir }
}

fn user_messages(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

// ---------- save_markdown / add_highlight ----------

#[tokio::test]
async fn save_markdown_and_highlight_happy_path() {
    let f = fixture(None);
    let article = f.app.save_markdown("https://x.test/a", "  Rate cuts  ", "Central banks cut rates.").await.unwrap();
    assert_eq!(article.title, "Rate cuts");
    assert_eq!(article.via, "manual");
    assert_eq!(article.blob_key, None);
    assert_eq!(article.words, 4);

    let h = f.app.add_highlight(&article.id, "cut rates", Some("  key point ")).await.unwrap();
    assert_eq!(h.article_id, article.id);
    assert_eq!(h.note.as_deref(), Some("key point"));
    let no_note = f.app.add_highlight(&article.id, "Central banks", Some("   ")).await.unwrap();
    assert_eq!(no_note.note, None);

    assert_eq!(f.repo.list_highlights(Some(&article.id)).await.unwrap().len(), 2);
    assert!(!f.app.has_llm());
}

#[tokio::test]
async fn save_markdown_and_highlight_validate_input() {
    let f = fixture(None);
    for (url, title, md) in [("", "T", "body"), ("  ", "T", "body"), ("https://x.test", "", "body"), ("https://x.test", "T", "  \n")] {
        let err = f.app.save_markdown(url, title, md).await.unwrap_err();
        assert!(matches!(err, AppError::Invalid(_)), "{url:?} {title:?} {md:?}: {err:?}");
    }
    let article = f.app.save_markdown("https://x.test/a", "A", "text").await.unwrap();
    assert!(matches!(f.app.add_highlight(&article.id, "  ", None).await.unwrap_err(), AppError::Invalid(_)));
    assert!(matches!(f.app.add_highlight("", "x", None).await.unwrap_err(), AppError::Invalid(_)));
    assert!(matches!(f.app.add_highlight("missing", "x", None).await.unwrap_err(), AppError::NotFound(_)));
    assert!(matches!(f.app.save_url("   ").await.unwrap_err(), AppError::Invalid(_)));
    assert!(matches!(f.app.save_url("ftp://x.test/a").await.unwrap_err(), AppError::Invalid(_)));
}

// ---------- save_url (against a local page, still no external network) ----------

fn sodium_html() -> String {
    let paragraphs: String = (0..8)
        .map(|i| format!("<p>Paragraph {i} explains how sodium-ion cells trade energy density for cost and safety, which matters for grid storage.</p>"))
        .collect();
    format!(
        "<html><head><title>Sodium cells | Site</title></head><body>
         <nav><a href=\"/\">Home</a><a href=\"/about\">About</a></nav>
         <article><h1>Sodium cells</h1><a href=\"/hero\"><img src=\"/hero.png\" alt=\"hero\"></a>{paragraphs}</article>
         <footer>Subscribe to our newsletter</footer></body></html>"
    )
}

#[test]
fn readability_extracts_article_and_drops_page_chrome() {
    let got = html_to_article(&sodium_html(), "https://x.test/sodium").unwrap();
    assert_eq!(got.via, Via::Readability);
    assert!(got.markdown.contains("sodium-ion cells"));
    assert!(!got.markdown.contains("Subscribe to our newsletter"));
    assert!(!got.markdown.contains("hero.png"), "images are dropped");
    assert!(!got.markdown.contains("[]("), "links left empty by dropped images are removed");
}

#[tokio::test]
async fn save_url_keeps_raw_html_as_blob() {
    let page = Router::new().route("/sodium", get(|| async { axum::response::Html(sodium_html()) }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, page).await.unwrap() });

    let f = fixture(None);
    let url = format!("http://{addr}/sodium");
    let article = f.app.save_url(&url).await.unwrap();
    assert_eq!(article.via, "readability");
    assert_eq!(article.url, url);
    assert!(article.markdown.contains("sodium-ion cells"));
    let key = article.blob_key.expect("raw html kept");
    assert!(key.ends_with(".html"));
    let raw = f.blobs.get(&key).await.unwrap().expect("blob stored");
    assert_eq!(String::from_utf8(raw).unwrap(), sodium_html());

    // Saving again upserts rather than duplicating.
    let again = f.app.save_url(&url).await.unwrap();
    assert_eq!(again.id, article.id);
    assert_eq!(f.repo.list_articles(10, 0).await.unwrap().len(), 1);
}

// ---------- chat ----------

#[tokio::test]
async fn chat_without_llm_is_no_llm() {
    let f = fixture(None);
    assert!(matches!(f.app.chat("s", "hello").await.unwrap_err(), AppError::NoLlm));
    assert!(matches!(f.app.brief(None).await.unwrap_err(), AppError::NoLlm));
    assert!(matches!(f.app.analyse("x", None).await.unwrap_err(), AppError::NoLlm));
}

#[tokio::test]
async fn chat_keeps_history_per_session_until_reset() {
    let (llm, fake) = fake_llm(|_, i| say(&format!("reply {i}"))).await;
    let f = fixture(Some(llm));
    assert!(f.app.has_llm());

    assert_eq!(f.app.chat("s1", "first question").await.unwrap(), "reply 0");
    assert_eq!(f.app.chat("s1", "second question").await.unwrap(), "reply 1");
    assert_eq!(f.app.chat("other", "unrelated").await.unwrap(), "reply 2");

    {
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[0]["model"], "fake-model");
        assert_eq!(seen[0]["messages"][0]["role"], "system");
        assert!(seen[0]["messages"][0]["content"].as_str().unwrap().contains("reading companion"));
        assert!(seen[0]["tools"].as_array().unwrap().len() >= 6);
        // The second request carries the first exchange, then the new question.
        let msgs = seen[1]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!((msgs[1]["role"].as_str(), msgs[1]["content"].as_str()), (Some("user"), Some("first question")));
        assert_eq!((msgs[2]["role"].as_str(), msgs[2]["content"].as_str()), (Some("assistant"), Some("reply 0")));
        assert_eq!(msgs[3]["content"], "second question");
        // Another session starts clean.
        assert_eq!(user_messages(&seen[2]), vec!["unrelated"]);
    }

    f.app.reset_chat("s1");
    f.app.chat("s1", "after reset").await.unwrap();
    let seen = fake.seen.lock().unwrap();
    assert_eq!(user_messages(&seen[3]), vec!["after reset"]);
}

#[tokio::test]
async fn chat_failure_does_not_poison_the_session() {
    // First call: the server errors (non-JSON body is not possible here, so fail via a missing message).
    let (llm, fake) = fake_llm(|_, i| if i == 0 { Value::Null } else { say("ok") }).await;
    let f = fixture(Some(llm));
    assert!(matches!(f.app.chat("s", "one").await.unwrap_err(), AppError::Upstream(_)));
    assert_eq!(f.app.chat("s", "two").await.unwrap(), "ok");
    let seen = fake.seen.lock().unwrap();
    assert_eq!(user_messages(&seen[1]), vec!["two"], "the failed exchange was rolled back");
}

#[tokio::test]
async fn agent_add_highlight_tool_writes_to_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemRepo::new());
    let a = repo
        .upsert_article(NewArticle { url: "https://x.test/a".into(), title: "Sodium".into(), markdown: "Sodium cells are cheap.".into(), via: "manual".into(), blob_key: None })
        .await
        .unwrap();
    let id = a.id.clone();
    let (llm, fake) = fake_llm(move |body, _| {
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "user" {
            call_tools(&[
                ("add_highlight", json!({ "article_id": id, "text": "cheap", "note": "why it matters" })),
                ("add_highlight", json!({ "article_id": "nope", "text": "x" })),
                ("no_such_tool", json!({})),
            ])
        } else {
            say("done")
        }
    })
    .await;
    let app = App::new(repo.clone(), Arc::new(FsBlobStore::new(dir.path())), AppConfig { llm: Some(llm), firecrawl_key: None });
    assert_eq!(app.chat("s", "highlight it").await.unwrap(), "done");
    let hs = repo.list_highlights(Some(&a.id)).await.unwrap();
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].text, "cheap");
    assert_eq!(hs[0].note.as_deref(), Some("why it matters"));

    // Failing tool calls became "Error: ..." results and did not abort the loop.
    let seen = fake.seen.lock().unwrap();
    let results: Vec<&str> = seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results[0].contains("why it matters"));
    assert!(results[1].starts_with("Error:"), "{}", results[1]);
    assert!(results[2].starts_with("Error: unknown tool"), "{}", results[2]);
}

// ---------- analyse / brief ----------

async fn two_articles(f: &Fixture) -> (Article, Article) {
    let a = f.app.save_markdown("https://x.test/a", "Sodium batteries", "Sodium cells are cheaper than lithium.").await.unwrap();
    let b = f.app.save_markdown("https://x.test/b", "Grid storage", "Grid storage needs cheap, safe cells.").await.unwrap();
    (a, b)
}

#[tokio::test]
async fn analyse_saves_an_analysis_with_direction_and_sources() {
    // The model reads the *other* article only; the script captures its id, so build the repo first.
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemRepo::new());
    let blobs = Arc::new(FsBlobStore::new(dir.path()));
    let setup = App::new(repo.clone(), blobs.clone(), AppConfig::default());
    let a = setup.save_markdown("https://x.test/a", "Sodium batteries", "Sodium cells are cheaper than lithium.").await.unwrap();
    let b = setup.save_markdown("https://x.test/b", "Grid storage", "Grid storage needs cheap, safe cells.").await.unwrap();

    let other = b.id.clone();
    let (llm, fake) = fake_llm(move |body, _| {
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "user" {
            call_tools(&[("read_article", json!({ "article_id": other })), ("read_article", json!({ "article_id": other }))])
        } else {
            say("FINAL ANALYSIS TEXT")
        }
    })
    .await;
    let app = App::new(repo.clone(), blobs, AppConfig { llm: Some(llm), firecrawl_key: None });

    let s = app.analyse(&a.id, Some("  focus on cost  ")).await.unwrap();
    assert_eq!(s.kind, SynthesisKind::Analysis);
    assert_eq!(s.title, "Analysis: Sodium batteries");
    assert_eq!(s.direction.as_deref(), Some("focus on cost"));
    assert_eq!(s.body, "FINAL ANALYSIS TEXT");
    assert!(s.article_ids.contains(&a.id), "the analysed article is always included");
    assert!(s.article_ids.contains(&b.id), "articles read during the run are included");
    assert_eq!(s.article_ids.len(), 2, "no duplicates");

    let saved = repo.get_synthesis(&s.id).await.unwrap().unwrap();
    assert_eq!(saved, s);

    let seen = fake.seen.lock().unwrap();
    let prompt = user_messages(&seen[0]).remove(0);
    assert!(prompt.contains("focus on cost"), "direction reaches the model: {prompt}");
    assert!(prompt.contains(&a.id) && prompt.contains("Sodium batteries"));
    assert!(!prompt.contains("{{"), "placeholders are all filled: {prompt}");
    // The tool result the model saw was the article text.
    let tool_msgs: Vec<&Value> = seen[1]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
    assert!(tool_msgs[0]["content"].as_str().unwrap().contains("Grid storage needs cheap"));
    // Analyse is a fresh run: no chat history leaks in.
    assert_eq!(user_messages(&seen[0]).len(), 1);
}

#[tokio::test]
async fn analyse_without_direction_and_for_a_missing_article() {
    let (llm, fake) = fake_llm(|_, _| say("body")).await;
    let f = fixture(Some(llm));
    let (a, _) = two_articles(&f).await;

    let s = f.app.analyse(&a.id, None).await.unwrap();
    assert_eq!(s.direction, None);
    assert_eq!(s.article_ids, vec![a.id.clone()]);
    {
        let seen = fake.seen.lock().unwrap();
        assert!(!user_messages(&seen[0])[0].contains("{{"));
    }

    let before = fake.seen.lock().unwrap().len();
    let err = f.app.analyse("does-not-exist", Some("x")).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    assert_eq!(fake.seen.lock().unwrap().len(), before, "no LLM call for a missing article");
    assert_eq!(f.repo.list_syntheses(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn brief_saves_a_brief_and_records_articles_read() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemRepo::new());
    let blobs = Arc::new(FsBlobStore::new(dir.path()));
    let setup = App::new(repo.clone(), blobs.clone(), AppConfig::default());
    let a = setup.save_markdown("https://x.test/a", "Sodium batteries", "Sodium cells are cheaper than lithium.").await.unwrap();
    let b = setup.save_markdown("https://x.test/b", "Grid storage", "Grid storage needs cheap, safe cells.").await.unwrap();
    let _unread = setup.save_markdown("https://x.test/c", "Unrelated", "Gardening in winter.").await.unwrap();

    let (ia, ib) = (a.id.clone(), b.id.clone());
    let (llm, fake) = fake_llm(move |_, i| match i {
        0 => call_tools(&[("list_articles", json!({})), ("search_library", json!({ "query": "sodium cells" }))]),
        1 => call_tools(&[
            ("read_article", json!({ "article_id": "bogus-id" })),
            ("read_article", json!({ "article_id": ia })),
            ("read_article", json!({ "id": ib })),
        ]),
        _ => say("BRIEF BODY"),
    })
    .await;
    let app = App::new(repo.clone(), blobs, AppConfig { llm: Some(llm), firecrawl_key: None });

    let s = app.brief(Some("energy storage")).await.unwrap();
    assert_eq!(s.kind, SynthesisKind::Brief);
    assert_eq!(s.title, "Brief: energy storage");
    assert_eq!(s.direction.as_deref(), Some("energy storage"));
    assert_eq!(s.body, "BRIEF BODY");
    assert_eq!(s.article_ids, vec![a.id.clone(), b.id.clone()], "only successfully read articles, in read order");

    let seen = fake.seen.lock().unwrap();
    assert!(user_messages(&seen[0])[0].contains("energy storage"));
    let results: Vec<String> = seen[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(results.len(), 5);
    assert!(results[0].contains("Sodium batteries"), "list_articles result");
    assert!(results[1].contains(&a.id), "search_library result");
    assert!(results[2].starts_with("Error:"), "tool errors are tool results: {}", results[2]);
    assert!(results[3].starts_with("# Sodium batteries"));
}

#[tokio::test]
async fn brief_without_direction_is_titled_with_the_date_and_runaway_loops_are_errors() {
    let (llm, fake) = fake_llm(|_, _| say("body")).await;
    let f = fixture(Some(llm));
    let s = f.app.brief(Some("   ")).await.unwrap();
    assert_eq!(s.direction, None);
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert!(s.title == format!("Brief: {date}") || s.title.starts_with("Brief: 20"), "{}", s.title);
    assert!(s.article_ids.is_empty());
    drop(fake);

    // A model that never stops calling tools hits the step limit; that is not saved as a synthesis.
    let (llm, fake) = fake_llm(|_, _| call_tools(&[("list_articles", json!({}))])).await;
    let f = fixture(Some(llm));
    let err = f.app.brief(None).await.unwrap_err();
    assert!(matches!(err, AppError::Upstream(_)), "{err:?}");
    assert_eq!(fake.seen.lock().unwrap().len(), 10);
    assert!(f.repo.list_syntheses(None, 10).await.unwrap().is_empty());

    // Chat, by contrast, returns the step-limit note.
    let reply = f.app.chat("s", "go").await.unwrap();
    assert!(reply.contains("too many tool steps"));
}

#[tokio::test]
async fn read_article_truncates_long_bodies_on_a_char_boundary() {
    let long = "é".repeat(30_000); // 60k bytes
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemRepo::new());
    let a = repo
        .upsert_article(NewArticle { url: "https://x.test/long".into(), title: "Long".into(), markdown: long, via: "manual".into(), blob_key: None })
        .await
        .unwrap();
    let id = a.id.clone();
    let (llm, fake) = fake_llm(move |_, i| if i == 0 { call_tools(&[("read_article", json!({ "article_id": id }))]) } else { say("ok") }).await;
    let app = App::new(repo, Arc::new(FsBlobStore::new(dir.path())), AppConfig { llm: Some(llm), firecrawl_key: None });
    app.chat("s", "read it").await.unwrap();
    let seen = fake.seen.lock().unwrap();
    let content = seen[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(content.ends_with("(truncated)"));
    assert!(content.len() < 40_200, "{}", content.len());
}

#[test]
fn app_error_maps_repo_errors() {
    use marginalia::repo::RepoError;
    assert!(matches!(AppError::from(RepoError::NotFound("x".into())), AppError::NotFound(_)));
    assert!(matches!(AppError::from(RepoError::Invalid("x".into())), AppError::Invalid(_)));
    assert!(matches!(AppError::from(RepoError::Backend("x".into())), AppError::Repo(_)));
}

// ---------- export / import ----------

#[tokio::test]
async fn export_then_import_into_a_fresh_library_keeps_everything() {
    let src = fixture(None);
    let (a, b) = two_articles(&src).await;
    let h = src.app.add_highlight(&a.id, "first", Some("note")).await.unwrap();
    let s = src
        .repo
        .save_synthesis(NewSynthesis { kind: SynthesisKind::Brief, title: "T".into(), direction: Some("d".into()), body: "body".into(), article_ids: vec![a.id.clone(), b.id.clone()] })
        .await
        .unwrap();

    let export = src.app.export().await.unwrap();
    assert_eq!(export.version, 1);
    assert_eq!((export.articles.len(), export.highlights.len(), export.syntheses.len()), (2, 1, 1));
    assert!(export.articles.contains(&a) && export.articles.contains(&b));
    assert_eq!(export.highlights, vec![h.clone()]);
    assert_eq!(export.syntheses, vec![s.clone()]);

    // Through JSON, as the backup file is.
    let json = serde_json::to_vec(&export).unwrap();
    let parsed: LibraryExport = serde_json::from_slice(&json).unwrap();

    let dst = fixture(None);
    dst.app.import(parsed).await.unwrap();
    assert_eq!(dst.repo.get_article(&a.id).await.unwrap(), Some(a.clone()));
    assert_eq!(dst.repo.list_highlights(None).await.unwrap(), vec![h]);
    assert_eq!(dst.repo.get_synthesis(&s.id).await.unwrap(), Some(s));
    let again = dst.app.export().await.unwrap();
    assert_eq!((again.articles, again.highlights, again.syntheses), (export.articles, export.highlights, export.syntheses));
    // Imports are noted in the usage log.
    assert!(dst.repo.usage().await.iter().any(|(kind, _)| kind == "import"));
}

#[tokio::test]
async fn import_refuses_a_non_empty_library_and_a_bad_snapshot() {
    let src = fixture(None);
    src.app.save_markdown("https://x.test/a", "A", "body").await.unwrap();
    let export = src.app.export().await.unwrap();

    // Non-empty: refused, and what was there stays.
    let err = src.app.import(export.clone()).await.unwrap_err();
    assert!(matches!(&err, AppError::Invalid(m) if m.contains("empty")), "{err:?}");
    assert_eq!(src.app.export().await.unwrap().articles, export.articles);

    // Unsupported version.
    let dst = fixture(None);
    let err = dst.app.import(LibraryExport { version: 99, ..export.clone() }).await.unwrap_err();
    assert!(matches!(err, AppError::Invalid(_)), "{err:?}");
    assert!(dst.app.export().await.unwrap().articles.is_empty());

    // An empty library exports to a valid, importable (empty) snapshot.
    let empty = dst.app.export().await.unwrap();
    assert_eq!(empty.version, 1);
    fixture(None).app.import(empty).await.unwrap();
}

#[tokio::test]
async fn export_pages_through_more_articles_than_one_page() {
    let f = fixture(None);
    for i in 0..205 {
        f.app.save_markdown(&format!("https://x.test/{i}"), &format!("T{i}"), "body text").await.unwrap();
    }
    let export = f.app.export().await.unwrap();
    assert_eq!(export.articles.len(), 205);
    let ids: std::collections::HashSet<_> = export.articles.iter().map(|a| a.id.clone()).collect();
    assert_eq!(ids.len(), 205);
}
