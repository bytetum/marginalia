//! REST + MCP tests: the router is driven in-process with `oneshot`; MemRepo, FsBlobStore on a tempdir, no LLM, no network.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use marginalia::api;
use marginalia::blob::FsBlobStore;
use marginalia::mcp::MarginaliaMcp;
use marginalia::mem::MemRepo;
use marginalia::service::{App, AppConfig};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

const TOKEN: &str = "s3cret";

fn app() -> (Arc<App>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let app = App::new(Arc::new(MemRepo::new()), Arc::new(FsBlobStore::new(dir.path().join("blobs"))), AppConfig::default());
    (Arc::new(app), dir)
}

async fn call(router: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = router.clone().oneshot(req.body(body).expect("request")).await.expect("response");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

async fn save_sample(router: &Router) -> String {
    let (status, article) = call(
        router,
        "POST",
        "/articles",
        Some(TOKEN),
        Some(json!({ "url": "https://example.com/otters", "title": "Otters at play", "markdown": "Otters hold hands while they sleep so they do not drift apart." })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{article}");
    article["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn bearer_token_is_enforced_except_health() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));

    let (status, body) = call(&router, "GET", "/articles", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "GET", "/articles", Some("wrong"), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, "GET", "/articles", Some("s3cre"), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, "GET", "/articles", Some(TOKEN), None).await, (StatusCode::OK, json!([])));

    let (status, health) = call(&router, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({ "ok": true, "llm": false }));

    // MCP sits behind the same middleware.
    assert_eq!(call(&router, "POST", "/mcp", None, Some(json!({}))).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_token_means_open() {
    let (app, _dir) = app();
    let router = api::router(app, None);
    assert_eq!(call(&router, "GET", "/articles", None, None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn article_lifecycle() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));
    let id = save_sample(&router).await;

    let (status, article) = call(&router, "GET", &format!("/articles/{id}"), Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(article["title"], "Otters at play");
    assert_eq!(article["via"], "manual");

    let (_, list) = call(&router, "GET", "/articles?limit=10&offset=0", Some(TOKEN), None).await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["id"], id.as_str());
    assert!(list[0].get("markdown").is_none());
    assert_eq!(call(&router, "GET", "/articles?offset=1", Some(TOKEN), None).await.1, json!([]));

    let (status, hl) = call(&router, "POST", &format!("/articles/{id}/highlights"), Some(TOKEN), Some(json!({ "text": "hold hands", "note": "cute" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{hl}");
    assert_eq!(hl["note"], "cute");
    let (_, per_article) = call(&router, "GET", &format!("/articles/{id}/highlights"), Some(TOKEN), None).await;
    assert_eq!(per_article.as_array().map(Vec::len), Some(1));
    let (_, all) = call(&router, "GET", "/highlights", Some(TOKEN), None).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));

    let (status, hits) = call(&router, "GET", "/search?q=otters&limit=5", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits[0]["article_id"], id.as_str());
    assert_eq!(call(&router, "GET", "/search?q=", Some(TOKEN), None).await.1, json!([]));

    assert_eq!(call(&router, "DELETE", &format!("/articles/{id}"), Some(TOKEN), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&router, "GET", &format!("/articles/{id}"), Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "GET", "/highlights", Some(TOKEN), None).await.1, json!([]));
}

#[tokio::test]
async fn errors_map_to_statuses() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));

    let (status, body) = call(&router, "GET", "/articles/nope", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().is_some_and(|m| m.contains("nope")));
    assert_eq!(call(&router, "DELETE", "/articles/nope", Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "GET", "/syntheses/nope", Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&router, "POST", "/articles/nope/highlights", Some(TOKEN), Some(json!({ "text": "x" }))).await.0,
        StatusCode::NOT_FOUND
    );

    // 400: bad input of every kind, always with an {error} body.
    let (status, body) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "url": "https://x.test", "title": "t" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    let (status, _) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "url": "ftp://x.test" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "nope": 1 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", "/syntheses?kind=poem", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", "/articles?limit=abc", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 503: everything that needs an LLM.
    let (status, body) = call(&router, "POST", "/chat", Some(TOKEN), Some(json!({ "message": "hi" }))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "POST", "/briefs", Some(TOKEN), Some(json!({ "direction": "risks" }))).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(call(&router, "POST", "/briefs", Some(TOKEN), None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(call(&router, "POST", "/articles/x/analyse", Some(TOKEN), None).await.0, StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(call(&router, "DELETE", "/chat/some-session", Some(TOKEN), None).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn syntheses_are_listed_and_filtered() {
    let (app, _dir) = app();
    let saved = app
        .repo()
        .save_synthesis(marginalia::domain::NewSynthesis {
            kind: marginalia::domain::SynthesisKind::Brief,
            title: "Brief".into(),
            direction: None,
            body: "body".into(),
            article_ids: vec![],
        })
        .await
        .expect("save");
    let router = api::router(app, None);
    let (_, all) = call(&router, "GET", "/syntheses", None, None).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));
    assert_eq!(call(&router, "GET", "/syntheses?kind=analysis", None, None).await.1, json!([]));
    assert_eq!(call(&router, "GET", "/syntheses?kind=brief&limit=1", None, None).await.1.as_array().map(Vec::len), Some(1));
    let (status, one) = call(&router, "GET", &format!("/syntheses/{}", saved.id), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["title"], "Brief");
}

// ---------- MCP ----------

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().expect("object")
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    serde_json::to_value(&result.content).expect("content").pointer("/0/text").and_then(Value::as_str).unwrap_or_default().to_string()
}

#[tokio::test]
async fn mcp_tools_over_in_process_transport() {
    let (app, _dir) = app();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let running = MarginaliaMcp::new(app).serve(server_io).await.expect("serve");
        let _ = running.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.expect("client");

    let info = client.peer_info().expect("peer info");
    assert!(info.instructions.as_deref().is_some_and(|i| i.contains("search")));

    let tools = client.list_all_tools().await.expect("tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "list_articles", "get_article", "search", "list_highlights", "add_highlight", "save_url", "save_markdown",
        "list_syntheses", "get_synthesis", "save_synthesis", "analyse_article", "write_brief",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}: {names:?}");
    }
    assert!(!names.iter().any(|n| n.contains("delete")));

    let saved = client
        .call_tool(CallToolRequestParams::new("save_markdown").with_arguments(args(
            json!({ "url": "https://example.com/otters", "title": "Otters at play", "markdown": "Otters hold hands while they sleep." }),
        )))
        .await
        .expect("save_markdown");
    assert_ne!(saved.is_error, Some(true));
    let id = serde_json::from_str::<Value>(&text_of(&saved)).expect("json")["id"].as_str().expect("id").to_string();

    let found = client.call_tool(CallToolRequestParams::new("search").with_arguments(args(json!({ "query": "otters" })))).await.expect("search");
    assert_ne!(found.is_error, Some(true));
    let hits: Value = serde_json::from_str(&text_of(&found)).expect("json");
    assert_eq!(hits[0]["article_id"], id.as_str());

    let listed = client.call_tool(CallToolRequestParams::new("list_articles").with_arguments(args(json!({})))).await.expect("list");
    assert_eq!(serde_json::from_str::<Value>(&text_of(&listed)).expect("json").as_array().map(Vec::len), Some(1));

    let hl = client
        .call_tool(CallToolRequestParams::new("add_highlight").with_arguments(args(json!({ "article_id": id, "text": "hold hands" }))))
        .await
        .expect("add_highlight");
    assert_ne!(hl.is_error, Some(true));

    let synth = client
        .call_tool(CallToolRequestParams::new("save_synthesis").with_arguments(args(
            json!({ "kind": "brief", "title": "Otters", "body": "They hold hands.", "article_ids": [id] }),
        )))
        .await
        .expect("save_synthesis");
    assert_ne!(synth.is_error, Some(true));
    let listed = client.call_tool(CallToolRequestParams::new("list_syntheses").with_arguments(args(json!({ "kind": "brief" })))).await.expect("list_syntheses");
    assert_eq!(serde_json::from_str::<Value>(&text_of(&listed)).expect("json").as_array().map(Vec::len), Some(1));

    // Failures are tool errors, not protocol errors.
    for (tool, a) in [
        ("get_article", json!({ "id": "missing" })),
        ("add_highlight", json!({ "article_id": "missing", "text": "x" })),
        ("save_synthesis", json!({ "kind": "poem", "title": "t", "body": "b", "article_ids": [] })),
        ("write_brief", json!({})),
    ] {
        let res = client.call_tool(CallToolRequestParams::new(tool).with_arguments(args(a))).await.unwrap_or_else(|e| panic!("{tool} was a protocol error: {e}"));
        assert_eq!(res.is_error, Some(true), "{tool}");
        assert!(!text_of(&res).is_empty());
    }

    let _ = client.cancel().await;
    let _ = server.await;
}

#[tokio::test]
async fn export_needs_the_token_and_returns_the_whole_library() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));

    let (status, body) = call(&router, "GET", "/export", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "GET", "/export", Some("wrong"), None).await.0, StatusCode::UNAUTHORIZED);

    let (status, empty) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["version"], 1);
    assert_eq!((empty["articles"].clone(), empty["highlights"].clone(), empty["syntheses"].clone()), (json!([]), json!([]), json!([])));

    let id = save_sample(&router).await;
    let (status, _) = call(&router, "POST", &format!("/articles/{id}/highlights"), Some(TOKEN), Some(json!({ "text": "hold hands" }))).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, export) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(export["version"], 1);
    assert!(export["exported_at"].is_string());
    assert_eq!(export["articles"][0]["id"], id.as_str());
    assert_eq!(export["articles"][0]["markdown"], "Otters hold hands while they sleep so they do not drift apart.");
    assert_eq!(export["highlights"][0]["article_id"], id.as_str());
    assert_eq!(export["syntheses"], json!([]));

    // Import is CLI-only: there is no REST route for it.
    for method in ["POST", "PUT"] {
        let (status, _) = call(&router, method, "/export", Some(TOKEN), Some(export.clone())).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
    assert_eq!(call(&router, "POST", "/import", Some(TOKEN), Some(export)).await.0, StatusCode::NOT_FOUND);
}
